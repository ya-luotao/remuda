//! R18 (with R3, R6): shared configuration injected by `remuda run`.

mod common;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::{Invocation, Sandbox};
use predicates::prelude::*;
use serde_json::{Value, json};
use unicode_normalization::UnicodeNormalization;

const HOOK: &str = r#"{"matcher": "Bash", "hooks": [{"type": "command", "command": "lint"}]}"#;

/// A sandbox where `default` (the native `$HOME/.claude`) is the source of shared
/// configuration: instructions, settings with a hook and an enabled plugin, an installed
/// plugin. `max` is a member, `solo` opts out, `cx` is a codex account.
struct Shared {
    sb: Sandbox,
    source: PathBuf,
    max: PathBuf,
    solo: PathBuf,
    plugin: PathBuf,
}

fn shared() -> Shared {
    let sb = Sandbox::new();
    let source = sb.home().join(".claude");
    fs::create_dir_all(source.join("skills/review")).unwrap();
    fs::create_dir_all(source.join("agents")).unwrap();
    fs::create_dir_all(source.join("projects")).unwrap();
    fs::write(source.join("CLAUDE.md"), "be brief\n").unwrap();
    fs::write(
        source.join("settings.json"),
        format!(
            r#"{{"model": "opus", "cleanupPeriodDays": 365,
                "hooks": {{"PreToolUse": [{HOOK}]}},
                "enabledPlugins": {{"tools@market": true, "off@market": false}}}}"#
        ),
    )
    .unwrap();
    let plugin = source.join("plugins/cache/market/tools/1.0.0");
    fs::create_dir_all(&plugin).unwrap();
    fs::write(
        source.join("plugins/installed_plugins.json"),
        json!({
            "version": 2,
            "plugins": {
                "tools@market": [{"scope": "user", "installPath": plugin, "version": "1.0.0"}],
                "off@market": [{"scope": "user", "installPath": plugin}],
            },
        })
        .to_string(),
    )
    .unwrap();
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
        plugin,
    }
}

impl Shared {
    fn shared_dir(&self) -> PathBuf {
        self.sb.remuda_home().join("shared/claude")
    }

    fn run(&self, args: &[&str]) -> Invocation {
        self.sb.remuda().arg("run").args(args).assert().success();
        self.sb.only_invocation()
    }
}

/// The settings file passed as `--settings=<path>` among `args`, parsed (R18: never inline).
fn settings_of(args: &[String]) -> Option<Value> {
    let found: Vec<&String> = args
        .iter()
        .filter(|a| a.starts_with("--settings"))
        .collect();
    assert!(found.len() <= 1, "one --settings at most: {args:?}");
    found.first().map(|a| {
        let path = Path::new(a.strip_prefix("--settings=").unwrap());
        assert!(path.is_absolute(), "a file, not inline JSON: {a}");
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    })
}

/// claude's memory directory for a project root (R18): NFC, then every UTF-16 code unit that
/// is not an ASCII letter or digit becomes `-`.
fn memory_of(source: &Path, root: &Path) -> String {
    let project: String = root
        .to_str()
        .unwrap()
        .nfc()
        .collect::<String>()
        .encode_utf16()
        .map(|u| match u8::try_from(u) {
            Ok(b) if b.is_ascii_alphanumeric() => char::from(b),
            _ => '-',
        })
        .collect();
    format!("{}/projects/{project}/memory", source.display())
}

/// Sorted entry names of a directory.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// The item links under `<shared>/.claude` (R18), as `(name, target)`, in remuda's item order.
fn item_links(shared: &Path) -> Vec<(&'static str, PathBuf)> {
    let root = shared.join(".claude");
    assert!(
        fs::symlink_metadata(&root).unwrap().file_type().is_dir(),
        "{} is a directory",
        root.display()
    );
    ["CLAUDE.md", "skills", "commands", "agents"]
        .into_iter()
        .filter_map(|item| {
            let link = root.join(item);
            let meta = fs::symlink_metadata(&link).ok()?;
            assert!(meta.file_type().is_symlink(), "{}", link.display());
            Some((item, fs::read_link(&link).unwrap()))
        })
        .collect()
}

