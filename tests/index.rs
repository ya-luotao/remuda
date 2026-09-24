//! R8: the session index — head/tail cold scan, incremental refresh, store dedup, cache.

mod common;

use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use common::transcripts::*;
use remuda::index::{self, Entry, Index, RefreshStats, SCHEMA_VERSION, Store};
use remuda::provider::Provider;
use remuda::registry::{Account, Home};
use tempfile::TempDir;

const SID: &str = "11111111-1111-4111-8111-111111111111";

/// A single store at `<tmp>/projects` with one project directory.
struct Corpus {
    tmp: TempDir,
}

impl Corpus {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("projects/-w-a")).unwrap();
        Corpus { tmp }
    }

    fn store(&self) -> Store {
        Store {
            provider: Provider::Claude,
            path: self.tmp.path().join("projects").canonicalize().unwrap(),
            accounts: vec!["claude:default".into()],
            thread_names: vec![],
        }
    }

    fn file(&self, sid: &str) -> PathBuf {
        self.store().path.join("-w-a").join(format!("{sid}.jsonl"))
    }

    fn write(&self, sid: &str, contents: &str) -> PathBuf {
        let path = self.file(sid);
        fs::write(&path, contents).unwrap();
        path
    }

    fn append(&self, sid: &str, contents: &str) {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(self.file(sid))
            .unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    fn refresh(&self, index: &mut Index) -> RefreshStats {
        index::refresh(index, &[self.store()], |_| {})
    }

    /// A fresh index refreshed once; returns the entry for `sid`.
    fn scan(&self, sid: &str) -> Entry {
        let mut index = Index::default();
        self.refresh(&mut index);
        entry(&index, &self.file(sid))
    }
}

fn entry(index: &Index, path: &Path) -> Entry {
    index
        .entries
        .get(path)
        .unwrap_or_else(|| {
            panic!(
                "no entry for {}: {:?}",
                path.display(),
                index.entries.keys()
            )
        })
        .clone()
}

/// `n` records of 10 KB each: realistic line sizes, so the tail window holds whole records.
fn many_fillers(n: usize, cwd: &str, ts: &str) -> String {
    (0..n).map(|_| filler(10_000, cwd, ts)).collect()
}

fn set_mtime(path: &Path, t: SystemTime) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

#[test]
fn large_file_tail_title_wins_and_head_fields_come_from_head() {
    let c = Corpus::new();
    let text = [
        user_from("sdk-rb-client", "first question", "/w/a", &ts(1)),
        many_fillers(8, "/w/a", &ts(2)),
        ai_title("Middle title"),
        many_fillers(8, "/w/a", &ts(3)),
        ai_title("Tail title"),
        assistant_text("msg_1", "answer", "/w/a", &ts(4)),
    ]
    .concat();
    let path = c.write(SID, &text);
    let e = c.scan(SID);
    assert_eq!(e.session_id, SID);
    assert_eq!(e.path, path);
    assert_eq!(e.store, c.store().path);
    assert_eq!(e.size, text.len() as u64);
    assert_eq!(e.scanned_offset, text.len() as u64);
    assert!(e.gap, "a >128 KB file is read as head + tail");
    assert_eq!(e.title.as_deref(), Some("Tail title"));
    assert_eq!(e.display_title(), Some("Tail title"));
    assert_eq!(e.first_user_text.as_deref(), Some("first question"));
    assert_eq!(e.entrypoint.as_deref(), Some("sdk-rb-client"));
    assert_eq!(e.cwd_first.as_deref(), Some("/w/a"));
    assert_eq!(e.cwd_last.as_deref(), Some("/w/a"));
    assert_eq!(e.ts_first, Some(ts(1)));
    assert_eq!(e.ts_last, Some(ts(4)));
    assert_eq!(e.last_activity(), Some(ts(4).parse().unwrap()));
}

