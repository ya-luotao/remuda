//! Waiting until usage frees up (SPEC R23 `pick --wait`, R10 `usage --wait`): when to try
//! again, from the windows that block and their resets, and the loop that tries.
//!
//! An attempt is whatever the command does once (`pick` gathers and ranks, `usage` reads one
//! account); the loop only says when the next one starts. It never queries anything itself:
//! an attempt reads the cache unless the command was given `--live`, as it would without
//! waiting.

use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::usage::{self, Window};
use crate::{pick, registry::Account};

/// After a blocking window's reset, how long before the next attempt: the agent's own clock
/// and the reset it reported may be a little apart.
pub const RESET_MARGIN: Duration = Duration::from_secs(30);
/// The least time from one attempt to the next: an attempt right after a reset that still
/// finds nothing free (another window blocks too) does not run again at once.
pub const MIN_INTERVAL: Duration = Duration::from_secs(60);
/// When no blocking window has a known reset ahead, how long until the next attempt.
pub const UNKNOWN_RETRY: Duration = Duration::from_secs(5 * 60);
/// The longest single sleep: the clock is read again at least this often, so that a computer
/// that was asleep does not put the attempt off by as long as it slept, and the status line's
/// time to the reset stays current.
pub const NAP: Duration = Duration::from_secs(60);

/// The window an attempt waits on, for the status line.
#[derive(Debug, Clone, PartialEq)]
pub struct Blocker {
    /// `claude:max`.
    pub account: String,
    pub window: Window,
}

/// When to try again, decided from one attempt that found nothing free.
#[derive(Debug, Clone, PartialEq)]
pub enum Wait {
    /// At `at`: the earliest reset ahead among the windows that block (`on`'s), plus
    /// [`RESET_MARGIN`], and at least [`MIN_INTERVAL`] after the attempt.
    At { at: Timestamp, on: Blocker },
    /// No window that blocks has a known reset ahead: at `at`, [`UNKNOWN_RETRY`] after the
    /// attempt. `on` is the first window that blocks.
    Unknown { at: Timestamp, on: Blocker },
    /// Nothing that blocks passes with time: why.
    Never(String),
}

impl Wait {
    /// When the next attempt starts; `None` for [`Wait::Never`].
    pub fn next(&self) -> Option<Timestamp> {
        match self {
            Wait::At { at, .. } | Wait::Unknown { at, .. } => Some(*at),
            Wait::Never(_) => None,
        }
    }
}

/// `t` plus `d`, or the last instant there is.
fn after(t: Timestamp, d: Duration) -> Timestamp {
    t.checked_add(d).unwrap_or(Timestamp::MAX)
}

/// When to try again (pure): `blocking` holds each window that keeps something from being free,
/// with its account, as read at `now`, the instant of the attempt. The earliest reset ahead
/// among them plus [`RESET_MARGIN`], at least [`MIN_INTERVAL`] after `now`; without a reset
/// ahead, [`UNKNOWN_RETRY`] after `now`. `None` when nothing is blocked by a window.
pub fn schedule<'a>(
    blocking: impl IntoIterator<Item = (&'a Account, &'a Window)>,
    now: Timestamp,
) -> Option<Wait> {
    let blocking: Vec<(&Account, &Window)> = blocking.into_iter().collect();
    let blocker = |(account, window): (&Account, &Window)| Blocker {
        account: account.qualified(),
        window: window.clone(),
    };
    let earliest = blocking
        .iter()
        .filter_map(|&(account, window)| Some((window.resets_at()?, account, window)))
        .min_by_key(|(reset, ..)| *reset);
    Some(match earliest {
        Some((reset, account, window)) => Wait::At {
            at: after(reset, RESET_MARGIN).max(after(now, MIN_INTERVAL)),
            on: blocker((account, window)),
        },
        None => Wait::Unknown {
            at: after(now, UNKNOWN_RETRY),
            on: blocker(*blocking.first()?),
        },
    })
}

/// How waiting ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Ended {
    /// The last attempt found something free (or was not asked to wait).
    Ready,
    /// The last attempt found nothing that time frees: why.
    Never(String),
    /// The next attempt was due at `next`, after the deadline (`--max-wait`): it was not made,
    /// and no time was slept out waiting for it.
    GaveUp { next: Timestamp },
    /// The next attempt was due at `next`, before the deadline, but remuda woke after the
    /// deadline (a computer that slept, a clock set forward, a late wake): it was not made.
    TimeUp {
        next: Timestamp,
        deadline: Timestamp,
    },
}

