//! R5, R8, R9: `remuda sessions [--limit N]`.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use common::transcripts::*;
use common::{Sandbox, parse_table};

const S_A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const S_B: &str = "bbbbbbbb-0000-4000-8000-000000000002";
const S_C: &str = "cccccccc-0000-4000-8000-000000000003";
const S_D: &str = "dddddddd-0000-4000-8000-000000000004";

struct Setup {
    sb: Sandbox,
    max: PathBuf,
    team: PathBuf,
    native_projects: PathBuf,
}

/// default + max share `$HOME/.claude/projects` (max via a symlink); team has its own store.
fn setup() -> Setup {
    let sb = Sandbox::new();
    let max = sb.make_claude_home("p/max");
    let team = sb.make_claude_home("p/team");
    sb.register(&[("max", &max), ("team", &team)]);
    let native_projects = sb.home().join(".claude/projects");
    fs::create_dir_all(&native_projects).unwrap();
    symlink(&native_projects, max.join("projects")).unwrap();
    fs::create_dir_all(team.join("projects")).unwrap();
    Setup {
        sb,
        max,
        team,
        native_projects,
    }
}

fn transcript(projects: &Path, sid: &str, text: &str) {
    let dir = projects.join("-w-proj");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("{sid}.jsonl")), text).unwrap();
}

fn history(path: &Path, sids: &[&str]) {
    let text: String = sids
        .iter()
        .map(|s| {
            format!(
                "{{\"display\":\"p\",\"pastedContents\":{{}},\"timestamp\":1,\"project\":\"/w\",\"sessionId\":\"{s}\"}}\n"
            )
        })
        .collect();
    fs::write(path, text).unwrap();
}

fn sessions(sb: &Sandbox, extra: &[&str]) -> (Vec<BTreeMap<String, String>>, String) {
    let out = sb
        .remuda()
        .env("TZ", "Asia/Shanghai")
        .arg("sessions")
        .args(extra)
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let header: Vec<&str> = stdout.lines().next().unwrap().split_whitespace().collect();
    assert_eq!(header, ["TIME", "ACCOUNTS", "TITLE", "CWD"]);
    (parse_table(&stdout), String::from_utf8(out.stderr).unwrap())
}

fn row(r: &BTreeMap<String, String>) -> [&str; 4] {
    [&r["TIME"], &r["ACCOUNTS"], &r["TITLE"], &r["CWD"]].map(String::as_str)
}

