//! The usage history (SPEC R24): the readings `remuda usage` and the TUI take, a point per
//! window, appended to `state/usage-history.jsonl`; and `remuda usage --history`, which reads
//! them back with the pace of each current window.
//!
//! Every write takes the lock of the state directory first and holds it through reading the
//! file, leaving out what is already there, and appending or compacting: a line appended while
//! a compaction is between its reading and its rename would go to the file being replaced, and
//! two readings of one cache at once would each add the same points.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Result, bail};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};

use super::{Reading, Source, format_percent, format_time};
use crate::owned::{self, Locks};

/// Points older than this many days are dropped by a compaction, and not recorded (R24).
pub const KEEP_DAYS: i64 = 45;
/// A compaction keeps at most this many lines: the ones written first go (R24).
pub const MAX_LINES: usize = 50_000;

const HOUR: SignedDuration = SignedDuration::from_hours(1);

/// [`Point::same`].
type Same<'a> = (
    &'a str,
    &'a str,
    Option<&'a str>,
    Timestamp,
    u64,
    Option<Timestamp>,
);

/// One window of one reading (R24).
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    /// When the usage was read: the cache time, or when a live query answered (R10).
    pub ts: Timestamp,
    /// `provider:name`.
    pub account: String,
    pub label: String,
    pub model: Option<String>,
    pub percent: f64,
    /// The reset, when it was ahead at the reading.
    pub resets_at: Option<Timestamp>,
    pub source: Source,
    /// The window's length in minutes, as codex told it (R10); `None` for claude's windows and
    /// for the points recorded before remuda kept it. Not part of what makes two points the
    /// same reading ([`Point::same`]).
    pub window_minutes: Option<u32>,
}

impl Point {
    fn same_key(&self, other: &Point) -> bool {
        self.account == other.account && self.label == other.label && self.model == other.model
    }

    /// What makes two points the same reading of the same window, recorded once (R24): the
    /// window, `ts`, `percent` (0 and -0 alike), and `resets_at`.
    fn same(&self) -> Same<'_> {
        let percent = if self.percent == 0.0 {
            0
        } else {
            self.percent.to_bits()
        };
        (
            &self.account,
            &self.label,
            self.model.as_deref(),
            self.ts,
            percent,
            self.resets_at,
        )
    }

    /// The point as its line in the history, `\n` included; `window_minutes` only when it is
    /// known.
    pub fn line(&self) -> String {
        let mut value = json!({
            "ts": self.ts.to_string(),
            "account": self.account,
            "label": self.label,
            "model": self.model,
            "percent": self.percent,
            "resets_at": self.resets_at.map(|t| t.to_string()),
            "source": self.source.name(),
        });
        if let (Some(minutes), Some(map)) = (self.window_minutes, value.as_object_mut()) {
            map.insert("window_minutes".to_string(), minutes.into());
        }
        format!("{value}\n")
    }

    /// A line of the history; `None` when it is not one (cut short, or not remuda's). A line
    /// whose `window_minutes` is missing, null, or anything but a whole number of minutes from
    /// 1 to `u32::MAX` is a point of unknown length: the key tells no point apart, and a point is
    /// not lost over it.
    pub fn parse(line: &str) -> Option<Point> {
        let v: Value = serde_json::from_str(line).ok()?;
        let text = |key: &str| v.get(key).and_then(Value::as_str);
        let resets_at = match v.get("resets_at")? {
            Value::Null => None,
            other => Some(other.as_str()?.parse().ok()?),
        };
        let model = match v.get("model")? {
            Value::Null => None,
            other => Some(other.as_str()?.to_string()),
        };
        let window_minutes = v
            .get("window_minutes")
            .and_then(Value::as_u64)
            .and_then(|m| u32::try_from(m).ok())
            .filter(|m| *m > 0);
        Some(Point {
            ts: text("ts")?.parse().ok()?,
            account: text("account")?.to_string(),
            label: text("label")?.to_string(),
            model,
            percent: v.get("percent")?.as_f64()?,
            resets_at,
            source: match text("source")? {
                "cached" => Source::Cached,
                "live" => Source::Live,
                _ => return None,
            },
            window_minutes,
        })
    }
}

/// The points of `account`'s `reading` (R24): a window each, with the percentage it holds.
/// None for a reading whose time is unknown (the same cache read again would be a new point
/// each time), nor for a window that has reset since (its percentage is unknown).
pub fn points(account: &str, reading: &Reading) -> Vec<Point> {
    let Some(ts) = reading.fetched_at else {
        return Vec::new();
    };
    reading
        .windows
        .iter()
        .filter_map(|w| {
            Some(Point {
                ts,
                account: account.to_string(),
                label: w.label.clone(),
                model: w.model.clone(),
                percent: w.used()?,
                resets_at: w.resets_at(),
                source: reading.source,
                window_minutes: w.window_minutes,
            })
        })
        .collect()
}

/// The oldest a point may be at `now` and still be kept.
fn horizon(now: Timestamp) -> Timestamp {
    now.checked_sub(SignedDuration::from_hours(24 * KEEP_DAYS))
        .unwrap_or(Timestamp::MIN)
}

/// What [`record`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Recorded {
    /// Points written.
    pub added: usize,
    /// Lines a compaction removed.
    pub dropped: usize,
}

/// Records `points` in the history of the state directory `state` (R24), under the lock of
/// the directory of the file that is replaced ([`owned::lock_state_file`]: the state
/// directory's, or that of the file a history that is a symlink points at), leaving out each
/// that is already there. With `compact`, also drops the lines older than [`KEEP_DAYS`] at
/// `now` and those that are not points, and keeps at most [`MAX_LINES`] of what remains and
/// the new points together, the first written going first (new points too, when they alone are
/// more): the file is then replaced atomically. Otherwise the new points are one append.
/// Nothing is touched when there is nothing to record. Nothing is written to a history
/// reached through a symlink that the group or others can read (R3), nor to one that is not a
/// regular file. `Err` then, and on a file system without locks (nothing is written), a state
/// directory that cannot be written or locked, or a history that cannot be read: the history
/// is a by-product, and callers say nothing of it. A history that is a symlink is resolved once,
/// when it is locked: the file read, checked, and written is that one, whatever the link is
/// pointed at meanwhile.
pub fn record(
    state: &Path,
    points: Vec<Point>,
    now: Timestamp,
    compact: bool,
    locks: &dyn Locks,
) -> Result<Recorded> {
    record_with(state, points, now, compact, locks, || {})
}

