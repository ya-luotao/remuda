//! R1, R2, R3, R14: `remuda add` and `remuda list`.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use common::Sandbox;
use predicates::prelude::*;

/// `(provider, name, home)` rows of `remuda list`.
fn list_rows(sb: &Sandbox) -> Vec<(String, String, String)> {
    let out = sb
        .remuda()
        .arg("list")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    common::parse_table(&String::from_utf8(out).unwrap())
        .into_iter()
        .map(|r| {
            let (provider, name) = r["ACCOUNT"].split_once(':').unwrap();
            row(provider, name, &r["HOME"])
        })
        .collect()
}

fn row(p: &str, n: &str, h: &str) -> (String, String, String) {
    (p.into(), n.into(), h.into())
}

/// Accounts as stored in config.toml: `(provider, name, home)`.
fn stored(sb: &Sandbox) -> Vec<(String, String, String)> {
    let doc: toml_edit::DocumentMut = sb.read_config().parse().unwrap();
    let Some(arr) = doc.get("account").and_then(|a| a.as_array_of_tables()) else {
        return Vec::new();
    };
    arr.iter()
        .map(|t| {
            let s = |k: &str| t[k].as_str().unwrap().to_string();
            (s("provider"), s("name"), s("home"))
        })
        .collect()
}

#[test]
fn list_without_config_shows_implicit_default() {
    let sb = Sandbox::new();
    assert_eq!(list_rows(&sb), [row("claude", "default", "default")]);
    assert!(
        !sb.remuda_home().exists(),
        "list must not create REMUDA_HOME"
    );
}

#[test]
fn add_then_list_shows_default_first() {
    let sb = Sandbox::new();
    let max = sb.make_claude_home("profiles/max");
    let team = sb.make_claude_home("profiles/team");
    for (name, dir) in [("max", &max), ("team", &team)] {
        sb.remuda()
            .args(["add", name, dir.to_str().unwrap()])
            .assert()
            .success()
            .stderr("");
    }
    assert_eq!(
        list_rows(&sb),
        [
            row("claude", "default", "default"),
            row("claude", "max", max.to_str().unwrap()),
            row("claude", "team", team.to_str().unwrap()),
        ]
    );
}

#[test]
fn add_stores_path_string_verbatim() {
    let sb = Sandbox::new();
    sb.make_claude_home("profiles/max");
    // Trailing slash and a doubled slash must survive untouched: the Keychain key hashes the string.
    let raw = format!("{}//profiles/max/", sb.root().display());
    assert!(fs::metadata(&raw).unwrap().is_dir());
    sb.remuda().args(["add", "max", &raw]).assert().success();
    assert_eq!(stored(&sb), [row("claude", "max", &raw)]);
}

#[test]
fn add_does_not_canonicalize_symlinks() {
    let sb = Sandbox::new();
    let real = sb.make_claude_home("real/max");
    let link = sb.root().join("link-max");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    sb.remuda()
        .args(["add", "max", link.to_str().unwrap()])
        .assert()
        .success();
    assert_eq!(stored(&sb), [row("claude", "max", link.to_str().unwrap())]);
}

#[test]
fn add_expands_tilde_once() {
    let sb = Sandbox::new();
    let dir = sb.home().join("profiles/max");
    fs::create_dir_all(dir.join("projects")).unwrap();
    sb.remuda()
        .args(["add", "max", "~/profiles/max"])
        .assert()
        .success()
        .stderr("");
    assert_eq!(stored(&sb), [row("claude", "max", dir.to_str().unwrap())]);
}

#[test]
fn add_explicit_claude_provider_is_accepted() {
    let sb = Sandbox::new();
    let max = sb.make_claude_home("max");
    sb.remuda()
        .args(["add", "--provider", "claude", "max", max.to_str().unwrap()])
        .assert()
        .success();
    assert_eq!(stored(&sb), [row("claude", "max", max.to_str().unwrap())]);
}

/// Asserts a user error: exit 1, one `remuda: ...` line on stderr containing `needle`,
/// and nothing written to the registry.
fn assert_add_fails(sb: &Sandbox, args: &[&str], needle: &str) {
    let before = fs::read_to_string(sb.config_path()).ok();
    let out = sb
        .remuda()
        .arg("add")
        .args(args)
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.starts_with("remuda: "), "stderr: {stderr:?}");
    assert_eq!(stderr.lines().count(), 1, "stderr: {stderr:?}");
    assert!(stderr.contains(needle), "expected {needle:?} in {stderr:?}");
    assert_eq!(
        fs::read_to_string(sb.config_path()).ok(),
        before,
        "config changed"
    );
}