#[test]
fn lists_sessions_newest_first_with_accounts_title_and_cwd() {
    let Setup {
        sb,
        max,
        team,
        native_projects,
    } = setup();
    // Resumed in another directory: shown with its last cwd.
    transcript(
        &native_projects,
        S_A,
        &[
            user("start here", "/w/one", &ts(1)),
            user("continue there", "/w/two", &ts(5)),
            ai_title("Moved session"),
        ]
        .concat(),
    );
    // No ai-title: the first user text stands in.
    transcript(
        &native_projects,
        S_B,
        &[
            meta_user(
                "<local-command-caveat>c</local-command-caveat>",
                "/w/b",
                &ts(9),
            ),
            user("  explain\n the   index ", "/w/b", &ts(10)),
        ]
        .concat(),
    );
    transcript(
        &team.join("projects"),
        S_C,
        &user("team work", "/w/c", &ts(3)),
    );
    // Nobody claims this one.
    transcript(&native_projects, S_D, &user("orphan", "/w/d", &ts(2)));

    // Launch log: S_A was started through remuda as max.
    fs::create_dir_all(sb.launch_log().parent().unwrap()).unwrap();
    fs::write(
        sb.launch_log(),
        format!("{{\"ts\":\"x\",\"account\":\"claude:max\",\"home\":\"/p\",\"cwd\":\"/w\",\"args\":[],\"session_id\":\"{S_A}\",\"injected\":true}}\n"),
    )
    .unwrap();
    // history.jsonl: team's own history knows S_C, and max resumed S_A at some point.
    history(&team.join("history.jsonl"), &[S_C]);
    history(&max.join("history.jsonl"), &[S_A]);
    // Live: S_B is running under the native login right now, S_A under team.
    sb.set_agents(
        None,
        &format!(r#"[{{"pid": 7, "sessionId": "{S_B}", "status": "busy"}}]"#),
    );
    sb.set_agents(
        Some(&team),
        &format!(r#"[{{"pid": 8, "sessionId": "{S_A}", "status": "idle"}}]"#),
    );
    sb.set_agents(Some(&max), "[]");

    let (rows, stderr) = sessions(&sb, &[]);
    assert_eq!(stderr, "");
    let rows: Vec<[&str; 4]> = rows.iter().map(row).collect();
    assert_eq!(
        rows,
        [
            [
                "2026-09-20 18:10",
                "claude:default",
                "explain the index",
                "/w/b"
            ],
            [
                "2026-09-20 18:05",
                "claude:max,claude:team",
                "Moved session",
                "/w/two"
            ],
            ["2026-09-20 18:03", "claude:team", "team work", "/w/c"],
            ["2026-09-20 18:02", "-", "orphan", "/w/d"],
        ]
    );

    // `claude agents --json` ran once per account, each under its own environment (R2, R7).
    let agents: Vec<Option<String>> = sb
        .invocations()
        .into_iter()
        .filter(|i| i.args == ["agents", "--json", "--all"])
        .map(|i| i.config_dir)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut want = vec![
        None,
        Some(max.display().to_string()),
        Some(team.display().to_string()),
    ];
    want.sort();
    assert_eq!(agents, want);
    assert_eq!(sb.invocations().len(), 3);
}

/// R16: the same session ID in two stores is two rows, each with the session's accounts.
#[test]
fn a_session_id_in_two_stores_is_listed_twice() {
    let Setup {
        sb,
        max,
        team,
        native_projects,
    } = setup();
    let text = user("same session", "/w/a", &ts(1));
    transcript(&native_projects, S_A, &text);
    transcript(&team.join("projects"), S_A, &text);
    history(&max.join("history.jsonl"), &[S_A]);
    let (rows, _) = sessions(&sb, &[]);
    assert_eq!(rows.len(), 2, "{rows:?}");
    for r in &rows {
        assert_eq!(r["ACCOUNTS"], "claude:max");
        assert_eq!(r["TITLE"], "same session");
    }
}

#[test]
fn limit_keeps_the_newest() {
    let Setup {
        sb,
        native_projects,
        ..
    } = setup();
    for (i, sid) in [S_A, S_B, S_C].iter().enumerate() {
        transcript(&native_projects, sid, &user(sid, "/w", &ts(i as u32)));
    }
    let (rows, _) = sessions(&sb, &["--limit", "2"]);
    let titles: Vec<&str> = rows.iter().map(|r| r["TITLE"].as_str()).collect();
    assert_eq!(titles, [S_C, S_B]);
}

#[test]
fn long_titles_are_shortened_and_missing_fields_are_dashes() {
    let Setup {
        sb,
        native_projects,
        ..
    } = setup();
    transcript(
        &native_projects,
        S_A,
        &user(&"word ".repeat(40), "/w", &ts(1)),
    );
    transcript(&native_projects, S_B, &ai_title("Only a title"));
    let (rows, _) = sessions(&sb, &[]);
    assert_eq!(rows.len(), 2);
    let a = rows
        .iter()
        .find(|r| r["TITLE"].starts_with("word"))
        .unwrap();
    assert_eq!(a["TITLE"].chars().count(), 60);
    assert!(a["TITLE"].ends_with('…'));
    let b = rows.iter().find(|r| r["TITLE"] == "Only a title").unwrap();
    assert_eq!(b["CWD"], "-");
    assert_eq!(b["ACCOUNTS"], "-");
    assert_ne!(b["TIME"], "", "falls back to the file's mtime");
}

#[test]
fn empty_when_there_are_no_transcripts() {
    let sb = Sandbox::new();
    let out = sb
        .remuda()
        .arg("sessions")
        .assert()
        .success()
        .stderr("")
        .get_output()
        .clone();
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "TIME  ACCOUNTS  TITLE  CWD\n"
    );
}

/// R3: the index cache holds titles and directories: it is written readable by the user alone,
/// in a `state/` that is, also over a cache and a directory from before that others could read.
#[test]
fn the_index_cache_is_private() {
    let mode = |path: &Path| fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777;
    let Setup {
        sb,
        native_projects,
        ..
    } = setup();
    transcript(&native_projects, S_A, &user("cached", "/w", &ts(1)));
    let state = sb.remuda_home().join("state");
    let cache = state.join("index.json");
    sessions(&sb, &[]);
    assert_eq!(mode(&state), 0o700);
    assert_eq!(mode(&cache), 0o600);

    fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o644)).unwrap();
    let (rows, _) = sessions(&sb, &[]);
    assert_eq!(rows[0]["TITLE"], "cached");
    assert_eq!(mode(&state), 0o700);
    assert_eq!(mode(&cache), 0o600);
}