/// Every entry under `dir`, recursively, with its type and link target: a snapshot to prove a
/// tree was not touched.
fn tree(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    for name in names(dir) {
        let path = dir.join(&name);
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

fn injected(inv: &Invocation) -> Vec<&String> {
    inv.args
        .iter()
        .filter(|a| {
            ["--add-dir=", "--settings=", "--plugin-dir="]
                .iter()
                .any(|p| a.starts_with(p))
        })
        .collect()
}

/// R18: a member's session gets instructions, the settings its home lacks (with the
/// auto-memory location) and the enabled plugins, as `--option=value` before the user's
/// arguments; the variable goes into claude's environment; the log keeps only option names
/// and sizes.
#[test]
fn a_member_session_gets_the_source_configuration_before_its_arguments() {
    let s = shared();
    let inv = s.run(&["max", "-p", "hi", "--add-dir", "/mine"]);
    let shared_dir = s.shared_dir();
    let settings = settings_of(&inv.args).expect("settings injected");
    let work = s.sb.work().canonicalize().unwrap();
    assert_eq!(
        settings,
        json!({
            "model": "opus",
            "cleanupPeriodDays": 365,
            "hooks": {"PreToolUse": [serde_json::from_str::<Value>(HOOK).unwrap()]},
            "enabledPlugins": {"tools@market": true, "off@market": false},
            "autoMemoryDirectory": memory_of(&s.source, &work),
        })
    );
    assert_eq!(inv.args[0], format!("--add-dir={}", shared_dir.display()));
    assert!(inv.args[1].starts_with("--settings="));
    assert_eq!(inv.args[2], format!("--plugin-dir={}", s.plugin.display()));
    assert_eq!(inv.args[3..7], ["-p", "hi", "--add-dir", "/mine"]);
    assert_eq!(inv.args[7], "--session-id");
    assert_eq!(inv.args.len(), 9, "{:?}", inv.args);
    assert_eq!(inv.add_dir_claude_md.as_deref(), Some("1"));
    assert_eq!(inv.config_dir.as_deref(), Some(s.max.to_str().unwrap()));
    assert_eq!(
        item_links(&shared_dir),
        [
            ("CLAUDE.md", s.source.join("CLAUDE.md")),
            ("skills", s.source.join("skills")),
            ("agents", s.source.join("agents")),
        ],
        "$REMUDA_HOME/shared/claude/.claude holds one link per item the source has"
    );

    let log = s.sb.launches();
    assert_eq!(log[0]["args"], json!(["-p", "hi", "--add-dir", "/mine"]));
    assert_eq!(log[0]["session_id"], json!(inv.args[8]));
    assert_eq!(
        log[0]["shared"],
        json!([
            {"option": "--add-dir", "bytes": shared_dir.to_str().unwrap().len()},
            {"option": "--settings", "bytes": inv.args[1].len() - "--settings=".len()},
            {"option": "--plugin-dir", "bytes": s.plugin.to_str().unwrap().len()},
        ])
    );
    let line = fs::read_to_string(s.sb.launch_log()).unwrap();
    assert!(!line.contains("opus"), "values are not logged: {line}");
}

/// R6, R18: resumes, continues and forks are sessions and get the injection too.
#[test]
fn resumes_and_forks_are_sessions() {
    for args in [
        &["max", "--resume", "abc"][..],
        &["max", "-c"],
        &["max", "--resume", "abc", "--fork-session"],
    ] {
        let s = shared();
        let inv = s.run(args);
        assert_eq!(injected(&inv).len(), 3, "{args:?}: {:?}", inv.args);
        assert_eq!(inv.args[3..2 + args.len()], args[1..], "{args:?}");
    }
}

/// R6, R18: subcommands, help and version are passed through untouched.
#[test]
fn non_sessions_get_nothing() {
    for args in [
        &["max", "agents", "--json"][..],
        &["max", "--help"],
        &["max", "-h"],
        &["max", "--version"],
        &["max", "-v"],
        &["max", "-p", "update"],
    ] {
        let s = shared();
        let inv = s.run(args);
        assert_eq!(inv.args, args[1..], "{args:?}");
        assert_eq!(inv.add_dir_claude_md, None);
        assert!(!s.shared_dir().exists(), "{args:?}: nothing prepared");
        assert!(s.sb.launches()[0].get("shared").is_none());
    }
}

/// R18: the source itself, an account with `share = false`, and codex accounts get nothing.
#[test]
fn the_source_opted_out_and_codex_accounts_get_nothing() {
    for account in ["default", "solo"] {
        let s = shared();
        let inv = s.run(&[account, "-p", "hi"]);
        assert_eq!(inv.args[..3], ["-p", "hi", "--session-id"], "{account}");
        assert_eq!(inv.args.len(), 4);
        assert_eq!(inv.add_dir_claude_md, None);
        assert!(!s.shared_dir().exists());
    }
    let s = shared();
    s.sb.install_codex();
    s.sb.remuda()
        .args(["run", "codex:cx", "-p", "hi"])
        .assert()
        .success();
    let [inv] = s.sb.codex_invocations().try_into().unwrap();
    assert_eq!(inv.args, ["-p", "hi"]);
    assert!(
        !s.solo.join("settings.json").exists(),
        "nothing written into homes"
    );
}

/// Without `[share.claude]` nothing is injected anywhere.
#[test]
fn nothing_is_shared_without_a_source() {
    let s = shared();
    let config = s.sb.read_config();
    let cut = config.find("[share.claude]").unwrap();
    s.sb.write_config(&config[..cut]);
    let inv = s.run(&["max", "-p", "hi"]);
    assert_eq!(inv.args[..2], ["-p", "hi"]);
    assert_eq!(inv.add_dir_claude_md, None);
}

/// R12, R18: a home that already shares everything with the source through symlinks gets
/// nothing injected.
#[test]
fn a_symlinked_home_gets_nothing() {
    let s = shared();
    for item in [
        "CLAUDE.md",
        "skills",
        "agents",
        "settings.json",
        "plugins",
        "projects",
    ] {
        symlink(s.source.join(item), s.max.join(item)).unwrap();
    }
    let inv = s.run(&["max", "-p", "hi"]);
    assert_eq!(inv.args[..3], ["-p", "hi", "--session-id"]);
    assert_eq!(inv.args.len(), 4, "{:?}", inv.args);
    assert_eq!(inv.add_dir_claude_md, None);
    assert!(s.sb.launches()[0].get("shared").is_none());
}

/// R18: each component on its own: instructions shared by symlink leave `--add-dir` out;
/// shared `projects` leaves auto-memory out; shared `plugins` leaves `--plugin-dir` out.
#[test]
fn components_are_skipped_one_by_one() {
    let s = shared();
    for item in ["CLAUDE.md", "skills", "agents", "projects", "plugins"] {
        symlink(s.source.join(item), s.max.join(item)).unwrap();
    }
    let inv = s.run(&["max"]);
    assert_eq!(inv.add_dir_claude_md, None);
    let args = injected(&inv);
    assert_eq!(args.len(), 1, "{:?}", inv.args);
    let settings = settings_of(&inv.args).unwrap();
    assert_eq!(settings["model"], json!("opus"));
    assert!(settings.get("autoMemoryDirectory").is_none(), "{settings}");

    // Some but not all instruction items shared: `--add-dir` again (R11 warns).
    fs::remove_file(s.max.join("agents")).unwrap();
    fs::remove_file(s.sb.claude_out()).unwrap();
    let inv = s.run(&["max"]);
    assert_eq!(inv.add_dir_claude_md.as_deref(), Some("1"));
    assert!(inv.args[0].starts_with("--add-dir="));
}

/// R18: the home's own settings win: a scalar it sets is not injected, an identical hook is
/// not added twice, and its own `autoMemoryDirectory` is kept.
#[test]
fn the_homes_own_settings_keep_precedence() {
    let s = shared();
    fs::write(
        s.max.join("settings.json"),
        format!(
            r#"{{"model": "sonnet", "hooks": {{"PreToolUse": [{HOOK}]}},
                "autoMemoryDirectory": "/mine"}}"#
        ),
    )
    .unwrap();
    let inv = s.run(&["max"]);
    assert_eq!(
        settings_of(&inv.args).unwrap(),
        json!({
            "cleanupPeriodDays": 365,
            "enabledPlugins": {"tools@market": true, "off@market": false},
        })
    );
}

