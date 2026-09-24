//! R4, R10, R17: codex accounts on the command line: `add`, `setup`, `list`, `run`, `usage`.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use common::{Sandbox, parse_table};
use predicates::prelude::*;
use serde_json::json;

/// Registers accounts by writing config.toml directly: `(provider, name, home)`.
fn register(sb: &Sandbox, accounts: &[(&str, &str, &str)]) {
    let mut text = String::new();
    for (provider, name, home) in accounts {
        text.push_str(&format!(
            "[[account]]\nprovider = \"{provider}\"\nname = \"{name}\"\nhome = \"{home}\"\n\n"
        ));
    }
    sb.write_config(&text);
}

/// `(provider, name, home)` rows of config.toml.
fn stored(sb: &Sandbox) -> Vec<(String, String, String)> {
    let doc: toml_edit::DocumentMut = sb.read_config().parse().unwrap();
    doc["account"]
        .as_array_of_tables()
        .unwrap()
        .iter()
        .map(|t| {
            let s = |k: &str| t[k].as_str().unwrap().to_string();
            (s("provider"), s("name"), s("home"))
        })
        .collect()
}

/// `remuda list`: `(ACCOUNT, EMAIL, HOME)` per row, and stderr.
fn list(sb: &Sandbox) -> (Vec<(String, String, String)>, String) {
    let out = sb
        .remuda()
        .arg("list")
        .assert()
        .success()
        .get_output()
        .clone();
    let rows = parse_table(&String::from_utf8(out.stdout).unwrap())
        .into_iter()
        .map(|r| (r["ACCOUNT"].clone(), r["EMAIL"].clone(), r["HOME"].clone()))
        .collect();
    (rows, String::from_utf8(out.stderr).unwrap())
}

fn row(account: &str, email: &str, home: &str) -> (String, String, String) {
    (account.into(), email.into(), home.into())
}

// --- add ------------------------------------------------------------------------------------

#[test]
fn add_registers_a_codex_home() {
    let sb = Sandbox::new();
    let home = sb.make_codex_home("c/work");
    let home = home.to_str().unwrap();
    sb.remuda()
        .args(["add", "--provider", "codex", "work", home])
        .assert()
        .success()
        .stderr("");
    assert_eq!(
        stored(&sb),
        [(String::from("codex"), "work".into(), home.into())]
    );
    // Without codex on PATH or ~/.codex there is no codex:default, and the identity of
    // codex:work cannot be asked.
    let (rows, stderr) = list(&sb);
    assert_eq!(
        rows,
        [
            row("claude:default", "unknown", "default"),
            row("codex:work", "unknown", home)
        ]
    );
    assert!(stderr.contains("`codex` not found on PATH"), "{stderr}");
}

#[test]
fn add_warns_about_a_directory_that_does_not_look_like_a_codex_home() {
    let sb = Sandbox::new();
    let dir = sb.root().join("empty");
    fs::create_dir(&dir).unwrap();
    sb.remuda()
        .args(["add", "--provider", "codex", "x", dir.to_str().unwrap()])
        .assert()
        .success()
        .stderr(predicate::str::contains("does not look like a Codex home"));
    assert_eq!(stored(&sb).len(), 1);
}

/// `$HOME/.codex` is `codex:default` (R1, R14).
#[test]
fn add_refuses_the_native_codex_home() {
    let sb = Sandbox::new();
    let native = sb.home().join(".codex");
    fs::create_dir_all(&native).unwrap();
    sb.remuda()
        .args(["add", "--provider", "codex", "x", native.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("codex:default"));
    assert!(!sb.config_path().exists());
}

/// A home is one provider's: the same directory cannot also be a claude account.
#[test]
fn add_refuses_a_home_registered_under_the_other_provider() {
    let sb = Sandbox::new();
    let dir = sb.make_claude_home("shared");
    let dir = dir.to_str().unwrap();
    sb.remuda().args(["add", "max", dir]).assert().success();
    sb.remuda()
        .args(["add", "--provider", "codex", "max", dir])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("already registered as claude:max"));
}

