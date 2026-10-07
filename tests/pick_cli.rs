//! R23: `remuda pick`. Every test runs with the fake curl first on PATH (R15); the key is a
//! sentinel that must never show in output or in curl's arguments.

mod common;

use std::path::{Path, PathBuf};

use common::Sandbox;
use serde_json::{Value, json};

const KEY: &str = "test-key-DO-NOT-LEAK";

fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

/// RFC 3339 for `offset` seconds from now.
fn iso(offset: i64) -> String {
    jiff::Timestamp::from_second(now() + offset)
        .unwrap()
        .to_string()
}

/// A claude usage limit: `session`, `weekly_all`, or a model name for `weekly_scoped`.
fn limit(kind: &str, percent: f64, resets_in: i64) -> Value {
    match kind {
        "session" | "weekly_all" => {
            json!({"kind": kind, "percent": percent, "severity": "normal", "resets_at": iso(resets_in)})
        }
        model => json!({"kind": "weekly_scoped", "percent": percent, "severity": "normal",
                        "resets_at": iso(resets_in),
                        "scope": {"model": {"id": null, "display_name": model}}}),
    }
}

/// A `.claude.json` logged in as `email`, with usage cached `age` seconds ago.
fn claude_json(email: &str, age: i64, limits: &[Value]) -> String {
    json!({
        "numStartups": 3,
        "oauthAccount": {"emailAddress": email, "organizationName": "Acme Secret Org"},
        "cachedUsageUtilization": {
            "fetchedAtMs": (now() - age) * 1000,
            "utilization": {"limits": limits},
        },
    })
    .to_string()
}

const DAY: i64 = 86_400;

/// A request body without the parts that follow the clock, so that two runs a moment apart
/// compare equal: the local time, and the time left until each reset (`3d` becomes `2d23h` as
/// soon as a second passes).
fn without_clock(body: &str) -> String {
    const RESETS: &str = "resets in ";
    let mut out = String::new();
    let mut rest = body;
    while let Some(i) = rest.find(RESETS) {
        out.push_str(&rest[..i + RESETS.len()]);
        rest = &rest[i + RESETS.len()..];
        let end = rest
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(rest.len());
        out.push('_');
        rest = &rest[end..];
    }
    out.push_str(rest);
    const TIME: &str = "local time: ";
    if let Some(i) = out.find(TIME) {
        let end = out[i..].find("\\n").map_or(out.len(), |j| i + j);
        out.replace_range(i + TIME.len()..end, "_");
    }
    out
}

/// Registers `(provider, name, home)` and appends `pick`.
fn configure(sb: &Sandbox, accounts: &[(&str, &str, &Path)], pick: &str) {
    let mut text = String::new();
    for (provider, name, home) in accounts {
        text.push_str(&format!(
            "[[account]]\nprovider = \"{provider}\"\nname = \"{name}\"\nhome = \"{}\"\n\n",
            home.display()
        ));
    }
    text.push_str(pick);
    sb.write_config(&text);
}

const MODELS: &str = "[pick.claude]\nmodels = [\"claude-opus-5-5\", \"claude-sonnet-5\"]\n";
const NOTES: &str = "[pick]\nnotes = \"Keep claude:team for the big refactor.\"\n";

/// `max` with 60% left on the week, `team` with 20%; `claude:default` has nothing (logged out).
fn two_accounts(pick: &str) -> (Sandbox, PathBuf, PathBuf) {
    let sb = Sandbox::new();
    let max = sb.make_claude_home("h/max");
    let team = sb.make_claude_home("h/team");
    sb.write_claude_json(
        Some(&max),
        &claude_json(
            "max@example.com",
            600,
            &[limit("weekly_all", 40.0, 3 * DAY)],
        ),
    );
    sb.write_claude_json(
        Some(&team),
        &claude_json(
            "team@example.com",
            600,
            &[limit("weekly_all", 80.0, 2 * DAY)],
        ),
    );
    configure(
        &sb,
        &[("claude", "max", &max), ("claude", "team", &team)],
        pick,
    );
    (sb, max, team)
}

struct Out {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn pick(sb: &Sandbox, key: bool, args: &[&str]) -> Out {
    let mut cmd = sb.remuda();
    cmd.arg("pick").args(args);
    if key {
        cmd.env("TYPESAFE_API_KEY", KEY);
    }
    let out = cmd.output().unwrap();
    let out = Out {
        code: out.status.code(),
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
    };
    assert!(
        !out.stdout.contains(KEY) && !out.stderr.contains(KEY),
        "the key leaked:\n{}\n{}",
        out.stdout,
        out.stderr
    );
    for argv in sb.curl_invocations() {
        assert!(
            !argv.iter().any(|a| a.contains(KEY) || a.contains("Bearer")),
            "{argv:?}"
        );
    }
    out
}

/// The value of `key` in the text report (`key  value` lines).
fn field<'a>(out: &'a Out, key: &str) -> &'a str {
    out.stdout
        .lines()
        .find_map(|l| l.strip_prefix(key).filter(|rest| rest.starts_with("  ")))
        .map(str::trim)
        .unwrap_or_else(|| panic!("no `{key}` in:\n{}", out.stdout))
}

/// A Jev answer choosing `choice` with `confidence`.
fn answer(choice: &str, confidence: f64, probabilities: &[(&str, f64)]) -> String {
    let probabilities: serde_json::Map<String, Value> = probabilities
        .iter()
        .map(|(k, p)| (k.to_string(), json!(p)))
        .collect();
    json!({"model": "jev-1.13.0", "answers": {"launch": {"type": "choice", "choice": choice,
           "confidence": confidence, "probabilities": probabilities}},
           "usage": {"input_tokens": 400, "output_tokens": 30}})
    .to_string()
}

/// Without a key the rules decide, and curl is never run.
#[test]
fn without_a_key_the_rules_decide() {
    let (sb, _, _) = two_accounts(&format!("{NOTES}{MODELS}"));
    let out = pick(&sb, false, &[]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    assert_eq!(field(&out, "account"), "claude:max");
    assert_eq!(field(&out, "model"), "claude-opus-5-5");
    assert_eq!(field(&out, "effort"), "agent default");
    assert_eq!(
        field(&out, "decided by"),
        "rules (no_key: TYPESAFE_API_KEY is not set)"
    );
    assert!(field(&out, "limit").starts_with("Week (all models) 60% left, resets in "));
    assert_eq!(
        field(&out, "command"),
        "remuda run claude:max --model claude-opus-5-5"
    );
    assert!(
        out.stdout.contains("claude:default  not logged in"),
        "{}",
        out.stdout
    );
    assert!(sb.curl_invocations().is_empty());
}

/// `--offline`, empty notes, or nothing to choose: no request even with a key.
#[test]
fn offline_no_notes_or_a_single_option_send_nothing() {
    let (sb, _, _) = two_accounts(&format!("{NOTES}{MODELS}"));
    let out = pick(&sb, true, &["--offline"]);
    assert_eq!(field(&out, "decided by"), "rules (offline: --offline)");

    let (sb, _, _) = two_accounts(MODELS);
    let out = pick(&sb, true, &[]);
    assert_eq!(
        field(&out, "decided by"),
        "rules (no_notes: no [pick] notes)"
    );

    // One account left, no models or efforts configured: one option.
    let (sb, _, _) = two_accounts(&format!("{NOTES}exclude = [\"team\"]\n"));
    let out = pick(&sb, true, &[]);
    assert_eq!(
        field(&out, "decided by"),
        "rules (single_option: only one option)"
    );
    assert_eq!(field(&out, "model"), "agent default");
    assert_eq!(field(&out, "command"), "remuda run claude:max");
    assert_eq!(field(&out, "excluded"), "claude:team");
    assert!(sb.curl_invocations().is_empty());
}

/// A confident Jev answer wins over the rules' first pair, and the report says both.
#[test]
fn a_confident_jev_answer_is_taken() {
    let (sb, _, _) = two_accounts(&format!("{NOTES}{MODELS}"));
    // Aliases in registry order: claude:max is account-1, claude:team account-2.
    sb.set_jev_response(&answer(
        "claude:account-2 / claude-opus-5-5",
        0.9,
        &[
            ("claude:account-2 / claude-opus-5-5", 0.95),
            ("claude:account-1 / claude-opus-5-5", 0.05),
        ],
    ));
    let out = pick(&sb, true, &[]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    assert_eq!(field(&out, "account"), "claude:team");
    assert_eq!(field(&out, "decided by"), "jev (confidence 0.90)");
    assert_eq!(
        field(&out, "rules"),
        "would choose claude:max / claude-opus-5-5"
    );
    let calls = sb.curl_invocations();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0][0], "-q");
    assert_eq!(
        calls[0].last().map(String::as_str),
        Some("https://api.typesafe.ai/v1/systemone")
    );
    assert!(
        sb.curl_stdin()
            .starts_with(&format!("header = \"Authorization: Bearer {KEY}\"\n"))
    );
}

/// Unsure of the pair but sure of the account: that account with its most probable model.
/// Spread out: the rules, and `--json` still shows Jev's confidence.
#[test]
fn a_probable_account_or_the_rules() {
    let (sb, _, _) = two_accounts(&format!("{NOTES}{MODELS}"));
    sb.set_jev_response(&answer(
        "claude:account-2 / claude-sonnet-5",
        0.3,
        &[
            ("claude:account-1 / claude-opus-5-5", 0.1),
            ("claude:account-1 / claude-sonnet-5", 0.1),
            ("claude:account-2 / claude-opus-5-5", 0.3),
            ("claude:account-2 / claude-sonnet-5", 0.5),
        ],
    ));
    let out = pick(&sb, true, &[]);
    assert_eq!(field(&out, "account"), "claude:team");
    assert_eq!(field(&out, "model"), "claude-sonnet-5");
    assert_eq!(
        field(&out, "decided by"),
        "jev (account, probability 0.80; confidence 0.30)"
    );

    sb.set_jev_response(&answer(
        "claude:account-2 / claude-sonnet-5",
        0.3,
        &[
            ("claude:account-1 / claude-opus-5-5", 0.3),
            ("claude:account-1 / claude-sonnet-5", 0.2),
            ("claude:account-2 / claude-opus-5-5", 0.2),
            ("claude:account-2 / claude-sonnet-5", 0.3),
        ],
    ));
    let out = pick(&sb, true, &[]);
    assert_eq!(field(&out, "account"), "claude:max");
    assert!(
        field(&out, "decided by").starts_with("rules (low_confidence: jev confidence 0.30"),
        "{}",
        out.stdout
    );
    let out = pick(&sb, true, &["--json"]);
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(v["decided_by"], "rules");
    assert_eq!(v["reason"], "low_confidence");
    assert_eq!(v["jev"]["confidence"], 0.3);
    assert_eq!(v["jev"]["model"], "jev-1.13.0");
}