#[test]
fn add_rejects_bad_names() {
    let sb = Sandbox::new();
    let dir = sb.make_claude_home("h");
    let d = dir.to_str().unwrap();
    assert_add_fails(&sb, &["default", d], "reserved");
    for bad in ["a.b", "a b", "a:b", "ma/x"] {
        assert_add_fails(&sb, &[bad, d], "name");
    }
    assert_add_fails(&sb, &["--", "-x", d], "cannot start with `-`");
}

#[test]
fn add_rejects_unknown_provider() {
    let sb = Sandbox::new();
    let dir = sb.make_claude_home("h");
    assert_add_fails(
        &sb,
        &["--provider", "gemini", "x", dir.to_str().unwrap()],
        "unknown provider \"gemini\"",
    );
}

#[test]
fn add_rejects_bad_paths() {
    let sb = Sandbox::new();
    sb.make_claude_home("work/rel");
    assert_add_fails(&sb, &["x", "rel"], "absolute");
    assert_add_fails(&sb, &["x", "~nobody/x"], "absolute");
    let missing = sb.root().join("missing");
    assert_add_fails(&sb, &["x", missing.to_str().unwrap()], "does not exist");
    let file = sb.root().join("file");
    fs::write(&file, "").unwrap();
    assert_add_fails(&sb, &["x", file.to_str().unwrap()], "not a directory");
    assert!(!sb.config_path().exists());
}

#[test]
fn add_rejects_non_nfc_path() {
    let sb = Sandbox::new();
    let nfc = sb.root().join("caf\u{e9}");
    fs::create_dir(&nfc).unwrap();
    let nfd = format!("{}/cafe\u{301}", sb.root().display());
    assert_add_fails(&sb, &["x", &nfd], "NFC");
}

#[test]
fn add_rejects_duplicate_name() {
    let sb = Sandbox::new();
    let a = sb.make_claude_home("a");
    let b = sb.make_claude_home("b");
    sb.remuda()
        .args(["add", "max", a.to_str().unwrap()])
        .assert()
        .success();
    assert_add_fails(&sb, &["max", b.to_str().unwrap()], "max");
}

#[test]
fn add_rejects_same_path_string_under_another_name() {
    let sb = Sandbox::new();
    let a = sb.make_claude_home("a");
    sb.remuda()
        .args(["add", "max", a.to_str().unwrap()])
        .assert()
        .success();
    assert_add_fails(&sb, &["other", a.to_str().unwrap()], "max");
}

#[test]
fn add_rejects_other_spelling_of_registered_dir() {
    let sb = Sandbox::new();
    let a = sb.make_claude_home("a");
    let link = sb.root().join("link-a");
    std::os::unix::fs::symlink(&a, &link).unwrap();
    sb.remuda()
        .args(["add", "max", a.to_str().unwrap()])
        .assert()
        .success();
    let slash = format!("{}/", a.display());
    assert_add_fails(&sb, &["other", &slash], "claude:max");
    assert_add_fails(&sb, &["other", link.to_str().unwrap()], "claude:max");
}

#[test]
fn add_warns_when_dir_does_not_look_like_claude_home() {
    let sb = Sandbox::new();
    let empty = sb.root().join("empty");
    fs::create_dir(&empty).unwrap();
    sb.remuda()
        .args(["add", "x", empty.to_str().unwrap()])
        .assert()
        .success()
        .stderr(
            predicate::str::starts_with("remuda: warning: ")
                .and(predicate::str::contains(".claude.json")),
        );
    assert_eq!(stored(&sb), [row("claude", "x", empty.to_str().unwrap())]);
}

#[test]
fn add_does_not_warn_when_only_projects_exists() {
    let sb = Sandbox::new();
    let dir = sb.root().join("p");
    fs::create_dir_all(dir.join("projects")).unwrap();
    sb.remuda()
        .args(["add", "x", dir.to_str().unwrap()])
        .assert()
        .success()
        .stderr("");
}

#[test]
fn add_leaves_home_directory_untouched() {
    let sb = Sandbox::new();
    let dir = sb.make_claude_home("h");
    let listing = |d: &std::path::Path| {
        let mut v: Vec<_> = fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        v.sort();
        v
    };
    let before = listing(&dir);
    sb.remuda()
        .args(["add", "x", dir.to_str().unwrap()])
        .assert()
        .success();
    assert_eq!(listing(&dir), before);
    assert_eq!(
        fs::read_to_string(dir.join(".claude.json")).unwrap(),
        "{}\n"
    );
}