// --- list -----------------------------------------------------------------------------------

/// R4, R10a, R17: identity is `codex login status` under the account's `CODEX_HOME` (it prints
/// to stderr and exits 1 when not logged in); only the login method, no email. `codex
/// app-server` is not run: it is for explicit live queries only.
#[test]
fn list_shows_the_codex_login_method() {
    let sb = Sandbox::new();
    sb.install_codex();
    sb.set_codex_login(None, "Logged in using ChatGPT\n");
    let work = sb.make_codex_home("c/work");
    let api = sb.make_codex_home("c/api");
    sb.set_codex_login(Some(&api), "Logged in using an API key - sk-proj-***ABCD\n");
    register(
        &sb,
        &[
            ("codex", "work", work.to_str().unwrap()),
            ("codex", "api", api.to_str().unwrap()),
        ],
    );
    let (rows, stderr) = list(&sb);
    assert_eq!(
        rows,
        [
            row("claude:default", "unknown", "default"),
            row("codex:default", "logged in (ChatGPT)", "default"),
            row("codex:work", "not logged in", work.to_str().unwrap()),
            row("codex:api", "logged in (API key)", api.to_str().unwrap()),
        ]
    );
    assert!(!stderr.contains("sk-proj"), "{stderr}");
    let mut homes: Vec<Option<String>> = sb
        .codex_invocations()
        .into_iter()
        .map(|inv| {
            assert_eq!(inv.args, ["login", "status"]);
            inv.codex_home
        })
        .collect();
    homes.sort();
    assert_eq!(
        homes,
        [
            None,
            Some(api.to_str().unwrap().to_string()),
            Some(work.to_str().unwrap().to_string())
        ]
    );
    assert!(sb.codex_rpc().is_empty());
}

/// Output remuda does not recognize: unknown, with a warning; never a crash.
#[test]
fn list_with_unrecognized_codex_output() {
    let sb = Sandbox::new();
    sb.install_codex();
    sb.set_codex_login(None, "something new\n");
    let (rows, stderr) = list(&sb);
    assert_eq!(rows[1], row("codex:default", "unknown", "default"));
    assert!(
        stderr.contains("codex:default: `codex login status`"),
        "{stderr}"
    );
}

// --- setup ----------------------------------------------------------------------------------

#[test]
fn setup_creates_a_codex_home_and_runs_codex_login() {
    let sb = Sandbox::new();
    sb.install_codex();
    sb.remuda()
        .env("CODEX_HOME", sb.root().join("elsewhere"))
        .args(["setup", "--provider", "codex", "work"])
        .assert()
        .success();
    let home = sb.remuda_home().join("homes/codex/work");
    let meta = fs::metadata(&home).expect("home created");
    assert_eq!(meta.permissions().mode() & 0o777, 0o700);
    let home = home.to_str().unwrap();
    assert_eq!(
        stored(&sb),
        [(String::from("codex"), "work".into(), home.into())]
    );
    let [inv] = sb.codex_invocations().try_into().unwrap();
    assert_eq!(inv.args, ["login"]);
    assert_eq!(inv.codex_home.as_deref(), Some(home));
    assert!(sb.invocations().is_empty(), "claude did not run");
    assert!(!sb.remuda_home().join("state").exists());
}

