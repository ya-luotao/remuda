//! R9: session attribution from the launch log, live sessions and `history.jsonl`.

mod common;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use remuda::Env;
use remuda::attribution::{self, Attribution};
use remuda::live::{LiveSession, Source};
use remuda::provider::Provider;
use remuda::registry::{Account, Home};

fn named(name: &str, home: &Path) -> Account {
    Account {
        provider: Provider::Claude,
        name: name.into(),
        home: Home::Path(home.display().to_string()),
    }
}

fn env_home(home: &Path) -> Env {
    [("HOME".to_string(), home.display().to_string())]
        .into_iter()
        .collect()
}

fn live(account: &str, session_id: Option<&str>) -> LiveSession {
    LiveSession {
        account: account.into(),
        pid: Some(1),
        short_id: None,
        cwd: None,
        kind: None,
        started_at: None,
        session_id: session_id.map(str::to_string),
        name: None,
        status: None,
        source: Source::Agents,
    }
}

fn history_line(session_id: &str) -> String {
    format!(
        "{{\"display\":\"a prompt\",\"pastedContents\":{{}},\"timestamp\":1790000000000,\
         \"project\":\"/w\",\"sessionId\":\"{session_id}\"}}\n"
    )
}

#[test]
fn launch_log_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("launches.jsonl");
    fs::write(
        &log,
        [
            r#"{"ts":"2026-09-20T10:00:00Z","account":"claude:max","home":"/p/max","cwd":"/w","args":[],"session_id":"s-1","injected":true}"#,
            r#"{"ts":"2026-09-20T10:01:00Z","account":"claude:max","home":"/p/max","cwd":"/w","args":["--resume","s-2","--fork-session"],"session_id":null,"injected":false}"#,
            r#"{"ts":"2026-09-20T10:02:00Z","account":"claude:team","home":"/p/team","cwd":"/w","args":["--resume","s-1"],"session_id":"s-1","injected":false}"#,
            r#"{"account":"claude:team"}"#,
            r#"{"session_id":"s-no-account"}"#,
            "garbage",
            r#"{"ts":"2026-09-20T10:03:00Z","account":"claude:default","home":"default","cwd":null,"args":[],"session_id":"s-3","injected":true}"#,
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();
    let mut a = Attribution::default();
    a.add_launch_log(&log);
    assert_eq!(a.accounts("s-1"), ["claude:max", "claude:team"]);
    assert_eq!(a.accounts("s-3"), ["claude:default"]);
    assert!(a.accounts("s-2").is_empty());
    assert!(a.accounts("s-no-account").is_empty());
    assert_eq!(a.len(), 2);
}

#[test]
fn live_alone() {
    let mut a = Attribution::default();
    a.add_live(&[
        live("claude:max", Some("s-1")),
        live("claude:team", None),
        live("claude:team", Some("s-1")),
    ]);
    assert_eq!(a.accounts("s-1"), ["claude:max", "claude:team"]);
    assert_eq!(a.len(), 1);
}

#[test]
fn history_alone_skips_lines_without_a_session_id() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("history.jsonl");
    fs::write(
        &path,
        [
            history_line("s-1"),
            "{\"display\":\"old entry\",\"pastedContents\":{},\"timestamp\":1,\"project\":\"/w\"}\n"
                .into(),
            "not json\n".into(),
            "{\"sessionId\": 42}\n".into(),
            history_line("s-2"),
            history_line("s-1"),
            // Unfinished last line (claude may be mid-write).
            "{\"sessionId\":\"s-partial".into(),
        ]
        .concat(),
    )
    .unwrap();
    let mut a = Attribution::default();
    a.add_history(&path, "claude:max");
    assert_eq!(a.accounts("s-1"), ["claude:max"]);
    assert_eq!(a.accounts("s-2"), ["claude:max"]);
    assert_eq!(a.len(), 2);
}

#[test]
fn missing_files_attribute_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let mut a = Attribution::default();
    a.add_launch_log(&tmp.path().join("nope.jsonl"));
    a.add_history(&tmp.path().join("nope.jsonl"), "claude:max");
    assert!(a.is_empty());
    let accounts = [
        Account::default_for(Provider::Claude),
        named("max", &tmp.path().join("max")),
    ];
    let a = attribution::collect(
        &accounts,
        &env_home(&tmp.path().join("home")),
        &tmp.path().join("state/launches.jsonl"),
        &[],
    );
    assert!(a.is_empty());
}

#[test]
fn history_paths() {
    let env = env_home(Path::new("/h"));
    assert_eq!(
        attribution::history_path(&Account::default_for(Provider::Claude), &env),
        Some("/h/.claude/history.jsonl".into())
    );
    assert_eq!(
        attribution::history_path(&named("max", Path::new("/p/max/")), &env),
        Some("/p/max/history.jsonl".into())
    );
}

#[test]
fn all_sources_are_merged() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let home = root.join("home");
    let (max, team, alt) = (root.join("p/max"), root.join("p/team"), root.join("p/alt"));
    for d in [&home.join(".claude"), &max, &team, &alt] {
        fs::create_dir_all(d).unwrap();
    }
    fs::write(home.join(".claude/history.jsonl"), history_line("s-native")).unwrap();
    fs::write(
        max.join("history.jsonl"),
        [history_line("s-both"), history_line("s-max")].concat(),
    )
    .unwrap();
    fs::write(team.join("history.jsonl"), history_line("s-both")).unwrap();
    // `alt` shares team's history file: it cannot tell the two apart, so it is not used.
    symlink(team.join("history.jsonl"), alt.join("history.jsonl")).unwrap();
    let log = root.join("state/launches.jsonl");
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    fs::write(
        &log,
        r#"{"account":"claude:alt","session_id":"s-launched"}"#.to_string() + "\n",
    )
    .unwrap();

    let accounts = [
        Account::default_for(Provider::Claude),
        named("max", &max),
        named("team", &team),
        named("alt", &alt),
    ];
    let a = attribution::collect(
        &accounts,
        &env_home(&home),
        &log,
        &[live("claude:team", Some("s-max"))],
    );
    assert_eq!(a.accounts("s-native"), ["claude:default"]);
    assert_eq!(a.accounts("s-max"), ["claude:max", "claude:team"]);
    assert!(a.accounts("s-both").contains(&"claude:max"));
    assert!(
        !a.accounts("s-both").contains(&"claude:team"),
        "shared history ignored"
    );
    assert!(!a.accounts("s-both").contains(&"claude:alt"));
    assert_eq!(a.accounts("s-launched"), ["claude:alt"]);
    assert_eq!(a.len(), 4);
}
