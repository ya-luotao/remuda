//! R10, R24: `remuda usage [<account>] [--live]`, its usage history and `--history`.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::Sandbox;
use predicates::prelude::*;

const LIVE_SAMPLE: &str = "You are currently using your subscription to power your Claude Code usage\n\n\
    Current session: 9% used \u{b7} resets Sep 24 at 3:19am (Asia/Shanghai)\n\
    Current week (all models): 74% used \u{b7} resets Sep 29 at 11:59am (Asia/Shanghai)\n\
    Current week (Fable): 85% used \u{b7} resets Sep 29 at 11:59am (Asia/Shanghai)\n\n\
    What's contributing to your limits usage?\n...\n";

/// The live query's arguments (R10): no settings files (an empty list, as an argument of its
/// own), no MCP servers.
const LIVE_ARGS: [&str; 6] = [
    "-p",
    "/usage",
    "--no-session-persistence",
    "--setting-sources",
    "",
    "--strict-mcp-config",
];

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

/// A `.claude.json` whose usage cache was fetched `age_secs` ago.
fn cache_json(age_secs: u128, utilization: &str) -> String {
    format!(
        r#"{{"numStartups": 5, "oauthAccount": {{"emailAddress": "x@example.com"}},
  "cachedUsageUtilization": {{"fetchedAtMs": {}, "accountUuid": "00000000-0000-0000-0000-000000000000",
  "utilization": {utilization}}}}}"#,
        now_ms() - age_secs * 1000
    )
}

/// The resets are in 2099: ahead whenever this runs.
const LIMITS: &str = r#"{
  "five_hour": {"utilization": 34, "resets_at": "2099-09-23T15:40:00.292773+00:00"},
  "seven_day": {"utilization": 76, "resets_at": "2099-09-25T05:00:00.632368+00:00"},
  "seven_day_opus": null, "extra_usage": {"is_enabled": false},
  "limits": [
    {"kind": "session", "group": "session", "percent": 34, "severity": "normal",
     "resets_at": "2099-09-23T15:39:59.632347+00:00", "scope": null, "is_active": false},
    {"kind": "weekly_all", "group": "weekly", "percent": 77, "severity": "warning",
     "resets_at": "2099-09-25T04:59:59.632368+00:00", "scope": null, "is_active": false},
    {"kind": "weekly_scoped", "group": "weekly", "percent": 100, "severity": "critical",
     "resets_at": "2099-09-25T04:59:59.632532+00:00",
     "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null}, "is_active": true}]}"#;

struct Setup {
    sb: Sandbox,
    max: PathBuf,
    team: PathBuf,
}

fn setup() -> Setup {
    let sb = Sandbox::new();
    let max = sb.make_claude_home("profiles/max");
    let team = sb.make_claude_home("profiles/team");
    sb.register(&[("max", &max), ("team", &team)]);
    Setup { sb, max, team }
}

/// Output blocks keyed by the header's first word (the account); lines whitespace-normalized.
fn blocks(stdout: &str) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let norm = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.starts_with(' ') {
            out.last_mut().expect("row before header").1.push(norm);
        } else {
            out.push((norm, Vec::new()));
        }
    }
    out
}

fn stdout_of(assert: assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stdout.clone()).unwrap()
}

// --- cached ------------------------------------------------------------------------------

#[test]
fn cached_usage_for_every_account_without_running_claude() {
    let Setup { sb, max, .. } = setup();
    sb.write_claude_json(Some(&max), &cache_json(200, LIMITS));
    sb.write_claude_json(None, &cache_json(7200, LIMITS));
    // team's .claude.json is `{}`: no cache.
    let out = stdout_of(sb.remuda().arg("usage").assert().success().stderr(""));
    let b = blocks(&out);
    let headers: Vec<&str> = b.iter().map(|(h, _)| h.as_str()).collect();
    assert_eq!(headers.len(), 3, "{out}");
    assert!(
        headers[0].starts_with("claude:default cached 2h ago ("),
        "{out}"
    );
    assert!(
        headers[1].starts_with("claude:max cached 3m ago ("),
        "{out}"
    );
    assert!(
        headers[2].starts_with("claude:team no cached usage"),
        "{out}"
    );
    let rows = [
        "Session 34% resets Sep 23 15:39",
        "Week (all models) 77% ! resets Sep 25 04:59",
        "Week (Fable) 100% !! resets Sep 25 04:59",
    ];
    assert_eq!(b[0].1, rows);
    assert_eq!(b[1].1, rows);
    assert!(b[2].1.is_empty());
    assert!(
        sb.invocations().is_empty(),
        "cached usage must not run claude"
    );
}

/// R10: a window whose reset has passed since the cache was written shows no percentage (and no
/// severity): what it holds now is unknown. One whose reset is ahead reads as recorded.
#[test]
fn cached_usage_past_a_reset_says_so() {
    let Setup { sb, max, .. } = setup();
    let at = |offset_secs: i64| {
        jiff::Timestamp::from_second((now_ms() / 1000) as i64 + offset_secs).unwrap()
    };
    // Cached two hours ago: the session, then critical, reset an hour ago.
    let (session, week) = (at(-3600), at(2 * 86_400));
    let limits = format!(
        r#"{{"limits": [
            {{"kind": "session", "percent": 97, "severity": "critical", "resets_at": "{session}"}},
            {{"kind": "weekly_all", "percent": 77, "severity": "warning", "resets_at": "{week}"}}]}}"#
    );
    sb.write_claude_json(Some(&max), &cache_json(7200, &limits));
    let out = stdout_of(sb.remuda().args(["usage", "max"]).assert().success());
    let b = blocks(&out);
    assert!(b[0].0.starts_with("claude:max cached 2h ago ("), "{out}");
    let utc = |t: jiff::Timestamp| {
        t.to_zoned(jiff::tz::TimeZone::UTC)
            .strftime("%b %-d %H:%M")
            .to_string()
    };
    assert_eq!(
        b[0].1,
        [
            format!("Session - reset since cached ({})", utc(session)),
            format!("Week (all models) 77% ! resets {}", utc(week)),
        ],
        "{out}"
    );
}

#[test]
fn cached_usage_uses_the_given_time_zone() {
    let Setup { sb, max, .. } = setup();
    sb.write_claude_json(Some(&max), &cache_json(10, LIMITS));
    let out = stdout_of(
        sb.remuda()
            .env("TZ", "Asia/Shanghai")
            .args(["usage", "max"])
            .assert()
            .success(),
    );
    let b = blocks(&out);
    assert_eq!(b.len(), 1, "{out}");
    assert_eq!(b[0].1[0], "Session 34% resets Sep 23 23:39");
}

#[test]
fn cached_usage_degrades_on_odd_shapes() {
    let Setup { sb, max, team } = setup();
    let odd = r#"{"five_hour": {"utilization": 12, "resets_at": null},
                  "limits": [{"kind": "weekly_all", "percent": 40, "resets_at": null, "severity": "normal"},
                             {"kind": "brand_new", "percent": 5, "severity": "normal", "resets_at": null}]}"#;
    sb.write_claude_json(Some(&max), &cache_json(60, odd));
    std::fs::write(team.join(".claude.json"), "{ truncated").unwrap();
    // The native ~/.claude.json does not exist at all.
    let out = stdout_of(sb.remuda().arg("usage").assert().success());
    let b = blocks(&out);
    assert!(
        b[0].0.starts_with("claude:default no cached usage"),
        "{out}"
    );
    assert!(b[1].0.starts_with("claude:max cached 1m ago ("), "{out}");
    assert_eq!(b[1].1, ["Week (all models) 40%", "brand_new 5%"]);
    assert!(b[2].0.starts_with("claude:team no cached usage"), "{out}");
}

#[test]
fn usage_of_one_account_and_unknown_account() {
    let Setup { sb, max, team } = setup();
    sb.write_claude_json(Some(&max), &cache_json(10, LIMITS));
    sb.write_claude_json(Some(&team), &cache_json(10, LIMITS));
    let out = stdout_of(
        sb.remuda()
            .args(["usage", "claude:team"])
            .assert()
            .success(),
    );
    let b = blocks(&out);
    assert_eq!(b.len(), 1);
    assert!(b[0].0.starts_with("claude:team "), "{out}");
    sb.remuda().args(["usage", "nope"]).assert().code(1).stderr(
        predicate::str::starts_with("remuda: ").and(predicate::str::contains("claude:max")),
    );
}

// --- live --------------------------------------------------------------------------------

