//! R7: live sessions per account — `claude agents --json`, with the `sessions/` fallback.
//!
//! These call the library directly. Only named accounts go through the fake claude here
//! (it then reads fixtures from `CLAUDE_CONFIG_DIR`, never from `$HOME`); the native login's
//! `agents` call is covered through the binary in `sessions_cli.rs`.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::Sandbox;
use remuda::Env;
use remuda::live::{self, LiveSession, Source};
use remuda::provider::Provider;
use remuda::registry::{Account, Home};

fn named(name: &str, home: &Path) -> Account {
    Account {
        provider: Provider::Claude,
        name: name.into(),
        home: Home::Path(home.display().to_string()),
    }
}

fn env(sb: &Sandbox) -> Env {
    [("HOME".to_string(), sb.home().display().to_string())]
        .into_iter()
        .collect()
}

fn ps() -> PathBuf {
    PathBuf::from("/bin/ps")
}

/// `ps -o lstart=` of `pid` in UTC, as claude writes it into `procStart`.
fn proc_start(pid: u32) -> String {
    let out = Command::new(ps())
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .env_clear()
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn session_file(dir: &Path, pid: u32, session_id: &str, proc_start: &str) {
    fs::create_dir_all(dir).unwrap();
    let body = serde_json::json!({
        "pid": pid, "sessionId": session_id, "cwd": "/w/live", "startedAt": 1_790_000_000_000i64,
        "procStart": proc_start, "version": "2.1.281", "kind": "interactive",
        "entrypoint": "cli", "status": "idle"
    });
    fs::write(dir.join(format!("{pid}.json")), body.to_string()).unwrap();
}

fn ids(sessions: &[LiveSession]) -> Vec<(String, Option<String>, Source)> {
    sessions
        .iter()
        .map(|s| (s.account.clone(), s.session_id.clone(), s.source))
        .collect()
}

#[test]
fn agents_json_is_queried_under_each_accounts_home() {
    let sb = Sandbox::new();
    let max = sb.make_claude_home("p/max");
    let team = sb.make_claude_home("p/team");
    sb.set_agents(
        Some(&max),
        r#"[{"pid": 11, "cwd": "/w/m", "kind": "interactive", "startedAt": 1,
             "sessionId": "s-max", "name": "m", "status": "busy"}]"#,
    );
    sb.set_agents(
        Some(&team),
        r#"[{"pid": 21, "sessionId": "s-team-1", "status": "idle"},
            {"pid": 22, "sessionId": "s-team-2", "status": "waiting-for-godot"}]"#,
    );
    // Would be picked up if the fallback ran: it must not, since agents worked.
    session_file(
        &max.join("sessions"),
        std::process::id(),
        "s-file",
        &proc_start(std::process::id()),
    );

    let accounts = [named("max", &max), named("team", &team)];
    let claude = sb.bin().join("claude");
    let got = live::collect(
        &accounts,
        Some(&claude),
        Some(&ps()),
        &env(&sb),
        live::TIMEOUT,
    );
    assert_eq!(
        ids(&got),
        [
            ("claude:max".into(), Some("s-max".into()), Source::Agents),
            (
                "claude:team".into(),
                Some("s-team-1".into()),
                Source::Agents
            ),
            (
                "claude:team".into(),
                Some("s-team-2".into()),
                Source::Agents
            ),
        ]
    );
    assert_eq!(got[0].cwd.as_deref(), Some("/w/m"));
    assert_eq!(got[2].status.as_deref(), Some("waiting-for-godot"));
}