/// Known limitation (R8): only the head and tail windows are read cold, so an `ai-title`
/// that exists only in the middle of a large file is not seen.
#[test]
fn large_file_with_only_a_middle_title_falls_back_to_first_user_text() {
    let c = Corpus::new();
    let text = [
        user("what is up", "/w/a", &ts(1)),
        many_fillers(8, "/w/a", &ts(2)),
        ai_title("Middle title"),
        many_fillers(8, "/w/a", &ts(3)),
    ]
    .concat();
    c.write(SID, &text);
    let e = c.scan(SID);
    assert_eq!(e.title, None);
    assert_eq!(e.display_title(), Some("what is up"));
}

#[test]
fn small_file_is_read_whole() {
    let c = Corpus::new();
    let text = [
        ai_title("Early title"),
        user("hi", "/w/a", &ts(1)),
        filler(50_000, "/w/a", &ts(2)),
        filler(50_000, "/w/b", &ts(3)),
    ]
    .concat();
    assert!(text.len() < 128 * 1024);
    c.write(SID, &text);
    let e = c.scan(SID);
    assert!(!e.gap);
    assert_eq!(e.title.as_deref(), Some("Early title"));
    assert_eq!(e.cwd_last.as_deref(), Some("/w/b"));
}

#[test]
fn first_user_text_skips_meta_and_tool_results_and_is_normalized() {
    let c = Corpus::new();
    let long = format!("{}{}", "a".repeat(250), " tail");
    c.write(
        "s1",
        &[
            meta_user("<local-command-caveat>Caveat</local-command-caveat>", "/w", &ts(1)),
            tool_result("tool output", "/w", &ts(2)),
            user("   ", "/w", &ts(3)),
            user_blocks(
                serde_json::json!([{"type": "image", "source": {}}, {"type": "text", "text": "  hello\n\n  world  "}]),
                "/w",
                &ts(4),
            ),
            user("second", "/w", &ts(5)),
        ]
        .concat(),
    );
    c.write("s2", &user(&long, "/w", &ts(1)));
    let mut index = Index::default();
    c.refresh(&mut index);
    let e1 = entry(&index, &c.file("s1"));
    assert_eq!(e1.first_user_text.as_deref(), Some("hello world"));
    assert_eq!(
        e1.ts_first,
        Some(ts(1)),
        "meta records still carry cwd/timestamp"
    );
    let e2 = entry(&index, &c.file("s2"));
    let want = format!("{}…", "a".repeat(199));
    assert_eq!(e2.first_user_text.as_deref(), Some(want.as_str()));
}

/// What claude writes for a local command such as `/clear` at the start of a session.
fn local_command(name: &str, minute: u32) -> String {
    [
        meta_user(
            "<local-command-caveat>Caveat: The messages below were generated by the user while \
             running local commands.</local-command-caveat>",
            "/w",
            &ts(minute),
        ),
        user(
            &format!(
                "<command-name>/{name}</command-name>\n            \
                 <command-message>{name}</command-message>\n            \
                 <command-args></command-args>"
            ),
            "/w",
            &ts(minute),
        ),
        user(
            "<local-command-stdout></local-command-stdout>",
            "/w",
            &ts(minute),
        ),
    ]
    .concat()
}

#[test]
fn first_user_text_skips_local_commands() {
    let c = Corpus::new();
    c.write(
        "s1",
        &[
            local_command("clear", 1),
            // A slash command claude expands (skills): `<command-message>` comes first.
            user(
                "<command-message>review</command-message>\n<command-name>/review</command-name>",
                "/w",
                &ts(2),
            ),
            user(
                "<local-command-stderr>Error: failed</local-command-stderr>",
                "/w",
                &ts(2),
            ),
            user_blocks(
                serde_json::json!([{"type": "text", "text": "  <command-name>/model</command-name>"}]),
                "/w",
                &ts(3),
            ),
            user("fix the index scan", "/w", &ts(4)),
        ]
        .concat(),
    );
    // A session that is only `/clear` so far: no user text yet, found once it arrives.
    c.write("s2", &local_command("clear", 1));
    let mut index = Index::default();
    c.refresh(&mut index);
    let e1 = entry(&index, &c.file("s1"));
    assert_eq!(e1.first_user_text.as_deref(), Some("fix the index scan"));
    assert_eq!(e1.display_title(), Some("fix the index scan"));
    assert_eq!(entry(&index, &c.file("s2")).first_user_text, None);
    c.append("s2", &user("now the real question", "/w", &ts(5)));
    let s = c.refresh(&mut index);
    assert_eq!(s.incremental, 1);
    assert_eq!(
        entry(&index, &c.file("s2")).first_user_text.as_deref(),
        Some("now the real question")
    );
}

