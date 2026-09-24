//! R14a: remuda remove <account>.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use common::transcripts::*;
use common::{Sandbox, parse_table};
use predicates::prelude::*;

const S_A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const S_B: &str = "bbbbbbbb-0000-4000-8000-000000000002";

/// An `[[account]]` table.
fn table(provider: &str, name: &str, home: &Path) -> String {
    format!(
        "[[account]]\nprovider = \"{provider}\"\nname = \"{name}\"\nhome = \"{}\"\n\n",
        home.display()
    )
}

/// Every file under `dir` with its contents.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(snapshot(&path));
        } else {
            files.insert(path.clone(), fs::read(&path).unwrap());
        }
    }
    files
}

/// The ACCOUNT column of `remuda list`.
fn listed(sb: &Sandbox) -> Vec<String> {
    let out = sb
        .remuda()
        .arg("list")
        .assert()
        .success()
        .get_output()
        .clone();
    parse_table(&String::from_utf8(out.stdout).unwrap())
        .into_iter()
        .map(|r| r["ACCOUNT"].clone())
        .collect()
}

#[test]
fn remove_unregisters_and_leaves_the_home_untouched() {
    let sb = Sandbox::new();
    sb.install_codex();
    let max = sb.make_claude_home("p/max");
    let team = sb.make_claude_home("p/team");
    let work = sb.make_codex_home("c/work");
    fs::create_dir_all(max.join("projects/-w")).unwrap();
    fs::write(max.join("projects/-w/s.jsonl"), "{}\n").unwrap();
    sb.write_config(&format!(
        "{}{}{}",
        table("claude", "max", &max),
        table("claude", "team", &team),
        table("codex", "work", &work)
    ));
    let before = snapshot(&max);

    let home = max.display().to_string();
    sb.remuda()
        .args(["remove", "max"])
        .assert()
        .success()
        .stdout("")
        .stderr(
            predicate::str::contains("removed claude:max")
                .and(predicate::str::contains(&home))
                .and(predicate::str::contains("left in place"))
                .and(predicate::str::contains(format!("remuda add max {home}"))),
        );

    let config = sb.read_config();
    assert!(!config.contains("\"max\""), "{config}");
    assert!(config.contains("name = \"team\""), "{config}");
    assert!(config.contains("name = \"work\""), "{config}");
    assert_eq!(snapshot(&max), before, "the home changed");
    let accounts = listed(&sb);
    assert!(
        !accounts.contains(&"claude:max".to_string()),
        "{accounts:?}"
    );
    assert!(
        accounts.contains(&"claude:team".to_string()),
        "{accounts:?}"
    );
    assert!(accounts.contains(&"codex:work".to_string()), "{accounts:?}");
}

#[test]
fn remove_codex_account_hint_names_the_provider() {
    let sb = Sandbox::new();
    sb.install_codex();
    let work = sb.make_codex_home("c/work");
    sb.write_config(&table("codex", "work", &work));
    sb.remuda()
        .args(["remove", "codex:work"])
        .assert()
        .success()
        .stdout("")
        .stderr(predicate::str::contains(format!(
            "remuda add --provider codex work {}",
            work.display()
        )));
    // The blank line after the table was the end of the file; it stays.
    assert_eq!(sb.read_config(), "\n");
}

