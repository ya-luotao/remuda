//! R5: bare `remuda` opens the TUI, which needs a terminal; the commands R5 lists, and no other.

mod common;

use common::Sandbox;
use predicates::prelude::*;

#[test]
fn bare_remuda_without_a_terminal_explains_and_exits_1() {
    let sb = Sandbox::new();
    sb.remuda().assert().code(1).stdout("").stderr(
        "remuda: the TUI needs a terminal (try `remuda list`, `remuda sessions`, `remuda usage`)\n",
    );
    // Nothing was started or written.
    assert!(sb.invocations().is_empty());
    assert!(!sb.remuda_home().exists());
}

#[test]
fn help_still_works() {
    let sb = Sandbox::new();
    sb.remuda()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage: remuda"));
}

/// R5, R23: `remuda pick` was removed on 2026-10-28: it is a command clap does not know, a
/// usage error (exit 2) whatever follows it, that runs and writes nothing, also with a
/// registered account whose usage it would have read. The help lists no such command.
#[test]
fn pick_is_not_a_command() {
    let sb = Sandbox::new();
    let max = sb.make_claude_home("h/max");
    sb.register(&[("max", &max)]);
    for args in [
        &["pick"][..],
        &["pick", "--json"],
        &["pick", "--live", "--wait", "--max-wait", "0"],
        &["pick", "--run", "--", "-p", "hi"],
        &["help", "pick"],
    ] {
        sb.remuda()
            .args(args)
            .assert()
            .code(2)
            .stdout("")
            .stderr(predicate::str::starts_with(
                "error: unrecognized subcommand 'pick'\n",
            ));
    }
    assert!(sb.invocations().is_empty());
    assert!(!sb.remuda_home().join("state").exists());
    sb.remuda()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("pick").not());
}
