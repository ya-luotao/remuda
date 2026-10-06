//! R8: the session index — head/tail cold scan, incremental refresh, store dedup, cache.

mod common;

use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use common::transcripts::*;
use remuda::index::{self, Entry, Given, Index, RefreshStats, SCHEMA_VERSION, Store};
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

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// R8 (review #10): a store that exists but cannot be listed says nothing about its
/// transcripts. Their entries stay as they were, the refresh names the store, and nothing is
/// read again once it can be listed.
#[test]
fn a_store_that_cannot_be_read_keeps_its_entries_and_is_reported() {
    let c = Corpus::new();
    c.write("a", &[user("a", "/w", &ts(1)), ai_title("A")].concat());
    c.write("b", &user("b", "/w", &ts(2)));
    let mut index = Index::default();
    c.refresh(&mut index);
    let before = index.clone();
    let store = c.store().path;

    chmod(&store, 0o000);
    let s = c.refresh(&mut index);
    chmod(&store, 0o755);
    assert_eq!(
        index, before,
        "nothing is known about the store: nothing changes"
    );
    assert_eq!(
        (s.files, s.removed, s.reused, s.cold, s.kept()),
        (2, 0, 0, 0, 2)
    );
    assert_eq!(s.unreadable.len(), 1, "{:?}", s.unreadable);
    assert_eq!(
        (s.unreadable[0].path.as_path(), s.unreadable[0].kept),
        (store.as_path(), 2)
    );
    assert!(!s.unreadable[0].error.is_empty());
    let said = s.incomplete().expect("an incomplete refresh says so");
    assert!(
        said.starts_with(&format!("incomplete: cannot read {}: ", store.display())),
        "{said}"
    );

    let s = c.refresh(&mut index);
    assert_eq!(
        (s.reused, s.cold, s.incremental, s.bytes_read),
        (2, 0, 0, 0),
        "readable again: nothing is read again"
    );
    assert!(s.unreadable.is_empty());
    assert_eq!(s.incomplete(), None);
    assert_eq!(index, before);
}

/// R8 (review #10): only what is below the directory that cannot be listed is kept. A
/// transcript that vanished beside it drops out, and so do those of a directory that is gone.
#[test]
fn a_project_that_cannot_be_read_keeps_its_entries_and_one_that_is_gone_drops_them() {
    let c = Corpus::new();
    let store = c.store();
    let locked = store.path.join("-w-a");
    let open = store.path.join("-w-b");
    fs::create_dir(&open).unwrap();
    c.write("a", &user("a", "/w/a", &ts(1)));
    fs::write(open.join("b.jsonl"), user("b", "/w/b", &ts(2))).unwrap();
    fs::write(open.join("c.jsonl"), user("c", "/w/b", &ts(3))).unwrap();
    let mut index = Index::default();
    c.refresh(&mut index);
    let a = entry(&index, &c.file("a"));

    chmod(&locked, 0o000);
    fs::remove_file(open.join("c.jsonl")).unwrap();
    let s = c.refresh(&mut index);
    chmod(&locked, 0o755);
    assert_eq!(
        (s.files, s.removed, s.reused, s.kept()),
        (2, 1, 1, 1),
        "{:?}",
        s.unreadable
    );
    assert_eq!(s.unreadable.len(), 1);
    assert_eq!(s.unreadable[0].path, locked);
    assert_eq!(entry(&index, &c.file("a")), a);
    assert!(!index.entries.contains_key(&open.join("c.jsonl")));

    // The project directory is gone: so are its transcripts.
    fs::remove_dir_all(&locked).unwrap();
    let s = c.refresh(&mut index);
    assert_eq!((s.files, s.removed, s.reused), (1, 1, 1));
    assert!(s.unreadable.is_empty());

    // And so is the store: a directory that does not exist has no transcripts.
    fs::remove_dir_all(&store.path).unwrap();
    let s = index::refresh(&mut index, std::slice::from_ref(&store), |_| {});
    assert_eq!((s.files, s.removed), (0, 1));
    assert!(s.unreadable.is_empty());
    assert!(index.entries.is_empty());
}

