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

const LIMITS: &str = r#"{
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
