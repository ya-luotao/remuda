//! R2, R5, R6: `remuda run` passthrough, child environment and exec.

mod common;

use std::fs;

use common::Sandbox;
use predicates::prelude::*;

const MAX_HOME: &str = "/nonexistent/profiles/max with space/";

/// A sandbox with `claude:max` registered at a home string with a space and a trailing
/// slash. `run` must not touch the directory, so it does not have to exist.
fn sandbox_with_max() -> Sandbox {
    let sb = Sandbox::new();
    sb.write_config(&format!(
        "[[account]]\nprovider = \"claude\"\nname = \"max\"\nhome = \"{MAX_HOME}\"\n"
    ));
    sb
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

// --- passthrough -------------------------------------------------------------------------

#[test]
fn run_passes_args_verbatim() {
    let sb = sandbox_with_max();
    let args = [
        "--resume",
        "abc",
        "-p",
        "hi there",
        "two\nlines",
        "",
        "--model",
        "opus",
    ];
    sb.remuda()
        .args(["run", "max"])
        .args(args)
        .assert()
        .success();
    assert_eq!(sb.only_invocation().args, strings(&args));
}

#[test]
fn run_passes_help_to_claude() {
    let sb = sandbox_with_max();
    sb.remuda()
        .args(["run", "max", "--help"])
        .assert()
        .success()
        .stdout("");
    assert_eq!(sb.only_invocation().args, ["--help"]);
}

#[test]
fn run_passes_version_to_claude() {
    let sb = sandbox_with_max();
    sb.remuda()
        .args(["run", "max", "--version"])
        .assert()
        .success()
        .stdout("");
    assert_eq!(sb.only_invocation().args, ["--version"]);
}

#[test]
fn run_passes_double_dash_verbatim() {
    let sb = sandbox_with_max();
    sb.remuda()
        .args(["run", "max", "--resume", "abc", "--", "-x"])
        .assert()
        .success();
    assert_eq!(sb.only_invocation().args, ["--resume", "abc", "--", "-x"]);
}

#[test]
fn run_passes_subcommand_verbatim() {
    let sb = sandbox_with_max();
    sb.remuda()
        .args(["run", "max", "agents", "--json"])
        .assert()
        .success();
    assert_eq!(sb.only_invocation().args, ["agents", "--json"]);
}

// --- environment -------------------------------------------------------------------------

#[test]
fn run_named_sets_config_dir_byte_exact() {
    let sb = sandbox_with_max();
    sb.remuda()
        .args(["run", "max", "doctor"])
        .assert()
        .success();
    assert_eq!(sb.only_invocation().config_dir.as_deref(), Some(MAX_HOME));
}

#[test]
fn run_named_overrides_inherited_config_dir() {
    let sb = sandbox_with_max();
    sb.remuda()
        .env("CLAUDE_CONFIG_DIR", "/inherited/elsewhere")
        .args(["run", "claude:max", "doctor"])
        .assert()
        .success();
    assert_eq!(sb.only_invocation().config_dir.as_deref(), Some(MAX_HOME));
}

#[test]
fn run_default_removes_inherited_config_dir() {
    for reference in ["default", "claude:default"] {
        let sb = sandbox_with_max();
        sb.remuda()
            .env("CLAUDE_CONFIG_DIR", "/inherited/elsewhere")
            .args(["run", reference, "doctor"])
            .assert()
            .success();
        assert_eq!(sb.only_invocation().config_dir, None, "{reference}");
    }
}

#[test]
fn run_default_works_without_config_file() {
    let sb = Sandbox::new();
    sb.remuda()
        .args(["run", "default", "doctor"])
        .assert()
        .success();
    assert_eq!(sb.only_invocation().config_dir, None);
}

#[test]
fn run_warns_and_keeps_securestorage_dir() {
    let sb = sandbox_with_max();
    sb.remuda()
        .env("CLAUDE_SECURESTORAGE_CONFIG_DIR", "/secure/x")
        .args(["run", "max", "doctor"])
        .assert()
        .success()
        .stderr(
            predicate::str::starts_with("remuda: warning: ")
                .and(predicate::str::contains("CLAUDE_SECURESTORAGE_CONFIG_DIR")),
        );
    let inv = sb.only_invocation();
    assert_eq!(inv.securestorage_dir.as_deref(), Some("/secure/x"));
    assert_eq!(inv.config_dir.as_deref(), Some(MAX_HOME));
}

#[test]
fn run_without_securestorage_dir_does_not_set_it() {
    let sb = sandbox_with_max();
    sb.remuda()
        .args(["run", "max", "doctor"])
        .assert()
        .success()
        .stderr("");
    assert_eq!(sb.only_invocation().securestorage_dir, None);
}

#[test]
fn run_keeps_cwd() {
    let sb = sandbox_with_max();
    let dir = sb.root().join("some project");
    fs::create_dir(&dir).unwrap();
    sb.remuda()
        .current_dir(&dir)
        .args(["run", "max", "doctor"])
        .assert()
        .success();
    assert_eq!(sb.only_invocation().cwd, dir.canonicalize().unwrap());
}

// --- exec and errors ---------------------------------------------------------------------

#[test]
fn run_exit_code_is_claudes() {
    let sb = sandbox_with_max();
    sb.remuda()
        .env("FAKE_CLAUDE_EXIT", "3")
        .args(["run", "max", "doctor"])
        .assert()
        .code(3);
    assert_eq!(sb.invocations().len(), 1);
}

#[test]
fn run_fails_when_claude_not_on_path() {
    let sb = sandbox_with_max();
    sb.remuda()
        .env("PATH", "/usr/bin:/bin")
        .args(["run", "max", "doctor"])
        .assert()
        .code(1)
        .stderr(predicate::str::starts_with("remuda: ").and(predicate::str::contains("claude")));
    assert!(sb.invocations().is_empty());
}

#[test]
fn run_reports_exec_failure() {
    let sb = sandbox_with_max();
    // Executable, but its interpreter does not exist: execve fails with ENOENT.
    let bad = sb.root().join("badbin");
    fs::create_dir(&bad).unwrap();
    common::write_executable(&bad.join("claude"), "#!/nonexistent/interpreter\n");
    sb.remuda()
        .env("PATH", format!("{}:/usr/bin:/bin", bad.display()))
        .args(["run", "max", "doctor"])
        .assert()
        .code(1)
        .stderr(predicate::str::starts_with("remuda: "));
}

/// Without an account `run` opens the TUI's account picker (R5, R16), which needs a terminal.
#[test]
fn run_without_account_and_without_a_terminal_explains() {
    let sb = sandbox_with_max();
    sb.remuda().arg("run").assert().code(1).stdout("").stderr(
        "remuda: no account given, and choosing one needs a terminal. \
         Use: remuda run <account> [args...]\n",
    );
    assert!(sb.invocations().is_empty());
    assert!(
        !sb.remuda_home().join("state").exists(),
        "nothing launched or logged"
    );
}

#[test]
fn run_unknown_account_lists_known() {
    let sb = sandbox_with_max();
    sb.remuda()
        .args(["run", "nope", "doctor"])
        .assert()
        .code(1)
        .stderr(
            predicate::str::starts_with("remuda: ")
                .and(predicate::str::contains("claude:default"))
                .and(predicate::str::contains("claude:max")),
        );
    assert!(sb.invocations().is_empty());
}

/// Bare `run` opens the picker and takes nothing else (R16): arguments that would have to be
/// dropped or read as an account name are refused with a usage error, terminal or not.
#[test]
fn run_without_an_account_takes_no_other_arguments() {
    let sb = sandbox_with_max();
    for args in [
        &["run", "--resume", "abc"][..],
        &["run", "--", "-p", "hi"],
        &["run", "-p"],
        &["run", "--help"],
    ] {
        sb.remuda().args(args).assert().code(2).stdout("").stderr(
            predicate::str::starts_with("remuda: ")
                .and(predicate::str::contains("takes no other arguments"))
                .and(predicate::str::contains("remuda run <account> [args...]")),
        );
    }
    assert!(sb.invocations().is_empty());
    assert!(!sb.remuda_home().join("state").exists());
}

/// Account names may start with `-` ([A-Za-z0-9_-]+): a registered one still runs.
#[test]
fn run_a_registered_account_whose_name_starts_with_a_dash() {
    let sb = Sandbox::new();
    sb.write_config("[[account]]\nprovider = \"claude\"\nname = \"-x\"\nhome = \"/p/x\"\n");
    sb.remuda()
        .args(["run", "--", "-x", "--resume", "abc"])
        .assert()
        .success();
    let inv = sb.only_invocation();
    assert_eq!(inv.args, ["--resume", "abc"]);
    assert_eq!(inv.config_dir.as_deref(), Some("/p/x"));
}

// --- codex (R1, R17) -----------------------------------------------------------------------

/// R1: with codex around (`~/.codex` and `codex` on PATH, so `codex:default` exists), a bare
/// `default` still means claude's native login.
#[test]
fn run_default_is_claude_even_with_codex_default_present() {
    let sb = sandbox_with_max();
    sb.install_codex();
    fs::create_dir_all(sb.home().join(".codex")).unwrap();
    sb.remuda()
        .env("CODEX_HOME", "/inherited/codex")
        .args(["run", "default", "doctor"])
        .assert()
        .success();
    let inv = sb.only_invocation();
    assert_eq!(inv.config_dir, None);
    assert_eq!(inv.args, ["doctor"]);
    assert!(sb.codex_invocations().is_empty());
}

/// R1, R17: `codex:default` runs codex with `CODEX_HOME` removed, args verbatim.
#[test]
fn run_codex_default_removes_codex_home() {
    let sb = sandbox_with_max();
    sb.install_codex();
    sb.remuda()
        .env("CODEX_HOME", "/inherited/codex")
        .args(["run", "codex:default", "resume", "--last", "-p"])
        .assert()
        .success();
    let [inv] = sb.codex_invocations().try_into().unwrap();
    assert_eq!(inv.codex_home, None);
    assert_eq!(inv.args, ["resume", "--last", "-p"]);
    assert!(sb.invocations().is_empty(), "claude did not run");
}
