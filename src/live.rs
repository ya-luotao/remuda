//! Running sessions per account (SPEC R7): `claude agents --json`, falling back to
//! `<home>/sessions/<pid>.json` checked against the live process table.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::Value;

use crate::launch::{self, EnvChange};
use crate::probe::Outcome;
use crate::registry::Account;
use crate::{Env, probe, text};

/// `--all` also lists background sessions that have stopped or finished (R7).
pub const AGENTS_ARGS: &[&str] = &["agents", "--json", "--all"];
/// Default wait for one account's `claude agents --json` (measured at ~0.14 s).
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// Where a [`LiveSession`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `claude agents --json`.
    Agents,
    /// `sessions/<pid>.json`, with the pid alive and its start time matching.
    SessionFile,
}

/// A claude session of one account known to `claude agents`: running interactive sessions,
/// and background sessions (running, or finished when listed with `--all`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSession {
    /// `provider:name`.
    pub account: String,
    /// Interactive sessions only; background entries have none (R7).
    pub pid: Option<u32>,
    /// The 8-character id of a background session, for `claude attach|logs|stop|rm <id>`.
    pub short_id: Option<String>,
    pub cwd: Option<String>,
    /// `interactive`, `background`, ... as reported.
    pub kind: Option<String>,
    /// Epoch milliseconds.
    pub started_at: Option<i64>,
    pub session_id: Option<String>,
    pub name: Option<String>,
    /// `status` (interactive) or `state` (background); shown verbatim, including values
    /// remuda does not know.
    pub status: Option<String>,
    pub source: Source,
}

/// What identifies a [`LiveSession`] within its account across collections.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LiveId {
    Pid(u32),
    Short(String),
    Session(String),
    Unknown,
}

/// Background states after which the session no longer runs (claude 2.1.281: `stop` gives
/// `stopped`, a finished task `done`).
pub const INACTIVE_STATES: &[&str] = &["stopped", "done"];

impl LiveSession {
    /// A background session (`claude --bg`): operated through its short id.
    pub fn is_background(&self) -> bool {
        self.kind.as_deref() == Some("background")
            || (self.pid.is_none() && self.short_id.is_some())
    }

    /// A background session that has stopped or finished; listed only with `--all`.
    pub fn is_inactive(&self) -> bool {
        self.is_background()
            && self
                .status
                .as_deref()
                .is_some_and(|s| INACTIVE_STATES.contains(&s))
    }

    /// `(account, id)`: keeps a selection on the same session when the list is collected again.
    pub fn key(&self) -> (String, LiveId) {
        let id = match (&self.pid, &self.short_id, &self.session_id) {
            (Some(pid), _, _) => LiveId::Pid(*pid),
            (None, Some(short), _) => LiveId::Short(short.clone()),
            (None, None, Some(session)) => LiveId::Session(session.clone()),
            (None, None, None) => LiveId::Unknown,
        };
        (self.account.clone(), id)
    }
}

/// Parses `claude agents --json` (a JSON array); `None` when the output is not one.
/// Interactive entries are identified by `pid`, background ones by their short `id` (they
/// have no pid); entries with neither are dropped. Unknown fields are ignored.
pub fn parse_agents(stdout: &str, account: &str) -> Option<Vec<LiveSession>> {
    let v: Value = serde_json::from_str(stdout).ok()?;
    let entries = v.as_array()?;
    Some(
        entries
            .iter()
            .filter_map(|e| {
                let session = from_json(e, account, pid_of(e), Source::Agents);
                (session.pid.is_some() || session.short_id.is_some()).then_some(session)
            })
            .collect(),
    )
}

fn pid_of(v: &Value) -> Option<u32> {
    v.get("pid")?.as_u64().and_then(|p| u32::try_from(p).ok())
}