/// R8 (review #10): a store that exists but cannot be read is not an empty one. Its sessions
/// stay listed as last indexed and in the cache, with a warning naming the store; the stores
/// that can be read are indexed as usual.
#[test]
fn a_store_that_cannot_be_read_is_warned_about_and_its_sessions_stay_listed() {
    let Setup {
        sb,
        team,
        native_projects,
        ..
    } = setup();
    let titles = |rows: &[BTreeMap<String, String>]| -> Vec<String> {
        rows.iter().map(|r| r["TITLE"].clone()).collect()
    };
    transcript(&native_projects, S_A, &user("native one", "/w", &ts(3)));
    transcript(&native_projects, S_B, &user("native two", "/w", &ts(2)));
    transcript(&team.join("projects"), S_C, &user("team one", "/w", &ts(1)));
    let (rows, stderr) = sessions(&sb, &[]);
    assert_eq!(titles(&rows), ["native one", "native two", "team one"]);
    assert_eq!(stderr, "");

    let store = native_projects.canonicalize().unwrap();
    fs::set_permissions(&store, fs::Permissions::from_mode(0o000)).unwrap();
    transcript(&team.join("projects"), S_D, &user("team two", "/w", &ts(4)));
    let (rows, stderr) = sessions(&sb, &[]);
    fs::set_permissions(&store, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        titles(&rows),
        ["team two", "native one", "native two", "team one"]
    );
    let warning = format!("remuda: warning: cannot read {}: ", store.display());
    assert!(stderr.starts_with(&warning), "{stderr}");
    assert!(
        stderr.ends_with("; 2 sessions below it are listed as last indexed\n"),
        "{stderr}"
    );
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    let cache = sb.remuda_home().join("state/index.json");
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&cache).unwrap()).unwrap();
    assert_eq!(v["entries"].as_object().unwrap().len(), 4);

    // Readable again: nothing to warn about.
    let (rows, stderr) = sessions(&sb, &[]);
    assert_eq!(rows.len(), 4);
    assert_eq!(stderr, "");
}

/// R8 (review #10): a project directory that can be listed but not searched is not an empty
/// one either. Its sessions stay listed and in the cache that is saved, with one warning.
#[test]
fn a_project_that_cannot_be_searched_is_warned_about_and_its_sessions_stay_listed() {
    let Setup {
        sb,
        native_projects,
        ..
    } = setup();
    transcript(&native_projects, S_A, &user("native one", "/w", &ts(3)));
    transcript(&native_projects, S_B, &user("native two", "/w", &ts(2)));
    let (rows, stderr) = sessions(&sb, &[]);
    assert_eq!((rows.len(), stderr.as_str()), (2, ""));
    let cache = sb.remuda_home().join("state/index.json");
    let saved = |cache: &Path| -> usize {
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(cache).unwrap()).unwrap();
        v["entries"].as_object().unwrap().len()
    };

    let project = native_projects.canonicalize().unwrap().join("-w-proj");
    fs::set_permissions(&project, fs::Permissions::from_mode(0o444)).unwrap();
    let (rows, stderr) = sessions(&sb, &[]);
    fs::set_permissions(&project, fs::Permissions::from_mode(0o755)).unwrap();
    let titles: Vec<&str> = rows.iter().map(|r| r["TITLE"].as_str()).collect();
    assert_eq!(titles, ["native one", "native two"]);
    assert_eq!(
        stderr
            .strip_prefix(&format!(
                "remuda: warning: cannot read {}: ",
                project.display()
            ))
            .and_then(|rest| rest.split_once("; "))
            .map(|(_, kept)| kept),
        Some("2 sessions below it are listed as last indexed\n"),
        "{stderr}"
    );
    assert_eq!(saved(&cache), 2, "the cache keeps them");

    let (rows, stderr) = sessions(&sb, &[]);
    assert_eq!((rows.len(), stderr.as_str()), (2, ""));
}