#[test]
fn failing_agents_falls_back_to_verified_session_files() {
    let sb = Sandbox::new();
    let max = sb.make_claude_home("p/max");
    let me = std::process::id();
    let real = proc_start(me);
    let dir = max.join("sessions");
    session_file(&dir, me, "s-alive", &real);

    // A stale file: the pid is running, but it is a different process than the one that
    // wrote the file (its start time differs).
    // Detached from the test's stdio, so a failure before `kill` cannot hold output pipes open.
    let mut other = Command::new("/bin/sleep")
        .arg("30")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    session_file(&dir, other.id(), "s-stale", "Mon Jan  5 00:00:00 2026");
    // A dead pid.
    let mut gone = Command::new("/usr/bin/true").spawn().unwrap();
    let gone_pid = gone.id();
    gone.wait().unwrap();
    session_file(&dir, gone_pid, "s-dead", &real);
    // A key file that must never be opened.
    let key = dir.join(format!("{me}.0123456789abcdef.key"));
    fs::write(&key, "secret").unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o000)).unwrap();

    let accounts = [named("max", &max)];
    let claude = sb.bin().join("claude");
    // No fake-agents.json: the fake claude prints nothing, which is not an agents list.
    let got = live::collect(
        &accounts,
        Some(&claude),
        Some(&ps()),
        &env(&sb),
        live::TIMEOUT,
    );
    other.kill().unwrap();
    other.wait().unwrap();
    assert_eq!(
        ids(&got),
        [(
            "claude:max".into(),
            Some("s-alive".into()),
            Source::SessionFile
        )]
    );
    assert_eq!(got[0].pid, Some(me));
    assert_eq!(got[0].cwd.as_deref(), Some("/w/live"));
}

#[test]
fn unparseable_or_hanging_agents_fall_back_too() {
    let sb = Sandbox::new();
    let garbled = sb.make_claude_home("p/garbled");
    let hung = sb.make_claude_home("p/hung");
    sb.set_agents(Some(&garbled), "Usage: claude agents [options]\n");
    sb.set_hang(Some(&hung), 30);
    let me = std::process::id();
    session_file(&garbled.join("sessions"), me, "s-g", &proc_start(me));
    session_file(&hung.join("sessions"), me, "s-h", &proc_start(me));

    let accounts = [named("garbled", &garbled), named("hung", &hung)];
    let claude = sb.bin().join("claude");
    let start = Instant::now();
    let got = live::collect(
        &accounts,
        Some(&claude),
        Some(&ps()),
        &env(&sb),
        Duration::from_millis(500),
    );
    assert!(start.elapsed() < Duration::from_secs(10));
    assert_eq!(
        ids(&got),
        [
            (
                "claude:garbled".into(),
                Some("s-g".into()),
                Source::SessionFile
            ),
            (
                "claude:hung".into(),
                Some("s-h".into()),
                Source::SessionFile
            ),
        ]
    );
}

#[test]
fn native_login_falls_back_to_home_dot_claude_sessions() {
    let sb = Sandbox::new();
    let me = std::process::id();
    session_file(
        &sb.home().join(".claude/sessions"),
        me,
        "s-native",
        &proc_start(me),
    );
    let accounts = [Account::default_for(Provider::Claude)];
    assert_eq!(
        live::sessions_dir(&accounts[0], &env(&sb)),
        Some(sb.home().join(".claude/sessions"))
    );
    // No claude at all: straight to the fallback.
    let got = live::collect(&accounts, None, Some(&ps()), &env(&sb), live::TIMEOUT);
    assert_eq!(
        ids(&got),
        [(
            "claude:default".into(),
            Some("s-native".into()),
            Source::SessionFile
        )]
    );
    // Without ps nothing can be verified, so nothing is claimed.
    assert!(live::collect(&accounts, None, None, &env(&sb), live::TIMEOUT).is_empty());
}

