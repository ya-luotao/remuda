//! R8, R17: codex rollouts in the session index, their titles and their preview.

mod common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use common::rollouts::*;
use remuda::index::{self, Entry, Index, Store};
use remuda::provider::{Provider, codex};
use remuda::registry::{Account, Home};
use remuda::transcript::{Message, Role};
use serde_json::json;
use tempfile::TempDir;

const ID: &str = "019c1e08-e4f6-7d70-a129-38ec744a3f3c";
const PARENT: &str = "019c1e00-0000-7000-8000-000000000000";
const OTHER: &str = "019c1e09-b0ff-7842-aca4-1397c3b7b047";

/// A codex home `<tmp>/c/work` registered as `codex:work`.
struct CodexHome {
    tmp: TempDir,
}

impl CodexHome {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("c/work/sessions")).unwrap();
        CodexHome { tmp }
    }

    fn home(&self) -> PathBuf {
        self.tmp.path().canonicalize().unwrap().join("c/work")
    }

    fn account(&self) -> Account {
        Account {
            provider: Provider::Codex,
            name: "work".into(),
            home: Home::Path(self.home().display().to_string()),
        }
    }

    fn stores(&self) -> Vec<Store> {
        index::stores(&[self.account()], &remuda::Env::new())
    }

    fn write(&self, id: &str, contents: &str) -> PathBuf {
        write_rollout(&self.home(), id, contents)
    }

    fn refresh(&self, index: &mut Index) -> index::RefreshStats {
        index::refresh(index, &self.stores(), |_| {})
    }

    fn scan(&self, path: &Path) -> Entry {
        let mut index = Index::default();
        self.refresh(&mut index);
        index.entries.get(path).cloned().expect("indexed")
    }
}

fn cli() -> serde_json::Value {
    json!("cli")
}

#[test]
fn a_codex_home_is_one_store_of_its_sessions() {
    let h = CodexHome::new();
    let stores = h.stores();
    assert_eq!(
        stores,
        [Store {
            provider: Provider::Codex,
            path: h.home().join("sessions"),
            accounts: vec!["codex:work".into()],
            thread_names: vec![h.home().join("session_index.jsonl")],
        }]
    );
    // codex:default is `$HOME/.codex`; a home without sessions/ has no store.
    let native = h.tmp.path().join("home/.codex/sessions");
    fs::create_dir_all(&native).unwrap();
    let env = [(
        "HOME".to_string(),
        h.tmp.path().join("home").display().to_string(),
    )]
    .into();
    let empty = Account {
        name: "empty".into(),
        home: Home::Path(h.tmp.path().join("c/empty").display().to_string()),
        ..h.account()
    };
    let stores = index::stores(&[Account::default_for(Provider::Codex), empty], &env);
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].path, native.canonicalize().unwrap());
    assert_eq!(stores[0].accounts, ["codex:default"]);
}

/// The first `session_meta` gives id, cwd, source and originator (a fork parent's meta after
/// it does not); the first real user text skips what codex injects.
#[test]
fn a_rollout_is_read_from_its_first_meta_and_first_real_user_text() {
    let h = CodexHome::new();
    let path = h.write(
        ID,
        &[
            meta(
                ID,
                "/w/start",
                json!({"subagent": {"thread_spawn": {}}}),
                100,
                &ts(0),
            ),
            parent_meta(PARENT, "/w/parent", &ts(0)),
            developer("## Memory\nuse it", &ts(1)),
            user(&[AGENTS_MD, ENVIRONMENT], &ts(1)),
            user(&["<user_instructions>\nx\n</user_instructions>"], &ts(1)),
            turn_context("/w/later", &ts(2)),
            event("user_message", "fix the index", &ts(2)),
            user(&["fix the index"], &ts(2)),
            assistant("On it.", &ts(3)),
        ]
        .concat(),
    );
    let e = h.scan(&path);
    assert_eq!(e.provider, Provider::Codex);
    assert_eq!(e.session_id, ID);
    assert_eq!(e.source.as_deref(), Some("subagent"));
    assert_eq!(e.originator.as_deref(), Some("codex-tui"));
    assert_eq!(e.cwd_first.as_deref(), Some("/w/start"));
    assert_eq!(e.cwd_last.as_deref(), Some("/w/later"));
    assert_eq!(e.ts_first.as_deref(), Some(ts(0).as_str()));
    assert_eq!(e.ts_last.as_deref(), Some(ts(3).as_str()));
    assert_eq!(e.first_user_text.as_deref(), Some("fix the index"));
    assert_eq!(e.title, None);
    assert_eq!(e.display_title(), Some("fix the index"));
    assert_eq!(e.entrypoint, None);
}

