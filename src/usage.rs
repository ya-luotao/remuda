//! Usage limits per account (SPEC R10): the cache claude writes into `.claude.json` or a live
//! `claude -p /usage` query; the rate limits codex records in its rollouts or a live
//! `codex app-server` query.

use std::fs;
use std::io;
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde_json::{Value, json};

use crate::account_command::{Parsed, Runner};
use crate::identity::{self, Identity};
use crate::provider::app_server::{ACCOUNT_READ, RATE_LIMITS_READ};
use crate::provider::{Provider, codex};
use crate::registry::Account;
use crate::{Env, text};

pub mod history;
pub mod snapshot;

pub use snapshot::{Reading, Reset, Snapshot, Source, Window};

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
/// user (malformed file, no cache, no recognizable limits, a limit that cannot be read).
///
/// Prefers `utilization.limits[]`; falls back to `five_hour` / `seven_day` when there is no
/// such list or it is empty. A limit that cannot be read (no `kind`, no numeric `percent`)
/// makes the cache unusable: the limits that could be read are not all of the account's, and
/// the one left out may be the one that is used up (R10).
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
    let limits = cached_limits(utilization);
    let listed = limits.items.len() + limits.unrecognized;
    let rows = match limits.complete() {
        Ok(rows) => rows,
        Err(0) => match cached_windows(utilization).complete() {
            Ok(rows) => rows,
            Err(0) => return Err("no usage limits in the cache".to_string()),
            Err(_) => return Err("usage limits in the cache not recognized".to_string()),
        },
        Err(unread) => {
            return Err(format!(
                "{unread} of {listed} usage limits in the cache not recognized"
            ));
        }
    };
    Ok(CachedUsage { fetched_at, rows })
}

/// The rows of `utilization.limits[]`: each of its entries is a limit, read or not.
fn cached_limits(utilization: &Value) -> Parsed<UsageRow> {
    let limits = utilization.get("limits").and_then(Value::as_array);
    Parsed::of(limits.into_iter().flatten().map(limit_row))
}

