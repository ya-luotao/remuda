//! R5, R13, R18: `remuda setup <name> [--email <addr>]`, and the links of a member's new home.

mod common;

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use common::Sandbox;
use predicates::prelude::*;
use serde_json::Value;

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

/// Names in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// A source home at `dir` with everything a session loads, so that a home linked to all of it
/// gets nothing injected (R18): the session store, settings, instructions and plugins, and
/// what is never linked. `commands`, `rules`, `agent-memory` and `output-styles` are missing,
/// and `agents` is a dangling link.
fn source_home(dir: &Path) {
    for sub in [
        "projects",
        "file-history",
        "skills/review",
        "hooks",
        "plugins",
        "sessions",
        "todos",
    ] {
        fs::create_dir_all(dir.join(sub)).unwrap();
    }
    for (file, content) in [
        (
            "settings.json",
            r#"{"model": "opus", "cleanupPeriodDays": 365}"#,
        ),
        ("CLAUDE.md", "be brief\n"),
        ("keybindings.json", "{}"),
        (".claude.json", "{}"),
        ("history.jsonl", ""),
        ("remote-settings.json", "{}"),
        ("policy-limits.json", "{}"),
    ] {
        fs::write(dir.join(file), content).unwrap();
    }
    symlink(dir.join("nowhere"), dir.join("agents")).unwrap();
}

/// What a home linked to [`source_home`] holds, sorted.
const LINKED: [&str; 8] = [
    "CLAUDE.md",
    "file-history",
    "hooks",
    "keybindings.json",
    "plugins",
    "projects",
    "settings.json",
    "skills",
];

/// Asserts that `home` holds exactly [`LINKED`], each a symlink whose target is, byte for
/// byte, `<source>/<item>` with `source` as given (R18: not canonicalized).
fn assert_linked(home: &Path, source: &str) {
    assert_links(home, source, &LINKED);
}

/// [`assert_linked`] for the sorted `items`.
fn assert_links(home: &Path, source: &str, items: &[&str]) {
    assert_eq!(names(home), items);
    for item in items {
        let link = home.join(item);
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "{item}"
        );
        assert_eq!(
            fs::read_link(&link).unwrap().as_os_str().as_bytes(),
            format!("{source}/{item}").as_bytes(),
            "{item}"
        );
    }
}