/// The request in a VS Code message sits under `## My request for Codex:`.
#[test]
fn the_request_in_an_ide_context_block_is_the_user_text() {
    let h = CodexHome::new();
    let path = h.write(
        ID,
        &[
            meta(ID, "/w", json!("vscode"), 10, &ts(0)),
            user(
                &[
                    ENVIRONMENT,
                    "# Context from my IDE setup:\n\n## Active file: a.rs\n\n\
                     ## My request for Codex:\nrename the thing\n",
                ],
                &ts(1),
            ),
        ]
        .concat(),
    );
    let e = h.scan(&path);
    assert_eq!(e.first_user_text.as_deref(), Some("rename the thing"));
    assert_eq!(e.source.as_deref(), Some("vscode"));
    // Without a request in it, the block is skipped like any injection.
    let path = h.write(
        OTHER,
        &[
            meta(OTHER, "/w", cli(), 10, &ts(0)),
            user(
                &["# Context from my IDE setup:\n\n## Active file: a.rs\n"],
                &ts(1),
            ),
            user(&["<image>", "what is this"], &ts(2)),
        ]
        .concat(),
    );
    assert_eq!(
        h.scan(&path).first_user_text.as_deref(),
        Some("what is this")
    );
}

/// Real rollouts put ~20-40 KB of instructions first: the head window grows until it has the
/// first real user text (R17), and the tail still gives the last activity.
#[test]
fn the_first_user_text_is_found_past_the_first_window() {
    let h = CodexHome::new();
    let big = "a".repeat(150 * 1024);
    let mut text = [
        meta(ID, "/w", cli(), 40 * 1024, &ts(0)),
        user(
            &[
                &format!("# AGENTS.md instructions for /w\n\n{big}"),
                ENVIRONMENT,
            ],
            &ts(1),
        ),
        user(&["the real question"], &ts(2)),
    ]
    .concat();
    for i in 0..400 {
        text.push_str(&tool_output(&"o".repeat(1024), &ts(3 + i / 100)));
    }
    text.push_str(&assistant("done", &ts(9)));
    let path = h.write(ID, &text);
    let e = h.scan(&path);
    assert_eq!(e.first_user_text.as_deref(), Some("the real question"));
    assert_eq!(e.ts_last.as_deref(), Some(ts(9).as_str()));
    // No turn_context anywhere: the last cwd is the meta's.
    assert_eq!(e.cwd_last.as_deref(), Some("/w"));
}

/// Titles are codex's thread names: the last line for the id in `session_index.jsonl`. It
/// changes on its own, so an unchanged rollout still gets the new name.
#[test]
fn titles_come_from_the_session_index() {
    let h = CodexHome::new();
    let path = h.write(
        ID,
        &[meta(ID, "/w", cli(), 10, &ts(0)), user(&["hello"], &ts(1))].concat(),
    );
    let names = h.home().join("session_index.jsonl");
    fs::write(
        &names,
        [thread_name(ID, "First name"), thread_name(OTHER, "Other")].concat() + "{bad\n",
    )
    .unwrap();
    let mut index = Index::default();
    h.refresh(&mut index);
    assert_eq!(index.entries[&path].title.as_deref(), Some("First name"));

    fs::OpenOptions::new()
        .append(true)
        .open(&names)
        .unwrap()
        .write_all(thread_name(ID, "Renamed").as_bytes())
        .unwrap();
    let stats = h.refresh(&mut index);
    assert_eq!(stats.reused, 1);
    assert_eq!(index.entries[&path].title.as_deref(), Some("Renamed"));
    fs::remove_file(&names).unwrap();
    h.refresh(&mut index);
    assert_eq!(index.entries[&path].title, None);
    assert_eq!(index.entries[&path].display_title(), Some("hello"));
}