/// R8 (review #10): a project directory that can be listed but not searched gives its
/// transcripts' names and nothing else. They are not taken for vanished: the entries stay, the
/// directory is reported once, and nothing is read again once it can be searched.
#[test]
fn a_project_that_cannot_be_searched_keeps_its_entries_and_is_reported() {
    let c = Corpus::new();
    c.write("a", &user("a", "/w", &ts(1)));
    c.write("b", &user("b", "/w", &ts(2)));
    let mut index = Index::default();
    c.refresh(&mut index);
    let before = index.clone();
    let project = c.store().path.join("-w-a");

    chmod(&project, 0o444);
    let names = fs::read_dir(&project).map(Iterator::count);
    let s = c.refresh(&mut index);
    chmod(&project, 0o755);
    assert_eq!(names.unwrap(), 2, "its names can still be listed");
    assert_eq!(index, before);
    assert_eq!((s.files, s.removed, s.cold, s.kept()), (2, 0, 0, 2));
    assert_eq!(s.unreadable.len(), 1, "{:?}", s.unreadable);
    assert_eq!(
        (s.unreadable[0].path.as_path(), s.unreadable[0].kept),
        (project.as_path(), 2)
    );

    let s = c.refresh(&mut index);
    assert_eq!(
        (s.reused, s.cold, s.incremental, s.bytes_read),
        (2, 0, 0, 0)
    );
    assert!(s.unreadable.is_empty());
}

/// R8: a store no longer listed drops out, also when it lies below a store that is still
/// listed and cannot be read just then: that failure says nothing for another store's entries.
#[test]
fn a_store_no_longer_listed_drops_out_though_one_above_it_cannot_be_read() {
    let c = Corpus::new();
    let outer = c.store();
    let nested = Store {
        path: outer.path.join("nested-home/projects"),
        accounts: vec!["claude:nested".into()],
        ..outer.clone()
    };
    let inside = nested.path.join("-w-n/n.jsonl");
    fs::create_dir_all(inside.parent().unwrap()).unwrap();
    fs::write(&inside, user("nested", "/w/n", &ts(2))).unwrap();
    c.write("a", &user("a", "/w/a", &ts(1)));
    let mut index = Index::default();
    index::refresh(&mut index, &[outer.clone(), nested.clone()], |_| {});
    assert_eq!(entry(&index, &inside).store, nested.path);
    assert_eq!(entry(&index, &c.file("a")).store, outer.path);

    chmod(&outer.path, 0o000);
    let s = index::refresh(&mut index, std::slice::from_ref(&outer), |_| {});
    chmod(&outer.path, 0o755);
    assert_eq!((s.files, s.removed, s.kept()), (1, 1, 1));
    assert!(!index.entries.contains_key(&inside));
    assert!(index.entries.contains_key(&c.file("a")));
}

/// R8: a file is read incrementally only if it still is the cached one, grown, once it is
/// open. Rewritten in place between the listing and the read (the same inode, larger) with an
/// mtime earlier than the cached one, it is scanned cold.
#[test]
fn a_file_rewritten_with_an_earlier_mtime_before_it_is_opened_is_rescanned_cold() {
    let c = Corpus::new();
    let path = c.write(SID, &user("old", "/w/a", &ts(1)));
    let mut index = Index::default();
    c.refresh(&mut index);
    let before = fs::metadata(&path).unwrap().modified().unwrap();
    c.append(SID, &ai_title("T"));
    set_mtime(&path, before + Duration::from_secs(60));

    let rewritten = [
        user("new", "/w/n", &ts(2)),
        filler(2_000, "/w/n", &ts(3)),
        ai_title("N"),
    ]
    .concat();
    // The first report comes after the listing and before anything is read.
    let s = index::refresh(&mut index, &[c.store()], |p| {
        if p.done == 0 {
            fs::write(&path, &rewritten).unwrap();
            set_mtime(&path, before - Duration::from_secs(60));
        }
    });
    assert_eq!((s.incremental, s.cold), (1, 0), "listed as grown");
    let e = entry(&index, &path);
    assert_eq!(e.first_user_text.as_deref(), Some("new"));
    assert_eq!(e.cwd_first.as_deref(), Some("/w/n"));
    assert_eq!(e.title.as_deref(), Some("N"));
    let s = c.refresh(&mut index);
    assert_eq!((s.reused, s.bytes_read), (1, 0), "cached as it was opened");
}