#[test]
fn live_usage_runs_claude_per_account_with_its_env() {
    let Setup { sb, max, team } = setup();
    sb.set_live_usage(None, LIVE_SAMPLE);
    sb.set_live_usage(Some(&max), "Current session: 0% used\n");
    sb.set_live_usage(Some(&team), LIVE_SAMPLE);
    let out = stdout_of(
        sb.remuda()
            .env("CLAUDE_CONFIG_DIR", sb.root().join("elsewhere"))
            .args(["usage", "--live"])
            .assert()
            .success()
            .stderr(""),
    );
    let b = blocks(&out);
    let headers: Vec<&str> = b.iter().map(|(h, _)| h.as_str()).collect();
    assert_eq!(
        headers,
        ["claude:default live", "claude:max live", "claude:team live"]
    );
    let sample_rows = [
        "Session 9% resets Sep 24 at 3:19am (Asia/Shanghai)",
        "Week (all models) 74% resets Sep 29 at 11:59am (Asia/Shanghai)",
        // Live rows carry no severity: marked from the percentage like the TUI (75 % / 90 %).
        "Week (Fable) 85% ! resets Sep 29 at 11:59am (Asia/Shanghai)",
    ];
    assert_eq!(b[0].1, sample_rows);
    assert_eq!(b[1].1, ["Session 0%"]);
    assert_eq!(b[2].1, sample_rows);

    assert_eq!(sb.invocations().len(), 3);
    for dir in [None, Some(max.as_path()), Some(team.as_path())] {
        let invs = sb.invocations_with(dir);
        assert_eq!(invs.len(), 1, "{dir:?}");
        // No settings files (so no hooks), no MCP servers: `""` is an argument of its own.
        assert_eq!(invs[0].args, LIVE_ARGS);
    }
    assert!(
        !sb.launch_log().exists(),
        "live usage must not log launches"
    );
    // What it read is recorded (R24), and nothing else is written.
    assert_eq!(state_entries(&sb), ["usage-history.jsonl"]);
}

/// R10: with `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` set, claude's `/usage` asks for no new
/// reading (the fake answers as claude 2.1.292 then does, without a `Current` line). The live
/// query runs without it and gets the account's rows; the other commands run for an account
/// (`auth status`) and launches keep it (R6).
#[test]
fn live_usage_runs_without_disable_nonessential_traffic() {
    const VAR: &str = "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC";
    let Setup { sb, max, .. } = setup();
    sb.set_live_usage(Some(&max), LIVE_SAMPLE);
    let out = stdout_of(
        sb.remuda()
            .env(VAR, "1")
            .args(["usage", "max", "--live"])
            .assert()
            .success(),
    );
    let b = blocks(&out);
    assert_eq!(b[0].0, "claude:max live", "{out}");
    assert_eq!(b[0].1.len(), 3, "{out}");
    assert!(b[0].1[0].starts_with("Session 9%"), "{out}");
    let invs = sb.invocations_with(Some(&max));
    assert_eq!(invs.len(), 1);
    assert_eq!(invs[0].args, LIVE_ARGS);
    assert_eq!(invs[0].nonessential_traffic, None);

    sb.set_auth(
        Some(&max),
        r#"{"loggedIn": true, "authMethod": "claude.ai"}"#,
    );
    sb.remuda().env(VAR, "1").arg("list").assert().success();
    sb.remuda()
        .env(VAR, "1")
        .args(["run", "max", "-p", "hi"])
        .assert()
        .success();
    let invs = sb.invocations_with(Some(&max));
    assert_eq!(invs.len(), 3);
    assert_eq!(invs[1].args, ["auth", "status", "--json"]);
    assert_eq!(invs[2].args[..2], ["-p", "hi"]);
    for inv in &invs[1..] {
        assert_eq!(inv.nonessential_traffic.as_deref(), Some("1"), "{inv:?}");
    }
}

#[test]
fn live_usage_prints_unparseable_output_raw() {
    let Setup { sb, max, .. } = setup();
    sb.set_live_usage(Some(&max), "Usage looks different now\n  Session: plenty\n");
    let out = stdout_of(
        sb.remuda()
            .args(["usage", "max", "--live"])
            .assert()
            .success(),
    );
    assert!(out.contains("\n    Usage looks different now\n"), "{out}");
    assert!(out.contains("\n      Session: plenty\n"), "{out}");
}

/// R10 (review #14): the week is used up and its line no longer reads `<N>% used`. The answer
/// is shown as it is, whole: the lines that still read would show the account as available.
#[test]
fn live_usage_read_in_part_is_shown_as_is() {
    let Setup { sb, max, .. } = setup();
    let drifted = LIVE_SAMPLE.replace(
        "Current week (all models): 74% used",
        "Current week (all models): limit reached",
    );
    sb.set_live_usage(Some(&max), &drifted);
    let out = stdout_of(
        sb.remuda()
            .args(["usage", "max", "--live"])
            .assert()
            .success(),
    );
    assert!(
        out.starts_with("claude:max  live (output not recognized; shown as is)\n"),
        "{out}"
    );
    for line in [
        "\n    Current session: 9% used \u{b7} resets Sep 24 at 3:19am (Asia/Shanghai)\n",
        "\n    Current week (all models): limit reached \u{b7} resets Sep 29 at 11:59am \
         (Asia/Shanghai)\n",
    ] {
        assert!(out.contains(line), "{line:?} in:\n{out}");
    }
    // Not the aligned rows of an answer that was read.
    assert!(!out.contains("  Session  "), "{out}");
}

/// R10: claude 2.1.292's answer when it could not get the account's limits: how the account is
/// billed, and no usage line. Its reason is said in one line, not the text claude printed.
#[test]
fn live_usage_that_tells_no_limits_says_why() {
    let Setup { sb, max, .. } = setup();
    sb.set_live_usage(
        Some(&max),
        "You are currently using your subscription to power your Claude Code usage\n\n\
         What's contributing to your limits usage?\n\
         Approximate, based on local sessions on this machine \u{2014} does not include other \
         devices or claude.ai. Behaviors are independent characteristics, not a breakdown.\n\n\
         Last 24h \u{b7} 3915 requests \u{b7} 29 sessions\n",
    );
    let out = stdout_of(
        sb.remuda()
            .args(["usage", "max", "--live"])
            .assert()
            .success(),
    );
    assert_eq!(out, "claude:max  live: no usage limits told for now\n");
}

/// R10: what claude 2.1.292 printed for `/usage` in an empty `CLAUDE_CONFIG_DIR` (exit 0): the
/// session's cost, which is what it prints for an account not on a Claude subscription.
#[test]
fn live_usage_of_an_account_not_logged_in_says_so() {
    let Setup { sb, max, .. } = setup();
    sb.set_live_usage(
        Some(&max),
        "Total cost:            $0.0000\n\
         Total duration (API):  0s\n\
         Total duration (wall): 0s\n\
         Total code changes:    0 lines added, 0 lines removed\n\
         Usage:                 0 input, 0 output, 0 cache read, 0 cache write\n",
    );
    let out = stdout_of(
        sb.remuda()
            .args(["usage", "max", "--live"])
            .assert()
            .success(),
    );
    assert_eq!(
        out,
        "claude:max  live: not logged in to a Claude subscription, or using an API key\n"
    );
}

/// R10: reset wording with a time out of range (here in the wordings with a comma and with a
/// year) is wording remuda cannot read: the percentage is shown with claude's wording, and the
/// command goes on to the end.
#[test]
fn live_usage_with_a_reset_time_out_of_range_is_shown_as_said() {
    let Setup { sb, max, .. } = setup();
    sb.set_live_usage(
        Some(&max),
        "Current session: 40% used \u{b7} resets Oct 9, 2:60pm (UTC)\n\
         Current week (all models): 50% used \u{b7} resets Jan 2, 2027 at 3:99pm (UTC)\n\
         Current week (Fable): 60% used \u{b7} resets Jan 2, 2027, 3:-1pm (UTC)\n",
    );
    let out = stdout_of(
        sb.remuda()
            .args(["usage", "max", "--live"])
            .assert()
            .success()
            .stderr(""),
    );
    let b = blocks(&out);
    assert_eq!(b.len(), 1, "{out}");
    assert_eq!(b[0].0, "claude:max live");
    assert_eq!(
        b[0].1,
        [
            "Session 40% resets Oct 9, 2:60pm (UTC)",
            "Week (all models) 50% resets Jan 2, 2027 at 3:99pm (UTC)",
            "Week (Fable) 60% resets Jan 2, 2027, 3:-1pm (UTC)",
        ]
    );
}

