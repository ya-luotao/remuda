//! Reading Claude transcripts (`projects/*/*.jsonl`) without loading them whole (SPEC R8).
//!
//! Transcripts are append-only JSONL written by a live process: only complete lines
//! (terminated by `\n`) are parsed, lines cut by a read window are dropped, and malformed
//! lines are skipped.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use crate::provider::{Provider, codex};

/// Default read window at each end of a transcript.
pub const WINDOW: u64 = 64 * 1024;
/// The preview's tail window grows up to this size.
pub const PREVIEW_CAP: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

/// One message of a preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub text: String,
}

/// The last `n` user / assistant messages of a transcript, oldest first.
///
/// Reads a tail window that grows (64 KB, 256 KB, 1 MB, 4 MB) until it holds more than `n`
/// messages (so the oldest one kept is not cut by the window) or reaches the file start.
/// User messages: string content or `text` blocks (`isMeta` records, local commands and
/// records that only carry tool results are skipped). Assistant messages: `text` blocks, `tool_use` as
/// `[tool: <name>]`, thinking skipped; claude writes one record per content block, so
/// consecutive assistant records sharing `message.id` form one message.
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

#[derive(Deserialize)]
struct PreviewRecord {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "isMeta")]
    is_meta: Option<bool>,
    message: Option<MessageBody>,
}

/// Messages of `lines`, in order; the `message.id` of each assistant message is kept for
/// merging consecutive records.
fn messages(lines: &[&[u8]]) -> Vec<Message> {
    let mut out: Vec<(Message, Option<String>)> = Vec::new();
    for line in lines {
        let Ok(record) = serde_json::from_slice::<PreviewRecord>(line) else {
            continue;
        };
        let Some(body) = record.message else { continue };
        let content = body.content.unwrap_or(Value::Null);
        match record.kind.as_deref() {
            Some("user") if record.is_meta != Some(true) => {
                if let Some(text) = user_text(&content) {
                    out.push((
                        Message {
                            role: Role::User,
                            text,
                        },
                        None,
                    ));
                }
            }
            Some("assistant") => {
                let Some(text) = assistant_text(&content) else {
                    continue;
                };
                match out.last_mut() {
                    Some((last, Some(id)))
                        if last.role == Role::Assistant && body.id.as_ref() == Some(id) =>
                    {
                        last.text.push('\n');
                        last.text.push_str(&text);
                    }
                    _ => out.push((
                        Message {
                            role: Role::Assistant,
                            text,
                        },
                        body.id,
                    )),
                }
            }
            _ => {}
        }
    }
    out.into_iter().map(|(m, _)| m).collect()
}

/// `text` blocks and `[tool: <name>]` for `tool_use`; `None` when there is neither.
fn assistant_text(content: &Value) -> Option<String> {
    let parts: Vec<String> = match content {
        Value::String(s) => vec![s.clone()],
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => b.get("text").and_then(Value::as_str).map(str::to_string),
                Some("tool_use") => Some(format!(
                    "[tool: {}]",
                    b.get("name").and_then(Value::as_str).unwrap_or("?")
                )),
                _ => None,
            })
            .filter(|t| !t.trim().is_empty())
            .collect(),
        _ => Vec::new(),
    };
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// Reads up to `len` bytes at `offset` (fewer at end of file).
pub(crate) fn read_at(file: &File, offset: u64, len: u64) -> io::Result<Vec<u8>> {
    let mut buf = vec![0; usize::try_from(len).unwrap_or(usize::MAX)];
    let mut filled = 0;
    while filled < buf.len() {
        match file.read_at(&mut buf[filled..], offset + filled as u64) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

/// The complete lines of a buffer read from a transcript.
pub(crate) struct Lines<'a> {
    /// Non-empty lines, without their `\n`, in file order.
    pub lines: Vec<&'a [u8]>,
    /// Offset in the buffer where the first kept line starts.
    pub start: usize,
    /// Offset just past the last `\n`; `None` when the buffer has none.
    pub end: Option<usize>,
}

/// Splits `buf` into complete lines. With `cut_first`, everything up to and including the
/// first `\n` is dropped (the buffer started mid-line, or one byte before a line start).
/// Bytes after the last `\n` are an unfinished line and dropped too.
pub(crate) fn complete_lines(buf: &[u8], cut_first: bool) -> Lines<'_> {
    let end = buf.iter().rposition(|&b| b == b'\n').map(|i| i + 1);
    let start = if cut_first {
        match buf.iter().position(|&b| b == b'\n') {
            Some(i) => i + 1,
            None => buf.len(),
        }
    } else {
        0
    };
    let lines = match end {
        Some(end) if start < end => buf[start..end - 1]
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .collect(),
        _ => Vec::new(),
    };
    Lines { lines, start, end }
}

/// The record fields the index needs; everything else (notably `message`) is skipped
/// without being built.
#[derive(Deserialize)]
struct Fields {
    #[serde(rename = "type")]
    kind: Option<String>,
    cwd: Option<String>,
    timestamp: Option<String>,
    entrypoint: Option<String>,
    #[serde(rename = "aiTitle")]
    ai_title: Option<String>,
    #[serde(rename = "isMeta")]
    is_meta: Option<bool>,
}

#[derive(Deserialize)]
struct WithMessage {
    message: Option<MessageBody>,
}

#[derive(Deserialize)]
struct MessageBody {
    id: Option<String>,
    content: Option<Value>,
}

fn fields(line: &[u8]) -> Option<Fields> {
    serde_json::from_slice(line).ok()
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.is_empty())
}

