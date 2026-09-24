//! Usage limits per account (SPEC R10): the cache claude writes into `.claude.json` or a live
//! `claude -p /usage` query; the rate limits codex records in its rollouts or a live
//! `codex app-server` query.

use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde_json::{Value, json};

use crate::identity::{self, Identity};
use crate::provider::app_server::{self, ACCOUNT_READ, RATE_LIMITS_READ};
use crate::provider::{Provider, codex};
use crate::registry::Account;
use crate::{Env, launch, probe, text};

/// When a limit resets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resets {
    At(Timestamp),
    /// Claude's own (localized) wording, shown verbatim.
    Text(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsageRow {
    pub label: String,
    pub percent: f64,
    /// `normal`, `warning`, `critical`, ... as reported; `None` when unknown.
    pub severity: Option<String>,
    pub resets: Option<Resets>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CachedUsage {
    pub fetched_at: Option<Timestamp>,
    pub rows: Vec<UsageRow>,
}

/// Parses `cachedUsageUtilization` out of a `.claude.json`. `Err` is a short notice for the
/// user (malformed file, no cache, no recognizable limits).
///
/// Prefers `utilization.limits[]`; falls back to `five_hour` / `seven_day` when `limits`
/// yields nothing. Rows that do not look as expected are dropped, never fatal.
pub fn parse_cached(claude_json: &str) -> Result<CachedUsage, String> {
    let v: Value =
        serde_json::from_str(claude_json).map_err(|_| "malformed .claude.json".to_string())?;
    let Some(cache) = v.get("cachedUsageUtilization").filter(|c| c.is_object()) else {
        return Err("no usage cache in .claude.json".to_string());
    };
    let fetched_at = cache
        .get("fetchedAtMs")
        .and_then(Value::as_i64)
        .and_then(|ms| Timestamp::from_millisecond(ms).ok());
    let utilization = cache.get("utilization").unwrap_or(&Value::Null);
    let mut rows: Vec<UsageRow> = utilization
        .get("limits")
        .and_then(Value::as_array)
        .map(|limits| limits.iter().filter_map(limit_row).collect())
        .unwrap_or_default();
    if rows.is_empty() {
        rows = [("five_hour", "Session"), ("seven_day", "Week (all models)")]
            .into_iter()
            .filter_map(|(key, label)| {
                let window = utilization.get(key)?;
                Some(UsageRow {
                    label: label.to_string(),
                    percent: window.get("utilization")?.as_f64()?,
                    severity: None,
                    resets: resets_at(window),
                })
            })
            .collect();
    }
    if rows.is_empty() {
        return Err("no usage limits in the cache".to_string());
    }
    Ok(CachedUsage { fetched_at, rows })
}

fn limit_row(limit: &Value) -> Option<UsageRow> {
    let kind = limit.get("kind")?.as_str()?;
    let label = match kind {
        "session" => "Session".to_string(),
        "weekly_all" => "Week (all models)".to_string(),
        "weekly_scoped" => {
            let model = limit
                .pointer("/scope/model/display_name")
                .and_then(Value::as_str)
                .unwrap_or("scoped");
            format!("Week ({model})")
        }
        other => other.to_string(),
    };
    Some(UsageRow {
        label,
        percent: limit.get("percent")?.as_f64()?,
        severity: limit
            .get("severity")
            .and_then(Value::as_str)
            .map(str::to_string),
        resets: resets_at(limit),
    })
}

fn resets_at(v: &Value) -> Option<Resets> {
    let text = v.get("resets_at")?.as_str()?;
    text.parse().ok().map(Resets::At)
}

/// The label of a codex window (R10), by its duration rather than its position (a Pro plan's
/// `primary` window is weekly): minutes rounded to whole hours; 5 hours is `Session`, 168 hours
/// `Week (all models)`, anything else `<N>h window` (`<N>d window` for whole days). A per-model
/// limit's `name` replaces `all models` in the week and is appended to the others.
pub fn codex_label(minutes: i64, name: Option<&str>) -> String {
    let hours = (minutes + 30) / 60;
    match (hours, name) {
        (5, None) => "Session".to_string(),
        (5, Some(name)) => format!("Session ({name})"),
        (168, None) => "Week (all models)".to_string(),
        (168, Some(name)) => format!("Week ({name})"),
        _ => {
            let duration = if hours >= 24 && hours % 24 == 0 {
                format!("{}d", hours / 24)
            } else if hours > 0 {
                format!("{hours}h")
            } else {
                format!("{minutes}m")
            };
            match name {
                None => format!("{duration} window"),
                Some(name) => format!("{duration} window ({name})"),
            }
        }
    }
}

/// Rows of one codex rate-limit snapshot (R10): a rollout's `rate_limits` (snake_case) or an
/// app-server snapshot (camelCase), `name` `None` for the general limit. A window without a
/// numeric percentage and a positive duration is dropped; rows go by window length.
pub fn codex_rows(snapshot: &Value, name: Option<&str>) -> Vec<UsageRow> {
    let mut windows: Vec<(i64, UsageRow)> = ["primary", "secondary"]
        .into_iter()
        .filter_map(|key| {
            let window = snapshot.get(key)?;
            let percent = field(window, "used_percent", "usedPercent")?.as_f64()?;
            let minutes = field(window, "window_minutes", "windowDurationMins")?
                .as_i64()
                .filter(|m| *m > 0)?;
            let resets = field(window, "resets_at", "resetsAt")
                .and_then(Value::as_i64)
                .and_then(|s| Timestamp::from_second(s).ok())
                .map(Resets::At);
            let row = UsageRow {
                label: codex_label(minutes, name),
                percent,
                severity: None,
                resets,
            };
            Some((minutes, row))
        })
        .collect();
    windows.sort_by_key(|(minutes, _)| *minutes);
    windows.into_iter().map(|(_, row)| row).collect()
}

/// `v[snake]`, else `v[camel]`: rollouts spell codex's fields one way, app-server the other.
fn field<'a>(v: &'a Value, snake: &str, camel: &str) -> Option<&'a Value> {
    v.get(snake).or_else(|| v.get(camel))
}