/// A cached `limits` entry that cannot be read spoils the cache (R10, review #14): the limits
/// that can be read are not shown as the account's usage.
#[test]
fn cached_usage_with_an_unreadable_limit_is_no_usage() {
    let Setup { sb, max, .. } = setup();
    let limits = r#"{"five_hour": {"utilization": 34}, "limits": [
        {"kind": "session", "percent": 34},
        {"kind": "weekly_all", "percent": "all of it"}]}"#;
    sb.write_claude_json(Some(&max), &cache_json(60, limits));
    let out = stdout_of(sb.remuda().args(["usage", "max"]).assert().success());
    assert_eq!(
        out,
        "claude:max  no cached usage (1 of 2 usage limits in the cache not recognized)\n"
    );
}

#[test]
fn live_usage_failure_is_reported_and_others_still_print() {
    let Setup { sb, max, .. } = setup();
    sb.set_live_usage(Some(&max), LIVE_SAMPLE);
    sb.set_live_usage(None, LIVE_SAMPLE);
    // team has no fixture: the fake exits 1.
    let out = stdout_of(sb.remuda().args(["usage", "--live"]).assert().code(1));
    let b = blocks(&out);
    assert_eq!(b[1].0, "claude:max live");
    assert_eq!(b[1].1.len(), 3);
    assert!(b[2].0.starts_with("claude:team error:"), "{out}");
    assert!(b[2].0.contains("exited with status 1"), "{out}");
}

#[test]
fn live_usage_timeout() {
    let Setup { sb, max, team } = setup();
    sb.set_hang(Some(&max), 30);
    sb.set_live_usage(Some(&team), LIVE_SAMPLE);
    sb.set_live_usage(None, LIVE_SAMPLE);
    let start = Instant::now();
    let out = stdout_of(
        sb.remuda()
            // Long enough for the other accounts' fake claude on a loaded machine (a 0.5 s
            // timeout flaked under a parallel `cargo test`), far below max's 30 s hang.
            .args(["usage", "--live", "--timeout", "3"])
            .assert()
            .code(1),
    );
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "took {:?}",
        start.elapsed()
    );
    let b = blocks(&out);
    assert!(
        b[1].0.starts_with("claude:max error:") && b[1].0.contains("timed out"),
        "{out}"
    );
    assert_eq!(b[2].1.len(), 3, "{out}");
}

#[test]
fn live_usage_needs_claude_on_path() {
    let Setup { sb, .. } = setup();
    sb.remuda()
        .env("PATH", "/usr/bin:/bin")
        .args(["usage", "--live"])
        .assert()
        .code(1)
        .stderr(predicate::str::starts_with("remuda: ").and(predicate::str::contains("claude")));
}

// --- with a terminal ---------------------------------------------------------------------

/// A pseudo-terminal, `(master, slave)`, that stops a background process which writes to it
/// (`TOSTOP`), as it stops one that reads it. The tests have no controlling terminal of their
/// own to rely on (CI has none), so one is made.
fn pty() -> (libc::c_int, libc::c_int) {
    let (mut master, mut slave) = (0, 0);
    // SAFETY: openpty(3) writes the two descriptors (the name, modes and size are not asked);
    // tcgetattr(3) and tcsetattr(3) read and write the zeroed `modes` of this function.
    unsafe {
        let made = libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        assert_eq!(made, 0, "openpty: {}", std::io::Error::last_os_error());
        let mut modes: libc::termios = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(slave, &mut modes), 0);
        modes.c_lflag |= libc::TOSTOP;
        assert_eq!(libc::tcsetattr(slave, libc::TCSANOW, &modes), 0);
    }
    (master, slave)
}

/// R4: a command remuda runs for its output is outside the terminal's foreground process
/// group, where a process that reads the terminal, or writes to one that asks for it, is
/// stopped until someone continues it: the query would hang until its timeout. remuda has the
/// command ignore those two signals, so the write goes through, the read fails at once, and
/// the query answers.
///
/// remuda runs here as the leader of a session of its own whose controlling terminal is a
/// pseudo-terminal, as under a real terminal: the fake claude's `/dev/tty` is that terminal.
/// The fake touches it with the shell's own `echo` and `read`: a shell may give the processes
/// it starts the default actions back (macOS's `/bin/sh` does), an agent does not.
#[test]
fn a_query_that_touches_the_terminal_is_not_stopped() {
    use std::os::unix::process::CommandExt;
    let Setup { sb, .. } = setup();
    common::write_executable(
        &sb.bin().join("claude"),
        "#!/bin/sh\n\
         echo hello > /dev/tty; echo \"write $?\" > \"$HOME/tty.write\"\n\
         read line < /dev/tty; echo \"read $?\" > \"$HOME/tty.read\"\n\
         echo 'Current session: 7% used'\n",
    );
    let (master, slave) = pty();
    let mut cmd = sb.remuda_process();
    cmd.args(["usage", "max", "--live", "--timeout", "5"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // SAFETY: the closure only calls setsid(2) and ioctl(2), both async-signal-safe: a new
    // session, with the pseudo-terminal as its controlling terminal.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() == -1 || libc::ioctl(slave, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let start = Instant::now();
    let remuda = cmd.spawn().unwrap();
    // SAFETY: the descriptor `pty` opened, closed once: remuda's session has the terminal now.
    unsafe { libc::close(slave) };
    // What is written to the terminal must be read: a session cannot end while its terminal
    // has output nobody took. Reading ends when the last process of the session is gone.
    let terminal = std::thread::spawn(move || {
        use std::io::Read;
        use std::os::fd::FromRawFd;
        // SAFETY: the descriptor `pty` opened; this file is its only owner.
        let mut master = unsafe { std::fs::File::from_raw_fd(master) };
        let mut shown = Vec::new();
        let _ = master.read_to_end(&mut shown);
        String::from_utf8_lossy(&shown).into_owned()
    });
    let out = remuda.wait_with_output().unwrap();
    let took = start.elapsed();
    let shown = terminal.join().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}{stderr}");
    let b = blocks(&stdout);
    assert_eq!(b[0].0, "claude:max live", "{stdout}");
    assert_eq!(b[0].1, ["Session 7%"], "{stdout}");
    assert!(took < Duration::from_secs(4), "took {took:?}: stopped");
    // It did reach the terminal: the write went through, and the read failed (EIO).
    let said = |name: &str| std::fs::read_to_string(sb.home().join(name)).unwrap();
    assert_eq!(said("tty.write"), "write 0\n");
    assert!(shown.contains("hello"), "{shown:?}");
    assert_ne!(said("tty.read"), "read 0\n");
}

// --- interrupted -------------------------------------------------------------------------

/// A fake agent that hangs with a process of its own, both in the agent's process group: the
/// agent waits for `<agent> inner`, which sleeps. Each writes its pid into `$HOME`.
const HANGING_AGENT: &str = "#!/bin/sh\n\
    if [ \"$1\" = inner ]; then echo $$ > \"$HOME/inner.pid\"; exec sleep 60; fi\n\
    echo $$ > \"$HOME/agent.pid\"\n\
    \"$0\" inner\n";

/// The pid a hanging agent wrote to `path`, once it is there.
fn pid_once_written(path: &std::path::Path) -> libc::pid_t {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        match std::fs::read_to_string(path).map(|text| text.trim().parse()) {
            Ok(Ok(pid)) => return pid,
            _ if Instant::now() < until => std::thread::sleep(Duration::from_millis(10)),
            other => panic!("no pid in {}: {other:?}", path.display()),
        }
    }
}

/// Whether process `pid` is gone, waiting up to 10 s for it to be.
fn gone(pid: libc::pid_t) -> bool {
    let until = Instant::now() + Duration::from_secs(10);
    // SAFETY: kill(2) with signal 0 only checks that the process exists.
    while unsafe { libc::kill(pid, 0) } == 0 {
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

/// `remuda <args>` with the hanging agent as `program`, sent `signal` once the agent runs:
/// remuda's own exit signal, and whether the agent and its process are gone. Only remuda is
/// signalled, as by a terminal whose foreground process group the agent has left, or by
/// `kill` (SIGTERM).
fn interrupted(
    sb: &Sandbox,
    program: &str,
    args: &[&str],
    signal: libc::c_int,
) -> (Option<i32>, bool, bool) {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    common::write_executable(&sb.bin().join(program), HANGING_AGENT);
    let mut cmd = sb.remuda_process();
    cmd.args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: the closure only calls `signal`, which is async-signal-safe. Whatever started
    // the tests may have left SIGINT or SIGTERM ignored, which remuda would then leave alone too.
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            Ok(())
        });
    }
    let mut remuda = cmd.spawn().unwrap();
    let agent = pid_once_written(&sb.home().join("agent.pid"));
    let inner = pid_once_written(&sb.home().join("inner.pid"));
    // SAFETY: kill(2) takes no pointers.
    assert_eq!(unsafe { libc::kill(remuda.id() as libc::pid_t, signal) }, 0);
    let until = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match remuda.try_wait().unwrap() {
            Some(status) => break status,
            None if Instant::now() < until => std::thread::sleep(Duration::from_millis(10)),
            None => {
                let _ = remuda.kill();
                panic!("remuda outlived the interrupt");
            }
        }
    };
    let gone = (gone(agent), gone(inner));
    for pid in [agent, inner] {
        // SAFETY: as above; nothing is left running when the assertion fails.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    (status.signal(), gone.0, gone.1)
}

