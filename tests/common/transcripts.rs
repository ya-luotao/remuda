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

/// `message.usage` as claude 2.1.281 writes it.
pub fn usage(input: u64, output: u64, cache_write: u64, cache_read: u64) -> Value {
    json!({"input_tokens": input, "output_tokens": output,
           "cache_creation_input_tokens": cache_write, "cache_read_input_tokens": cache_read,
           "cache_creation": {"ephemeral_1h_input_tokens": cache_write,
                              "ephemeral_5m_input_tokens": 0},
           "server_tool_use": {"web_search_requests": 0, "web_fetch_requests": 0},
           "service_tier": "standard", "inference_geo": "not_available"})
}

/// `message.usage` with the cache write split by lifetime.
pub fn usage_split(
    input: u64,
    output: u64,
    write_5m: u64,
    write_1h: u64,
    cache_read: u64,
) -> Value {
    let mut usage = usage(input, output, write_5m + write_1h, cache_read);
    usage["cache_creation"] = json!({"ephemeral_5m_input_tokens": write_5m,
                                     "ephemeral_1h_input_tokens": write_1h});
    usage
}

/// `usage` with `speed` and `inference_geo` set.
pub fn with_flags(mut usage: Value, speed: &str, geo: &str) -> Value {
    usage["speed"] = json!(speed);
    usage["inference_geo"] = json!(geo);
    usage
}

/// One content block's record of assistant message `id` (claude writes one per block, each
/// repeating the message's usage), with its `requestId`.
pub fn assistant_usage(session: &str, id: &str, model: &str, usage: Value, ts: &str) -> String {
    let mut m = base("assistant", "/w/proj", ts);
    m.insert("sessionId".into(), json!(session));
    m.insert("requestId".into(), json!(format!("req_{id}")));
    m.insert(
        "message".into(),
        json!({"id": id, "type": "message", "role": "assistant", "model": model,
               "content": [{"type": "text", "text": "ok"}], "stop_reason": null,
               "usage": usage}),
    );
    line(Value::Object(m))
}

fn parse(record: &str) -> serde_json::Map<String, Value> {
    match serde_json::from_str(record).unwrap() {
        Value::Object(m) => m,
        other => panic!("not a record: {other}"),
    }
}

/// `record` as a fork copies it: with `forkedFrom` naming the parent session.
pub fn forked(record: &str, parent: &str) -> String {
    let mut m = parse(record);
    m.insert(
        "forkedFrom".into(),
        json!({"sessionId": parent, "messageUuid": "00000000-0000-4000-8000-0000000000ff"}),
    );
    line(Value::Object(m))
}

/// A subagent's record: `isSidechain: true` and `agentId`.
pub fn sidechain(record: &str, agent: &str) -> String {
    let mut m = parse(record);
    m.insert("isSidechain".into(), json!(true));
    m.insert("agentId".into(), json!(agent));
    line(Value::Object(m))
}

/// A `progress` record of claude 2.1.7x–2.1.8x repeating a subagent's assistant record `inner`.
pub fn agent_progress(inner: &str, agent: &str, ts: &str) -> String {
    let inner = parse(inner);
    let mut m = base("progress", "/w/proj", ts);
    m.insert(
        "data".into(),
        json!({"type": "agent_progress", "agentId": agent,
               "message": {"type": "assistant", "timestamp": ts, "message": inner["message"],
                           "requestId": inner["requestId"]}}),
    );
    line(Value::Object(m))
}

/// A user record whose tool result sums a subagent's usage (`toolUseResult.usage`).
pub fn task_result(usage: Value, ts: &str) -> String {
    let mut m = base("user", "/w/proj", ts);
    m.insert(
        "message".into(),
        json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_01",
                                            "content": [{"type": "text", "text": "done"}]}]}),
    );
    m.insert(
        "toolUseResult".into(),
        json!({"status": "completed", "agentId": "a1", "totalTokens": 1,
               "totalToolUseCount": 1, "usage": usage}),
    );
    line(Value::Object(m))
}
