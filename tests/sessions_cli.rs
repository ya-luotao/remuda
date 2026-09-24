//! R5, R8, R9: `remuda sessions [--limit N]`.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::symlink;
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