/// R4: an agent command runs outside the terminal's foreground process group, so Ctrl-C does
/// not reach it by itself: remuda passes the signal on to the command's group, and then ends
/// by it as before.
#[test]
fn an_interrupt_reaches_a_running_claude_query() {
    let Setup { sb, .. } = setup();
    let got = interrupted(
        &sb,
        "claude",
        &["usage", "max", "--live", "--timeout", "60"],
        libc::SIGINT,
    );
    assert_eq!(got, (Some(libc::SIGINT), true, true));
}

/// R4: the same for `codex app-server`, which never was in the foreground process group.
#[test]
fn an_interrupt_reaches_a_running_codex_query() {
    let sb = Sandbox::new();
    let got = interrupted(
        &sb,
        "codex",
        &["usage", "codex:default", "--live", "--timeout", "60"],
        libc::SIGINT,
    );
    assert_eq!(got, (Some(libc::SIGINT), true, true));
}

/// One `remuda <args>` (`usage --live`, or a command that waits with `--live`) over 60 claude
/// and 60 codex accounts (and the two `default` ones), each query a fake agent that hangs, sent
/// `signal` `after` remuda was started: whether any process remuda had started outlived it.
///
/// remuda is given the writing end of a pipe, which every process it starts inherits: the
/// reading end sees the end of the pipe when the last of them is gone, and not before. That
/// counts a query from its `fork` on, before it could say anything itself.
fn outlived_by_a_query(after: Duration, signal: libc::c_int, args: &[&str]) -> bool {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    /// Per provider: enough that starting them all takes a while.
    const ACCOUNTS: usize = 60;
    let sb = Sandbox::new();
    let mut config = String::new();
    for i in 0..ACCOUNTS {
        for (provider, home) in [
            ("claude", sb.make_claude_home(&format!("h/a{i}"))),
            ("codex", sb.make_codex_home(&format!("c/a{i}"))),
        ] {
            config.push_str(&format!(
                "[[account]]\nprovider = \"{provider}\"\nname = \"a{i}\"\nhome = \"{}\"\n\n",
                home.display()
            ));
        }
    }
    sb.write_config(&config);
    let pids = sb.home().join("pids");
    std::fs::create_dir(&pids).unwrap();
    for program in ["claude", "codex"] {
        // One process, the shell become `sleep`: a shell that is forking as the signal comes
        // loses its child to it, here as under a terminal, and that is not remuda's doing.
        // The pid is for cleaning up after a failure only.
        let agent = "#!/bin/sh\necho $$ > \"$HOME/pids/$$\"\nexec sleep 20\n";
        common::write_executable(&sb.bin().join(program), agent);
    }
    let (mut alive, held) = std::io::pipe().unwrap();
    let held_fd = held.as_raw_fd();
    let mut cmd = sb.remuda_process();
    cmd.args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: the closure only calls `signal` and `fcntl`, both async-signal-safe: SIGINT and
    // SIGTERM have their default actions (see `interrupted`), and the pipe's writing end, closed
    // on exec in every other process the tests start, stays open in remuda and what it starts.
    unsafe {
        cmd.pre_exec(move || {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            if libc::fcntl(held_fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut remuda = cmd.spawn().unwrap();
    drop(held);
    std::thread::sleep(after);
    // SAFETY: kill(2) takes no pointers.
    assert_eq!(unsafe { libc::kill(remuda.id() as libc::pid_t, signal) }, 0);
    let until = Instant::now() + Duration::from_secs(20);
    let status = loop {
        match remuda.try_wait().unwrap() {
            Some(status) => break status,
            None if Instant::now() < until => std::thread::sleep(Duration::from_millis(5)),
            None => {
                let _ = remuda.kill();
                panic!("remuda outlived the interrupt");
            }
        }
    };
    assert_eq!(status.signal(), Some(signal), "{args:?}: {status:?}");
    // remuda is gone and starts nothing more. What it told goes within moments; what it did
    // not tell sleeps on.
    let (tx, ended) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = alive.read(&mut [0]);
        let _ = tx.send(());
    });
    let outlived = ended.recv_timeout(Duration::from_secs(5)).is_err();
    if outlived {
        for entry in std::fs::read_dir(&pids).unwrap() {
            let pid = entry.unwrap().file_name().to_str().unwrap().parse::<i32>();
            // SAFETY: as above; nothing is left running when the assertion fails.
            unsafe { libc::kill(pid.unwrap(), libc::SIGKILL) };
        }
    }
    outlived
}

/// R4 (review round 1): Ctrl-C while queries are being started, in parallel, reaches every
/// one of them: the ones already running, and the ones remuda had started and not yet come
/// to watch. Nothing new is started, and remuda ends by the signal once the last start is
/// over. The interrupt comes at a range of moments after remuda's own start, so that some
/// fall among the starts whatever the machine's speed; one that comes before or after them
/// proves nothing and costs little.
#[test]
fn an_interrupt_while_queries_are_starting_reaches_them_all() {
    for after in (0..120).step_by(8).map(Duration::from_millis) {
        assert!(
            !outlived_by_a_query(after, libc::SIGINT, &["usage", "--live", "--timeout", "60"]),
            "interrupted after {after:?}: a query outlived remuda"
        );
    }
}

// --- history (R24) and the reset note (R10) -------------------------------------------------

/// The names in `state/`, sorted.
fn state_entries(sb: &Sandbox) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(sb.remuda_home().join("state"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// The lines of the usage history, as JSON.
fn history(sb: &Sandbox) -> Vec<serde_json::Value> {
    let path = sb.remuda_home().join("state/usage-history.jsonl");
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Now, to the minute: times written into fixtures print as they are.
fn minute() -> jiff::Timestamp {
    let s = (now_ms() / 1000) as i64;
    jiff::Timestamp::from_second(s - s % 60).unwrap()
}

fn plus(t: jiff::Timestamp, secs: i64) -> jiff::Timestamp {
    jiff::Timestamp::from_second(t.as_second() + secs).unwrap()
}

/// `Oct 8 12:59` in UTC, as `remuda usage` prints a time in the sandbox.
fn utc(t: jiff::Timestamp) -> String {
    t.to_zoned(jiff::tz::TimeZone::UTC)
        .strftime("%b %-d %H:%M")
        .to_string()
}

/// A `.claude.json` whose usage cache was fetched at `fetched_ms`.
fn cache_at(fetched_ms: i64, utilization: &str) -> String {
    format!(
        r#"{{"oauthAccount": {{"emailAddress": "x@example.com"}},
  "cachedUsageUtilization": {{"fetchedAtMs": {fetched_ms}, "utilization": {utilization}}}}}"#
    )
}

/// R24: `remuda usage` records each window it read, at the time the cache was written (not
/// when it ran): the same cache read again adds nothing. `state/` holds the history alone, the
/// user's (0600).
#[test]
fn cached_usage_is_recorded_at_the_cache_time() {
    use std::os::unix::fs::PermissionsExt;
    let Setup { sb, max, .. } = setup();
    let fetched = now_ms() as i64 - 200_000;
    sb.write_claude_json(Some(&max), &cache_at(fetched, LIMITS));
    let ts = jiff::Timestamp::from_millisecond(fetched)
        .unwrap()
        .to_string();
    for _ in 0..2 {
        sb.remuda().arg("usage").assert().success().stderr("");
    }
    assert_eq!(
        history(&sb),
        [
            serde_json::json!({"ts": ts, "account": "claude:max", "label": "Session",
                "model": null, "percent": 34.0, "resets_at": "2099-09-23T15:39:59.632347Z",
                "source": "cached"}),
            serde_json::json!({"ts": ts, "account": "claude:max", "label": "Week (all models)",
                "model": null, "percent": 77.0, "resets_at": "2099-09-25T04:59:59.632368Z",
                "source": "cached"}),
            serde_json::json!({"ts": ts, "account": "claude:max", "label": "Week (Fable)",
                "model": "Fable", "percent": 100.0, "resets_at": "2099-09-25T04:59:59.632532Z",
                "source": "cached"}),
        ]
    );
    assert_eq!(state_entries(&sb), ["usage-history.jsonl"]);
    let mode = std::fs::metadata(sb.remuda_home().join("state/usage-history.jsonl"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    // A new cache is a new reading; the windows that reset since it was written are not.
    let session_passed = format!(
        r#"{{"limits": [{{"kind": "session", "percent": 50, "resets_at": "{}"}},
            {{"kind": "weekly_all", "percent": 78, "resets_at": "2099-09-25T04:59:59Z"}}]}}"#,
        plus(minute(), -60)
    );
    sb.write_claude_json(Some(&max), &cache_at(fetched + 1000, &session_passed));
    sb.remuda().args(["usage", "max"]).assert().success();
    let got = history(&sb);
    assert_eq!(got.len(), 4, "{got:?}");
    assert_eq!(got[3]["label"], "Week (all models)");
    assert_eq!(got[3]["percent"], 78.0);
}

/// R24: a live answer is recorded at the time it arrived; a failure or an answer that tells
/// no usage is not.
#[test]
fn live_usage_is_recorded_when_it_answers() {
    let Setup { sb, max, team } = setup();
    sb.set_live_usage(Some(&max), LIVE_SAMPLE);
    sb.set_live_usage(
        Some(&team),
        "You are currently using your subscription to power your Claude Code usage\n",
    );
    let before = jiff::Timestamp::now();
    // The default account has no fixture: its query fails (exit 1).
    sb.remuda().args(["usage", "--live"]).assert().code(1);
    let after = jiff::Timestamp::now();
    let got = history(&sb);
    let labels: Vec<(&str, &str)> = got
        .iter()
        .map(|p| (p["account"].as_str().unwrap(), p["label"].as_str().unwrap()))
        .collect();
    // Max's three windows; neither the default's failure nor team's answer without usage.
    assert_eq!(
        labels,
        [
            ("claude:max", "Session"),
            ("claude:max", "Week (all models)"),
            ("claude:max", "Week (Fable)"),
        ]
    );
    for p in &got {
        assert_eq!(p["source"], "live");
        let ts: jiff::Timestamp = p["ts"].as_str().unwrap().parse().unwrap();
        assert!(before <= ts && ts <= after, "{p}");
    }
    assert_eq!(got[2]["model"], "Fable");
    // Claude's wording, read as an instant ahead of the answer.
    for p in &got {
        let reset: jiff::Timestamp = p["resets_at"].as_str().unwrap().parse().unwrap();
        assert!(reset > before, "{p}");
    }
    assert!(!sb.launch_log().exists());
}

/// R24: `--history` lists each window of an account in time order: the current one point by
/// point, with its pace from the latest point; the windows before a line each; nothing older
/// than `--days`. It records nothing itself.
#[test]
fn history_shows_the_current_window_and_its_pace() {
    let Setup { sb, max, .. } = setup();
    let now = minute();
    let h = |n: i64| plus(now, n * 3600);
    let line = |at: jiff::Timestamp, label: &str, percent: f64, reset: Option<jiff::Timestamp>| {
        format!(
            "{}\n",
            serde_json::json!({"ts": at.to_string(), "account": "claude:max", "label": label,
                "model": null, "percent": percent, "resets_at": reset.map(|r| r.to_string()),
                "source": "cached"})
        )
    };
    let state = sb.remuda_home().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let text = [
        line(h(-240), "Session", 5.0, Some(h(-239))),
        line(h(-8), "Session", 40.0, Some(h(-6))),
        line(h(-7), "Session", 88.0, Some(h(-6))),
        line(h(-2), "Session", 12.0, Some(h(2))),
        line(h(-1), "Session", 30.0, Some(h(2))),
    ]
    .concat();
    std::fs::write(state.join("usage-history.jsonl"), &text).unwrap();
    // A cache that `--history` does not record.
    sb.write_claude_json(Some(&max), &cache_json(60, LIMITS));

    let out = stdout_of(
        sb.remuda()
            .args(["usage", "--history"])
            .assert()
            .success()
            .stderr(""),
    );
    assert_eq!(
        out,
        format!(
            "claude:default  no usage history in the last 7 days\n\n\
             claude:max\n  Session\n    window ended {}: peaked 88%\n    \
             {}  12%  resets {}\n    {}  30%  resets {}\n    \
             used 30% with 40% of the window elapsed: behind an even pace; \
             at this pace 75% at reset\n\n\
             claude:team  no usage history in the last 7 days\n",
            utc(h(-6)),
            utc(h(-2)),
            utc(h(2)),
            utc(h(-1)),
            utc(h(2)),
        )
    );
    let out = stdout_of(
        sb.remuda()
            .args(["usage", "--history", "max", "--days", "30"])
            .assert()
            .success(),
    );
    assert!(
        out.starts_with(&format!(
            "claude:max\n  Session\n    window ended {}: peaked 5%\n",
            utc(h(-239))
        )),
        "{out}"
    );
    assert_eq!(
        std::fs::read_to_string(state.join("usage-history.jsonl")).unwrap(),
        text,
        "--history records nothing"
    );
}

/// R24: no history yet: a line, exit 0, nothing created. `--history` does not combine with
/// `--live`, and `--days` needs it.
#[test]
fn history_without_a_history_and_its_options() {
    let Setup { sb, .. } = setup();
    let out = stdout_of(sb.remuda().args(["usage", "--history"]).assert().success());
    assert!(out.starts_with("no usage history yet ("), "{out}");
    assert_eq!(out.lines().count(), 1, "{out}");
    assert!(!sb.remuda_home().join("state").exists());
    sb.remuda()
        .args(["usage", "--history", "--live"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("cannot be used with"));
    sb.remuda()
        .args(["usage", "--days", "3"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--history"));
    sb.remuda()
        .args(["usage", "--history", "--days", "0"])
        .assert()
        .code(2);
    sb.remuda()
        .args(["usage", "--history", "nobody"])
        .assert()
        .code(1);
}

/// R24: a history that exists but cannot be read is an error for `--history`.
#[test]
fn history_that_cannot_be_read_is_an_error() {
    use std::os::unix::fs::PermissionsExt;
    let Setup { sb, .. } = setup();
    let state = sb.remuda_home().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let path = state.join("usage-history.jsonl");
    std::fs::write(&path, "").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    sb.remuda()
        .args(["usage", "--history"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("cannot read"));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// R10: a window that resets within the hour with at least 25% left gets a note under it, in
/// cached and in live output.
#[test]
fn a_reset_within_the_hour_with_much_left_gets_a_note() {
    let Setup { sb, max, team } = setup();
    let soon = plus(minute(), 30 * 60 + 60);
    let limits = format!(
        r#"{{"limits": [{{"kind": "session", "percent": 34, "resets_at": "{soon}"}},
            {{"kind": "weekly_all", "percent": 77, "resets_at": "2099-09-25T04:59:59Z"}}]}}"#
    );
    sb.write_claude_json(Some(&max), &cache_json(60, &limits));
    let out = stdout_of(sb.remuda().args(["usage", "max"]).assert().success());
    let b = blocks(&out);
    assert_eq!(b[0].1.len(), 3, "{out}");
    assert!(b[0].1[0].starts_with("Session 34% resets "), "{out}");
    // 30 or 31 minutes, as the clock turns between the fixture and the run.
    let note = &b[0].1[1];
    assert!(
        note == "note: resets in 31 min with 66% left"
            || note == "note: resets in 30 min with 66% left",
        "{out}"
    );
    assert!(b[0].1[2].starts_with("Week (all models) 77%"), "{out}");
    assert!(out.contains("\n  note: resets in "), "{out}");

    // Live: claude's wording read as an instant 20-odd minutes ahead, in UTC.
    let at = plus(minute(), 25 * 60);
    let wording = at
        .to_zoned(jiff::tz::TimeZone::UTC)
        .strftime("%b %-d at %-I:%M%P (UTC)")
        .to_string();
    sb.set_live_usage(
        Some(&team),
        &format!("Current session: 10% used \u{b7} resets {wording}\n"),
    );
    let out = stdout_of(
        sb.remuda()
            .args(["usage", "--live", "team"])
            .assert()
            .success(),
    );
    assert!(out.contains("with 90% left"), "{out}");
}

/// R24, R3: a `$REMUDA_HOME` remuda cannot write in: `remuda usage` prints as always, exits 0,
/// says nothing of the history, and creates nothing.
#[test]
fn usage_without_a_writable_state_still_succeeds() {
    use std::os::unix::fs::PermissionsExt;
    let Setup { sb, max, .. } = setup();
    sb.write_claude_json(Some(&max), &cache_json(60, LIMITS));
    let home = sb.remuda_home();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o500)).unwrap();
    let out = stdout_of(sb.remuda().arg("usage").assert().success().stderr(""));
    assert!(out.contains("claude:max  cached"), "{out}");
    assert!(!home.join("state").exists());
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    // A `state` that is a file: the same.
    std::fs::write(home.join("state"), "").unwrap();
    sb.remuda().arg("usage").assert().success().stderr("");
    assert_eq!(std::fs::read_to_string(home.join("state")).unwrap(), "");
}

/// R24, R3: `remuda usage`s at the same time lose no point. Each compacts the history: the
/// first to take the lock drops the old points and replaces the file, while the others append;
/// without the lock of `state/` over the reading and the writing, a point appended between a
/// compaction's reading and its rename would go to the file being replaced.
#[test]
fn concurrent_usage_runs_lose_no_point() {
    const ACCOUNTS: usize = 16;
    const ROUNDS: usize = 6;
    const OLD: usize = 3000;
    let sb = Sandbox::new();
    let homes: Vec<PathBuf> = (0..ACCOUNTS)
        .map(|n| sb.make_claude_home(&format!("profiles/a{n}")))
        .collect();
    let names: Vec<String> = (0..ACCOUNTS).map(|n| format!("a{n}")).collect();
    let pairs: Vec<(&str, &std::path::Path)> = names
        .iter()
        .zip(&homes)
        .map(|(n, h)| (n.as_str(), h.as_path()))
        .collect();
    sb.register(&pairs);
    let state = sb.remuda_home().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let path = state.join("usage-history.jsonl");
    let old_line = format!(
        "{}\n",
        serde_json::json!({"ts": "2020-01-01T00:00:00Z", "account": "claude:gone",
            "label": "Session", "model": null, "percent": 1.0, "resets_at": null,
            "source": "cached"})
    );
    let start = now_ms() as i64 - 600_000;
    for round in 0..ROUNDS {
        // Points too old to keep, for the round's first compaction to drop.
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        text.push_str(&old_line.repeat(OLD));
        std::fs::write(&path, text).unwrap();
        for home in &homes {
            let limits = r#"{"limits": [{"kind": "session", "percent": 5,
                "resets_at": "2099-01-01T00:00:00Z"}]}"#;
            sb.write_claude_json(Some(home), &cache_at(start + round as i64 * 1000, limits));
        }
        let barrier = std::sync::Barrier::new(ACCOUNTS);
        std::thread::scope(|scope| {
            for name in &names {
                let (sb, barrier) = (&sb, &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    sb.remuda()
                        .args(["usage", name])
                        .assert()
                        .success()
                        .stderr("");
                });
            }
        });
        let got = history(&sb);
        assert!(
            got.iter().all(|p| p["account"] != "claude:gone"),
            "round {round}: old points kept"
        );
        for name in &names {
            let account = format!("claude:{name}");
            let mine = got.iter().filter(|p| p["account"] == account).count();
            assert_eq!(mine, round + 1, "round {round}: {account} lost a point");
        }
    }
    assert_eq!(
        state_entries(&sb),
        ["usage-history.jsonl"],
        "no temporary file"
    );
}