/// Every failure falls back to the rules with `jev_error`, and never shows the key.
#[test]
fn jev_errors_fall_back_to_the_rules() {
    let (sb, _, _) = two_accounts(&format!("{NOTES}{MODELS}"));
    let decided = |sb: &Sandbox| {
        let out = pick(sb, true, &[]);
        assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
        assert_eq!(field(&out, "account"), "claude:max");
        field(&out, "decided by").to_string()
    };
    sb.set_jev_response("{\"error\": \"invalid api key\"}");
    sb.set_jev_status(401);
    assert_eq!(
        decided(&sb),
        "rules (jev_error: HTTP 401: {\"error\": \"invalid api key\"})"
    );
    sb.set_jev_status(200);
    sb.set_jev_response("<html>not json</html>");
    assert!(decided(&sb).starts_with("rules (jev_error: not JSON in the response"));
    sb.set_jev_response(&answer("claude:max / claude-opus-5-5", 1.0, &[]));
    assert_eq!(
        decided(&sb),
        "rules (jev_error: Jev chose \"claude:max / claude-opus-5-5\", which is not an offered \
         option)"
    );
    sb.set_curl_exit(28);
    assert_eq!(decided(&sb), "rules (jev_error: timed out after 10s)");
    assert_eq!(sb.curl_invocations().len(), 4);

    // No curl on PATH. PATH is the sandbox's bin alone: `/usr/bin/curl` must not be found.
    std::fs::remove_file(sb.bin().join("curl")).unwrap();
    let out = sb
        .remuda()
        .env("PATH", sb.bin())
        .env("TYPESAFE_API_KEY", KEY)
        .arg("pick")
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("rules (jev_error: `curl` not found on PATH)"),
        "{stdout}"
    );
    assert!(!stdout.contains(KEY));
}

/// The request carries no email, organization, path or working directory; qualified account
/// names in the notes are aliased, bare ones are left as written; the key goes on stdin only.
#[test]
fn the_request_is_private() {
    let sb = Sandbox::new();
    let secret = sb.make_claude_home("h/secretname");
    sb.write_claude_json(
        Some(&secret),
        &claude_json(
            "secret.person@example.com",
            600,
            &[limit("weekly_all", 10.0, 3 * DAY)],
        ),
    );
    let other = sb.make_claude_home("h/other");
    sb.write_claude_json(
        Some(&other),
        &claude_json(
            "other@example.com",
            600,
            &[limit("weekly_all", 30.0, 3 * DAY)],
        ),
    );
    let notes = "Use claude:secretname for long refactors; secretname otherwise, as written.";
    configure(
        &sb,
        &[
            ("claude", "secretname", &secret),
            ("claude", "other", &other),
        ],
        &format!("[pick]\nnotes = \"{notes}\"\n"),
    );
    sb.set_jev_response(&answer("claude:account-1 / default", 0.9, &[]));
    let out = pick(&sb, true, &[]);
    assert_eq!(field(&out, "account"), "claude:secretname");
    let body = sb.jev_request_body();
    for leak in [
        "secret.person@example.com",
        "other@example.com",
        "Acme Secret Org",
        "claude:secretname",
        sb.root().to_str().unwrap(),
        sb.home().to_str().unwrap(),
        sb.work().to_str().unwrap(),
        "h/secretname",
        KEY,
    ] {
        assert!(!body.contains(leak), "{leak:?} in {body}");
    }
    let v: Value = serde_json::from_str(&body).unwrap();
    let state = v["state"].as_str().unwrap();
    assert!(
        state
            .contains("Use claude:account-1 for long refactors; secretname otherwise, as written."),
        "{state}"
    );
    assert!(
        state.contains("account claude:account-1 (claude)"),
        "{state}"
    );
    assert_eq!(v["model"], "jev-latest");
    assert!(
        sb.curl_stdin()
            .contains(&format!("Authorization: Bearer {KEY}"))
    );
}

/// Every qualified name in the notes is aliased, however it is written: right after CJK text,
/// before full-width punctuation, after `-` or `_`, and when the account is not registered (it
/// gets the next alias, in the order the notes name them). A longer name is another account,
/// and bare names stay as written. `--print-request` shows exactly what is sent.
#[test]
fn every_qualified_name_in_the_notes_is_aliased() {
    let sb = Sandbox::new();
    let secret = sb.make_claude_home("h/secretname");
    sb.write_claude_json(
        Some(&secret),
        &claude_json("a@example.com", 600, &[limit("weekly_all", 10.0, 3 * DAY)]),
    );
    let other = sb.make_claude_home("h/other");
    sb.write_claude_json(
        Some(&other),
        &claude_json("b@example.com", 600, &[limit("weekly_all", 30.0, 3 * DAY)]),
    );
    let notes = "把claude:secretname留给大重构。别用 claude:oldsecret（已移除的账号）；\
                 -claude:secretname、_claude:secretname_ 和 codex:secretcodex 也一样。\
                 claude:secretname2 不是它。max 照旧。";
    configure(
        &sb,
        &[
            ("claude", "secretname", &secret),
            ("claude", "other", &other),
        ],
        &format!("[pick]\nnotes = \"{notes}\"\n"),
    );
    sb.set_jev_response(&answer("claude:account-1 / default", 0.9, &[]));
    let out = pick(&sb, true, &[]);
    assert_eq!(field(&out, "account"), "claude:secretname");
    let body = sb.jev_request_body();
    for leak in ["secretname", "oldsecret", "secretcodex"] {
        assert!(!body.contains(leak), "{leak:?} in {body}");
    }
    let v: Value = serde_json::from_str(&body).unwrap();
    let state = v["state"].as_str().unwrap();
    assert!(
        state.contains(
            "把claude:account-1留给大重构。别用 claude:account-3（已移除的账号）；\
             -claude:account-1、_claude:account-4 和 codex:account-1 也一样。\
             claude:account-5 不是它。max 照旧。"
        ),
        "{state}"
    );
    let printed = pick(&sb, false, &["--print-request"]);
    assert_eq!(
        without_clock(printed.stdout.trim_end()),
        without_clock(&body)
    );
}

/// `--print-request` prints the body a send uses, sends nothing, and needs no key.
#[test]
fn print_request_shows_the_body_without_sending() {
    let (sb, _, _) = two_accounts(&format!("{NOTES}{MODELS}"));
    let printed = pick(&sb, false, &["--print-request"]);
    assert_eq!(printed.code, Some(0));
    assert!(
        printed.stderr.contains("would not be sent (no_key)"),
        "{}",
        printed.stderr
    );
    assert!(sb.curl_invocations().is_empty());
    let body: Value = serde_json::from_str(printed.stdout.trim_end()).unwrap();
    assert_eq!(body["questions"]["launch"]["type"], "choice");

    sb.set_jev_response(&answer("claude:account-1 / claude-opus-5-5", 0.9, &[]));
    pick(&sb, true, &[]);
    assert_eq!(
        without_clock(&sb.jev_request_body()),
        without_clock(printed.stdout.trim_end())
    );
}

/// Staleness is shown; a stale exhausted window stays exhausted until its reset. A window whose
/// reset has passed is of unknown usage: named, never counted, and the output offers `--live`.
#[test]
fn stale_usage_and_passed_resets() {
    let sb = Sandbox::new();
    let old = sb.make_claude_home("h/old");
    let spent = sb.make_claude_home("h/spent");
    // Five hours old, the session reset since.
    sb.write_claude_json(
        Some(&old),
        &claude_json(
            "o@example.com",
            5 * 3600,
            &[
                limit("session", 100.0, -3600),
                limit("weekly_all", 50.0, 2 * DAY),
            ],
        ),
    );
    // Five hours old and exhausted until tomorrow.
    sb.write_claude_json(
        Some(&spent),
        &claude_json(
            "s@example.com",
            5 * 3600,
            &[limit("weekly_all", 100.0, DAY)],
        ),
    );
    configure(
        &sb,
        &[("claude", "old", &old), ("claude", "spent", &spent)],
        "",
    );
    let out = pick(&sb, false, &[]);
    assert_eq!(field(&out, "account"), "claude:old");
    assert_eq!(
        field(&out, "usage"),
        "cached 5h ago (stale: may be higher now)"
    );
    assert!(
        out.stdout
            .contains("claude:spent / default  Week (all models): 100% used, below the 10% left"),
        "{}",
        out.stdout
    );
    assert!(field(&out, "limit").starts_with("Week (all models) 50% left, resets in "));
    assert_eq!(
        field(&out, "unknown"),
        "Session: reset since cached (--live asks the agent)"
    );
    let body = pick(&sb, false, &["--print-request"]).stdout;
    let v: Value = serde_json::from_str(body.trim_end()).unwrap();
    let state = v["state"].as_str().unwrap();
    assert!(
        state.contains("usage: cached 5h ago (stale: may be higher now)"),
        "{state}"
    );
    assert!(
        state.contains("  Session: usage unknown (reset since cached)\n"),
        "{state}"
    );
    assert!(!state.contains(": 0% used"), "{state}");

    let v: Value = serde_json::from_str(&pick(&sb, false, &["--json"]).stdout).unwrap();
    let old = &v["candidates"][1];
    assert_eq!(
        (old["account"].as_str(), old["stale"].as_bool()),
        (Some("claude:old"), Some(true))
    );
    assert_eq!(old["headroom"], 50.0);
    assert_eq!(old["binding"], "Week (all models)");
    assert_eq!(old["reset_passed"], true);
    assert_eq!(v["candidates"][2]["reset_passed"], false);
}

