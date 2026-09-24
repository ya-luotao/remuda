//! Token statistics per account and model, counted from claude transcripts and codex rollouts
//! and cached in `$REMUDA_HOME/state/stats.json` (SPEC R20).

use std::collections::hash_map::Entry as Slot;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

use anyhow::Result;
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan};
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

use crate::Env;
use crate::attribution::Attribution;
use crate::index::{self, RefreshStats, Stat};
use crate::provider::Provider;
use crate::registry::{self, Account};
use crate::transcript::{contains, read_at};

/// Bump whenever [`Row`], [`FileStats`] or the counting rules change: a mismatching cache is
/// rebuilt.
pub const SCHEMA_VERSION: u32 = 1;

/// Worker threads for reading transcripts (IO bound).
const WORKERS: usize = 8;

/// Read size; an unfinished line is carried over to the next read.
const CHUNK: u64 = 8 * 1024 * 1024;

/// [`Row::model`] of a codex request seen before the rollout's first `turn_context`.
pub const PENDING: u32 = u32::MAX;

/// The model of a request whose transcript names none.
const UNKNOWN_MODEL: &str = "unknown";

/// Token counts of one request, or a sum of them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    /// Without cache reads and writes.
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    /// Part of `output`; recorded by codex only.
    pub reasoning: u64,
}

impl Tokens {
    /// Input + cache read + cache write + output (reasoning is part of output).
    pub fn total(&self) -> u64 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
            .saturating_add(self.output)
    }

    pub fn is_zero(&self) -> bool {
        *self == Tokens::default()
    }

    pub fn add(&mut self, other: &Tokens) {
        self.input = self.input.saturating_add(other.input);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.output = self.output.saturating_add(other.output);
        self.reasoning = self.reasoning.saturating_add(other.reasoning);
    }

    /// The largest of each count: records of one request repeat its usage, growing.
    pub fn max_each(&mut self, other: &Tokens) {
        self.input = self.input.max(other.input);
        self.cache_read = self.cache_read.max(other.cache_read);
        self.cache_write = self.cache_write.max(other.cache_write);
        self.output = self.output.max(other.output);
        self.reasoning = self.reasoning.max(other.reasoning);
    }
}

/// One request counted from a transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "RowRepr", into = "RowRepr")]
pub struct Row {
    /// FNV-1a hash of the request's key: its `message.id` (claude), or its cumulative total
    /// (codex).
    pub key: u64,
    /// Seconds since the Unix epoch; the earliest of the request's records.
    pub ts: Option<i64>,
    /// Index into [`FileStats::models`], or [`PENDING`].
    pub model: u32,
    /// Marked as a fork's copy of its parent's record (`forkedFrom`).
    pub copy: bool,
    pub tokens: Tokens,
}

/// A [`Row`] in the cache: `[key, ts (0: none), model, flags (bit 0: copy), input, cache read,
/// cache write, output, reasoning]`. The cache holds a row per request (~600K), so it is kept
/// compact.
type RowRepr = (u64, i64, u32, u8, u64, u64, u64, u64, u64);

impl From<RowRepr> for Row {
    fn from(
        (key, ts, model, flags, input, cache_read, cache_write, output, reasoning): RowRepr,
    ) -> Self {
        Row {
            key,
            ts: (ts != 0).then_some(ts),
            model,
            copy: flags & 1 != 0,
            tokens: Tokens {
                input,
                cache_read,
                cache_write,
                output,
                reasoning,
            },
        }
    }
}

impl From<Row> for RowRepr {
    fn from(r: Row) -> Self {
        let t = r.tokens;
        (
            r.key,
            r.ts.unwrap_or(0),
            r.model,
            u8::from(r.copy),
            t.input,
            t.cache_read,
            t.cache_write,
            t.output,
            t.reasoning,
        )
    }
}

/// What was counted from one transcript or rollout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStats {
    pub provider: Provider,
    /// Claude: the file stem at a project's top level, else the name of the session directory
    /// it is below. Codex: the id in the file name.
    pub session_id: String,
    /// [`Source::path`] of the source it was listed under (for attribution).
    pub source: PathBuf,
    pub size: u64,
    /// Nanoseconds since the Unix epoch.
    pub mtime_ns: i128,
    pub ino: u64,
    /// Offset of the first byte not yet parsed; always a line start.
    pub scanned_offset: u64,
    /// Codex: the last `total_token_usage` seen, `[input, cached, output, reasoning, total]`.
    pub codex_total: Option<[u64; 5]>,
    /// Codex: the model of the last `turn_context` seen.
    pub codex_model: Option<String>,
    /// The models [`Row::model`] indexes.
    pub models: Vec<String>,
    pub rows: Vec<Row>,
}