/// Rows of an `account/rateLimits/read` result (R10): the general limit
/// (`rateLimitsByLimitId.codex`, else `rateLimits`), then every other limit of
/// `rateLimitsByLimitId` by its ID, named by its `limitName` (else its ID).
pub fn codex_live_rows(result: &Value) -> Vec<UsageRow> {
    let by_id = result.get("rateLimitsByLimitId").and_then(Value::as_object);
    let general = by_id
        .and_then(|map| map.get("codex"))
        .filter(|snapshot| snapshot.is_object())
        .or_else(|| result.get("rateLimits"));
    let mut rows = general.map_or_else(Vec::new, |g| codex_rows(g, None));
    let general_id = general
        .and_then(|g| g.get("limitId"))
        .and_then(Value::as_str);
    if let Some(map) = by_id {
        let mut ids: Vec<&String> = map
            .keys()
            .filter(|id| id.as_str() != "codex" && Some(id.as_str()) != general_id)
            .collect();
        ids.sort();
        for id in ids {
            let snapshot = &map[id];
            let name = snapshot
                .get("limitName")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .unwrap_or(id);
            rows.extend(codex_rows(snapshot, Some(name)));
        }
    }
    rows
}

/// Parses the `Current session` / `Current week (...)` lines of `claude -p /usage`.
pub fn parse_live(stdout: &str) -> Vec<UsageRow> {
    stdout.lines().filter_map(live_row).collect()
}

fn live_row(line: &str) -> Option<UsageRow> {
    let line = line.trim();
    let (label, rest) = if let Some(rest) = line.strip_prefix("Current session:") {
        ("Session".to_string(), rest)
    } else {
        let (inner, rest) = line.strip_prefix("Current week (")?.split_once("):")?;
        (format!("Week ({inner})"), rest)
    };
    let (used, suffix) = match rest.split_once(" \u{b7} ") {
        Some((used, suffix)) => (used, Some(suffix)),
        None => (rest, None),
    };
    let percent = used.trim().strip_suffix("% used")?.trim().parse().ok()?;
    let resets = suffix
        .and_then(|s| s.trim().strip_prefix("resets "))
        .map(|t| Resets::Text(t.trim().to_string()));
    Some(UsageRow {
        label,
        percent,
        severity: None,
        resets,
    })
}

/// `Sep 23 15:39` in `tz`.
pub fn format_time(ts: Timestamp, tz: &TimeZone) -> String {
    ts.to_zoned(tz.clone()).strftime("%b %-d %H:%M").to_string()
}