/// The review of 2026-10-02, on real data: usage cached four days ago, every reset passed since,
/// was recommended as "100% left" ahead of usage cached nine minutes ago with 71% left. It is of
/// unknown headroom: feasible, after every pair of known headroom. `--json` says `reset_passed`
/// and never names a reset in the past; nothing is queried live unless `--live` asks.
#[test]
fn usage_past_every_reset_ranks_after_known_headroom() {
    let sb = Sandbox::new();
    let alt = sb.make_claude_home("h/team-alt");
    let max = sb.make_claude_home("h/max");
    sb.write_claude_json(
        Some(&alt),
        &claude_json(
            "alt@example.com",
            4 * DAY,
            &[
                limit("session", 100.0, -4 * DAY + 3 * 3600),
                limit("weekly_all", 100.0, -DAY),
            ],
        ),
    );
    sb.write_claude_json(
        Some(&max),
        &claude_json(
            "max@example.com",
            540,
            &[limit("weekly_all", 29.0, 3 * DAY)],
        ),
    );
    // team-alt first: registry order would favor it.
    let accounts = [
        ("claude", "team-alt", alt.as_path()),
        ("claude", "max", &max),
    ];
    configure(&sb, &accounts, "");
    let out = pick(&sb, false, &[]);
    assert_eq!(field(&out, "account"), "claude:max");
    assert!(field(&out, "limit").starts_with("Week (all models) 71% left, resets in "));
    assert!(!out.stdout.contains("--live"), "{}", out.stdout);

    let v: Value = serde_json::from_str(&pick(&sb, false, &["--json"]).stdout).unwrap();
    assert_eq!(v["account"], "claude:max");
    let of = |account: &str| {
        v["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["account"] == account)
            .unwrap_or_else(|| panic!("no {account}: {v:#}"))
    };
    let alt_pair = of("claude:team-alt");
    assert_eq!(alt_pair["feasible"], true);
    assert_eq!(alt_pair["reset_passed"], true);
    assert_eq!(alt_pair["rules_rank"], 2);
    assert_eq!(alt_pair["stale"], true);
    for null in ["headroom", "binding", "resets_at", "why_not"] {
        assert_eq!(alt_pair[null], Value::Null, "{null}: {alt_pair:#}");
    }
    let max_pair = of("claude:max");
    assert_eq!(max_pair["reset_passed"], false);
    assert_eq!(max_pair["rules_rank"], 1);
    assert_eq!(max_pair["headroom"], 71.0);
    let resets_at: jiff::Timestamp = max_pair["resets_at"].as_str().unwrap().parse().unwrap();
    assert!(resets_at.as_second() > now(), "{max_pair:#}");

    // Alone, it is recommended, as what it is; `--run` launches it and says so, without a live
    // query of its own.
    configure(&sb, &accounts, "[pick]\nexclude = [\"claude:max\"]\n");
    let out = pick(&sb, false, &[]);
    assert_eq!(field(&out, "account"), "claude:team-alt");
    assert_eq!(field(&out, "limit"), "unknown");
    assert_eq!(
        field(&out, "unknown"),
        "Session, Week (all models): reset since cached (--live asks the agent)"
    );
    assert_eq!(
        field(&out, "usage"),
        "cached 4d ago (stale: may be higher now)"
    );
    let state = pick(&sb, false, &["--print-request"]).stdout;
    assert!(
        state.contains("Week (all models): usage unknown (reset since cached)"),
        "{state}"
    );
    let out = pick(&sb, false, &["--run", "--", "-p", "hello"]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    assert!(
        out.stderr.contains(
            "remuda: pick: usage unknown: Session, Week (all models): reset since cached \
             (--live asks the agent)"
        ),
        "{}",
        out.stderr
    );
    let inv = sb.only_invocation();
    assert_eq!(inv.config_dir.as_deref(), alt.to_str());
    assert_eq!(
        inv.args[..2],
        ["-p", "hello"],
        "the launch, not `-p /usage`"
    );
}

/// A live query takes time, and a reset does not wait for it: the usage is read once it is all
/// gathered, not when remuda started. Here the query fails after three seconds and the cache
/// decides; its exhausted session reset in the meantime, so it no longer blocks the pair.
#[test]
fn usage_is_read_once_gathered_not_when_pick_started() {
    let sb = Sandbox::new();
    let slow = sb.make_claude_home("h/slow");
    sb.write_claude_json(
        Some(&slow),
        &claude_json("s@example.com", 3600, &[limit("session", 100.0, 2)]),
    );
    // Every claude run for this home sleeps, then prints nothing: the live output is not
    // recognized, and the cache is used.
    sb.set_hang(Some(&slow), 3);
    configure(&sb, &[("claude", "slow", &slow)], "");
    let out = pick(&sb, false, &["--live", "--json"]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(v["account"], "claude:slow");
    let pair = &v["candidates"][1];
    assert_eq!(pair["account"], "claude:slow");
    assert_eq!(pair["source"], "cached");
    assert_eq!(pair["feasible"], true, "{pair:#}");
    assert_eq!(pair["reset_passed"], true);
    assert_eq!(pair["headroom"], Value::Null);
    assert_eq!(pair["resets_at"], Value::Null);
}

/// A live answer is recorded when it arrives and read when every account is in ([`gather`]'s
/// two instants, here 100 seconds apart). A reset it names behind its arrival is no reset
/// ahead; one that falls before everything is gathered has passed since it was asked.
#[test]
fn a_live_answer_is_recorded_when_it_arrives() {
    use remuda::pick::{self, Config, Sources};
    use remuda::provider::Provider;
    use remuda::registry::{Account, Home};
    use std::sync::atomic::{AtomicI64, Ordering};

    static TICKS: AtomicI64 = AtomicI64::new(0);
    fn t0() -> jiff::Timestamp {
        "2026-09-27T10:00:00Z".parse().unwrap()
    }
    fn clock() -> jiff::Timestamp {
        t0() + jiff::SignedDuration::from_secs(100 * TICKS.fetch_add(1, Ordering::SeqCst))
    }

    let sb = Sandbox::new();
    let home = sb.make_claude_home("h/max");
    sb.set_live_usage(
        Some(&home),
        "Current session: 60% used \u{b7} resets Sep 27 at 9:59am (UTC)\n\
         Current week (all models): 40% used \u{b7} resets Sep 27 at 10:01am (UTC)\n\
         Current week (Fable): 5% used \u{b7} resets Sep 27 at 10:30am (UTC)\n",
    );
    let account = Account {
        provider: Provider::Claude,
        name: "max".into(),
        home: Home::Path(home.to_str().unwrap().into()),
    };
    let claude = sb.bin().join("claude");
    let env = remuda::Env::new();
    let config = Config::default();
    let sources = Sources {
        env: &env,
        clock,
        agents: &remuda::account_command::OnPath {
            claude: Some(&claude),
            codex: None,
        },
        live: Some(std::time::Duration::from_secs(30)),
        provider: None,
        seen_by: None,
    };
    let (entries, now) = pick::gather(&[account], &config, &sources);
    let answered = t0();
    assert_eq!(now, answered + jiff::SignedDuration::from_secs(100));
    let usage = entries[0].usage.as_ref().expect("live usage");
    assert_eq!(usage.source.name(), "live");
    assert_eq!(usage.fetched_at, Some(answered));
    assert_eq!(usage.age_seconds, Some(100));
    let read: Vec<(&str, Option<f64>, Option<jiff::Timestamp>, bool)> = usage
        .windows
        .iter()
        .map(|w| (w.label.as_str(), w.used(), w.resets_at(), w.reset_passed()))
        .collect();
    assert_eq!(
        read,
        [
            // Behind when it was answered: the percentage stands, without a reset.
            ("Session", Some(60.0), None, false),
            // It reset while the rest was gathered.
            ("Week (all models)", None, None, true),
            (
                "Week (Fable)",
                Some(5.0),
                Some("2026-09-27T10:30:00Z".parse().unwrap()),
                false
            ),
        ]
    );
    let candidates = pick::candidates(&entries, &config, now, None);
    assert!(candidates[0].feasible());
    assert_eq!(candidates[0].headroom, Some(40.0));
    // Nothing to ask live: it was.
    assert_eq!(
        pick::live_hint(&entries[0], &candidates[0]).as_deref(),
        Some("Week (all models): reset since asked")
    );
    let v = pick::to_json(&entries, &candidates, None, &config, &[], None, now);
    assert_eq!(v["candidates"][0]["resets_at"], Value::Null);
    assert_eq!(v["candidates"][0]["reset_passed"], true);
    assert_eq!(v["candidates"][0]["age_seconds"], 100);

    // The answer's age, counted from the same instant, is in the text and in what Jev reads:
    // a live answer that waited for the others is not "just now".
    let asked = pick::Asked::Skipped(pick::Reason::Offline);
    let decision = pick::decide(&candidates, &entries, &config, asked).unwrap();
    let text = pick::format_text(&entries, &candidates, &decision, &config, now, &[], None);
    assert!(text.contains("usage       live 1m ago\n"), "{text}");
    let mut aliases = remuda::privacy::Aliases::default();
    aliases.note("claude:max");
    let tz = jiff::tz::TimeZone::UTC;
    let request = remuda::jev::request(&entries, &candidates, &config, &aliases, now, &tz, None);
    let state = request.body["state"].as_str().unwrap();
    assert!(state.contains("  usage: live, 1m ago\n"), "{state}");
    assert!(
        state.contains("  Week (all models): usage unknown (reset since asked)\n"),
        "{state}"
    );
    // One option: `launch` is not asked. With a second model it is, and each description
    // carries the age.
    let mut config = config;
    config.claude.models = vec!["claude-opus-5-5".into(), "claude-sonnet-5".into()];
    let candidates = pick::candidates(&entries, &config, now, None);
    let request = remuda::jev::request(&entries, &candidates, &config, &aliases, now, &tz, None);
    assert_eq!(
        request.body["questions"]["launch"]["criteria"]["claude:account-1 / claude-opus-5-5"],
        "tightest: Session 40% left; usage unknown: Week (all models): reset since asked; data \
         live, 1m old"
    );
}

/// Exclusions, `min_headroom`, and per-model windows; cached codex usage has no per-model
/// limits.
#[test]
fn hard_rules() {
    let sb = Sandbox::new();
    sb.install_codex();
    let max = sb.make_claude_home("h/max");
    let low = sb.make_claude_home("h/low");
    let gone = sb.make_claude_home("h/gone");
    sb.write_claude_json(
        Some(&max),
        &claude_json(
            "m@example.com",
            60,
            &[
                limit("weekly_all", 50.0, 3 * DAY),
                limit("Fable", 100.0, 3 * DAY),
            ],
        ),
    );
    sb.write_claude_json(
        Some(&low),
        &claude_json("l@example.com", 60, &[limit("weekly_all", 75.0, 3 * DAY)]),
    );
    sb.write_claude_json(
        Some(&gone),
        &claude_json("g@example.com", 60, &[limit("weekly_all", 0.0, 3 * DAY)]),
    );
    let work = sb.make_codex_home("c/work");
    sb.set_codex_login(Some(&work), "Logged in using ChatGPT");
    let limits = json!({"limit_id": "codex", "limit_name": null,
        "primary": {"used_percent": 20.0, "window_minutes": 10080, "resets_at": now() + 4 * DAY},
        "secondary": null});
    common::rollouts::write_rollout(
        &work,
        "019c1e08-e4f6-7d70-a129-38ec744a3f3c",
        &common::rollouts::token_count(limits, &iso(-600)),
    );
    configure(
        &sb,
        &[
            ("claude", "max", &max),
            ("claude", "low", &low),
            ("claude", "gone", &gone),
            ("codex", "work", &work),
        ],
        "[pick]\nexclude = [\"gone\"]\nmin_headroom = 30\n\n\
         [pick.claude]\nmodels = [\"claude-fable-5-1\", \"claude-opus-5-5\"]\n\n\
         [pick.codex]\nmodels = [\"gpt-6-astra\"]\n",
    );
    let v: Value = serde_json::from_str(&pick(&sb, false, &["--json"]).stdout).unwrap();
    let why = |account: &str, model: Option<&str>| {
        v["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["account"] == account && c["model"].as_str() == model)
            .map(|c| c["why_not"].clone())
            .unwrap_or_else(|| panic!("no {account} {model:?}: {v:#}"))
    };
    assert_eq!(why("claude:gone", None), "excluded ([pick] exclude)");
    assert!(
        why("claude:low", Some("claude-opus-5-5"))
            .as_str()
            .unwrap()
            .starts_with("Week (all models): 75% used, below the 30% left required")
    );
    assert!(
        why("claude:max", Some("claude-fable-5-1"))
            .as_str()
            .unwrap()
            .starts_with("Week (Fable): 100% used")
    );
    assert_eq!(why("claude:max", Some("claude-opus-5-5")), Value::Null);
    assert_eq!(why("codex:work", Some("gpt-6-astra")), Value::Null);
    // Model order first: codex's first model ranks with claude's first; codex:work has 80%
    // left, claude:max/opus is claude's second model.
    assert_eq!(v["account"], "codex:work");
    assert_eq!(
        v["command"],
        json!(["remuda", "run", "codex:work", "-m", "gpt-6-astra"])
    );
    let out = pick(&sb, false, &[]);
    assert!(
        out.stdout
            .contains("per-model limits unknown (codex reports them only live)"),
        "{}",
        out.stdout
    );
    let body = pick(&sb, false, &["--print-request"]).stdout;
    assert!(body.contains("per-model limits: unknown (codex reports them only live)"));
    assert!(
        !body.contains("claude:gone") && !body.contains("account-3 ("),
        "{body}"
    );
}

/// Logged-out accounts are not feasible; with nothing feasible, the reasons and exit 1.
#[test]
fn nothing_feasible_exits_1_with_the_reasons() {
    let sb = Sandbox::new();
    sb.install_codex();
    let fresh = sb.make_claude_home("h/fresh");
    let cx = sb.make_codex_home("c/cx");
    configure(
        &sb,
        &[("claude", "fresh", &fresh), ("codex", "cx", &cx)],
        "",
    );
    let out = pick(&sb, true, &[]);
    assert_eq!(out.code, Some(1));
    assert!(
        out.stderr.contains("nothing to recommend"),
        "{}",
        out.stderr
    );
    for line in [
        "claude:fresh    not logged in (no oauthAccount in .claude.json, no usage cache)",
        "codex:cx        not logged in (`codex login status`)",
    ] {
        assert!(out.stdout.contains(line), "{line:?} in:\n{}", out.stdout);
    }
    let out = pick(&sb, true, &["--json"]);
    assert_eq!(out.code, Some(1));
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(v["account"], Value::Null);
    // The schema holds with nothing feasible: `session` is there, null without a resume.
    assert_eq!(v.get("session"), Some(&Value::Null));
    assert!(sb.curl_invocations().is_empty());
}

/// Why `account` is not feasible, from the `not feasible:` lines of the report.
fn why_not<'a>(out: &'a Out, account: &str) -> &'a str {
    out.stdout
        .lines()
        .find_map(|l| l.trim_start().strip_prefix(account))
        .map(str::trim)
        .unwrap_or_else(|| panic!("no `{account}` in:\n{}", out.stdout))
}

/// With `--live`, one `codex app-server` run per codex account answers its usage and whether
/// it is logged in (`account/read`): `codex login status` runs only for an account whose
/// server did not say.
#[test]
fn live_asks_codex_once() {
    let sb = Sandbox::new();
    sb.install_codex();
    let work = sb.make_codex_home("c/work");
    sb.set_codex_account(
        Some(&work),
        r#"{"account": {"type": "chatgpt", "email": "cx@example.com", "planType": "pro"}}"#,
    );
    sb.set_codex_rate_limits(
        Some(&work),
        r#"{"rateLimits": {"limitId": "codex", "primary":
            {"usedPercent": 30, "windowDurationMins": 10080, "resetsAt": 1790414559}}}"#,
    );
    // Logged out: the fake's `account/read` answers `account: null`, as a logged-out home's.
    let out_home = sb.make_codex_home("c/out");
    // A codex without app-server, logged in.
    let old = sb.make_codex_home("c/old");
    sb.set_codex_without_app_server(Some(&old));
    sb.set_codex_login(Some(&old), "Logged in using ChatGPT");
    configure(
        &sb,
        &[
            ("codex", "work", &work),
            ("codex", "out", &out_home),
            ("codex", "old", &old),
        ],
        "[pick]\nexclude = [\"claude:default\", \"codex:default\"]\n",
    );
    let out = pick(&sb, false, &["--live"]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    assert_eq!(field(&out, "account"), "codex:work");
    // Live, with the answer's age (R23).
    let usage = field(&out, "usage");
    assert!(
        usage.starts_with("live ") && usage.ends_with(" ago"),
        "{usage:?}"
    );
    assert_eq!(
        why_not(&out, "codex:out"),
        "not logged in (`codex app-server` account/read)"
    );
    let runs = |home: &Path| -> Vec<Vec<String>> {
        sb.codex_invocations()
            .into_iter()
            .filter(|i| i.codex_home.as_deref() == home.to_str())
            .map(|i| i.args)
            .collect()
    };
    assert_eq!(runs(&work), [["app-server"]]);
    assert_eq!(runs(&out_home), [["app-server"]]);
    assert_eq!(
        runs(&old),
        [vec!["app-server"], vec!["login", "status"]],
        "the server did not say: `codex login status` does"
    );
    // Without `--live`, `codex login status` alone, as before.
    let before = sb.codex_invocations().len();
    let out = pick(&sb, false, &[]);
    assert_eq!(
        why_not(&out, "codex:out"),
        "not logged in (`codex login status`)"
    );
    let after: Vec<Vec<String>> = sb.codex_invocations()[before..]
        .iter()
        .map(|i| i.args.clone())
        .collect();
    assert_eq!(after.len(), 3, "{after:?}");
    assert!(
        after.iter().all(|args| args == &["login", "status"]),
        "{after:?}"
    );
}

/// `--live` asks each agent first; a failed query falls back to the cache, with a note.
#[test]
fn live_usage_first() {
    let (sb, max, _) = two_accounts("");
    let resets = jiff::Timestamp::from_second(now() + 3 * DAY)
        .unwrap()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .strftime("%b %-d at %-I:%M%P (UTC)")
        .to_string();
    sb.set_live_usage(
        Some(&max),
        &format!(
            "Current session: 5% used \u{b7} resets {resets}\n\
                  Current week (all models): 95% used \u{b7} resets {resets}\n"
        ),
    );
    let v: Value = serde_json::from_str(&pick(&sb, false, &["--live", "--json"]).stdout).unwrap();
    let max = v["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["account"] == "claude:max")
        .unwrap()
        .clone();
    assert_eq!(max["source"], "live");
    assert_eq!(max["stale"], false);
    assert!(
        max["why_not"]
            .as_str()
            .unwrap()
            .starts_with("Week (all models): 95% used")
    );
    // team's live query fails (no fixture): its cache decides.
    assert_eq!(v["account"], "claude:team");
    let out = pick(&sb, false, &["--live"]);
    assert!(
        out.stdout.contains(
            "live query failed (`claude -p /usage --no-session-persistence` \
                             exited with status 1"
        ),
        "{}",
        out.stdout
    );
    assert_eq!(field(&out, "usage"), "cached 10m ago");
    assert_eq!(
        sb.invocations()
            .iter()
            .filter(|i| i.args.first().map(String::as_str) == Some("-p"))
            .count(),
        6,
        "each claude account, `default` included, once per run"
    );
}

/// R10, R23 (review #14): a live answer read only in part is not the account's usage. Here
/// the week is used up, in words this version does not read: the cache decides, with a note,
/// instead of the session line alone (95% left) standing for the account.
#[test]
fn a_live_answer_read_in_part_falls_back_to_the_cache() {
    let (sb, max, _) = two_accounts("");
    sb.set_live_usage(
        Some(&max),
        "Current session: 5% used\nCurrent week (all models): limit reached\n",
    );
    let out = pick(&sb, false, &["--live", "--json"]);
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    let max = v["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["account"] == "claude:max")
        .unwrap_or_else(|| panic!("no claude:max: {v:#}"));
    assert_eq!(max["source"], "cached");
    assert_eq!(max["binding"], "Week (all models)");
    assert_eq!(max["headroom"], 60.0);
    let out = pick(&sb, false, &["--live"]);
    assert_eq!(field(&out, "account"), "claude:max");
    assert!(
        out.stdout
            .contains("live output not recognized; using cached usage"),
        "{}",
        out.stdout
    );
}

/// R10, R23: a live answer that tells no usage (claude 2.1.292: how the account is billed and no
/// usage line; or the session's cost, for an account not logged in to a subscription) falls
/// back to the cache like a failed query, the note giving its reason. The decision is the
/// cache's.
#[test]
fn a_live_answer_that_tells_no_usage_falls_back_with_its_reason() {
    for (said, note) in [
        (
            "You are currently using your subscription to power your Claude Code usage\n\n\
             What's contributing to your limits usage?\n",
            "live: no usage limits told for now; using cached usage",
        ),
        (
            "Total cost:            $0.0000\nTotal duration (API):  0s\n",
            "live: not logged in to a Claude subscription, or using an API key; using cached usage",
        ),
    ] {
        let (sb, max, _) = two_accounts("");
        sb.set_live_usage(Some(&max), said);
        let out = pick(&sb, false, &["--live", "--json"]);
        let v: Value = serde_json::from_str(&out.stdout).unwrap();
        let max = v["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["account"] == "claude:max")
            .unwrap_or_else(|| panic!("no claude:max: {v:#}"));
        assert_eq!(max["source"], "cached");
        assert_eq!(max["feasible"], true);
        assert_eq!(max["headroom"], 60.0);
        let out = pick(&sb, false, &["--live"]);
        assert_eq!(field(&out, "account"), "claude:max");
        assert!(out.stdout.contains(note), "{note}:\n{}", out.stdout);
        assert!(!out.stdout.contains("not recognized"), "{}", out.stdout);
    }
}

/// The `--json` report's fields.
#[test]
fn json_report() {
    let (sb, _, _) = two_accounts(
        "[pick]\nnotes = \"x\"\n\n[pick.claude]\nmodels = [\"claude-opus-5-5\"]\n\
         efforts = [\"medium\", \"high\"]\ndefault_effort = \"medium\"\n",
    );
    let mut response: Value = serde_json::from_str(&answer(
        "claude:account-1 / claude-opus-5-5",
        0.8,
        &[
            ("claude:account-1 / claude-opus-5-5", 0.85),
            ("claude:account-2 / claude-opus-5-5", 0.15),
        ],
    ))
    .unwrap();
    response["answers"]["effort_claude"] = json!({"type": "score", "score": 1.0,
        "confidence": 0.7, "legend": {"0": "medium", "1": "high"},
        "probabilities": {"0": 0.2, "1": 0.8}});
    sb.set_jev_response(&response.to_string());
    let out = pick(&sb, true, &["--json"]);
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "account",
            "candidates",
            "command",
            "decided_by",
            "effort",
            "effort_by",
            "jev",
            "model",
            "provider",
            "reason",
            "session"
        ]
    );
    assert_eq!(v["session"], Value::Null);
    assert_eq!(v["account"], "claude:max");
    assert_eq!(v["provider"], "claude");
    assert_eq!(v["effort"], "high");
    assert_eq!(v["effort_by"], "jev");
    assert_eq!(v["decided_by"], "jev");
    assert_eq!(v["reason"], Value::Null);
    assert_eq!(
        v["jev"],
        json!({"model": "jev-1.13.0", "confidence": 0.8, "effort_confidence": 0.7,
               "effort_error": null, "error": null})
    );
    assert_eq!(
        v["command"],
        json!([
            "remuda",
            "run",
            "claude:max",
            "--model",
            "claude-opus-5-5",
            "--effort",
            "high"
        ])
    );
    let c = &v["candidates"][1];
    let keys: Vec<&str> = c.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "account",
            "affine",
            "age_seconds",
            "binding",
            "default_model_windows",
            "feasible",
            "fetched_at",
            "headroom",
            "jev_probability",
            "model",
            "reset_passed",
            "resets_at",
            "rules_rank",
            "source",
            "stale",
            "why_not"
        ]
    );
    assert_eq!(c["account"], "claude:max");
    assert_eq!(c["feasible"], true);
    assert_eq!(c["headroom"], 60.0);
    assert_eq!(c["binding"], "Week (all models)");
    assert_eq!(c["source"], "cached");
    assert_eq!(c["reset_passed"], false);
    assert_eq!(c["rules_rank"], 1);
    assert_eq!(c["jev_probability"], 0.85);
    assert_eq!(c["affine"], false);
    let age = c["age_seconds"].as_i64().unwrap();
    assert!((600..700).contains(&age), "{age}");
    assert_eq!(v["candidates"][0]["account"], "claude:default");
    assert_eq!(v["candidates"][0]["feasible"], false);
}

