//! Reading codex rollouts (SPEC R17): `<home>/sessions/**/rollout-<time>-<id>.jsonl`, append-only
//! JSONL like claude's transcripts, and the thread names in `<home>/session_index.jsonl`.
//!
//! Record shapes (codex 0.155.1, checked against 1441 real rollouts):
//! `{"timestamp", "type": "session_meta" | "turn_context" | "response_item" | "event_msg" | …,
//! "payload": {…}}`. The first record is the session's own `session_meta` (a subagent's
//! rollout has its fork parent's after it). Rollouts from 2025 start with a bare
//! `{"id", "timestamp", "instructions"}` line and hold bare `{"type": "message", …}` records.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::path::Path;

use jiff::Timestamp;
use serde::Deserialize;
use serde_json::Value;

use crate::transcript::{
    FIRST_USER_TEXT_CAP, Head, Message, PREVIEW_CAP, Role, Tail, WINDOW, complete_lines, one_line,
    read_at,
};
use crate::{index, usage};

/// At most this many rollouts, newest first, are read for the cached rate limits (R10).
pub const RATE_LIMIT_FILES: usize = 16;
/// Each rollout is read from its end for the rate limits, in a window growing up to this size.
pub const RATE_LIMIT_TAIL_CAP: u64 = 1024 * 1024;

/// The fields of a record the index and the preview look at; the rest is skipped.
#[derive(Deserialize)]
struct Record {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    payload: Option<Payload>,
    // 2025 format: the first line's id, and bare message / tool records.
    id: Option<String>,
    role: Option<String>,
    content: Option<Value>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct Payload {
    #[serde(rename = "type")]
    kind: Option<String>,
    id: Option<String>,
    cwd: Option<String>,
    originator: Option<String>,
    source: Option<Value>,
    role: Option<String>,
    content: Option<Value>,
    name: Option<String>,
}

fn record(line: &[u8]) -> Option<Record> {
    serde_json::from_slice(line).ok()
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.is_empty())
}

/// What a record is, for the index and the preview.
enum Item {
    /// The session's metadata (a `session_meta`, or a 2025 first line).
    Meta {
        id: Option<String>,
        cwd: Option<String>,
        source: Option<String>,
        originator: Option<String>,
    },
    TurnContext {
        cwd: Option<String>,
    },
    /// A message: its role and its content blocks.
    Message {
        role: String,
        content: Value,
    },
    /// A tool call (`function_call`, `custom_tool_call`, …), by name.
    ToolCall(String),
    Other,
}

fn item(r: Record) -> Item {
    let (kind, payload) = match (r.kind.as_deref(), r.payload) {
        (Some("session_meta"), Some(p)) => {
            return Item::Meta {
                id: non_empty(p.id),
                cwd: non_empty(p.cwd),
                source: p.source.as_ref().and_then(source_name),
                originator: non_empty(p.originator),
            };
        }
        (Some("turn_context"), Some(p)) => {
            return Item::TurnContext {
                cwd: non_empty(p.cwd),
            };
        }
        (Some("response_item"), Some(p)) => (p.kind.clone(), p),
        // 2025 format.
        (None, None) if r.id.is_some() => {
            return Item::Meta {
                id: non_empty(r.id),
                cwd: None,
                source: None,
                originator: None,
            };
        }
        (Some(_), None) => (
            r.kind.clone(),
            Payload {
                kind: r.kind,
                id: None,
                cwd: None,
                originator: None,
                source: None,
                role: r.role,
                content: r.content,
                name: r.name,
            },
        ),
        _ => return Item::Other,
    };
    match kind.as_deref() {
        Some("message") => match (payload.role, payload.content) {
            (Some(role), Some(content)) => Item::Message { role, content },
            _ => Item::Other,
        },
        Some(
            k @ ("function_call"
            | "custom_tool_call"
            | "local_shell_call"
            | "web_search_call"
            | "image_generation_call"
            | "tool_search_call"),
        ) => Item::ToolCall(
            non_empty(payload.name).unwrap_or_else(|| k.trim_end_matches("_call").to_string()),
        ),
        _ => Item::Other,
    }
}

