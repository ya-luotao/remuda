//! R18 (with R3, R6, R13): what a member's launch gets from the shared configuration: the
//! memory variable for a linked `projects`, and nothing else. Nothing is injected (the
//! fallback removed on 2026-10-28) and nothing is written.

mod common;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use common::homes::ClaudeHome;
use common::{Invocation, Sandbox};
use predicates::prelude::*;
use serde_json::json;

const HOOK: &str = r#"{"matcher": "Bash", "hooks": [{"type": "command", "command": "lint"}]}"#;
const MEMORY_DIR_VAR: &str = "CLAUDE_CODE_REMOTE_MEMORY_DIR";

/// A sandbox where `default` (the native `$HOME/.claude`) is the source of shared
/// configuration, holding everything a launch once injected: instructions, rules, settings
/// with a hook and an enabled plugin, an installed plugin, a session store and agent memory.
/// `max` is a member that links nothing, `solo` opts out, `cx` is a codex account.
struct Shared {
    sb: Sandbox,
    source: PathBuf,
    max: PathBuf,
    solo: PathBuf,
}

fn shared() -> Shared {
    let sb = Sandbox::new();
    let install = "plugins/cache/market/tools/1.0.0";
    let source = ClaudeHome::at(sb.home().join(".claude"))
        .skill("review")
        .dir("agents")
        .dir("projects")
        .dir("agent-memory")
        .claude_md("be brief\n")
        .file("rules/style.md", "be terse\n")
        .settings(&format!(
            r#"{{"model": "opus", "cleanupPeriodDays": 365,
                "hooks": {{"PreToolUse": [{HOOK}]}},
                "enabledPlugins": {{"tools@market": true}}}}"#
        ))
        .plugin("tools@market", install, Some("1.0.0"))
        .into_path();
    let max = sb.make_claude_home("max");
    let solo = sb.make_claude_home("solo");
    let cx = sb.make_codex_home("cx");
    sb.write_config(&format!(
        "[[account]]\nprovider = \"claude\"\nname = \"max\"\nhome = \"{}\"\n\n\
         [[account]]\nprovider = \"claude\"\nname = \"solo\"\nhome = \"{}\"\nshare = false\n\n\
         [[account]]\nprovider = \"codex\"\nname = \"cx\"\nhome = \"{}\"\n\n\
         [share.claude]\nfrom = \"default\"\n",
        max.display(),
        solo.display(),
        cx.display()
    ));
    Shared {
        sb,
        source,
        max,
        solo,
    }
}

impl Shared {
    /// `remuda run <args>`, which must succeed; what claude got.
    fn run(&self, args: &[&str]) -> Invocation {
        let _ = fs::remove_file(self.sb.claude_out());
        self.sb.remuda().arg("run").args(args).assert().success();
        self.sb.only_invocation()
    }

    /// `max/<item>` as a symlink to the source's.
    fn link(&self, item: &str) {
        symlink(self.source.join(item), self.max.join(item)).unwrap();
    }
}

/// Every entry below `dir`, recursively, with its type, size and link target: a snapshot to
/// prove a tree was not touched.
fn tree(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let Ok(listing) = fs::read_dir(dir) else {
        return out;
    };
    for entry in listing {
        let path = entry.unwrap().path();
        let meta = fs::symlink_metadata(&path).unwrap();
        let kind = if meta.file_type().is_symlink() {
            format!("-> {}", fs::read_link(&path).unwrap().display())
        } else if meta.is_dir() {
            out.extend(tree(&path));
            "dir".to_string()
        } else {
            format!("file {}", meta.len())
        };
        out.push((path, kind));
    }
    out.sort();
    out
}

/// The options a launch once injected (R18, before 2026-10-28), in the `--option=value` form
/// it used; none of them may come back. (The user's own, in either form, pass through.)
fn injected(inv: &Invocation) -> Vec<&String> {
    inv.args
        .iter()
        .filter(|a| {
            ["--add-dir=", "--settings=", "--plugin-dir="]
                .iter()
                .any(|o| a.starts_with(o))
        })
        .collect()
}