#[test]
fn grown_file_parses_only_the_new_bytes() {
    let c = Corpus::new();
    let first = user("first", "/w/a", &ts(1));
    let text = [
        first.clone(),
        filler(100_000, "/w/a", &ts(2)),
        ai_title("Old"),
    ]
    .concat();
    let path = c.write(SID, &text);
    let mut index = Index::default();
    let s1 = c.refresh(&mut index);
    assert_eq!((s1.cold, s1.incremental, s1.reused), (1, 0, 0));

    // Make the already-scanned prefix unparseable without moving any offsets: a rescan
    // would lose `first_user_text` and `cwd_first`.
    let mut f = fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.seek(SeekFrom::Start(0)).unwrap();
    f.write_all("x".repeat(first.len() - 1).as_bytes()).unwrap();
    drop(f);
    let added = [
        ai_title("New"),
        user("later", "/w/b", &ts(9)),
        tool_result("r", "/w/b", &ts(10)),
    ]
    .concat();
    c.append(SID, &added);

    let s2 = c.refresh(&mut index);
    assert_eq!((s2.cold, s2.incremental, s2.reused), (0, 1, 0));
    assert_eq!(s2.bytes_read, added.len() as u64);
    let e = entry(&index, &path);
    assert_eq!(e.first_user_text.as_deref(), Some("first"));
    assert_eq!(e.cwd_first.as_deref(), Some("/w/a"));
    assert_eq!(e.title.as_deref(), Some("New"));
    assert_eq!(e.cwd_last.as_deref(), Some("/w/b"));
    assert_eq!(e.ts_last, Some(ts(10)));
    assert_eq!(e.size, (text.len() + added.len()) as u64);
    assert_eq!(e.scanned_offset, e.size);

    let s3 = c.refresh(&mut index);
    assert_eq!(
        (s3.cold, s3.incremental, s3.reused, s3.bytes_read),
        (0, 0, 1, 0)
    );
}

#[test]
fn incremental_scan_fills_head_fields_that_were_not_there_yet() {
    let c = Corpus::new();
    c.write(SID, &ai_title("Just started"));
    let mut index = Index::default();
    c.refresh(&mut index);
    assert_eq!(entry(&index, &c.file(SID)).first_user_text, None);
    c.append(SID, &user_from("cli", "now a prompt", "/w/a", &ts(1)));
    let s = c.refresh(&mut index);
    assert_eq!(s.incremental, 1);
    let e = entry(&index, &c.file(SID));
    assert_eq!(e.first_user_text.as_deref(), Some("now a prompt"));
    assert_eq!(e.cwd_first.as_deref(), Some("/w/a"));
    assert_eq!(e.ts_first, Some(ts(1)));
    assert_eq!(e.entrypoint.as_deref(), Some("cli"));
}

#[test]
fn partial_last_line_is_ignored_until_completed() {
    let c = Corpus::new();
    let complete = user("q", "/w/a", &ts(1));
    let partial = ai_title("Done");
    let (head, rest) = partial.split_at(12);
    c.write(SID, &format!("{complete}{head}"));
    let mut index = Index::default();
    c.refresh(&mut index);
    let e = entry(&index, &c.file(SID));
    assert_eq!(e.title, None);
    assert_eq!(e.scanned_offset, complete.len() as u64);

    c.append(SID, rest);
    let s = c.refresh(&mut index);
    assert_eq!(s.incremental, 1);
    assert_eq!(s.bytes_read, partial.len() as u64);
    let e = entry(&index, &c.file(SID));
    assert_eq!(e.title.as_deref(), Some("Done"));
    assert_eq!(e.scanned_offset, (complete.len() + partial.len()) as u64);
}