/// R8: a transcript that is a symlink to somewhere out of reach is a single transcript that
/// cannot be read: it is left out, like one that cannot be opened, and is no directory that
/// keeps what the index has. A project directory that is such a symlink is a directory that
/// cannot be read: its entries stay, and it is reported.
#[test]
fn a_symlinked_transcript_out_of_reach_drops_out_and_a_symlinked_project_keeps_its_entries() {
    let c = Corpus::new();
    let store = c.store().path;
    let away = c.tmp.path().canonicalize().unwrap().join("away");
    fs::create_dir_all(away.join("project")).unwrap();
    fs::write(away.join("linked.jsonl"), user("linked", "/w/l", &ts(1))).unwrap();
    fs::write(
        away.join("project/p.jsonl"),
        user("in project", "/w/p", &ts(2)),
    )
    .unwrap();
    let linked = store.join("-w-a/linked.jsonl");
    let project = store.join("-w-linked");
    symlink(away.join("linked.jsonl"), &linked).unwrap();
    symlink(away.join("project"), &project).unwrap();
    c.write("a", &user("a", "/w/a", &ts(3)));
    let mut index = Index::default();
    let s = c.refresh(&mut index);
    assert_eq!((s.files, s.cold), (3, 3));
    let in_project = entry(&index, &project.join("p.jsonl"));

    chmod(&away, 0o000);
    let s = c.refresh(&mut index);
    chmod(&away, 0o755);
    assert_eq!(
        (s.files, s.removed, s.reused, s.kept()),
        (2, 1, 1, 1),
        "{:?}",
        s.unreadable
    );
    assert!(!index.entries.contains_key(&linked));
    assert_eq!(entry(&index, &project.join("p.jsonl")), in_project);
    let unreadable: Vec<(&Path, usize)> = s
        .unreadable
        .iter()
        .map(|u| (u.path.as_path(), u.kept))
        .collect();
    assert_eq!(unreadable, [(project.as_path(), 1)]);

    // In reach again: the transcript is indexed again, the project's is not read again.
    let s = c.refresh(&mut index);
    assert_eq!((s.files, s.cold, s.reused), (3, 1, 2));
    assert!(s.unreadable.is_empty());
    assert_eq!(
        entry(&index, &linked).first_user_text.as_deref(),
        Some("linked")
    );
}