/// R18: the source's own `autoMemoryDirectory` is not overridden.
#[test]
fn the_sources_memory_location_is_kept() {
    let s = shared();
    fs::write(
        s.source.join("settings.json"),
        r#"{"autoMemoryDirectory": "/theirs"}"#,
    )
    .unwrap();
    let inv = s.run(&["max"]);
    assert_eq!(
        settings_of(&inv.args).unwrap(),
        json!({"autoMemoryDirectory": "/theirs"})
    );
}

/// R18: the user's own `--settings` (either form) means no injected settings and no
/// auto-memory, said on stderr; instructions and plugins are still injected.
#[test]
fn the_users_settings_suppress_injected_settings() {
    for form in [&["--settings", "/my.json"][..], &["--settings={\"a\":1}"]] {
        let s = shared();
        let mut args = vec!["run", "max"];
        args.extend_from_slice(form);
        s.sb.remuda()
            .args(&args)
            .assert()
            .success()
            .stderr(predicate::str::contains(
                "--settings given: settings and auto-memory from claude:default are not injected",
            ));
        let inv = s.sb.only_invocation();
        assert!(inv.args[0].starts_with("--add-dir="), "{:?}", inv.args);
        assert!(inv.args[1].starts_with("--plugin-dir="), "{:?}", inv.args);
        assert_eq!(inv.args[2..2 + form.len()], *form);
        let log = s.sb.launches();
        let options: Vec<&str> = log[0]["shared"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["option"].as_str().unwrap())
            .collect();
        assert_eq!(options, ["--add-dir", "--plugin-dir"]);
    }
}