fn from_json(v: &Value, account: &str, pid: Option<u32>, source: Source) -> LiveSession {
    let text = |key: &str| {
        v.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    LiveSession {
        account: account.to_string(),
        pid,
        short_id: text("id"),
        cwd: text("cwd"),
        kind: text("kind"),
        started_at: v.get("startedAt").and_then(Value::as_i64),
        session_id: text("sessionId"),
        name: text("name"),
        status: text("status").or_else(|| text("state")),
        source,
    }
}

/// `<home>/sessions`, or `$HOME/.claude/sessions` for the native login.
pub fn sessions_dir(account: &Account, env: &Env) -> Option<PathBuf> {
    account.home_dir(env).map(|d| d.join("sessions"))
}

/// Sessions recorded in `dir/<pid>.json` whose process is alive with a start time equal to
/// the file's `procStart` (UTC, ctime format). `start_time(pid)` returns the running
/// process's start time in the same format, or `None` when there is no such process. Only
/// files named `<digits>.json` are opened; `*.key` files are never touched.
pub fn read_session_files(
    dir: &Path,
    account: &str,
    start_time: impl Fn(u32) -> Option<String> + Sync,
) -> Vec<LiveSession> {
    read_session_files_checked(dir, account, start_time).0
}

/// [`read_session_files`], and the running pids whose file says nothing usable about them:
/// it cannot be read (e.g. caught half-written), is not JSON, lacks `pid` or `procStart`, or
/// names another pid. Such a process may run any session (R16). A file that vanished meanwhile
/// belonged to a session that ended.
pub fn read_session_files_checked(
    dir: &Path,
    account: &str,
    start_time: impl Fn(u32) -> Option<String> + Sync,
) -> (Vec<LiveSession>, Vec<u32>) {
    let Ok(listing) = fs::read_dir(dir) else {
        return (Vec::new(), Vec::new());
    };
    // Decide by name alone before opening anything: `<digits>.json` only.
    let mut pids: Vec<u32> = listing
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let stem = name.to_str()?.strip_suffix(".json")?;
            if stem.is_empty() || !stem.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            stem.parse().ok()
        })
        .collect();
    pids.sort_unstable();
    let gone = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
    // `None`: the file is there but says nothing usable about its pid.
    let candidates: Vec<(u32, Option<(Value, String)>)> = pids
        .into_iter()
        .filter_map(|pid| {
            let path = dir.join(format!("{pid}.json"));
            match fs::metadata(&path) {
                Ok(meta) if meta.is_file() => {}
                Ok(_) => return None,
                Err(e) if gone(&e) => return None,
                Err(_) => return Some((pid, None)),
            }
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(e) if gone(&e) => return None,
                Err(_) => return Some((pid, None)),
            };
            let parsed = serde_json::from_slice::<Value>(&bytes).ok().and_then(|v| {
                // The file must describe the process its name says.
                if pid_of(&v)? != pid {
                    return None;
                }
                let recorded = v.get("procStart")?.as_str()?.to_string();
                Some((v, recorded))
            });
            Some((pid, parsed))
        })
        .collect();
    let running = probe::parallel(&candidates, |(pid, _)| start_time(*pid));
    let (mut sessions, mut unreadable) = (Vec::new(), Vec::new());
    for ((pid, parsed), running) in candidates.iter().zip(running) {
        match (parsed, running) {
            (_, None) => {}
            (None, Some(_)) => unreadable.push(*pid),
            (Some((v, recorded)), Some(running)) => {
                if same_start(&running, recorded) {
                    sessions.push(from_json(v, account, Some(*pid), Source::SessionFile));
                }
            }
        }
    }
    (sessions, unreadable)
}

/// Start time of process `pid` in UTC, ctime format (`Wed Sep 23 14:41:43 2026`), from
/// `ps -o lstart= -p <pid>` run with `TZ=UTC LC_ALL=C`. `None` when there is no such process.
pub fn process_start(ps: &Path, pid: u32) -> Option<String> {
    process_start_checked(ps, pid).ok().flatten()
}

/// [`process_start`], with `Err` when `ps` itself did not run to completion (could not be
/// started, timed out, was killed): then nothing is known about the process.
pub fn process_start_checked(ps: &Path, pid: u32) -> Result<Option<String>, String> {
    let pid = pid.to_string();
    let env = [
        EnvChange::Set("TZ".into(), "UTC".into()),
        EnvChange::Set("LC_ALL".into(), "C".into()),
    ];
    let outcome = probe::run_captured_with(ps, &["-o", "lstart=", "-p", &pid], &env, PS_TIMEOUT);
    match &outcome {
        // `ps -p` exits non-zero when there is no such process.
        Outcome::Exited { code: Some(_), .. } => Ok(outcome
            .success_stdout()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)),
        _ => Err(outcome.describe(PS_TIMEOUT)),
    }
}