/// R6, R18: a member whose home links nothing gets nothing from the source: its arguments
/// as typed, with R6's `--session-id` after them, and no `--add-dir`, `--settings` or
/// `--plugin-dir`; R6's home variable, and no other change to the environment; a log line
/// without a `shared` field. Nothing is said of a `--settings` of its own.
#[test]
fn a_member_without_links_gets_nothing_but_r6() {
    let s = shared();
    let inv = s.run(&["max", "-p", "hi", "--add-dir", "/mine"]);
    assert_eq!(
        inv.args[..5],
        ["-p", "hi", "--add-dir", "/mine", "--session-id"]
    );
    assert_eq!(inv.args.len(), 6, "{:?}", inv.args);
    uuid::Uuid::parse_str(&inv.args[5]).expect("a session id");
    assert_eq!(injected(&inv), Vec::<&String>::new());
    assert_eq!(inv.config_dir.as_deref(), Some(s.max.to_str().unwrap()));
    assert_eq!(inv.add_dir_claude_md, None);
    assert_eq!(inv.memory_dir, None);
    let log = s.sb.launches();
    assert_eq!(log[0]["args"], json!(["-p", "hi", "--add-dir", "/mine"]));
    assert_eq!(log[0]["session_id"], json!(inv.args[5]));
    assert!(log[0].get("shared").is_none(), "{}", log[0]);

    // Resumes, continues and forks are sessions; subcommands, help and version are not:
    // all alike get nothing from the source.
    for args in [
        &["max", "--resume", "abc"][..],
        &["max", "-c"],
        &["max", "--resume", "abc", "--fork-session"],
        &["max", "agents", "--json"],
        &["max", "--help"],
        &["max", "-v"],
        &["max", "-p", "update"],
    ] {
        let inv = s.run(args);
        assert_eq!(injected(&inv), Vec::<&String>::new(), "{args:?}");
        assert_eq!(inv.add_dir_claude_md, None, "{args:?}");
        assert_eq!(inv.memory_dir, None, "{args:?}");
        assert!(
            inv.args
                .starts_with(&args[1..].iter().map(|a| a.to_string()).collect::<Vec<_>>()),
            "{args:?}: {:?}",
            inv.args
        );
    }
    // The user's own `--settings`: nothing would have been set, so nothing is said.
    let _ = fs::remove_file(s.sb.claude_out());
    s.sb.remuda()
        .args(["run", "max", "--settings", "/my.json"])
        .assert()
        .success()
        .stderr(predicate::str::contains("given").not());
    let inv = s.sb.only_invocation();
    assert_eq!(inv.args[..2], ["--settings", "/my.json"]);
    assert_eq!(inv.memory_dir, None);
}

/// R13, R18: a launch writes nothing below `$REMUDA_HOME` but its line in the launch log: no
/// `shared/`, no `state/settings/`, and nothing into any home.
#[test]
fn a_launch_writes_only_the_launch_log() {
    let s = shared();
    let remuda = s.sb.remuda_home();
    let paths = |tree: &[(PathBuf, String)]| -> Vec<PathBuf> {
        tree.iter().map(|(p, _)| p.clone()).collect()
    };
    let before = tree(&remuda);
    s.run(&["max"]);
    let after = tree(&remuda);
    let state = remuda.join("state");
    let mut added: Vec<PathBuf> = paths(&after)
        .into_iter()
        .filter(|p| !paths(&before).contains(p))
        .collect();
    added.sort();
    assert_eq!(added, [state.clone(), s.sb.launch_log()], "{after:?}");

    // A second launch, as a member with a linked `projects` this time, adds one line.
    s.link("projects");
    let before = (tree(&remuda), tree(&s.source), tree(&s.max));
    let log_before = fs::read_to_string(s.sb.launch_log()).unwrap();
    s.run(&["max", "-p", "hi"]);
    let after = (tree(&remuda), tree(&s.source), tree(&s.max));
    assert_eq!(after.1, before.1, "the source is not touched");
    assert_eq!(after.2, before.2, "the member's home is not touched");
    assert_eq!(
        paths(&after.0),
        paths(&before.0),
        "nothing new below $REMUDA_HOME"
    );
    let changed: Vec<&PathBuf> = after
        .0
        .iter()
        .filter(|entry| !before.0.contains(entry))
        .map(|(p, _)| p)
        .collect();
    assert_eq!(changed, [&s.sb.launch_log()]);
    let log_after = fs::read_to_string(s.sb.launch_log()).unwrap();
    assert!(log_after.starts_with(&log_before));
    assert_eq!(log_after.lines().count(), 2);
    assert!(!remuda.join("shared").exists());
    assert!(!state.join("settings").exists());
}

