//! R10: `remuda usage [<account>] [--live]`.

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
        assert_eq!(invs[0].args, ["-p", "/usage", "--no-session-persistence"]);
    }
    assert!(
        !sb.remuda_home().join("state").exists(),
        "live usage must not log launches"
    );
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
    assert_eq!(invs[0].args, ["-p", "/usage", "--no-session-persistence"]);
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

/// `remuda <args>` with the hanging agent as `program`, interrupted once the agent runs:
/// remuda's own exit signal, and whether the agent and its process are gone. Only remuda is
/// signalled, as by a terminal whose foreground process group the agent has left.
fn interrupted(sb: &Sandbox, program: &str, args: &[&str]) -> (Option<i32>, bool, bool) {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    common::write_executable(&sb.bin().join(program), HANGING_AGENT);
    let mut cmd = sb.remuda_process();
    cmd.args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: the closure only calls `signal`, which is async-signal-safe. Whatever started
    // the tests may have left SIGINT ignored, which remuda would then leave alone too.
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            Ok(())
        });
    }
    let mut remuda = cmd.spawn().unwrap();
    let agent = pid_once_written(&sb.home().join("agent.pid"));
    let inner = pid_once_written(&sb.home().join("inner.pid"));
    // SAFETY: kill(2) takes no pointers.
    assert_eq!(
        unsafe { libc::kill(remuda.id() as libc::pid_t, libc::SIGINT) },
        0
    );
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
    );
    assert_eq!(got, (Some(libc::SIGINT), true, true));
}

/// One `remuda usage --live` over 60 claude and 60 codex accounts (and the two `default`
/// ones), each query a fake agent that hangs, interrupted `after` remuda was started: whether
/// any process remuda had started outlived it.
///
/// remuda is given the writing end of a pipe, which every process it starts inherits: the
/// reading end sees the end of the pipe when the last of them is gone, and not before. That
/// counts a query from its `fork` on, before it could say anything itself.
fn outlived_by_a_query(after: Duration) -> bool {
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
    cmd.args(["usage", "--live", "--timeout", "60"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: the closure only calls `signal` and `fcntl`, both async-signal-safe: SIGINT has
    // its default action (see `interrupted`), and the pipe's writing end, closed on exec in
    // every other process the tests start, stays open in remuda and what it starts.
    unsafe {
        cmd.pre_exec(move || {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
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
    assert_eq!(
        unsafe { libc::kill(remuda.id() as libc::pid_t, libc::SIGINT) },
        0
    );
    let until = Instant::now() + Duration::from_secs(10);
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
    assert_eq!(status.signal(), Some(libc::SIGINT), "{status:?}");
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
            !outlived_by_a_query(after),
            "interrupted after {after:?}: a query outlived remuda"
        );
    }
}