/// R8 (review #10): a home that cannot be searched does not make its store an absent one. The
/// store cannot be resolved, so its sessions stay listed and in the cache that is saved, with
/// a warning naming it as the home gives it.
#[test]
fn a_store_that_cannot_be_resolved_is_warned_about_and_its_sessions_stay_listed() {
    let Setup {
        sb,
        team,
        native_projects,
        ..
    } = setup();
    transcript(&native_projects, S_A, &user("native one", "/w", &ts(3)));
    transcript(&team.join("projects"), S_B, &user("team one", "/w", &ts(2)));
    transcript(&team.join("projects"), S_C, &user("team two", "/w", &ts(1)));
    let titles = |rows: &[BTreeMap<String, String>]| -> Vec<String> {
        rows.iter().map(|r| r["TITLE"].clone()).collect()
    };
    let (rows, stderr) = sessions(&sb, &[]);
    assert_eq!(titles(&rows), ["native one", "team one", "team two"]);
    assert_eq!(stderr, "");
    let cache = sb.remuda_home().join("state/index.json");
    let saved = |cache: &Path| -> usize {
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(cache).unwrap()).unwrap();
        v["entries"].as_object().unwrap().len()
    };

    fs::set_permissions(&team, fs::Permissions::from_mode(0o000)).unwrap();
    let (rows, stderr) = sessions(&sb, &[]);
    fs::set_permissions(&team, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(titles(&rows), ["native one", "team one", "team two"]);
    let warning: Vec<&str> = stderr
        .lines()
        .filter(|l| l.contains("cannot read"))
        .collect();
    let [warning] = warning.as_slice() else {
        panic!("one warning: {stderr}")
    };
    assert!(
        warning.starts_with(&format!(
            "remuda: warning: cannot read {}: ",
            team.join("projects").display()
        )),
        "{warning}"
    );
    assert!(
        warning.ends_with("; 2 sessions below it are listed as last indexed"),
        "{warning}"
    );
    assert_eq!(saved(&cache), 3, "the cache keeps them");

    let (rows, stderr) = sessions(&sb, &[]);
    assert_eq!((rows.len(), stderr.as_str()), (3, ""));
}

/// R8 (review #10): while one store cannot be resolved, a store that is gone is still gone.
/// Its sessions leave the list and the cache; only those of the store that cannot be resolved
/// stay, with the warning.
#[test]
fn a_store_that_is_gone_drops_out_while_another_cannot_be_resolved() {
    let Setup {
        sb,
        team,
        native_projects,
        ..
    } = setup();
    transcript(&native_projects, S_A, &user("native one", "/w", &ts(3)));
    transcript(&team.join("projects"), S_B, &user("team one", "/w", &ts(2)));
    let (rows, stderr) = sessions(&sb, &[]);
    assert_eq!((rows.len(), stderr.as_str()), (2, ""));

    fs::remove_dir_all(&native_projects).unwrap();
    fs::set_permissions(&team, fs::Permissions::from_mode(0o000)).unwrap();
    let (rows, stderr) = sessions(&sb, &[]);
    fs::set_permissions(&team, fs::Permissions::from_mode(0o755)).unwrap();
    let titles: Vec<&str> = rows.iter().map(|r| r["TITLE"].as_str()).collect();
    assert_eq!(titles, ["team one"]);
    assert!(
        stderr.contains("; 1 session below it is listed as last indexed"),
        "{stderr}"
    );
    let cache = sb.remuda_home().join("state/index.json");
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&cache).unwrap()).unwrap();
    let saved: Vec<&String> = v["entries"].as_object().unwrap().keys().collect();
    assert_eq!(saved.len(), 1, "{saved:?}");
    assert!(saved[0].contains(S_B), "{saved:?}");
}