/// One line of `session_index.jsonl` renaming `id` at `updated_at`.
fn renamed(id: &str, name: &str, updated_at: &str) -> String {
    serde_json::to_string(&json!({"id": id, "thread_name": name, "updated_at": updated_at}))
        .unwrap()
        + "\n"
}

/// Two codex homes whose `sessions` resolve to one directory are one store of both accounts,
/// and both homes' `session_index.jsonl` name its threads: per id, the newest `updated_at`
/// across the files wins, whichever home wrote it.
#[test]
fn a_shared_codex_store_takes_thread_names_from_every_home() {
    let h = CodexHome::new();
    let work = h.home();
    let alt = h.tmp.path().canonicalize().unwrap().join("c/alt");
    fs::create_dir_all(&alt).unwrap();
    std::os::unix::fs::symlink(work.join("sessions"), alt.join("sessions")).unwrap();
    let id_path = h.write(ID, &[meta(ID, "/w", cli(), 10, &ts(0))].concat());
    let other_path = h.write(OTHER, &[meta(OTHER, "/w", cli(), 10, &ts(0))].concat());
    fs::write(
        work.join("session_index.jsonl"),
        [
            renamed(ID, "Named in work", "2026-09-20T10:00:00Z"),
            renamed(OTHER, "Newer in work", "2026-09-20T12:00:00Z"),
        ]
        .concat(),
    )
    .unwrap();
    fs::write(
        alt.join("session_index.jsonl"),
        [
            renamed(ID, "Renamed in alt", "2026-09-20T11:00:00Z"),
            renamed(OTHER, "Older in alt", "2026-09-20T09:00:00Z"),
        ]
        .concat(),
    )
    .unwrap();
    let accounts = [
        h.account(),
        Account {
            provider: Provider::Codex,
            name: "alt".into(),
            home: Home::Path(alt.display().to_string()),
        },
    ];
    let stores = index::stores(&accounts, &remuda::Env::new());
    assert_eq!(stores.len(), 1, "{stores:?}");
    assert_eq!(stores[0].accounts, ["codex:work", "codex:alt"]);
    assert_eq!(
        stores[0].thread_names,
        [
            work.join("session_index.jsonl"),
            alt.join("session_index.jsonl")
        ]
    );
    let mut index = Index::default();
    index::refresh(&mut index, &stores, |_| {});
    assert_eq!(
        index.entries[&id_path].title.as_deref(),
        Some("Renamed in alt")
    );
    assert_eq!(
        index.entries[&other_path].title.as_deref(),
        Some("Newer in work")
    );
}

/// Across the files of a shared store a line without `updated_at` is older than any with one;
/// on a tie, or when neither has one, the later file wins. Within a file the last line wins.
#[test]
fn thread_names_across_files_newest_then_later_file() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("a.jsonl"), tmp.path().join("b.jsonl"));
    let bare = |id: &str, name: &str| {
        serde_json::to_string(&json!({"id": id, "thread_name": name})).unwrap() + "\n"
    };
    let t = "2026-09-20T10:00:00Z";
    fs::write(
        &a,
        [
            renamed("dated", "dated in a", t),
            bare("undated", "undated in a"),
            renamed("tie", "tie in a", t),
            bare("neither", "neither in a"),
            renamed("last", "first line", "2026-09-20T12:00:00Z"),
            renamed("last", "last line", t),
        ]
        .concat(),
    )
    .unwrap();
    fs::write(
        &b,
        [
            bare("dated", "undated in b"),
            renamed("undated", "dated in b", t),
            renamed("tie", "tie in b", t),
            bare("neither", "neither in b"),
        ]
        .concat(),
    )
    .unwrap();
    let names = codex::thread_names(&[&a, &b]);
    assert_eq!(names["dated"], "dated in a");
    assert_eq!(names["undated"], "dated in b");
    assert_eq!(names["tie"], "tie in b");
    assert_eq!(names["neither"], "neither in b");
    assert_eq!(names["last"], "last line");
    // A missing file has none.
    assert_eq!(codex::thread_names(&[tmp.path().join("missing")]).len(), 0);
}