/// `--run` launches the recommendation as `remuda run` does, the options before the user's.
#[test]
fn run_launches_claude_with_the_options() {
    let (sb, max, _) = two_accounts(
        "[pick.claude]\nmodels = [\"claude-opus-5-5\"]\nefforts = [\"high\", \"max\"]\n\
         default_effort = \"high\"\n",
    );
    let out = pick(&sb, false, &["--run", "--", "-p", "hello"]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    assert!(
        out.stderr
            .contains("remuda: pick: claude:max / claude-opus-5-5, effort high; decided by rules"),
        "{}",
        out.stderr
    );
    let inv = sb.only_invocation();
    assert_eq!(inv.config_dir.as_deref(), max.to_str());
    assert_eq!(
        inv.args[..6],
        [
            "--model",
            "claude-opus-5-5",
            "--effort",
            "high",
            "-p",
            "hello"
        ]
    );
    assert_eq!(inv.args[6], "--session-id");
    assert_eq!(inv.args.len(), 8);
    let log = sb.launches();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0]["account"], "claude:max");
    assert_eq!(log[0]["session_id"], inv.args[7]);
    assert_eq!(log[0]["injected"], true);
    assert_eq!(
        log[0]["args"],
        json!([
            "--model",
            "claude-opus-5-5",
            "--effort",
            "high",
            "-p",
            "hello"
        ])
    );
}

