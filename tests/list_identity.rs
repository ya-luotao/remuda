//! R10a: `remuda list` shows each account's login identity.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::{Sandbox, parse_table};
use predicates::prelude::*;

const AUTH_MAX: &str = r#"{"loggedIn": true, "authMethod": "claude.ai", "email": "max@example.com",
  "orgName": "Example Org", "subscriptionType": "max", "configDirectory": "/ignored"}"#;
const AUTH_NATIVE: &str = r#"{"loggedIn": true, "authMethod": "claude.ai", "email": "native@example.com",
  "orgName": "Native Org", "subscriptionType": "pro"}"#;
const AUTH_TEAM: &str = r#"{"loggedIn": true, "email": "team@example.com", "orgName": "Team Org",
  "subscriptionType": "team", "futureField": [1, 2]}"#;
const CLAUDE_JSON_MAX: &str = r#"{"numStartups": 3, "oauthAccount": {"accountUuid": "00000000-0000-0000-0000-000000000000",
  "emailAddress": "max-cached@example.com", "organizationName": "Cached Org"}}"#;

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

/// Runs `remuda list <extra>`, expects exit 0, returns rows keyed by ACCOUNT, and stderr.
fn list(sb: &Sandbox, extra: &[&str]) -> (BTreeMap<String, BTreeMap<String, String>>, String) {
    let out = sb
        .remuda()
        .arg("list")
        .args(extra)
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let rows = parse_table(&stdout);
    let header: Vec<&str> = stdout.lines().next().unwrap().split_whitespace().collect();
    assert_eq!(header, ["ACCOUNT", "EMAIL", "ORG", "PLAN", "HOME"]);
    let keyed = rows
        .into_iter()
        .map(|r| (r["ACCOUNT"].clone(), r))
        .collect();
    (keyed, String::from_utf8(out.stderr).unwrap())
}

fn cells(row: &BTreeMap<String, String>) -> [&str; 4] {
    [&row["EMAIL"], &row["ORG"], &row["PLAN"], &row["HOME"]].map(String::as_str)
}

#[test]
fn list_shows_identity_under_each_accounts_env() {
    let Setup { sb, max, team } = setup();
    sb.set_auth(None, AUTH_NATIVE);
    sb.set_auth(Some(&max), AUTH_MAX);
    sb.set_auth(Some(&team), AUTH_TEAM);

    let out = sb
        .remuda()
        .env("CLAUDE_CONFIG_DIR", sb.root().join("elsewhere"))
        .arg("list")
        .assert()
        .success()
        .stderr("")
        .get_output()
        .clone();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let rows = parse_table(&stdout);
    let order: Vec<&str> = rows.iter().map(|r| r["ACCOUNT"].as_str()).collect();
    assert_eq!(order, ["claude:default", "claude:max", "claude:team"]);
    assert_eq!(
        cells(&rows[0]),
        ["native@example.com", "Native Org", "pro", "default"]
    );
    assert_eq!(
        cells(&rows[1]),
        [
            "max@example.com",
            "Example Org",
            "max",
            max.to_str().unwrap()
        ]
    );
    assert_eq!(
        cells(&rows[2]),
        [
            "team@example.com",
            "Team Org",
            "team",
            team.to_str().unwrap()
        ]
    );

    // One `auth status --json` per account, each under that account's env; default has
    // CLAUDE_CONFIG_DIR removed even though the parent had it set.
    assert_eq!(sb.invocations().len(), 3);
    for dir in [None, Some(max.as_path()), Some(team.as_path())] {
        let invs = sb.invocations_with(dir);
        assert_eq!(invs.len(), 1, "{dir:?}");
        assert_eq!(invs[0].args, ["auth", "status", "--json"]);
    }
    assert!(
        !sb.remuda_home().join("state").exists(),
        "list must not log launches"
    );
}