/// `session_meta.source`: `cli`, `vscode`, `exec`, … as is; an object such as
/// `{"subagent": {…}}` by its key.
fn source_name(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Object(map) => map.keys().next().cloned(),
        _ => None,
    }
}

/// What codex writes into the user's turn besides the user's own words: tagged blocks such as
/// `<environment_context>`, `<user_instructions>`, `<user_action>`, `<image>`, and the
/// `# AGENTS.md instructions` block. A VS Code message wraps the request in
/// `# Context from my IDE setup:` … `## My request for Codex:`; the request is the text.
fn real_text(block: &str) -> Option<&str> {
    let text = block.trim();
    if let Some(ide) = text.strip_prefix("# Context from my IDE setup") {
        return ide
            .split_once("## My request for Codex:")
            .map(|(_, request)| request.trim())
            .filter(|r| !r.is_empty());
    }
    let tagged = text.strip_prefix('<').is_some_and(|rest| {
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        rest.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
    });
    let injected = tagged || text.starts_with("# AGENTS.md instructions");
    (!text.is_empty() && !injected).then_some(text)
}

/// The user's own text in a user message: its real `input_text` blocks, joined.
fn user_text(content: &Value) -> Option<String> {
    let texts: Vec<&str> = content
        .as_array()?
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("input_text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .filter_map(real_text)
        .collect();
    (!texts.is_empty()).then(|| texts.join("\n"))
}

/// The `output_text` of an assistant message.
fn assistant_text(content: &Value) -> Option<String> {
    let texts: Vec<&str> = content
        .as_array()?
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("output_text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .filter(|t| !t.trim().is_empty())
        .collect();
    (!texts.is_empty()).then(|| texts.join("\n"))
}

/// Head fields of a rollout, fed lines in file order: the id, cwd, source and originator of
/// the **first** record only (a later `session_meta` is a fork parent's), the first
/// timestamp, and the first real user text.
pub(crate) fn feed_head(head: &mut Head, lines: &[&[u8]]) {
    for line in lines {
        if head.complete() {
            return;
        }
        let Some(r) = record(line) else { continue };
        head.ts_first = head.ts_first.take().or(non_empty(r.timestamp.clone()));
        let first = !head.started;
        head.started = true;
        match item(r) {
            Item::Meta {
                id,
                cwd,
                source,
                originator,
            } if first => {
                head.session_id = id;
                head.cwd_first = cwd;
                head.source = source;
                head.originator = originator;
            }
            Item::Message { role, content } if role == "user" && head.first_user_text.is_none() => {
                head.first_user_text =
                    user_text(&content).map(|t| one_line(&t, FIRST_USER_TEXT_CAP));
            }
            _ => {}
        }
    }
}

/// Whether the head has everything a rollout's head gives.
pub(crate) fn head_complete(head: &Head) -> bool {
    head.started && head.ts_first.is_some() && head.first_user_text.is_some()
}

/// Tail fields, fed lines in file order and read backwards: the last timestamp and the last
/// `turn_context` cwd (codex has no title records: titles are thread names).
pub(crate) fn feed_tail(tail: &mut Tail, lines: &[&[u8]]) {
    for line in lines.iter().rev() {
        if tail.cwd_last.is_some() && tail.ts_last.is_some() {
            return;
        }
        let Some(r) = record(line) else { continue };
        tail.ts_last = tail.ts_last.take().or(non_empty(r.timestamp.clone()));
        if tail.cwd_last.is_none()
            && let Item::TurnContext { cwd } = item(r)
        {
            tail.cwd_last = cwd;
        }
    }
}

/// The session id in a rollout's file name, `rollout-<time>-<id>.jsonl` (the id is the last 36
/// characters of the stem); `None` for any other name.
pub fn rollout_id(file_name: &str) -> Option<&str> {
    let stem = file_name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    stem.get(stem.len().checked_sub(36)?..)
}

/// Thread names from the `session_index.jsonl` files of the homes sharing one store (R17):
/// within a file the last `thread_name` per id wins (codex appends a line per rename). Across
/// files, the name whose line has the newest `updated_at` wins, whichever home wrote it: a
/// rename in any home is the latest name. A line without a readable `updated_at` is older than
/// any with one; on a tie the later file (registry order) wins. Missing or unreadable files
/// have none; bad lines are skipped.
pub fn thread_names(paths: &[impl AsRef<Path>]) -> HashMap<String, String> {
    let mut named: HashMap<String, (Option<Timestamp>, String)> = HashMap::new();
    for path in paths {
        for (id, (at, name)) in thread_names_of(path.as_ref()) {
            match named.get(&id) {
                Some((newest, _)) if *newest > at => {}
                _ => {
                    named.insert(id, (at, name));
                }
            }
        }
    }
    named
        .into_iter()
        .map(|(id, (_, name))| (id, name))
        .collect()
}

/// One `session_index.jsonl`: the last line per id, with its `updated_at`.
fn thread_names_of(path: &Path) -> HashMap<String, (Option<Timestamp>, String)> {
    #[derive(Deserialize)]
    struct Line {
        id: Option<String>,
        thread_name: Option<String>,
        updated_at: Option<String>,
    }
    let mut names = HashMap::new();
    let Ok(bytes) = fs::read(path) else {
        return names;
    };
    for line in complete_lines(&bytes, false).lines {
        if let Ok(Line {
            id: Some(id),
            thread_name: Some(name),
            updated_at,
        }) = serde_json::from_slice(line)
            && !name.trim().is_empty()
        {
            let at = updated_at.and_then(|t| t.parse::<Timestamp>().ok());
            names.insert(id, (at, name));
        }
    }
    names
}

/// The last `n` user / assistant messages of a rollout, oldest first (R17): user text without
/// codex's injections, assistant text, tool calls as `[tool: <name>]`; reasoning, tool output
/// and developer text are skipped. Everything the assistant does between two user messages is
/// one message. Reads a growing tail window like [`crate::transcript::preview`].
pub fn preview(path: &Path, n: usize) -> io::Result<Vec<Message>> {
    let file = File::open(path)?;
    let size = file.metadata()?.len();
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut window = WINDOW;
    loop {
        let start = size.saturating_sub(window);
        let read_from = start.saturating_sub(1);
        let buf = read_at(&file, read_from, size - read_from)?;
        let lines = complete_lines(&buf, start > 0);
        let mut messages = messages(&lines.lines);
        if messages.len() > n || start == 0 || window >= PREVIEW_CAP {
            let skip = messages.len().saturating_sub(n);
            messages.drain(..skip);
            return Ok(messages);
        }
        window *= 4;
    }
}

fn messages(lines: &[&[u8]]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    let assistant = |out: &mut Vec<Message>, text: String| match out.last_mut() {
        Some(last) if last.role == Role::Assistant => {
            last.text.push('\n');
            last.text.push_str(&text);
        }
        _ => out.push(Message {
            role: Role::Assistant,
            text,
        }),
    };
    for line in lines {
        let Some(r) = record(line) else { continue };
        match item(r) {
            Item::Message { role, content } => match role.as_str() {
                "user" => {
                    if let Some(text) = user_text(&content) {
                        out.push(Message {
                            role: Role::User,
                            text,
                        });
                    }
                }
                "assistant" => {
                    if let Some(text) = assistant_text(&content) {
                        assistant(&mut out, text);
                    }
                }
                _ => {}
            },
            Item::ToolCall(name) => assistant(&mut out, format!("[tool: {name}]")),
            _ => {}
        }
    }
    out
}

/// The newest general rate limits codex recorded in `home`'s rollouts (R10): the `rate_limits`
/// of a `token_count` event whose `limit_id` is `codex` or null (2025) and which has a usable
/// window, from `sessions/**` and `archived_sessions/`, newest by the record's `timestamp`.
/// Bounded: at most [`RATE_LIMIT_FILES`] rollouts, newest mtime first, stopping at the first one
/// last written before the best record found (none of its records can be newer). `None` when
/// there is none.
pub fn cached_rate_limits(home: &Path) -> Option<(Timestamp, Value)> {
    let mut files = Vec::new();
    index::list_rollouts(&home.join("sessions"), &mut files);
    index::list_rollouts(&home.join("archived_sessions"), &mut files);
    files.sort_by_key(|(_, _, stat)| std::cmp::Reverse(stat.mtime_ns));
    let mut best: Option<(Timestamp, Value)> = None;
    for (path, _, stat) in files.into_iter().take(RATE_LIMIT_FILES) {
        if let Some((at, _)) = &best
            && stat.mtime_ns < at.as_nanosecond()
        {
            break;
        }
        if let Some(found) = last_rate_limits(&path)
            && best.as_ref().is_none_or(|(at, _)| found.0 > *at)
        {
            best = Some(found);
        }
    }
    best
}

/// The last general rate limits in one rollout, read from its end in a window that grows ×4
/// from 64 KB up to [`RATE_LIMIT_TAIL_CAP`] (like [`preview`]).
fn last_rate_limits(path: &Path) -> Option<(Timestamp, Value)> {
    const NEEDLE: &[u8] = b"\"token_count\"";
    let file = File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let mut window = WINDOW;
    loop {
        let start = size.saturating_sub(window);
        let read_from = start.saturating_sub(1);
        let buf = read_at(&file, read_from, size - read_from).ok()?;
        let found = complete_lines(&buf, start > 0)
            .lines
            .iter()
            .rev()
            .filter(|line| line.windows(NEEDLE.len()).any(|w| w == NEEDLE))
            .find_map(|line| rate_limits_of(line));
        if found.is_some() || start == 0 || window >= RATE_LIMIT_TAIL_CAP {
            return found;
        }
        window *= 4;
    }
}

/// A `token_count` event's general rate limits and its timestamp: `limit_id` absent, null or
/// `codex` (per-model limits are shown live only), with at least one usable window.
fn rate_limits_of(line: &[u8]) -> Option<(Timestamp, Value)> {
    let r: Value = serde_json::from_slice(line).ok()?;
    if r.get("type")?.as_str()? != "event_msg" {
        return None;
    }
    let payload = r.get("payload")?;
    if payload.get("type")?.as_str()? != "token_count" {
        return None;
    }
    let limits = payload.get("rate_limits").filter(|l| l.is_object())?;
    match limits.get("limit_id") {
        None | Some(Value::Null) => {}
        Some(Value::String(id)) if id == "codex" => {}
        Some(_) => return None,
    }
    let at: Timestamp = r.get("timestamp")?.as_str()?.parse().ok()?;
    if usage::codex_rows(limits, None).is_empty() {
        return None;
    }
    Some((at, limits.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injected_blocks_are_not_user_text() {
        for injected in [
            "<environment_context>\n  <cwd>/w</cwd>\n</environment_context>",
            "  <user_instructions>x</user_instructions>",
            "<user_action>\n  <context>",
            "<image>",
            "</image>",
            "<codex_internal_context s=1>",
            "# AGENTS.md instructions for /w\n\n<INSTRUCTIONS>",
            "# Context from my IDE setup:\n\n## Active file: a.rs",
            "   ",
        ] {
            assert_eq!(real_text(injected), None, "{injected:?}");
        }
        assert_eq!(real_text(" fix it "), Some("fix it"));
        assert_eq!(
            real_text("<div> is broken"),
            None,
            "tag-like text is skipped too"
        );
        assert_eq!(real_text("< 3 items"), Some("< 3 items"));
        assert_eq!(
            real_text("# Context from my IDE setup:\n## My request for Codex:\n do it\n"),
            Some("do it")
        );
    }

    #[test]
    fn rollout_ids_come_from_file_names() {
        let id = "019c1e08-e4f6-7d70-a129-38ec744a3f3c";
        assert_eq!(
            rollout_id(&format!("rollout-2026-02-02T19-07-05-{id}.jsonl")),
            Some(id)
        );
        assert_eq!(rollout_id(&format!("rollout-2025-04-18-{id}.json")), None);
        assert_eq!(rollout_id("rollout-short.jsonl"), None);
        assert_eq!(rollout_id(&format!("other-{id}.jsonl")), None);
    }

    // --- cached rate limits (R10) ---

    const T: i64 = 1_790_000_000;

    fn iso(second: i64) -> String {
        Timestamp::from_second(second).unwrap().to_string()
    }

    /// A `token_count` event at `second` with a weekly window at `pct`.
    fn token_count(second: i64, limit_id: Value, pct: f64) -> String {
        let limits = serde_json::json!({"limit_id": limit_id, "limit_name": null,
            "primary": {"used_percent": pct, "window_minutes": 10080, "resets_at": 1790414559},
            "secondary": null, "credits": null, "plan_type": "pro"});
        event(second, limits)
    }

    fn event(second: i64, rate_limits: Value) -> String {
        let record = serde_json::json!({"timestamp": iso(second), "type": "event_msg",
            "payload": {"type": "token_count", "info": null, "rate_limits": rate_limits}});
        format!("{record}\n")
    }

    /// A rollout `n` under `home/<dir>`, last modified at `mtime` (whole seconds).
    fn rollout(home: &Path, dir: &str, n: u32, contents: &str, mtime: i64) {
        let dir = home.join(dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!(
            "rollout-2026-09-20T10-00-00-00000000-0000-0000-0000-{n:012}.jsonl"
        ));
        fs::write(&path, contents).unwrap();
        let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime as u64);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(at)
            .unwrap();
    }

    fn percent_of(found: Option<(Timestamp, Value)>) -> Option<(i64, f64)> {
        found.map(|(at, v)| {
            (
                at.as_second(),
                v["primary"]["used_percent"].as_f64().unwrap(),
            )
        })
    }

    const DAY: &str = "sessions/2026/09/20";

    /// R10: the newest general record wins; a per-model or null `rate_limits` after it does not
    /// hide it.
    #[test]
    fn cached_rate_limits_take_the_newest_general_record() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        assert_eq!(cached_rate_limits(home), None);
        let newer = [
            token_count(T + 1, serde_json::json!("codex"), 50.0),
            token_count(T + 2, serde_json::json!("codex_bengalfox"), 7.0),
            event(T + 3, Value::Null),
            format!(
                "{}\n",
                serde_json::json!({"timestamp": iso(T + 4), "type": "response_item",
                "payload": {"type": "message", "role": "user", "content": []}})
            ),
        ]
        .concat();
        rollout(home, DAY, 1, &newer, T + 10);
        rollout(
            home,
            DAY,
            2,
            &token_count(T, serde_json::json!("codex"), 10.0),
            T,
        );
        assert_eq!(percent_of(cached_rate_limits(home)), Some((T + 1, 50.0)));
    }

    /// R10: files are read newest mtime first, but the newest record decides; reading stops at
    /// a file last written before the best record.
    #[test]
    fn a_record_in_an_older_file_can_be_newer() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        rollout(home, DAY, 1, &token_count(T + 1, Value::Null, 50.0), T + 10);
        rollout(
            home,
            DAY,
            2,
            &token_count(T + 5, serde_json::json!("codex"), 60.0),
            T + 5,
        );
        // Last written before T + 5: not read (its record could not really be newer).
        rollout(
            home,
            DAY,
            3,
            &token_count(T + 9, serde_json::json!("codex"), 70.0),
            T + 2,
        );
        assert_eq!(percent_of(cached_rate_limits(home)), Some((T + 5, 60.0)));
    }

    #[test]
    fn archived_sessions_count() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        rollout(
            home,
            "archived_sessions",
            1,
            &token_count(T, serde_json::json!("codex"), 30.0),
            T,
        );
        assert_eq!(percent_of(cached_rate_limits(home)), Some((T, 30.0)));
        rollout(
            home,
            DAY,
            2,
            &token_count(T + 1, serde_json::json!("codex"), 40.0),
            T + 1,
        );
        assert_eq!(percent_of(cached_rate_limits(home)), Some((T + 1, 40.0)));
    }

    /// R10: the tail window grows ×4 from 64 KB up to 1 MB.
    #[test]
    fn the_tail_window_grows() {
        let filler = |bytes: usize| -> String {
            let line = format!(
                "{}\n",
                serde_json::json!({"timestamp": iso(T), "type": "response_item",
                    "payload": {"type": "reasoning", "text": "x".repeat(1000)}})
            );
            line.repeat(bytes / line.len() + 1)
        };
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let record = token_count(T, serde_json::json!("codex"), 30.0);
        rollout(home, DAY, 1, &(record.clone() + &filler(300 * 1024)), T);
        assert_eq!(percent_of(cached_rate_limits(home)), Some((T, 30.0)));
        let far = tempfile::tempdir().unwrap();
        rollout(far.path(), DAY, 1, &(record + &filler(1100 * 1024)), T);
        assert_eq!(cached_rate_limits(far.path()), None);
    }

    /// R10: at most 16 rollouts are read, newest first.
    #[test]
    fn only_the_newest_16_files_are_read() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        rollout(home, DAY, 0, &token_count(T, Value::Null, 30.0), T);
        for n in 1..RATE_LIMIT_FILES as u32 {
            rollout(home, DAY, n, "{}\n", T + i64::from(n));
        }
        assert_eq!(RATE_LIMIT_FILES, 16);
        assert_eq!(percent_of(cached_rate_limits(home)), Some((T, 30.0)));
        rollout(home, DAY, 99, "{}\n", T + 100);
        assert_eq!(cached_rate_limits(home), None);
    }

    /// R10: malformed lines, records without a timestamp or a usable window, and other limits
    /// are skipped; a null `limit_id` (2025) counts.
    #[test]
    fn malformed_lines_and_records_without_a_timestamp_are_skipped() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let no_time = token_count(T + 2, Value::Null, 90.0).replace("\"timestamp\"", "\"time\"");
        let bad_time = token_count(T + 2, Value::Null, 90.0).replace(&iso(T + 2), "yesterday");
        let junk_window = event(
            T + 3,
            serde_json::json!({"limit_id": "codex", "primary": {"used_percent": "x"}}),
        );
        let contents = [
            token_count(T + 1, Value::Null, 30.0),
            no_time,
            bad_time,
            junk_window,
            event(T + 4, serde_json::json!("limits")),
            token_count(T + 5, serde_json::json!(7), 80.0),
            "{\"type\": \"event_msg\", \"payload\": {\"type\": \"token_count\", trunc\n"
                .to_string(),
            "not json but \"token_count\"\n".to_string(),
        ]
        .concat();
        rollout(home, DAY, 1, &contents, T + 10);
        assert_eq!(percent_of(cached_rate_limits(home)), Some((T + 1, 30.0)));
    }

    #[test]
    fn sources_are_named() {
        assert_eq!(
            source_name(&serde_json::json!("cli")).as_deref(),
            Some("cli")
        );
        assert_eq!(
            source_name(&serde_json::json!({"subagent": {"other": "guardian"}})).as_deref(),
            Some("subagent")
        );
        assert_eq!(source_name(&serde_json::json!(null)), None);
        assert_eq!(source_name(&serde_json::json!("")), None);
    }
}