/// R18: an unrecognized `installed_plugins.json` means no plugins, not a failed launch.
#[test]
fn an_unrecognized_plugin_file_injects_no_plugins() {
    let s = shared();
    fs::write(
        s.source.join("plugins/installed_plugins.json"),
        r#"{"version": 3, "plugins": []}"#,
    )
    .unwrap();
    let inv = s.run(&["max"]);
    assert!(
        !inv.args.iter().any(|a| a.starts_with("--plugin-dir")),
        "{:?}",
        inv.args
    );
    assert!(settings_of(&inv.args).is_some());
}

/// R18: a settings file that is not a JSON object fails the launch before anything runs or
/// is logged.
#[test]
fn a_settings_file_that_is_not_an_object_fails_the_launch() {
    let s = shared();
    fs::write(s.source.join("settings.json"), "[1, 2]").unwrap();
    s.sb.remuda()
        .args(["run", "max"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "settings.json is not a JSON object",
        ));
    assert!(s.sb.invocations().is_empty());
    assert!(!s.sb.launch_log().exists());
}

/// R18: a `$REMUDA_HOME/shared/claude/.claude` from before, one symlink to the whole source
/// home, is migrated by a member's launch to the directory of item links; the source home
/// and the other entries of `shared/claude` are not touched, and nothing temporary is left.
#[test]
fn the_old_whole_home_link_is_migrated() {
    let s = shared();
    let dir = s.shared_dir();
    fs::create_dir_all(&dir).unwrap();
    symlink(&s.source, dir.join(".claude")).unwrap();
    fs::write(dir.join("notes"), "mine").unwrap();
    let source_before = tree(&s.source);
    let inv = s.run(&["max"]);
    assert_eq!(inv.args[0], format!("--add-dir={}", dir.display()));
    assert_eq!(inv.add_dir_claude_md.as_deref(), Some("1"));
    assert_eq!(
        item_links(&dir),
        [
            ("CLAUDE.md", s.source.join("CLAUDE.md")),
            ("skills", s.source.join("skills")),
            ("agents", s.source.join("agents")),
        ]
    );
    assert_eq!(names(&dir), [".claude", "notes"]);
    assert_eq!(
        names(&dir.join(".claude")),
        ["CLAUDE.md", "agents", "skills"]
    );
    assert_eq!(fs::read_to_string(dir.join("notes")).unwrap(), "mine");
    assert_eq!(tree(&s.source), source_before);
    assert!(!dir.join(".claude/settings.json").exists());
    assert!(!dir.join(".claude/projects").exists());
}