/// The cached statistics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cache {
    pub schema_version: u32,
    /// Keyed by transcript path.
    pub files: BTreeMap<PathBuf, FileStats>,
}

impl Default for Cache {
    fn default() -> Self {
        Cache {
            schema_version: SCHEMA_VERSION,
            files: BTreeMap::new(),
        }
    }
}

impl Cache {
    /// Loads the cache; a missing, unreadable, malformed or other-schema file is an empty cache.
    pub fn load(path: &Path) -> Cache {
        #[derive(Deserialize)]
        struct Version {
            schema_version: u32,
        }
        let Ok(text) = fs::read(path) else {
            return Cache::default();
        };
        match serde_json::from_slice::<Version>(&text) {
            Ok(v) if v.schema_version == SCHEMA_VERSION => {
                serde_json::from_slice(&text).unwrap_or_default()
            }
            _ => Cache::default(),
        }
    }

    /// Writes the cache atomically.
    pub fn save(&self, path: &Path) -> Result<()> {
        registry::write_atomic(path, &serde_json::to_vec(self)?)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// A claude `projects` store.
    Claude,
    /// A codex `sessions` store.
    CodexSessions,
    /// A codex home's `archived_sessions`.
    CodexArchived,
}

impl SourceKind {
    pub fn provider(self) -> Provider {
        match self {
            SourceKind::Claude => Provider::Claude,
            SourceKind::CodexSessions | SourceKind::CodexArchived => Provider::Codex,
        }
    }
}

/// A directory of transcripts or rollouts, read once however many accounts share it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub kind: SourceKind,
    /// Realpath of the directory.
    pub path: PathBuf,
    /// `provider:name` of every account whose directory resolves here, in registry order.
    pub accounts: Vec<String>,
}

/// The sources of `accounts`: the session stores of the index (R8, R17), then each codex
/// home's `archived_sessions`, grouped by realpath like the stores. Missing directories are
/// skipped.
pub fn sources(accounts: &[Account], env: &Env) -> Vec<Source> {
    let mut out: Vec<Source> = index::stores(accounts, env)
        .into_iter()
        .map(|store| Source {
            kind: match store.provider {
                Provider::Claude => SourceKind::Claude,
                Provider::Codex => SourceKind::CodexSessions,
            },
            path: store.path,
            accounts: store.accounts,
        })
        .collect();
    for account in accounts.iter().filter(|a| a.provider == Provider::Codex) {
        let Some(home) = account.home_dir(env) else {
            continue;
        };
        let Ok(real) = fs::canonicalize(home.join("archived_sessions")) else {
            continue;
        };
        if !real.is_dir() {
            continue;
        }
        match out
            .iter_mut()
            .find(|s| s.kind == SourceKind::CodexArchived && s.path == real)
        {
            Some(source) => source.accounts.push(account.qualified()),
            None => out.push(Source {
                kind: SourceKind::CodexArchived,
                path: real,
                accounts: vec![account.qualified()],
            }),
        }
    }
    out
}

/// The files of a source with the session id their path gives, and their stat.
fn list_source(source: &Source) -> Vec<(PathBuf, String, Stat)> {
    match source.kind {
        SourceKind::Claude => list_claude(&source.path),
        SourceKind::CodexSessions | SourceKind::CodexArchived => {
            let mut out = Vec::new();
            index::list_rollouts(&source.path, &mut out);
            out
        }
    }
}

/// `<store>/*/*.jsonl` (the session id is the stem), and every `*.jsonl` at any depth below a
/// directory `<store>/<project>/<dir>/` (subagent transcripts; the session id is `<dir>`).
/// Symlinked directories below a project are not followed; unreadable entries and non-UTF-8
/// names are skipped.
fn list_claude(store: &Path) -> Vec<(PathBuf, String, Stat)> {
    let mut out = Vec::new();
    let Ok(projects) = fs::read_dir(store) else {
        return out;
    };
    for project in projects.flatten() {
        let dir = project.path();
        if !fs::metadata(&dir).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        let Ok(items) = fs::read_dir(&dir) else {
            continue;
        };
        for item in items.flatten() {
            let Ok(kind) = item.file_type() else { continue };
            if kind.is_dir() {
                if let Some(session_id) = item.file_name().to_str() {
                    list_below(&item.path(), session_id, &mut out);
                }
                continue;
            }
            let path = item.path();
            if let Some(session_id) = jsonl_stem(&path) {
                let session_id = session_id.to_string();
                push_file(path, session_id, &mut out);
            }
        }
    }
    out
}

