//! R5: bare `remuda` opens the TUI, which needs a terminal.

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