/// R24, R3: two `$REMUDA_HOME`s whose `state/usage-history.jsonl` are symlinks to one file
/// share its lock (that of the directory of the file replaced): `remuda usage`s through both at
/// the same time, each compacting, lose no point. With a lock on each state directory, each
/// compaction replaced the shared file with what it alone had read.
#[test]
fn usage_through_histories_linked_to_one_file_loses_no_point() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    const ACCOUNTS: usize = 12;
    const ROUNDS: usize = 5;
    const OLD: usize = 2000;
    let sb = Sandbox::new();
    let homes: Vec<PathBuf> = (0..ACCOUNTS)
        .map(|n| sb.make_claude_home(&format!("profiles/a{n}")))
        .collect();
    let names: Vec<String> = (0..ACCOUNTS).map(|n| format!("a{n}")).collect();
    let pairs: Vec<(&str, &std::path::Path)> = names
        .iter()
        .zip(&homes)
        .map(|(n, h)| (n.as_str(), h.as_path()))
        .collect();
    sb.register(&pairs);
    // A second `$REMUDA_HOME` with the same accounts.
    let second = sb.root().join("second-remuda");
    std::fs::create_dir_all(second.join("state")).unwrap();
    std::fs::copy(sb.config_path(), second.join("config.toml")).unwrap();
    let shared = sb.root().join("shared/usage-history.jsonl");
    std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
    std::fs::write(&shared, "").unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o600)).unwrap();
    for home in [sb.remuda_home(), second.clone()] {
        std::fs::create_dir_all(home.join("state")).unwrap();
        symlink(&shared, home.join("state/usage-history.jsonl")).unwrap();
    }
    let old_line = format!(
        "{}\n",
        serde_json::json!({"ts": "2020-01-01T00:00:00Z", "account": "claude:gone",
            "label": "Session", "model": null, "percent": 1.0, "resets_at": null,
            "source": "cached"})
    );
    let start = now_ms() as i64 - 600_000;
    let read = || -> Vec<serde_json::Value> {
        std::fs::read_to_string(&shared)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };
    for round in 0..ROUNDS {
        let mut text = std::fs::read_to_string(&shared).unwrap();
        text.push_str(&old_line.repeat(OLD));
        std::fs::write(&shared, text).unwrap();
        for home in &homes {
            let limits = r#"{"limits": [{"kind": "session", "percent": 5,
                "resets_at": "2099-01-01T00:00:00Z"}]}"#;
            sb.write_claude_json(Some(home), &cache_at(start + round as i64 * 1000, limits));
        }
        let barrier = std::sync::Barrier::new(ACCOUNTS);
        std::thread::scope(|scope| {
            for (n, name) in names.iter().enumerate() {
                // Half the accounts through each `$REMUDA_HOME`.
                let remuda_home = if n % 2 == 0 {
                    sb.remuda_home()
                } else {
                    second.clone()
                };
                let (sb, barrier) = (&sb, &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    sb.remuda()
                        .env("REMUDA_HOME", &remuda_home)
                        .args(["usage", name])
                        .assert()
                        .success()
                        .stderr("");
                });
            }
        });
        let got = read();
        assert!(
            got.iter().all(|p| p["account"] != "claude:gone"),
            "round {round}: old points kept"
        );
        for name in &names {
            let account = format!("claude:{name}");
            let mine = got.iter().filter(|p| p["account"] == account).count();
            assert_eq!(mine, round + 1, "round {round}: {account} lost a point");
        }
    }
    for home in [sb.remuda_home(), second] {
        let link = home.join("state/usage-history.jsonl");
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
    }
    let mode = std::fs::metadata(&shared).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