const PS_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether two ctime-format times are equal, ignoring whitespace differences (ctime pads
/// single-digit days, `ps` pads the end).
pub fn same_start(a: &str, b: &str) -> bool {
    let words = |s: &str| s.split_whitespace().map(str::to_string).collect::<Vec<_>>();
    let (a, b) = (words(a), words(b));
    !a.is_empty() && a == b
}

/// Live sessions of every account, queried in parallel. Per account: `claude agents --json`
/// under the account's environment; if claude is missing, fails, times out or prints
/// something else, the account's `sessions/` directory, verified with `ps`. With neither
/// claude nor ps, an account contributes nothing ([`collect_report`] says which).
pub fn collect(
    accounts: &[Account],
    claude: Option<&Path>,
    ps: Option<&Path>,
    env: &Env,
    timeout: Duration,
) -> Vec<LiveSession> {
    collect_report(accounts, claude, ps, env, timeout).sessions
}

/// What [`collect_report`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Collection {
    pub sessions: Vec<LiveSession>,
    /// Accounts whose running sessions could not be read: they may run anything.
    pub unknown: Vec<Unknown>,
}

/// An account [`collect_report`] could not read, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unknown {
    /// `provider:name`.
    pub account: String,
    pub reason: String,
}

/// [`collect`], also naming the accounts it could not read (R16: a resume must know that no
/// account runs the session). An account is read when `agents --json` answered, or when its
/// `sessions/` directory could be listed with `ps` at hand (a missing directory means no
/// session ever ran there), `ps` itself worked for every candidate, and every running pid's
/// file could be read ([`read_session_files_checked`]).
pub fn collect_report(
    accounts: &[Account],
    claude: Option<&Path>,
    ps: Option<&Path>,
    env: &Env,
    timeout: Duration,
) -> Collection {
    let accounts: Vec<&Account> = accounts.iter().filter(|a| a.provider.has_live()).collect();
    let per_account = probe::parallel(&accounts, |account| {
        let qualified = account.qualified();
        let agents = match claude {
            Some(claude) => {
                let change = launch::env_change(account);
                let outcome = probe::run_captured(claude, AGENTS_ARGS, &change, timeout);
                match outcome
                    .success_stdout()
                    .map(|out| parse_agents(out, &qualified))
                {
                    Some(Some(sessions)) => return Ok(sessions),
                    Some(None) => "`claude agents --json` printed no session list".to_string(),
                    None => format!("`claude agents --json` {}", outcome.describe(timeout)),
                }
            }
            None => "`claude` not found on PATH".to_string(),
        };
        let fallback = match (ps, sessions_dir(account, env)) {
            (None, _) => Err("no `ps` to check its sessions/ directory".to_string()),
            (_, None) => Err("its sessions/ directory is unknown (HOME is not set)".to_string()),
            (Some(ps), Some(dir)) => match fs::read_dir(&dir) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
                Err(e) => Err(format!("cannot read {}: {e}", dir.display())),
                Ok(_) => {
                    let ps_failed = AtomicBool::new(false);
                    let (sessions, unreadable) =
                        read_session_files_checked(&dir, &qualified, |pid| {
                            process_start_checked(ps, pid).unwrap_or_else(|_| {
                                ps_failed.store(true, Ordering::Relaxed);
                                None
                            })
                        });
                    if ps_failed.load(Ordering::Relaxed) {
                        Err(format!("`{}` failed", ps.display()))
                    } else if !unreadable.is_empty() {
                        // Named in full: a stale half-written file stays until removed.
                        let files: Vec<String> = unreadable
                            .iter()
                            .map(|pid| {
                                let file = dir.join(format!("{pid}.json"));
                                format!("{} of running pid {pid}", file.display())
                            })
                            .collect();
                        Err(format!(
                            "cannot read {} (if it is stale, remove it)",
                            files.join(", ")
                        ))
                    } else {
                        Ok(sessions)
                    }
                }
            },
        };
        fallback.map_err(|why| Unknown {
            account: qualified.clone(),
            reason: format!("{agents}, and {why}"),
        })
    });
    let mut collection = Collection::default();
    for result in per_account {
        match result {
            Ok(sessions) => collection.sessions.extend(sessions),
            Err(unknown) => collection.unknown.push(unknown),
        }
    }
    collection
}