/// Checked before anything is created: codex takes no email, and must be on PATH.
#[test]
fn setup_codex_refusals_create_nothing() {
    let sb = Sandbox::new();
    sb.remuda()
        .args(["setup", "--provider", "codex", "work"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("`codex` not found on PATH"));
    sb.install_codex();
    sb.remuda()
        .args([
            "setup",
            "--provider",
            "codex",
            "work",
            "--email",
            "a@example.com",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("takes no email"));
    sb.remuda()
        .args(["setup", "--provider", "gemini", "work"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("unknown provider \"gemini\""));
    assert!(!sb.remuda_home().exists());
    assert!(sb.codex_invocations().is_empty());
}

// --- run ------------------------------------------------------------------------------------

const WORK_HOME: &str = "/nonexistent/codex homes/work/";

/// R17: `CODEX_HOME` byte-exact, args verbatim, no session id injected; logged with a null
/// session id.
#[test]
fn run_codex_sets_codex_home_and_logs_without_a_session_id() {
    let sb = Sandbox::new();
    sb.install_codex();
    register(&sb, &[("codex", "work", WORK_HOME)]);
    for reference in ["codex:work", "work"] {
        sb.remuda()
            .env("CODEX_HOME", "/inherited")
            .args(["run", reference, "-p", "fix it", "--", "-x"])
            .assert()
            .success();
    }
    let invocations = sb.codex_invocations();
    assert_eq!(invocations.len(), 2);
    for inv in invocations {
        assert_eq!(inv.codex_home.as_deref(), Some(WORK_HOME));
        assert_eq!(inv.args, ["-p", "fix it", "--", "-x"]);
        assert_eq!(inv.cwd, sb.work().canonicalize().unwrap());
    }
    assert!(sb.invocations().is_empty());
    let log = sb.launches();
    assert_eq!(log[0]["account"], json!("codex:work"));
    assert_eq!(log[0]["home"], json!(WORK_HOME));
    assert_eq!(log[0]["args"], json!(["-p", "fix it", "--", "-x"]));
    assert_eq!(log[0]["session_id"], serde_json::Value::Null);
    assert_eq!(log[0]["injected"], json!(false));
}

/// R17: `run codex:<name> resume <id>` resumes in place: the log names the resumed id; a
/// fork's id is codex's, so it stays null, and `fork_of` names the forked one. Args verbatim
/// either way.
#[test]
fn run_codex_resume_logs_the_resumed_id() {
    let sb = Sandbox::new();
    sb.install_codex();
    register(&sb, &[("codex", "work", WORK_HOME)]);
    let id = "019c1e08-e4f6-7d70-a129-38ec744a3f3c";
    for verb in ["resume", "fork"] {
        sb.remuda()
            .args(["run", "codex:work", verb, id, "-C", "/w"])
            .assert()
            .success();
    }
    let invocations = sb.codex_invocations();
    assert_eq!(invocations[0].args, ["resume", id, "-C", "/w"]);
    assert_eq!(invocations[1].args, ["fork", id, "-C", "/w"]);
    let log = sb.launches();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0]["account"], json!("codex:work"));
    assert_eq!(log[0]["args"], json!(["resume", id, "-C", "/w"]));
    assert_eq!(log[0]["session_id"], json!(id));
    assert_eq!(log[0]["injected"], json!(false));
    assert_eq!(log[1]["args"], json!(["fork", id, "-C", "/w"]));
    assert_eq!(log[1]["session_id"], serde_json::Value::Null);
    assert_eq!(log[1]["fork_of"], json!(id));
    assert_eq!(log[1]["injected"], json!(false));
}

#[test]
fn run_codex_without_codex_on_path() {
    let sb = Sandbox::new();
    register(&sb, &[("codex", "work", WORK_HOME)]);
    sb.remuda()
        .args(["run", "codex:work"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("`codex` not found on PATH"));
}

// --- usage ----------------------------------------------------------------------------------

/// `remuda usage` output blocks: the header (whitespace-normalized), then its rows.
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

/// `remuda <args>`: stdout and the exit code.
fn usage(sb: &Sandbox, args: &[&str]) -> (String, Option<i32>) {
    let out = sb.remuda().args(args).output().unwrap();
    (String::from_utf8(out.stdout).unwrap(), out.status.code())
}

/// The block of `account` (its header starts with the name).
fn block<'a>(blocks: &'a [(String, Vec<String>)], account: &str) -> &'a (String, Vec<String>) {
    let prefix = format!("{account} ");
    blocks
        .iter()
        .find(|(header, _)| header.starts_with(&prefix))
        .unwrap_or_else(|| panic!("no block for {account}: {blocks:?}"))
}