/// R24, R10: a limit of a kind remuda does not know is shown and recorded under its own name,
/// whatever that is; `--history` lists it without a pace (its length is unknown) and does not
/// fail on it.
#[test]
fn history_of_windows_of_unknown_length() {
    let Setup { sb, max, .. } = setup();
    let reset = plus(minute(), 3600);
    let limits = format!(
        r#"{{"limits": [
            {{"kind": "额度 window", "percent": 30, "resets_at": "{reset}"}},
            {{"kind": "153722867280912931m window", "percent": 40, "resets_at": "{reset}"}}]}}"#
    );
    sb.write_claude_json(Some(&max), &cache_json(60, &limits));
    sb.remuda().args(["usage", "max"]).assert().success();
    assert_eq!(history(&sb).len(), 2);
    let out = stdout_of(
        sb.remuda()
            .args(["usage", "--history", "max"])
            .assert()
            .success()
            .stderr(""),
    );
    assert!(out.contains("\n  额度 window\n"), "{out}");
    assert!(out.contains("\n  153722867280912931m window\n"), "{out}");
    assert!(!out.contains("elapsed"), "{out}");
}

/// R24, R10: a live answer is recorded at the time it arrived, not when the command started:
/// the query is held until after a time taken once it has started.
#[test]
fn live_usage_is_recorded_at_the_answer_not_the_start() {
    let Setup { sb, max, .. } = setup();
    sb.set_live_usage(Some(&max), LIVE_SAMPLE);
    let gate = sb.root().join("gate");
    std::fs::create_dir_all(&gate).unwrap();
    let (started, open) = (gate.join("started"), gate.join("open"));
    common::write_executable(
        &gate.join("claude"),
        &format!(
            "#!/bin/sh\n: > '{}'\nwhile [ ! -f '{}' ]; do sleep 0.05; done\nexec '{}' \"$@\"\n",
            started.display(),
            open.display(),
            sb.bin().join("claude").display()
        ),
    );
    let mut remuda = sb.remuda_process();
    remuda
        .env("PATH", format!("{}:{}", gate.display(), sb.path_var()))
        .args(["usage", "--live", "max"])
        .stdout(std::process::Stdio::null());
    let mut child = remuda.spawn().unwrap();
    let waited = Instant::now();
    while !started.exists() {
        assert!(
            waited.elapsed() < Duration::from_secs(20),
            "claude never ran"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(1100));
    let bound = jiff::Timestamp::now();
    std::thread::sleep(Duration::from_millis(100));
    std::fs::write(&open, "").unwrap();
    assert!(child.wait().unwrap().success());
    let got = history(&sb);
    assert_eq!(got.len(), 3, "{got:?}");
    for p in &got {
        let ts: jiff::Timestamp = p["ts"].as_str().unwrap().parse().unwrap();
        assert!(
            ts > bound,
            "recorded at {ts}, before the answer could arrive ({bound})"
        );
    }
}

/// R4, R10 (lane review round 1): SIGTERM, which `kill` or a supervisor sends remuda alone, is
/// passed on like Ctrl-C: the live query that `usage --wait --live` has under way (in a process
/// group of its own, which no timeout of remuda's ends once remuda is gone) ends with remuda.
#[test]
fn a_sigterm_reaches_the_query_of_a_live_usage_wait() {
    let Setup { sb, .. } = setup();
    let got = interrupted(
        &sb,
        "claude",
        &["usage", "max", "--wait", "--live", "--timeout", "60"],
        libc::SIGTERM,
    );
    assert_eq!(got, (Some(libc::SIGTERM), true, true));
    let sb = Sandbox::new();
    let got = interrupted(
        &sb,
        "codex",
        &[
            "usage",
            "codex:default",
            "--wait",
            "--live",
            "--timeout",
            "60",
        ],
        libc::SIGTERM,
    );
    assert_eq!(got, (Some(libc::SIGTERM), true, true));
}

/// R4, R23 (lane review round 1): the same for the queries of `pick --wait --live` (and of
/// `usage --live`), started in parallel: SIGTERM among the starts reaches every one of them,
/// the ones running and the ones being started, and so it does once they all run.
#[test]
fn a_sigterm_while_queries_are_starting_reaches_them_all() {
    for args in [
        &["pick", "--wait", "--live", "--timeout", "60"][..],
        &["usage", "--live", "--timeout", "60"],
    ] {
        // Among the starts, and once they all run.
        let moments = (0..120)
            .step_by(12)
            .chain([1000])
            .map(Duration::from_millis);
        for after in moments {
            assert!(
                !outlived_by_a_query(after, libc::SIGTERM, args),
                "{args:?}: terminated after {after:?}: a query outlived remuda"
            );
        }
    }
}

// --- wait --------------------------------------------------------------------------------

/// `remuda <args>`: exit code, stdout, stderr, and how long it took.
fn timed(sb: &Sandbox, args: &[&str]) -> (Option<i32>, String, String, Duration) {
    let start = Instant::now();
    let out = sb.remuda().args(args).output().unwrap();
    (
        out.status.code(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
        start.elapsed(),
    )
}

/// R10 `usage --wait`: it waits for one account; without one it is a usage error that points
/// to `pick --wait`. `--max-wait` needs `--wait`.
#[test]
fn usage_wait_needs_an_account() {
    let Setup { sb, .. } = setup();
    let (code, stdout, stderr, _) = timed(&sb, &["usage", "--wait"]);
    assert_eq!(code, Some(2), "{stderr}");
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("to wait for any account use `remuda pick --wait`"),
        "{stderr}"
    );
    for args in [
        &["usage", "max", "--max-wait", "5"][..],
        &["usage", "max", "--wait", "--max-wait", "-1"],
    ] {
        let (code, _, stderr, _) = timed(&sb, args);
        assert_eq!(code, Some(2), "{args:?}: {stderr}");
    }
    assert!(sb.invocations().is_empty());
}

/// R10 `usage --wait`: an account with no window used up (less than `[pick] min_headroom`
/// percent left) is done at once, printed as `usage` prints it: also when the only window that
/// was used up has reset since it was cached (unknown, never used up; no `--live` is implied),
/// and when there is no cached usage at all.
#[test]
fn usage_wait_is_done_at_once_when_nothing_is_used_up() {
    let Setup { sb, max, .. } = setup();
    let at = |offset_secs: i64| {
        jiff::Timestamp::from_second((now_ms() / 1000) as i64 + offset_secs).unwrap()
    };
    let (session, week) = (at(-3600), at(2 * 86_400));
    let limits = format!(
        r#"{{"limits": [
            {{"kind": "session", "percent": 100, "severity": "critical", "resets_at": "{session}"}},
            {{"kind": "weekly_all", "percent": 77, "severity": "warning", "resets_at": "{week}"}}]}}"#
    );
    sb.write_claude_json(Some(&max), &cache_json(7200, &limits));
    for account in ["max", "team"] {
        let plain = stdout_of(sb.remuda().args(["usage", account]).assert().success());
        let (code, stdout, stderr, took) = timed(&sb, &["usage", account, "--wait"]);
        assert_eq!(code, Some(0), "{account}: {stderr}");
        assert_eq!(stdout, plain);
        assert_eq!(stderr, "", "{account}: no status line on a pipe");
        assert!(took < Duration::from_secs(30), "{account}: {took:?}");
    }
    let max_out = stdout_of(sb.remuda().args(["usage", "max"]).assert().success());
    assert!(max_out.contains("reset since cached"), "{max_out}");
    assert!(sb.invocations().is_empty(), "--wait never queries live");
}