/// R13: without `[share.claude]` the new home is an empty directory.
#[test]
fn setup_without_shared_configuration_creates_an_empty_home() {
    let sb = Sandbox::new();
    // A native home full of things to link changes nothing.
    source_home(&sb.home().join(".claude"));
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
    // The login found an empty, registered home.
    let [login] = sb.logins().try_into().unwrap();
    assert_eq!((login.registered, login.entries.len()), (true, 0));
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

/// R12, R13, R18: with `[share.claude]`, the new claude home gets one link for each listed item
/// the source has, made before the login; what is never linked, what the source lacks, and a
/// dangling item are left out, and the latter two are reported. Sessions of the account then
/// get nothing injected: everything resolves to the source's.
#[test]
fn setup_links_a_members_home_to_the_source() {
    let sb = Sandbox::new();
    let source = sb.home().join(".claude");
    source_home(&source);
    sb.write_config("[share.claude]\nfrom = \"default\"\n");

    let home = new_home(&sb, "work");
    let home_str = home.to_str().unwrap().to_string();
    let source_str = source.to_str().unwrap().to_string();
    sb.remuda()
        .args(["setup", "work"])
        .assert()
        .success()
        .stderr(
            predicate::str::contains(format!(
                "remuda: linked 8 items of {home_str} to claude:default ({source_str}): \
                 projects, file-history, settings.json, CLAUDE.md, skills, hooks, plugins, \
                 keybindings.json\n"
            ))
            .and(predicate::str::contains(
                "remuda: not linked (claude:default has none): commands, agents, rules, \
                 agent-memory, output-styles\n",
            )),
        );

    let meta = fs::symlink_metadata(&home).unwrap();
    assert!(meta.is_dir());
    assert_eq!(meta.permissions().mode() & 0o777, 0o700);
    // `.claude.json`, `history.jsonl`, `sessions` and the organization's files stay per
    // account, and so does what is not on the list (`todos`).
    assert_linked(&home, &source_str);
    assert_eq!(stored(&sb), [("work".to_string(), home_str.clone())]);
    assert!(sb.read_config().starts_with("[share.claude]\n"));
    // Nothing in the source changed.
    assert!(
        fs::symlink_metadata(source.join("projects"))
            .unwrap()
            .is_dir()
    );
    assert_eq!(names(&source.join("projects")), [] as [&str; 0]);

    let inv = sb.only_invocation();
    assert_eq!(inv.args, ["auth", "login"]);
    assert_eq!(inv.config_dir.as_deref(), Some(home_str.as_str()));
    // The home was linked and registered before the login ran: what the login found.
    let [login] = sb.logins().try_into().unwrap();
    assert_eq!(login.config_dir, home_str);
    assert!(login.registered, "registered before the login");
    let expected: Vec<(String, Option<String>)> = LINKED
        .iter()
        .map(|item| (item.to_string(), Some(format!("{source_str}/{item}"))))
        .collect();
    assert_eq!(login.entries, expected);

    // R18: a session of the new account gets nothing injected but the memory variable, which
    // lets claude write its auto-memory by the source's path instead of through the link.
    sb.remuda()
        .args(["run", "work", "-p", "hi"])
        .assert()
        .success();
    let [_, inv] = sb.invocations().try_into().unwrap();
    assert_eq!(inv.args[..3], ["-p", "hi", "--session-id"]);
    assert_eq!(inv.args.len(), 4, "{:?}", inv.args);
    assert_eq!(inv.config_dir.as_deref(), Some(home_str.as_str()));
    assert_eq!(inv.add_dir_claude_md, None);
    assert_eq!(inv.memory_dir.as_deref(), Some(source_str.as_str()));
    let [launch] = sb.launches().try_into().unwrap();
    assert!(launch.get("shared").is_none(), "{launch}");
    assert!(!sb.remuda_home().join("shared").exists());
    assert_linked(&home, &source_str);
}

/// R18: a source whose `settings.json` sets authentication keeps it to itself: `setup` does
/// not link it, says which settings are the reason (never their values), and links the rest.
/// The account's sessions then get the source's settings injected without the authentication.
#[test]
fn setup_does_not_link_settings_that_set_authentication() {
    let sb = Sandbox::new();
    let source = sb.home().join(".claude");
    source_home(&source);
    fs::write(
        source.join("settings.json"),
        r#"{"model": "opus", "cleanupPeriodDays": 365, "apiKeyHelper": "/bin/zz-helper",
            "forceLoginOrgUUID": "zz-org",
            "env": {"ANTHROPIC_API_KEY": "sk-zz-secret", "EDITOR": "vi"}}"#,
    )
    .unwrap();
    sb.write_config("[share.claude]\nfrom = \"default\"\n");

    let home = new_home(&sb, "work");
    let source_str = source.to_str().unwrap().to_string();
    let assert = sb.remuda().args(["setup", "work"]).assert().success();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains(
            "remuda: settings.json not linked: claude:default sets authentication settings \
             (apiKeyHelper, forceLoginOrgUUID, env.ANTHROPIC_API_KEY); they stay per account, \
             and the rest is injected at launch\n"
        ),
        "{stderr}"
    );
    assert!(stderr.contains("remuda: linked 7 items of "), "{stderr}");
    // The shared store is then cleaned up by this home's own settings, which it lacks (R11).
    assert!(
        stderr.contains(
            "remuda: warning: this account shares projects without a settings.json of its own"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("zz-"), "no value is shown: {stderr}");
    let linked: Vec<&str> = LINKED
        .into_iter()
        .filter(|item| *item != "settings.json")
        .collect();
    assert_links(&home, &source_str, &linked);
    // The login found the same.
    let [login] = sb.logins().try_into().unwrap();
    assert!(login.registered);
    assert_eq!(login.entries.len(), 7);
    assert!(
        login
            .entries
            .iter()
            .all(|(name, _)| name != "settings.json")
    );

    sb.remuda()
        .args(["run", "work", "-p", "hi"])
        .assert()
        .success();
    let [_, inv] = sb.invocations().try_into().unwrap();
    let path = inv.args[0].strip_prefix("--settings=").expect("--settings");
    assert_eq!(inv.args[1..4], ["-p", "hi", "--session-id"]);
    assert_eq!(inv.args.len(), 5, "{:?}", inv.args);
    let text = fs::read_to_string(path).unwrap();
    assert!(!text.contains("zz-"), "{text}");
    let injected: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        injected,
        serde_json::json!({"model": "opus", "cleanupPeriodDays": 365, "env": {"EDITOR": "vi"}})
    );
    assert_links(&home, &source_str, &linked);
}