/// [`record`], running `meanwhile` between reading the history and writing it: the tests' way
/// in to that moment.
fn record_with(
    state: &Path,
    points: Vec<Point>,
    now: Timestamp,
    compact: bool,
    locks: &dyn Locks,
    meanwhile: impl FnOnce(),
) -> Result<Recorded> {
    let horizon = horizon(now);
    let points: Vec<Point> = points.into_iter().filter(|p| p.ts >= horizon).collect();
    if points.is_empty() {
        return Ok(Recorded::default());
    }
    let path = owned::usage_history(state);
    let locked = owned::lock_state_file(&path, locks)?;
    locked.check_private()?;
    let text = locked
        .read()?
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let lines: Vec<(&str, Option<Point>)> = text
        .split_inclusive('\n')
        .map(|line| (line, Point::parse(line)))
        .collect();
    let mut known: HashSet<_> = lines
        .iter()
        .filter_map(|(_, p)| p.as_ref().map(Point::same))
        .collect();
    let fresh: Vec<&Point> = points.iter().filter(|p| known.insert(p.same())).collect();
    meanwhile();

    if compact {
        let kept: Vec<String> = lines
            .iter()
            .filter(|(_, p)| p.as_ref().is_some_and(|p| p.ts >= horizon))
            .map(|(line, _)| match line.ends_with('\n') {
                true => line.to_string(),
                false => format!("{line}\n"),
            })
            .collect();
        // The cap is on what the file would hold: what is kept, then the new points.
        let over = (kept.len() + fresh.len()).saturating_sub(MAX_LINES);
        let from_kept = over.min(kept.len());
        let from_fresh = over - from_kept;
        let dropped = lines.len() - (kept.len() - from_kept);
        if dropped > 0 || from_fresh > 0 {
            let fresh = &fresh[from_fresh..];
            let mut bytes: String = kept[from_kept..].concat();
            bytes.extend(fresh.iter().map(|p| p.line()));
            locked.check_private()?;
            locked.replace(bytes.as_bytes())?;
            return Ok(Recorded {
                added: fresh.len(),
                dropped,
            });
        }
    }
    if !fresh.is_empty() {
        let new_lines: String = fresh.iter().map(|p| p.line()).collect();
        // A last line cut short is ended first, so the next point starts a line of its own.
        let start = if text.is_empty() || text.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        locked.append(&format!("{start}{new_lines}"))?;
    }
    Ok(Recorded {
        added: fresh.len(),
        dropped: 0,
    })
}

/// The history's text; `None` when there is none. Opened without blocking and read only as a
/// regular file: a FIFO in its place does not hold `remuda usage` up.
fn read_text(path: &Path) -> Result<Option<String>> {
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !file.metadata()?.file_type().is_file() {
        bail!("{} is not a regular file", path.display());
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// The points in the history at `path`, in the order written; lines that are not points are
/// skipped. `None` when there is no history.
pub fn load(path: &Path) -> Result<Option<Vec<Point>>> {
    Ok(read_text(path)?.map(|text| text.lines().filter_map(Point::parse).collect()))
}

/// How long the window labeled `label` is (R24), from its R10 label: `Session` five hours,
/// `Week …` seven days (claude's `session`, `weekly_all` and `weekly_scoped` limits, its older
/// `five_hour` and `seven_day` fields, its live `Current session` and `Current week` lines;
/// codex's windows of 5 and 168 hours), codex's `<N>h window`, `<N>d window` and `<N>m
/// window` as they say, and a claude limit of unknown kind labeled `five_hour` or
/// `seven_day…` by that kind. Codex's labels give its windows' minutes rounded to whole hours:
/// a point that tells its window's minutes is as long as they say instead ([`Point`]'s
/// `window_minutes`), and the label is read for the points that do not. `None`: unknown.
pub fn window_length(label: &str) -> Option<SignedDuration> {
    let base = match label.strip_suffix(')').and_then(|l| l.rsplit_once(" (")) {
        Some((base, _)) => base,
        None => label,
    };
    if base == "Session" || base == "five_hour" {
        return Some(SignedDuration::from_hours(5));
    }
    if base == "Week" || base.starts_with("seven_day") {
        return Some(SignedDuration::from_hours(7 * 24));
    }
    let count = base.strip_suffix(" window")?;
    // Only the ASCII units, stripped as such: a label is any text a cache gives (R10).
    let (number, per) = [('m', 1), ('h', 60), ('d', 24 * 60)]
        .into_iter()
        .find_map(|(unit, per)| Some((count.strip_suffix(unit)?, per)))?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let minutes = number.parse::<i64>().ok()?.checked_mul(per)?;
    // No longer than a window can be and still be counted in seconds and placed in time.
    (1..=MAX_WINDOW_MINUTES)
        .contains(&minutes)
        .then(|| SignedDuration::from_mins(minutes))
}

/// The longest window a pace is given for: ten years. A label claiming more is of unknown
/// length (R24), and so is a point telling more.
const MAX_WINDOW_MINUTES: i64 = 10 * 366 * 24 * 60;

/// How long the window of `series` (one key's points, in time order) is (R24): what its latest
/// point tells (codex's minutes), else what its label says ([`window_length`]). Points recorded
/// before remuda kept the minutes tell none, so a key's points may disagree: the latest counts,
/// for its windows and for the pace of the current one (whose latest point it is).
fn series_length(series: &[&Point]) -> Option<SignedDuration> {
    let latest = series.last()?;
    match latest.window_minutes {
        Some(minutes) => (1..=MAX_WINDOW_MINUTES)
            .contains(&i64::from(minutes))
            .then(|| SignedDuration::from_mins(i64::from(minutes))),
        None => window_length(&latest.label),
    }
}

/// One window of a key's points: a run of points in time order whose resets agree.
struct Group<'a> {
    points: Vec<&'a Point>,
    /// The latest reset a point of it told.
    reset: Option<Timestamp>,
}

/// `points` (of one key, in time order) by window (R24). A point starts a new window when it
/// was read at or after the reset of the one before, or when its reset is further than
/// `slack` from it: claude's live resets are told to the minute or the hour, its cache's to
/// the second, and the next window resets at least a window's length later. A point without
/// a reset stays in the window before.
fn windows<'a>(points: &[&'a Point], slack: SignedDuration) -> Vec<Group<'a>> {
    let mut groups: Vec<Group<'a>> = Vec::new();
    for &point in points {
        let joins = groups.last().is_some_and(|g| match g.reset {
            None => true,
            Some(reset) => {
                point.ts < reset
                    && point
                        .resets_at
                        .is_none_or(|r| r.duration_since(reset).abs() <= slack)
            }
        });
        if !joins {
            groups.push(Group {
                points: Vec::new(),
                reset: None,
            });
        }
        let group = groups.last_mut().expect("a group was pushed");
        group.points.push(point);
        if point.resets_at.is_some() {
            group.reset = point.resets_at;
        }
    }
    groups
}

/// The pace of a window of length `length` that resets at `reset`, from its latest point
/// (R24): how much of it had elapsed when that point was read, and what that rate comes to.
/// The window starts at `reset − length`, and `e = (ts − start) / length`; the latest point
/// was read before its reset, so `e < 1`. A point read before the start (`e ≤ 0`) gets no
/// projection.
pub fn pace(
    latest: &Point,
    reset: Timestamp,
    length: SignedDuration,
    tz: &TimeZone,
) -> Option<String> {
    let length = length.as_secs_f64();
    if length <= 0.0 {
        return None;
    }
    let start = reset.as_second() as f64 - length;
    let elapsed = latest.ts.as_second() as f64 - start;
    let e = elapsed / length;
    let used = latest.percent;
    let shown = (e * 100.0).floor().clamp(0.0, 100.0);
    let head = format!(
        "used {} with {shown:.0}% of the window elapsed",
        format_percent(used)
    );
    let resets = format_time(reset, tz);
    if e <= 0.0 {
        return Some(head);
    }
    if used >= 100.0 {
        return Some(format!("{head}: limit reached (resets {resets})"));
    }
    if used / 100.0 > e {
        let full = start + elapsed * 100.0 / used;
        let full = Timestamp::from_second(full.floor() as i64).ok()?;
        Some(format!(
            "{head}: ahead of an even pace; at this pace 100% by {} (resets {resets})",
            format_time(full, tz)
        ))
    } else {
        Some(format!(
            "{head}: behind an even pace; at this pace {:.0}% at reset",
            (used / e).round()
        ))
    }
}