/// R8 (review #10): a store whose path cannot be resolved is not a store that is gone. Here
/// `projects` is a link, and a directory on the way to its target cannot be searched: the
/// store is in no list, and its real path, which the cached entries are below, is not known.
/// Its entries stay, it is reported as the home gives it, and nothing is read again once it
/// resolves; a store that could be listed is refreshed as usual meanwhile. A home that is
/// gone has no store.
#[test]
fn a_store_that_cannot_be_resolved_keeps_its_entries_and_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (work, solo, away) = (root.join("p/work"), root.join("p/solo"), root.join("away"));
    let real = away.join("deep/projects");
    fs::create_dir_all(real.join("-w")).unwrap();
    fs::create_dir_all(solo.join("projects/-s")).unwrap();
    fs::create_dir_all(&work).unwrap();
    symlink(&real, work.join("projects")).unwrap();
    for (file, text) in [
        (real.join("-w/a.jsonl"), "a"),
        (real.join("-w/b.jsonl"), "b"),
        (solo.join("projects/-s/s.jsonl"), "s"),
        (solo.join("projects/-s/gone.jsonl"), "gone"),
    ] {
        fs::write(file, user(text, "/w", &ts(1))).unwrap();
    }
    let account = |name: &str, home: &Path| Account {
        provider: Provider::Claude,
        name: name.into(),
        home: Home::Path(home.display().to_string()),
    };
    let accounts = [account("work", &work), account("solo", &solo)];
    let env = [("HOME".to_string(), root.join("home").display().to_string())]
        .into_iter()
        .collect();
    let refresh = |index: &mut Index| {
        let (stores, given) = index::resolve(&accounts, &env);
        let s = index::refresh_with(index, &stores, &given, |_| {});
        let unresolved: Vec<Given> = given.into_iter().filter(|g| g.real.is_err()).collect();
        (stores, unresolved, s)
    };
    let mut index = Index::default();
    let (stores, unresolved, s) = refresh(&mut index);
    assert_eq!((stores.len(), unresolved.len(), s.files), (2, 0, 4));
    assert_eq!(stores[0].path, real);
    let of_work = |index: &Index| -> Vec<Entry> {
        let below = |e: &&Entry| e.store == real;
        index.entries.values().filter(below).cloned().collect()
    };
    let before = of_work(&index);
    assert_eq!(before.len(), 2);

    chmod(&away, 0o000);
    fs::remove_file(solo.join("projects/-s/gone.jsonl")).unwrap();
    let (stores, unresolved, s) = refresh(&mut index);
    let listed = index::stores(&accounts, &env);
    chmod(&away, 0o755);
    assert_eq!(stores, listed, "the list of stores is the same either way");
    assert_eq!(stores.len(), 1);
    let [
        Given {
            provider,
            path,
            account,
            real: Err(error),
        },
    ] = unresolved.as_slice()
    else {
        panic!("{unresolved:?}")
    };
    assert_eq!(
        (*provider, path.as_path(), account.as_str()),
        (
            Provider::Claude,
            work.join("projects").as_path(),
            "claude:work"
        )
    );
    assert!(!error.is_empty());
    assert_eq!(
        (s.files, s.removed, s.reused, s.cold, s.kept()),
        (3, 1, 1, 0, 2),
        "{:?}",
        s.unreadable
    );
    assert_eq!(s.unreadable.len(), 1);
    let u = &s.unreadable[0];
    assert_eq!(
        (u.path.as_path(), u.kept, u.unresolved, &u.error),
        (work.join("projects").as_path(), 2, true, error)
    );
    assert_eq!(of_work(&index), before);

    let (_, unresolved, s) = refresh(&mut index);
    assert!(unresolved.is_empty() && s.unreadable.is_empty());
    assert_eq!(
        (s.reused, s.cold, s.incremental, s.bytes_read),
        (3, 0, 0, 0),
        "resolved again: nothing is read again"
    );

    // The link's target is gone: no store there, and nothing that cannot be told.
    fs::remove_dir_all(&away).unwrap();
    let (stores, unresolved, s) = refresh(&mut index);
    assert_eq!((stores.len(), unresolved.len()), (1, 0));
    assert_eq!((s.files, s.removed), (1, 2));
    assert!(s.unreadable.is_empty());
}

/// Two claude homes `a` and `b` below `root`, each with its own store and one transcript, and
/// what refreshes an index with the stores of `accounts` as they resolve now.
struct Homes {
    root: PathBuf,
    env: remuda::Env,
    _tmp: TempDir,
}

impl Homes {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let homes = Homes {
            env: [("HOME".to_string(), root.join("home").display().to_string())]
                .into_iter()
                .collect(),
            root,
            _tmp: tmp,
        };
        homes.transcript("a");
        homes.transcript("b");
        homes
    }

    fn home(&self, name: &str) -> PathBuf {
        self.root.join("p").join(name)
    }

    /// Writes `<home>/projects/-w/<name>.jsonl`; returns it.
    fn transcript(&self, name: &str) -> PathBuf {
        let file = self.home(name).join(format!("projects/-w/{name}.jsonl"));
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, user(name, "/w", &ts(1))).unwrap();
        file
    }

    fn account(&self, name: &str) -> Account {
        Account {
            provider: Provider::Claude,
            name: name.into(),
            home: Home::Path(self.home(name).display().to_string()),
        }
    }

    /// The account `name` with the home of `home`.
    fn account_at(&self, name: &str, home: &str) -> Account {
        Account {
            name: name.into(),
            ..self.account(home)
        }
    }

    fn refresh(&self, index: &mut Index, names: &[&str]) -> RefreshStats {
        let accounts: Vec<Account> = names.iter().map(|n| self.account(n)).collect();
        self.refresh_accounts(index, &accounts)
    }

    fn refresh_accounts(&self, index: &mut Index, accounts: &[Account]) -> RefreshStats {
        let (stores, given) = index::resolve(accounts, &self.env);
        index::refresh_with(index, &stores, &given, |_| {})
    }
}