/// Every `*.jsonl` at any depth below `dir`, for `session_id`.
fn list_below(dir: &Path, session_id: &str, out: &mut Vec<(PathBuf, String, Stat)>) {
    let Ok(items) = fs::read_dir(dir) else {
        return;
    };
    for item in items.flatten() {
        let Ok(kind) = item.file_type() else { continue };
        let path = item.path();
        if kind.is_dir() {
            list_below(&path, session_id, out);
        } else if jsonl_stem(&path).is_some() {
            push_file(path, session_id.to_string(), out);
        }
    }
}

/// The non-empty UTF-8 stem of a `*.jsonl` file name.
fn jsonl_stem(path: &Path) -> Option<&str> {
    path.file_name()?
        .to_str()?
        .strip_suffix(".jsonl")
        .filter(|s| !s.is_empty())
}

fn push_file(path: PathBuf, session_id: String, out: &mut Vec<(PathBuf, String, Stat)>) {
    match fs::metadata(&path) {
        Ok(meta) if meta.is_file() => out.push((path, session_id, Stat::of(&meta))),
        _ => {}
    }
}

/// A transcript or rollout that has to be read.
struct Job {
    provider: Provider,
    path: PathBuf,
    session_id: String,
    source: PathBuf,
    /// Present when [`refresh`] decided on an incremental read; `None` means whole.
    cached: Option<FileStats>,
}