/// A background session's short id as claude prints it: 8 lowercase hex digits. Anything
/// else is not passed to claude, where it could be read as an option (R16).
pub fn is_short_id(id: &str) -> bool {
    id.len() == 8 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn check_short_id(id: &str) -> Result<(), String> {
    if is_short_id(id) {
        Ok(())
    } else {
        Err(format!("{id:?} is not a background session id"))
    }
}

/// `claude logs <id>` usually answers at once; it is given this long.
pub const LOGS_TIMEOUT: Duration = Duration::from_secs(15);

/// What remuda does to a background session besides attaching (R7, R16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// `claude stop <id>`: the conversation is kept.
    Stop,
    /// `claude rm <id>`: deletes the session and its worktree. Never with its
    /// `--discard-unpushed` / `--force-remove-worktree` flags.
    Remove,
}

impl Control {
    /// The claude subcommand.
    pub fn command(self) -> &'static str {
        match self {
            Control::Stop => "stop",
            Control::Remove => "rm",
        }
    }
}

/// `claude logs <short_id>` under the account's environment, as plain text; a malformed id
/// is refused without running anything (the raw output
/// is terminal bytes: see [`text::terminal_text`]). `Err` says why it failed.
pub fn logs(
    claude: &Path,
    account: &Account,
    short_id: &str,
    timeout: Duration,
) -> Result<String, String> {
    check_short_id(short_id)?;
    let change = launch::env_change(account);
    let outcome = probe::run_captured(claude, &["logs", short_id], &change, timeout);
    match outcome.success_stdout() {
        Some(out) => Ok(text::terminal_text(out)),
        None => Err(failure(&outcome, timeout)),
    }
}

/// `claude stop|rm <short_id>` under the account's environment; its output (trimmed), or
/// why it failed (a malformed id is refused without running anything).
pub fn control(
    claude: &Path,
    account: &Account,
    verb: Control,
    short_id: &str,
    timeout: Duration,
) -> Result<String, String> {
    check_short_id(short_id)?;
    let change = launch::env_change(account);
    let outcome = probe::run_captured(claude, &[verb.command(), short_id], &change, timeout);
    match outcome.success_stdout() {
        Some(out) => Ok(text::terminal_text(out).trim().to_string()),
        None => Err(failure(&outcome, timeout)),
    }
}