/// The rows of the older `five_hour` / `seven_day` fields: one that is there (not null) is a
/// window, read or not.
fn cached_windows(utilization: &Value) -> Parsed<UsageRow> {
    let windows = [("five_hour", "Session"), ("seven_day", "Week (all models)")];
    Parsed::of(windows.into_iter().filter_map(|(key, label)| {
        let window = utilization.get(key).filter(|w| !w.is_null())?;
        Some(
            window
                .get("utilization")
                .and_then(Value::as_f64)
                .map(|percent| UsageRow {
                    label: label.to_string(),
                    percent,
                    severity: None,
                    resets: resets_at(window),
                }),
        )
    }))
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

/// The start of each line of `claude -p /usage` that is a usage window.
const LIVE_LINES: [&str; 2] = ["Current session", "Current week"];

/// Parses the `Current session` / `Current week (...)` lines of `claude -p /usage`. A line that
/// starts like one and cannot be read is counted, not dropped: the caller must not take the
/// rest for the account's usage (R10).
pub fn parse_live(stdout: &str) -> Parsed<UsageRow> {
    let lines = stdout.lines().filter(|line| {
        LIVE_LINES
            .iter()
            .any(|start| line.trim().starts_with(start))
    });
    Parsed::of(lines.map(live_row))
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
    format_ago((now.as_second() - then.as_second()).max(0))
}

/// [`format_age`] of an age in seconds.
pub fn format_ago(secs: i64) -> String {
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

/// A window that resets within this many seconds of now ...
pub const REMINDER_WITHIN: i64 = 60 * 60;
/// ... with at least this percentage left gets a note in `remuda usage` (R10).
pub const REMINDER_LEFT: f64 = 25.0;

/// Indented, aligned rows of `reading`'s windows: label, percent, `!`/`!!` for warning/critical
/// ([`Window::severity`]), reset time. A window whose reset has passed since the usage was
/// recorded has `-` for its percent and says so (R10). A window that resets within
/// [`REMINDER_WITHIN`] of `now` with at least [`REMINDER_LEFT`] percent left is followed by a
/// note saying so: what is left then goes unused (R10).
pub fn format_rows(reading: &Reading, tz: &TimeZone, now: Timestamp) -> String {
    let cells: Vec<(&str, String, &str, String, Option<String>)> = reading
        .windows
        .iter()
        .map(|w| {
            let mark = match w.severity() {
                "warning" => "!",
                "critical" => "!!",
                _ => "",
            };
            let (percent, resets) = match (w.used(), w.reset, &w.wording) {
                (None, Reset::Passed(at), _) => (
                    "-".to_string(),
                    format!("{} ({})", w.reset_since(), format_time(at, tz)),
                ),
                (None, _, _) => ("-".to_string(), String::new()),
                (Some(used), _, Some(text)) => (format_percent(used), format!("resets {text}")),
                (Some(used), Reset::Ahead(at), None) => (
                    format_percent(used),
                    format!("resets {}", format_time(at, tz)),
                ),
                (Some(used), _, None) => (format_percent(used), String::new()),
            };
            (w.label.as_str(), percent, mark, resets, reminder(w, now))
        })
        .collect();
    let label_w = cells.iter().map(|c| text::width(c.0)).max().unwrap_or(0);
    let pct_w = cells.iter().map(|c| c.1.len()).max().unwrap_or(0);
    cells
        .iter()
        .map(|(label, pct, mark, resets, note)| {
            let label = text::pad(label, label_w);
            let line = format!("  {label}  {pct:>pct_w$} {mark:<2}  {resets}");
            let note = note
                .as_ref()
                .map(|note| format!("  note: {note}\n"))
                .unwrap_or_default();
            format!("{}\n{note}", line.trim_end())
        })
        .collect()
}

/// `resets in 42 min with 71% left`, for a window whose reset is ahead and within
/// [`REMINDER_WITHIN`] of `now`, with at least [`REMINDER_LEFT`] percent left (R10). A window
/// that has reset since, or whose reset is unknown, gets none.
fn reminder(w: &Window, now: Timestamp) -> Option<String> {
    let Reset::Ahead(at) = w.reset else {
        return None;
    };
    let secs = at.as_second() - now.as_second();
    let left = w.left()?;
    if secs > REMINDER_WITHIN || left < REMINDER_LEFT {
        return None;
    }
    let minutes = ((secs + 59) / 60).max(1);
    Some(format!(
        "resets in {minutes} min with {} left",
        format_percent(left)
    ))
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
    cached_attempt(account, env, tz, now).0
}

/// [`cached_report`], and the reading it shows (`None` without cached usage): for
/// `usage --wait` (R10), which reads it for windows that are used up, and for the usage
/// history (R24), which records it.
pub fn cached_attempt(
    account: &Account,
    env: &Env,
    tz: &TimeZone,
    now: Timestamp,
) -> (String, Option<Reading>) {
    cached_text(account, &cached_usage(account, env), tz, now)
}

/// [`cached_attempt`] of the cached usage `cached` already read.
pub fn cached_text(
    account: &Account,
    cached: &Result<CachedUsage, String>,
    tz: &TimeZone,
    now: Timestamp,
) -> (String, Option<Reading>) {
    let name = account.qualified();
    match cached {
        Err(notice) => (format!("{name}  no cached usage ({notice})\n"), None),
        Ok(cached) => {
            let reading = Snapshot::cached(cached).at(now);
            let when = match (reading.age_text(), reading.fetched_at) {
                (Some(age), Some(at)) => format!("cached {age} ({})", format_time(at, tz)),
                _ => "cached (time unknown)".to_string(),
            };
            let text = format!("{name}  {when}\n{}", format_rows(&reading, tz, now));
            (text, Some(reading))
        }
    }
}

/// The windows of `reading` that are used up for `usage --wait` (R10): of known usage, with
/// less than `min_headroom` percent left (`[pick] min_headroom`, R3). A window that has reset
/// since its usage was recorded is unknown, never used up.
pub fn exhausted(reading: &Reading, min_headroom: u32) -> Vec<&Window> {
    reading
        .windows
        .iter()
        .filter(|w| w.left().is_some_and(|left| left < f64::from(min_headroom)))
        .collect()
}

/// Claude's live usage query (R10): `/usage` in print mode, which calls no model, leaving no
/// transcript (`--no-session-persistence`). It is remuda's probe, not the user's session, so it
/// loads none of the account's settings files (`--setting-sources` with an empty list: no
/// hooks run, no plugin is enabled, the files' `env` is not applied) and none of its MCP
/// servers (`--strict-mcp-config` without `--mcp-config`). The empty list is an argument of its
/// own: claude reads `--setting-sources=` as a flag without its value. Verified on 2.1.292
/// (R10's Basis); not `--bare`, with which `/usage` prints the session's cost instead of the
/// limits.
pub const LIVE_USAGE_ARGS: &[&str] = &[
    "-p",
    "/usage",
    "--no-session-persistence",
    "--setting-sources",
    "",
    "--strict-mcp-config",
];

/// Set, it makes claude's `/usage` send no request and only repeat a reading another run took
/// within the hour, or print no usage line at all (R10, verified on 2.1.292).
pub const NONESSENTIAL_TRAFFIC_VAR: &str = "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC";

/// What the live query removes from the environment it inherits (R10): the user asked for a
/// live reading. Only this one command; launches and the other commands keep it.
const LIVE_USAGE_UNSET: &[&str] = &[NONESSENTIAL_TRAFFIC_VAR];

/// What a live query answered.
#[derive(Debug, Clone, PartialEq)]
pub enum LiveUsage {
    Rows(Vec<UsageRow>),
    /// Claude's answer told no usage, and said why (R10): no usage line, none left unread.
    Untold(Untold),
    /// Output with no recognizable limits, kept verbatim (codex: the result as indented JSON).
    Unrecognized(String),
}

/// Why `claude -p /usage` told no usage (R10), as the words it printed instead show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Untold {
    /// How the account is billed (its subscription, or its overages), without a usage line:
    /// claude could not get the account's limits just now.
    Unavailable,
    /// The session's cost (`Total cost: …`) instead of limits: what claude prints for an
    /// account that is not logged in to a Claude subscription, or not using one (an API key).
    NotSubscribed,
}

impl Untold {
    /// The reason in a few words. No `/` in it: in private mode the TUI masks a word with a
    /// `/` as a path (R21).
    pub fn reason(self) -> &'static str {
        match self {
            Untold::Unavailable => "no usage limits told for now",
            Untold::NotSubscribed => "not logged in to a Claude subscription, or using an API key",
        }
    }
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

/// Queries `account`'s usage live through its agent (R10): `claude -p /usage`, or
/// `codex app-server`. `Err` says why the query failed.
pub fn live_usage(
    account: &Account,
    agents: &dyn Runner,
    timeout: Duration,
) -> Result<LiveResult, String> {
    match account.provider {
        Provider::Claude => live_claude(account, agents, timeout).map(LiveResult::from),
        Provider::Codex => {
            let live = live_codex(account, agents, timeout)?;
            Ok(LiveResult {
                usage: live.usage?,
                identity: live
                    .login
                    .filter(|identity| matches!(identity, Identity::LoggedIn { .. })),
            })
        }
    }
}

/// Runs `claude` with [`LIVE_USAGE_ARGS`] for `account` (R10), without
/// [`NONESSENTIAL_TRAFFIC_VAR`]: never through `launch::prepare`, so no `--session-id` is
/// injected and nothing is logged. An answer with a usage line that cannot be read is not
/// recognized as a whole: its other lines are not the account's usage.
fn live_claude(
    account: &Account,
    agents: &dyn Runner,
    timeout: Duration,
) -> Result<LiveUsage, String> {
    let output = agents
        .run_ok_without(account, LIVE_USAGE_ARGS, LIVE_USAGE_UNSET, timeout)
        .map_err(|failure| failure.to_string())?;
    Ok(classify_live(output.stdout))
}

/// The start of the line in which `claude -p /usage` says how the account is billed: by its
/// subscription, or by its overages (verified on 2.1.292).
const BILLED_LINES: [&str; 2] = [
    "You are currently using your subscription to power your Claude Code usage",
    "You are currently using your overages to power your Claude Code usage",
];

/// What `claude -p /usage` answered (R10): its usage lines, all of them read; else, with no
/// usage line at all, why it told none (how the account is billed, or the session's cost);
/// else the text as it is. A usage line that cannot be read leaves the whole answer
/// unrecognized, whatever else it says.
pub fn classify_live(stdout: String) -> LiveUsage {
    match parse_live(&stdout).complete() {
        Ok(rows) => LiveUsage::Rows(rows),
        Err(0)
            if stdout
                .lines()
                .any(|line| BILLED_LINES.iter().any(|b| line.trim().starts_with(b))) =>
        {
            LiveUsage::Untold(Untold::Unavailable)
        }
        Err(0)
            if stdout
                .lines()
                .find(|line| !line.trim().is_empty())
                .is_some_and(|line| line.trim().starts_with("Total cost:")) =>
        {
            LiveUsage::Untold(Untold::NotSubscribed)
        }
        Err(_) => LiveUsage::Unrecognized(stdout),
    }
}

/// What one `codex app-server` run told about an account (R4, R10): the two things it is
/// asked, each answered or not on its own.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexLive {
    /// The usage, or why `account/rateLimits/read` gave none: its error is the query's failure.
    pub usage: Result<LiveUsage, String>,
    /// Whether the account is logged in, and as whom, as `account/read` said; `None` when it
    /// failed or was not recognized.
    pub login: Option<Identity>,
}