/// R18: a member whose `projects` is the source's gets `CLAUDE_CODE_REMOTE_MEMORY_DIR`, the
/// source's home as registered, so that claude writes its memory by the source's path rather
/// than through the link; nothing else, nothing is created, and it goes with every session
/// invocation (R6) and not with a subcommand. Not set when the user passes `--settings` or
/// `--setting-sources` (either form, said on stderr), sets the variable themselves (silently),
/// or a settings file (the home's, the source's, the project's) chooses `autoMemoryDirectory`;
/// set again when the source's settings cannot be read, and whether or not `agent-memory` is
/// linked too.
#[test]
fn a_linked_projects_gets_the_memory_variable() {
    let s = shared();
    s.link("projects");
    let source = s.source.to_str().unwrap();
    let inv = s.run(&["max", "-p", "hi"]);
    assert_eq!(inv.args[..3], ["-p", "hi", "--session-id"]);
    assert_eq!(inv.args.len(), 4, "{:?}", inv.args);
    assert_eq!(inv.memory_dir.as_deref(), Some(source));
    assert_eq!(inv.add_dir_claude_md, None);
    assert_eq!(injected(&inv), Vec::<&String>::new());
    assert!(s.sb.launches()[0].get("shared").is_none());
    assert_eq!(
        tree(&s.source.join("agent-memory")),
        [],
        "nothing is created"
    );

    for args in [
        &["max", "--resume", "abc"][..],
        &["max", "-c"],
        &["max", "--resume", "abc", "--fork-session"],
    ] {
        assert_eq!(s.run(args).memory_dir.as_deref(), Some(source), "{args:?}");
    }
    for args in [&["max", "auth", "status"][..], &["max", "--version"]] {
        assert_eq!(s.run(args).memory_dir, None, "{args:?}");
    }

    // The user's own settings: remuda does not know whether they choose the location.
    for form in [
        &["--settings", "/my.json"][..],
        &["--settings={\"a\":1}"],
        &["--setting-sources", "user"],
        &["--setting-sources=user,local"],
    ] {
        let _ = fs::remove_file(s.sb.claude_out());
        let option = form[0].split('=').next().unwrap();
        s.sb.remuda()
            .args(["run", "max"])
            .args(form)
            .assert()
            .success()
            .stderr(predicate::str::contains(format!(
                "remuda: {option} given: {MEMORY_DIR_VAR} is not set for this session\n"
            )));
        let inv = s.sb.only_invocation();
        assert_eq!(inv.memory_dir, None, "{form:?}");
        assert_eq!(inv.args[..form.len()], *form, "the user's own, as typed");
        assert_eq!(inv.args.len(), form.len() + 2, "{:?}", inv.args);
    }
    // Already set by the user: theirs, and nothing is said.
    let _ = fs::remove_file(s.sb.claude_out());
    s.sb.remuda()
        .env(MEMORY_DIR_VAR, "/theirs")
        .args(["run", "max", "--settings", "/my.json"])
        .assert()
        .success()
        .stderr(predicate::str::contains("given").not());
    assert_eq!(
        s.sb.only_invocation().memory_dir.as_deref(),
        Some("/theirs")
    );
    // A settings file chooses the location: the home's own ...
    fs::write(
        s.max.join("settings.json"),
        r#"{"autoMemoryDirectory": "/mine"}"#,
    )
    .unwrap();
    assert_eq!(s.run(&["max"]).memory_dir, None);
    fs::remove_file(s.max.join("settings.json")).unwrap();
    // ... the source's ...
    let settings = fs::read_to_string(s.source.join("settings.json")).unwrap();
    fs::write(
        s.source.join("settings.json"),
        r#"{"autoMemoryDirectory": "/theirs"}"#,
    )
    .unwrap();
    assert_eq!(s.run(&["max"]).memory_dir, None);
    // ... or the project's.
    fs::write(s.source.join("settings.json"), &settings).unwrap();
    let project = s.sb.work().join(".claude");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("settings.local.json"),
        r#"{"autoMemoryDirectory": "/p"}"#,
    )
    .unwrap();
    assert_eq!(s.run(&["max"]).memory_dir, None);
    fs::remove_dir_all(&project).unwrap();
    assert_eq!(s.run(&["max"]).memory_dir.as_deref(), Some(source));
    // The source's settings cannot be read: no location is seen, and the launch goes on.
    fs::write(s.source.join("settings.json"), "[1, 2]").unwrap();
    assert_eq!(s.run(&["max"]).memory_dir.as_deref(), Some(source));
    assert!(!s.sb.launches().is_empty());
    // `agent-memory` linked too: the same directory either way.
    s.link("agent-memory");
    assert_eq!(s.run(&["max"]).memory_dir.as_deref(), Some(source));
}

