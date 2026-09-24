//! R5, R13: `remuda setup <name> [--email <addr>]`.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use common::Sandbox;
use predicates::prelude::*;

fn new_home(sb: &Sandbox, name: &str) -> PathBuf {
    sb.remuda_home().join("homes").join("claude").join(name)
}

/// `(name, home)` of every stored account.
fn stored(sb: &Sandbox) -> Vec<(String, String)> {
    let Ok(text) = fs::read_to_string(sb.config_path()) else {
        return Vec::new();
    };
    let doc: toml_edit::DocumentMut = text.parse().unwrap();
    doc.get("account")
        .and_then(|a| a.as_array_of_tables())
        .map(|arr| {
            arr.iter()
                .map(|t| {
                    (
                        t["name"].as_str().unwrap().to_string(),
                        t["home"].as_str().unwrap().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn setup_creates_home_registers_and_logs_in() {
    let sb = Sandbox::new();
    sb.remuda()
        .env("CLAUDE_CONFIG_DIR", sb.root().join("elsewhere"))
        .args(["setup", "work"])
        .assert()
        .success();

    let home = new_home(&sb, "work");
    let home_str = home.to_str().unwrap().to_string();
    let meta = fs::metadata(&home).expect("home created");
    assert!(meta.is_dir());
    assert_eq!(meta.permissions().mode() & 0o777, 0o700);
    assert_eq!(
        fs::read_dir(&home).unwrap().count(),
        0,
        "remuda writes nothing into the home"
    );
    assert_eq!(stored(&sb), [("work".to_string(), home_str.clone())]);

    let inv = sb.only_invocation();
    assert_eq!(inv.args, ["auth", "login"]);
    assert_eq!(inv.config_dir.as_deref(), Some(home_str.as_str()));
    assert!(
        !sb.remuda_home().join("state").exists(),
        "setup is not a session launch"
    );
}

#[test]
fn setup_passes_email() {
    let sb = Sandbox::new();
    sb.remuda()
        .args(["setup", "work", "--email", "me+work@example.com"])
        .assert()
        .success();
    assert_eq!(
        sb.only_invocation().args,
        ["auth", "login", "--email", "me+work@example.com"]
    );
}

#[test]
fn setup_appends_to_existing_config() {
    let sb = Sandbox::new();
    let max = sb.make_claude_home("profiles/max");
    sb.write_config(&format!(
        "# mine\n[[account]]\nprovider = \"claude\"\nname = \"max\"\nhome = \"{}\"\n",
        max.display()
    ));
    sb.remuda().args(["setup", "work"]).assert().success();
    assert!(sb.read_config().starts_with("# mine\n"));
    let names: Vec<String> = stored(&sb).into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, ["max", "work"]);
}

#[test]
fn setup_login_failure_keeps_registration_and_prints_retry_hint() {
    let sb = Sandbox::new();
    sb.remuda()
        .env("FAKE_CLAUDE_EXIT", "3")
        .args(["setup", "work"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("remuda run work auth login"));
    assert!(new_home(&sb, "work").is_dir());
    assert_eq!(stored(&sb).len(), 1);
}

/// With `codex:work` registered a bare `work` is ambiguous: the retry names `claude:work`.
#[test]
fn setup_retry_hint_is_unambiguous() {
    let sb = Sandbox::new();
    sb.write_config("[[account]]\nprovider = \"codex\"\nname = \"work\"\nhome = \"/c/work\"\n");
    sb.remuda()
        .env("FAKE_CLAUDE_EXIT", "3")
        .args(["setup", "work"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains(
            "retry with: remuda run claude:work auth login",
        ));
    assert!(new_home(&sb, "work").is_dir());
}

/// Asserts exit 1 with a `remuda: ` error containing `needle`, and no side effects.
fn assert_setup_fails(sb: &Sandbox, args: &[&str], needle: &str) {
    let config_before = fs::read_to_string(sb.config_path()).ok();
    sb.remuda()
        .arg("setup")
        .args(args)
        .assert()
        .code(1)
        .stderr(predicate::str::starts_with("remuda: ").and(predicate::str::contains(needle)));
    assert_eq!(fs::read_to_string(sb.config_path()).ok(), config_before);
    assert!(sb.invocations().is_empty(), "claude must not run");
}

#[test]
fn setup_refuses_bad_names() {
    let sb = Sandbox::new();
    assert_setup_fails(&sb, &["default"], "reserved");
    assert_setup_fails(&sb, &["a.b"], "name");
    assert_setup_fails(&sb, &["../x"], "name");
    assert_setup_fails(&sb, &["--", "-x"], "cannot start with `-`");
    assert!(!sb.remuda_home().join("homes").exists());
}

#[test]
fn setup_refuses_registered_name() {
    let sb = Sandbox::new();
    let max = sb.make_claude_home("profiles/max");
    sb.register(&[("max", &max)]);
    assert_setup_fails(&sb, &["max"], "claude:max");
    assert!(!new_home(&sb, "max").exists());
}

#[test]
fn setup_refuses_existing_directory() {
    let sb = Sandbox::new();
    let home = new_home(&sb, "work");
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join("keep"), "x").unwrap();
    assert_setup_fails(&sb, &["work"], "already exists");
    assert_eq!(fs::read_to_string(home.join("keep")).unwrap(), "x");
}

#[test]
fn setup_needs_claude_before_creating_anything() {
    let sb = Sandbox::new();
    sb.remuda()
        .env("PATH", "/usr/bin:/bin")
        .args(["setup", "work"])
        .assert()
        .code(1)
        .stderr(predicate::str::starts_with("remuda: ").and(predicate::str::contains("claude")));
    assert!(!sb.remuda_home().exists());
}

#[test]
fn setup_refuses_relative_remuda_home() {
    let sb = Sandbox::new();
    sb.remuda()
        .env("REMUDA_HOME", "rel")
        .args(["setup", "work"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("absolute"));
    assert!(!sb.work().join("rel").exists());
    assert!(sb.invocations().is_empty());
}