#[test]
fn small_file_with_a_partial_last_line_still_fills_head_fields_later() {
    let c = Corpus::new();
    let title = ai_title("T");
    c.write(SID, &format!("{title}{{\"type\":\"us"));
    let mut index = Index::default();
    c.refresh(&mut index);
    let e = entry(&index, &c.file(SID));
    assert!(!e.gap);
    assert_eq!(e.scanned_offset, title.len() as u64);
    c.append(SID, &format!("\n{}", user("hello", "/w/a", &ts(1))));
    c.refresh(&mut index);
    let e = entry(&index, &c.file(SID));
    assert_eq!(e.first_user_text.as_deref(), Some("hello"));
}

#[test]
fn partial_last_line_of_a_large_file_is_ignored() {
    let c = Corpus::new();
    let text = [
        user("q", "/w/a", &ts(1)),
        filler(200_000, "/w/a", &ts(2)),
        assistant_text("m", "a", "/w/c", &ts(3)),
    ]
    .concat();
    let tail = user("unfinished", "/w/z", &ts(9));
    c.write(SID, &format!("{text}{}", &tail[..tail.len() - 5]));
    let e = c.scan(SID);
    assert_eq!(e.scanned_offset, text.len() as u64);
    assert_eq!(e.cwd_last.as_deref(), Some("/w/c"));
    assert_eq!(e.ts_last, Some(ts(3)));
}

#[test]
fn tail_window_inside_one_huge_line_grows_until_it_finds_records() {
    let c = Corpus::new();
    let text = [
        user("q", "/w/a", &ts(1)),
        assistant_text("m", "a", "/w/c", &ts(3)),
        filler(300_000, "/w/d", &ts(4)),
    ]
    .concat();
    c.write(SID, &text);
    let e = c.scan(SID);
    assert_eq!(e.scanned_offset, text.len() as u64);
    assert_eq!(e.cwd_last.as_deref(), Some("/w/d"));
    assert_eq!(e.ts_last, Some(ts(4)));
}

/// The tail window grows (64 KB, 256 KB, ...) while it lies inside one huge line; once it
/// reaches the head window the file is covered without a gap, so a middle title is found.
#[test]
fn tail_window_that_grows_into_the_head_leaves_no_gap() {
    let c = Corpus::new();
    let text = [
        user("q", "/w/a", &ts(1)),
        many_fillers(8, "/w/a", &ts(2)),
        ai_title("Middle title"),
        filler(100_000, "/w/e", &ts(5)),
    ]
    .concat();
    c.write(SID, &text);
    let e = c.scan(SID);
    assert!(!e.gap);
    assert_eq!(e.title.as_deref(), Some("Middle title"));
    assert_eq!(e.cwd_last.as_deref(), Some("/w/e"));
    assert_eq!(e.scanned_offset, text.len() as u64);
}

#[test]
fn cross_directory_resume_keeps_first_and_last_cwd() {
    let c = Corpus::new();
    c.write(
        SID,
        &[
            user("start", "/w/a", &ts(1)),
            assistant_text("m1", "ok", "/w/a", &ts(2)),
            user("resumed elsewhere", "/w/b", &ts(3)),
            assistant_text("m2", "ok", "/w/b", &ts(4)),
            ai_title("After resume"),
        ]
        .concat(),
    );
    let e = c.scan(SID);
    assert_eq!(e.cwd_first.as_deref(), Some("/w/a"));
    assert_eq!(e.cwd_last.as_deref(), Some("/w/b"));
    assert_eq!(e.ts_last, Some(ts(4)), "ai-title has no timestamp");
    assert_eq!(e.title.as_deref(), Some("After resume"));
}

