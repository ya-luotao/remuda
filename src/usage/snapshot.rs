//! A usage snapshot read at an instant (SPEC R10, R23). A [`UsageRow`] is what an agent said at
//! some time; what it means now depends on when it was said and on whether the window has reset
//! since. [`Snapshot::at`] answers that once, for `remuda usage`, `remuda pick`, the request to
//! Jev and the TUI alike: each window's reset as an instant, whether it has passed, the
//! percentage (unknown once it has), and the snapshot's age and staleness.

use jiff::Timestamp;
use jiff::tz::TimeZone;

use super::{CRIT_AT, CachedUsage, Resets, UsageRow, WARN_AT, format_ago};

/// Where a snapshot came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Cached,
    Live,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Cached => "cached",
            Source::Live => "live",
        }
    }
}

/// What an agent said about an account's usage, and when.
#[derive(Debug, Clone, Copy)]
pub struct Snapshot<'a> {
    pub source: Source,
    /// When the agent recorded it (the cache time) or answered (a live query); `None` when
    /// unknown.
    pub fetched_at: Option<Timestamp>,
    pub rows: &'a [UsageRow],
    /// Where a window that tells no reset of its own takes one from
    /// ([`Snapshot::with_resets_of`]).
    pub resets_of: Option<&'a CachedUsage>,
}

impl<'a> Snapshot<'a> {
    pub fn cached(cached: &'a CachedUsage) -> Self {
        Snapshot {
            source: Source::Cached,
            fetched_at: cached.fetched_at,
            rows: &cached.rows,
            resets_of: None,
        }
    }

    /// A live query's rows, answered at `answered_at`: the time the answer arrived, not the
    /// time the query was started.
    pub fn live(rows: &'a [UsageRow], answered_at: Timestamp) -> Self {
        Snapshot {
            source: Source::Live,
            fetched_at: Some(answered_at),
            rows,
            resets_of: None,
        }
    }

    /// A window whose row tells no reset, or one in wording remuda cannot read, takes the reset
    /// `cached` records for the same limit, while that is ahead (R10). A reset that was read
    /// is never replaced, whatever it came to.
    pub fn with_resets_of(mut self, cached: &'a CachedUsage) -> Self {
        self.resets_of = Some(cached);
        self
    }

    /// The snapshot as it reads at `now` (R10). A reset's wording is read as the next such time
    /// after it was said. A reset that was already behind when the usage was recorded says
    /// nothing: the percentage stands, without a reset. Of the others, one that is not after
    /// `now` has passed: that window's percentage is unknown.
    pub fn at(&self, now: Timestamp) -> Reading {
        let said = self.fetched_at.unwrap_or(now);
        let windows = self
            .rows
            .iter()
            .map(|row| {
                let told = row.resets.as_ref().and_then(|r| reset_instant(r, said));
                let reset = match told {
                    Some(t) => seen(t, self.fetched_at, now),
                    None => self.cached_reset(&row.label, now),
                };
                Window {
                    label: row.label.clone(),
                    model: window_model(&row.label),
                    reset,
                    wording: match &row.resets {
                        Some(Resets::Text(text)) => Some(text.clone()),
                        _ => None,
                    },
                    percent: row.percent,
                    reported: row.severity.clone(),
                    source: self.source,
                }
            })
            .collect();
        Reading {
            source: self.source,
            fetched_at: self.fetched_at,
            age_seconds: self
                .fetched_at
                .map(|at| (now.as_second() - at.as_second()).max(0)),
            windows,
        }
    }

    /// The reset `resets_of` records for the limit `label`, only while it is ahead: a cached
    /// reset that has passed says nothing about a percentage it did not record.
    fn cached_reset(&self, label: &str, now: Timestamp) -> Reset {
        let Some(cached) = self.resets_of else {
            return Reset::Unknown;
        };
        let same = cached.rows.iter().find(|row| row.label == label);
        match same.and_then(|row| row.resets.as_ref()) {
            Some(Resets::At(t)) => match seen(*t, cached.fetched_at, now) {
                ahead @ Reset::Ahead(_) => ahead,
                Reset::Passed(_) | Reset::Unknown => Reset::Unknown,
            },
            _ => Reset::Unknown,
        }
    }
}