/// R18: the variable an outer remuda launch set (the source's home) is decided again for
/// every launch, not inherited: removed for the source, an opted-out account and a member
/// whose `projects` is its own, set again for a member whose `projects` is the source's. A
/// value of the user's own passes through to everyone.
#[test]
fn an_inherited_memory_variable_is_decided_again() {
    let s = shared();
    let source = s.source.to_str().unwrap();
    for account in ["default", "solo", "max"] {
        for (value, kept) in [(source, None), ("/theirs", Some("/theirs"))] {
            let _ = fs::remove_file(s.sb.claude_out());
            s.sb.remuda()
                .env(MEMORY_DIR_VAR, value)
                .args(["run", account])
                .assert()
                .success();
            assert_eq!(
                s.sb.only_invocation().memory_dir.as_deref(),
                kept,
                "{account} with {value}"
            );
        }
    }
    s.link("projects");
    let _ = fs::remove_file(s.sb.claude_out());
    s.sb.remuda()
        .env(MEMORY_DIR_VAR, source)
        .args(["run", "max"])
        .assert()
        .success();
    assert_eq!(s.sb.only_invocation().memory_dir.as_deref(), Some(source));
}

/// R18: the source itself, an account with `share = false`, and codex accounts get nothing,
/// whatever their `projects` links.
#[test]
fn the_source_opted_out_and_codex_accounts_get_nothing() {
    let s = shared();
    symlink(s.source.join("projects"), s.solo.join("projects")).unwrap();
    for account in ["default", "solo"] {
        let inv = s.run(&[account, "-p", "hi"]);
        assert_eq!(inv.args[..3], ["-p", "hi", "--session-id"], "{account}");
        assert_eq!(inv.args.len(), 4);
        assert_eq!(inv.add_dir_claude_md, None);
        assert_eq!(inv.memory_dir, None, "{account}");
    }
    s.sb.install_codex();
    s.sb.remuda()
        .args(["run", "codex:cx", "-p", "hi"])
        .assert()
        .success();
    let [inv] = s.sb.codex_invocations().try_into().unwrap();
    assert_eq!(inv.args, ["-p", "hi"]);
    assert!(
        !s.solo.join("settings.json").exists() && !s.sb.remuda_home().join("shared").exists(),
        "nothing written"
    );
}

/// Without `[share.claude]` nothing is added anywhere, a linked `projects` or not.
#[test]
fn nothing_is_shared_without_a_source() {
    let s = shared();
    s.link("projects");
    let config = s.sb.read_config();
    let cut = config.find("[share.claude]").unwrap();
    s.sb.write_config(&config[..cut]);
    let inv = s.run(&["max", "-p", "hi"]);
    assert_eq!(inv.args[..2], ["-p", "hi"]);
    assert_eq!(inv.memory_dir, None);
    assert_eq!(inv.add_dir_claude_md, None);
}