fn path(home: &Path) -> &str {
    home.to_str().unwrap()
}

/// A rollout `rate_limits` record, snake_case like codex writes it.
fn rollout_limits(limit_id: serde_json::Value, windows: &[(f64, i64, i64)]) -> serde_json::Value {
    let window = |i: usize| {
        windows.get(i).map_or(serde_json::Value::Null, |(pct, minutes, resets)| {
            json!({"used_percent": pct, "window_minutes": minutes, "resets_at": resets})
        })
    };
    json!({"limit_id": limit_id, "limit_name": null, "primary": window(0),
           "secondary": window(1), "credits": {"has_credits": false, "unlimited": false,
           "balance": "0"}, "individual_limit": null, "spend_control_reached": null,
           "plan_type": "pro", "rate_limit_reached_type": null})
}

/// R10: cached codex usage is the newest general rate limits recorded in the home's rollouts
/// (`sessions/**` and `archived_sessions/`); codex is not run.
#[test]
fn usage_reads_codex_rate_limits_from_rollouts() {
    use common::rollouts::{token_count, ts, write_archived_rollout, write_rollout};
    let sb = Sandbox::new();
    let work = sb.make_codex_home("c/work");
    let empty = sb.make_codex_home("c/empty");
    register(
        &sb,
        &[
            ("codex", "work", path(&work)),
            ("codex", "empty", path(&empty)),
        ],
    );
    // A Pro plan: one weekly window. Older records and a per-model one after it do not count.
    let pro = rollout_limits(json!("codex"), &[(99.0, 10080, 1790414559)]);
    let older = rollout_limits(json!("codex"), &[(50.0, 10080, 1790414559)]);
    let spark = json!({"limit_id": "codex_bengalfox", "limit_name": "GPT-5.3-Codex-Spark",
                       "primary": {"used_percent": 5.0, "window_minutes": 300, "resets_at": 1}});
    write_rollout(
        &work,
        "019c1e08-e4f6-7d70-a129-38ec744a3f3c",
        &[
            token_count(older, &ts(1)),
            token_count(pro, &ts(2)),
            token_count(spark, &ts(3)),
            token_count(serde_json::Value::Null, &ts(4)),
        ]
        .concat(),
    );
    // codex:default (`~/.codex`) has only an archived rollout, from 2025: a null `limit_id`,
    // windows of 299 and 10079 minutes.
    let plus = rollout_limits(
        serde_json::Value::Null,
        &[(10.0, 299, 1790100000), (20.0, 10079, 1790500000)],
    );
    write_archived_rollout(
        &sb.home().join(".codex"),
        "019c1e09-b0ff-7842-aca4-1397c3b7b047",
        &token_count(plus, &ts(5)),
    );
    let (out, code) = usage(&sb, &["usage"]);
    assert_eq!(code, Some(0), "{out}");
    let b = blocks(&out);
    let (header, rows) = block(&b, "codex:work");
    assert!(
        header.starts_with("codex:work cached ") && header.ends_with("(Sep 20 10:02)"),
        "{out}"
    );
    assert_eq!(rows, &["Week (all models) 99% !! resets Sep 26 09:22"]);
    let (header, rows) = block(&b, "codex:default");
    assert!(header.ends_with("(Sep 20 10:05)"), "{out}");
    assert_eq!(
        rows,
        &[
            "Session 10% resets Sep 22 18:00",
            "Week (all models) 20% resets Sep 27 09:06"
        ]
    );
    let (header, rows) = block(&b, "codex:empty");
    assert_eq!(
        header,
        &format!(
            "codex:empty no cached usage (no rate limits in the rollouts under {})",
            path(&empty)
        )
    );
    assert!(rows.is_empty());
    assert!(sb.codex_invocations().is_empty());
}

