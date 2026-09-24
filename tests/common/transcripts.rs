//! Synthetic Claude transcript records (R15): real record shapes, made-up content.

use serde_json::{Value, json};

/// A JSONL line: the compact JSON of `v` plus `\n`.
pub fn line(v: Value) -> String {
    format!("{v}\n")
}

/// `2026-09-20T10:<minute>:00.000Z`.
pub fn ts(minute: u32) -> String {
    format!("2026-09-20T10:{minute:02}:00.000Z")
}

fn base(kind: &str, cwd: &str, ts: &str) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("parentUuid".into(), Value::Null);
    m.insert("isSidechain".into(), json!(false));
    m.insert("type".into(), json!(kind));
    m.insert("cwd".into(), json!(cwd));
    m.insert(
        "sessionId".into(),
        json!("00000000-0000-4000-8000-00000000000a"),
    );
    m.insert("version".into(), json!("2.1.281"));
    m.insert("entrypoint".into(), json!("cli"));
    m.insert("timestamp".into(), json!(ts));
    m.insert("uuid".into(), json!("00000000-0000-4000-8000-0000000000ff"));
    m
}

/// A user record with string content.
pub fn user(text: &str, cwd: &str, ts: &str) -> String {
    let mut m = base("user", cwd, ts);
    m.insert("message".into(), json!({"role": "user", "content": text}));
    line(Value::Object(m))
}

/// A user record with string content and a given `entrypoint`.
pub fn user_from(entrypoint: &str, text: &str, cwd: &str, ts: &str) -> String {
    let mut m = base("user", cwd, ts);
    m.insert("entrypoint".into(), json!(entrypoint));
    m.insert("message".into(), json!({"role": "user", "content": text}));
    line(Value::Object(m))
}

/// A user record whose content is a list of blocks.
pub fn user_blocks(blocks: Value, cwd: &str, ts: &str) -> String {
    let mut m = base("user", cwd, ts);
    m.insert("message".into(), json!({"role": "user", "content": blocks}));
    line(Value::Object(m))
}

/// An `isMeta` user record (claude's own boilerplate, e.g. the local-command caveat).
pub fn meta_user(text: &str, cwd: &str, ts: &str) -> String {
    let mut m = base("user", cwd, ts);
    m.insert("isMeta".into(), json!(true));
    m.insert("message".into(), json!({"role": "user", "content": text}));
    line(Value::Object(m))
}

/// A user record that only carries a tool result.
pub fn tool_result(body: &str, cwd: &str, ts: &str) -> String {
    user_blocks(
        json!([{"type": "tool_result", "tool_use_id": "toolu_01", "content": body}]),
        cwd,
        ts,
    )
}

/// An assistant record (claude writes one record per content block, sharing `message.id`).
pub fn assistant(id: &str, blocks: Value, cwd: &str, ts: &str) -> String {
    let mut m = base("assistant", cwd, ts);
    m.insert(
        "message".into(),
        json!({"id": id, "type": "message", "role": "assistant", "model": "claude-test",
               "content": blocks}),
    );
    line(Value::Object(m))
}

pub fn assistant_text(id: &str, text: &str, cwd: &str, ts: &str) -> String {
    assistant(id, json!([{"type": "text", "text": text}]), cwd, ts)
}

pub fn tool_use(id: &str, name: &str, cwd: &str, ts: &str) -> String {
    assistant(
        id,
        json!([{"type": "tool_use", "id": "toolu_01", "name": name, "input": {"x": 1}}]),
        cwd,
        ts,
    )
}

pub fn thinking(id: &str, text: &str, cwd: &str, ts: &str) -> String {
    assistant(
        id,
        json!([{"type": "thinking", "thinking": text, "signature": "sig"}]),
        cwd,
        ts,
    )
}

/// `{"type":"ai-title","aiTitle":…}`: no cwd, no timestamp.
pub fn ai_title(title: &str) -> String {
    line(json!({"type": "ai-title", "aiTitle": title,
                "sessionId": "00000000-0000-4000-8000-00000000000a"}))
}

/// An attachment record padded to at least `bytes` bytes (one line).
pub fn filler(bytes: usize, cwd: &str, ts: &str) -> String {
    let mut m = base("attachment", cwd, ts);
    m.insert(
        "attachment".into(),
        json!({"type": "file", "content": "f".repeat(bytes)}),
    );
    line(Value::Object(m))
}