/// R8 (review #10): a store that cannot be resolved keeps the entries of the store it last
/// resolved to, and those alone. Another store of the same provider whose directory is gone,
/// or whose account left the registry, drops out as it always did: what is not known of one
/// store says nothing for another.
#[test]
fn a_store_that_cannot_be_resolved_does_not_keep_the_entries_of_another() {
    for removed_from_the_registry in [false, true] {
        let h = Homes::new();
        let (a, b) = (h.home("a"), h.home("b"));
        let mut index = Index::default();
        assert_eq!(h.refresh(&mut index, &["a", "b"]).files, 2);

        let still = if removed_from_the_registry {
            vec!["b"]
        } else {
            fs::remove_dir_all(a.join("projects")).unwrap();
            vec!["a", "b"]
        };
        chmod(&b, 0o000);
        let s = h.refresh(&mut index, &still);
        chmod(&b, 0o755);
        assert_eq!(
            (s.files, s.removed, s.kept()),
            (1, 1, 1),
            "{:?}",
            s.unreadable
        );
        let kept: Vec<&PathBuf> = index.entries.keys().collect();
        assert_eq!(kept, [&b.join("projects/-w/b.jsonl")]);
        assert_eq!(s.unreadable.len(), 1);
        assert_eq!(s.unreadable[0].path, b.join("projects"));

        let s = h.refresh(&mut index, &still);
        assert_eq!((s.files, s.reused, s.removed, s.bytes_read), (1, 1, 0, 0));
    }
}

/// R8: while one store cannot be resolved, stores that come and go leave nothing behind: the
/// index does not grow with what is no longer there.
#[test]
fn stores_that_come_and_go_leave_nothing_while_another_cannot_be_resolved() {
    let h = Homes::new();
    let b = h.home("b");
    let mut index = Index::default();
    h.refresh(&mut index, &["b"]);
    chmod(&b, 0o000);
    let mut sizes = Vec::new();
    for n in 1..=4 {
        // The account `c<n>` is registered, indexed, and removed with its home.
        let name = format!("c{n}");
        h.transcript(&name);
        let s = h.refresh(&mut index, &["b", &name]);
        assert_eq!((s.files, s.cold, s.kept()), (2, 1, 1));
        fs::remove_dir_all(h.home(&name)).unwrap();
        let s = h.refresh(&mut index, &["b"]);
        sizes.push((s.files, s.removed, s.kept()));
    }
    chmod(&b, 0o755);
    assert_eq!(sizes, [(1, 1, 1); 4]);
    assert_eq!(index.stores.len(), 1, "{:?}", index.stores);
}

/// R8: which entries a store that cannot be resolved keeps is told by the real path the index
/// remembers for it, not by where that path is. Here the home cannot be searched while its
/// `projects` link leads to a directory that can be reached and that no other account lists:
/// the entries stay. An index that remembers nothing of the store (one written before this
/// was kept) has nothing to tell them by: they drop out, and the store is still reported.
#[test]
fn the_entries_kept_are_those_of_the_real_path_the_index_remembers() {
    let h = Homes::new();
    let (a, b) = (h.home("a"), h.home("b"));
    let elsewhere = h.root.join("elsewhere/store");
    fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
    fs::rename(b.join("projects"), &elsewhere).unwrap();
    symlink(&elsewhere, b.join("projects")).unwrap();
    let mut index = Index::default();
    assert_eq!(h.refresh(&mut index, &["a", "b"]).files, 2);
    assert_eq!(
        index.stores.get(&b.join("projects")),
        Some(&elsewhere),
        "{:?}",
        index.stores
    );
    let remembered = index.clone();

    chmod(&b, 0o000);
    let s = h.refresh(&mut index, &["a", "b"]);
    let mut forgetful = forgetful_of(&remembered);
    let forgot = h.refresh(&mut forgetful, &["a", "b"]);
    chmod(&b, 0o755);
    assert_eq!((s.files, s.removed, s.reused, s.kept()), (2, 0, 1, 1));
    assert_eq!(index, remembered);
    assert_eq!(s.unreadable[0].path, b.join("projects"));
    assert!(!s.remembered, "what is remembered did not change");

    assert_eq!(
        (forgot.files, forgot.removed, forgot.kept()),
        (1, 1, 0),
        "nothing remembered: nothing to keep"
    );
    assert_eq!(forgot.unreadable.len(), 1);
    let left: Vec<&PathBuf> = forgetful.entries.keys().collect();
    assert_eq!(left, [&a.join("projects/-w/a.jsonl")]);

    // The cache keeps what is remembered: saved and loaded, it tells the same.
    let saved = h.root.join("state/index.json");
    remembered.save(&saved).unwrap();
    assert_eq!(Index::load(&saved), remembered);
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&saved).unwrap()).unwrap();
    assert_eq!(v["schema_version"], SCHEMA_VERSION);
    assert_eq!(v["stores"].as_object().map(|m| m.len()), Some(2));
    // And one written before, without it, is read all the same.
    let mut before = v.clone();
    before.as_object_mut().unwrap().remove("stores");
    fs::write(&saved, before.to_string()).unwrap();
    assert_eq!(Index::load(&saved), forgetful_of(&remembered));
}