/// An `account/rateLimits/read` result: Pro's weekly general limit, and a per-model limit.
const RATE_LIMITS: &str = r#"{
  "rateLimits": {"limitId": "codex", "limitName": null,
    "primary": {"usedPercent": 99, "windowDurationMins": 10080, "resetsAt": 1790414559},
    "secondary": null, "credits": {"hasCredits": false, "unlimited": false, "balance": "0"},
    "planType": "pro"},
  "rateLimitsByLimitId": {
    "codex": {"limitId": "codex", "limitName": null,
      "primary": {"usedPercent": 99, "windowDurationMins": 10080, "resetsAt": 1790414559},
      "secondary": null, "planType": "pro"},
    "codex_bengalfox": {"limitId": "codex_bengalfox", "limitName": "GPT-5.3-Codex-Spark",
      "primary": {"usedPercent": 5, "windowDurationMins": 300, "resetsAt": 1790300000},
      "secondary": {"usedPercent": 7, "windowDurationMins": 10080, "resetsAt": 1790700000},
      "planType": "pro"}},
  "rateLimitResetCredits": null
}"#;

const CX_ACCOUNT: &str = r#"{"account":{"type":"chatgpt","email":"cx@example.com","planType":"pro"},"requiresOpenaiAuth":true}"#;

const LIVE_ROWS: [&str; 3] = [
    "Week (all models) 99% !! resets Sep 26 09:22",
    "Session (GPT-5.3-Codex-Spark) 5% resets Sep 25 01:33",
    "Week (GPT-5.3-Codex-Spark) 7% resets Sep 29 16:40",
];

/// R4, R10: `usage --live` runs one `codex app-server` per codex account, in its `CODEX_HOME`,
/// asking the rate limits and the account in the same session; the header shows the email and
/// plan, the rows include per-model limits.
#[test]
fn live_usage_asks_codex_app_server() {
    let sb = Sandbox::new();
    sb.install_codex();
    let work = sb.make_codex_home("c/work");
    register(&sb, &[("codex", "work", path(&work))]);
    for home in [None, Some(work.as_path())] {
        sb.set_codex_rate_limits(home, RATE_LIMITS);
    }
    sb.set_codex_account(Some(&work), CX_ACCOUNT);
    sb.set_codex_account(
        None,
        r#"{"account":{"type":"apiKey"},"requiresOpenaiAuth":true}"#,
    );
    sb.set_live_usage(None, "Current session: 3% used\n");
    let (out, code) = usage(&sb, &["usage", "--live"]);
    assert_eq!(code, Some(0), "{out}");
    let b = blocks(&out);
    let headers: Vec<&str> = b.iter().map(|(h, _)| h.as_str()).collect();
    assert_eq!(
        headers,
        [
            "claude:default live",
            "codex:default live logged in (API key)",
            "codex:work live cx@example.com (pro)",
        ]
    );
    assert_eq!(b[1].1, LIVE_ROWS);
    assert_eq!(b[2].1, LIVE_ROWS);

    // One app-server per codex home, each with the handshake, then both requests.
    let homes = [None, Some(path(&work).to_string())];
    let invocations = sb.codex_invocations();
    assert_eq!(invocations.len(), 2, "{invocations:?}");
    let rpc = sb.codex_rpc();
    for home in &homes {
        let runs: Vec<_> = invocations
            .iter()
            .filter(|i| &i.codex_home == home)
            .collect();
        assert_eq!(runs.len(), 1, "{home:?}: {invocations:?}");
        assert_eq!(runs[0].args, ["app-server"]);
        let messages: Vec<&serde_json::Value> = rpc
            .iter()
            .filter(|r| &r.codex_home == home)
            .map(|r| &r.message)
            .collect();
        let version = env!("CARGO_PKG_VERSION");
        assert_eq!(
            messages,
            [
                &json!({"id": 1, "method": "initialize",
                        "params": {"clientInfo": {"name": "remuda", "version": version}}}),
                &json!({"method": "initialized"}),
                &json!({"id": 2, "method": "account/rateLimits/read",
                        "params": {"excludeResetCreditDetails": true}}),
                &json!({"id": 3, "method": "account/read", "params": {"refreshToken": false}}),
            ],
            "{home:?}"
        );
    }
    assert_eq!(
        sb.invocations().len(),
        1,
        "claude asked once, for its own account"
    );
    assert!(!sb.remuda_home().join("state").exists());
}