/// [`Outcome::describe`], falling back to stdout when a failing command wrote nothing to
/// stderr.
fn failure(outcome: &Outcome, timeout: Duration) -> String {
    if let Outcome::Exited {
        code: Some(code),
        stdout,
        stderr,
    } = outcome
        && stderr.trim().is_empty()
        && let Some(line) = stdout.lines().map(str::trim).find(|l| !l.is_empty())
    {
        return format!("exited with status {code}: {line}");
    }
    outcome.describe(timeout)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::registry::CLAUDE;

    fn session(account: &str, pid: Option<u32>, source: Source) -> LiveSession {
        LiveSession {
            account: account.into(),
            pid,
            short_id: None,
            cwd: None,
            kind: None,
            started_at: None,
            session_id: None,
            name: None,
            status: None,
            source,
        }
    }

    #[test]
    fn agents_output_is_parsed_leniently() {
        let out = r#"[
          {"pid": 101, "cwd": "/w/a", "kind": "interactive", "startedAt": 1790147118010,
           "sessionId": "s-1", "name": "fix bug", "status": "idle", "future": {"x": 1}},
          {"pid": 102, "cwd": "", "kind": "bg", "sessionId": "", "status": "dreaming"},
          {"cwd": "/no/pid"},
          {"pid": "103"},
          {"pid": -1},
          7
        ]"#;
        let got = parse_agents(out, "claude:max").unwrap();
        let mut first = session("claude:max", Some(101), Source::Agents);
        first.cwd = Some("/w/a".into());
        first.kind = Some("interactive".into());
        first.started_at = Some(1790147118010);
        first.session_id = Some("s-1".into());
        first.name = Some("fix bug".into());
        first.status = Some("idle".into());
        let mut second = session("claude:max", Some(102), Source::Agents);
        second.kind = Some("bg".into());
        second.status = Some("dreaming".into());
        assert_eq!(got, [first, second]);

        assert_eq!(parse_agents("[]", "a"), Some(vec![]));
        assert_eq!(parse_agents("", "a"), None);
        assert_eq!(parse_agents("{\"pid\": 1}", "a"), None);
        assert_eq!(parse_agents("No sessions.\n", "a"), None);
    }

    /// Background entries (claude 2.1.281) have a short `id` and a `state`, and no `pid` (R7).
    #[test]
    fn background_entries_have_a_short_id_and_a_state() {
        let out = r#"[
          {"id": "766560c5", "cwd": "/private/tmp/x", "kind": "background",
           "startedAt": 1790192116861,
           "sessionId": "766560c5-74e6-45f5-89fd-d92926b14898", "state": "blocked"},
          {"id": "f35a30d1", "kind": "background", "sessionId": "f35a30d1-eebf", "state": "done",
           "status": "wins-over-state"},
          {"id": "", "kind": "background", "state": "stopped"},
          {"id": 5, "state": "stopped"}
        ]"#;
        let got = parse_agents(out, "claude:default").unwrap();
        let mut blocked = session("claude:default", None, Source::Agents);
        blocked.short_id = Some("766560c5".into());
        blocked.cwd = Some("/private/tmp/x".into());
        blocked.kind = Some("background".into());
        blocked.started_at = Some(1790192116861);
        blocked.session_id = Some("766560c5-74e6-45f5-89fd-d92926b14898".into());
        blocked.status = Some("blocked".into());
        let mut done = session("claude:default", None, Source::Agents);
        done.short_id = Some("f35a30d1".into());
        done.kind = Some("background".into());
        done.session_id = Some("f35a30d1-eebf".into());
        done.status = Some("wins-over-state".into());
        assert_eq!(got, [blocked.clone(), done]);
        assert!(blocked.is_background());
        assert!(!blocked.is_inactive());
        assert_eq!(
            blocked.key(),
            (
                "claude:default".to_string(),
                LiveId::Short("766560c5".into())
            )
        );
    }

    #[test]
    fn inactive_background_sessions() {
        let bg = |state: &str| {
            let mut s = session("a", None, Source::Agents);
            s.short_id = Some("x".into());
            s.kind = Some("background".into());
            s.status = Some(state.into());
            s
        };
        assert!(bg("stopped").is_inactive());
        assert!(bg("done").is_inactive());
        assert!(!bg("blocked").is_inactive());
        assert!(!bg("working").is_inactive());
        // Interactive sessions are running by definition.
        let mut interactive = session("a", Some(1), Source::Agents);
        interactive.status = Some("stopped".into());
        assert!(!interactive.is_background());
        assert!(!interactive.is_inactive());
        assert_eq!(interactive.key(), ("a".to_string(), LiveId::Pid(1)));
    }

    #[test]
    fn short_ids_are_eight_lowercase_hex_digits() {
        assert!(is_short_id("766560c5"));
        assert!(is_short_id("0badf00d"));
        for bad in [
            "",
            "766560c",
            "766560c5a",
            "766560C5",
            "--force",
            "-766560c",
            "76656 c5",
        ] {
            assert!(!is_short_id(bad), "{bad:?}");
        }
        let account = Account::default_for(CLAUDE);
        let nowhere = Path::new("/nonexistent/claude");
        let err = logs(nowhere, &account, "--all", TIMEOUT).unwrap_err();
        assert_eq!(err, "\"--all\" is not a background session id");
        let err = control(nowhere, &account, Control::Remove, "-f", TIMEOUT).unwrap_err();
        assert_eq!(err, "\"-f\" is not a background session id");
    }

    #[test]
    fn start_times_compare_ignoring_padding() {
        assert!(same_start(
            "Wed Sep  3 01:02:03 2026    ",
            "Wed Sep 3 01:02:03 2026"
        ));
        assert!(same_start(
            "Wed Sep 23 14:41:43 2026",
            " Wed Sep 23 14:41:43 2026\n"
        ));
        assert!(!same_start(
            "Wed Sep 23 14:41:43 2026",
            "Wed Sep 23 14:41:44 2026"
        ));
        assert!(!same_start("", ""));
    }

    #[test]
    fn session_files_need_a_live_pid_with_the_same_start_time() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let write = |name: &str, body: &str| fs::write(d.join(name), body).unwrap();
        write(
            "101.json",
            r#"{"pid": 101, "sessionId": "s-alive", "cwd": "/w/a", "startedAt": 5,
                "procStart": "Wed Sep 23 14:41:43 2026", "kind": "interactive",
                "name": "n", "status": "busy", "messagingSocketPath": "/tmp/x"}"#,
        );
        write(
            "102.json",
            r#"{"pid": 102, "sessionId": "s-reused-pid", "procStart": "Mon Sep 21 09:00:00 2026"}"#,
        );
        write(
            "103.json",
            r#"{"pid": 103, "sessionId": "s-dead", "procStart": "Wed Sep 23 14:41:43 2026"}"#,
        );
        write("104.json", r#"{"pid": 104, "sessionId": "s-no-procstart"}"#);
        write(
            "105.json",
            r#"{"pid": 999, "sessionId": "s-pid-mismatch", "procStart": "Wed Sep 23 14:41:43 2026"}"#,
        );
        write("106.json", "not json");
        write("1x.json", r#"{"pid": 1}"#);
        write("notes.txt", "x");
        // Never opened: unreadable, and would be an error if it were.
        write("101.abcdef.key", "secret");
        fs::set_permissions(d.join("101.abcdef.key"), fs::Permissions::from_mode(0o000)).unwrap();
        fs::create_dir(d.join("107.json")).unwrap();

        let table: HashMap<u32, &str> = [
            (101, "Wed Sep 23 14:41:43 2026    "),
            (102, "Wed Sep 23 14:41:43 2026    "),
            (104, "Wed Sep 23 14:41:43 2026    "),
            (105, "Wed Sep 23 14:41:43 2026    "),
            (999, "Wed Sep 23 14:41:43 2026    "),
        ]
        .into_iter()
        .collect();
        let got = read_session_files(d, "claude:team", |pid| {
            table.get(&pid).map(|s| s.to_string())
        });
        let mut want = session("claude:team", Some(101), Source::SessionFile);
        want.cwd = Some("/w/a".into());
        want.kind = Some("interactive".into());
        want.started_at = Some(5);
        want.session_id = Some("s-alive".into());
        want.name = Some("n".into());
        want.status = Some("busy".into());
        assert_eq!(got, [want]);

        assert!(read_session_files(&d.join("missing"), "a", |_| None).is_empty());
    }

    /// P-a: `<digits>.json` of a running pid that cannot be read, is not JSON, lacks `pid` or
    /// `procStart`, or names another pid is reported; of a dead pid it is ignored.
    #[test]
    fn unreadable_session_files_of_running_pids_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let write = |name: &str, body: &str| fs::write(d.join(name), body).unwrap();
        let start = "Wed Sep 23 14:41:43 2026";
        write(
            "201.json",
            &format!(r#"{{"pid": 201, "sessionId": "s-ok", "procStart": "{start}"}}"#),
        );
        write("202.json", r#"{"pid": 202, "sessionId": "s-half"#);
        write("203.json", r#"{"pid": 203, "sessionId": "s-no-procstart"}"#);
        write(
            "204.json",
            &format!(r#"{{"sessionId": "s-no-pid", "procStart": "{start}"}}"#),
        );
        write(
            "205.json",
            &format!(r#"{{"pid": 999, "sessionId": "s-other", "procStart": "{start}"}}"#),
        );
        write("206.json", "{}");
        fs::set_permissions(d.join("206.json"), fs::Permissions::from_mode(0o000)).unwrap();
        // Dead: whatever the file says, nothing runs under it.
        write("207.json", "not json");
        let alive: HashMap<u32, &str> = [201, 202, 203, 204, 205, 206]
            .into_iter()
            .map(|pid| (pid, start))
            .collect();
        let (sessions, unreadable) = read_session_files_checked(d, "claude:team", |pid| {
            alive.get(&pid).map(|s| s.to_string())
        });
        let ids: Vec<Option<&str>> = sessions.iter().map(|s| s.session_id.as_deref()).collect();
        assert_eq!(ids, [Some("s-ok")]);
        assert_eq!(unreadable, [202, 203, 204, 205, 206]);
    }

    fn ps() -> PathBuf {
        PathBuf::from("/bin/ps")
    }

    #[test]
    fn process_start_is_utc_and_none_for_a_dead_pid() {
        let me = std::process::id();
        let utc = process_start(&ps(), me).expect("own process has a start time");
        let fmt = "%a %b %e %H:%M:%S %Y";
        let parse = |s: &str| {
            let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
            jiff::civil::DateTime::strptime(fmt, &s).unwrap()
        };
        // The same instant printed in a zone 8 hours ahead of UTC.
        let out = std::process::Command::new(ps())
            .args(["-o", "lstart=", "-p", &me.to_string()])
            .env("TZ", "Asia/Shanghai")
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        let shanghai = String::from_utf8(out.stdout).unwrap();
        let diff = parse(&shanghai) - parse(&utc);
        assert_eq!(diff.get_hours(), 8, "{utc:?} vs {shanghai:?}");

        let mut child = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        assert_eq!(process_start(&ps(), dead), None);
    }
}