/// The reset instant `t`, told in usage recorded at `fetched_at`, as seen at `now`. Whether it
/// says anything is decided against the recording first, then where it lies against `now`: a
/// clock behind the recording does not bring back a reset the recording had already passed.
fn seen(t: Timestamp, fetched_at: Option<Timestamp>, now: Timestamp) -> Reset {
    if fetched_at.is_some_and(|at| t <= at) {
        Reset::Unknown
    } else if t > now {
        Reset::Ahead(t)
    } else {
        Reset::Passed(t)
    }
}

/// A snapshot read at an instant: its age, and its windows as they stand then.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub source: Source,
    pub fetched_at: Option<Timestamp>,
    /// Seconds since `fetched_at` (0 for a time ahead); `None` when unknown.
    pub age_seconds: Option<i64>,
    pub windows: Vec<Window>,
}

impl Reading {
    /// Older than `after_minutes`, or of unknown age; live usage never is (R23).
    pub fn stale(&self, after_minutes: u32) -> bool {
        match (self.source, self.age_seconds) {
            (Source::Live, _) => false,
            (Source::Cached, None) => true,
            (Source::Cached, Some(age)) => age > i64::from(after_minutes) * 60,
        }
    }

    /// `5h ago`; `None` when the age is unknown.
    pub fn age_text(&self) -> Option<String> {
        self.age_seconds.map(format_ago)
    }
}

/// When a window resets, seen from the instant of the reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reset {
    /// No reset is known: none was told (nor cached, [`Snapshot::with_resets_of`]), its wording
    /// was not recognized, or the time was already behind when the usage was recorded.
    Unknown,
    Ahead(Timestamp),
    /// The window reset at this instant, after its usage was recorded: the recorded
    /// percentage is obsolete.
    Passed(Timestamp),
}

/// One usage window of a [`Reading`].
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    /// As R10 labels it: `Session`, `Week (all models)`, `Week (Fable)`.
    pub label: String,
    /// The name in the label's trailing parentheses; `None` for a general window.
    pub model: Option<String>,
    pub reset: Reset,
    /// Claude's own wording of the reset, for display.
    pub wording: Option<String>,
    /// Percent used as recorded; obsolete once the reset has passed, so only [`Window::used`]
    /// tells it.
    percent: f64,
    /// The severity as reported; as obsolete as the percentage.
    reported: Option<String>,
    source: Source,
}

impl Window {
    /// Percent used; `None` when the reset has passed since it was recorded: nothing is known
    /// about the window after its reset, and 0 would only be a lower bound.
    pub fn used(&self) -> Option<f64> {
        (!self.reset_passed()).then_some(self.percent)
    }

    /// Percent left; `None` as for [`Window::used`].
    pub fn left(&self) -> Option<f64> {
        self.used().map(|used| (100.0 - used).max(0.0))
    }

    /// The reset, when it is ahead: never an instant in the past.
    pub fn resets_at(&self) -> Option<Timestamp> {
        match self.reset {
            Reset::Ahead(t) => Some(t),
            Reset::Unknown | Reset::Passed(_) => None,
        }
    }

    pub fn reset_passed(&self) -> bool {
        matches!(self.reset, Reset::Passed(_))
    }

    /// How a reset that has passed is said: `reset since cached`, or, for a live answer read
    /// after a reset it named, `reset since asked`.
    pub fn reset_since(&self) -> &'static str {
        match self.source {
            Source::Cached => "reset since cached",
            Source::Live => "reset since asked",
        }
    }

    /// `normal`, `warning`, `critical`, ...: as reported, else derived from the percentage
    /// (live rows carry none, R10); `normal` once the reset has passed.
    pub fn severity(&self) -> &str {
        match (self.used(), self.reported.as_deref()) {
            (None, _) => "normal",
            (Some(_), Some(reported)) => reported,
            (Some(used), None) if used >= CRIT_AT => "critical",
            (Some(used), None) if used >= WARN_AT => "warning",
            (Some(_), None) => "normal",
        }
    }
}

/// `Week (Fable)` → `Fable`; `Week (all models)`, `Session`, `5h window` → `None`.
fn window_model(label: &str) -> Option<String> {
    let inner = label.strip_suffix(')')?.rsplit_once(" (")?.1;
    (inner != "all models").then(|| inner.to_string())
}