/// `remuda usage --history` (R24): for each of `accounts` (in order), each window it has
/// points for in the last `days` days, the current window point by point and with its pace,
/// the windows before it a line each. All times in `tz`.
pub fn report(
    points: &[Point],
    accounts: &[String],
    days: u32,
    now: Timestamp,
    tz: &TimeZone,
) -> String {
    let since = now
        .checked_sub(SignedDuration::from_hours(24 * i64::from(days)))
        .unwrap_or(Timestamp::MIN);
    let blocks: Vec<String> = accounts
        .iter()
        .map(|account| {
            let mine: Vec<&Point> = points
                .iter()
                .filter(|p| &p.account == account && p.ts >= since)
                .collect();
            if mine.is_empty() {
                let span = if days == 1 { "day" } else { "days" };
                return format!("{account}  no usage history in the last {days} {span}\n");
            }
            let mut keys: Vec<&Point> = Vec::new();
            for p in &mine {
                if !keys.iter().any(|k| k.same_key(p)) {
                    keys.push(p);
                }
            }
            let mut out = format!("{account}\n");
            for key in keys {
                let mut series: Vec<&Point> =
                    mine.iter().copied().filter(|p| p.same_key(key)).collect();
                series.sort_by_key(|p| p.ts);
                out.push_str(&key_section(&series, now, tz));
            }
            out
        })
        .collect();
    blocks.join("\n")
}