/// `index` remembering nothing of its stores.
fn forgetful_of(index: &Index) -> Index {
    Index {
        stores: Default::default(),
        ..index.clone()
    }
}

/// R8 (review #10): what the index remembers of a store goes by the directory its home gives,
/// not by the account's name. An account removed and registered again under the same name
/// with another home, whose store cannot be resolved before it was ever indexed, does not
/// take over the entries of the old home's store, whether that home is still there or gone:
/// they drop out, also from the cache saved and read back, and the new store is reported.
#[test]
fn an_account_given_another_home_does_not_take_over_the_entries_of_its_name() {
    for old_home_deleted in [false, true] {
        let h = Homes::new();
        let (old, new) = (h.home("a"), h.home("b"));
        let saved = h.root.join("state/index.json");
        let mut index = Index::default();
        let work_at = |home: &str| [h.account_at("work", home)];
        assert_eq!(h.refresh_accounts(&mut index, &work_at("a")).files, 1);
        assert_eq!(
            index.stores.get(&old.join("projects")),
            Some(&old.join("projects"))
        );
        index.save(&saved).unwrap();

        // `work` is now registered with the home `b`, which cannot be searched.
        if old_home_deleted {
            fs::remove_dir_all(&old).unwrap();
        }
        chmod(&new, 0o000);
        let mut sizes = Vec::new();
        for _ in 0..2 {
            let mut index = Index::load(&saved);
            let s = h.refresh_accounts(&mut index, &work_at("b"));
            index.save(&saved).unwrap();
            assert_eq!(s.unreadable.len(), 1, "{:?}", s.unreadable);
            assert_eq!(s.unreadable[0].path, new.join("projects"));
            assert!(index.stores.is_empty(), "{:?}", index.stores);
            sizes.push((s.files, s.removed, s.kept()));
        }
        chmod(&new, 0o755);
        assert_eq!(sizes, [(0, 1, 0), (0, 0, 0)], "deleted: {old_home_deleted}");
        assert!(Index::load(&saved).entries.is_empty());

        // In reach: the new home's own transcript is indexed.
        let mut index = Index::load(&saved);
        let s = h.refresh_accounts(&mut index, &work_at("b"));
        assert_eq!((s.files, s.cold), (1, 1));
        let indexed: Vec<&PathBuf> = index.entries.keys().collect();
        assert_eq!(indexed, [&new.join("projects/-w/b.jsonl")]);
    }
}

/// R8 (review #10): and the other way round: an account registered again under another name
/// with the same home is the same store. When it cannot be resolved before the next refresh,
/// its entries stay, also in the cache saved and read back.
#[test]
fn an_account_given_another_name_keeps_the_entries_of_its_home() {
    let h = Homes::new();
    let home = h.home("a");
    let saved = h.root.join("state/index.json");
    let mut index = Index::default();
    h.refresh_accounts(&mut index, &[h.account_at("work", "a")]);
    index.save(&saved).unwrap();
    let before = Index::load(&saved);

    chmod(&home, 0o000);
    let mut kept = Vec::new();
    for _ in 0..2 {
        let mut index = Index::load(&saved);
        let s = h.refresh_accounts(&mut index, &[h.account_at("renamed", "a")]);
        index.save(&saved).unwrap();
        kept.push((s.files, s.removed, s.kept(), s.remembered));
    }
    chmod(&home, 0o755);
    assert_eq!(kept, [(1, 0, 1, false); 2]);
    assert_eq!(Index::load(&saved), before);
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
