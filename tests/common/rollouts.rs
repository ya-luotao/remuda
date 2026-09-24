//! Synthetic codex rollout records (R15, R17): the shapes of codex 0.155.1's
//! `sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`, made-up content.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::transcripts::line;

/// `2026-09-20T10:<minute>:00.000Z`.
pub fn ts(minute: u32) -> String {
    format!("2026-09-20T10:{minute:02}:00.000Z")
}

fn record(kind: &str, ts: &str, payload: Value) -> String {
    line(json!({"timestamp": ts, "type": kind, "payload": payload}))
}

/// The first record: `session_meta`, with `base_instructions` padded to `pad` bytes (real ones
/// are ~20 KB, which pushes the first user text past a 64 KB head window).
pub fn meta(id: &str, cwd: &str, source: Value, pad: usize, ts: &str) -> String {
    record(
        "session_meta",
        ts,
        json!({"session_id": id, "id": id, "timestamp": ts, "cwd": cwd,
               "originator": "codex-tui", "cli_version": "0.155.1", "source": source,
               "model_provider": "openai",
               "base_instructions": {"text": "i".repeat(pad)}}),
    )
}

/// A fork parent's `session_meta`, written after the session's own (seen in subagent rollouts).
pub fn parent_meta(parent: &str, cwd: &str, ts: &str) -> String {
    record(
        "session_meta",
        ts,
        json!({"session_id": parent, "id": parent, "timestamp": ts, "cwd": cwd,
               "originator": "codex-tui", "source": "cli"}),
    )
}

pub fn turn_context(cwd: &str, ts: &str) -> String {
    record(
        "turn_context",
        ts,
        json!({"turn_id": "t1", "cwd": cwd, "approval_policy": "on-request"}),
    )
}

fn message(role: &str, blocks: Value, ts: &str) -> String {
    record(
        "response_item",
        ts,
        json!({"type": "message", "id": "msg_1", "role": role, "content": blocks}),
    )
}

/// A user message with one `input_text` block per text.
pub fn user(texts: &[&str], ts: &str) -> String {
    let blocks: Vec<Value> = texts
        .iter()
        .map(|t| json!({"type": "input_text", "text": t}))
        .collect();
    message("user", Value::Array(blocks), ts)
}

pub fn developer(text: &str, ts: &str) -> String {
    message(
        "developer",
        json!([{"type": "input_text", "text": text}]),
        ts,
    )
}

pub fn assistant(text: &str, ts: &str) -> String {
    message(
        "assistant",
        json!([{"type": "output_text", "text": text}]),
        ts,
    )
}

pub fn function_call(name: &str, ts: &str) -> String {
    record(
        "response_item",
        ts,
        json!({"type": "function_call", "name": name, "arguments": "{}", "call_id": "c1"}),
    )
}

pub fn custom_tool_call(name: &str, ts: &str) -> String {
    record(
        "response_item",
        ts,
        json!({"type": "custom_tool_call", "status": "completed", "call_id": "c2",
               "name": name, "input": "x"}),
    )
}

pub fn tool_output(body: &str, ts: &str) -> String {
    record(
        "response_item",
        ts,
        json!({"type": "function_call_output", "call_id": "c1", "output": body}),
    )
}

pub fn reasoning(text: &str, ts: &str) -> String {
    record(
        "response_item",
        ts,
        json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": text}],
               "encrypted_content": "gAAAA"}),
    )
}

/// An `event_msg` (these duplicate response items; the index ignores them).
pub fn event(kind: &str, text: &str, ts: &str) -> String {
    record("event_msg", ts, json!({"type": kind, "message": text}))
}

/// The injected blocks codex puts before the first real user text.
pub const ENVIRONMENT: &str =
    "<environment_context>\n  <cwd>/w</cwd>\n  <shell>zsh</shell>\n</environment_context>";
pub const AGENTS_MD: &str =
    "# AGENTS.md instructions for /w\n\n<INSTRUCTIONS>\nbe nice\n</INSTRUCTIONS>";

/// The 2025 format: a first line without `type`, then bare records.
pub fn old_first(id: &str, ts: &str) -> String {
    line(json!({"id": id, "timestamp": ts, "instructions": null}))
}

pub fn old_user(text: &str) -> String {
    line(json!({"type": "message", "id": null, "role": "user",
                "content": [{"type": "input_text", "text": text}]}))
}

pub fn old_state() -> String {
    line(json!({"record_type": "state"}))
}

/// A `token_count` event: the rate limits codex recorded after a model turn (R10).
pub fn token_count(rate_limits: Value, ts: &str) -> String {
    record(
        "event_msg",
        ts,
        json!({"type": "token_count", "info": null, "rate_limits": rate_limits}),
    )
}

/// One line of `<home>/session_index.jsonl`.
pub fn thread_name(id: &str, name: &str) -> String {
    line(json!({"id": id, "thread_name": name, "updated_at": "2026-09-20T10:00:00Z"}))
}

/// `<home>/sessions/2026/09/20/rollout-2026-09-20T10-00-00-<id>.jsonl`, written; returns it.
pub fn write_rollout(home: &Path, id: &str, contents: &str) -> PathBuf {
    write_rollout_in(&home.join("sessions/2026/09/20"), id, contents)
}

/// The same rollout, archived: `<home>/archived_sessions/rollout-…-<id>.jsonl`.
pub fn write_archived_rollout(home: &Path, id: &str, contents: &str) -> PathBuf {
    write_rollout_in(&home.join("archived_sessions"), id, contents)
}

fn write_rollout_in(dir: &Path, id: &str, contents: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("rollout-2026-09-20T10-00-00-{id}.jsonl"));
    fs::write(&path, contents).unwrap();
    path
}