/// What is shown while waiting (R23): the line for a wait, refreshed before each nap, and
/// cleared once the wait is over, whichever way it ends.
pub trait Status {
    fn show(&mut self, wait: &Wait, now: Timestamp);
    fn clear(&mut self);
}

/// Attempts until one finds something free (R23, R10): `attempt` returns its result and, when
/// nothing was free, when to try again (or an error, which ends the loop). Between attempts,
/// sleeps until then, in naps of at most [`NAP`] measured against `clock`, showing `status`
/// before each; `status` is cleared before this returns, an error included, so that nothing
/// printed after it lands on the line. Stops at once on [`Wait::Never`], and when the next attempt would start after
/// `deadline`: no attempt starts after it, and no time is slept out in which nothing would be
/// checked. Nor does one start when remuda woke after the deadline (a computer that slept, a
/// clock set forward, a late wake): the clock is read again before each attempt. An attempt
/// under way is never cut short. Returns the last attempt's result.
pub fn until<T, E>(
    mut attempt: impl FnMut() -> Result<(T, Option<Wait>), E>,
    deadline: Option<Timestamp>,
    clock: impl Fn() -> Timestamp,
    mut sleep: impl FnMut(Duration),
    status: &mut impl Status,
) -> Result<(T, Ended), E> {
    let ended = attempts(&mut attempt, deadline, &clock, &mut sleep, status);
    status.clear();
    ended
}

/// [`until`]'s loop, without the clearing of `status`.
fn attempts<T, E>(
    attempt: &mut impl FnMut() -> Result<(T, Option<Wait>), E>,
    deadline: Option<Timestamp>,
    clock: &impl Fn() -> Timestamp,
    sleep: &mut impl FnMut(Duration),
    status: &mut impl Status,
) -> Result<(T, Ended), E> {
    let (mut result, mut wait) = attempt()?;
    loop {
        let Some(waiting) = wait else {
            return Ok((result, Ended::Ready));
        };
        let next = match &waiting {
            Wait::Never(why) => return Ok((result, Ended::Never(why.clone()))),
            Wait::At { at, .. } | Wait::Unknown { at, .. } => *at,
        };
        let past = |t: Timestamp| deadline.is_some_and(|deadline| t > deadline);
        if past(next) {
            return Ok((result, Ended::GaveUp { next }));
        }
        loop {
            let now = clock();
            if now >= next {
                break;
            }
            status.show(&waiting, now);
            sleep(next.duration_since(now).unsigned_abs().min(NAP));
        }
        if let Some(deadline) = deadline.filter(|deadline| clock() > *deadline) {
            return Ok((result, Ended::TimeUp { next, deadline }));
        }
        (result, wait) = attempt()?;
    }
}

/// `--max-wait`: the deadline, `max_wait` after `start`.
pub fn deadline(start: Timestamp, max_wait: Option<Duration>) -> Option<Timestamp> {
    max_wait.map(|d| after(start, d))
}