#[test]
fn remove_ambiguous_bare_name_lists_candidates() {
    let sb = Sandbox::new();
    sb.install_codex();
    let claude_x = sb.make_claude_home("p/x");
    let codex_x = sb.make_codex_home("c/x");
    let text = format!(
        "{}{}",
        table("claude", "x", &claude_x),
        table("codex", "x", &codex_x)
    );
    sb.write_config(&text);
    sb.remuda()
        .args(["remove", "x"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("claude:x").and(predicate::str::contains("codex:x")));
    assert_eq!(sb.read_config(), text);

    sb.remuda().args(["remove", "codex:x"]).assert().success();
    let config = sb.read_config();
    assert_eq!(config, table("claude", "x", &claude_x));
}

#[test]
fn remove_refuses_default() {
    let sb = Sandbox::new();
    for reference in ["default", "claude:default", "codex:default"] {
        sb.remuda()
            .args(["remove", reference])
            .assert()
            .code(1)
            .stdout("")
            .stderr(predicate::str::contains("implicit"));
    }
    assert!(!sb.config_path().exists(), "a config file was created");
}

#[test]
fn remove_unknown_account() {
    let sb = Sandbox::new();
    let max = sb.make_claude_home("p/max");
    sb.register(&[("max", &max)]);
    let text = sb.read_config();
    sb.remuda()
        .args(["remove", "nobody"])
        .assert()
        .code(1)
        .stderr(
            predicate::str::contains("unknown account \"nobody\"")
                .and(predicate::str::contains("claude:max")),
        );
    assert_eq!(sb.read_config(), text);
}

#[test]
fn remove_refuses_the_share_source() {
    let sb = Sandbox::new();
    let max = sb.make_claude_home("p/max");
    let solo = sb.make_claude_home("p/solo");
    let text = format!(
        "{}[[account]]\nprovider = \"claude\"\nname = \"solo\"\nhome = \"{}\"\n\
         share = false\n\n[share.claude]\nfrom = \"max\"\n",
        table("claude", "max", &max),
        solo.display()
    );
    sb.write_config(&text);
    sb.remuda()
        .args(["remove", "max"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("[share.claude]"));
    assert_eq!(sb.read_config(), text);

    // A member that opted out can go; the sharing stays.
    sb.remuda().args(["remove", "solo"]).assert().success();
    let config = sb.read_config();
    assert!(!config.contains("solo"), "{config}");
    assert!(
        config.contains("[share.claude]\nfrom = \"max\"\n"),
        "{config}"
    );
    assert!(listed(&sb).contains(&"claude:max".to_string()));
}

#[test]
fn remove_writes_through_a_symlink_and_keeps_permissions() {
    let sb = Sandbox::new();
    let max = sb.make_claude_home("p/max");
    let dotfiles = sb.root().join("dotfiles");
    fs::create_dir(&dotfiles).unwrap();
    let target = dotfiles.join("remuda.toml");
    fs::write(
        &target,
        format!("# tracked in dotfiles\n\n{}", table("claude", "max", &max)),
    )
    .unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    fs::create_dir_all(sb.remuda_home()).unwrap();
    symlink(&target, sb.config_path()).unwrap();

    sb.remuda().args(["remove", "max"]).assert().success();

    let meta = fs::symlink_metadata(sb.config_path()).unwrap();
    assert!(meta.file_type().is_symlink());
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        "# tracked in dotfiles\n\n"
    );
    let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let names: Vec<_> = fs::read_dir(&dotfiles)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, ["remuda.toml"], "temp files left behind");
}

/// `remuda sessions` rows: `(session id, ACCOUNTS)`, keyed by title (the session id).
fn sessions(sb: &Sandbox) -> BTreeMap<String, String> {
    let out = sb
        .remuda()
        .arg("sessions")
        .assert()
        .success()
        .get_output()
        .clone();
    parse_table(&String::from_utf8(out.stdout).unwrap())
        .into_iter()
        .map(|r| (r["TITLE"].clone(), r["ACCOUNTS"].clone()))
        .collect()
}

/// R9, R14a: after a removal, a session in a store still shared with a remaining account stays
/// listed with its launch-log attribution; one in the removed account's own store is gone; the
/// launch log is not touched.
#[test]
fn sessions_after_remove_keep_working() {
    let sb = Sandbox::new();
    let max = sb.make_claude_home("p/max");
    let team = sb.make_claude_home("p/team");
    let solo = sb.make_claude_home("p/solo");
    sb.register(&[("max", &max), ("team", &team), ("solo", &solo)]);
    // max and team share max's store; solo has its own.
    let shared = max.join("projects");
    fs::create_dir_all(shared.join("-w")).unwrap();
    symlink(&shared, team.join("projects")).unwrap();
    fs::write(
        shared.join(format!("-w/{S_A}.jsonl")),
        user(S_A, "/w", &ts(1)),
    )
    .unwrap();
    fs::create_dir_all(solo.join("projects/-w")).unwrap();
    fs::write(
        solo.join(format!("projects/-w/{S_B}.jsonl")),
        user(S_B, "/w", &ts(2)),
    )
    .unwrap();
    for home in [
        None,
        Some(max.as_path()),
        Some(team.as_path()),
        Some(solo.as_path()),
    ] {
        sb.set_agents(home, "[]");
    }
    fs::create_dir_all(sb.launch_log().parent().unwrap()).unwrap();
    let log = format!(
        "{{\"ts\":\"x\",\"account\":\"claude:max\",\"home\":\"{}\",\"cwd\":\"/w\",\"args\":[],\
         \"session_id\":\"{S_A}\",\"injected\":true}}\n",
        max.display()
    );
    fs::write(sb.launch_log(), &log).unwrap();

    let before = sessions(&sb);
    assert_eq!(before.get(S_A).map(String::as_str), Some("claude:max"));
    assert!(before.contains_key(S_B), "{before:?}");

    for name in ["max", "solo"] {
        sb.remuda().args(["remove", name]).assert().success();
    }
    let after = sessions(&sb);
    assert_eq!(after.get(S_A).map(String::as_str), Some("claude:max"));
    assert!(!after.contains_key(S_B), "{after:?}");
    assert_eq!(fs::read_to_string(sb.launch_log()).unwrap(), log);
}