/// When a limit resets, as an instant: [`Resets::At`] as is; claude's wording
/// (`Sep 24 at 3:19am (Asia/Shanghai)`, `3am (UTC)`) parsed best-effort, as the next such
/// time around `now`. `None` when the wording is not recognized.
fn reset_instant(resets: &Resets, now: Timestamp) -> Option<Timestamp> {
    match resets {
        Resets::At(ts) => Some(*ts),
        Resets::Text(text) => parse_reset_text(text, now),
    }
}

fn parse_reset_text(text: &str, now: Timestamp) -> Option<Timestamp> {
    let (rest, zone) = text.trim().strip_suffix(')')?.rsplit_once(" (")?;
    let zone = TimeZone::get(zone).ok()?;
    let (date, time) = match rest.split_once(" at ") {
        Some((date, time)) => (Some(date.trim()), time.trim()),
        None => (None, rest.trim()),
    };
    let time = time.to_ascii_lowercase();
    let (clock, pm) = match (time.strip_suffix("am"), time.strip_suffix("pm")) {
        (Some(c), _) => (c, false),
        (_, Some(c)) => (c, true),
        _ => return None,
    };
    let (hour, minute) = match clock.split_once(':') {
        Some((h, m)) => (h.parse::<i8>().ok()?, m.parse::<i8>().ok()?),
        None => (clock.parse::<i8>().ok()?, 0),
    };
    if !(1..=12).contains(&hour) {
        return None;
    }
    let hour = hour % 12 + if pm { 12 } else { 0 };
    let today = now.to_zoned(zone.clone()).date();
    let at = |d: jiff::civil::Date| -> Option<Timestamp> {
        Some(
            d.at(hour, minute, 0, 0)
                .to_zoned(zone.clone())
                .ok()?
                .timestamp(),
        )
    };
    match date {
        None => {
            let t = at(today)?;
            if t >= now {
                Some(t)
            } else {
                at(today.tomorrow().ok()?)
            }
        }
        Some(date) => {
            let (month, day) = date.split_once(' ')?;
            const MONTHS: [&str; 12] = [
                "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
            ];
            let month = month.get(..3)?.to_ascii_lowercase();
            let month = MONTHS.iter().position(|m| *m == month)? as i8 + 1;
            let day: i8 = day.trim().parse().ok()?;
            // The nearest year that does not put the reset more than a day in the past.
            let year = today.year();
            let this = at(jiff::civil::Date::new(year, month, day).ok()?)?;
            if this.as_second() >= now.as_second() - 86_400 {
                Some(this)
            } else {
                at(jiff::civil::Date::new(year + 1, month, day).ok()?)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    const NOW: &str = "2026-09-27T10:00:00Z";

    fn row(label: &str, percent: f64, resets: Option<Resets>) -> UsageRow {
        UsageRow {
            label: label.into(),
            percent,
            severity: None,
            resets,
        }
    }

    fn at(s: &str) -> Option<Resets> {
        Some(Resets::At(ts(s)))
    }

    fn text(s: &str) -> Option<Resets> {
        Some(Resets::Text(s.into()))
    }

    fn cached(fetched_at: Option<&str>, rows: Vec<UsageRow>) -> CachedUsage {
        CachedUsage {
            fetched_at: fetched_at.map(ts),
            rows,
        }
    }

    #[test]
    fn reset_text_becomes_an_instant() {
        let now = ts("2026-09-23T18:00:00Z"); // Sep 24 02:00 in Shanghai
        let parse = |t: &str| reset_instant(&Resets::Text(t.into()), now);
        assert_eq!(
            parse("Sep 24 at 3:19am (Asia/Shanghai)"),
            Some(ts("2026-09-23T19:19:00Z"))
        );
        assert_eq!(
            parse("Sep 29 at 11:59am (Asia/Shanghai)"),
            Some(ts("2026-09-29T03:59:00Z"))
        );
        assert_eq!(
            parse("Sep 29 at 12pm (UTC)"),
            Some(ts("2026-09-29T12:00:00Z"))
        );
        assert_eq!(
            parse("Sep 29 at 12:30am (UTC)"),
            Some(ts("2026-09-29T00:30:00Z"))
        );
        // No date: the next such time.
        assert_eq!(parse("7pm (UTC)"), Some(ts("2026-09-23T19:00:00Z")));
        assert_eq!(parse("5pm (UTC)"), Some(ts("2026-09-24T17:00:00Z")));
        // Around New Year: January is next year.
        let dec = ts("2026-12-30T00:00:00Z");
        assert_eq!(
            reset_instant(&Resets::Text("Jan 2 at 1am (UTC)".into()), dec),
            Some(ts("2027-01-02T01:00:00Z"))
        );
        for junk in [
            "",
            "soon",
            "Sep 24 at 3:19am",
            "Sep 24 at 3:19am (Not/AZone)",
            "Sep 24 at 13am (UTC)",
            "Foo 24 at 3am (UTC)",
            "Sep 24 at 3:xxam (UTC)",
        ] {
            assert_eq!(parse(junk), None, "{junk:?}");
        }
        let at = ts("2026-09-25T05:00:00Z");
        assert_eq!(reset_instant(&Resets::At(at), now), Some(at));
    }

    /// R10, R23: a window whose reset fell after the usage was recorded is of unknown usage,
    /// not 0% used; one whose reset is still ahead reads as recorded, however old the data.
    #[test]
    fn a_reset_that_passed_since_the_usage_was_recorded_leaves_it_unknown() {
        let now = ts(NOW);
        let cache = cached(
            Some("2026-09-27T00:00:00Z"),
            vec![
                UsageRow {
                    severity: Some("critical".into()),
                    ..row("Session", 100.0, at("2026-09-27T09:00:00Z"))
                },
                row("Week (all models)", 100.0, at("2026-09-28T09:00:00Z")),
            ],
        );
        let reading = Snapshot::cached(&cache).at(now);
        let [session, week] = &reading.windows[..] else {
            panic!("{reading:?}");
        };
        assert_eq!(session.reset, Reset::Passed(ts("2026-09-27T09:00:00Z")));
        assert!(session.reset_passed());
        assert_eq!((session.used(), session.left()), (None, None));
        assert_eq!(session.resets_at(), None, "never an instant in the past");
        assert_eq!(
            session.severity(),
            "normal",
            "as obsolete as the percentage"
        );

        assert_eq!(week.reset, Reset::Ahead(ts("2026-09-28T09:00:00Z")));
        assert!(!week.reset_passed());
        assert_eq!((week.used(), week.left()), (Some(100.0), Some(0.0)));
        assert_eq!(week.resets_at(), Some(ts("2026-09-28T09:00:00Z")));
        assert_eq!(week.severity(), "critical");

        // The instant of the reset itself has passed.
        let on_time = Snapshot::cached(&cache).at(ts("2026-09-27T09:00:00Z"));
        assert!(on_time.windows[0].reset_passed());
        let before = Snapshot::cached(&cache).at(ts("2026-09-27T08:59:59Z"));
        assert_eq!(before.windows[0].used(), Some(100.0));
        // A cache of unknown age was recorded before its resets.
        let undated = CachedUsage {
            fetched_at: None,
            ..cache
        };
        assert!(Snapshot::cached(&undated).at(now).windows[0].reset_passed());
    }

    /// R10, R23: a reset that was already behind when the usage was recorded says nothing: a
    /// live answer keeps its percentage, whatever its reset reads as.
    #[test]
    fn a_reset_behind_when_recorded_leaves_the_percentage() {
        let now = ts(NOW);
        let rows = [
            // Wording read into the past: at most a day back (`parse_reset_text`).
            row("Session", 60.0, text("Sep 27 at 9am (UTC)")),
            row("Week (all models)", 95.0, at("2026-09-26T00:00:00Z")),
            row("Week (Fable)", 20.0, text("soon")),
        ];
        let reading = Snapshot::live(&rows, now).at(now);
        let used: Vec<Option<f64>> = reading.windows.iter().map(Window::used).collect();
        assert_eq!(used, [Some(60.0), Some(95.0), Some(20.0)]);
        for w in &reading.windows {
            assert_eq!(w.reset, Reset::Unknown, "{}", w.label);
            assert_eq!(w.resets_at(), None, "{}", w.label);
        }
        assert_eq!(reading.windows[1].severity(), "critical");
        assert_eq!(
            reading.windows[0].wording.as_deref(),
            Some("Sep 27 at 9am (UTC)")
        );
        assert_eq!(reading.windows[1].wording, None);
        // A cache that records a reset already behind it: the same.
        let cache = cached(
            Some("2026-09-27T09:30:00Z"),
            vec![row("Session", 34.0, at("2026-09-27T09:00:00Z"))],
        );
        let odd = &Snapshot::cached(&cache).at(now).windows[0];
        assert_eq!((odd.used(), odd.reset), (Some(34.0), Reset::Unknown));
        // A clock that runs behind the cache does not bring that reset back as one ahead: what
        // a reset says is decided against the recording first.
        let behind = &Snapshot::cached(&cache)
            .at(ts("2026-09-27T08:30:00Z"))
            .windows[0];
        assert_eq!((behind.used(), behind.reset), (Some(34.0), Reset::Unknown));
        assert_eq!(behind.resets_at(), None);
        // One the cache did record ahead of itself is ahead for that clock too.
        let cache = cached(
            Some("2026-09-27T09:30:00Z"),
            vec![row("Session", 34.0, at("2026-09-27T09:45:00Z"))],
        );
        let ahead = &Snapshot::cached(&cache)
            .at(ts("2026-09-27T08:30:00Z"))
            .windows[0];
        assert_eq!(ahead.reset, Reset::Ahead(ts("2026-09-27T09:45:00Z")));
    }

    /// R10: wording is read from when it was said, so a live answer that has been kept a while
    /// passes its reset like a cache does.
    #[test]
    fn a_live_answer_ages() {
        let asked = ts("2026-09-27T05:00:00Z");
        let rows = [
            row("Session", 88.0, text("7am (UTC)")),
            row("Week (all models)", 40.0, text("Sep 30 at 12pm (UTC)")),
        ];
        let then = Snapshot::live(&rows, asked).at(asked);
        assert_eq!(
            then.windows[0].reset,
            Reset::Ahead(ts("2026-09-27T07:00:00Z"))
        );
        assert_eq!(then.windows[0].severity(), "warning");
        let later = Snapshot::live(&rows, asked).at(ts(NOW));
        assert_eq!(
            later.windows[0].reset,
            Reset::Passed(ts("2026-09-27T07:00:00Z")),
            "not tomorrow's 7am"
        );
        assert_eq!(later.windows[0].used(), None);
        assert_eq!(later.windows[1].used(), Some(40.0));
        assert_eq!(
            later.windows[1].resets_at(),
            Some(ts("2026-09-30T12:00:00Z"))
        );
        assert_eq!(later.age_seconds, Some(5 * 3600));
        assert!(!later.stale(1), "live usage is never stale");
    }

    /// R23: cached usage older than the given minutes, or of unknown age, is stale.
    #[test]
    fn age_and_staleness() {
        let now = ts(NOW);
        let aged = |fetched_at: Option<&str>| {
            let cache = cached(fetched_at, Vec::new());
            Snapshot::cached(&cache).at(now)
        };
        let two_hours = aged(Some("2026-09-27T08:00:00Z"));
        assert_eq!(two_hours.age_seconds, Some(7200));
        assert_eq!(two_hours.age_text().as_deref(), Some("2h ago"));
        assert!(!two_hours.stale(120), "exactly as old as allowed");
        assert!(two_hours.stale(119));
        assert!(aged(Some("2026-09-27T07:59:59Z")).stale(120));
        let unknown = aged(None);
        assert_eq!((unknown.age_seconds, unknown.age_text()), (None, None));
        assert!(unknown.stale(u32::MAX));
        // A clock that runs behind the cache: no negative age.
        let ahead = aged(Some("2026-09-27T10:00:30Z"));
        assert_eq!(ahead.age_seconds, Some(0));
        assert_eq!(ahead.source.name(), "cached");
        assert_eq!(Snapshot::live(&[], now).at(now).source.name(), "live");
    }

    /// R10: a window that tells no reset remuda can read takes the cached reset of the same
    /// limit, only while that is ahead: a reset that has passed is never placed anywhere. A
    /// reset that was read keeps what it came to, behind the answer included.
    #[test]
    fn untold_resets_fall_back_to_the_cache_while_it_is_ahead() {
        let now = ts(NOW);
        let rows = [
            row("Session", 12.0, text("soon")),
            row("Week (all models)", 91.0, None),
            row("Week (Fable)", 5.0, text("Sep 30 at 12pm (UTC)")),
            row("Week (Sonnet)", 7.0, text("whenever")),
        ];
        let cache = cached(
            Some("2026-09-27T06:00:00Z"),
            vec![
                row("Session", 5.0, at("2026-09-27T12:30:00Z")),
                row("Week (all models)", 80.0, at("2026-09-27T09:00:00Z")),
                row("Week (Fable)", 1.0, at("2026-10-02T00:00:00Z")),
            ],
        );
        let reading = Snapshot::live(&rows, now).with_resets_of(&cache).at(now);
        let resets: Vec<Reset> = reading.windows.iter().map(|w| w.reset).collect();
        assert_eq!(
            resets,
            [
                Reset::Ahead(ts("2026-09-27T12:30:00Z")),
                // The cache's reset has passed: no instant, and the live percentage stands.
                Reset::Unknown,
                // Its own reset wins.
                Reset::Ahead(ts("2026-09-30T12:00:00Z")),
                Reset::Unknown,
            ]
        );
        assert_eq!(reading.windows[1].used(), Some(91.0));
        // The cache read on its own: that window has reset since.
        let own = Snapshot::cached(&cache).at(now);
        assert!(own.windows[1].reset_passed());
        assert_eq!(own.windows[1].resets_at(), None);
        // Without a cache to ask: unknown.
        let alone = Snapshot::live(&rows, now).at(now);
        assert_eq!(alone.windows[0].reset, Reset::Unknown);

        // Wording that was read, and names a reset already behind the answer, is not a reset
        // remuda could not read: the cache's later reset for that limit does not replace it,
        // or the timeline would draw a reset the agent did not tell.
        let rows = [
            row("Session", 60.0, text("Sep 27 at 9am (UTC)")),
            row("Week (all models)", 91.0, at("2026-09-26T00:00:00Z")),
        ];
        let cache = cached(
            Some("2026-09-27T08:00:00Z"),
            vec![
                row("Session", 20.0, at("2026-09-27T12:00:00Z")),
                row("Week (all models)", 80.0, at("2026-09-30T00:00:00Z")),
            ],
        );
        let reading = Snapshot::live(&rows, now).with_resets_of(&cache).at(now);
        for w in &reading.windows {
            assert_eq!(
                (w.reset, w.resets_at()),
                (Reset::Unknown, None),
                "{}",
                w.label
            );
        }
        // The same answer, kept until after a reset it told: passed, and it stays so.
        let rows = [row("Session", 60.0, text("Sep 27 at 11am (UTC)"))];
        let later = ts("2026-09-27T11:30:00Z");
        let reading = Snapshot::live(&rows, now).with_resets_of(&cache).at(later);
        assert_eq!(
            reading.windows[0].reset,
            Reset::Passed(ts("2026-09-27T11:00:00Z"))
        );
        // A cached reset that the cache itself had already passed is no reset to borrow.
        let rows = [row("Session", 60.0, None)];
        let cache = cached(
            Some("2026-09-27T12:30:00Z"),
            vec![row("Session", 20.0, at("2026-09-27T12:00:00Z"))],
        );
        let reading = Snapshot::live(&rows, now).with_resets_of(&cache).at(now);
        assert_eq!(reading.windows[0].reset, Reset::Unknown);
    }

    #[test]
    fn a_per_model_window_names_its_model() {
        assert_eq!(window_model("Week (Fable)").as_deref(), Some("Fable"));
        assert_eq!(window_model("Week (all models)"), None);
        assert_eq!(window_model("Session"), None);
        let rows = [
            row("Session (GPT-5.3-Codex-Spark)", 1.0, None),
            row("5h window", 1.0, None),
        ];
        let reading = Snapshot::live(&rows, ts(NOW)).at(ts(NOW));
        assert_eq!(
            reading.windows[0].model.as_deref(),
            Some("GPT-5.3-Codex-Spark")
        );
        assert_eq!(reading.windows[1].model, None);
    }

    /// R10: the live sources provide no severity: 75% is a warning, 90% critical.
    #[test]
    fn live_rows_are_marked_at_75_and_90_percent() {
        assert_eq!((WARN_AT, CRIT_AT), (75.0, 90.0));
        let now = ts(NOW);
        let severity = |row: UsageRow| {
            Snapshot::live(&[row], now).at(now).windows[0]
                .severity()
                .to_string()
        };
        for (percent, want) in [
            (0.0, "normal"),
            (74.9, "normal"),
            (75.0, "warning"),
            (89.9, "warning"),
            (90.0, "critical"),
            (100.0, "critical"),
        ] {
            assert_eq!(severity(row("Session", percent, None)), want, "{percent}%");
        }
        // A reported severity (cached rows) wins over the percentage.
        let reported = UsageRow {
            severity: Some("normal".into()),
            ..row("Session", 95.0, None)
        };
        assert_eq!(severity(reported), "normal");
    }
}