#[test]
fn shrunk_file_is_rescanned_cold() {
    let c = Corpus::new();
    c.write(
        SID,
        &[
            user("long one", "/w/a", &ts(1)),
            filler(5_000, "/w/a", &ts(2)),
        ]
        .concat(),
    );
    let mut index = Index::default();
    c.refresh(&mut index);
    c.write(SID, &user("short", "/w/s", &ts(5)));
    let s = c.refresh(&mut index);
    assert_eq!((s.cold, s.incremental), (1, 0));
    let e = entry(&index, &c.file(SID));
    assert_eq!(e.first_user_text.as_deref(), Some("short"));
    assert_eq!(e.cwd_first.as_deref(), Some("/w/s"));
}

#[test]
fn same_size_with_newer_mtime_or_older_mtime_is_rescanned_cold() {
    let c = Corpus::new();
    let path = c.write(SID, &user("aaaa", "/w/a", &ts(1)));
    let mut index = Index::default();
    c.refresh(&mut index);
    let before = fs::metadata(&path).unwrap().modified().unwrap();

    fs::write(&path, user("bbbb", "/w/a", &ts(1))).unwrap();
    set_mtime(&path, before + Duration::from_secs(5));
    let s = c.refresh(&mut index);
    assert_eq!(s.cold, 1);
    assert_eq!(
        entry(&index, &path).first_user_text.as_deref(),
        Some("bbbb")
    );

    // Grown, but with an mtime that went backwards. The prefix is overwritten in place with
    // same-length junk first: a cold rescan loses `first_user_text`, an incremental one
    // would keep the cached value.
    let len = fs::metadata(&path).unwrap().len() as usize;
    fs::write(&path, format!("{}\n", "x".repeat(len - 1))).unwrap();
    c.append(SID, &ai_title("T"));
    set_mtime(&path, before - Duration::from_secs(60));
    let s = c.refresh(&mut index);
    assert_eq!((s.cold, s.incremental), (1, 0));
    let e = entry(&index, &path);
    assert_eq!(e.first_user_text, None);
    assert_eq!(e.title.as_deref(), Some("T"));
}

#[test]
fn replaced_file_that_looks_grown_is_rescanned_cold() {
    let c = Corpus::new();
    let path = c.write(SID, &user("old", "/w/a", &ts(1)));
    let mut index = Index::default();
    c.refresh(&mut index);
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, [user("new", "/w/n", &ts(2)), ai_title("R")].concat()).unwrap();
    fs::rename(&tmp, &path).unwrap();
    let s = c.refresh(&mut index);
    assert_eq!((s.cold, s.incremental), (1, 0));
    assert_eq!(entry(&index, &path).first_user_text.as_deref(), Some("new"));
}

#[test]
fn malformed_lines_are_skipped() {
    let c = Corpus::new();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"not json at all\n");
    bytes.extend_from_slice(b"\xff\xfe invalid utf-8\n");
    bytes.extend_from_slice(b"[1, 2, 3]\n");
    bytes.extend_from_slice(b"{\"type\": \"user\", \"cwd\": 5, \"message\": 3}\n");
    bytes.extend_from_slice(b"\n");
    bytes.extend_from_slice(user("valid", "/w/v", &ts(1)).as_bytes());
    bytes.extend_from_slice(b"{\"type\": \"ai-title\", \"aiTitle\": 42}\n");
    bytes.extend_from_slice(b"{\"type\": \"user\", \"message\": {\"content\": {\"odd\": 1}}}\n");
    bytes.extend_from_slice(b"{truncated\n");
    fs::write(c.file(SID), &bytes).unwrap();
    let e = c.scan(SID);
    assert_eq!(e.first_user_text.as_deref(), Some("valid"));
    assert_eq!(e.cwd_first.as_deref(), Some("/w/v"));
    assert_eq!(e.cwd_last.as_deref(), Some("/w/v"));
    assert_eq!(e.title, None);
}