/// R18: a link with another target is corrected and a link the source has no item for is
/// removed, when a member launches; a `.claude` that is already right is not rewritten.
#[test]
fn the_item_links_are_corrected() {
    let s = shared();
    let root = s.shared_dir().join(".claude");
    fs::create_dir_all(&root).unwrap();
    symlink("/elsewhere/CLAUDE.md", root.join("CLAUDE.md")).unwrap();
    symlink(s.source.join("commands"), root.join("commands")).unwrap();
    s.run(&["max"]);
    assert_eq!(
        item_links(&s.shared_dir()),
        [
            ("CLAUDE.md", s.source.join("CLAUDE.md")),
            ("skills", s.source.join("skills")),
            ("agents", s.source.join("agents")),
        ]
    );
    let stamp = |name: &str| {
        let m = fs::symlink_metadata(root.join(name)).unwrap();
        (m.ino(), m.modified().unwrap())
    };
    let before: Vec<_> = ["", "CLAUDE.md", "skills", "agents"]
        .iter()
        .map(|n| stamp(n))
        .collect();
    std::thread::sleep(std::time::Duration::from_millis(20));
    s.sb.remuda().args(["run", "max"]).assert().success();
    assert_eq!(s.sb.invocations().len(), 2);
    let after: Vec<_> = ["", "CLAUDE.md", "skills", "agents"]
        .iter()
        .map(|n| stamp(n))
        .collect();
    assert_eq!(before, after, "nothing rewritten when already right");
    assert_eq!(names(&s.shared_dir()), [".claude"]);
}