/// R10: `account/read` failing does not fail the query: the usage is shown, without an identity.
#[test]
fn live_usage_codex_account_read_error_keeps_the_usage() {
    let sb = Sandbox::new();
    sb.install_codex();
    let work = sb.make_codex_home("c/work");
    register(&sb, &[("codex", "work", path(&work))]);
    sb.set_codex_rate_limits(Some(&work), RATE_LIMITS);
    sb.set_codex_account_error(Some(&work), r#"{"code":-32603,"message":"backend down"}"#);
    let (out, code) = usage(&sb, &["usage", "codex:work", "--live"]);
    assert_eq!(code, Some(0), "{out}");
    let b = blocks(&out);
    assert_eq!(b.len(), 1, "{out}");
    assert_eq!(b[0].0, "codex:work live");
    assert_eq!(b[0].1, LIVE_ROWS);
    // An `account/read` result remuda does not recognize: the same.
    fs::remove_file(work.join("fake-account-error.json")).unwrap();
    sb.set_codex_account(Some(&work), r#"{"account":"x"}"#);
    let (out, code) = usage(&sb, &["usage", "codex:work", "--live"]);
    assert_eq!(code, Some(0), "{out}");
    assert_eq!(blocks(&out)[0].0, "codex:work live");
}

/// R10: a logged-out home's rate limits are an error: that account fails (exit 1), the others
/// still print.
#[test]
fn live_usage_codex_logged_out_fails() {
    let sb = Sandbox::new();
    sb.install_codex();
    let work = sb.make_codex_home("c/work");
    register(&sb, &[("codex", "work", path(&work))]);
    sb.set_codex_rate_limits(None, RATE_LIMITS);
    sb.set_live_usage(None, "Current session: 3% used\n");
    let (out, code) = usage(&sb, &["usage", "--live"]);
    assert_eq!(code, Some(1), "{out}");
    let b = blocks(&out);
    assert_eq!(b.len(), 3, "{out}");
    assert_eq!(block(&b, "claude:default").1, ["Session 3%"]);
    assert_eq!(block(&b, "codex:default").1, LIVE_ROWS);
    assert_eq!(
        block(&b, "codex:work").0,
        "codex:work error: `codex app-server` account/rateLimits/read: codex account \
         authentication required to read rate limits"
    );
}

/// A hanging codex is stopped at the timeout.
#[test]
fn live_usage_codex_timeout() {
    let sb = Sandbox::new();
    sb.install_codex();
    let work = sb.make_codex_home("c/work");
    register(&sb, &[("codex", "work", path(&work))]);
    sb.set_codex_hang(Some(&work), 30);
    let start = Instant::now();
    let (out, code) = usage(&sb, &["usage", "codex:work", "--live", "--timeout", "3"]);
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "took {:?}",
        start.elapsed()
    );
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        blocks(&out)[0].0,
        "codex:work error: `codex app-server` timed out after 3s"
    );
}

/// A codex without `app-server`: that account's error.
#[test]
fn live_usage_codex_without_app_server() {
    let sb = Sandbox::new();
    sb.install_codex();
    let work = sb.make_codex_home("c/work");
    register(&sb, &[("codex", "work", path(&work))]);
    sb.set_codex_without_app_server(Some(&work));
    let (out, code) = usage(&sb, &["usage", "codex:work", "--live"]);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        blocks(&out)[0].0,
        "codex:work error: `codex app-server` exited with status 2: error: unrecognized \
         subcommand 'app-server'"
    );
}