/// `45s ago`, `3m ago`, `2h ago`, `3d ago` (floored; a future time counts as `0s ago`).
pub fn format_age(then: Timestamp, now: Timestamp) -> String {
    let secs = (now.as_second() - then.as_second()).max(0);
    match secs {
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86400),
    }
}

/// Live rows carry no severity: from this percentage on they are marked warning (R10).
pub const WARN_AT: f64 = 75.0;
/// ... and from this one critical (R10).
pub const CRIT_AT: f64 = 90.0;

/// The reported severity, else one derived from the percentage (live rows carry none).
pub fn severity(r: &UsageRow) -> &str {
    match r.severity.as_deref() {
        Some(s) => s,
        None if r.percent >= CRIT_AT => "critical",
        None if r.percent >= WARN_AT => "warning",
        None => "normal",
    }
}

/// Indented, aligned rows: label, percent, `!`/`!!` for warning/critical ([`severity`]),
/// reset time.
pub fn format_rows(rows: &[UsageRow], tz: &TimeZone) -> String {
    let cells: Vec<(&str, String, &str, String)> = rows
        .iter()
        .map(|r| {
            let mark = match severity(r) {
                "warning" => "!",
                "critical" => "!!",
                _ => "",
            };
            let resets = match &r.resets {
                Some(Resets::At(ts)) => format!("resets {}", format_time(*ts, tz)),
                Some(Resets::Text(text)) => format!("resets {text}"),
                None => String::new(),
            };
            (r.label.as_str(), format_percent(r.percent), mark, resets)
        })
        .collect();
    let label_w = cells.iter().map(|c| text::width(c.0)).max().unwrap_or(0);
    let pct_w = cells.iter().map(|c| c.1.len()).max().unwrap_or(0);
    cells
        .iter()
        .map(|(label, pct, mark, resets)| {
            let label = text::pad(label, label_w);
            let line = format!("  {label}  {pct:>pct_w$} {mark:<2}  {resets}");
            format!("{}\n", line.trim_end())
        })
        .collect()
}

/// `34%`, `12.5%`.
pub fn format_percent(p: f64) -> String {
    if p.fract() == 0.0 {
        format!("{p:.0}%")
    } else {
        format!("{p:.1}%")
    }
}

/// An account's cached usage (R10): claude's cache in `.claude.json`, or the newest general rate
/// limits codex recorded in the home's rollouts. `Err` is a short notice for the user.
pub fn cached_usage(account: &Account, env: &Env) -> Result<CachedUsage, String> {
    match account.provider {
        Provider::Claude => cached_claude(account, env),
        Provider::Codex => {
            let Some(home) = account.home_dir(env) else {
                return Err("HOME is not set".to_string());
            };
            match codex::cached_rate_limits(&home) {
                Some((at, limits)) => Ok(CachedUsage {
                    fetched_at: Some(at),
                    rows: codex_rows(&limits, None),
                }),
                None => Err(format!(
                    "no rate limits in the rollouts under {}",
                    home.display()
                )),
            }
        }
    }
}

fn cached_claude(account: &Account, env: &Env) -> Result<CachedUsage, String> {
    let Some(path) = account.claude_json(env) else {
        return Err("HOME is not set".to_string());
    };
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(format!("no {}", path.display()));
        }
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    parse_cached(&text)
}

/// `remuda usage` block for one account from its cached usage (R10).
pub fn cached_report(account: &Account, env: &Env, tz: &TimeZone, now: Timestamp) -> String {
    let name = account.qualified();
    match cached_usage(account, env) {
        Err(notice) => format!("{name}  no cached usage ({notice})\n"),
        Ok(cached) => {
            let when = match cached.fetched_at {
                Some(at) => format!("cached {} ({})", format_age(at, now), format_time(at, tz)),
                None => "cached (time unknown)".to_string(),
            };
            format!("{name}  {when}\n{}", format_rows(&cached.rows, tz))
        }
    }
}

pub const LIVE_USAGE_ARGS: &[&str] = &["-p", "/usage", "--no-session-persistence"];

/// What a live query answered.
#[derive(Debug, Clone, PartialEq)]
pub enum LiveUsage {
    Rows(Vec<UsageRow>),
    /// Output with no recognizable limits, kept verbatim (codex: the result as indented JSON).
    Unrecognized(String),
}

/// A live query's usage, and the identity the same query told (R10): codex's `account/read`,
/// only when it names a logged-in account; always `None` for claude.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveResult {
    pub usage: LiveUsage,
    pub identity: Option<Identity>,
}