/// R10 `usage --wait --max-wait`: a window used up whose reset is later than the deadline:
/// remuda gives up at once (no check would come before it) with the account's usage, exit 1.
/// `--max-wait 0` is one look.
#[test]
fn usage_wait_gives_up_when_the_next_check_is_after_max_wait() {
    let Setup { sb, max, .. } = setup();
    // The Fable window is at 100% until 2099.
    sb.write_claude_json(Some(&max), &cache_json(200, LIMITS));
    let plain = stdout_of(sb.remuda().args(["usage", "max"]).assert().success());
    for max_wait in ["0", "1", "3600"] {
        let (code, stdout, stderr, took) =
            timed(&sb, &["usage", "max", "--wait", "--max-wait", max_wait]);
        assert_eq!(code, Some(1), "{max_wait}: {stderr}");
        assert_eq!(stdout, plain);
        assert!(
            stderr.starts_with(
                "remuda: gave up waiting: the next check (Sep 25 05:00) would \
                 come after --max-wait"
            ),
            "{max_wait}: {stderr}"
        );
        assert!(took < Duration::from_secs(30), "{max_wait}: {took:?}");
    }
    // A `min_headroom` of 0 leaves nothing used up: done.
    sb.write_config(&format!("{}[pick]\nmin_headroom = 0\n", sb.read_config()));
    let (code, stdout, _, _) = timed(&sb, &["usage", "max", "--wait", "--max-wait", "0"]);
    assert_eq!((code, stdout), (Some(0), plain));
    assert!(sb.invocations().is_empty());
}