/// How claude records a local or slash command (`/clear`, `/model`, a skill) and its output
/// as user records; none of it is text the user typed (R8). Newer claude versions put
/// `<command-message>` before `<command-name>`.
const LOCAL_COMMAND_PREFIXES: [&str; 4] = [
    "<command-name>",
    "<command-message>",
    "<local-command-stdout>",
    "<local-command-stderr>",
];

fn is_local_command(text: &str) -> bool {
    let text = text.trim_start();
    LOCAL_COMMAND_PREFIXES.iter().any(|p| text.starts_with(p))
}

/// Text a user typed: string content, or the `text` blocks of block content. `None` when
/// there is none (e.g. a record that only carries tool results) or it records a local
/// command.
fn user_text(content: &Value) -> Option<String> {
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .filter(|t| !t.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    (!text.trim().is_empty() && !is_local_command(&text)).then_some(text)
}

/// Whitespace collapsed to single spaces, at most `cap` characters (the last one `…` when cut).
pub(crate) fn one_line(text: &str, cap: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= cap {
        return joined;
    }
    let mut cut: String = joined.chars().take(cap.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// Maximum length of [`Head::first_user_text`], in characters.
pub(crate) const FIRST_USER_TEXT_CAP: usize = 200;

/// Fields taken from the first records that have them; fed lines in file order. Claude's
/// transcripts and codex's rollouts ([`crate::provider::codex`]) fill different fields.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Head {
    pub provider: Provider,
    pub cwd_first: Option<String>,
    pub ts_first: Option<String>,
    pub entrypoint: Option<String>,
    pub first_user_text: Option<String>,
    /// Codex: the first record has been read (only it describes the session).
    pub started: bool,
    /// Codex: from the first `session_meta`.
    pub session_id: Option<String>,
    pub source: Option<String>,
    pub originator: Option<String>,
}

impl Head {
    pub fn new(provider: Provider) -> Self {
        Head {
            provider,
            ..Head::default()
        }
    }

    pub fn complete(&self) -> bool {
        match self.provider {
            Provider::Claude => {
                self.cwd_first.is_some()
                    && self.ts_first.is_some()
                    && self.entrypoint.is_some()
                    && self.first_user_text.is_some()
            }
            Provider::Codex => codex::head_complete(self),
        }
    }

    pub fn feed(&mut self, lines: &[&[u8]]) {
        if self.provider == Provider::Codex {
            return codex::feed_head(self, lines);
        }
        for line in lines {
            if self.complete() {
                return;
            }
            let Some(f) = fields(line) else { continue };
            self.cwd_first = self.cwd_first.take().or(non_empty(f.cwd));
            self.ts_first = self.ts_first.take().or(non_empty(f.timestamp));
            self.entrypoint = self.entrypoint.take().or(non_empty(f.entrypoint));
            // `isMeta` user records are claude's own boilerplate (e.g. the local-command caveat);
            // local commands are skipped by `user_text`.
            if self.first_user_text.is_none()
                && f.kind.as_deref() == Some("user")
                && f.is_meta != Some(true)
            {
                self.first_user_text = serde_json::from_slice::<WithMessage>(line)
                    .ok()
                    .and_then(|r| r.message?.content)
                    .and_then(|c| user_text(&c))
                    .map(|t| one_line(&t, FIRST_USER_TEXT_CAP));
            }
        }
    }
}

/// Fields taken from the last records that have them; fed lines in file order, read backwards.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Tail {
    pub provider: Provider,
    pub title: Option<String>,
    pub cwd_last: Option<String>,
    pub ts_last: Option<String>,
}

impl Tail {
    pub fn new(provider: Provider) -> Self {
        Tail {
            provider,
            ..Tail::default()
        }
    }

    pub fn complete(&self) -> bool {
        self.title.is_some() && self.cwd_last.is_some() && self.ts_last.is_some()
    }

    pub fn feed_backwards(&mut self, lines: &[&[u8]]) {
        if self.provider == Provider::Codex {
            return codex::feed_tail(self, lines);
        }
        for line in lines.iter().rev() {
            if self.complete() {
                return;
            }
            let Some(f) = fields(line) else { continue };
            if self.title.is_none() && f.kind.as_deref() == Some("ai-title") {
                self.title = non_empty(f.ai_title);
            }
            self.cwd_last = self.cwd_last.take().or(non_empty(f.cwd));
            self.ts_last = self.ts_last.take().or(non_empty(f.timestamp));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_lines_drops_cut_and_unfinished_lines() {
        let l = complete_lines(b"a\nbb\n\nccc", false);
        assert_eq!(l.lines, [b"a".as_slice(), b"bb"]);
        assert_eq!((l.start, l.end), (0, Some(6)));
        let l = complete_lines(b"tail of cut\nx\ny\npartial", true);
        assert_eq!(l.lines, [b"x".as_slice(), b"y"]);
        assert_eq!((l.start, l.end), (12, Some(16)));
        // One byte before a line start: the cut is exactly that byte.
        let l = complete_lines(b"\nx\n", true);
        assert_eq!(l.lines, [b"x".as_slice()]);
        let l = complete_lines(b"no newline at all", true);
        assert!(l.lines.is_empty());
        assert_eq!(l.end, None);
        let l = complete_lines(b"", false);
        assert!(l.lines.is_empty());
    }

    #[test]
    fn one_line_collapses_and_caps() {
        assert_eq!(one_line("  a\n\n b\tc  ", 10), "a b c");
        assert_eq!(one_line("abcdef", 4), "abc…");
        assert_eq!(one_line("abcd", 4), "abcd");
        assert_eq!(one_line("日本語テキスト", 3), "日本…");
    }
}