/// One `codex app-server` run in `account`'s environment (R4, R10), asking
/// `account/rateLimits/read` (without the reset-credit lookup) and `account/read` (without a
/// token refresh). `Err` when the run itself gave no answers. A caller that needs both the
/// usage and the login state gets them from this one run.
pub fn live_codex(
    account: &Account,
    agents: &dyn Runner,
    timeout: Duration,
) -> Result<CodexLive, String> {
    let answers = agents.requests(
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
    let usage = limits.map(|limits| {
        let rows = codex_live_rows(&limits);
        if rows.is_empty() {
            LiveUsage::Unrecognized(
                serde_json::to_string_pretty(&limits).unwrap_or_else(|_| limits.to_string()),
            )
        } else {
            LiveUsage::Rows(rows)
        }
    });
    let login = read.ok().as_ref().and_then(identity::parse_account_read);
    Ok(CodexLive { usage, login })
}

/// `remuda usage --live` block for one account; `false` when the query failed (R10), which an
/// agent that is not on PATH is too. The header names the identity the query told (codex: email
/// and plan). The rows are read when they are answered, by `clock`: the query may take as long
/// as `timeout`.
pub fn live_report(
    account: &Account,
    agents: &dyn Runner,
    tz: &TimeZone,
    clock: fn() -> Timestamp,
    timeout: Duration,
) -> (String, bool) {
    let (text, ok, _) = live_attempt(account, agents, tz, clock, timeout);
    (text, ok)
}

/// [`live_report`], and the reading it shows (`None` when the answer gave no usage rows): for
/// `usage --wait --live` (R10) and for the usage history (R24). The answer is read when it
/// arrived, by `clock` after the query.
pub fn live_attempt(
    account: &Account,
    agents: &dyn Runner,
    tz: &TimeZone,
    clock: fn() -> Timestamp,
    timeout: Duration,
) -> (String, bool, Option<Reading>) {
    let result = live_usage(account, agents, timeout);
    live_text(account, &result, tz, clock())
}

/// [`live_attempt`] of the answer `result`, read at `answered_at`: when it arrived.
pub fn live_text(
    account: &Account,
    result: &Result<LiveResult, String>,
    tz: &TimeZone,
    answered_at: Timestamp,
) -> (String, bool, Option<Reading>) {
    let name = account.qualified();
    match result {
        Err(e) => (format!("{name}  error: {e}\n"), false, None),
        Ok(LiveResult { usage, identity }) => {
            let who = identity.as_ref().map(who_and_plan).unwrap_or_default();
            match usage {
                LiveUsage::Untold(untold) => {
                    (format!("{name}  live: {}\n", untold.reason()), true, None)
                }
                LiveUsage::Unrecognized(stdout) => {
                    let raw: String = stdout
                        .lines()
                        .map(|l| format!("    {}\n", l.trim_end()))
                        .collect();
                    (
                        format!("{name}  live (output not recognized; shown as is){who}\n{raw}"),
                        true,
                        None,
                    )
                }
                LiveUsage::Rows(rows) => {
                    let reading = Snapshot::live(rows, answered_at).at(answered_at);
                    (
                        format!(
                            "{name}  live{who}\n{}",
                            format_rows(&reading, tz, answered_at)
                        ),
                        true,
                        Some(reading),
                    )
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_command::Scripted;
    use crate::probe::Outcome;

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

    /// R10: a limit is read leniently in what it may leave out or say anew: an unknown kind, no
    /// scope, no severity, a reset time that is not one.
    #[test]
    fn cached_limits_are_read_leniently() {
        let limits = r#"{"limits": [
            {"kind": "session", "percent": 12.5, "severity": "normal", "resets_at": null},
            {"kind": "monthly_thing", "group": "monthly", "percent": 3, "severity": "novel"},
            {"kind": "weekly_scoped", "percent": 50, "scope": {"surface": "x"}},
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

    /// R10 (review #14): a limit that cannot be read is counted, and makes the cache unusable:
    /// the limits around it are not all of the account's, and the one left out may be the one
    /// that is used up. The older fields do not stand in for the list.
    #[test]
    fn a_cached_limit_that_cannot_be_read_spoils_the_cache() {
        let session = r#"{"kind": "session", "percent": 12}"#;
        let old = r#""five_hour": {"utilization": 34}, "seven_day": {"utilization": 76}"#;
        for unread in [
            r#"{"kind": "weekly_all", "percent": "lots"}"#,
            r#"{"kind": "weekly_scoped", "scope": {"model": {"display_name": "Fable"}}}"#,
            r#"{"percent": 100}"#,
            r#""garbage""#,
        ] {
            let utilization: Value =
                serde_json::from_str(&format!(r#"{{"limits": [{session}, {unread}], {old}}}"#))
                    .unwrap();
            assert_eq!(
                cached_limits(&utilization),
                Parsed {
                    items: vec![row("Session", 12.0, None, None)],
                    unrecognized: 1
                },
                "{unread}"
            );
            assert_eq!(
                parse_cached(&cache_with(&utilization.to_string())),
                Err("1 of 2 usage limits in the cache not recognized".to_string()),
                "{unread}"
            );
        }
        // Nothing of the list can be read: still not the older fields.
        let none = format!(r#"{{"limits": [{{"percent": 1}}, 7], {old}}}"#);
        assert_eq!(
            parse_cached(&cache_with(&none)),
            Err("2 of 2 usage limits in the cache not recognized".to_string())
        );
        // The older fields: a window that is there without a number.
        for unread in [
            r#"{"five_hour": {"resets_at": null}, "seven_day": {"utilization": 76}}"#,
            r#"{"five_hour": {"utilization": 34}, "seven_day": {"utilization": "most"}}"#,
            r#"{"five_hour": 34}"#,
        ] {
            assert_eq!(
                parse_cached(&cache_with(unread)),
                Err("usage limits in the cache not recognized".to_string()),
                "{unread}"
            );
        }
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
            Parsed {
                items: vec![
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
                ],
                unrecognized: 0
            }
        );
        assert_eq!(
            parse_live("  Current session: 0% used\n").items,
            [row("Session", 0.0, None, None)]
        );
        // Neither a usage line nor like one.
        for other in ["", "hello\n", "Currently: 5% used\n", "session: 5% used\n"] {
            let parsed = parse_live(other);
            assert_eq!(
                (parsed.items.len(), parsed.unrecognized),
                (0, 0),
                "{other:?}"
            );
        }
    }

    /// R10 (review #14): a line that starts like a usage line and cannot be read is counted,
    /// so that the lines around it are not taken for the account's usage.
    #[test]
    fn a_usage_line_that_cannot_be_read_is_counted() {
        for unread in [
            "Current session: lots used\n",
            "Current week (x: 5% used\n",
            "Current week: 5% used\n",
            "Current session \u{b7} 5% used\n",
            "  Current week (all models): limit reached \u{b7} resets 3am (UTC)\n",
        ] {
            let parsed = parse_live(unread);
            assert_eq!(
                (parsed.items.len(), parsed.unrecognized),
                (0, 1),
                "{unread:?}"
            );
            assert_eq!(parsed.complete(), Err(1), "{unread:?}");
        }
        // The week is used up, and said in words this version does not know.
        let drifted = LIVE.replace(
            "Current week (all models): 74% used",
            "Current week (all models): limit reached",
        );
        let parsed = parse_live(&drifted);
        let labels: Vec<&str> = parsed.items.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, ["Session", "Week (Fable)"]);
        assert_eq!(parsed.unrecognized, 1);
        assert_eq!(parsed.complete(), Err(1));
    }

    fn claude_account(name: &str) -> Account {
        Account {
            provider: crate::registry::CLAUDE,
            name: name.into(),
            home: crate::registry::Home::Path(format!("/h/{name}")),
        }
    }

    fn codex_account(name: &str) -> Account {
        Account {
            provider: crate::registry::CODEX,
            name: name.into(),
            home: crate::registry::Home::Path(format!("/c/{name}")),
        }
    }

    const USAGE: &str =
        "claude:max -p /usage --no-session-persistence --setting-sources \"\" --strict-mcp-config";
    const T: Duration = Duration::from_secs(90);

    /// R10: `claude -p /usage` answers rows; a failure says the command and why; a claude that
    /// is not on PATH is the query's failure.
    #[test]
    fn live_claude_usage_is_the_commands_answer() {
        let max = claude_account("max");
        let agents = Scripted::new().on(USAGE, Scripted::exited(0, LIVE, ""));
        let result = live_usage(&max, &agents, T).unwrap();
        assert_eq!(result.identity, None);
        assert_eq!(result.usage, LiveUsage::Rows(parse_live(LIVE).items));
        assert_eq!(agents.ran(), [USAGE]);

        let agents = Scripted::new().on(USAGE, Outcome::TimedOut);
        assert_eq!(
            live_usage(&max, &agents, T),
            Err("`claude -p /usage --no-session-persistence --setting-sources \"\" --strict-mcp-config` timed out after 90s".to_string())
        );
        let agents = Scripted::new().on(USAGE, Scripted::exited(1, "", "Not logged in\n"));
        assert_eq!(
            live_usage(&max, &agents, T),
            Err(
                "`claude -p /usage --no-session-persistence --setting-sources \"\" --strict-mcp-config` exited with status 1: Not logged in"
                    .to_string()
            )
        );
        let agents = Scripted::new().without(Provider::Claude);
        assert_eq!(
            live_usage(&max, &agents, T),
            Err("`claude` not found on PATH".to_string())
        );
        assert_eq!(
            live_report(&max, &agents, &TimeZone::UTC, Timestamp::now, T),
            (
                "claude:max  error: `claude` not found on PATH\n".to_string(),
                false
            )
        );
        assert!(agents.ran().is_empty());
    }

    /// R10: the live query runs without `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`, which would
    /// keep `/usage` from asking for a new reading; nothing else is removed.
    #[test]
    fn live_claude_runs_without_disable_nonessential_traffic() {
        let max = claude_account("max");
        let agents = Scripted::new().on(USAGE, Scripted::exited(0, LIVE, ""));
        live_usage(&max, &agents, T).unwrap();
        assert_eq!(
            agents.unset_for(USAGE),
            [["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"]]
        );
    }

    /// R10 (review #14): the week is used up and its line no longer reads `<N>% used`: the
    /// answer is not recognized, whole; the session line alone would show the account as
    /// available.
    #[test]
    fn a_live_answer_read_in_part_is_not_recognized() {
        let max = claude_account("max");
        let drifted = LIVE.replace(
            "Current week (all models): 74% used",
            "Current week (all models): limit reached",
        );
        let agents = Scripted::new().on(USAGE, Scripted::exited(0, &drifted, ""));
        assert_eq!(
            live_usage(&max, &agents, T),
            Ok(LiveUsage::Unrecognized(drifted.clone()).into())
        );
        let (report, ok) = live_report(&max, &agents, &TimeZone::UTC, Timestamp::now, T);
        assert!(ok);
        assert!(
            report.starts_with("claude:max  live (output not recognized; shown as is)\n"),
            "{report}"
        );
        assert!(
            report.contains("    Current week (all models): limit reached"),
            "{report}"
        );
        // No usage line at all is not recognized either, as before.
        let agents = Scripted::new().on(USAGE, Scripted::exited(0, "Usage is unavailable.\n", ""));
        assert_eq!(
            live_usage(&max, &agents, T),
            Ok(LiveUsage::Unrecognized("Usage is unavailable.\n".into()).into())
        );
    }

    /// What claude 2.1.292 printed for `/usage` when it could not get the account's limits
    /// (with `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`, an account whose limits it had not
    /// fetched within the hour).
    const LIVE_UNAVAILABLE: &str = "You are currently using your subscription to power your Claude Code usage\n\n\
        What's contributing to your limits usage?\n\
        Approximate, based on local sessions on this machine \u{2014} does not include other devices or claude.ai. Behaviors are independent characteristics, not a breakdown.\n\n\
        Last 24h \u{b7} 3915 requests \u{b7} 29 sessions\n";

    /// What claude 2.1.292 printed for `/usage` in an empty `CLAUDE_CONFIG_DIR` (not logged in).
    const LIVE_NOT_SUBSCRIBED: &str = "Total cost:            $0.0000\n\
        Total duration (API):  0s\n\
        Total duration (wall): 0s\n\
        Total code changes:    0 lines added, 0 lines removed\n\
        Usage:                 0 input, 0 output, 0 cache read, 0 cache write\n";

    const OVERAGES: &str = "You are currently using your overages to power your Claude Code usage. We will \
        automatically switch you back to your subscription rate limits when they reset";

    /// R10: an answer without a single usage line says why it tells none: how the account is
    /// billed (its subscription or its overages), or the session's cost instead of limits.
    /// Anything else, or a usage line that cannot be read, is the text as it is; usage lines
    /// read whole are the usage, whatever else the answer says.
    #[test]
    fn an_answer_without_usage_lines_says_why() {
        assert_eq!(
            classify_live(LIVE_UNAVAILABLE.into()),
            LiveUsage::Untold(Untold::Unavailable)
        );
        assert_eq!(
            classify_live(format!("{OVERAGES}\n")),
            LiveUsage::Untold(Untold::Unavailable)
        );
        assert_eq!(
            classify_live(format!("\n{LIVE_NOT_SUBSCRIBED}")),
            LiveUsage::Untold(Untold::NotSubscribed)
        );
        // The overages line heads usage lines too: they are the usage.
        let over = LIVE.replace(
            "You are currently using your subscription to power your Claude Code usage",
            OVERAGES,
        );
        assert_eq!(classify_live(over), LiveUsage::Rows(parse_live(LIVE).items));
        for unrecognized in [
            String::new(),
            "Usage is unavailable.\n".to_string(),
            // A usage line that cannot be read: the reason is not given for the whole answer.
            format!("{LIVE_UNAVAILABLE}Current week (all models): limit reached\n"),
            // The cost, but not where claude prints it instead of the limits.
            format!("Something else\n{LIVE_NOT_SUBSCRIBED}"),
        ] {
            assert_eq!(
                classify_live(unrecognized.clone()),
                LiveUsage::Unrecognized(unrecognized.clone()),
                "{unrecognized:?}"
            );
        }
    }

    /// R10: an answer that tells no usage is the query's answer, shown as its reason in one
    /// line, not as the text claude printed.
    #[test]
    fn a_live_answer_that_tells_no_usage_gives_its_reason() {
        let max = claude_account("max");
        let agents = Scripted::new().on(USAGE, Scripted::exited(0, LIVE_UNAVAILABLE, ""));
        assert_eq!(
            live_usage(&max, &agents, T),
            Ok(LiveUsage::Untold(Untold::Unavailable).into())
        );
        assert_eq!(
            live_report(&max, &agents, &TimeZone::UTC, Timestamp::now, T),
            (
                "claude:max  live: no usage limits told for now\n".to_string(),
                true
            )
        );
        let agents = Scripted::new().on(USAGE, Scripted::exited(0, LIVE_NOT_SUBSCRIBED, ""));
        assert_eq!(
            live_report(&max, &agents, &TimeZone::UTC, Timestamp::now, T),
            (
                "claude:max  live: not logged in to a Claude subscription, or using an API key\n"
                    .to_string(),
                true
            )
        );
        // In private mode the TUI masks a word with a `/` as a path (R21).
        for untold in [Untold::Unavailable, Untold::NotSubscribed] {
            assert!(!untold.reason().contains('/'), "{untold:?}");
        }
    }

    /// R10: one `codex app-server` run answers the rate limits and the identity; the identity
    /// is extra, the rate limits are the query.
    #[test]
    fn live_codex_usage_is_one_app_server_run() {
        let work = codex_account("work");
        let limits =
            json!({"rateLimits": {"primary": {"usedPercent": 4, "windowDurationMins": 300}}});
        let read =
            json!({"account": {"type": "chatgpt", "email": "c@example.com", "planType": "plus"}});
        let session = vec![row("Session", 4.0, None, None)];
        let agents =
            Scripted::new().app_server_says("codex:work", Ok(vec![Ok(limits.clone()), Ok(read)]));
        let result = live_usage(&work, &agents, T).unwrap();
        assert_eq!(result.usage, LiveUsage::Rows(session.clone()));
        assert_eq!(
            result.identity.as_ref().map(Identity::who).as_deref(),
            Some("c@example.com")
        );
        assert_eq!(
            agents.ran(),
            ["codex:work app-server account/rateLimits/read account/read"]
        );
        // `account/read` fails, or names nobody: the usage all the same.
        for read in [
            Err("`codex app-server` account/read: no".to_string()),
            Ok(json!({"account": null})),
            Ok(json!("?")),
        ] {
            let agents =
                Scripted::new().app_server_says("codex:work", Ok(vec![Ok(limits.clone()), read]));
            assert_eq!(
                live_usage(&work, &agents, T),
                Ok(LiveUsage::Rows(session.clone()).into())
            );
        }
        // The rate limits fail: the query does.
        let denied = "`codex app-server` account/rateLimits/read: authentication required";
        let agents = Scripted::new().app_server_says(
            "codex:work",
            Ok(vec![Err(denied.to_string()), Ok(json!({"account": null}))]),
        );
        assert_eq!(live_usage(&work, &agents, T), Err(denied.to_string()));
        let agents = Scripted::new().app_server_says(
            "codex:work",
            Err("`codex app-server` timed out after 90s".into()),
        );
        assert_eq!(
            live_usage(&work, &agents, T),
            Err("`codex app-server` timed out after 90s".to_string())
        );
        let agents = Scripted::new().app_server_says("codex:work", Ok(vec![Ok(limits)]));
        assert_eq!(
            live_usage(&work, &agents, T),
            Err("`codex app-server` did not answer each request".to_string())
        );
        // No window to show: the result as it is.
        let agents = Scripted::new().app_server_says(
            "codex:work",
            Ok(vec![Ok(json!({"rateLimits": null})), Ok(json!({}))]),
        );
        assert_eq!(
            live_usage(&work, &agents, T),
            Ok(LiveUsage::Unrecognized("{\n  \"rateLimits\": null\n}".into()).into())
        );
        let agents = Scripted::new().without(Provider::Codex);
        assert_eq!(
            live_report(&work, &agents, &TimeZone::UTC, Timestamp::now, T),
            (
                "codex:work  error: `codex` not found on PATH\n".to_string(),
                false
            )
        );
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

    /// A live answer's rows as text, read when they were answered.
    fn live_text(rows: &[UsageRow]) -> String {
        let now = ts("2026-09-23T12:00:00Z");
        format_rows(&Snapshot::live(rows, now).at(now), &TimeZone::UTC, now)
    }

    fn normalized(out: &str) -> Vec<String> {
        out.lines()
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect()
    }

    #[test]
    fn formats_rows_with_markers() {
        let cached = parse_cached(CACHE).unwrap();
        let fetched = cached.fetched_at.unwrap();
        let out = format_rows(
            &Snapshot::cached(&cached).at(fetched),
            &TimeZone::UTC,
            fetched,
        );
        // Read when cached (15:19), 21 minutes before the session resets with 66% left: the
        // note of R10 follows its row.
        assert_eq!(
            normalized(&out),
            [
                "Session 34% resets Sep 23 15:39",
                "note: resets in 21 min with 66% left",
                "Week (all models) 77% ! resets Sep 25 04:59",
                "Week (Fable) 100% !! resets Sep 25 04:59",
            ]
        );
        assert!(out.lines().all(|l| l.starts_with("  ")), "{out}");
        // Percent columns line up.
        let pct_end: Vec<usize> = out
            .lines()
            .filter(|l| !l.trim_start().starts_with("note:"))
            .map(|l| l.find('%').unwrap())
            .collect();
        assert!(pct_end.windows(2).all(|w| w[0] == w[1]), "{out}");
        let live = live_text(&parse_live(LIVE).items);
        assert!(live.contains("9%"), "{live}");
        assert!(
            live.contains("resets Sep 24 at 3:19am (Asia/Shanghai)"),
            "{live}"
        );
        // Labels align by display width (a double-width model name).
        let wide = live_text(&[
            row("Week (模型)", 5.0, None, None),
            row("Session", 7.0, None, None),
        ]);
        let pct_col: Vec<usize> = wide
            .lines()
            .map(|l| text::width(&l[..l.find('%').unwrap()]))
            .collect();
        assert_eq!(pct_col[0], pct_col[1], "{wide}");
        let fractional = live_text(&[row("X", 12.5, None, None)]);
        assert_eq!(fractional.trim(), "X  12.5%");
    }

    /// R10: a window whose reset has passed since the cache was written has no percentage (and
    /// no marker): it says when it reset. The others read as recorded.
    #[test]
    fn a_passed_reset_is_said_instead_of_the_old_percentage() {
        let cached = parse_cached(CACHE).unwrap();
        // After the session's reset (Sep 23 15:39), before the week's.
        let reading = Snapshot::cached(&cached).at(ts("2026-09-24T00:00:00Z"));
        let out = format_rows(&reading, &TimeZone::UTC, ts("2026-09-24T00:00:00Z"));
        assert_eq!(
            normalized(&out),
            [
                "Session - reset since cached (Sep 23 15:39)",
                "Week (all models) 77% ! resets Sep 25 04:59",
                "Week (Fable) 100% !! resets Sep 25 04:59",
            ]
        );
        // The `-` ends where the percentages do.
        let ends: Vec<usize> = out
            .lines()
            .map(|l| {
                l.find(" - ")
                    .map_or_else(|| l.find('%').unwrap(), |i| i + 1)
            })
            .collect();
        assert!(ends.windows(2).all(|w| w[0] == w[1]), "{out}");
        // Every reset passed: the exhausted per-model week is not marked critical any more.
        let later = Snapshot::cached(&cached).at(ts("2026-10-01T00:00:00Z"));
        assert_eq!(
            normalized(&format_rows(
                &later,
                &TimeZone::UTC,
                ts("2026-10-01T00:00:00Z")
            )),
            [
                "Session - reset since cached (Sep 23 15:39)",
                "Week (all models) - reset since cached (Sep 25 04:59)",
                "Week (Fable) - reset since cached (Sep 25 04:59)",
            ]
        );
        // A live answer naming a reset already behind keeps its percentage, and its wording.
        let now = ts("2026-09-24T00:00:00Z");
        let rows = [row("Session", 95.0, None, text("Sep 23 at 11pm (UTC)"))];
        let live = format_rows(&Snapshot::live(&rows, now).at(now), &TimeZone::UTC, now);
        assert_eq!(
            normalized(&live),
            ["Session 95% !! resets Sep 23 at 11pm (UTC)"]
        );
        // The same answer, asked before that reset and read after it.
        let asked = ts("2026-09-23T20:00:00Z");
        let aged = format_rows(&Snapshot::live(&rows, asked).at(now), &TimeZone::UTC, now);
        assert_eq!(
            normalized(&aged),
            ["Session - reset since asked (Sep 23 23:00)"]
        );
    }

    /// R10: a window that resets within an hour with at least 25% left is followed by a note;
    /// one further off, with less left, that has reset since, or without a reset is not.
    #[test]
    fn a_window_about_to_reset_with_much_left_gets_a_note() {
        let now = ts("2026-10-08T12:00:00Z");
        let rows = [
            row("A", 29.0, None, at("2026-10-08T12:42:00Z")),
            row("B", 29.0, None, at("2026-10-08T13:00:00Z")),
            row("C", 29.0, None, at("2026-10-08T13:00:01Z")),
            row("D", 76.0, None, at("2026-10-08T12:10:00Z")),
            row("E", 75.0, None, at("2026-10-08T12:00:30Z")),
            row("F", 10.0, None, None),
            row("G", 10.0, None, text("Oct 8 at 12:20pm (UTC)")),
        ];
        let out = format_rows(&Snapshot::live(&rows, now).at(now), &TimeZone::UTC, now);
        assert_eq!(
            normalized(&out),
            [
                "A 29% resets Oct 8 12:42",
                "note: resets in 42 min with 71% left",
                "B 29% resets Oct 8 13:00",
                "note: resets in 60 min with 71% left",
                "C 29% resets Oct 8 13:00",
                "D 76% ! resets Oct 8 12:10",
                "E 75% ! resets Oct 8 12:00",
                "note: resets in 1 min with 25% left",
                "F 10%",
                "G 10% resets Oct 8 at 12:20pm (UTC)",
                "note: resets in 20 min with 90% left",
            ]
        );
        assert!(out.contains("\n  note: resets in 42 min"), "{out}");
        // Cached usage read after the reset: no percentage, no note.
        let cached = CachedUsage {
            fetched_at: Some(ts("2026-10-08T11:00:00Z")),
            rows: vec![row("A", 29.0, None, at("2026-10-08T11:30:00Z"))],
        };
        let out = format_rows(&Snapshot::cached(&cached).at(now), &TimeZone::UTC, now);
        assert!(!out.contains("note"), "{out}");
    }

    #[test]
    fn live_rows_get_the_same_markers_in_text() {
        let out = live_text(&[
            row("Session", 74.0, None, None),
            row("Week (all models)", 75.0, None, None),
            row("Week (Fable)", 90.0, None, None),
        ]);
        assert_eq!(
            normalized(&out),
            [
                "Session 74%",
                "Week (all models) 75% !",
                "Week (Fable) 90% !!"
            ]
        );
    }

    /// R10 `usage --wait`: a window is used up with less than `min_headroom` percent left, of
    /// known usage only: one that has reset since it was cached never is.
    #[test]
    fn used_up_windows_are_known_and_below_min_headroom() {
        let at = |s: &str| s.parse::<Timestamp>().unwrap();
        let now = at("2026-10-08T10:00:00Z");
        let row = |label: &str, percent: f64, resets: &str| UsageRow {
            label: label.into(),
            percent,
            severity: None,
            resets: Some(Resets::At(at(resets))),
        };
        let cached = CachedUsage {
            fetched_at: Some(at("2026-10-08T08:00:00Z")),
            rows: vec![
                row("Session", 100.0, "2026-10-08T09:00:00Z"),
                row("Week (all models)", 90.0, "2026-10-09T09:00:00Z"),
                row("Week (Fable)", 95.5, "2026-10-09T09:00:00Z"),
            ],
        };
        let reading = Snapshot::cached(&cached).at(now);
        let labels = |min| -> Vec<String> {
            exhausted(&reading, min)
                .iter()
                .map(|w| w.label.clone())
                .collect()
        };
        assert_eq!(labels(10), ["Week (Fable)"]);
        assert_eq!(labels(11), ["Week (all models)", "Week (Fable)"]);
        assert!(labels(0).is_empty());
    }
}