/// R10 `usage --wait --live`: each attempt asks the agent; a query that failed is nothing to
/// wait for (exit 1, as `usage --live` exits), one whose answer has nothing used up is done.
#[test]
fn usage_wait_live_asks_the_agent() {
    let Setup { sb, max, .. } = setup();
    // No fixture: the fake claude exits 1.
    let (code, stdout, stderr, _) = timed(&sb, &["usage", "max", "--wait", "--live"]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stdout.starts_with("claude:max  error: "), "{stdout}");
    assert_eq!(
        stderr,
        "remuda: nothing to wait for: the live query failed\n"
    );
    sb.set_live_usage(Some(&max), LIVE_SAMPLE);
    let (code, stdout, stderr, _) = timed(&sb, &["usage", "max", "--wait", "--live"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stdout.starts_with("claude:max  live\n"), "{stdout}");
    let invs = sb.invocations_with(Some(&max));
    assert_eq!(invs.len(), 2);
    assert!(invs.iter().all(|i| i.args == LIVE_ARGS));
}

// --- history (R24) with --wait (R10) ------------------------------------------------------

/// R24, R10: `--history` shows what was recorded; it does not combine with `--wait` (nor with
/// `--max-wait`, which needs `--wait`).
#[test]
fn history_does_not_combine_with_wait() {
    let Setup { sb, .. } = setup();
    for args in [
        &["usage", "max", "--history", "--wait"][..],
        &["usage", "max", "--wait", "--history"],
        &["usage", "max", "--history", "--wait", "--max-wait", "5"],
        &["usage", "max", "--history", "--max-wait", "5"],
    ] {
        let (code, stdout, stderr, _) = timed(&sb, args);
        assert_eq!(code, Some(2), "{args:?}: {stderr}");
        assert_eq!(stdout, "", "{args:?}");
    }
    let (_, _, stderr, _) = timed(&sb, &["usage", "max", "--history", "--wait"]);
    assert!(stderr.contains("cannot be used with"), "{stderr}");
    assert!(!sb.remuda_home().join("state").exists());
}

/// R24, R10: `usage --wait` records the reading it prints, as `usage` does: cached at the cache
/// time, when it is done at once and when it gives up (`--max-wait`) on a window used up; live at
/// the time the answer arrived. A live query that failed records nothing.
#[test]
fn usage_wait_records_the_reading_it_prints() {
    let Setup { sb, max, team } = setup();
    // Nothing used up: done at once.
    let fetched = now_ms() as i64 - 60_000;
    let free = r#"{"limits": [{"kind": "session", "percent": 34,
        "resets_at": "2099-09-23T15:39:59Z"}]}"#;
    sb.write_claude_json(Some(&team), &cache_at(fetched, free));
    let (code, _, stderr, _) = timed(&sb, &["usage", "team", "--wait"]);
    assert_eq!(code, Some(0), "{stderr}");
    let got = history(&sb);
    let ts = jiff::Timestamp::from_millisecond(fetched)
        .unwrap()
        .to_string();
    assert_eq!(
        got,
        [
            serde_json::json!({"ts": ts, "account": "claude:team", "label": "Session",
            "model": null, "percent": 34.0, "resets_at": "2099-09-23T15:39:59Z",
            "source": "cached"})
        ]
    );

    // A window used up until 2099: gives up at once, and what it printed is recorded.
    sb.write_claude_json(Some(&max), &cache_at(fetched, LIMITS));
    let (code, stdout, _, _) = timed(&sb, &["usage", "max", "--wait", "--max-wait", "0"]);
    assert_eq!(code, Some(1));
    assert!(stdout.contains("Week (Fable)"), "{stdout}");
    let max_points: Vec<_> = history(&sb)
        .into_iter()
        .filter(|p| p["account"] == "claude:max")
        .collect();
    let labels: Vec<&str> = max_points
        .iter()
        .map(|p| p["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels, ["Session", "Week (all models)", "Week (Fable)"]);
    assert!(max_points.iter().all(|p| p["ts"] == ts.as_str()));

    // Live: recorded at the answer; a failed query (no fixture for the default) records nothing.
    let before = history(&sb).len();
    let (code, _, _, _) = timed(&sb, &["usage", "default", "--wait", "--live"]);
    assert_eq!(code, Some(1));
    assert_eq!(history(&sb).len(), before);
    let mut free_live = LIVE_SAMPLE.replace("85% used", "15% used");
    free_live = free_live.replace("74% used", "14% used");
    sb.set_live_usage(Some(&team), &free_live);
    let start = jiff::Timestamp::now();
    let (code, stdout, stderr, _) = timed(&sb, &["usage", "team", "--wait", "--live"]);
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stdout.starts_with("claude:team  live\n"), "{stdout}");
    let live: Vec<_> = history(&sb)
        .into_iter()
        .filter(|p| p["source"] == "live")
        .collect();
    assert_eq!(live.len(), 3, "{live:?}");
    for p in &live {
        assert_eq!(p["account"], "claude:team");
        let at: jiff::Timestamp = p["ts"].as_str().unwrap().parse().unwrap();
        assert!(at >= start, "{p}");
    }
    assert!(!sb.launch_log().exists());
}

/// R10, R24: a codex window is recorded with the minutes codex told (`window_minutes`), and
/// `--history` paces it by them, not by its label: 40% used half an hour into a window of 90
/// minutes (labeled `2h window`) is ahead of an even pace; two hours would be behind it.
#[test]
fn a_codex_window_is_paced_by_its_minutes() {
    use common::rollouts::{token_count, write_rollout};
    let sb = Sandbox::new();
    let work = sb.make_codex_home("c/work");
    sb.write_config(&format!(
        "[[account]]\nprovider = \"codex\"\nname = \"work\"\nhome = \"{}\"\n",
        work.display()
    ));
    let now = minute();
    let (read, reset) = (plus(now, -30 * 60), plus(now, 30 * 60));
    let limits = serde_json::json!({"limit_id": "codex", "limit_name": null,
        "primary": {"used_percent": 40.0, "window_minutes": 90, "resets_at": reset.as_second()},
        "secondary": null, "plan_type": "plus"});
    write_rollout(
        &work,
        "019c1e08-e4f6-7d70-a129-38ec744a3f3c",
        &token_count(limits, &read.to_string()),
    );
    let out = stdout_of(sb.remuda().args(["usage", "codex:work"]).assert().success());
    assert!(out.contains("2h window"), "{out}");
    assert_eq!(
        history(&sb),
        [
            serde_json::json!({"ts": read.to_string(), "account": "codex:work",
            "label": "2h window", "model": null, "percent": 40.0,
            "resets_at": reset.to_string(), "source": "cached", "window_minutes": 90})
        ]
    );
    let out = stdout_of(
        sb.remuda()
            .args(["usage", "--history", "codex:work"])
            .assert()
            .success()
            .stderr(""),
    );
    assert_eq!(
        out,
        format!(
            "codex:work\n  2h window\n    {}  40%  resets {}\n    \
             used 40% with 33% of the window elapsed: ahead of an even pace; \
             at this pace 100% by {} (resets {})\n",
            utc(read),
            utc(reset),
            utc(plus(now, 15 * 60)),
            utc(reset),
        )
    );
    assert!(sb.codex_invocations().is_empty());
}

/// R10, R24: `usage --wait` takes a second attempt and records the first one's reading only.
/// The session is used up and resets in about ten seconds: the first attempt waits, the next
/// is `MIN_INTERVAL` (60 s) later, when the cached session has reset since: done, exit 0. The
/// history holds the first attempt's points, once each: the second attempt's week is the same
/// reading, its session (`reset since cached`) is not recorded. The wait is real, and the
/// status line is there on a terminal (stderr is a pseudo-terminal here).
#[test]
fn usage_wait_records_the_attempt_before_a_reset_once() {
    use std::os::fd::FromRawFd;
    let Setup { sb, max, .. } = setup();
    let fetched = now_ms() as i64 - 60_000;
    let reset = jiff::Timestamp::from_millisecond(now_ms() as i64 + 10_000).unwrap();
    let limits = format!(
        r#"{{"limits": [
            {{"kind": "session", "percent": 100, "resets_at": "{reset}"}},
            {{"kind": "weekly_all", "percent": 20, "resets_at": "2099-09-23T15:39:59Z"}}]}}"#
    );
    sb.write_claude_json(Some(&max), &cache_at(fetched, &limits));
    let (master, slave) = pty();
    let mut cmd = sb.remuda_process();
    // SAFETY: the descriptor `pty` opened; the command owns it from here, and closes it once
    // it is dropped.
    let terminal_end = unsafe { std::fs::File::from_raw_fd(slave) };
    cmd.args(["usage", "max", "--wait"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(terminal_end);
    let start = Instant::now();
    let remuda = cmd.spawn().unwrap();
    // Only remuda has the terminal's end now: reading ends when it is gone.
    drop(cmd);
    let terminal = std::thread::spawn(move || {
        use std::io::Read;
        // SAFETY: the descriptor `pty` opened; this file is its only owner.
        let mut master = unsafe { std::fs::File::from_raw_fd(master) };
        let mut shown = Vec::new();
        let _ = master.read_to_end(&mut shown);
        String::from_utf8_lossy(&shown).into_owned()
    });
    let out = remuda.wait_with_output().unwrap();
    let took = start.elapsed();
    let shown = terminal.join().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "{stdout}{shown}");
    assert!(
        took >= Duration::from_secs(60),
        "took {took:?}: no second attempt"
    );
    assert!(took < Duration::from_secs(90), "took {took:?}");
    assert!(
        shown.contains("remuda: waiting: claude:max Session 100% used"),
        "{shown:?}"
    );
    assert!(stdout.contains("reset since cached"), "{stdout}");
    let ts = jiff::Timestamp::from_millisecond(fetched)
        .unwrap()
        .to_string();
    assert_eq!(
        history(&sb),
        [
            serde_json::json!({"ts": ts, "account": "claude:max", "label": "Session",
                "model": null, "percent": 100.0, "resets_at": reset.to_string(),
                "source": "cached"}),
            serde_json::json!({"ts": ts, "account": "claude:max",
                "label": "Week (all models)", "model": null, "percent": 20.0,
                "resets_at": "2099-09-23T15:39:59Z", "source": "cached"}),
        ]
    );
}