/// R18: an entry that is not a symlink is left alone, whether it is an item inside `.claude`
/// or `.claude` itself: the launch goes on without instructions, and says why.
#[test]
fn a_shared_entry_that_is_not_a_link_is_left_alone() {
    let s = shared();
    let dir = s.shared_dir();
    let root = dir.join(".claude");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("CLAUDE.md"), "a copy").unwrap();
    s.sb.remuda()
        .args(["run", "max"])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "CLAUDE.md is not a symlink; remuda does not replace it",
        ));
    let inv = s.sb.only_invocation();
    assert_eq!(inv.add_dir_claude_md, None);
    assert!(!inv.args[0].starts_with("--add-dir="), "{:?}", inv.args);
    assert!(settings_of(&inv.args).is_some());
    assert_eq!(
        fs::read_to_string(root.join("CLAUDE.md")).unwrap(),
        "a copy"
    );
    assert_eq!(names(&root), ["CLAUDE.md"], "nothing else was created");

    fs::remove_dir_all(&root).unwrap();
    fs::write(&root, "what").unwrap();
    s.sb.remuda()
        .args(["run", "max"])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "is neither a directory nor a symlink; remuda does not replace it",
        ));
    assert_eq!(fs::read_to_string(&root).unwrap(), "what");
    assert_eq!(names(&dir), [".claude"]);
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .env("HOME", dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// R18: auto-memory is keyed by the main repository's root: from a subdirectory and from a
/// worktree alike.
#[test]
fn memory_is_keyed_by_the_main_repository() {
    let s = shared();
    let repo = s.sb.root().canonicalize().unwrap().join("the repo");
    fs::create_dir_all(repo.join("src/deep")).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
    let tree = s.sb.root().canonicalize().unwrap().join("tree");
    git(&repo, &["worktree", "add", "-q", tree.to_str().unwrap()]);
    for cwd in [repo.join("src/deep"), tree] {
        let _ = fs::remove_file(s.sb.claude_out());
        s.sb.remuda()
            .current_dir(&cwd)
            .args(["run", "max"])
            .assert()
            .success();
        let inv = s.sb.only_invocation();
        assert_eq!(
            settings_of(&inv.args).unwrap()["autoMemoryDirectory"],
            json!(memory_of(&s.source, &repo)),
            "from {}",
            cwd.display()
        );
    }
}

/// R18: no auto-memory for a project name over 200 characters (its encoding is unverified).
#[test]
fn no_memory_for_a_long_project_name() {
    let s = shared();
    let deep = s.sb.root().canonicalize().unwrap().join("d".repeat(200));
    fs::create_dir_all(&deep).unwrap();
    s.sb.remuda()
        .current_dir(&deep)
        .args(["run", "max"])
        .assert()
        .success();
    let settings = settings_of(&s.sb.only_invocation().args).unwrap();
    assert!(settings.get("autoMemoryDirectory").is_none(), "{settings}");
}

/// R18: settings travel in a 0600 file under `$REMUDA_HOME/state/settings/`, named by the
/// content's hash and reused; no settings value appears in claude's arguments.
#[test]
fn settings_go_through_a_private_file() {
    let s = shared();
    let first = s.run(&["max"]);
    assert!(
        !first.args.iter().any(|a| a.contains("opus")),
        "{:?}",
        first.args
    );
    let path = first.args[1]
        .strip_prefix("--settings=")
        .unwrap()
        .to_string();
    assert!(
        Path::new(&path).starts_with(s.sb.remuda_home().join("state/settings")),
        "{path}"
    );
    fs::remove_file(s.sb.claude_out()).unwrap();
    let again = s.run(&["max"]);
    assert_eq!(
        again.args[1], first.args[1],
        "the same content reuses its file"
    );
    let mut files: Vec<String> = fs::read_dir(s.sb.remuda_home().join("state/settings"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    files.sort();
    let name = Path::new(&path).file_name().unwrap().to_str().unwrap();
    // One settings file, and the lock that guards the directory.
    assert_eq!(files, [".lock", name]);
}

/// R18: authentication keys of the source never reach a member.
#[test]
fn authentication_keys_are_withheld() {
    let s = shared();
    fs::write(
        s.source.join("settings.json"),
        r#"{"model": "opus", "apiKeyHelper": "/bin/key", "forceLoginMethod": "console",
            "env": {"ANTHROPIC_API_KEY": "sk-x", "CLAUDE_CODE_USE_BEDROCK": "1", "TZ": "UTC",
                    "CLAUDE_CODE_USE_MANTLE": "1", "AWS_BEARER_TOKEN_BEDROCK": "b",
                    "ANTHROPIC_AWS_API_KEY": "k", "ANTHROPIC_MODEL": "opus",
                    "DISABLE_TELEMETRY": "1"}}"#,
    )
    .unwrap();
    let inv = s.run(&["max"]);
    let settings = settings_of(&inv.args).unwrap();
    assert_eq!(settings["model"], json!("opus"));
    assert_eq!(
        settings["env"],
        json!({"TZ": "UTC", "ANTHROPIC_MODEL": "opus", "DISABLE_TELEMETRY": "1"})
    );
    assert!(settings.get("apiKeyHelper").is_none() && settings.get("forceLoginMethod").is_none());
    let path = inv.args[1].strip_prefix("--settings=").unwrap();
    assert!(!fs::read_to_string(path).unwrap().contains("sk-x"));
}

/// R18: the project's own settings keep precedence over the shared ones.
#[test]
fn project_settings_keep_precedence() {
    let s = shared();
    let claude_dir = s.sb.work().join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    fs::write(claude_dir.join("settings.json"), r#"{"model": "haiku"}"#).unwrap();
    fs::write(
        claude_dir.join("settings.local.json"),
        format!(r#"{{"hooks": {{"PreToolUse": [{HOOK}]}}, "cleanupPeriodDays": 7}}"#),
    )
    .unwrap();
    let settings = settings_of(&s.run(&["max"]).args).unwrap();
    for gone in ["model", "hooks", "cleanupPeriodDays"] {
        assert!(settings.get(gone).is_none(), "{gone}: {settings}");
    }
    assert!(settings.get("enabledPlugins").is_some());
}

/// R18: a directory outside the Basic Multilingual Plane is two UTF-16 units, two dashes.
#[test]
fn memory_encodes_utf16_code_units() {
    let s = shared();
    let dir = s.sb.root().canonicalize().unwrap().join("📁x");
    fs::create_dir_all(&dir).unwrap();
    s.sb.remuda()
        .current_dir(&dir)
        .args(["run", "max"])
        .assert()
        .success();
    let settings = settings_of(&s.sb.only_invocation().args).unwrap();
    let memory = settings["autoMemoryDirectory"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(memory.ends_with("---x/memory"), "{memory}");
    assert_eq!(memory, memory_of(&s.source, &dir));
}

/// R18: remuda runs no git to find the project root: a `git` on PATH is never called, and a
/// `GIT_DIR` in the environment changes nothing (claude walks the file system too).
#[test]
fn the_project_root_comes_from_the_file_system_not_git() {
    let s = shared();
    let marker = s.sb.root().join("git-ran");
    common::write_executable(
        &s.sb.bin().join("git"),
        &format!("#!/bin/sh\n: > '{}'\nexit 1\n", marker.display()),
    );
    let root = s.sb.root().canonicalize().unwrap();
    let repo = root.join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::create_dir_all(repo.join("src")).unwrap();
    s.sb.remuda()
        .current_dir(repo.join("src"))
        .env("GIT_DIR", root.join("elsewhere/.git"))
        .args(["run", "max"])
        .assert()
        .success();
    assert!(!marker.exists(), "git was run");
    let settings = settings_of(&s.sb.only_invocation().args).unwrap();
    assert_eq!(
        settings["autoMemoryDirectory"],
        json!(memory_of(&s.source, &repo))
    );
}

/// R18: `--setting-sources` (either form), like `--settings`, means no injected settings or
/// auto-memory, said on stderr.
#[test]
fn setting_sources_suppress_injected_settings() {
    for form in [
        &["--setting-sources", "user"][..],
        &["--setting-sources=user,local"],
    ] {
        let s = shared();
        let mut args = vec!["run", "max"];
        args.extend_from_slice(form);
        s.sb.remuda()
            .args(&args)
            .assert()
            .success()
            .stderr(predicate::str::contains(
                "--setting-sources given: settings and auto-memory from claude:default are not \
                 injected",
            ));
        let inv = s.sb.only_invocation();
        assert!(settings_of(&inv.args).is_none(), "{:?}", inv.args);
        assert!(inv.args[0].starts_with("--add-dir="));
    }
}