#[test]
fn run_launches_codex_with_the_options() {
    let sb = Sandbox::new();
    sb.install_codex();
    let work = sb.make_codex_home("c/work");
    sb.set_codex_login(Some(&work), "Logged in using ChatGPT");
    configure(
        &sb,
        &[("codex", "work", &work)],
        "[pick]\nexclude = [\"claude:default\"]\n\n[pick.codex]\nmodels = [\"gpt-6-astra\"]\n\
         efforts = [\"high\", \"xhigh\"]\ndefault_effort = \"xhigh\"\n",
    );
    let out = pick(
        &sb,
        false,
        &["--provider", "codex", "--run", "--", "fix the bug"],
    );
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    let runs: Vec<_> = sb
        .codex_invocations()
        .into_iter()
        .filter(|i| i.args.first().map(String::as_str) != Some("login"))
        .collect();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].codex_home.as_deref(), work.to_str());
    assert_eq!(
        runs[0].args,
        [
            "-m",
            "gpt-6-astra",
            "-c",
            "model_reasoning_effort=xhigh",
            "fix the bug"
        ]
    );
}

/// An option the user set is not injected; arguments that neither start a new session nor
/// name one to resume are refused before anything runs, with `--run` or without (R23
/// Resuming); `--run --json` is a usage error, and arguments after `--` no longer need `--run`.
#[test]
fn run_respects_the_users_options_and_refuses_unnamed_sessions() {
    let (sb, _, _) = two_accounts(&format!("{NOTES}{MODELS}"));
    let out = pick(&sb, false, &["--run", "--", "--model", "fable"]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    assert!(
        out.stderr.contains(
            "--model is already in the arguments; not injecting the recommended model \
             (claude-opus-5-5)"
        ),
        "{}",
        out.stderr
    );
    assert_eq!(sb.only_invocation().args[..2], ["--model", "fable"]);

    let id = "766560c5-0000-4000-8000-000000000000";
    for args in [
        &["-c"][..],
        &["--continue"],
        &["--resume"],
        &["--session-id", id],
        &["--resume", id, "-c"],
        // A fork, like a resume, names exactly one session.
        &["--resume", id, "--resume", "other", "--fork-session"],
    ] {
        for run in [true, false] {
            let (sb, _, _) = two_accounts(&format!("{NOTES}{MODELS}"));
            sb.set_jev_response(&answer("claude:account-1 / claude-opus-5-5", 0.9, &[]));
            let mut argv: Vec<&str> = if run { vec!["--run", "--"] } else { vec!["--"] };
            argv.extend(args);
            let out = pick(&sb, true, &argv);
            assert_eq!(out.code, Some(1), "{argv:?}: {}{}", out.stdout, out.stderr);
            assert!(
                out.stderr.contains(&format!(
                    "`pick --run` starts a new session; these arguments do not: {}",
                    args.join(" ")
                )),
                "{argv:?}: {}",
                out.stderr
            );
            assert!(sb.invocations().is_empty(), "{argv:?}");
            assert!(
                sb.curl_invocations().is_empty(),
                "refused before asking Jev: {argv:?}"
            );
        }
    }

    for args in [&["--run", "--json"][..], &["--print-request", "--run"]] {
        assert_eq!(pick(&sb, false, args).code, Some(2), "{args:?}");
    }
    // A new session's arguments without `--run`: shown in the command, nothing launched.
    let (sb, _, _) = two_accounts(&format!("{NOTES}{MODELS}"));
    for args in [&["--", "x"][..], &["--", "-p", "hi"]] {
        let out = pick(&sb, false, args);
        assert_eq!(out.code, Some(0), "{args:?}: {}{}", out.stdout, out.stderr);
        assert_eq!(
            field(&out, "command"),
            format!(
                "remuda run claude:max --model claude-opus-5-5 {}",
                args[1..].join(" ")
            )
        );
    }
    let out = pick(&sb, false, &["--json", "--", "-p", "hi"]);
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(
        v["command"],
        json!([
            "remuda",
            "run",
            "claude:max",
            "--model",
            "claude-opus-5-5",
            "-p",
            "hi"
        ])
    );
    assert_eq!(v["session"], Value::Null);
    assert!(sb.invocations().is_empty());
    assert!(
        !sb.launch_log().exists(),
        "nothing launched, nothing logged"
    );
}

const SESSION: &str = "766560c5-0000-4000-8000-000000000000";

/// The claude launches so far: every invocation but `remuda sessions`' `agents --json`.
fn launched(sb: &Sandbox) -> Vec<common::Invocation> {
    sb.invocations()
        .into_iter()
        .filter(|i| i.args.first().map(String::as_str) != Some("agents"))
        .collect()
}

/// A claude transcript record of `kind` in `cwd`, `ago` seconds before now; an assistant
/// record names `model`.
fn record(kind: &str, cwd: &str, ago: i64, model: &str) -> String {
    let mut v = json!({"type": kind, "cwd": cwd, "timestamp": iso(-ago),
                       "sessionId": SESSION, "entrypoint": "cli"});
    v["message"] = match kind {
        "assistant" => json!({"id": "msg_1", "role": "assistant", "model": model,
                              "content": [{"type": "text", "text": "done"}]}),
        _ => json!({"role": "user", "content": "Refactor the parser"}),
    };
    format!("{v}\n")
}

/// Writes transcript `id` into `projects`, its last record `ago` seconds old, in `cwd`.
fn write_transcript(projects: &Path, id: &str, cwd: &str, ago: i64, model: &str) {
    write_transcript_in(projects, "-w-proj", id, cwd, ago, model);
}

/// [`write_transcript`] into the project directory `project`.
fn write_transcript_in(projects: &Path, project: &str, id: &str, cwd: &str, ago: i64, model: &str) {
    let dir = projects.join(project);
    std::fs::create_dir_all(&dir).unwrap();
    let text = [
        record("user", cwd, ago + 600, ""),
        record("assistant", cwd, ago + 590, model),
        record("user", cwd, ago, ""),
    ]
    .concat();
    std::fs::write(dir.join(format!("{id}.jsonl")), text).unwrap();
}

/// The home `config.toml` registers `account` (`provider:name`) with, as written there.
fn home_of(sb: &Sandbox, account: &str) -> String {
    let (provider, name) = account.split_once(':').unwrap();
    let config = sb.read_config();
    let key = format!("provider = \"{provider}\"\nname = \"{name}\"\nhome = \"");
    let at = config
        .find(&key)
        .unwrap_or_else(|| panic!("{account} in {config}"))
        + key.len();
    config[at..].split('"').next().unwrap().to_string()
}

/// Appends a launch record to the launch log, with the home the account is registered with.
fn log_launch(
    sb: &Sandbox,
    account: &str,
    ago: i64,
    cwd: &str,
    session: &str,
    fork_of: Option<&str>,
) {
    let home = home_of(sb, account);
    log_launch_as(sb, account, &home, ago, cwd, session, fork_of);
}

/// Appends a launch record to the launch log, as `account` launched with `home`.
fn log_launch_as(
    sb: &Sandbox,
    account: &str,
    home: &str,
    ago: i64,
    cwd: &str,
    session: &str,
    fork_of: Option<&str>,
) {
    let log = sb.launch_log();
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    let mut record = json!({"ts": iso(-ago), "account": account, "home": home, "cwd": cwd,
                            "args": ["--resume", session], "session_id": session,
                            "injected": false});
    if let Some(of) = fork_of {
        record["fork_of"] = json!(of);
    }
    let mut text = std::fs::read_to_string(&log).unwrap_or_default();
    text.push_str(&format!("{record}\n"));
    std::fs::write(&log, text).unwrap();
}

/// [`two_accounts`] sharing one `projects` (R18), which holds [`SESSION`]: `max` has more
/// headroom, `team` less. The index is built (`remuda sessions`) unless `index` is false.
fn shared_store(pick: &str, ago: i64, index: bool) -> (Sandbox, PathBuf) {
    let (sb, max, team) = two_accounts(pick);
    let shared = sb.root().join("shared-projects");
    std::fs::create_dir_all(&shared).unwrap();
    for home in [&max, &team] {
        std::os::unix::fs::symlink(&shared, home.join("projects")).unwrap();
    }
    write_transcript(&shared, SESSION, "/w/proj", ago, "claude-opus-5-5");
    if index {
        sb.remuda().arg("sessions").assert().success();
    }
    (sb, shared)
}

/// R23 (Resuming): the account that ran the session lately is recommended to resume it, ahead
/// of more headroom; nothing is injected, nothing is logged without `--run`, and Jev reads the
/// warm account by alias, never the session.
#[test]
fn resume_prefers_the_account_that_ran_it() {
    // Models and efforts are configured: resuming, none of them is injected.
    let (sb, _) = shared_store(
        &format!("{NOTES}{MODELS}efforts = [\"high\", \"max\"]\ndefault_effort = \"high\"\n"),
        300,
        true,
    );
    // Launched three hours ago; the transcript was written to five minutes ago, there.
    log_launch(&sb, "claude:team", 3 * 3600, "/w/proj", SESSION, None);
    let before = std::fs::read_to_string(sb.launch_log()).unwrap();
    let out = pick(&sb, false, &["--", "--resume", SESSION]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    assert_eq!(field(&out, "account"), "claude:team");
    assert_eq!(field(&out, "model"), "the session's (claude-opus-5-5)");
    assert_eq!(field(&out, "effort"), "agent default");
    // `5m ago`, or `6m` should the run cross a minute.
    let session = field(&out, "session");
    assert!(
        session.starts_with(&format!("resume {SESSION}: claude:team ran it "))
            && session.ends_with(" ago (prompt cache warm: preferred); model claude-opus-5-5"),
        "{session}"
    );
    assert_eq!(
        field(&out, "command"),
        format!("remuda run claude:team --resume {SESSION}")
    );
    // `$HOME/.claude` has no `projects`: the default account cannot see the session.
    assert_eq!(
        why_not(&out, "claude:default"),
        "cannot see this session (its store is not this account's)"
    );
    assert_eq!(
        std::fs::read_to_string(sb.launch_log()).unwrap(),
        before,
        "nothing is logged without --run"
    );
    assert!(launched(&sb).is_empty());

    let out = pick(&sb, false, &["--json", "--", "--resume", SESSION]);
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    let keys: Vec<&str> = v["session"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "affine",
            "age_seconds",
            "id",
            "indexed",
            "kind",
            "last_account",
            "last_active_at",
            "model"
        ]
    );
    assert_eq!(v["session"]["id"], SESSION);
    assert_eq!(v["session"]["kind"], "resume");
    assert_eq!(v["session"]["last_account"], "claude:team");
    assert_eq!(v["session"]["affine"], true);
    assert_eq!(v["session"]["indexed"], true);
    assert_eq!(v["session"]["model"], "claude-opus-5-5");
    let age = v["session"]["age_seconds"].as_i64().unwrap();
    assert!(
        (300..360).contains(&age),
        "the transcript's time, not the launch's: {age}"
    );
    assert_eq!(v["model"], Value::Null);
    assert_eq!(v["effort"], Value::Null);
    assert_eq!(
        v["command"],
        json!(["remuda", "run", "claude:team", "--resume", SESSION])
    );
    let affine: Vec<(&str, bool, bool)> = v["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["account"].as_str().unwrap(),
                c["affine"].as_bool().unwrap(),
                c["feasible"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        affine,
        [
            ("claude:default", false, false),
            ("claude:max", false, true),
            ("claude:team", true, true)
        ]
    );

    // Jev reads the warm account by alias, and nothing of the session.
    let out = pick(&sb, true, &["--print-request", "--", "--resume", SESSION]);
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    let body = out.stdout;
    assert!(
        body.contains("resuming a session: claude:account-2 ran the session to be resumed ")
            && body.contains(" minutes ago (its prompt cache is warm)"),
        "{body}"
    );
    for leak in [
        SESSION,
        "766560c5",
        "/w/proj",
        "Refactor the parser",
        "claude:team",
        sb.root().to_str().unwrap(),
    ] {
        assert!(!body.contains(leak), "{leak:?} in {body}");
    }
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(
        v["questions"].get("effort_claude").is_none(),
        "no effort when resuming"
    );
}

/// R23 (Resuming): the last activity is the later of the launch and the transcript's last
/// record, the latter only when written in the launch's directory; past `affinity_minutes`
/// nothing is preferred, and the rules decide as without a session.
#[test]
fn the_last_activity_and_the_window() {
    // The transcript was written to before the launch: the launch counts.
    let (sb, _) = shared_store(&format!("{NOTES}{MODELS}"), 2 * 3600, true);
    log_launch(&sb, "claude:team", 600, "/w/proj", SESSION, None);
    let out = pick(&sb, false, &["--json", "--", "--resume", SESSION]);
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    let age = v["session"]["age_seconds"].as_i64().unwrap();
    assert!((600..660).contains(&age), "the launch's time: {age}");
    assert_eq!(v["account"], "claude:team");

    // Written to lately, but elsewhere than the launch: only the launch counts, three hours ago.
    let (sb, _) = shared_store(&format!("{NOTES}{MODELS}"), 300, true);
    log_launch(
        &sb,
        "claude:team",
        3 * 3600,
        "/somewhere/else",
        SESSION,
        None,
    );
    let out = pick(&sb, false, &["--", "--resume", SESSION]);
    assert_eq!(field(&out, "account"), "claude:max");
    assert_eq!(
        field(&out, "session"),
        format!(
            "resume {SESSION}: claude:team ran it 3h ago (more than 60 minutes ago: its prompt \
             cache has likely expired); model claude-opus-5-5"
        )
    );
    // A longer window takes it in; 0 turns affinity off.
    let (sb, _) = shared_store(
        &format!("[pick]\nnotes = \"x\"\naffinity_minutes = 240\n\n{MODELS}"),
        300,
        true,
    );
    log_launch(
        &sb,
        "claude:team",
        3 * 3600,
        "/somewhere/else",
        SESSION,
        None,
    );
    assert_eq!(
        field(&pick(&sb, false, &["--", "--resume", SESSION]), "account"),
        "claude:team"
    );
    let (sb, _) = shared_store(
        &format!("[pick]\nnotes = \"x\"\naffinity_minutes = 0\n\n{MODELS}"),
        300,
        true,
    );
    log_launch(&sb, "claude:team", 60, "/w/proj", SESSION, None);
    let out = pick(&sb, false, &["--", "--resume", SESSION]);
    assert_eq!(field(&out, "account"), "claude:max");
    assert!(
        field(&out, "session").contains("(affinity off: [pick] affinity_minutes = 0)"),
        "{}",
        out.stdout
    );

    // Never launched through remuda: no account to prefer.
    let (sb, _) = shared_store(&format!("{NOTES}{MODELS}"), 300, true);
    let out = pick(&sb, false, &["--", "--resume", SESSION]);
    assert_eq!(field(&out, "account"), "claude:max");
    assert_eq!(
        field(&out, "session"),
        format!(
            "resume {SESSION}: not launched through remuda: no account's prompt cache is known \
             to be warm; model claude-opus-5-5"
        )
    );
}

/// R23 (Resuming): a launch that forked the session counts for it (the fork read all of it),
/// with the fork's own last activity; and `--resume <id> --fork-session` is a fork.
#[test]
fn a_fork_of_the_session_counts() {
    let (sb, shared) = shared_store(&format!("{NOTES}{MODELS}"), 5 * 3600, true);
    let fork = "f0f0f0f0-0000-4000-8000-000000000000";
    write_transcript(&shared, fork, "/w/proj", 120, "claude-opus-5-5");
    sb.remuda().arg("sessions").assert().success();
    log_launch(&sb, "claude:max", 4 * 3600, "/w/proj", SESSION, None);
    log_launch(&sb, "claude:team", 3 * 3600, "/w/proj", fork, Some(SESSION));
    let out = pick(
        &sb,
        false,
        &["--json", "--", "--resume", SESSION, "--fork-session"],
    );
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(v["session"]["kind"], "fork");
    assert_eq!(v["session"]["last_account"], "claude:team");
    let age = v["session"]["age_seconds"].as_i64().unwrap();
    assert!((120..180).contains(&age), "the fork's last record: {age}");
    assert_eq!(v["account"], "claude:team");
    assert_eq!(
        v["command"],
        json!([
            "remuda",
            "run",
            "claude:team",
            "--resume",
            SESSION,
            "--fork-session"
        ])
    );
}

/// R23 (Resuming): affinity orders, it never admits. A warm account that is excluded is not
/// recommended, and the output says what resuming elsewhere costs.
#[test]
fn a_warm_account_that_is_not_feasible_is_named() {
    let (sb, _) = shared_store(
        &format!("[pick]\nnotes = \"x\"\nexclude = [\"claude:team\"]\n\n{MODELS}"),
        300,
        true,
    );
    log_launch(&sb, "claude:team", 3600, "/w/proj", SESSION, None);
    let out = pick(&sb, false, &["--", "--resume", SESSION]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    assert_eq!(field(&out, "account"), "claude:max");
    assert!(
        out.stdout.contains("claude:team ran this session ")
            && out.stdout.contains(
                " ago, but excluded ([pick] exclude); resuming as another account rewrites its \
                 prompt cache"
            ),
        "{}",
        out.stdout
    );
}

/// R23 (Resuming): an account whose store does not hold the session cannot resume it; where
/// the session index does not have the session, no account is held back, and the output says
/// so. The facts are read from the index, never by scanning the stores.
#[test]
fn accounts_that_cannot_see_the_session() {
    let (sb, max, team) = two_accounts(&format!("{NOTES}{MODELS}"));
    write_transcript(
        &team.join("projects"),
        SESSION,
        "/w/proj",
        300,
        "claude-fable-5-1",
    );
    std::fs::create_dir_all(max.join("projects")).unwrap();
    // Not indexed yet: every account is offered, and the model is unknown.
    let out = pick(&sb, false, &["--", "--resume", SESSION]);
    assert_eq!(field(&out, "account"), "claude:max");
    assert!(
        out.stdout.contains(
            "not in the session index: every account is offered, whether or not it can see \
             the session (`remuda sessions` indexes it)"
        ),
        "{}",
        out.stdout
    );
    assert!(
        field(&out, "session").ends_with("; model unknown"),
        "{}",
        out.stdout
    );

    sb.remuda().arg("sessions").assert().success();
    let out = pick(&sb, false, &["--", "--resume", SESSION]);
    assert_eq!(field(&out, "account"), "claude:team");
    assert_eq!(
        why_not(&out, "claude:max"),
        "cannot see this session (its store is not this account's)"
    );
    assert!(
        field(&out, "session").ends_with("; model claude-fable-5-1"),
        "{}",
        out.stdout
    );
    // The provider is the session's: a codex `--provider` conflicts.
    let out = pick(
        &sb,
        false,
        &["--provider", "codex", "--", "--resume", SESSION],
    );
    assert_eq!(out.code, Some(1));
    assert!(
        out.stderr
            .contains("--provider codex, but the arguments resume a claude session"),
        "{}",
        out.stderr
    );
}

/// R23 (Resuming): an index entry whose file is gone, or is not the session's by its name, is
/// not read: the launch's time counts, the model is unknown, and no account is held back.
#[test]
fn index_entries_that_are_gone_or_not_the_session_are_ignored() {
    // Gone since it was indexed.
    let (sb, shared) = shared_store(&format!("{NOTES}{MODELS}"), 300, true);
    log_launch(&sb, "claude:team", 3 * 3600, "/w/proj", SESSION, None);
    std::fs::remove_file(shared.join("-w-proj").join(format!("{SESSION}.jsonl"))).unwrap();
    let out = pick(&sb, false, &["--json", "--", "--resume", SESSION]);
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(v["session"]["model"], Value::Null);
    assert_eq!(v["session"]["indexed"], false);
    assert!(v["session"]["age_seconds"].as_i64().unwrap() >= 3 * 3600);
    assert_eq!(v["session"]["affine"], false);
    assert_eq!(v["account"], "claude:max");

    // The entry names another file, written to lately: not the session's.
    let (sb, shared) = shared_store(&format!("{NOTES}{MODELS}"), 3 * 3600, true);
    log_launch(&sb, "claude:team", 3 * 3600, "/w/proj", SESSION, None);
    let other = "0e0e0e0e-0000-4000-8000-000000000000";
    write_transcript(&shared, other, "/w/proj", 60, "claude-fable-5-1");
    let index = sb.remuda_home().join("state").join("index.json");
    let text = std::fs::read_to_string(&index).unwrap();
    assert!(text.contains(&format!("{SESSION}.jsonl")));
    std::fs::write(
        &index,
        text.replace(&format!("{SESSION}.jsonl"), &format!("{other}.jsonl")),
    )
    .unwrap();
    let out = pick(&sb, false, &["--json", "--", "--resume", SESSION]);
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(
        v["session"]["model"],
        Value::Null,
        "not the other file's model"
    );
    let age = v["session"]["age_seconds"].as_i64().unwrap();
    assert!(age >= 3 * 3600, "not the other file's time: {age}");
    assert_eq!(v["account"], "claude:max");
}

/// R23 (Resuming): with stores that are not shared, each account has its own copy of the
/// session. The last account's activity is that of the copy in its own store, and each
/// account's model that of the copy it would resume: another account's copy, written to a
/// minute ago with another model, neither warms it nor frees its windows.
#[test]
fn independent_copies_are_each_their_accounts() {
    let sb = Sandbox::new();
    let max = sb.make_claude_home("h/max");
    let team = sb.make_claude_home("h/team");
    sb.write_claude_json(
        Some(&max),
        &claude_json(
            "max@example.com",
            600,
            &[limit("weekly_all", 40.0, 3 * DAY)],
        ),
    );
    sb.write_claude_json(
        Some(&team),
        &claude_json(
            "team@example.com",
            600,
            &[
                limit("weekly_all", 20.0, 3 * DAY),
                limit("Fable", 100.0, 3 * DAY),
            ],
        ),
    );
    configure(
        &sb,
        &[("claude", "max", &max), ("claude", "team", &team)],
        &format!("{NOTES}{MODELS}"),
    );
    // max's copy: Opus, written to a minute ago; team's: Fable, idle for three hours.
    write_transcript(
        &max.join("projects"),
        SESSION,
        "/w/proj",
        60,
        "claude-opus-5-5",
    );
    write_transcript(
        &team.join("projects"),
        SESSION,
        "/w/proj",
        3 * 3600,
        "claude-fable-5-1",
    );
    sb.remuda().arg("sessions").assert().success();
    log_launch(&sb, "claude:max", 4 * 3600, "/w/proj", SESSION, None);
    log_launch(&sb, "claude:team", 3 * 3600, "/w/proj", SESSION, None);

    let out = pick(&sb, false, &["--json", "--", "--resume", SESSION]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(v["session"]["last_account"], "claude:team");
    let age = v["session"]["age_seconds"].as_i64().unwrap();
    assert!(age >= 3 * 3600, "team's own copy, not max's: {age}");
    assert_eq!(v["session"]["affine"], false);
    assert_eq!(v["account"], "claude:max");
    assert_eq!(v["session"]["model"], "claude-opus-5-5", "max's copy");
    let team = &v["candidates"][2];
    assert_eq!(team["account"], "claude:team");
    assert_eq!(team["feasible"], false);
    assert!(
        team["why_not"]
            .as_str()
            .unwrap()
            .starts_with("Week (Fable): 100% used"),
        "team resumes its Fable copy: {team}"
    );
}

/// R23 (Resuming): a store may hold several copies of a session (one per project directory).
/// The one written to last is the one it goes on in: its activity and its model, whatever the
/// order of their paths.
#[test]
fn of_copies_in_one_store_the_latest_counts() {
    let (sb, shared) = shared_store(&format!("{NOTES}{MODELS}"), 60, false);
    // The current copy (`-w-proj`, a minute ago, Fable) and an old one that sorts first.
    write_transcript(&shared, SESSION, "/w/proj", 60, "claude-fable-5-1");
    write_transcript_in(
        &shared,
        "-a-old-project",
        SESSION,
        "/w/proj",
        3 * 3600,
        "claude-opus-5-5",
    );
    sb.remuda().arg("sessions").assert().success();
    log_launch(&sb, "claude:team", 3 * 3600, "/w/proj", SESSION, None);
    let out = pick(&sb, false, &["--json", "--", "--resume", SESSION]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    let age = v["session"]["age_seconds"].as_i64().unwrap();
    assert!(age < 600, "the current copy's activity: {age}");
    assert_eq!(v["session"]["affine"], true);
    assert_eq!(v["session"]["model"], "claude-fable-5-1");
    assert_eq!(v["account"], "claude:team");
}

/// R23 (Resuming, The request): a per-model window's label may name the session's model (here
/// a path, as the quota names it and the transcript too). Resuming, Jev reads the windows by
/// their kind only: the label is not sent; locally the window applies as named.
#[test]
fn a_quota_label_naming_the_session_model_is_never_sent() {
    let secret = "/Users/private_client/projects/SECRET_MODEL";
    let (sb, shared) = shared_store(&format!("{NOTES}{MODELS}"), 300, false);
    write_transcript(&shared, SESSION, "/w/proj", 300, secret);
    for (account, used) in [("claude:max", 40.0), ("claude:team", 80.0)] {
        let home = PathBuf::from(home_of(&sb, account));
        sb.write_claude_json(
            Some(&home),
            &claude_json(
                "x@example.com",
                600,
                &[
                    limit("weekly_all", used, 3 * DAY),
                    limit(secret, 50.0, 3 * DAY),
                ],
            ),
        );
    }
    sb.remuda().arg("sessions").assert().success();
    log_launch(&sb, "claude:team", 3 * 3600, "/w/proj", SESSION, None);
    let out = pick(&sb, true, &["--print-request", "--", "--resume", SESSION]);
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    for leak in [secret, "SECRET_MODEL", "private_client"] {
        assert!(!out.stdout.contains(leak), "{leak:?} in {}", out.stdout);
    }
    assert!(
        out.stdout
            .contains("weekly window of the session's model: 50% used"),
        "{}",
        out.stdout
    );
    // Locally the window applies: it binds max (50% left, less than its general 60%).
    let out = pick(&sb, false, &["--json", "--", "--resume", SESSION]);
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    let max = &v["candidates"][1];
    assert_eq!(max["account"], "claude:max");
    assert_eq!(max["binding"], format!("Week ({secret})"));
}

/// R23 (Resuming), R2: an account registered again under the same name with another home is
/// another login, whose cache never held the session: it is not warm, and nothing is preferred.
#[test]
fn a_name_registered_again_with_another_home_is_not_warm() {
    let (sb, _) = shared_store(&format!("{NOTES}{MODELS}"), 300, true);
    // Run as claude:team ten minutes ago, when team was another home: warm by the time alone.
    let old = sb.root().join("h/team-before");
    log_launch_as(
        &sb,
        "claude:team",
        old.to_str().unwrap(),
        600,
        "/w/proj",
        SESSION,
        None,
    );
    let out = pick(&sb, false, &["--", "--resume", SESSION]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    assert_eq!(field(&out, "account"), "claude:max");
    assert!(
        field(&out, "session").contains(
            "(registered now with another home: another login, whose cache does not hold it)"
        ),
        "{}",
        out.stdout
    );
    // The same home, spelled with a trailing slash, is another home string (R2).
    let (sb, _) = shared_store(&format!("{NOTES}{MODELS}"), 300, true);
    let home = format!("{}/", home_of(&sb, "claude:team"));
    log_launch_as(&sb, "claude:team", &home, 600, "/w/proj", SESSION, None);
    let out = pick(&sb, false, &["--json", "--", "--resume", SESSION]);
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(v["session"]["affine"], false);
    assert_eq!(v["account"], "claude:max");
    // Byte for byte the registered home: warm.
    let (sb, _) = shared_store(&format!("{NOTES}{MODELS}"), 300, true);
    log_launch(&sb, "claude:team", 600, "/w/proj", SESSION, None);
    let out = pick(&sb, false, &["--json", "--", "--resume", SESSION]);
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(v["session"]["affine"], true);
    assert_eq!(v["account"], "claude:team");
}

/// R23 (Resuming, Output): with nothing feasible, the text still says who ran the session and
/// why it cannot be resumed as that account.
#[test]
fn nothing_feasible_still_tells_the_session() {
    let (sb, _) = shared_store(
        &format!("[pick]\nnotes = \"x\"\nexclude = [\"claude:team\", \"claude:max\"]\n\n{MODELS}"),
        300,
        true,
    );
    log_launch(&sb, "claude:team", 3600, "/w/proj", SESSION, None);
    let out = pick(&sb, false, &["--", "--resume", SESSION]);
    assert_eq!(out.code, Some(1), "{}{}", out.stdout, out.stderr);
    let session = field(&out, "session");
    assert!(
        session.starts_with(&format!("resume {SESSION}: claude:team ran it "))
            && session.ends_with(" ago (prompt cache warm: preferred); model claude-opus-5-5"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout.contains(" ago, but excluded ([pick] exclude); resuming as another account rewrites its prompt cache"),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains("\nnot feasible:\n"), "{}", out.stdout);
}

/// R23 (Resuming, `--run`): launching another account than the warm one, stderr says who ran
/// the session, why not that account, and what resuming elsewhere costs.
#[test]
fn run_tells_why_not_the_warm_account() {
    let (sb, _) = shared_store(
        &format!("[pick]\nnotes = \"x\"\nexclude = [\"claude:team\"]\n\n{MODELS}"),
        300,
        true,
    );
    log_launch(&sb, "claude:team", 3600, "/w/proj", SESSION, None);
    let out = pick(&sb, false, &["--run", "--", "--resume", SESSION]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    assert!(
        out.stderr.contains(&format!(
            "remuda: pick: session resume {SESSION}: claude:team ran it "
        )),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr
            .contains("remuda: pick: claude:team ran this session ")
            && out.stderr.contains(
                " ago, but excluded ([pick] exclude); resuming as another account rewrites its \
                 prompt cache\n"
            ),
        "{}",
        out.stderr
    );
    assert_eq!(launched(&sb)[0].args, ["--resume", SESSION]);
    assert_eq!(
        launched(&sb)[0].config_dir.as_deref(),
        Some(home_of(&sb, "claude:max").as_str())
    );
}

/// R23 (Output): the command shown, run by a shell, gives the agent the very arguments given
/// after `--`.
#[test]
fn the_command_shown_replays_as_the_same_arguments() {
    let (sb, _, _) = two_accounts(&format!("{NOTES}{MODELS}"));
    let args = [
        "-p",
        "Please print $(printf CHANGED) with spaces",
        "",
        "it's \"quoted\" `x` \\ *",
    ];
    let mut argv = vec!["--"];
    argv.extend(args);
    let out = pick(&sb, false, &argv);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    let command = field(&out, "command").to_string();
    assert!(
        command.starts_with("remuda run claude:max --model claude-opus-5-5 -p "),
        "{command}"
    );
    let status = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("remuda() {{ \"$REMUDA_BIN\" \"$@\"; }}; {command}"))
        .env_clear()
        .env("REMUDA_BIN", env!("CARGO_BIN_EXE_remuda"))
        .env("HOME", sb.home())
        .env("REMUDA_HOME", sb.remuda_home())
        .env("PATH", sb.path_var())
        .env("FAKE_CLAUDE_OUT", sb.claude_out())
        .env("FAKE_CODEX_OUT", sb.codex_out())
        .env("FAKE_CURL_OUT", sb.curl_out())
        .env("TZ", "UTC")
        .current_dir(sb.work())
        .status()
        .unwrap();
    assert!(status.success(), "{command}");
    let inv = sb.only_invocation();
    let mut want = vec!["--model", "claude-opus-5-5"];
    want.extend(args);
    assert_eq!(inv.args[..want.len()], want, "{command}");
}

/// R23 (The request, Resuming): a transcript's model is any text; a path there is shown on the
/// user's terminal and never sent.
#[test]
fn a_transcript_model_is_never_sent() {
    let (sb, shared) = shared_store(&format!("{NOTES}{MODELS}"), 300, false);
    let secret = "/Users/private-client/projects/SECRET-MODEL";
    write_transcript(&shared, SESSION, "/w/proj", 300, secret);
    sb.remuda().arg("sessions").assert().success();
    log_launch(&sb, "claude:team", 3600, "/w/proj", SESSION, None);
    let out = pick(&sb, true, &["--print-request", "--", "--resume", SESSION]);
    assert_eq!(out.code, Some(0), "{}", out.stderr);
    for leak in [secret, "SECRET-MODEL", "private-client"] {
        assert!(!out.stdout.contains(leak), "{leak:?} in {}", out.stdout);
    }
    let out = pick(&sb, false, &["--", "--resume", SESSION]);
    assert_eq!(field(&out, "model"), format!("the session's ({secret})"));
}

/// R23 (`--run`, Resuming): a resume is launched exactly as `remuda run <account> --resume
/// <id>`: the same arguments, nothing injected, logged with the session's id.
#[test]
fn run_resumes_exactly_as_remuda_run() {
    let (sb, _) = shared_store(&format!("{NOTES}{MODELS}"), 300, true);
    log_launch(&sb, "claude:team", 3600, "/w/proj", SESSION, None);
    let out = pick(&sb, false, &["--run", "--", "--resume", SESSION]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    assert!(
        out.stderr.contains(&format!(
            "remuda: pick: claude:team / default, effort agent default; decided by rules \
             (no_key: TYPESAFE_API_KEY is not set); resume {SESSION} (its prompt cache is warm)"
        )),
        "{}",
        out.stderr
    );
    let picked = launched(&sb).remove(0);
    assert_eq!(launched(&sb).len(), 1);
    assert_eq!(picked.args, ["--resume", SESSION]);
    let log = sb.launches();
    assert_eq!(log.len(), 2);
    assert_eq!(log[1]["account"], "claude:team");
    assert_eq!(log[1]["session_id"], SESSION);
    assert_eq!(log[1]["injected"], false);
    assert_eq!(log[1]["args"], json!(["--resume", SESSION]));

    // `remuda run` itself: the same launch.
    let (sb2, _) = shared_store(&format!("{NOTES}{MODELS}"), 300, true);
    sb2.remuda()
        .args(["run", "claude:team", "--resume", SESSION])
        .assert()
        .success();
    let run = launched(&sb2).remove(0);
    assert_eq!(run.args, picked.args);
    assert_eq!(
        run.config_dir
            .as_deref()
            .map(|d| d.replace(sb2.root().to_str().unwrap(), "")),
        picked
            .config_dir
            .as_deref()
            .map(|d| d.replace(sb.root().to_str().unwrap(), ""))
    );
}

/// R23 (Resuming, R17): codex `resume <id>` is passed as given, without `-m`, `-c` or `-C`,
/// to the codex account that ran it.
#[test]
fn run_resumes_codex_as_given() {
    use common::rollouts::{meta, model_turn, user, write_rollout};
    let sb = Sandbox::new();
    sb.install_codex();
    let work = sb.make_codex_home("c/work");
    let other = sb.make_codex_home("c/other");
    for home in [&work, &other] {
        sb.set_codex_login(Some(home), "Logged in using ChatGPT");
        std::fs::create_dir_all(home.join("sessions")).unwrap();
    }
    configure(
        &sb,
        &[("codex", "work", &work), ("codex", "other", &other)],
        "[pick]\nexclude = [\"claude:default\"]\n\n[pick.codex]\nmodels = [\"gpt-6-astra\"]\n\
         efforts = [\"high\", \"xhigh\"]\ndefault_effort = \"xhigh\"\n",
    );
    let id = "019a0000-0000-7000-8000-000000000001";
    write_rollout(
        &work,
        id,
        &[
            meta(id, "/w/proj", json!("cli"), 10, &iso(-900)),
            user(&["fix the bug"], &iso(-890)),
            model_turn("gpt-6-sol", &iso(-120)),
        ]
        .concat(),
    );
    sb.remuda().arg("sessions").assert().success();
    log_launch(&sb, "codex:work", 900, "/w/proj", id, None);
    let out = pick(&sb, false, &["--", "resume", id]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    assert_eq!(field(&out, "account"), "codex:work");
    assert_eq!(field(&out, "model"), "the session's (gpt-6-sol)");
    assert_eq!(
        why_not(&out, "codex:other"),
        "cannot see this session (its store is not this account's)"
    );
    assert_eq!(why_not(&out, "claude:default"), "excluded ([pick] exclude)");
    let out = pick(&sb, false, &["--run", "--", "resume", id]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    let runs: Vec<_> = sb
        .codex_invocations()
        .into_iter()
        .filter(|i| i.args.first().map(String::as_str) == Some("resume"))
        .collect();
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0].args, ["resume", id]);
    assert_eq!(runs[0].codex_home.as_deref(), work.to_str());
    let log = sb.launches();
    assert_eq!(log.last().unwrap()["session_id"], id);
    assert_eq!(log.last().unwrap()["args"], json!(["resume", id]));
}

/// One feasible pair with efforts to score: only the effort is asked, the rules keep the pair,
/// and the output says Jev chose only the effort.
#[test]
fn one_option_asks_jev_only_the_effort() {
    let (sb, _, _) = two_accounts(
        "[pick]\nnotes = \"x\"\nexclude = [\"team\"]\n\n[pick.claude]\n\
         efforts = [\"medium\", \"high\", \"max\"]\ndefault_effort = \"medium\"\n",
    );
    sb.set_jev_response(
        r#"{"model": "jev-1.13.0", "answers": {"effort_claude": {"type": "score", "score": 2.0,
            "confidence": 0.9, "probabilities": {"0": 0.0, "1": 0.1, "2": 0.9}}}}"#,
    );
    let out = pick(&sb, true, &[]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    assert_eq!(field(&out, "account"), "claude:max");
    assert_eq!(field(&out, "effort"), "max (jev, confidence 0.90)");
    assert_eq!(
        field(&out, "decided by"),
        "rules (single_option: only one option; jev was asked only the effort)"
    );
    let body: Value = serde_json::from_str(&sb.jev_request_body()).unwrap();
    assert!(body["questions"].get("launch").is_none(), "{body:#}");
    assert_eq!(body["questions"]["effort_claude"]["type"], "score");
    let v: Value = serde_json::from_str(&pick(&sb, true, &["--json"]).stdout).unwrap();
    assert_eq!(
        (&v["decided_by"], &v["reason"], &v["effort_by"]),
        (&json!("rules"), &json!("single_option"), &json!("jev"))
    );
    assert_eq!(v["jev"]["confidence"], Value::Null);
}

/// An unusable effort answer costs only the effort: the pair is still Jev's, the effort is
/// `default_effort`, and the problem is reported.
#[test]
fn a_bad_effort_answer_keeps_the_launch() {
    let (sb, _, _) = two_accounts(&format!(
        "{NOTES}{MODELS}efforts = [\"medium\", \"high\"]\ndefault_effort = \"medium\"\n"
    ));
    let mut response: Value = serde_json::from_str(&answer(
        "claude:account-2 / claude-opus-5-5",
        0.9,
        &[("claude:account-2 / claude-opus-5-5", 0.95)],
    ))
    .unwrap();
    response["answers"]["effort_claude"] = json!({"type": "score", "score": 7, "confidence": 0.9});
    sb.set_jev_response(&response.to_string());
    let out = pick(&sb, true, &[]);
    assert_eq!(field(&out, "account"), "claude:team");
    assert_eq!(field(&out, "decided by"), "jev (confidence 0.90)");
    assert_eq!(
        field(&out, "effort"),
        "medium (jev's effort answer is unusable: `effort_claude.score` is not a number from 0 \
         to 1)"
    );
    let v: Value = serde_json::from_str(&pick(&sb, true, &["--json"]).stdout).unwrap();
    assert_eq!(v["effort"], "medium");
    assert_eq!(v["effort_by"], "rules");
    assert_eq!(
        v["jev"]["effort_error"],
        "`effort_claude.score` is not a number from 0 to 1"
    );
    assert_eq!(v["jev"]["error"], Value::Null);
}

/// Without `models`, a per-model window is shown for the `default` pair, in the output, the
/// JSON and the request, and does not make it infeasible.
#[test]
fn per_model_windows_without_models_are_shown_not_counted() {
    let sb = Sandbox::new();
    let max = sb.make_claude_home("h/max");
    sb.write_claude_json(
        Some(&max),
        &claude_json(
            "m@example.com",
            600,
            &[
                limit("weekly_all", 40.0, 3 * DAY),
                limit("Fable", 100.0, 3 * DAY),
            ],
        ),
    );
    configure(&sb, &[("claude", "max", &max)], "");
    let out = pick(&sb, false, &[]);
    assert_eq!(out.code, Some(0), "{}{}", out.stdout, out.stderr);
    assert_eq!(field(&out, "account"), "claude:max");
    assert!(
        field(&out, "also").starts_with("Week (Fable) 100% used, resets in ")
            && field(&out, "also")
                .ends_with("(counts only if the agent's default model is of that family)"),
        "{}",
        out.stdout
    );
    let v: Value = serde_json::from_str(&pick(&sb, false, &["--json"]).stdout).unwrap();
    let c = v["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["account"] == "claude:max")
        .unwrap();
    assert_eq!(c["feasible"], true);
    assert_eq!(c["headroom"], 60.0);
    assert_eq!(c["default_model_windows"][0]["label"], "Week (Fable)");
    assert_eq!(c["default_model_windows"][0]["percent"], 100.0);
    let body = pick(&sb, false, &["--print-request"]).stdout;
    let body: Value = serde_json::from_str(body.trim_end()).unwrap();
    assert!(
        body["state"]
            .as_str()
            .unwrap()
            .contains("(per-model; applies only if the agent's default model is in this family)"),
        "{body:#}"
    );
}