#[test]
fn list_falls_back_to_claude_json_when_command_fails() {
    let Setup { sb, max, team } = setup();
    sb.set_auth(Some(&team), AUTH_TEAM);
    sb.write_claude_json(Some(&max), CLAUDE_JSON_MAX);
    sb.write_claude_json(
        None,
        r#"{"oauthAccount": {"emailAddress": "native-cached@example.com"}}"#,
    );
    let (rows, stderr) = list(&sb, &[]);
    assert_eq!(
        cells(&rows["claude:max"]),
        [
            "max-cached@example.com (cached)",
            "Cached Org",
            "-",
            max.to_str().unwrap()
        ]
    );
    assert_eq!(
        cells(&rows["claude:default"])[..3],
        ["native-cached@example.com (cached)", "-", "-"]
    );
    assert_eq!(rows["claude:team"]["EMAIL"], "team@example.com");
    assert!(stderr.contains("claude:max"), "{stderr}");
    assert!(
        stderr.lines().all(|l| l.starts_with("remuda: warning: ")),
        "{stderr}"
    );
}

#[test]
fn list_shows_not_logged_in() {
    let Setup { sb, max, .. } = setup();
    sb.set_auth(Some(&max), r#"{"loggedIn": false}"#);
    // The stale cache must not override an explicit loggedIn: false.
    sb.write_claude_json(Some(&max), CLAUDE_JSON_MAX);
    let (rows, _) = list(&sb, &[]);
    assert_eq!(rows["claude:max"]["EMAIL"], "not logged in");
}

#[test]
fn list_shows_unknown_when_nothing_is_available() {
    let Setup { sb, .. } = setup();
    let (rows, _) = list(&sb, &[]);
    for account in ["claude:default", "claude:max", "claude:team"] {
        assert_eq!(rows[account]["EMAIL"], "unknown", "{account}");
    }
}

#[test]
fn list_treats_unparseable_output_as_failure() {
    let Setup { sb, max, team } = setup();
    sb.set_auth(Some(&max), "Not JSON at all\n");
    sb.set_auth(Some(&team), "[1, 2, 3]");
    sb.write_claude_json(Some(&max), CLAUDE_JSON_MAX);
    sb.set_auth(None, AUTH_NATIVE);
    let (rows, _) = list(&sb, &[]);
    assert_eq!(
        rows["claude:max"]["EMAIL"],
        "max-cached@example.com (cached)"
    );
    assert_eq!(rows["claude:team"]["EMAIL"], "unknown");
    assert_eq!(rows["claude:default"]["EMAIL"], "native@example.com");
}

#[test]
fn list_timeout_falls_back_and_does_not_block_others() {
    let Setup { sb, max, team } = setup();
    // The timeout leaves the non-hanging fakes ample time even on a loaded machine; the
    // hang is far longer, so finishing well under it proves the hanging one was cut off.
    let hang = Duration::from_secs(120);
    sb.set_hang(Some(&max), hang.as_secs() as u32);
    sb.write_claude_json(Some(&max), CLAUDE_JSON_MAX);
    sb.set_auth(Some(&team), AUTH_TEAM);
    sb.set_auth(None, AUTH_NATIVE);
    let start = Instant::now();
    let (rows, stderr) = list(&sb, &["--timeout", "3"]);
    assert!(start.elapsed() < hang / 6, "took {:?}", start.elapsed());
    assert_eq!(
        rows["claude:max"]["EMAIL"],
        "max-cached@example.com (cached)"
    );
    assert_eq!(rows["claude:team"]["EMAIL"], "team@example.com");
    assert_eq!(rows["claude:default"]["EMAIL"], "native@example.com");
    assert!(stderr.contains("timed out"), "{stderr}");
}

#[test]
fn list_without_claude_on_path_uses_cache() {
    let Setup { sb, max, .. } = setup();
    sb.write_claude_json(Some(&max), CLAUDE_JSON_MAX);
    let out = sb
        .remuda()
        .env("PATH", "/usr/bin:/bin")
        .arg("list")
        .assert()
        .success()
        .stderr(predicate::str::contains("claude"))
        .get_output()
        .clone();
    let rows = parse_table(&String::from_utf8(out.stdout).unwrap());
    assert_eq!(rows[1]["EMAIL"], "max-cached@example.com (cached)");
    assert_eq!(rows[0]["EMAIL"], "unknown");
}

#[test]
fn list_rejects_bad_timeout() {
    let Setup { sb, .. } = setup();
    for bad in ["0", "-1", "abc"] {
        sb.remuda()
            .args(["list", "--timeout", bad])
            .assert()
            .code(2);
    }
}