#[test]
fn add_preserves_comments_and_unknown_keys() {
    let sb = Sandbox::new();
    let a = sb.make_claude_home("a");
    let b = sb.make_claude_home("b");
    let original = format!(
        "# remuda accounts, hand-edited\nfuture_setting = \"keep me\"\n\n\
         [[account]]\n# the main one\nprovider = \"claude\"\nname = \"max\"\nhome = \"{}\"\nnote = 42 # trailing\n",
        a.display()
    );
    sb.write_config(&original);
    sb.remuda()
        .args(["add", "team", b.to_str().unwrap()])
        .assert()
        .success();
    let after = sb.read_config();
    assert!(
        after.starts_with(&original),
        "existing content changed:\n{after}"
    );
    assert_eq!(
        stored(&sb),
        [
            row("claude", "max", a.to_str().unwrap()),
            row("claude", "team", b.to_str().unwrap()),
        ]
    );
}

#[test]
fn add_writes_atomically_and_keeps_permissions() {
    let sb = Sandbox::new();
    let a = sb.make_claude_home("a");
    sb.write_config("# empty\n");
    fs::set_permissions(sb.config_path(), fs::Permissions::from_mode(0o600)).unwrap();
    sb.remuda()
        .args(["add", "max", a.to_str().unwrap()])
        .assert()
        .success();
    let mode = fs::metadata(sb.config_path()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let names: Vec<_> = fs::read_dir(sb.remuda_home())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, ["config.toml"], "temp files left behind");
}

#[test]
fn add_writes_through_symlinked_config() {
    let sb = Sandbox::new();
    let a = sb.make_claude_home("a");
    let dotfiles = sb.root().join("dotfiles");
    fs::create_dir(&dotfiles).unwrap();
    let target = dotfiles.join("remuda.toml");
    fs::write(&target, "# tracked in dotfiles\n").unwrap();
    fs::create_dir_all(sb.remuda_home()).unwrap();
    std::os::unix::fs::symlink(&target, sb.config_path()).unwrap();

    sb.remuda()
        .args(["add", "max", a.to_str().unwrap()])
        .assert()
        .success();

    assert!(
        fs::symlink_metadata(sb.config_path())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        fs::read_to_string(&target)
            .unwrap()
            .starts_with("# tracked in dotfiles\n")
    );
    assert_eq!(stored(&sb), [row("claude", "max", a.to_str().unwrap())]);
}

#[test]
fn invalid_config_is_a_user_error() {
    let sb = Sandbox::new();
    sb.write_config("[[account]\nbroken");
    sb.remuda().arg("list").assert().code(1).stderr(
        predicate::str::starts_with("remuda: ").and(predicate::str::contains("config.toml")),
    );
}

#[test]
fn list_and_add_ignore_claude_config_dir_in_env() {
    // remuda's own env may carry CLAUDE_CONFIG_DIR (e.g. run inside a claude session);
    // the registry does not depend on it.
    let sb = Sandbox::new();
    sb.remuda()
        .env("CLAUDE_CONFIG_DIR", sb.root().join("elsewhere"))
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("default"));
}

#[test]
fn relative_home_in_config_is_a_user_error() {
    let sb = Sandbox::new();
    sb.write_config(
        "[[account]]\nprovider = \"claude\"\nname = \"max\"\nhome = \"profiles/max\"\n",
    );
    for args in [&["list"][..], &["run", "max"][..]] {
        sb.remuda().args(args).assert().code(1).stderr(
            predicate::str::starts_with("remuda: ")
                .and(predicate::str::contains("config.toml"))
                .and(predicate::str::contains("absolute")),
        );
    }
    assert!(sb.invocations().is_empty());
}

#[test]
fn add_refuses_the_native_home() {
    let sb = Sandbox::new();
    let native = sb.home().join(".claude");
    fs::create_dir_all(native.join("projects")).unwrap();
    let link = sb.root().join("link-native");
    std::os::unix::fs::symlink(&native, &link).unwrap();
    let spellings = [
        native.to_str().unwrap().to_string(),
        format!("{}/", native.display()),
        "~/.claude".to_string(),
        "~/.claude/".to_string(),
        link.to_str().unwrap().to_string(),
    ];
    for path in &spellings {
        assert_add_fails(&sb, &["x", path], "default");
    }
    assert!(!sb.config_path().exists());
}

#[test]
fn add_allows_other_dirs_under_home() {
    let sb = Sandbox::new();
    fs::create_dir_all(sb.home().join(".claude")).unwrap();
    let other = sb.home().join(".claude-max");
    fs::create_dir_all(other.join("projects")).unwrap();
    sb.remuda()
        .args(["add", "max", "~/.claude-max"])
        .assert()
        .success();
}