/// Rollouts are append-only like transcripts: new records are read incrementally.
#[test]
fn appended_records_are_read_incrementally() {
    let h = CodexHome::new();
    let path = h.write(
        ID,
        &[
            meta(ID, "/w", cli(), 10, &ts(0)),
            user(&[ENVIRONMENT], &ts(0)),
        ]
        .concat(),
    );
    let mut index = Index::default();
    h.refresh(&mut index);
    assert_eq!(index.entries[&path].first_user_text, None);
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(
            [
                user(&["now the question"], &ts(1)),
                parent_meta(PARENT, "/elsewhere", &ts(1)),
                turn_context("/w2", &ts(2)),
            ]
            .concat()
            .as_bytes(),
        )
        .unwrap();
    let stats = h.refresh(&mut index);
    assert_eq!(stats.incremental, 1);
    let e = &index.entries[&path];
    assert_eq!(e.first_user_text.as_deref(), Some("now the question"));
    assert_eq!(e.session_id, ID);
    assert_eq!(e.source.as_deref(), Some("cli"));
    assert_eq!(e.cwd_last.as_deref(), Some("/w2"));
    assert_eq!(e.ts_last.as_deref(), Some(ts(2).as_str()));
}

/// Only `sessions/**/rollout-*.jsonl`; the 2025 format degrades to what it has.
#[test]
fn what_counts_as_a_rollout() {
    let h = CodexHome::new();
    let old = h.write(
        OTHER,
        &[
            old_first(OTHER, &ts(0)),
            old_state(),
            old_user(ENVIRONMENT),
            old_user("init a project"),
        ]
        .concat(),
    );
    let dir = h.home().join("sessions/2025/04/18");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("rollout-2025-04-18-{ID}.json")), "{}\n").unwrap();
    fs::write(dir.join("notes.jsonl"), "{}\n").unwrap();
    let mut index = Index::default();
    h.refresh(&mut index);
    assert_eq!(index.entries.keys().collect::<Vec<_>>(), [&old]);
    let e = &index.entries[&old];
    assert_eq!(e.session_id, OTHER);
    assert_eq!(e.first_user_text.as_deref(), Some("init a project"));
    assert_eq!(e.source, None);
    assert_eq!(e.cwd_last, None);
}

fn u(text: &str) -> Message {
    Message {
        role: Role::User,
        text: text.into(),
    }
}

fn a(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        text: text.into(),
    }
}

/// R17: the last N user / assistant messages; tool calls as `[tool: <name>]`; reasoning,
/// tool output, developer text and injections skipped; one assistant turn is one message.
#[test]
fn preview_of_a_rollout() {
    let h = CodexHome::new();
    let path = h.write(
        ID,
        &[
            meta(ID, "/w", cli(), 10, &ts(0)),
            developer("secret developer text", &ts(0)),
            user(&[AGENTS_MD, ENVIRONMENT], &ts(0)),
            user(&["fix the bug"], &ts(1)),
            reasoning("thinking hard", &ts(1)),
            assistant("Let me look.", &ts(2)),
            function_call("exec_command", &ts(2)),
            tool_output("huge output", &ts(2)),
            custom_tool_call("apply_patch", &ts(3)),
            event("agent_message", "Done.", &ts(3)),
            assistant("Done.", &ts(3)),
            user(&["thanks"], &ts(4)),
        ]
        .concat(),
    );
    assert_eq!(
        codex::preview(&path, 10).unwrap(),
        [
            u("fix the bug"),
            a("Let me look.\n[tool: exec_command]\n[tool: apply_patch]\nDone."),
            u("thanks")
        ]
    );
    assert_eq!(codex::preview(&path, 1).unwrap(), [u("thanks")]);
}