/// R10: without codex on PATH, each codex account fails on its own; claude's still print.
#[test]
fn live_usage_without_codex_on_path() {
    let sb = Sandbox::new();
    register(&sb, &[("codex", "work", WORK_HOME)]);
    sb.set_live_usage(None, "Current session: 3% used\n");
    let (out, code) = usage(&sb, &["usage", "codex:work", "--live"]);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(out, "codex:work  error: `codex` not found on PATH\n");
    let (out, code) = usage(&sb, &["usage", "--live"]);
    assert_eq!(code, Some(1), "{out}");
    let b = blocks(&out);
    assert_eq!(b[0].0, "claude:default live");
    assert_eq!(b[1].0, "codex:work error: `codex` not found on PATH");
}

/// R10: a result without a usable window is shown as is (indented JSON), and is not a failure.
#[test]
fn live_usage_codex_unrecognized() {
    let sb = Sandbox::new();
    sb.install_codex();
    let work = sb.make_codex_home("c/work");
    register(&sb, &[("codex", "work", path(&work))]);
    sb.set_codex_rate_limits(
        Some(&work),
        r#"{"rateLimits":{"limitId":"codex","primary":null,"secondary":null}}"#,
    );
    let (out, code) = usage(&sb, &["usage", "codex:work", "--live"]);
    assert_eq!(code, Some(0), "{out}");
    // No account fixture: `account/read` says not logged in, which is not shown.
    assert_eq!(
        out,
        "codex:work  live (output not recognized; shown as is)\n    {\n      \"rateLimits\": {\n        \
         \"limitId\": \"codex\",\n        \"primary\": null,\n        \"secondary\": null\n      }\n    }\n"
    );
}

// --- sessions -------------------------------------------------------------------------------

/// R8, R17: codex sessions are listed with the account whose home holds them, titled by their
/// thread name.
#[test]
fn sessions_include_codex_rollouts() {
    use common::rollouts::{meta, thread_name, ts, user, write_rollout};
    let sb = Sandbox::new();
    let work = sb.make_codex_home("c/work");
    register(&sb, &[("codex", "work", work.to_str().unwrap())]);
    let id = "019c1e08-e4f6-7d70-a129-38ec744a3f3c";
    let sub = "019c1e09-b0ff-7842-aca4-1397c3b7b047";
    write_rollout(
        &work,
        id,
        &[
            meta(id, "/w/proj", json!("cli"), 10, &ts(1)),
            user(&["hello codex"], &ts(2)),
        ]
        .concat(),
    );
    // A subagent's rollout is noise, but `remuda sessions` lists everything (like SDK ones).
    write_rollout(
        &work,
        sub,
        &[meta(
            sub,
            "/w/sub",
            json!({"subagent": "review"}),
            10,
            &ts(3),
        )]
        .concat(),
    );
    let out = sb
        .remuda()
        .arg("sessions")
        .assert()
        .success()
        .get_output()
        .clone();
    let rows = parse_table(&String::from_utf8(out.stdout).unwrap());
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[1]["ACCOUNTS"], "codex:work");
    assert_eq!(rows[1]["TITLE"], "hello codex");
    assert_eq!(rows[1]["CWD"], "/w/proj");
    assert_eq!(rows[0]["ACCOUNTS"], "codex:work");

    fs::write(
        work.join("session_index.jsonl"),
        thread_name(id, "Greeting codex"),
    )
    .unwrap();
    let out = sb
        .remuda()
        .arg("sessions")
        .assert()
        .success()
        .get_output()
        .clone();
    let rows = parse_table(&String::from_utf8(out.stdout).unwrap());
    assert_eq!(rows[1]["TITLE"], "Greeting codex");
}