/// Brings `cache` up to date with `sources`, like the index (R8): unchanged files are reused,
/// grown ones read from their last complete line, any other one whole; vanished files and
/// files of sources no longer listed drop out. `progress(done, total)` is called after listing
/// and after each file read, `total` being the files that need reading. Never fails:
/// unreadable files are dropped.
pub fn refresh(
    cache: &mut Cache,
    sources: &[Source],
    mut progress: impl FnMut(usize, usize),
) -> RefreshStats {
    let mut stats = RefreshStats::default();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut jobs: Vec<Job> = Vec::new();
    for source in sources {
        let provider = source.kind.provider();
        for (path, session_id, stat) in list_source(source) {
            if !seen.insert(path.clone()) {
                continue;
            }
            let cached = cache
                .files
                .get(&path)
                .filter(|f| f.ino == stat.ino && f.source == source.path && f.provider == provider);
            let incremental = match cached {
                Some(f) if f.size == stat.size && f.mtime_ns == stat.mtime_ns => {
                    stats.reused += 1;
                    continue;
                }
                Some(f) if stat.size > f.size && stat.mtime_ns >= f.mtime_ns => {
                    stats.incremental += 1;
                    cached.cloned()
                }
                _ => {
                    stats.cold += 1;
                    None
                }
            };
            jobs.push(Job {
                provider,
                path,
                session_id,
                source: source.path.clone(),
                cached: incremental,
            });
        }
    }
    let before = cache.files.len();
    cache.files.retain(|path, _| seen.contains(path));
    stats.removed = before - cache.files.len();

    let total = jobs.len();
    progress(0, total);
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<(usize, Option<(FileStats, u64)>)>();
    thread::scope(|scope| {
        for _ in 0..WORKERS.min(total) {
            let tx = tx.clone();
            let (jobs, next) = (&jobs, &next);
            scope.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(i) else { break };
                    if tx.send((i, scan(job))).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        for (done, (i, result)) in rx.into_iter().enumerate() {
            match result {
                Some((file, bytes)) => {
                    stats.bytes_read += bytes;
                    cache.files.insert(jobs[i].path.clone(), file);
                }
                // Vanished or unreadable since listing: drop it rather than keep stale counts.
                None => {
                    cache.files.remove(&jobs[i].path);
                }
            }
            progress(done + 1, total);
        }
    });
    stats.files = cache.files.len();
    stats
}

/// Reads one file: from the cached offset, or whole. Returns what was counted and the bytes
/// read; `None` if the file cannot be read.
fn scan(job: &Job) -> Option<(FileStats, u64)> {
    let file = File::open(&job.path).ok()?;
    // Stat the open file: it may have changed since listing. The listing's decision can only
    // be downgraded here (to a whole read), never upgraded.
    let stat = Stat::of(&file.metadata().ok()?);
    let mut counted = match &job.cached {
        Some(cached) if stat.size > cached.size && stat.ino == cached.ino => cached.clone(),
        _ => FileStats {
            provider: job.provider,
            session_id: job.session_id.clone(),
            source: job.source.clone(),
            size: 0,
            mtime_ns: 0,
            ino: stat.ino,
            scanned_offset: 0,
            codex_total: None,
            codex_model: None,
            models: Vec::new(),
            rows: Vec::new(),
        },
    };
    counted.size = stat.size;
    counted.mtime_ns = stat.mtime_ns;

    let mut counter = Counter::new(&mut counted);
    let mut offset = counter.file.scanned_offset;
    // Bytes from `offset` on that were read but not parsed: the start of an unfinished line.
    let mut carry: Vec<u8> = Vec::new();
    let mut bytes = 0;
    loop {
        let at = offset + carry.len() as u64;
        if at >= stat.size {
            break;
        }
        let chunk = read_at(&file, at, CHUNK.min(stat.size - at)).ok()?;
        if chunk.is_empty() {
            break;
        }
        bytes += chunk.len() as u64;
        let (Some(first), Some(last)) = (
            chunk.iter().position(|&b| b == b'\n'),
            chunk.iter().rposition(|&b| b == b'\n'),
        ) else {
            carry.extend_from_slice(&chunk);
            continue;
        };
        // The chunk's first line ends the carried one.
        carry.extend_from_slice(&chunk[..first]);
        let rest = chunk[first..last].split(|&b| b == b'\n').skip(1);
        for line in std::iter::once(&carry[..]).chain(rest) {
            if !line.is_empty() {
                counter.line(line);
            }
        }
        offset += (carry.len() + last - first + 1) as u64;
        carry.clear();
        carry.extend_from_slice(&chunk[last + 1..]);
    }
    counted.scanned_offset = offset;
    Some((counted, bytes))
}

/// 64-bit FNV-1a of `parts`, concatenated.
fn fnv1a(parts: &[&[u8]]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for &b in *part {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

/// Seconds since the epoch of an RFC 3339 timestamp; `None` when missing or unparseable.
fn seconds(ts: Option<&str>) -> Option<i64> {
    ts?.parse::<Timestamp>().ok().map(|t| t.as_second())
}

/// The earlier of two timestamps, either of which may be unknown.
fn earliest(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        _ => a.or(b),
    }
}

#[derive(Deserialize)]
struct ClaudeRecord {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "forkedFrom")]
    forked_from: Option<IgnoredAny>,
    /// Named fields only: `content` is skipped without being built.
    message: Option<ClaudeMessage>,
}

#[derive(Deserialize)]
struct ClaudeMessage {
    id: Option<String>,
    model: Option<String>,
    usage: Option<ClaudeUsage>,
}

#[derive(Deserialize)]
struct ClaudeUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    iterations: Option<Vec<ClaudeIteration>>,
}

/// One request behind a message (`usage.iterations[]`).
#[derive(Deserialize)]
struct ClaudeIteration {
    #[serde(rename = "type")]
    kind: Option<String>,
    model: Option<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct CodexRecord {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    payload: Option<CodexPayload>,
}

#[derive(Deserialize)]
struct CodexPayload {
    #[serde(rename = "type")]
    kind: Option<String>,
    /// `turn_context` only.
    model: Option<String>,
    /// `token_count` only; `null` in some events.
    info: Option<CodexInfo>,
}

#[derive(Deserialize)]
struct CodexInfo {
    total_token_usage: Option<CodexUsage>,
    last_token_usage: Option<CodexUsage>,
}

#[derive(Deserialize)]
struct CodexUsage {
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    reasoning_output_tokens: Option<u64>,
    total_tokens: Option<u64>,
}

impl CodexUsage {
    /// `[input, cached, output, reasoning, total]`, missing counts 0.
    fn numbers(&self) -> [u64; 5] {
        [
            self.input_tokens,
            self.cached_input_tokens,
            self.output_tokens,
            self.reasoning_output_tokens,
            self.total_tokens,
        ]
        .map(|n| n.unwrap_or(0))
    }
}

/// Counts the lines of one file into its [`FileStats`], merging records of one request.
struct Counter<'a> {
    file: &'a mut FileStats,
    /// Index into `file.rows` by key.
    rows: HashMap<u64, usize>,
    /// Index into `file.models` by name.
    models: HashMap<String, u32>,
}

