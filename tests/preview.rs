//! R8: preview — the last N user / assistant messages, read from the tail.

mod common;

use std::fs;
use std::path::PathBuf;

use common::transcripts::*;
use remuda::transcript::{Message, Role, preview};
use serde_json::json;
use tempfile::TempDir;

fn write(contents: &str) -> (TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("s.jsonl");
    fs::write(&path, contents).unwrap();
    (tmp, path)
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

#[test]
fn messages_keep_text_and_tool_names_and_skip_the_rest() {
    let (_t, path) = write(
        &[
            meta_user(
                "<local-command-caveat>x</local-command-caveat>",
                "/w",
                &ts(0),
            ),
            user("fix the bug", "/w", &ts(1)),
            ai_title("Fixing"),
            thinking("m1", "secret reasoning", "/w", &ts(2)),
            assistant_text("m1", "Let me look.", "/w", &ts(2)),
            tool_use("m1", "Bash", "/w", &ts(2)),
            tool_result("huge tool output body", "/w", &ts(3)),
            filler(100, "/w", &ts(3)),
            tool_use("m2", "Edit", "/w", &ts(4)),
            tool_result("ok", "/w", &ts(4)),
            assistant_text("m3", "Done.", "/w", &ts(5)),
            user_blocks(
                json!([{"type": "text", "text": "thanks"}, {"type": "image", "source": {}},
                       {"type": "text", "text": "again"}]),
                "/w",
                &ts(6),
            ),
        ]
        .concat(),
    );
    assert_eq!(
        preview(&path, 10).unwrap(),
        [
            u("fix the bug"),
            a("Let me look.\n[tool: Bash]"),
            a("[tool: Edit]"),
            a("Done."),
            u("thanks\nagain"),
        ]
    );
}

#[test]
fn local_commands_are_not_messages() {
    let (_t, path) = write(
        &[
            meta_user(
                "<local-command-caveat>x</local-command-caveat>",
                "/w",
                &ts(0),
            ),
            user(
                "<command-name>/clear</command-name>\n<command-message>clear</command-message>",
                "/w",
                &ts(0),
            ),
            user(
                "<local-command-stdout></local-command-stdout>",
                "/w",
                &ts(0),
            ),
            user(
                "<command-message>review</command-message>\n<command-name>/review</command-name>",
                "/w",
                &ts(1),
            ),
            user(
                "<local-command-stderr>Error</local-command-stderr>",
                "/w",
                &ts(1),
            ),
            user("fix the bug", "/w", &ts(2)),
            assistant_text("m1", "Done.", "/w", &ts(3)),
        ]
        .concat(),
    );
    assert_eq!(preview(&path, 10).unwrap(), [u("fix the bug"), a("Done.")]);
}

#[test]
fn only_the_last_n_messages() {
    let lines: String = (0..10)
        .map(|i| {
            if i % 2 == 0 {
                user(&format!("q{i}"), "/w", &ts(i))
            } else {
                assistant_text(&format!("m{i}"), &format!("a{i}"), "/w", &ts(i))
            }
        })
        .collect();
    let (_t, path) = write(&lines);
    assert_eq!(preview(&path, 3).unwrap(), [a("a7"), u("q8"), a("a9")]);
    assert_eq!(preview(&path, 0).unwrap(), []);
    assert_eq!(preview(&path, 100).unwrap().len(), 10);
}

#[test]
fn huge_lines_make_the_window_grow() {
    let big = "b".repeat(300_000);
    let (_t, path) = write(
        &[
            user("before", "/w", &ts(0)),
            user(&big, "/w", &ts(1)),
            tool_result(&"r".repeat(1_500_000), "/w", &ts(2)),
            assistant_text("m1", "short answer", "/w", &ts(3)),
        ]
        .concat(),
    );
    let got = preview(&path, 2).unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].role, Role::User);
    assert_eq!(got[0].text, big);
    assert_eq!(got[1], a("short answer"));
    assert_eq!(preview(&path, 5).unwrap()[0], u("before"));
}

#[test]
fn window_growth_is_capped() {
    let (_t, path) = write(
        &[
            user("unreachable", "/w", &ts(0)),
            tool_result(&"r".repeat(5_000_000), "/w", &ts(1)),
            assistant_text("m1", "last", "/w", &ts(2)),
        ]
        .concat(),
    );
    assert_eq!(preview(&path, 3).unwrap(), [a("last")]);
}

#[test]
fn unfinished_and_malformed_lines_are_skipped() {
    let partial = assistant_text("m9", "still streaming", "/w", &ts(9));
    let (_t, path) = write(&format!(
        "{}not json\n{{\"type\":\"assistant\",\"message\":7}}\n{}{}",
        user("q", "/w", &ts(1)),
        assistant_text("m1", "a", "/w", &ts(2)),
        &partial[..partial.len() - 3]
    ));
    assert_eq!(preview(&path, 5).unwrap(), [u("q"), a("a")]);
}

#[test]
fn empty_and_missing_files() {
    let (_t, path) = write("");
    assert_eq!(preview(&path, 5).unwrap(), []);
    assert!(preview(&path.with_file_name("missing.jsonl"), 5).is_err());
}