impl From<LiveUsage> for LiveResult {
    fn from(usage: LiveUsage) -> Self {
        LiveResult {
            usage,
            identity: None,
        }
    }
}

/// Queries `account`'s usage live with its agent `program` (R10): `claude -p /usage`, or
/// `codex app-server`. `Err` says why the query failed.
pub fn live_usage(
    account: &Account,
    program: &Path,
    timeout: Duration,
) -> Result<LiveResult, String> {
    match account.provider {
        Provider::Claude => live_claude(account, program, timeout).map(LiveResult::from),
        Provider::Codex => live_codex(account, program, timeout),
    }
}

/// Runs `claude -p /usage --no-session-persistence` for `account` (R10): never through
/// `launch::prepare`, so no `--session-id` is injected and nothing is logged.
fn live_claude(account: &Account, program: &Path, timeout: Duration) -> Result<LiveUsage, String> {
    let change = launch::env_change(account);
    let outcome = probe::run_captured(program, LIVE_USAGE_ARGS, &change, timeout);
    let Some(stdout) = outcome.success_stdout() else {
        return Err(format!(
            "`claude {}` {}",
            LIVE_USAGE_ARGS.join(" "),
            outcome.describe(timeout)
        ));
    };
    let rows = parse_live(stdout);
    Ok(if rows.is_empty() {
        LiveUsage::Unrecognized(stdout.to_string())
    } else {
        LiveUsage::Rows(rows)
    })
}

/// One `codex app-server` run in `account`'s environment (R4, R10), asking
/// `account/rateLimits/read` (without the reset-credit lookup) and `account/read` (without a
/// token refresh). The rate limits are the query: their error is its failure. The identity is
/// extra: it is dropped when `account/read` fails, is not recognized, or names no logged-in
/// account.
fn live_codex(account: &Account, program: &Path, timeout: Duration) -> Result<LiveResult, String> {
    let answers = app_server::call(
        program,
        account,
        &[
            (RATE_LIMITS_READ, json!({"excludeResetCreditDetails": true})),
            (ACCOUNT_READ, json!({"refreshToken": false})),
        ],
        timeout,
    )?;
    let [limits, read]: [Result<Value, String>; 2] = answers
        .try_into()
        .map_err(|_| "`codex app-server` did not answer each request".to_string())?;
    let limits = limits?;
    let identity = read
        .ok()
        .as_ref()
        .and_then(identity::parse_account_read)
        .filter(|identity| matches!(identity, Identity::LoggedIn { .. }));
    let rows = codex_live_rows(&limits);
    let usage = if rows.is_empty() {
        LiveUsage::Unrecognized(
            serde_json::to_string_pretty(&limits).unwrap_or_else(|_| limits.to_string()),
        )
    } else {
        LiveUsage::Rows(rows)
    };
    Ok(LiveResult { usage, identity })
}

/// `remuda usage --live` block for one account; `false` when the query failed (R10). `program`
/// is the account's agent, `None` when it is not on PATH. The header names the identity the
/// query told (codex: email and plan).
pub fn live_report(
    account: &Account,
    program: Option<&Path>,
    tz: &TimeZone,
    timeout: Duration,
) -> (String, bool) {
    let name = account.qualified();
    let Some(program) = program else {
        return (
            format!(
                "{name}  error: `{}` not found on PATH\n",
                account.provider.program()
            ),
            false,
        );
    };
    match live_usage(account, program, timeout) {
        Err(e) => (format!("{name}  error: {e}\n"), false),
        Ok(LiveResult { usage, identity }) => {
            let who = identity.as_ref().map(who_and_plan).unwrap_or_default();
            match usage {
                LiveUsage::Unrecognized(stdout) => {
                    let raw: String = stdout
                        .lines()
                        .map(|l| format!("    {}\n", l.trim_end()))
                        .collect();
                    (
                        format!("{name}  live (output not recognized; shown as is){who}\n{raw}"),
                        true,
                    )
                }
                LiveUsage::Rows(rows) => (
                    format!("{name}  live{who}\n{}", format_rows(&rows, tz)),
                    true,
                ),
            }
        }
    }
}

/// `  cx@example.com (pro)`: the identity after a live header.
fn who_and_plan(identity: &Identity) -> String {
    match identity {
        Identity::LoggedIn {
            plan: Some(plan), ..
        } => format!("  {} ({plan})", identity.who()),
        _ => format!("  {}", identity.who()),
    }
}