impl<'a> Counter<'a> {
    fn new(file: &'a mut FileStats) -> Self {
        let rows = file
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| (r.key, i))
            .collect();
        let models = file
            .models
            .iter()
            .enumerate()
            .map(|(i, m)| (m.clone(), i as u32))
            .collect();
        Counter { file, rows, models }
    }

    fn model(&mut self, name: &str) -> u32 {
        if let Some(&i) = self.models.get(name) {
            return i;
        }
        let i = self.file.models.len() as u32;
        self.file.models.push(name.to_string());
        self.models.insert(name.to_string(), i);
        i
    }

    /// Adds a request, or merges it into the row of its key: the largest of each count and
    /// the earliest timestamp. Requests without tokens are not kept.
    fn put(&mut self, row: Row) {
        if row.tokens.is_zero() {
            return;
        }
        match self.rows.get(&row.key) {
            Some(&i) => {
                let old = &mut self.file.rows[i];
                old.tokens.max_each(&row.tokens);
                old.ts = earliest(old.ts, row.ts);
                old.copy |= row.copy;
            }
            None => {
                self.rows.insert(row.key, self.file.rows.len());
                self.file.rows.push(row);
            }
        }
    }

    fn line(&mut self, line: &[u8]) {
        match self.file.provider {
            Provider::Claude => self.claude(line),
            Provider::Codex => self.codex(line),
        }
    }

    /// An assistant record: its message's usage by `message.id`, and each advisor call in
    /// `usage.iterations` under its own model.
    fn claude(&mut self, line: &[u8]) {
        if !contains(line, b"\"usage\"") {
            return;
        }
        let Ok(record) = serde_json::from_slice::<ClaudeRecord>(line) else {
            return;
        };
        if record.kind.as_deref() != Some("assistant") {
            return;
        }
        let Some(ClaudeMessage {
            id: Some(id),
            model,
            usage: Some(usage),
        }) = record.message
        else {
            return;
        };
        let model = model.unwrap_or_else(|| UNKNOWN_MODEL.to_string());
        if id.is_empty() || model == "<synthetic>" {
            return;
        }
        let ts = seconds(record.timestamp.as_deref());
        let copy = record.forked_from.is_some();
        let model = self.model(&model);
        self.put(Row {
            key: fnv1a(&[b"claude:", id.as_bytes()]),
            ts,
            model,
            copy,
            tokens: Tokens {
                input: usage.input_tokens.unwrap_or(0),
                cache_read: usage.cache_read_input_tokens.unwrap_or(0),
                cache_write: usage.cache_creation_input_tokens.unwrap_or(0),
                output: usage.output_tokens.unwrap_or(0),
                reasoning: 0,
            },
        });
        for (i, it) in usage.iterations.iter().flatten().enumerate() {
            if it.kind.as_deref() != Some("advisor_message") {
                continue;
            }
            let model = self.model(it.model.as_deref().unwrap_or(UNKNOWN_MODEL));
            let index = i.to_string();
            self.put(Row {
                key: fnv1a(&[b"advisor:", id.as_bytes(), b":", index.as_bytes()]),
                ts,
                model,
                copy,
                tokens: Tokens {
                    input: it.input_tokens.unwrap_or(0),
                    cache_read: it.cache_read_input_tokens.unwrap_or(0),
                    cache_write: it.cache_creation_input_tokens.unwrap_or(0),
                    output: it.output_tokens.unwrap_or(0),
                    reasoning: 0,
                },
            });
        }
    }

    /// A `turn_context` sets the model; a `token_count` event whose total changed counts its
    /// latest request.
    fn codex(&mut self, line: &[u8]) {
        if !contains(line, b"\"token_count\"") && !contains(line, b"\"turn_context\"") {
            return;
        }
        let Ok(record) = serde_json::from_slice::<CodexRecord>(line) else {
            return;
        };
        let Some(payload) = record.payload else {
            return;
        };
        match record.kind.as_deref() {
            Some("turn_context") => {
                let Some(model) = payload.model.filter(|m| !m.is_empty()) else {
                    return;
                };
                // Requests before the first turn_context (a rollout that starts compacted)
                // take its model.
                if self.file.codex_model.is_none() {
                    let index = self.model(&model);
                    for row in &mut self.file.rows {
                        if row.model == PENDING {
                            row.model = index;
                        }
                    }
                }
                self.file.codex_model = Some(model);
            }
            Some("event_msg") if payload.kind.as_deref() == Some("token_count") => {
                let Some(info) = payload.info else { return };
                let Some(total) = info.total_token_usage.map(|u| u.numbers()) else {
                    return;
                };
                // A repeated event, or the context size recorded after compacting.
                if self.file.codex_total == Some(total) {
                    return;
                }
                self.file.codex_total = Some(total);
                let Some(last) = info.last_token_usage.map(|u| u.numbers()) else {
                    return;
                };
                let [input, cached, output, reasoning, _] = last;
                let model = match self.file.codex_model.clone() {
                    Some(name) => self.model(&name),
                    None => PENDING,
                };
                let key = total.map(|n| n.to_string()).join(",");
                self.put(Row {
                    key: fnv1a(&[b"codex:", key.as_bytes()]),
                    ts: seconds(record.timestamp.as_deref()),
                    model,
                    copy: false,
                    tokens: Tokens {
                        input: input.saturating_sub(cached),
                        cache_read: cached,
                        cache_write: 0,
                        output,
                        reasoning,
                    },
                });
            }
            _ => {}
        }
    }
}

