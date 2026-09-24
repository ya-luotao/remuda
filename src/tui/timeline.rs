//! The reset timeline (R10): one text axis from now to +7 days; per account, markers where
//! its limits reset.

use jiff::{SignedDuration, Timestamp};

pub const SPAN: SignedDuration = SignedDuration::from_hours(7 * 24);
const DAY: SignedDuration = SignedDuration::from_hours(24);

/// Column of `t` on an axis `width` columns wide that starts at `now` and ends at
/// `now + 7d`: past times are column 0, times beyond the axis `None`.
pub fn column(now: Timestamp, t: Timestamp, width: usize) -> Option<usize> {
    if width == 0 {
        return None;
    }
    let offset = t.duration_since(now);
    if offset > SPAN {
        return None;
    }
    let frac = offset.as_secs_f64().max(0.0) / SPAN.as_secs_f64();
    Some((frac * (width - 1) as f64).round() as usize)
}

/// Labels for the day ticks: `now`, `+1d`, ... `+7d`; a label that would overlap the one
/// before it is left out, and the last one is kept inside the axis.
pub fn axis(width: usize) -> String {
    let mut line = vec![' '; width];
    let mut free_from = 0;
    for day in 0..=7i32 {
        let Some(col) = column(Timestamp::UNIX_EPOCH, tick(day), width) else {
            continue;
        };
        let label: Vec<char> = if day == 0 {
            "now".chars().collect()
        } else {
            format!("+{day}d").chars().collect()
        };
        let start = col.min(width.saturating_sub(label.len()));
        if start < free_from || start + label.len() > width {
            continue;
        }
        line[start..start + label.len()].copy_from_slice(&label);
        free_from = start + label.len() + 1;
    }
    line.into_iter().collect()
}

fn tick(day: i32) -> Timestamp {
    Timestamp::UNIX_EPOCH + DAY * day
}

/// One account's track: `·` with `|` at each day, and `marks` drawn in order (a later mark
/// wins a shared column). Marks beyond the axis are left out.
pub fn track(now: Timestamp, marks: &[(char, Timestamp)], width: usize) -> String {
    let mut line = vec!['·'; width];
    for day in 1..=7 {
        if let Some(col) = column(Timestamp::UNIX_EPOCH, tick(day), width) {
            line[col] = '|';
        }
    }
    for (mark, at) in marks {
        if let Some(col) = column(now, *at, width) {
            line[col] = *mark;
        }
    }
    line.into_iter().collect()
}

/// Time until `t`, compact: `45m`, `3h05m`, `2d03h`; `now` once it has passed.
pub fn until(now: Timestamp, t: Timestamp) -> String {
    let mins = t.duration_since(now).as_secs() / 60;
    if mins <= 0 {
        return "now".to_string();
    }
    let (d, h, m) = (mins / 1440, mins / 60 % 24, mins % 60);
    if d > 0 {
        format!("{d}d{h:02}h")
    } else if h > 0 {
        format!("{h}h{m:02}m")
    } else {
        format!("{m}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    const NOW: &str = "2026-09-24T00:00:00Z";

    #[test]
    fn columns_span_seven_days() {
        let now = ts(NOW);
        assert_eq!(column(now, now, 71), Some(0));
        assert_eq!(column(now, ts("2026-10-01T00:00:00Z"), 71), Some(70));
        assert_eq!(column(now, ts("2026-09-27T12:00:00Z"), 71), Some(35));
        assert_eq!(column(now, ts("2026-09-23T00:00:00Z"), 71), Some(0));
        assert_eq!(column(now, ts("2026-10-01T00:00:01Z"), 71), None);
        assert_eq!(column(now, now, 0), None);
    }

    #[test]
    fn track_marks_session_and_week() {
        let now = ts(NOW);
        let marks = [
            ('W', ts("2026-10-01T00:00:00Z")),
            ('S', ts("2026-09-24T02:00:00Z")),
        ];
        let line = track(now, &marks, 29);
        assert_eq!(line.chars().count(), 29);
        assert_eq!(line, "S···|···|···|···|···|···|···W");
        // A later mark wins a shared column; marks off the axis are dropped.
        let line = track(
            now,
            &[
                ('W', ts("2026-09-24T01:00:00Z")),
                ('S', ts("2026-09-24T00:30:00Z")),
                ('x', ts("2026-12-01T00:00:00Z")),
            ],
            29,
        );
        assert_eq!(line, "S···|···|···|···|···|···|···|");
    }

    #[test]
    fn axis_labels_fit() {
        // +7d would overlap +6d at this width.
        assert_eq!(axis(29), "now +1d +2d +3d +4d +5d +6d  ");
        let a = axis(57);
        assert!(a.starts_with("now     +1d     +2d"), "{a:?}");
        assert!(a.ends_with("+7d"), "{a:?}");
        assert_eq!(a.chars().count(), 57);
        // Too narrow for every label: overlapping ones are dropped.
        let narrow = axis(15);
        assert!(narrow.starts_with("now"), "{narrow:?}");
        assert_eq!(narrow.chars().count(), 15);
    }

    #[test]
    fn until_is_compact() {
        let now = ts(NOW);
        assert_eq!(until(now, ts("2026-09-24T00:45:30Z")), "45m");
        assert_eq!(until(now, ts("2026-09-24T03:05:00Z")), "3h05m");
        assert_eq!(until(now, ts("2026-09-26T03:05:00Z")), "2d03h");
        assert_eq!(until(now, ts("2026-09-23T03:05:00Z")), "now");
    }
}