#[test]
fn only_top_level_jsonl_files_are_indexed() {
    let c = Corpus::new();
    let store = c.store().path;
    c.write(SID, &user("top", "/w/a", &ts(1)));
    fs::create_dir_all(store.join("-w-a").join(SID).join("subagents")).unwrap();
    fs::write(
        store.join("-w-a").join(SID).join("subagents/agent-1.jsonl"),
        user("sub", "/w/a", &ts(1)),
    )
    .unwrap();
    fs::write(store.join("stray.jsonl"), user("stray", "/w", &ts(1))).unwrap();
    fs::write(store.join("-w-a/notes.txt"), "x").unwrap();
    fs::write(store.join("-w-a/.hidden.jsonl.tmp"), "x").unwrap();
    let mut index = Index::default();
    let s = c.refresh(&mut index);
    assert_eq!(s.files, 1);
    let paths: Vec<_> = index.entries.keys().cloned().collect();
    assert_eq!(paths, [c.file(SID)]);
}

#[test]
fn vanished_files_drop_out() {
    let c = Corpus::new();
    c.write("a", &user("a", "/w", &ts(1)));
    c.write("b", &user("b", "/w", &ts(2)));
    let mut index = Index::default();
    c.refresh(&mut index);
    fs::remove_file(c.file("a")).unwrap();
    let s = c.refresh(&mut index);
    assert_eq!((s.files, s.removed, s.reused), (1, 1, 1));
    assert!(!index.entries.contains_key(&c.file("a")));
    // A store that is no longer listed drops out too.
    let s = index::refresh(&mut index, &[], |_| {});
    assert_eq!((s.files, s.removed), (0, 1));
    assert!(index.entries.is_empty());
}

#[test]
fn sorted_is_newest_first() {
    let c = Corpus::new();
    c.write("old", &user("o", "/w", &ts(1)));
    c.write("new", &user("n", "/w", &ts(30)));
    c.write("mid", &user("m", "/w", &ts(20)));
    let mut index = Index::default();
    c.refresh(&mut index);
    let order: Vec<&str> = index
        .sorted()
        .iter()
        .map(|e| e.session_id.as_str())
        .collect();
    assert_eq!(order, ["new", "mid", "old"]);
}

#[test]
fn progress_counts_files_that_need_reading() {
    let c = Corpus::new();
    for i in 0..5 {
        c.write(&format!("s{i}"), &user("x", "/w", &ts(i)));
    }
    let mut index = Index::default();
    let mut seen = Vec::new();
    index::refresh(&mut index, &[c.store()], |p| {
        seen.push((p.done, p.total, p.entry.map(|e| e.session_id.clone())))
    });
    assert_eq!(seen.first(), Some(&(0, 5, None)));
    assert_eq!(seen.len(), 6);
    assert_eq!(seen.last().map(|s| (s.0, s.1)), Some((5, 5)));
    let mut ids: Vec<String> = seen.iter().filter_map(|s| s.2.clone()).collect();
    ids.sort();
    assert_eq!(ids, ["s0", "s1", "s2", "s3", "s4"]);
}