/// A period the statistics are shown for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Today,
    Week,
    Month,
    All,
}

impl Period {
    pub const ALL: [Period; 4] = [Period::Today, Period::Week, Period::Month, Period::All];

    /// As given to `--period`.
    pub fn name(self) -> &'static str {
        match self {
            Period::Today => "today",
            Period::Week => "7d",
            Period::Month => "30d",
            Period::All => "all",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Period::Today => "today",
            Period::Week => "last 7 days",
            Period::Month => "last 30 days",
            Period::All => "all time",
        }
    }

    pub fn parse(name: &str) -> Option<Period> {
        Period::ALL.into_iter().find(|p| p.name() == name)
    }

    pub fn next(self) -> Period {
        match self {
            Period::Today => Period::Week,
            Period::Week => Period::Month,
            Period::Month => Period::All,
            Period::All => Period::Today,
        }
    }

    /// Local midnight of today, of 6 days before, or of 29 days before; `None` for all.
    pub fn start(self, now: Timestamp, tz: &TimeZone) -> Option<Timestamp> {
        let days_back = match self {
            Period::Today => 0,
            Period::Week => 6,
            Period::Month => 29,
            Period::All => return None,
        };
        let day = now
            .to_zoned(tz.clone())
            .checked_sub(days_back.days())
            .ok()?;
        Some(day.start_of_day().ok()?.timestamp())
    }
}

/// Tokens of one model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRow {
    pub provider: Provider,
    /// As recorded.
    pub model: String,
    pub tokens: Tokens,
}

/// The tokens of the sessions attributed to exactly these accounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// `provider:name`, in registry order (accounts no longer registered last, by name);
    /// empty for unattributed sessions.
    pub accounts: Vec<String>,
    /// Most tokens first.
    pub models: Vec<ModelRow>,
}

impl Section {
    pub fn total(&self) -> Tokens {
        let mut total = Tokens::default();
        for m in &self.models {
            total.add(&m.tokens);
        }
        total
    }
}

/// The statistics of one period.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub period: Period,
    /// Start of the period; `None` for all.
    pub since: Option<Timestamp>,
    /// Every registered account, then each other group of accounts, then unattributed.
    pub sections: Vec<Section>,
    /// Tokens per model over every section.
    pub overall: Vec<ModelRow>,
}

/// The statistics of every period.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// One per [`Period`], in [`Period::ALL`] order.
    pub tables: Vec<Table>,
    /// Transcripts and rollouts counted from.
    pub files: usize,
}

impl Report {
    /// The table of `period`; a report holds one per period.
    pub fn table(&self, period: Period) -> &Table {
        self.tables
            .iter()
            .find(|t| t.period == period)
            .expect("a report has a table per period")
    }
}