/// One window key's section of [`report`]: `series` in time order.
fn key_section(series: &[&Point], now: Timestamp, tz: &TimeZone) -> String {
    let label = &series[0].label;
    let length = series_length(series);
    let slack = length.map_or(HOUR, |l| (l / 2).min(HOUR));
    let groups = windows(series, slack);
    let last = groups.len() - 1;
    let mut out = format!("  {label}\n");
    for (n, group) in groups.iter().enumerate() {
        let current = n == last && group.reset.is_none_or(|r| r > now);
        let peak = group
            .points
            .iter()
            .map(|p| p.percent)
            .fold(f64::NEG_INFINITY, f64::max);
        if !current {
            let line = match group.reset {
                Some(r) if r <= now => format!("window ended {}", format_time(r, tz)),
                Some(r) => format!("window to {}", format_time(r, tz)),
                None => "earlier window".to_string(),
            };
            out.push_str(&format!("    {line}: peaked {}\n", format_percent(peak)));
            continue;
        }
        let width = group
            .points
            .iter()
            .map(|p| format_percent(p.percent).len())
            .max()
            .unwrap_or(0);
        for p in &group.points {
            let percent = format_percent(p.percent);
            let line = match p.resets_at {
                Some(r) => format!(
                    "    {}  {percent:>width$}  resets {}",
                    format_time(p.ts, tz),
                    format_time(r, tz)
                ),
                None => format!("    {}  {percent:>width$}", format_time(p.ts, tz)),
            };
            out.push_str(&line);
            out.push('\n');
        }
        let latest = group.points.last().expect("a group has a point");
        if let (Some(reset), Some(length)) = (group.reset, length)
            && let Some(pace) = pace(latest, reset, length, tz)
        {
            out.push_str(&format!("    {pace}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::sync::{Arc, Barrier};

    use super::*;
    use crate::owned::{Flock, NoLocks};
    use crate::usage::{CachedUsage, Resets, Snapshot, UsageRow};

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn row(label: &str, percent: f64, resets: Option<&str>) -> UsageRow {
        UsageRow {
            label: label.into(),
            percent,
            severity: None,
            resets: resets.map(|r| Resets::At(ts(r))),
            window_minutes: None,
        }
    }

    fn point(account: &str, label: &str, at: &str, percent: f64, resets: Option<&str>) -> Point {
        Point {
            ts: ts(at),
            account: account.into(),
            label: label.into(),
            model: crate::usage::Snapshot::live(&[row(label, 0.0, None)], ts(at))
                .at(ts(at))
                .windows[0]
                .model
                .clone(),
            percent,
            resets_at: resets.map(ts),
            source: Source::Cached,
            window_minutes: None,
        }
    }

    fn lines(path: &Path) -> Vec<Point> {
        load(path).unwrap().unwrap_or_default()
    }

    const NOW: &str = "2026-10-08T12:00:00Z";

    /// R24: a point per window, at the reading's time (the cache time, not now), with the
    /// model of its label and the reset when it is ahead. A window that has reset since is not
    /// recorded, nor is a reading whose time is unknown.
    #[test]
    fn points_are_the_windows_of_a_reading_at_its_time() {
        let cached = CachedUsage {
            fetched_at: Some(ts("2026-10-08T09:00:00Z")),
            rows: vec![
                row("Session", 40.0, Some("2026-10-08T11:00:00Z")),
                row("Week (all models)", 71.0, Some("2026-10-09T12:59:00Z")),
                row("Week (Fable)", 12.5, None),
            ],
        };
        let reading = Snapshot::cached(&cached).at(ts(NOW));
        let got = points("claude:max", &reading);
        assert_eq!(
            got,
            [
                Point {
                    ts: ts("2026-10-08T09:00:00Z"),
                    account: "claude:max".into(),
                    label: "Week (all models)".into(),
                    model: None,
                    percent: 71.0,
                    resets_at: Some(ts("2026-10-09T12:59:00Z")),
                    source: Source::Cached,
                    window_minutes: None,
                },
                Point {
                    ts: ts("2026-10-08T09:00:00Z"),
                    account: "claude:max".into(),
                    label: "Week (Fable)".into(),
                    model: Some("Fable".into()),
                    percent: 12.5,
                    resets_at: None,
                    source: Source::Cached,
                    window_minutes: None,
                },
            ],
            "the session reset at 11:00, after the cache and before now: not recorded"
        );
        let unknown = CachedUsage {
            fetched_at: None,
            ..cached
        };
        assert!(points("claude:max", &Snapshot::cached(&unknown).at(ts(NOW))).is_empty());
    }

    /// R24: a line is the schema, and reads back as the point it was.
    #[test]
    fn a_point_is_one_line_of_json() {
        let mut p = point(
            "codex:work",
            "Week (Fable)",
            "2026-10-08T09:00:00.528Z",
            71.0,
            Some("2026-10-09T12:59:00Z"),
        );
        p.source = Source::Live;
        let line = p.line();
        assert!(line.ends_with('\n') && line.matches('\n').count() == 1);
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            v,
            json!({"ts": "2026-10-08T09:00:00.528Z", "account": "codex:work",
                   "label": "Week (Fable)", "model": "Fable", "percent": 71.0,
                   "resets_at": "2026-10-09T12:59:00Z", "source": "live"})
        );
        assert_eq!(Point::parse(&line), Some(p.clone()));
        for bad in ["", "{", "{\"ts\": \"x\"}", "[1]", "{\"ts\": 1}"] {
            assert_eq!(Point::parse(bad), None, "{bad}");
        }

        // A codex window's minutes, when known, are one more key; read back as they were.
        p.window_minutes = Some(90);
        let line = p.line();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["window_minutes"], json!(90));
        assert_eq!(v.as_object().unwrap().len(), 8);
        assert_eq!(Point::parse(&line), Some(p.clone()));
        // Null, and anything but a whole number of minutes in range, reads as unknown: the line
        // is still the point it was.
        let with = |minutes: Value| {
            let mut v = v.clone();
            v["window_minutes"] = minutes;
            Point::parse(&v.to_string())
        };
        assert_eq!(with(Value::Null).unwrap().window_minutes, None);
        assert_eq!(
            with(json!(u32::MAX)).unwrap().window_minutes,
            Some(u32::MAX)
        );
        for bad in [
            json!("90"),
            json!(0),
            json!(-5),
            json!(1.5),
            json!(u64::from(u32::MAX) + 1),
        ] {
            assert_eq!(
                with(bad.clone()),
                Some(Point {
                    window_minutes: None,
                    ..p.clone()
                }),
                "{bad}"
            );
        }
    }

    /// R24: the points of a codex reading tell its windows' minutes, those of claude's none;
    /// lines written before remuda kept them (no `window_minutes`), and lines whose minutes
    /// cannot be read, are read as points of unknown length, and are the same reading as a new point that tells them (the minutes
    /// are not part of what makes two points one): not recorded twice. A compaction keeps each
    /// line as it was, the minutes with it.
    #[test]
    fn window_minutes_are_recorded_and_old_lines_still_read() {
        let mut codex_row = row("2h window", 40.0, Some("2026-10-08T12:30:00Z"));
        codex_row.window_minutes = Some(90);
        let cached = CachedUsage {
            fetched_at: Some(ts("2026-10-08T11:30:00Z")),
            rows: vec![codex_row, row("Session", 10.0, None)],
        };
        let got = points("codex:work", &Snapshot::cached(&cached).at(ts(NOW)));
        let minutes: Vec<Option<u32>> = got.iter().map(|p| p.window_minutes).collect();
        assert_eq!(minutes, [Some(90), None]);

        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let path = owned::usage_history(&state);
        fs::create_dir(&state).unwrap();
        // A line as an earlier remuda wrote it: no `window_minutes` key at all.
        let old_line = "{\"ts\":\"2026-10-08T11:30:00Z\",\"account\":\"codex:work\",\
                        \"label\":\"2h window\",\"model\":null,\"percent\":40.0,\
                        \"resets_at\":\"2026-10-08T12:30:00Z\",\"source\":\"cached\"}\n";
        // One whose `window_minutes` cannot be read: a point all the same, of unknown length.
        let odd_line = "{\"ts\":\"2026-10-08T11:35:00Z\",\"account\":\"codex:work\",\
                        \"label\":\"2h window\",\"model\":null,\"percent\":45.0,\
                        \"resets_at\":\"2026-10-08T12:30:00Z\",\"source\":\"cached\",\
                        \"window_minutes\":\"90\"}\n";
        let stale = point("codex:work", "2h window", "2026-08-01T00:00:00Z", 1.0, None);
        fs::write(&path, stale.line() + old_line + odd_line).unwrap();
        let odd = Point::parse(odd_line).unwrap();
        assert_eq!(odd.window_minutes, None);
        let old = Point::parse(old_line).unwrap();
        assert_eq!(old.window_minutes, None);
        assert_eq!(
            Point {
                window_minutes: Some(90),
                ..old.clone()
            },
            got[0]
        );
        // The same reading, now with its minutes: already there.
        let now = ts(NOW);
        assert_eq!(
            record(&state, vec![got[0].clone()], now, false, &Flock)
                .unwrap()
                .added,
            0
        );
        // A new reading with its minutes, compacted in: the old lines kept as they were (the
        // one with minutes that cannot be read too), the new one with its key.
        let mut new = got[0].clone();
        new.ts = ts("2026-10-08T11:45:00Z");
        new.percent = 55.0;
        let done = record(&state, vec![new.clone()], now, true, &Flock).unwrap();
        assert_eq!(
            done,
            Recorded {
                added: 1,
                dropped: 1
            }
        );
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, format!("{old_line}{odd_line}{}", new.line()));
        assert!(
            new.line().contains("\"window_minutes\":90"),
            "{}",
            new.line()
        );
        assert_eq!(lines(&path), [old, odd, new]);
    }

    /// R24: what is already recorded is not recorded again: the same cache read twice is one
    /// point a window, also with another reading of the window recorded in between (a live
    /// answer); a reading that differs in its time, percentage or reset is a point.
    #[test]
    fn a_reading_already_recorded_is_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let path = owned::usage_history(&state);
        let now = ts(NOW);
        let cached = point("claude:max", "Session", "2026-10-08T09:00:00Z", 40.0, None);
        let rec = |p: Vec<Point>| record(&state, p, now, false, &Flock).unwrap().added;
        assert_eq!(rec(vec![cached.clone()]), 1);
        assert_eq!(rec(vec![cached.clone()]), 0);
        let mut live = point("claude:max", "Session", "2026-10-08T10:00:00Z", 45.0, None);
        live.source = Source::Live;
        assert_eq!(rec(vec![live.clone()]), 1);
        assert_eq!(
            rec(vec![cached.clone()]),
            0,
            "not the last point, still recorded"
        );
        // Another time, percentage, reset; another account, label: each a point.
        let mut others = Vec::new();
        for change in 0..5 {
            let mut p = cached.clone();
            match change {
                0 => p.ts = ts("2026-10-08T09:30:00Z"),
                1 => p.percent = 41.0,
                2 => p.resets_at = Some(ts("2026-10-08T13:00:00Z")),
                3 => p.account = "claude:team".into(),
                _ => p.label = "Week (all models)".into(),
            }
            others.push(p);
        }
        // Twice in one batch: once.
        others.push(others[0].clone());
        assert_eq!(rec(others.clone()), 5);
        let mut want = vec![cached, live];
        want.extend(others.into_iter().take(5));
        assert_eq!(lines(&path), want);
        let meta = fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    /// R24: nothing to record touches nothing: no state directory is made.
    #[test]
    fn nothing_to_record_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("remuda/state");
        let old = point("claude:max", "Session", "2026-08-01T00:00:00Z", 1.0, None);
        assert_eq!(
            record(&state, vec![old], ts(NOW), false, &Flock).unwrap(),
            Recorded::default(),
            "older than the 45 days kept: not recorded"
        );
        assert_eq!(
            record(&state, vec![], ts(NOW), true, &Flock).unwrap(),
            Recorded::default()
        );
        assert!(!dir.path().join("remuda").exists());
    }

    /// R24: a compaction drops the points older than 45 days and the lines that are not
    /// points, and replaces the file with the rest and the new points; with nothing to drop,
    /// the new points are appended to the same file. Without `compact` (the TUI) nothing is
    /// dropped.
    #[test]
    fn a_compaction_drops_old_points_and_broken_lines() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let path = owned::usage_history(&state);
        fs::create_dir(&state).unwrap();
        let old = point("claude:max", "Session", "2026-08-23T11:59:59Z", 1.0, None);
        let edge = point("claude:max", "Session", "2026-08-24T12:00:00Z", 2.0, None);
        let recent = point("claude:max", "Session", "2026-10-08T09:00:00Z", 3.0, None);
        let text = format!("{}not json\n{}{}", old.line(), edge.line(), recent.line());
        fs::write(&path, &text).unwrap();
        let new = point("claude:max", "Session", "2026-10-08T11:00:00Z", 4.0, None);
        let now = ts(NOW);

        // The TUI's recording appends, and drops nothing.
        let inode = fs::metadata(&path).unwrap().ino();
        let got = record(&state, vec![new.clone()], now, false, &Flock).unwrap();
        assert_eq!(
            got,
            Recorded {
                added: 1,
                dropped: 0
            }
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            text.clone() + &new.line()
        );
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);

        // `remuda usage`'s compacts: exactly 45 days is kept.
        let newer = point("claude:max", "Session", "2026-10-08T11:30:00Z", 5.0, None);
        let got = record(&state, vec![newer.clone()], now, true, &Flock).unwrap();
        assert_eq!(
            got,
            Recorded {
                added: 1,
                dropped: 2
            }
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            [&edge, &recent, &new, &newer].map(Point::line).concat()
        );
        assert_ne!(fs::metadata(&path).unwrap().ino(), inode, "replaced");
        let names: Vec<_> = fs::read_dir(&state)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["usage-history.jsonl"], "no temporary file left");

        // Nothing to drop: appended in place.
        let inode = fs::metadata(&path).unwrap().ino();
        let last = point("claude:max", "Session", "2026-10-08T11:45:00Z", 6.0, None);
        let got = record(&state, vec![last], now, true, &Flock).unwrap();
        assert_eq!(
            got,
            Recorded {
                added: 1,
                dropped: 0
            }
        );
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    }

    /// R24: a last line cut short (a write that did not end) does not swallow the next point.
    #[test]
    fn a_line_cut_short_is_ended_before_appending() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let path = owned::usage_history(&state);
        fs::create_dir(&state).unwrap();
        let first = point("claude:max", "Session", "2026-10-08T09:00:00Z", 3.0, None);
        fs::write(&path, format!("{}{{\"ts\": \"2026", first.line())).unwrap();
        let new = point("claude:max", "Session", "2026-10-08T11:00:00Z", 4.0, None);
        record(&state, vec![new.clone()], ts(NOW), false, &Flock).unwrap();
        assert_eq!(lines(&path), [first.clone(), new.clone()]);
        // A compaction drops it, and ends what it keeps.
        let newer = point("claude:max", "Session", "2026-10-08T11:30:00Z", 5.0, None);
        let got = record(&state, vec![newer.clone()], ts(NOW), true, &Flock).unwrap();
        assert_eq!(got.dropped, 1);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            [&first, &new, &newer].map(Point::line).concat()
        );
    }

    /// R24: beyond the most lines kept, a compaction drops the first written.
    #[test]
    fn a_compaction_keeps_at_most_the_last_lines() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let path = owned::usage_history(&state);
        fs::create_dir(&state).unwrap();
        let base = ts("2026-10-01T00:00:00Z");
        let at = |n: i64| base.checked_add(SignedDuration::from_secs(n)).unwrap();
        let text: String = (0..MAX_LINES as i64)
            .map(|n| {
                let mut p = point("claude:max", "Session", NOW, 1.0, None);
                p.ts = at(n);
                p.line()
            })
            .collect();
        fs::write(&path, text).unwrap();
        let mut new = point("claude:max", "Session", NOW, 2.0, None);
        new.ts = at(MAX_LINES as i64 + 5);
        let got = record(&state, vec![new.clone()], ts(NOW), true, &Flock).unwrap();
        assert_eq!(
            got,
            Recorded {
                added: 1,
                dropped: 1
            }
        );
        let kept = lines(&path);
        assert_eq!(kept.len(), MAX_LINES);
        assert_eq!(kept[0].ts, at(1), "the first written went");
        assert_eq!(kept.last(), Some(&new));
    }

    /// R24, R3: on a file system without locks nothing is recorded, and a state directory
    /// that cannot be written in fails without a word from `record` (its callers say nothing).
    #[test]
    fn without_a_lock_or_a_state_directory_nothing_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let p = point("claude:max", "Session", "2026-10-08T09:00:00Z", 3.0, None);
        assert!(record(&state, vec![p.clone()], ts(NOW), true, &NoLocks).is_err());
        assert!(record(&state, vec![p.clone()], ts(NOW), false, &NoLocks).is_err());
        assert!(!owned::usage_history(&state).exists());
        let blocked = dir.path().join("blocked");
        fs::write(&blocked, "").unwrap();
        assert!(record(&blocked.join("state"), vec![p], ts(NOW), true, &Flock).is_err());
    }

    /// R24: a history that is not a regular file is not read (a FIFO would block) and not
    /// written to.
    #[test]
    fn a_history_that_is_not_a_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let path = owned::usage_history(&state);
        fs::create_dir(&state).unwrap();
        let fifo = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let p = point("claude:max", "Session", "2026-10-08T09:00:00Z", 3.0, None);
        assert!(record(&state, vec![p], ts(NOW), true, &Flock).is_err());
        assert!(load(&path).is_err());
    }

    /// R24, R3: appends and compactions at the same time lose no line. Each takes the lock of
    /// the state directory over its reading and its writing; without it, a point appended
    /// between a compaction's reading and its rename goes to the file being replaced.
    #[test]
    fn concurrent_appends_and_compactions_lose_no_point() {
        const WRITERS: usize = 6;
        const EACH: usize = 40;
        const COMPACTIONS: usize = 40;
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(dir.path().join("state"));
        let now = ts(NOW);
        let base = ts("2026-10-08T00:00:00Z");
        let barrier = Arc::new(Barrier::new(WRITERS + 1));
        let mut threads = Vec::new();
        for w in 0..WRITERS {
            let (state, barrier) = (Arc::clone(&state), Arc::clone(&barrier));
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                for n in 0..EACH {
                    let mut p = point(&format!("claude:w{w}"), "Session", NOW, 1.0, None);
                    p.ts = base
                        .checked_add(SignedDuration::from_secs(n as i64))
                        .unwrap();
                    record(&state, vec![p], now, false, &Flock).unwrap();
                }
            }));
        }
        let (compactor, b) = (Arc::clone(&state), Arc::clone(&barrier));
        threads.push(std::thread::spawn(move || {
            b.wait();
            let path = owned::usage_history(&compactor);
            for n in 0..COMPACTIONS {
                // A point too old to keep, written as a line of its own (under the lock, as
                // every write is), so that the next compaction has something to drop and
                // replaces the file.
                {
                    let locked = owned::lock_state_file(&path, &Flock).unwrap();
                    let old = point("claude:old", "Session", "2026-01-01T00:00:00Z", 1.0, None);
                    locked.append(&old.line()).unwrap();
                }
                let mut p = point("claude:compactor", "Session", NOW, 1.0, None);
                p.ts = base
                    .checked_add(SignedDuration::from_secs(n as i64))
                    .unwrap();
                let got = record(&compactor, vec![p], now, true, &Flock).unwrap();
                assert!(got.dropped >= 1, "{got:?}");
            }
        }));
        for t in threads {
            t.join().unwrap();
        }
        let got = lines(&owned::usage_history(&state));
        for w in 0..WRITERS {
            let account = format!("claude:w{w}");
            let mine = got.iter().filter(|p| p.account == account).count();
            assert_eq!(mine, EACH, "{account}: points lost");
        }
        let compacted = got
            .iter()
            .filter(|p| p.account == "claude:compactor")
            .count();
        assert_eq!(compacted, COMPACTIONS);
        assert!(got.iter().all(|p| p.account != "claude:old"));
    }

    /// R24, R3: two state directories whose histories are symlinks to one file take one lock,
    /// that of the directory of the file replaced: their appends and compactions at the same
    /// time lose no point (with a lock on each state directory, each compaction replaced the
    /// file with what it alone had read).
    #[test]
    fn histories_linked_to_one_file_lose_no_point() {
        use std::os::unix::fs::symlink;
        const EACH: usize = 30;
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("elsewhere/history.jsonl");
        fs::create_dir(dir.path().join("elsewhere")).unwrap();
        fs::write(&shared, "").unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o600)).unwrap();
        let states: Vec<_> = ["a", "b"]
            .iter()
            .map(|home| {
                let state = dir.path().join(home).join("state");
                fs::create_dir_all(&state).unwrap();
                symlink(&shared, owned::usage_history(&state)).unwrap();
                state
            })
            .collect();
        let now = ts(NOW);
        let base = ts("2026-10-08T00:00:00Z");
        let barrier = Arc::new(Barrier::new(states.len()));
        let threads: Vec<_> = states
            .iter()
            .enumerate()
            .map(|(n, state)| {
                let (state, barrier) = (state.clone(), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    barrier.wait();
                    for i in 0..EACH {
                        // A point too old to keep, appended through this home, so that this
                        // home's next compaction replaces the shared file.
                        let mut old = point("claude:old", "Session", NOW, 1.0, None);
                        old.ts = ts("2026-01-01T00:00:00Z");
                        let path = owned::usage_history(&state);
                        {
                            let locked = owned::lock_state_file(&path, &Flock).unwrap();
                            locked.append(&old.line()).unwrap();
                        }
                        let mut p = point(&format!("claude:h{n}"), "Session", NOW, 1.0, None);
                        p.ts = base
                            .checked_add(SignedDuration::from_secs(i as i64))
                            .unwrap();
                        record(&state, vec![p], now, true, &Flock).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let got = lines(&shared);
        for n in 0..states.len() {
            let mine = got
                .iter()
                .filter(|p| p.account == format!("claude:h{n}"))
                .count();
            assert_eq!(mine, EACH, "claude:h{n}: points lost");
        }
        assert!(
            fs::symlink_metadata(owned::usage_history(&states[0]))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(
            fs::metadata(&shared).unwrap().permissions().mode() & 0o777,
            0o600,
            "the target keeps its mode"
        );
    }

    /// R24, R3: a history that is a symlink is resolved once, when it is locked. Pointed at
    /// another file after it was read and checked, before it is written, the write still goes
    /// to the file locked, read, and checked: the other file, someone else's committed history
    /// that others can read, is neither replaced nor appended to. A history that is a file of
    /// the state directory and is replaced by a symlink meanwhile is not written through it.
    #[test]
    fn a_history_pointed_elsewhere_after_it_was_read_is_not_written_elsewhere() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        fs::create_dir(&state).unwrap();
        let link = owned::usage_history(&state);
        let (a, b) = (dir.path().join("a/h.jsonl"), dir.path().join("b/h.jsonl"));
        fs::create_dir(a.parent().unwrap()).unwrap();
        fs::create_dir(b.parent().unwrap()).unwrap();
        let expired = point("claude:old", "Session", "2026-01-01T00:00:00Z", 1.0, None);
        let kept = point("claude:a", "Session", "2026-10-08T08:00:00Z", 2.0, None);
        let committed = point("claude:b", "Session", "2026-10-08T09:00:00Z", 3.0, None);
        let new = point(
            "claude:writer",
            "Session",
            "2026-10-08T11:00:00Z",
            4.0,
            None,
        );
        let retarget = |to: &Path| {
            let temp = state.join("link.tmp");
            symlink(to, &temp).unwrap();
            fs::rename(&temp, &link).unwrap();
        };
        for compact in [true, false] {
            fs::write(&a, expired.line() + &kept.line()).unwrap();
            fs::set_permissions(&a, fs::Permissions::from_mode(0o600)).unwrap();
            fs::write(&b, committed.line()).unwrap();
            fs::set_permissions(&b, fs::Permissions::from_mode(0o644)).unwrap();
            retarget(&a);
            let got = record_with(&state, vec![new.clone()], ts(NOW), compact, &Flock, || {
                retarget(&b)
            })
            .unwrap();
            if compact {
                assert_eq!(
                    got,
                    Recorded {
                        added: 1,
                        dropped: 1
                    }
                );
                assert_eq!(lines(&a), [kept.clone(), new.clone()]);
            } else {
                assert_eq!(
                    got,
                    Recorded {
                        added: 1,
                        dropped: 0
                    }
                );
                assert_eq!(lines(&a), [expired.clone(), kept.clone(), new.clone()]);
            }
            assert_eq!(
                fs::read_to_string(&b).unwrap(),
                committed.line(),
                "compact {compact}: the file the link was pointed at was written"
            );
            assert_eq!(
                fs::metadata(&b).unwrap().permissions().mode() & 0o777,
                0o644
            );
            assert_eq!(
                fs::read_link(&link).unwrap(),
                b,
                "the user's link stays as they set it"
            );
        }

        // A history of the state directory itself, replaced by a symlink meanwhile.
        for compact in [true, false] {
            fs::remove_file(&link).unwrap();
            fs::write(&link, expired.line()).unwrap();
            fs::write(&b, committed.line()).unwrap();
            let got = record_with(&state, vec![new.clone()], ts(NOW), compact, &Flock, || {
                retarget(&b)
            });
            assert!(got.is_err(), "compact {compact}: {got:?}");
            assert_eq!(
                fs::read_to_string(&b).unwrap(),
                committed.line(),
                "compact {compact}"
            );
        }
    }

    /// R24, R3: the file a history that is a symlink points at, removed after it was read and
    /// before it is written, is not created again: neither by an append nor by a compaction,
    /// whose replacement would otherwise give it a mode of its own (the umask's). A history of
    /// the state directory itself, removed meanwhile, is created again as R3 creates the files
    /// of `state/`: mode 0600, holding what was to be written.
    #[test]
    fn a_history_removed_before_an_append_is_not_created_again() {
        removed_after_it_was_read(false);
    }

    /// [`a_history_removed_before_an_append_is_not_created_again`], for a compaction.
    #[test]
    fn a_history_removed_before_a_compaction_is_not_created_again() {
        removed_after_it_was_read(true);
    }

    fn removed_after_it_was_read(compact: bool) {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        fs::create_dir(&state).unwrap();
        let path = owned::usage_history(&state);
        let target = dir.path().join("a/h.jsonl");
        fs::create_dir(target.parent().unwrap()).unwrap();
        let expired = point("claude:old", "Session", "2026-01-01T00:00:00Z", 1.0, None);
        let kept = point("claude:a", "Session", "2026-10-08T08:00:00Z", 2.0, None);
        let new = point(
            "claude:writer",
            "Session",
            "2026-10-08T11:00:00Z",
            4.0,
            None,
        );
        symlink(&target, &path).unwrap();
        fs::write(&target, expired.line() + &kept.line()).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let got = record_with(&state, vec![new.clone()], ts(NOW), compact, &Flock, || {
            fs::remove_file(&target).unwrap()
        });
        let e = got.expect_err(&format!("compact {compact}"));
        assert!(!target.exists(), "compact {compact}: created again ({e:#})");
        let left: Vec<_> = fs::read_dir(target.parent().unwrap()).unwrap().collect();
        assert!(left.is_empty(), "compact {compact}: {left:?}");

        // A history in the state directory: made again, the user's alone.
        fs::remove_file(&path).unwrap();
        fs::write(&path, expired.line() + &kept.line()).unwrap();
        let got = record_with(&state, vec![new.clone()], ts(NOW), compact, &Flock, || {
            fs::remove_file(&path).unwrap()
        })
        .unwrap();
        assert_eq!(got.added, 1, "compact {compact}");
        let want = match compact {
            // The compaction writes what it kept of what it read, and the new point.
            true => vec![kept.clone(), new.clone()],
            false => vec![new.clone()],
        };
        assert_eq!(lines(&path), want, "compact {compact}");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "compact {compact}");
    }

    /// R24, R3: a history that is a symlink to a file the group or others can read gets no
    /// point, whether the point would be appended or the file compacted with it in; the file
    /// and its mode are left as they are.
    #[test]
    fn a_linked_history_others_can_read_is_not_written() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        fs::create_dir(&state).unwrap();
        let target = dir.path().join("shared.jsonl");
        symlink(&target, owned::usage_history(&state)).unwrap();
        let new = point("claude:max", "Session", "2026-10-08T11:00:00Z", 4.0, None);
        let old = point("claude:max", "Session", "2026-01-01T00:00:00Z", 1.0, None);
        for (case, text, compact) in [
            ("append", String::new(), false),
            ("append, compacting nothing", String::new(), true),
            ("compaction", old.line(), true),
        ] {
            fs::write(&target, &text).unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
            let got = record(&state, vec![new.clone()], ts(NOW), compact, &Flock);
            let e = got.expect_err(case);
            assert!(e.to_string().contains("group or others"), "{case}: {e}");
            assert_eq!(fs::read_to_string(&target).unwrap(), text, "{case}");
            assert_eq!(
                fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                0o644,
                "{case}"
            );
        }
        // Made private by its owner, it is written to.
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let got = record(&state, vec![new.clone()], ts(NOW), true, &Flock).unwrap();
        assert_eq!(
            got,
            Recorded {
                added: 1,
                dropped: 1
            }
        );
        assert_eq!(lines(&target), [new]);
    }

    /// R24: the cap is on what the file holds, the new points included: a batch larger than it
    /// keeps its last points, and the counts say what was written and what went.
    #[test]
    fn the_cap_holds_for_a_large_batch() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let path = owned::usage_history(&state);
        let base = ts("2026-10-01T00:00:00Z");
        let at = |n: usize| {
            base.checked_add(SignedDuration::from_secs(n as i64))
                .unwrap()
        };
        let batch = |from: usize, count: usize| -> Vec<Point> {
            (from..from + count)
                .map(|n| {
                    let mut p = point("claude:max", "Session", NOW, 1.0, None);
                    p.ts = at(n);
                    p
                })
                .collect()
        };
        let got = record(&state, batch(0, MAX_LINES + 2), ts(NOW), true, &Flock).unwrap();
        assert_eq!(
            got,
            Recorded {
                added: MAX_LINES,
                dropped: 0
            }
        );
        let kept = lines(&path);
        assert_eq!(kept.len(), MAX_LINES);
        assert_eq!(kept[0].ts, at(2), "the first of the batch went");
        // A batch that alone is over the cap, on a history: all of the history goes, and the
        // batch's first points.
        let got = record(
            &state,
            batch(MAX_LINES + 2, MAX_LINES + 1),
            ts(NOW),
            true,
            &Flock,
        )
        .unwrap();
        assert_eq!(
            got,
            Recorded {
                added: MAX_LINES,
                dropped: MAX_LINES
            }
        );
        let kept = lines(&path);
        assert_eq!(kept.len(), MAX_LINES);
        assert_eq!(kept[0].ts, at(MAX_LINES + 3));
        assert_eq!(kept.last().unwrap().ts, at(2 * MAX_LINES + 2));
        // The TUI does not compact: its batch is appended whole.
        let got = record(&state, batch(3 * MAX_LINES, 3), ts(NOW), false, &Flock).unwrap();
        assert_eq!(
            got,
            Recorded {
                added: 3,
                dropped: 0
            }
        );
    }

    /// R24: window lengths from R10's labels; codex's to the hour its label is rounded to.
    #[test]
    fn window_lengths_come_from_the_labels() {
        let h = |n: i64| Some(SignedDuration::from_hours(n));
        let cases = [
            ("Session", h(5)),
            ("Session (GPT-5.3-Codex-Spark)", h(5)),
            ("Week (all models)", h(168)),
            ("Week (Fable)", h(168)),
            ("five_hour", h(5)),
            ("seven_day_opus", h(168)),
            ("2h window", h(2)),
            ("3d window (Spark)", h(72)),
            ("45m window", Some(SignedDuration::from_mins(45))),
            ("weekly_other", None),
            ("0h window", None),
            ("h window", None),
            ("Weekly", None),
            ("", None),
            // Any text a cache gives is a label (R10): never cut inside a character, nor a
            // count out of range.
            ("额度 window", None),
            ("度m window", None),
            ("1度 window", None),
            ("５h window", None),
            ("+5h window", None),
            ("153722867280912931m window", None),
            ("9223372036854775807d window", None),
            (
                "5270400m window",
                Some(SignedDuration::from_mins(5_270_400)),
            ),
            ("5270401m window", None),
        ];
        for (label, want) in cases {
            assert_eq!(window_length(label), want, "{label:?}");
        }
        // A codex window of 90 minutes is labeled `2h window` (R10): two hours, then.
        assert_eq!(crate::usage::codex_label(90, None), "2h window");
        assert_eq!(window_length(&crate::usage::codex_label(299, None)), h(5));
        assert_eq!(
            window_length(&crate::usage::codex_label(10079, None)),
            h(168)
        );
    }

    fn pace_of(at: &str, percent: f64, reset: &str, hours: i64) -> Option<String> {
        let p = point("claude:max", "Session", at, percent, Some(reset));
        pace(
            &p,
            ts(reset),
            SignedDuration::from_hours(hours),
            &TimeZone::UTC,
        )
    }

    /// R24: the pace from the latest point: the share of the window elapsed when it was read,
    /// ahead (the time it reaches 100%) or behind (the percentage at the reset).
    #[test]
    fn pace_is_read_from_the_latest_point() {
        // A week resetting Oct 9 12:59 started Oct 2 12:59; by Oct 5 21:38, 48.006% of it had
        // elapsed, and 71% used at that rate is 100% 113.6 hours in.
        assert_eq!(
            pace_of("2026-10-05T21:38:00Z", 71.0, "2026-10-09T12:59:00Z", 168).unwrap(),
            "used 71% with 48% of the window elapsed: ahead of an even pace; \
             at this pace 100% by Oct 7 06:34 (resets Oct 9 12:59)"
        );
        // Half the session gone, a quarter used: half of it at the reset.
        assert_eq!(
            pace_of("2026-10-08T12:30:00Z", 25.0, "2026-10-08T15:00:00Z", 5).unwrap(),
            "used 25% with 50% of the window elapsed: behind an even pace; \
             at this pace 50% at reset"
        );
        assert_eq!(
            pace_of("2026-10-08T12:30:00Z", 0.0, "2026-10-08T15:00:00Z", 5).unwrap(),
            "used 0% with 50% of the window elapsed: behind an even pace; \
             at this pace 0% at reset"
        );
        assert_eq!(
            pace_of("2026-10-08T12:30:00Z", 100.0, "2026-10-08T15:00:00Z", 5).unwrap(),
            "used 100% with 50% of the window elapsed: limit reached (resets Oct 8 15:00)"
        );
        // Read before the window started (a reset further than its length ahead, a clock
        // behind): nothing elapsed, no projection, no division by it.
        assert_eq!(
            pace_of("2026-10-08T09:00:00Z", 10.0, "2026-10-08T15:00:00Z", 5).unwrap(),
            "used 10% with 0% of the window elapsed"
        );
        assert_eq!(
            pace_of("2026-10-08T10:00:00Z", 10.0, "2026-10-08T15:00:00Z", 5).unwrap(),
            "used 10% with 0% of the window elapsed"
        );
    }

    fn report_at(points: &[Point], accounts: &[&str], days: u32, tz: &TimeZone) -> String {
        let accounts: Vec<String> = accounts.iter().map(|a| a.to_string()).collect();
        report(points, &accounts, days, ts(NOW), tz)
    }

    /// R24: `--history`: each window of an account, the current one point by point with its
    /// pace, the ones before a line each; a live reset told to the minute and a cached one to
    /// the second are one window.
    #[test]
    fn the_history_lists_the_current_window_and_folds_the_ones_before() {
        let mut live = point(
            "claude:max",
            "Session",
            "2026-10-08T10:30:00Z",
            30.0,
            Some("2026-10-08T15:00:00Z"),
        );
        live.source = Source::Live;
        let points = vec![
            point(
                "claude:max",
                "Session",
                "2026-10-08T02:00:00Z",
                40.0,
                Some("2026-10-08T05:00:00Z"),
            ),
            point(
                "claude:max",
                "Session",
                "2026-10-08T04:00:00Z",
                88.0,
                Some("2026-10-08T05:00:00Z"),
            ),
            point(
                "claude:max",
                "Session",
                "2026-10-08T10:00:00Z",
                12.0,
                Some("2026-10-08T14:59:59.632Z"),
            ),
            point(
                "claude:max",
                "Week (Fable)",
                "2026-10-08T10:00:00Z",
                5.0,
                None,
            ),
            live,
            point("claude:team", "Session", "2026-09-20T10:00:00Z", 1.0, None),
        ];
        let out = report_at(&points, &["claude:max", "claude:team"], 7, &TimeZone::UTC);
        assert_eq!(
            out,
            "claude:max\n\
             \x20 Session\n\
             \x20   window ended Oct 8 05:00: peaked 88%\n\
             \x20   Oct 8 10:00  12%  resets Oct 8 14:59\n\
             \x20   Oct 8 10:30  30%  resets Oct 8 15:00\n\
             \x20   used 30% with 10% of the window elapsed: ahead of an even pace; \
             at this pace 100% by Oct 8 11:40 (resets Oct 8 15:00)\n\
             \x20 Week (Fable)\n\
             \x20   Oct 8 10:00  5%\n\
             \n\
             claude:team  no usage history in the last 7 days\n"
        );
        // The times are in the time zone asked for, like `remuda usage`'s.
        let tokyo = TimeZone::get("Asia/Tokyo").unwrap();
        let out = report_at(&points, &["claude:max"], 7, &tokyo);
        assert!(
            out.contains("window ended Oct 8 14:00: peaked 88%"),
            "{out}"
        );
        assert!(
            out.contains("Oct 8 19:30  30%  resets Oct 9 00:00"),
            "{out}"
        );
        // `--days` starts the history later: the team's point of Sep 20 is in 30 days.
        let out = report_at(&points, &["claude:team"], 30, &TimeZone::UTC);
        assert_eq!(out, "claude:team\n  Session\n    Sep 20 10:00  1%\n");
        let out = report_at(&points, &["claude:team"], 1, &TimeZone::UTC);
        assert_eq!(out, "claude:team  no usage history in the last 1 day\n");
    }

    /// R24: a window whose label gives no length the pace can use is listed without a pace
    /// line, whatever its label (claude shows an unknown kind under its own name, R10).
    #[test]
    fn a_window_of_unknown_length_has_no_pace() {
        for label in ["额度 window", "153722867280912931m window", "weekly_other"] {
            let p = point(
                "claude:max",
                label,
                "2026-10-08T10:00:00Z",
                30.0,
                Some("2026-10-08T15:00:00Z"),
            );
            let out = report_at(&[p], &["claude:max"], 7, &TimeZone::UTC);
            assert_eq!(
                out,
                format!("claude:max\n  {label}\n    Oct 8 10:00  30%  resets Oct 8 15:00\n"),
                "{label}"
            );
        }
    }

    /// A codex point of the window `2h window` (codex's label for 90 minutes, R10), read at `at`,
    /// telling `minutes`.
    fn codex_point(at: &str, percent: f64, reset: &str, minutes: Option<u32>) -> Point {
        Point {
            window_minutes: minutes,
            ..point("codex:work", "2h window", at, percent, Some(reset))
        }
    }

    /// R24: a codex window is as long as its points say, not as its label rounds it: 40% used
    /// half an hour into a window of 90 minutes is ahead of an even pace, where two hours would
    /// be behind it. Of a key's points that disagree (some recorded before the minutes were
    /// kept), the latest one's length counts; without minutes, the label's.
    #[test]
    fn a_codex_window_is_as_long_as_its_points_say() {
        let pace_line = |points: &[Point]| {
            let out = report_at(points, &["codex:work"], 7, &TimeZone::UTC);
            out.lines().last().unwrap().trim().to_string()
        };
        let exact = "used 40% with 33% of the window elapsed: ahead of an even pace; \
                     at this pace 100% by Oct 8 12:15 (resets Oct 8 12:30)";
        let rounded = "used 40% with 50% of the window elapsed: behind an even pace; \
                       at this pace 80% at reset";
        let latest = |minutes| {
            codex_point(
                "2026-10-08T11:30:00Z",
                40.0,
                "2026-10-08T12:30:00Z",
                minutes,
            )
        };
        let earlier = |minutes| {
            codex_point(
                "2026-10-08T11:10:00Z",
                20.0,
                "2026-10-08T12:30:00Z",
                minutes,
            )
        };
        assert_eq!(pace_line(&[latest(Some(90))]), exact);
        assert_eq!(pace_line(&[latest(None)]), rounded);
        assert_eq!(pace_line(&[earlier(None), latest(Some(90))]), exact);
        assert_eq!(pace_line(&[earlier(Some(90)), latest(None)]), rounded);
        // At most ten years, as for a label; beyond it unknown, no pace (the label says no
        // better).
        for (minutes, paced) in [(5_270_400, true), (5_270_401, false), (u32::MAX, false)] {
            let out = report_at(&[latest(Some(minutes))], &["codex:work"], 7, &TimeZone::UTC);
            assert_eq!(out.contains("elapsed"), paced, "{minutes}: {out}");
        }

        // Its windows too: a reset 50 minutes from the one before is another window of 90
        // minutes (further than half of it), the same one of two hours (within the hour).
        let first = |minutes| {
            codex_point(
                "2026-10-08T10:00:00Z",
                30.0,
                "2026-10-08T11:30:00Z",
                minutes,
            )
        };
        let second = |minutes| {
            codex_point(
                "2026-10-08T11:00:00Z",
                10.0,
                "2026-10-08T12:20:00Z",
                minutes,
            )
        };
        let out = report_at(
            &[first(Some(90)), second(Some(90))],
            &["codex:work"],
            7,
            &TimeZone::UTC,
        );
        assert!(
            out.contains("    window ended Oct 8 11:30: peaked 30%\n"),
            "{out}"
        );
        let out = report_at(
            &[first(None), second(None)],
            &["codex:work"],
            7,
            &TimeZone::UTC,
        );
        assert!(!out.contains("window ended"), "{out}");
        assert!(
            out.contains("Oct 8 10:00  30%  resets Oct 8 11:30\n"),
            "{out}"
        );
    }

    /// R24: a window whose reset has passed is folded, the last one too; a point read after
    /// a window's reset starts the next window even without a reset of its own.
    #[test]
    fn a_window_past_its_reset_is_folded() {
        let points = vec![
            point(
                "claude:max",
                "Session",
                "2026-10-08T02:00:00Z",
                40.0,
                Some("2026-10-08T05:00:00Z"),
            ),
            point("claude:max", "Session", "2026-10-08T06:00:00Z", 3.0, None),
            point(
                "claude:max",
                "Session",
                "2026-10-08T07:00:00Z",
                9.0,
                Some("2026-10-08T11:00:00Z"),
            ),
        ];
        let out = report_at(&points, &["claude:max"], 7, &TimeZone::UTC);
        assert_eq!(
            out,
            "claude:max\n  Session\n    window ended Oct 8 05:00: peaked 40%\n    \
             window ended Oct 8 11:00: peaked 9%\n"
        );
    }
}