/// R11, R18: a linked `settings.json` is shared whole. Authentication added to the source's
/// afterwards is read by the account through the link; each of its sessions says which, by
/// name, and starts as before (no `--settings`: the file is the source's). Without
/// authentication there is nothing to say.
#[test]
fn a_session_names_authentication_read_through_a_linked_settings_file() {
    let sb = Sandbox::new();
    let source = sb.home().join(".claude");
    source_home(&source);
    sb.write_config("[share.claude]\nfrom = \"default\"\n");
    sb.remuda().args(["setup", "work"]).assert().success();
    let home = new_home(&sb, "work");
    assert_linked(&home, source.to_str().unwrap());
    let run = || {
        let out = sb
            .remuda()
            .args(["run", "work", "-p", "hi"])
            .assert()
            .success();
        String::from_utf8(out.get_output().stderr.clone()).unwrap()
    };
    let stderr = run();
    assert!(!stderr.contains("authentication"), "{stderr}");

    fs::write(
        source.join("settings.json"),
        r#"{"model": "opus", "cleanupPeriodDays": 365,
            "env": {"ANTHROPIC_API_KEY": "sk-zz-secret", "EDITOR": "vi"}}"#,
    )
    .unwrap();
    let stderr = run();
    assert!(
        stderr.contains(
            "remuda: warning: claude:work reads the authentication settings of claude:default \
             through its settings.json link: env.ANTHROPIC_API_KEY\n"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("zz-"), "no value is shown: {stderr}");
    let invocations = sb.invocations();
    let inv = invocations.last().unwrap();
    assert_eq!(inv.args[..3], ["-p", "hi", "--session-id"]);
    assert_eq!(inv.args.len(), 4, "{:?}", inv.args);
    assert!(
        fs::symlink_metadata(home.join("settings.json"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "remuda never changes the link"
    );
}

/// R18: a source `settings.json` that is not a JSON object cannot be checked, and is not
/// linked either.
#[test]
fn setup_does_not_link_settings_it_cannot_read() {
    let sb = Sandbox::new();
    let source = sb.home().join(".claude");
    source_home(&source);
    fs::write(source.join("settings.json"), "[]").unwrap();
    sb.write_config("[share.claude]\nfrom = \"default\"\n");
    sb.remuda()
        .args(["setup", "work"])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "remuda: warning: settings.json not linked: that of claude:default cannot be read \
             as a JSON object, so it cannot be checked for authentication settings\n",
        ));
    assert!(!names(&new_home(&sb, "work")).contains(&"settings.json".to_string()));
    assert_eq!(stored(&sb).len(), 1);
}

/// R13: a `homes` or `homes/<provider>` that is a symlink would take the new home, and its
/// links, out of `$REMUDA_HOME`. `setup` refuses before anything is created: the directory
/// the link points at stays as it is, nothing is registered, and no login runs.
#[test]
fn setup_refuses_a_symlinked_homes_directory() {
    for level in ["homes", "homes/claude"] {
        let sb = Sandbox::new();
        source_home(&sb.home().join(".claude"));
        sb.write_config("[share.claude]\nfrom = \"default\"\n");
        let outside = sb.root().join("outside");
        fs::create_dir(&outside).unwrap();
        let link = sb.remuda_home().join(level);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&outside, &link).unwrap();

        assert_setup_fails(
            &sb,
            &["work"],
            &format!("{} is a symbolic link", link.display()),
        );
        assert_eq!(names(&outside), [] as [&str; 0], "{level}");
        assert_eq!(fs::read_link(&link).unwrap(), outside, "{level}");
        assert!(stored(&sb).is_empty(), "{level}");
        assert!(sb.logins().is_empty(), "{level}");
        // Codex homes are made the same way.
        if level == "homes" {
            sb.install_codex();
            sb.remuda()
                .args(["setup", "--provider", "codex", "work"])
                .assert()
                .code(1)
                .stderr(predicate::str::contains("is a symbolic link"));
            assert_eq!(names(&outside), [] as [&str; 0]);
            assert!(sb.codex_invocations().is_empty());
        }
    }
}