/// Background entries have a short id and a state instead of a pid (R7, claude 2.1.281);
/// stopped ones are listed only with `--all`, which remuda passes.
#[test]
fn background_sessions_are_collected_including_stopped_ones() {
    let sb = Sandbox::new();
    let max = sb.make_claude_home("p/max");
    let running = r#"{"id": "766560c5", "cwd": "/private/tmp/x", "kind": "background",
        "startedAt": 1790192116861, "sessionId": "766560c5-74e6-45f5-89fd-d92926b14898",
        "state": "blocked"}"#;
    let interactive = r#"{"pid": 11, "cwd": "/w/m", "kind": "interactive", "startedAt": 1,
        "sessionId": "s-max", "name": "m", "status": "busy"}"#;
    let stopped = r#"{"id": "0badf00d", "cwd": "/w/old", "kind": "background",
        "startedAt": 1790000000000, "sessionId": "0badf00d-0000", "state": "stopped"}"#;
    sb.set_agents(Some(&max), &format!("[{running}, {interactive}]"));
    sb.set_agents_all(
        Some(&max),
        &format!("[{running}, {interactive}, {stopped}]"),
    );

    let claude = sb.bin().join("claude");
    let got = live::collect(
        &[named("max", &max)],
        Some(&claude),
        None,
        &env(&sb),
        live::TIMEOUT,
    );
    let summary: Vec<(Option<u32>, Option<&str>, Option<&str>)> = got
        .iter()
        .map(|s| (s.pid, s.short_id.as_deref(), s.status.as_deref()))
        .collect();
    assert_eq!(
        summary,
        [
            (None, Some("766560c5"), Some("blocked")),
            (Some(11), None, Some("busy")),
            (None, Some("0badf00d"), Some("stopped")),
        ]
    );
    assert!(got[0].is_background() && !got[0].is_inactive());
    // Only the `--all` fixture lists the stopped session, and it is found under max's
    // CLAUDE_CONFIG_DIR: remuda asked with `--all` under the account's environment.
    assert!(got[2].is_inactive());
}

/// R16: before a resume remuda must know that no account runs the session. An account whose
/// `agents --json` failed is "unknown" unless the `sessions/` fallback could be read.
#[test]
fn accounts_that_could_not_be_read_are_reported() {
    let sb = Sandbox::new();
    let ok = sb.make_claude_home("p/ok");
    let blind = sb.make_claude_home("p/blind");
    sb.set_agents(
        Some(&ok),
        r#"[{"pid": 11, "sessionId": "s-ok", "status": "idle"}]"#,
    );
    // `blind` has no agents fixture: the fake claude prints nothing, which is not a list.
    let accounts = [named("ok", &ok), named("blind", &blind)];
    let claude = sb.bin().join("claude");

    // No ps: blind's sessions cannot be verified.
    let got = live::collect_report(&accounts, Some(&claude), None, &env(&sb), live::TIMEOUT);
    assert_eq!(
        ids(&got.sessions),
        [("claude:ok".into(), Some("s-ok".into()), Source::Agents)]
    );
    let unknown: Vec<&str> = got.unknown.iter().map(|u| u.account.as_str()).collect();
    assert_eq!(unknown, ["claude:blind"]);
    let reason = &got.unknown[0].reason;
    assert!(reason.contains("agents --json"), "{reason}");
    assert!(reason.contains("ps"), "{reason}");
    // `collect` is the same without the report.
    assert_eq!(
        live::collect(&accounts, Some(&claude), None, &env(&sb), live::TIMEOUT),
        got.sessions
    );

    // With ps: no `sessions/` directory means no sessions ever ran there.
    let got = live::collect_report(
        &accounts,
        Some(&claude),
        Some(&ps()),
        &env(&sb),
        live::TIMEOUT,
    );
    assert!(got.unknown.is_empty(), "{:?}", got.unknown);

    // An unreadable `sessions/` directory proves nothing.
    let dir = blind.join("sessions");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
    let got = live::collect_report(
        &accounts,
        Some(&claude),
        Some(&ps()),
        &env(&sb),
        live::TIMEOUT,
    );
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let unknown: Vec<&str> = got.unknown.iter().map(|u| u.account.as_str()).collect();
    assert_eq!(unknown, ["claude:blind"]);

    // No claude and no ps at all: every account is unknown.
    let got = live::collect_report(&accounts, None, None, &env(&sb), live::TIMEOUT);
    assert_eq!(got.unknown.len(), 2);
}