/// The status line while waiting, without `remuda: `: `waiting: claude:max Week (Fable) 100%
/// used, resets in 1h12m; next check Oct 8 21:40 (Ctrl-C stops)`; for a window without a
/// known reset, `…95% used, reset unknown; next check …`. `None` for [`Wait::Never`].
pub fn status_line(wait: &Wait, now: Timestamp, tz: &TimeZone) -> Option<String> {
    let (at, on, unknown) = match wait {
        Wait::At { at, on } => (at, on, ""),
        Wait::Unknown { at, on } => (at, on, ", reset unknown"),
        Wait::Never(_) => return None,
    };
    Some(format!(
        "waiting: {} {}{unknown}; next check {} (Ctrl-C stops)",
        on.account,
        pick::used_text(&on.window, now),
        usage::format_time(*at, tz)
    ))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::convert::Infallible;

    use super::*;
    use crate::provider::Provider;
    use crate::registry::Home;
    use crate::usage::{CachedUsage, Resets, Snapshot, UsageRow};

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    const NOW: &str = "2026-10-08T10:00:00Z";

    fn account(name: &str) -> Account {
        Account {
            provider: Provider::Claude,
            name: name.into(),
            home: Home::Path(format!("/h/{name}")),
        }
    }

    /// Windows cached a minute before [`NOW`], read at [`NOW`]: `(label, percent, reset)`.
    fn windows(rows: &[(&str, f64, Option<&str>)]) -> Vec<Window> {
        let cached = CachedUsage {
            fetched_at: Some(ts(NOW) - jiff::SignedDuration::from_mins(1)),
            rows: rows
                .iter()
                .map(|(label, percent, reset)| UsageRow {
                    label: label.to_string(),
                    percent: *percent,
                    severity: None,
                    resets: reset.map(|r| Resets::At(ts(r))),
                })
                .collect(),
        };
        Snapshot::cached(&cached).at(ts(NOW)).windows
    }

    /// R23 `--wait`: the earliest known reset among the windows that block, plus the margin;
    /// a window without a reset ahead does not hold the others back.
    #[test]
    fn the_earliest_known_reset_plus_a_margin() {
        let (max, team) = (account("max"), account("team"));
        let w = windows(&[
            ("Week (all models)", 100.0, Some("2026-10-10T10:00:00Z")),
            ("Session", 96.0, Some("2026-10-08T11:12:00Z")),
            ("Week (Fable)", 100.0, None),
        ]);
        let wait = schedule([(&max, &w[0]), (&team, &w[1]), (&max, &w[2])], ts(NOW));
        assert_eq!(
            wait,
            Some(Wait::At {
                at: ts("2026-10-08T11:12:30Z"),
                on: Blocker {
                    account: "claude:team".into(),
                    window: w[1].clone(),
                },
            })
        );
    }

    /// R23 `--wait`: never sooner than a minute after the attempt, even when the reset is
    /// right ahead: an attempt right after a reset that still finds nothing free does not
    /// run again at once.
    #[test]
    fn at_least_a_minute_between_attempts() {
        let max = account("max");
        let w = windows(&[("Session", 100.0, Some("2026-10-08T10:00:05Z"))]);
        let wait = schedule([(&max, &w[0])], ts(NOW)).unwrap();
        assert_eq!(wait.next(), Some(ts("2026-10-08T10:01:00Z")));
        // Exactly at the floor: the margin decides.
        let w = windows(&[("Session", 100.0, Some("2026-10-08T10:00:30Z"))]);
        let wait = schedule([(&max, &w[0])], ts(NOW)).unwrap();
        assert_eq!(wait.next(), Some(ts("2026-10-08T10:01:00Z")));
        let w = windows(&[("Session", 100.0, Some("2026-10-08T10:00:31Z"))]);
        let wait = schedule([(&max, &w[0])], ts(NOW)).unwrap();
        assert_eq!(wait.next(), Some(ts("2026-10-08T10:01:01Z")));
    }

    /// R23 `--wait`: with no reset known ahead (none told, or one already behind when the
    /// usage was recorded), five minutes; with nothing that blocks, nothing to schedule.
    #[test]
    fn without_a_known_reset_five_minutes() {
        let max = account("max");
        let w = windows(&[
            ("Week (all models)", 100.0, None),
            // Behind when it was cached: the percentage stands, no reset is known.
            ("Session", 100.0, Some("2026-10-08T09:00:00Z")),
        ]);
        assert_eq!(w[1].resets_at(), None);
        assert_eq!(w[1].used(), Some(100.0));
        let wait = schedule([(&max, &w[1]), (&max, &w[0])], ts(NOW)).unwrap();
        assert_eq!(
            wait,
            Wait::Unknown {
                at: ts("2026-10-08T10:05:00Z"),
                on: Blocker {
                    account: "claude:max".into(),
                    window: w[1].clone(),
                },
            }
        );
        assert_eq!(schedule([], ts(NOW)), None);
    }

    /// A fake clock that each sleep moves on, and a script of attempts.
    struct Fake {
        now: Cell<Timestamp>,
        slept: RefCell<Vec<Duration>>,
        attempts: RefCell<Vec<Timestamp>>,
        statuses: RefCell<Vec<Timestamp>>,
        cleared: Cell<usize>,
    }

    /// The status of a [`Fake`]: when each line was shown, and how often it was cleared.
    struct Recorder<'a>(&'a Fake);

    impl Status for Recorder<'_> {
        fn show(&mut self, _: &Wait, now: Timestamp) {
            self.0.statuses.borrow_mut().push(now);
        }

        fn clear(&mut self) {
            self.0.cleared.set(self.0.cleared.get() + 1);
        }
    }

    impl Fake {
        fn new() -> Fake {
            Fake {
                now: Cell::new(ts(NOW)),
                slept: RefCell::new(Vec::new()),
                attempts: RefCell::new(Vec::new()),
                statuses: RefCell::new(Vec::new()),
                cleared: Cell::new(0),
            }
        }

        /// [`until`] over `waits`, one per attempt (`None`: free); the attempt returns its
        /// number.
        fn run(&self, waits: Vec<Option<Wait>>, deadline: Option<Timestamp>) -> (usize, Ended) {
            self.run_sleeping(waits, deadline, |d| d)
        }

        /// [`Fake::run`] with a sleep that moves the clock by `slept(asked)`.
        fn run_sleeping(
            &self,
            waits: Vec<Option<Wait>>,
            deadline: Option<Timestamp>,
            slept: impl Fn(Duration) -> Duration,
        ) -> (usize, Ended) {
            let mut waits = waits.into_iter();
            let got = until(
                || {
                    self.attempts.borrow_mut().push(self.now.get());
                    let n = self.attempts.borrow().len();
                    Ok::<_, Infallible>((n, waits.next().expect("one attempt too many")))
                },
                deadline,
                || self.now.get(),
                |d| {
                    self.slept.borrow_mut().push(d);
                    self.now.set(after(self.now.get(), slept(d)));
                },
                &mut Recorder(self),
            )
            .unwrap();
            assert_eq!(
                self.cleared.get(),
                1,
                "the status is cleared once, at the end"
            );
            got
        }
    }

    fn at(t: &str) -> Wait {
        Wait::At {
            at: ts(t),
            on: Blocker {
                account: "claude:max".into(),
                window: windows(&[("Session", 100.0, None)]).remove(0),
            },
        }
    }

    /// R23 `--wait`: the loop sleeps until each attempt's time, in naps of at most a minute,
    /// and returns the first attempt that finds something free.
    #[test]
    fn attempts_until_free_napping_against_the_clock() {
        let fake = Fake::new();
        let got = fake.run(
            vec![
                Some(at("2026-10-08T10:02:30Z")),
                Some(at("2026-10-08T10:03:30Z")),
                None,
            ],
            None,
        );
        assert_eq!(got, (3, Ended::Ready));
        assert_eq!(
            *fake.attempts.borrow(),
            [
                ts(NOW),
                ts("2026-10-08T10:02:30Z"),
                ts("2026-10-08T10:03:30Z")
            ]
        );
        let s = Duration::from_secs;
        assert_eq!(*fake.slept.borrow(), [s(60), s(60), s(30), s(60)]);
        assert!(fake.slept.borrow().iter().all(|d| *d <= NAP));
        assert_eq!(fake.statuses.borrow().len(), 4, "a status before each nap");
        // Free at once: no sleep, no status.
        let fake = Fake::new();
        assert_eq!(fake.run(vec![None], None), (1, Ended::Ready));
        assert!(fake.slept.borrow().is_empty() && fake.statuses.borrow().is_empty());
    }

    /// R23 `--wait`: a clock that jumps (a computer that slept) is caught at the next nap: the
    /// attempt is not put off by the time it jumped.
    #[test]
    fn a_clock_that_jumps_is_caught_at_the_next_nap() {
        let fake = Fake::new();
        let three_hours = |_| Duration::from_secs(3 * 3600);
        let got = fake.run_sleeping(
            vec![Some(at("2026-10-08T12:00:00Z")), None],
            None,
            three_hours,
        );
        assert_eq!(got, (2, Ended::Ready));
        assert_eq!(fake.slept.borrow().len(), 1);
        assert_eq!(fake.attempts.borrow()[1], ts("2026-10-08T13:00:00Z"));
    }

    /// R23 `--max-wait` (lane review round 1): a sleep that ends after the deadline (a late
    /// wake) starts no attempt: the wait ends with the last attempt's result. The deadline
    /// was ahead of the attempt's time when remuda went to sleep.
    #[test]
    fn an_oversleep_past_the_deadline_starts_no_attempt() {
        let fake = Fake::new();
        let late = |d| d + Duration::from_secs(4 * 60);
        let got = fake.run_sleeping(
            vec![Some(at("2026-10-08T10:01:00Z")), None],
            Some(ts("2026-10-08T10:02:00Z")),
            late,
        );
        assert_eq!(
            got,
            (
                1,
                Ended::TimeUp {
                    next: ts("2026-10-08T10:01:00Z"),
                    deadline: ts("2026-10-08T10:02:00Z"),
                }
            )
        );
        assert_eq!(fake.attempts.borrow().len(), 1);
    }

    /// R23 `--max-wait` (lane review round 1): a clock set forward past the deadline during a
    /// nap starts no attempt either; one that wakes at the deadline itself still makes it.
    #[test]
    fn a_clock_jump_past_the_deadline_starts_no_attempt() {
        let fake = Fake::new();
        let three_hours = |_| Duration::from_secs(3 * 3600);
        let got = fake.run_sleeping(
            vec![Some(at("2026-10-08T10:30:00Z")), None],
            Some(ts("2026-10-08T10:40:00Z")),
            three_hours,
        );
        assert_eq!(
            got,
            (
                1,
                Ended::TimeUp {
                    next: ts("2026-10-08T10:30:00Z"),
                    deadline: ts("2026-10-08T10:40:00Z"),
                }
            )
        );
        // The next attempt exactly at the deadline is not after it.
        let fake = Fake::new();
        let got = fake.run(
            vec![Some(at("2026-10-08T10:01:00Z")), None],
            Some(ts("2026-10-08T10:01:00Z")),
        );
        assert_eq!(got, (2, Ended::Ready));
    }

    /// R23 `--wait` (lane review round 2): an attempt that fails after a status line was shown
    /// (here `--run` refusing arguments once another provider can launch) ends the wait with its
    /// error, and the line is cleared before the error comes out to be printed.
    #[test]
    fn the_status_is_cleared_before_an_error_comes_out() {
        let fake = Fake::new();
        let mut n = 0;
        let got = until(
            || {
                n += 1;
                match n {
                    1 => Ok((n, Some(at("2026-10-08T10:01:00Z")))),
                    _ => Err("refused"),
                }
            },
            None,
            || fake.now.get(),
            |d| fake.now.set(after(fake.now.get(), d)),
            &mut Recorder(&fake),
        );
        assert_eq!(got, Err("refused"));
        assert_eq!(fake.statuses.borrow().len(), 1, "a line was shown");
        assert_eq!(fake.cleared.get(), 1, "and cleared before the error");
    }

    /// R23 `--wait`: what time does not free ends the wait at once, without a sleep.
    #[test]
    fn never_stops_at_once() {
        let fake = Fake::new();
        let got = fake.run(vec![Some(Wait::Never("excluded".into()))], None);
        assert_eq!(got, (1, Ended::Never("excluded".into())));
        assert!(fake.slept.borrow().is_empty());
    }

    /// R23 `--max-wait`: no attempt starts after the deadline; when the next one would, the
    /// wait ends right away instead of sleeping out time in which nothing would be checked.
    #[test]
    fn no_attempt_after_the_deadline() {
        let s = Duration::from_secs;
        // `--max-wait 0`: one attempt.
        let fake = Fake::new();
        let got = fake.run(
            vec![Some(at("2026-10-08T10:01:00Z"))],
            deadline(ts(NOW), Some(s(0))),
        );
        assert_eq!(
            got,
            (
                1,
                Ended::GaveUp {
                    next: ts("2026-10-08T10:01:00Z")
                }
            )
        );
        assert!(fake.slept.borrow().is_empty());
        // Two attempts fit in ten minutes; a third, an hour later, does not.
        let fake = Fake::new();
        let got = fake.run(
            vec![
                Some(at("2026-10-08T10:10:00Z")),
                Some(at("2026-10-08T11:10:00Z")),
            ],
            deadline(ts(NOW), Some(s(600))),
        );
        assert_eq!(
            got,
            (
                2,
                Ended::GaveUp {
                    next: ts("2026-10-08T11:10:00Z")
                }
            )
        );
        assert_eq!(fake.now.get(), ts("2026-10-08T10:10:00Z"));
    }

    /// The status line names the account, the window, the time to its reset, and the next
    /// check in local time.
    #[test]
    fn status_lines() {
        let w = windows(&[
            ("Week (Fable)", 100.0, Some("2026-10-08T11:12:00Z")),
            ("Week (all models)", 95.0, None),
        ]);
        let on = |w: &Window| Blocker {
            account: "claude:max".into(),
            window: w.clone(),
        };
        let tz = TimeZone::get("Asia/Shanghai").unwrap();
        let wait = Wait::At {
            at: ts("2026-10-08T11:12:30Z"),
            on: on(&w[0]),
        };
        assert_eq!(
            status_line(&wait, ts(NOW), &tz).unwrap(),
            "waiting: claude:max Week (Fable) 100% used, resets in 1h12m; \
             next check Oct 8 19:12 (Ctrl-C stops)"
        );
        let wait = Wait::Unknown {
            at: ts("2026-10-08T10:05:00Z"),
            on: on(&w[1]),
        };
        assert_eq!(
            status_line(&wait, ts(NOW), &tz).unwrap(),
            "waiting: claude:max Week (all models) 95% used, reset unknown; \
             next check Oct 8 18:05 (Ctrl-C stops)"
        );
        assert_eq!(status_line(&Wait::Never("x".into()), ts(NOW), &tz), None);
    }
}
