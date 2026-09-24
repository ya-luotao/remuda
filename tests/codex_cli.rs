//! R4, R17: codex accounts on the command line: `add`, `setup`, `list`, `run`, `usage`.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;

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

/// R4, R17: identity is `codex login status` under the account's `CODEX_HOME` (it prints to
/// stderr and exits 1 when not logged in); only the login method, no email.
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

/// R4: codex has no usage; it says so and is not a failure.
#[test]
fn usage_shows_codex_as_unsupported() {
    let sb = Sandbox::new();
    sb.install_codex();
    register(&sb, &[("codex", "work", WORK_HOME)]);
    for args in [
        &["usage"][..],
        &["usage", "--live"],
        &["usage", "codex:work", "--live"],
    ] {
        let out = sb.remuda().args(args).output().unwrap();
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(
            stdout.contains("codex:work  usage is not available for codex"),
            "{args:?}: {stdout}"
        );
        if args.len() == 3 {
            // Only the account asked for; nothing failed.
            assert!(!stdout.contains("codex:default"), "{stdout}");
            assert!(out.status.success(), "{args:?}");
        } else {
            assert!(
                stdout.contains("codex:default  usage is not available for codex"),
                "{args:?}: {stdout}"
            );
        }
    }
    assert!(sb.codex_invocations().is_empty());
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