/// P-a: in the fallback, a session file of a running process that cannot be read (e.g. caught
/// half-written) says nothing about that process: the account is unknown, not idle.
#[test]
fn a_running_pids_unreadable_session_file_makes_the_account_unknown() {
    let sb = Sandbox::new();
    let blind = sb.make_claude_home("p/blind");
    let dir = blind.join("sessions");
    fs::create_dir_all(&dir).unwrap();
    let me = std::process::id();
    let accounts = [named("blind", &blind)];
    let claude = sb.bin().join("claude");
    let report = || {
        live::collect_report(
            &accounts,
            Some(&claude),
            Some(&ps()),
            &env(&sb),
            live::TIMEOUT,
        )
    };
    for body in [
        r#"{"pid": "#.to_string(),
        format!(r#"{{"pid": {me}, "sessionId": "s"}}"#),
    ] {
        fs::write(dir.join(format!("{me}.json")), body).unwrap();
        let got = report();
        assert_eq!(got.sessions, []);
        let [unknown] = got.unknown.as_slice() else {
            panic!("{got:?}")
        };
        assert_eq!(unknown.account, "claude:blind");
        // C3: the file is named in full, so a stale one can be removed.
        let file = dir.join(format!("{me}.json"));
        assert!(
            unknown.reason.ends_with(&format!(
                "and cannot read {} of running pid {me} (if it is stale, remove it)",
                file.display()
            )),
            "{}",
            unknown.reason
        );
    }
    // The same file of a process that is gone: nothing runs there.
    let mut gone = Command::new("/usr/bin/true").spawn().unwrap();
    let gone_pid = gone.id();
    gone.wait().unwrap();
    fs::remove_file(dir.join(format!("{me}.json"))).unwrap();
    fs::write(dir.join(format!("{gone_pid}.json")), r#"{"pid": "#).unwrap();
    let got = report();
    assert_eq!((got.sessions, got.unknown), (vec![], vec![]));
}

// --- background session commands (R7, R16) ----------------------------------------------

#[test]
fn logs_stop_and_rm_run_under_the_accounts_environment() {
    let sb = Sandbox::new();
    let max = sb.make_claude_home("p/max");
    fs::write(
        max.join("fake-logs.txt"),
        "\u{1b}[2J\u{1b}[H\u{1b}[1mBuilding\u{1b}[0m the index\r\nstep 2\r\n",
    )
    .unwrap();
    let claude = sb.bin().join("claude");
    let account = named("max", &max);

    let logs = live::logs(&claude, &account, "766560c5", Duration::from_secs(10));
    assert_eq!(logs, Ok("Building the index\nstep 2".to_string()));
    assert_eq!(
        live::control(
            &claude,
            &account,
            live::Control::Stop,
            "766560c5",
            live::TIMEOUT
        ),
        Ok(String::new())
    );
    assert_eq!(
        live::control(
            &claude,
            &account,
            live::Control::Remove,
            "766560c5",
            live::TIMEOUT
        ),
        Ok(String::new())
    );
    let calls: Vec<(Vec<String>, Option<String>)> = sb
        .invocations()
        .into_iter()
        .map(|i| (i.args, i.config_dir))
        .collect();
    let home = Some(max.display().to_string());
    assert_eq!(
        calls,
        [
            (
                vec!["logs".to_string(), "766560c5".to_string()],
                home.clone()
            ),
            (
                vec!["stop".to_string(), "766560c5".to_string()],
                home.clone()
            ),
            // Never any of rm's destructive flags.
            (vec!["rm".to_string(), "766560c5".to_string()], home),
        ]
    );
}

#[test]
fn background_command_failures_say_why() {
    let sb = Sandbox::new();
    let max = sb.make_claude_home("p/max");
    let claude = sb.bin().join("claude");
    let account = named("max", &max);
    // No logs fixture: the fake fails like claude does for an unknown session.
    let err = live::logs(&claude, &account, "0badf00d", Duration::from_secs(10)).unwrap_err();
    assert!(err.contains("exited with status 1"), "{err}");
    assert!(err.contains("no fixture"), "{err}");
    fs::write(
        max.join("fake-rm-error.txt"),
        "Refusing: 2 unpushed commits\n",
    )
    .unwrap();
    let err = live::control(
        &claude,
        &account,
        live::Control::Remove,
        "0badf00d",
        live::TIMEOUT,
    )
    .unwrap_err();
    assert_eq!(err, "exited with status 1: Refusing: 2 unpushed commits");
    // The native login, and the stdout of a command that fails without stderr.
    fs::write(sb.home().join(".fake-stop-error.txt"), "").unwrap();
    let native = Account::default_for(Provider::Claude);
    let err = live::control(
        &claude,
        &native,
        live::Control::Stop,
        "0badf00d",
        live::TIMEOUT,
    )
    .unwrap_err();
    assert_eq!(err, "exited with status 1");
}