/// The copy of a request that counts, and the largest of each count over all its copies.
struct Winner<'a> {
    copy: bool,
    ts: Option<i64>,
    path: &'a Path,
    file: &'a FileStats,
    model: u32,
    tokens: Tokens,
}

impl Winner<'_> {
    /// Lower wins: not a copy first, then the earliest timestamp (the path sorting first wins
    /// ties, as files are visited in path order).
    fn rank(&self) -> (bool, i64) {
        (self.copy, self.ts.unwrap_or(i64::MAX))
    }
}

/// Model tokens keyed by provider and model.
type Models<'a> = BTreeMap<(Provider, &'a str), Tokens>;

/// The statistics of `cache` for each period, counting each request once (R20). A claude
/// session's accounts come from `attribution`, a codex rollout's from the source it was listed
/// under; `accounts` is the registry, in order.
pub fn report(
    cache: &Cache,
    sources: &[Source],
    attribution: &Attribution,
    accounts: &[Account],
    now: Timestamp,
    tz: &TimeZone,
) -> Report {
    let mut winners: HashMap<u64, Winner<'_>> = HashMap::new();
    for (path, file) in &cache.files {
        let relay_copy = attribution.is_relay_copy(path);
        for row in &file.rows {
            let candidate = Winner {
                copy: row.copy || relay_copy,
                ts: row.ts,
                path,
                file,
                model: row.model,
                tokens: row.tokens,
            };
            match winners.entry(row.key) {
                Slot::Vacant(slot) => {
                    slot.insert(candidate);
                }
                Slot::Occupied(mut slot) => {
                    let winner = slot.get_mut();
                    let mut tokens = winner.tokens;
                    tokens.max_each(&row.tokens);
                    if candidate.rank() < winner.rank() {
                        *winner = candidate;
                    }
                    winner.tokens = tokens;
                }
            }
        }
    }

    let registered: Vec<String> = accounts.iter().map(Account::qualified).collect();
    let position = |name: &str| registered.iter().position(|r| r == name);
    let mut groups: Vec<Vec<String>> = Vec::new();
    let mut group_of_accounts: HashMap<Vec<String>, usize> = HashMap::new();
    let mut group_of_file: HashMap<&Path, usize> = HashMap::new();
    let starts: Vec<Option<i64>> = Period::ALL
        .iter()
        .map(|p| p.start(now, tz).map(|t| t.as_second()))
        .collect();
    // Per period, per group.
    let mut counted: Vec<BTreeMap<usize, Models<'_>>> = vec![BTreeMap::new(); Period::ALL.len()];

    for winner in winners.values() {
        let file = winner.file;
        let group = *group_of_file.entry(winner.path).or_insert_with(|| {
            let mut names: Vec<String> = match file.provider {
                Provider::Claude => attribution
                    .accounts(&file.session_id)
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                Provider::Codex => sources
                    .iter()
                    .find(|s| s.kind.provider() == Provider::Codex && s.path == file.source)
                    .map(|s| s.accounts.clone())
                    .unwrap_or_default(),
            };
            names.sort_by(|a, b| {
                (position(a).unwrap_or(usize::MAX), a).cmp(&(position(b).unwrap_or(usize::MAX), b))
            });
            names.dedup();
            *group_of_accounts.entry(names.clone()).or_insert_with(|| {
                groups.push(names);
                groups.len() - 1
            })
        });
        let model = match file.models.get(winner.model as usize) {
            Some(name) if winner.model != PENDING => name.as_str(),
            _ => UNKNOWN_MODEL,
        };
        for (period, start) in counted.iter_mut().zip(&starts) {
            let within = match (start, winner.ts) {
                (None, _) => true,
                (Some(start), Some(ts)) => ts >= *start,
                (Some(_), None) => false,
            };
            if within {
                period
                    .entry(group)
                    .or_default()
                    .entry((file.provider, model))
                    .or_default()
                    .add(&winner.tokens);
            }
        }
    }

    let tables = Period::ALL
        .iter()
        .zip(counted)
        .map(|(&period, by_group)| {
            let mut overall: Models<'_> = BTreeMap::new();
            for models in by_group.values() {
                for (key, tokens) in models {
                    overall.entry(*key).or_default().add(tokens);
                }
            }
            let section = |names: &[String]| Section {
                accounts: names.to_vec(),
                models: group_of_accounts
                    .get(names)
                    .and_then(|g| by_group.get(g))
                    .map(model_rows)
                    .unwrap_or_default(),
            };
            let mut sections: Vec<Section> = registered
                .iter()
                .map(|name| section(std::slice::from_ref(name)))
                .collect();
            let mut others: Vec<&Vec<String>> = by_group
                .keys()
                .map(|&g| &groups[g])
                // Not unattributed, and not a registered account alone (listed above).
                .filter(|names| match names.as_slice() {
                    [] => false,
                    [one] => position(one).is_none(),
                    _ => true,
                })
                .collect();
            others.sort_by_key(|names| {
                (
                    position(&names[0]).unwrap_or(usize::MAX),
                    names.len(),
                    names.to_vec(),
                )
            });
            sections.extend(others.into_iter().map(|names| section(names)));
            if group_of_accounts
                .get(&Vec::new())
                .is_some_and(|g| by_group.contains_key(g))
            {
                sections.push(section(&[]));
            }
            Table {
                period,
                since: period.start(now, tz),
                sections,
                overall: model_rows(&overall),
            }
        })
        .collect();
    Report {
        tables,
        files: cache.files.len(),
    }
}