/// R18: the targets are written from the source's home as registered, not canonicalized: a
/// source registered through a symlink, under a name ending in a space, is linked to by that
/// string.
#[test]
fn setup_links_to_the_source_home_as_registered() {
    let sb = Sandbox::new();
    let real = sb.root().join("real source");
    source_home(&real);
    let registered = sb.root().join("src home ");
    symlink(&real, &registered).unwrap();
    let registered_str = registered.to_str().unwrap().to_string();
    sb.write_config(&format!(
        "[[account]]\nprovider = \"claude\"\nname = \"src\"\nhome = \"{registered_str}\"\n\n\
         [share.claude]\nfrom = \"src\"\n"
    ));
    sb.remuda()
        .args(["setup", "work"])
        .assert()
        .success()
        .stderr(predicate::str::contains(format!(
            "to claude:src ({registered_str}): projects, "
        )));
    let home = new_home(&sb, "work");
    assert_linked(&home, &registered_str);
    assert_eq!(
        fs::canonicalize(home.join("projects")).unwrap(),
        fs::canonicalize(real.join("projects")).unwrap()
    );
    assert_eq!(sb.only_invocation().args, ["auth", "login"]);
}

/// R18: a source whose home is missing leaves the new home empty; `setup` says so and goes on.
#[test]
fn setup_links_nothing_when_the_source_home_is_missing() {
    let sb = Sandbox::new();
    sb.write_config("[share.claude]\nfrom = \"default\"\n");
    sb.remuda()
        .args(["setup", "work"])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "remuda: warning: the home of claude:default does not exist: nothing linked\n",
        ));
    let home = new_home(&sb, "work");
    assert_eq!(names(&home), [] as [&str; 0]);
    assert_eq!(stored(&sb).len(), 1);
    assert_eq!(sb.only_invocation().args, ["auth", "login"]);
}

/// R17, R18: a codex home is created empty, whatever `[share.claude]` says.
#[test]
fn codex_setup_links_nothing() {
    let sb = Sandbox::new();
    sb.install_codex();
    source_home(&sb.home().join(".claude"));
    sb.write_config("[share.claude]\nfrom = \"default\"\n");
    sb.remuda()
        .args(["setup", "--provider", "codex", "work"])
        .assert()
        .success()
        .stderr(predicate::str::contains("linked").not());
    let home = sb.remuda_home().join("homes/codex/work");
    assert_eq!(names(&home), [] as [&str; 0]);
    let [inv] = sb.codex_invocations().try_into().unwrap();
    assert_eq!(inv.args, ["login"]);
    assert!(sb.invocations().is_empty());
}

/// R18: a failed login keeps the links as it keeps the registration.
#[test]
fn setup_login_failure_keeps_the_links() {
    let sb = Sandbox::new();
    let source = sb.home().join(".claude");
    source_home(&source);
    sb.write_config("[share.claude]\nfrom = \"default\"\n");
    sb.remuda()
        .env("FAKE_CLAUDE_EXIT", "3")
        .args(["setup", "work"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("remuda run work auth login"));
    assert_linked(&new_home(&sb, "work"), source.to_str().unwrap());
    assert_eq!(stored(&sb).len(), 1);
}

/// R3, R14, R23: `setup` refuses, before creating anything, an account that would leave the
/// registry invalid (a bare name in `[pick]` becoming ambiguous).
#[test]
fn setup_refuses_a_name_that_would_leave_the_config_invalid() {
    let sb = Sandbox::new();
    let personal = sb.make_claude_home("h/personal");
    sb.write_config(&format!(
        "[[account]]\nprovider = \"claude\"\nname = \"personal\"\nhome = \"{}\"\n\n\
         [pick]\nexclude = [\"personal\"]\n",
        personal.display()
    ));
    let before = sb.read_config();
    sb.remuda()
        .args(["setup", "--provider", "codex", "personal"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "adding codex:personal would leave",
        ));
    assert_eq!(sb.read_config(), before);
    assert!(!sb.remuda_home().join("homes/codex/personal").exists());
}