/// R8 (review #10): an account removed and registered again under the same name with another
/// home, whose store cannot be resolved before it was indexed, does not keep the sessions of
/// the old home: they leave the list and the saved cache, on this run and the next, and the
/// new store is named in the warning with nothing kept below it.
#[test]
fn an_account_registered_again_with_another_home_does_not_keep_the_old_sessions() {
    let sb = common::Sandbox::new();
    let (old, new) = (sb.make_claude_home("p/old"), sb.make_claude_home("p/new"));
    transcript(&old.join("projects"), S_A, &user("old home", "/w", &ts(2)));
    transcript(&new.join("projects"), S_B, &user("new home", "/w", &ts(1)));
    sb.register(&[("work", &old)]);
    let (rows, stderr) = sessions(&sb, &[]);
    assert_eq!((rows.len(), stderr.as_str()), (1, ""));
    assert_eq!(rows[0]["TITLE"], "old home");
    let cache = sb.remuda_home().join("state/index.json");
    let saved = |cache: &Path| -> usize {
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(cache).unwrap()).unwrap();
        v["entries"].as_object().unwrap().len()
    };

    sb.register(&[("work", &new)]);
    fs::set_permissions(&new, fs::Permissions::from_mode(0o000)).unwrap();
    let runs = [sessions(&sb, &[]), sessions(&sb, &[])];
    let left = saved(&cache);
    fs::set_permissions(&new, fs::Permissions::from_mode(0o755)).unwrap();
    for (rows, stderr) in &runs {
        assert!(rows.is_empty(), "{rows:?}");
        let warnings: Vec<&str> = stderr
            .lines()
            .filter(|l| l.contains("cannot read"))
            .collect();
        let [warning] = warnings.as_slice() else {
            panic!("one warning: {stderr}")
        };
        assert!(
            warning.starts_with(&format!(
                "remuda: warning: cannot read {}: ",
                new.join("projects").display()
            )),
            "{warning}"
        );
        assert!(!warning.contains("below it"), "nothing is kept: {warning}");
    }
    assert_eq!(left, 0, "the saved cache does not keep the old home's");

    let (rows, stderr) = sessions(&sb, &[]);
    assert_eq!((rows.len(), stderr.as_str()), (1, ""));
    assert_eq!(rows[0]["TITLE"], "new home");
}

#[test]
fn index_cache_is_written_and_reused() {
    let Setup {
        sb,
        native_projects,
        ..
    } = setup();
    transcript(&native_projects, S_A, &user("cached", "/w", &ts(1)));
    let (first, _) = sessions(&sb, &[]);
    let cache = sb.remuda_home().join("state/index.json");
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&cache).unwrap()).unwrap();
    assert_eq!(v["schema_version"], remuda::index::SCHEMA_VERSION);
    assert_eq!(v["entries"].as_object().unwrap().len(), 1);

    // Unchanged transcript: the cached entry is used as is (a doctored title shows through).
    let key = v["entries"]
        .as_object()
        .unwrap()
        .keys()
        .next()
        .unwrap()
        .clone();
    let mut doctored = v.clone();
    doctored["entries"][&key]["first_user_text"] = serde_json::json!("from cache");
    fs::write(&cache, doctored.to_string()).unwrap();
    let (second, _) = sessions(&sb, &[]);
    assert_eq!(first[0]["TITLE"], "cached");
    assert_eq!(second[0]["TITLE"], "from cache");

    // A deleted cache is rebuilt.
    fs::remove_file(&cache).unwrap();
    let (third, _) = sessions(&sb, &[]);
    assert_eq!(third, first);
}

#[test]
fn sessions_works_without_claude_on_path() {
    let Setup {
        sb,
        native_projects,
        ..
    } = setup();
    transcript(&native_projects, S_A, &user("q", "/w", &ts(1)));
    fs::remove_file(sb.bin().join("claude")).unwrap();
    let (rows, stderr) = sessions(&sb, &[]);
    assert_eq!(rows.len(), 1);
    assert_eq!(stderr, "");
}

/// Display columns (CJK characters take two).
fn columns(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

#[test]
fn cjk_titles_align_and_truncate_by_display_width() {
    let Setup {
        sb,
        native_projects,
        ..
    } = setup();
    transcript(
        &native_projects,
        S_A,
        &user("修复索引的增量扫描", "/w/a", &ts(2)),
    );
    transcript(&native_projects, S_B, &user("plain", "/w/b", &ts(1)));
    transcript(
        &native_projects,
        S_C,
        &user(&"长".repeat(80), "/w/c", &ts(3)),
    );
    let out = sb
        .remuda()
        .arg("sessions")
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8(out.stdout).unwrap();
    // Every CWD value starts at the column where the header says CWD starts.
    let lines: Vec<&str> = stdout.lines().collect();
    let cwd_col = columns(&lines[0][..lines[0].find("CWD").unwrap()]);
    for line in &lines[1..] {
        let at = line.find("/w/").unwrap();
        assert_eq!(columns(&line[..at]), cwd_col, "{stdout}");
    }
    // The long title is cut to 60 columns, not 60 characters.
    let long = lines.iter().find(|l| l.contains("长长")).unwrap();
    let title = long.split_whitespace().nth(3).unwrap();
    assert!(title.ends_with('…'), "{title}");
    assert!(columns(title) <= 60 && columns(title) >= 59, "{title}");
}