#[test]
fn homes_sharing_one_projects_store_are_scanned_once() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let native = root.join("home/.claude/projects");
    fs::create_dir_all(native.join("-w")).unwrap();
    fs::write(native.join("-w/s1.jsonl"), user("one", "/w", &ts(1))).unwrap();
    fs::write(native.join("-w/s2.jsonl"), user("two", "/w", &ts(2))).unwrap();
    let (max, team, solo, empty) = (
        root.join("p/max"),
        root.join("p/team"),
        root.join("p/solo"),
        root.join("p/empty"),
    );
    for h in [&max, &team, &solo, &empty] {
        fs::create_dir_all(h).unwrap();
    }
    symlink(&native, max.join("projects")).unwrap();
    // A second spelling of the same target (relative link through a symlinked dir).
    symlink("../max/projects", team.join("projects")).unwrap();
    fs::create_dir_all(solo.join("projects/-s")).unwrap();
    fs::write(
        solo.join("projects/-s/s3.jsonl"),
        user("three", "/s", &ts(3)),
    )
    .unwrap();

    let account = |name: &str, home: &Path| Account {
        provider: Provider::Claude,
        name: name.into(),
        home: Home::Path(format!("{}/", home.display())),
    };
    let accounts = vec![
        Account::default_for(Provider::Claude),
        account("max", &max),
        account("team", &team),
        account("solo", &solo),
        account("empty", &empty),
    ];
    let env = [("HOME".to_string(), root.join("home").display().to_string())]
        .into_iter()
        .collect();
    let stores = index::stores(&accounts, &env);
    assert_eq!(
        stores,
        [
            Store {
                provider: Provider::Claude,
                path: native.clone(),
                accounts: vec![
                    "claude:default".into(),
                    "claude:max".into(),
                    "claude:team".into()
                ],
                thread_names: vec![],
            },
            Store {
                provider: Provider::Claude,
                path: solo.join("projects"),
                accounts: vec!["claude:solo".into()],
                thread_names: vec![],
            },
        ]
    );

    let mut index = Index::default();
    let s = index::refresh(&mut index, &stores, |_| {});
    assert_eq!((s.files, s.cold), (3, 3));
    let by_store: Vec<(&str, &Path)> = index
        .sorted()
        .iter()
        .map(|e| (e.session_id.as_str(), e.store.as_path()))
        .collect();
    assert_eq!(
        by_store,
        [
            ("s3", solo.join("projects").as_path()),
            ("s2", native.as_path()),
            ("s1", native.as_path())
        ]
    );
}

#[test]
fn cache_survives_across_runs() {
    let c = Corpus::new();
    c.write("a", &user("a", "/w", &ts(1)));
    c.write("b", &[user("b", "/w", &ts(2)), ai_title("B")].concat());
    let cache = c.tmp.path().join("remuda/state/index.json");
    let mut index = Index::load(&cache);
    assert!(index.entries.is_empty());
    c.refresh(&mut index);
    index.save(&cache).unwrap();

    let mut again = Index::load(&cache);
    assert_eq!(again, index);
    let s = c.refresh(&mut again);
    assert_eq!(
        (s.reused, s.cold, s.incremental, s.bytes_read),
        (2, 0, 0, 0)
    );
    let text = fs::read_to_string(&cache).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["schema_version"], SCHEMA_VERSION);
}

#[test]
fn cache_with_another_schema_or_garbage_is_rebuilt() {
    let c = Corpus::new();
    c.write("a", &user("a", "/w", &ts(1)));
    let cache = c.tmp.path().join("index.json");
    let mut index = Index::default();
    c.refresh(&mut index);
    index.save(&cache).unwrap();

    let mut v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cache).unwrap()).unwrap();
    v["schema_version"] = serde_json::json!(SCHEMA_VERSION + 1);
    fs::write(&cache, v.to_string()).unwrap();
    let mut loaded = Index::load(&cache);
    assert!(loaded.entries.is_empty());
    assert_eq!(loaded.schema_version, SCHEMA_VERSION);
    assert_eq!(c.refresh(&mut loaded).cold, 1);

    // A cache written before local commands were skipped (schema 1) holds such titles.
    v["schema_version"] = serde_json::json!(1);
    fs::write(&cache, v.to_string()).unwrap();
    assert!(
        Index::load(&cache).entries.is_empty(),
        "schema 1 is discarded"
    );

    for garbage in ["", "{", "[]", "{\"schema_version\": 1, \"entries\": 7}"] {
        fs::write(&cache, garbage).unwrap();
        assert!(Index::load(&cache).entries.is_empty(), "{garbage:?}");
    }
    assert!(
        Index::load(&c.tmp.path().join("missing.json"))
            .entries
            .is_empty()
    );
}