/// Most tokens first, then by provider and model.
fn model_rows(models: &Models<'_>) -> Vec<ModelRow> {
    let mut rows: Vec<ModelRow> = models
        .iter()
        .map(|(&(provider, model), &tokens)| ModelRow {
            provider,
            model: model.to_string(),
            tokens,
        })
        .collect();
    rows.sort_by(|a, b| {
        b.tokens
            .total()
            .cmp(&a.tokens.total())
            .then_with(|| a.provider.cmp(&b.provider))
            .then_with(|| a.model.cmp(&b.model))
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_matches_the_reference() {
        assert_eq!(fnv1a(&[b"a"]), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(&[]), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(&[b"clau", b"de:x"]), fnv1a(&[b"claude:x"]));
    }

    #[test]
    fn a_row_is_a_compact_array() {
        let row = Row {
            key: u64::MAX,
            ts: Some(1_790_000_000),
            model: 2,
            copy: true,
            tokens: Tokens {
                input: 1,
                cache_read: 2,
                cache_write: 3,
                output: 4,
                reasoning: 5,
            },
        };
        let json = serde_json::to_string(&row).unwrap();
        assert_eq!(json, "[18446744073709551615,1790000000,2,1,1,2,3,4,5]");
        assert_eq!(serde_json::from_str::<Row>(&json).unwrap(), row);
        let bare = Row {
            ts: None,
            copy: false,
            model: PENDING,
            ..row
        };
        let json = serde_json::to_string(&bare).unwrap();
        assert_eq!(json, "[18446744073709551615,0,4294967295,0,1,2,3,4,5]");
        assert_eq!(serde_json::from_str::<Row>(&json).unwrap(), bare);
    }

    #[test]
    fn periods_parse_name_label_and_cycle() {
        for p in Period::ALL {
            assert_eq!(Period::parse(p.name()), Some(p));
        }
        assert_eq!(Period::parse("2w"), None);
        assert_eq!(Period::ALL.map(Period::name), ["today", "7d", "30d", "all"]);
        assert_eq!(
            Period::ALL.map(Period::label),
            ["today", "last 7 days", "last 30 days", "all time"]
        );
        assert_eq!(
            Period::ALL.map(Period::next),
            [Period::Week, Period::Month, Period::All, Period::Today]
        );
    }

    #[test]
    fn tokens_total_and_max_each() {
        let mut a = Tokens {
            input: 3,
            cache_read: 300,
            cache_write: 100,
            output: 10,
            reasoning: 4,
        };
        assert_eq!(a.total(), 413, "reasoning is part of output");
        a.max_each(&Tokens {
            input: 1,
            cache_read: 400,
            cache_write: 0,
            output: 25,
            reasoning: 2,
        });
        assert_eq!(
            a,
            Tokens {
                input: 3,
                cache_read: 400,
                cache_write: 100,
                output: 25,
                reasoning: 4,
            }
        );
        let mut sum = Tokens::default();
        assert!(sum.is_zero());
        sum.add(&a);
        sum.add(&a);
        assert_eq!(sum.total(), 2 * a.total());
        let huge = Tokens {
            input: u64::MAX,
            output: 1,
            ..Tokens::default()
        };
        assert_eq!(huge.total(), u64::MAX);
    }
}