/// When a limit resets, as an instant: [`Resets::At`] as is; claude's wording
/// (`Sep 24 at 3:19am (Asia/Shanghai)`, `3am (UTC)`) parsed best-effort, as the next such
/// time around `now`. `None` when the wording is not recognized.
pub fn reset_instant(resets: &Resets, now: Timestamp) -> Option<Timestamp> {
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

    const CACHE: &str = r#"{"numStartups": 2, "cachedUsageUtilization": {
      "fetchedAtMs": 1790176749528, "accountUuid": "00000000-0000-0000-0000-000000000000",
      "utilization": {
        "five_hour": {"utilization": 34, "resets_at": "2026-09-23T15:40:00.292773+00:00"},
        "seven_day": {"utilization": 76, "resets_at": "2026-09-25T05:00:00.632368+00:00"},
        "seven_day_opus": null, "extra_usage": {"is_enabled": false},
        "limits": [
          {"kind": "session", "group": "session", "percent": 34, "severity": "normal",
           "resets_at": "2026-09-23T15:39:59.632347+00:00", "scope": null, "is_active": false},
          {"kind": "weekly_all", "group": "weekly", "percent": 77, "severity": "warning",
           "resets_at": "2026-09-25T04:59:59.632368+00:00", "scope": null, "is_active": false},
          {"kind": "weekly_scoped", "group": "weekly", "percent": 100, "severity": "critical",
           "resets_at": "2026-09-25T04:59:59.632532+00:00",
           "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null}, "is_active": true}]}}}"#;

    const LIVE: &str = "You are currently using your subscription to power your Claude Code usage\n\n\
        Current session: 9% used \u{b7} resets Sep 24 at 3:19am (Asia/Shanghai)\n\
        Current week (all models): 74% used \u{b7} resets Sep 29 at 11:59am (Asia/Shanghai)\n\
        Current week (Fable): 85% used \u{b7} resets Sep 29 at 11:59am (Asia/Shanghai)\n\n\
        What's contributing to your limits usage?\n...\n";

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn row(label: &str, percent: f64, severity: Option<&str>, resets: Option<Resets>) -> UsageRow {
        UsageRow {
            label: label.into(),
            percent,
            severity: severity.map(str::to_string),
            resets,
        }
    }

    fn at(s: &str) -> Option<Resets> {
        Some(Resets::At(ts(s)))
    }

    fn text(s: &str) -> Option<Resets> {
        Some(Resets::Text(s.into()))
    }

    fn cache_with(utilization: &str) -> String {
        format!(
            r#"{{"cachedUsageUtilization": {{"fetchedAtMs": 1000, "utilization": {utilization}}}}}"#
        )
    }

    #[test]
    fn parses_cached_limits() {
        let cached = parse_cached(CACHE).unwrap();
        assert_eq!(
            cached.fetched_at,
            Some(Timestamp::from_millisecond(1790176749528).unwrap())
        );
        assert_eq!(
            cached.rows,
            [
                row(
                    "Session",
                    34.0,
                    Some("normal"),
                    at("2026-09-23T15:39:59.632347Z")
                ),
                row(
                    "Week (all models)",
                    77.0,
                    Some("warning"),
                    at("2026-09-25T04:59:59.632368Z")
                ),
                row(
                    "Week (Fable)",
                    100.0,
                    Some("critical"),
                    at("2026-09-25T04:59:59.632532Z")
                ),
            ]
        );
    }

    #[test]
    fn cached_limits_degrade_quietly() {
        let limits = r#"{"limits": [
            {"kind": "session", "percent": 12.5, "severity": "normal", "resets_at": null},
            {"kind": "monthly_thing", "group": "monthly", "percent": 3, "severity": "novel"},
            {"kind": "weekly_scoped", "percent": 50, "scope": {"surface": "x"}},
            {"kind": "weekly_all", "percent": "lots"},
            {"percent": 1},
            "garbage",
            {"kind": "weekly_all", "percent": 40, "resets_at": "not a time"}]}"#;
        let cached = parse_cached(&cache_with(limits)).unwrap();
        assert_eq!(
            cached.rows,
            [
                row("Session", 12.5, Some("normal"), None),
                row("monthly_thing", 3.0, Some("novel"), None),
                row("Week (scoped)", 50.0, None, None),
                row("Week (all models)", 40.0, None, None),
            ]
        );
    }

    #[test]
    fn falls_back_to_five_hour_and_seven_day() {
        let old = r#"{"five_hour": {"utilization": 34, "resets_at": "2026-09-23T15:40:00+00:00"},
                      "seven_day": {"utilization": 76, "resets_at": null}, "seven_day_opus": null}"#;
        let expected = [
            row("Session", 34.0, None, at("2026-09-23T15:40:00Z")),
            row("Week (all models)", 76.0, None, None),
        ];
        assert_eq!(parse_cached(&cache_with(old)).unwrap().rows, expected);
        // An unusable `limits` also falls back.
        let odd = old.replacen('{', r#"{"limits": "soon", "#, 1);
        assert_eq!(parse_cached(&cache_with(&odd)).unwrap().rows, expected);
        let empty = old.replacen('{', r#"{"limits": [], "#, 1);
        assert_eq!(parse_cached(&cache_with(&empty)).unwrap().rows, expected);
    }

    #[test]
    fn cache_problems_are_notices() {
        for bad in [
            "",
            "{not json",
            "[]",
            "{}",
            r#"{"cachedUsageUtilization": null}"#,
            r#"{"cachedUsageUtilization": {"fetchedAtMs": 1}}"#,
            &cache_with(r#"{"limits": []}"#),
            &cache_with(r#"{"five_hour": null}"#),
        ] {
            assert!(parse_cached(bad).is_err(), "{bad:?}");
        }
        let no_time = r#"{"cachedUsageUtilization": {"utilization": {"limits": [{"kind": "session", "percent": 1}]}}}"#;
        assert_eq!(parse_cached(no_time).unwrap().fetched_at, None);
    }

    fn at_second(s: i64) -> Option<Resets> {
        Some(Resets::At(Timestamp::from_second(s).unwrap()))
    }

    /// R10: codex windows are labeled by their duration, never by their position.
    #[test]
    fn codex_windows_are_labeled_by_duration() {
        let rows = |v: Value| codex_rows(&v, None);
        let window = |pct: f64, minutes: i64, resets: Value| json!({"used_percent": pct, "window_minutes": minutes, "resets_at": resets});
        // A Pro plan: one weekly window, as `primary`.
        let pro = json!({"limit_id": "codex", "limit_name": null,
            "primary": window(99.0, 10080, json!(1790414559)), "secondary": null,
            "credits": {"has_credits": false}, "plan_type": "pro"});
        assert_eq!(
            rows(pro),
            [row("Week (all models)", 99.0, None, at_second(1790414559))]
        );
        // A Plus plan: five hours and a week, in either position; shortest first.
        let plus = json!({"primary": window(10.0, 300, json!(1)),
                          "secondary": window(20.0, 10080, json!(2))});
        let expected = [
            row("Session", 10.0, None, at_second(1)),
            row("Week (all models)", 20.0, None, at_second(2)),
        ];
        assert_eq!(rows(plus), expected);
        let swapped = json!({"primary": window(20.0, 10080, json!(2)),
                             "secondary": window(10.0, 300, json!(1))});
        assert_eq!(rows(swapped), expected);
        // 2025 rollouts: 299 and 10079 minutes, no reset time.
        let legacy = json!({"limit_id": null, "primary": window(1.5, 299, Value::Null),
                            "secondary": window(2.0, 10079, Value::Null)});
        assert_eq!(
            rows(legacy),
            [
                row("Session", 1.5, None, None),
                row("Week (all models)", 2.0, None, None)
            ]
        );
        // app-server spells the same in camelCase.
        let camel = json!({"primary": {"usedPercent": 10, "windowDurationMins": 300, "resetsAt": 1},
                           "secondary": {"usedPercent": 20, "windowDurationMins": 10080,
                                         "resetsAt": 2}});
        assert_eq!(rows(camel), expected);
        // A per-model limit is named.
        let spark = json!({"primary": window(5.0, 300, json!(1)),
                           "secondary": window(7.0, 10080, json!(2))});
        assert_eq!(
            codex_rows(&spark, Some("GPT-5.3-Codex-Spark")),
            [
                row("Session (GPT-5.3-Codex-Spark)", 5.0, None, at_second(1)),
                row("Week (GPT-5.3-Codex-Spark)", 7.0, None, at_second(2)),
            ]
        );
        // Other durations.
        for (minutes, name, label) in [
            (60, None, "1h window"),
            (1440, None, "1d window"),
            (43200, None, "30d window"),
            (2160, None, "36h window"),
            (20, None, "20m window"),
            (1440, Some("X"), "1d window (X)"),
            (10080, Some("X"), "Week (X)"),
        ] {
            assert_eq!(codex_label(minutes, name), label, "{minutes} {name:?}");
        }
        // Windows without a numeric percentage and a positive duration are dropped.
        for junk in [
            json!({"primary": {"used_percent": 5}}),
            json!({"primary": {"used_percent": "x", "window_minutes": 300}}),
            json!({"primary": {"used_percent": 5, "window_minutes": 0}}),
            json!({"primary": {"used_percent": 5, "window_minutes": "300"}}),
            json!({"primary": null, "secondary": null}),
            json!({"primary": 3}),
            json!("rate limits"),
            Value::Null,
        ] {
            assert!(rows(junk.clone()).is_empty(), "{junk}");
        }
    }

    /// R10: an `account/rateLimits/read` result: the general limit, then per-model limits.
    #[test]
    fn codex_live_rows_general_then_per_model() {
        let snapshot = |id: &str, name: Value, primary: Value, secondary: Value| {
            json!({"limitId": id, "limitName": name, "primary": primary, "secondary": secondary,
                   "credits": null, "planType": "pro"})
        };
        let window = |pct: i64, minutes: i64| json!({"usedPercent": pct, "windowDurationMins": minutes, "resetsAt": 1790414559});
        let general = snapshot("codex", Value::Null, window(99, 10080), Value::Null);
        let result = json!({
            "rateLimits": general,
            "rateLimitsByLimitId": {
                "premium": snapshot("premium", Value::Null, Value::Null, Value::Null),
                "codex_bengalfox": snapshot("codex_bengalfox", json!("GPT-5.3-Codex-Spark"),
                                            window(5, 300), window(7, 10080)),
                "codex": general,
                "codex_other": snapshot("codex_other", Value::Null, window(1, 60), Value::Null),
            },
            "rateLimitResetCredits": null,
        });
        let labels = |v: &Value| -> Vec<(String, f64)> {
            codex_live_rows(v)
                .into_iter()
                .map(|r| (r.label, r.percent))
                .collect()
        };
        let owned = |rows: &[(&str, f64)]| -> Vec<(String, f64)> {
            rows.iter().map(|(l, p)| (l.to_string(), *p)).collect()
        };
        assert_eq!(
            labels(&result),
            owned(&[
                ("Week (all models)", 99.0),
                ("Session (GPT-5.3-Codex-Spark)", 5.0),
                ("Week (GPT-5.3-Codex-Spark)", 7.0),
                ("1h window (codex_other)", 1.0),
            ])
        );
        assert_eq!(codex_live_rows(&result)[0].resets, at_second(1790414559));
        // Without the map, `rateLimits` alone.
        assert_eq!(
            labels(&json!({"rateLimits": general})),
            owned(&[("Week (all models)", 99.0)])
        );
        // The map's `codex` wins over `rateLimits`.
        let other = snapshot("codex", Value::Null, window(10, 300), Value::Null);
        assert_eq!(
            labels(&json!({"rateLimits": other, "rateLimitsByLimitId": {"codex": general}})),
            owned(&[("Week (all models)", 99.0)])
        );
        // `rateLimits` naming another limit is not repeated as a per-model limit.
        let spark = snapshot(
            "codex_bengalfox",
            json!("Spark"),
            window(5, 300),
            Value::Null,
        );
        assert_eq!(
            labels(&json!({"rateLimits": spark,
                           "rateLimitsByLimitId": {"codex_bengalfox": spark}})),
            owned(&[("Session", 5.0)])
        );
        for empty in [
            json!({}),
            json!({"rateLimits": null}),
            json!({"rateLimits": {"limitId": "codex", "primary": null, "secondary": null}}),
            json!([]),
        ] {
            assert!(codex_live_rows(&empty).is_empty(), "{empty}");
        }
    }

    #[test]
    fn parses_live_output() {
        assert_eq!(
            parse_live(LIVE),
            [
                row(
                    "Session",
                    9.0,
                    None,
                    text("Sep 24 at 3:19am (Asia/Shanghai)")
                ),
                row(
                    "Week (all models)",
                    74.0,
                    None,
                    text("Sep 29 at 11:59am (Asia/Shanghai)")
                ),
                row(
                    "Week (Fable)",
                    85.0,
                    None,
                    text("Sep 29 at 11:59am (Asia/Shanghai)")
                ),
            ]
        );
        assert_eq!(
            parse_live("Current session: 0% used\n"),
            [row("Session", 0.0, None, None)]
        );
        for junk in [
            "",
            "hello\n",
            "Current session: lots used\n",
            "Current week (x: 5% used\n",
        ] {
            assert!(parse_live(junk).is_empty(), "{junk:?}");
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

    #[test]
    fn formats_times_and_ages() {
        let t = ts("2026-09-03T15:39:59.9Z");
        assert_eq!(format_time(t, &TimeZone::UTC), "Sep 3 15:39");
        assert_eq!(
            format_time(t, &TimeZone::fixed(jiff::tz::offset(8))),
            "Sep 3 23:39"
        );
        let now = ts("2026-09-23T12:00:00Z");
        let cases = [
            ("2026-09-23T12:00:00Z", "0s ago"),
            ("2026-09-23T12:00:30Z", "0s ago"),
            ("2026-09-23T11:59:15Z", "45s ago"),
            ("2026-09-23T11:56:40Z", "3m ago"),
            ("2026-09-23T09:59:00Z", "2h ago"),
            ("2026-09-20T11:00:00Z", "3d ago"),
        ];
        for (then, want) in cases {
            assert_eq!(format_age(ts(then), now), want, "{then}");
        }
    }

    #[test]
    fn formats_rows_with_markers() {
        let rows = parse_cached(CACHE).unwrap().rows;
        let out = format_rows(&rows, &TimeZone::UTC);
        let normalized: Vec<String> = out
            .lines()
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect();
        assert_eq!(
            normalized,
            [
                "Session 34% resets Sep 23 15:39",
                "Week (all models) 77% ! resets Sep 25 04:59",
                "Week (Fable) 100% !! resets Sep 25 04:59",
            ]
        );
        assert!(out.lines().all(|l| l.starts_with("  ")), "{out}");
        // Percent columns line up.
        let pct_end: Vec<usize> = out.lines().map(|l| l.find('%').unwrap()).collect();
        assert!(pct_end.windows(2).all(|w| w[0] == w[1]), "{out}");
        let live = format_rows(&parse_live(LIVE), &TimeZone::UTC);
        assert!(live.contains("9%"), "{live}");
        assert!(
            live.contains("resets Sep 24 at 3:19am (Asia/Shanghai)"),
            "{live}"
        );
        // Labels align by display width (a double-width model name).
        let wide = format_rows(
            &[
                row("Week (模型)", 5.0, None, None),
                row("Session", 7.0, None, None),
            ],
            &TimeZone::UTC,
        );
        let pct_col: Vec<usize> = wide
            .lines()
            .map(|l| text::width(&l[..l.find('%').unwrap()]))
            .collect();
        assert_eq!(pct_col[0], pct_col[1], "{wide}");
        let fractional = format_rows(&[row("X", 12.5, None, None)], &TimeZone::UTC);
        assert_eq!(fractional.trim(), "X  12.5%");
    }

    #[test]
    fn live_rows_are_marked_at_75_and_90_percent() {
        assert_eq!((WARN_AT, CRIT_AT), (75.0, 90.0));
        let live = |percent| row("Session", percent, None, None);
        for (percent, want) in [
            (0.0, "normal"),
            (74.9, "normal"),
            (75.0, "warning"),
            (89.9, "warning"),
            (90.0, "critical"),
            (100.0, "critical"),
        ] {
            assert_eq!(severity(&live(percent)), want, "{percent}%");
        }
        // A reported severity (cached rows) wins over the percentage.
        assert_eq!(
            severity(&row("Session", 95.0, Some("normal"), None)),
            "normal"
        );
    }

    #[test]
    fn live_rows_get_the_same_markers_in_text() {
        let out = format_rows(
            &[
                row("Session", 74.0, None, None),
                row("Week (all models)", 75.0, None, None),
                row("Week (Fable)", 90.0, None, None),
            ],
            &TimeZone::UTC,
        );
        let normalized: Vec<String> = out
            .lines()
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect();
        assert_eq!(
            normalized,
            [
                "Session 74%",
                "Week (all models) 75% !",
                "Week (Fable) 90% !!"
            ]
        );
    }
}
