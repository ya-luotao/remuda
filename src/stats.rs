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
use crate::pricing::{PRICES_AS_OF, Prices, Rate};
use crate::provider::Provider;
use crate::registry::{self, Account};
use crate::text::{self, human_count, human_usd};
use crate::transcript::{contains, read_at};

/// Bump whenever [`Row`], [`FileStats`] or the counting rules change: a mismatching cache is
/// rebuilt (2: cache write by lifetime, fast / US flags).
pub const SCHEMA_VERSION: u32 = 2;

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
    /// Cache writes with a 5-minute lifetime, and those recorded without one.
    pub cache_write_5m: u64,
    /// Cache writes with a 1-hour lifetime.
    pub cache_write_1h: u64,
    pub output: u64,
    /// Part of `output`; recorded by codex only.
    pub reasoning: u64,
}

impl Tokens {
    /// Input + cache read + cache write + output (reasoning is part of output).
    pub fn total(&self) -> u64 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write())
            .saturating_add(self.output)
    }

    /// Cache writes of both lifetimes.
    pub fn cache_write(&self) -> u64 {
        self.cache_write_5m.saturating_add(self.cache_write_1h)
    }

    pub fn is_zero(&self) -> bool {
        *self == Tokens::default()
    }

    pub fn add(&mut self, other: &Tokens) {
        self.input = self.input.saturating_add(other.input);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write_5m = self.cache_write_5m.saturating_add(other.cache_write_5m);
        self.cache_write_1h = self.cache_write_1h.saturating_add(other.cache_write_1h);
        self.output = self.output.saturating_add(other.output);
        self.reasoning = self.reasoning.saturating_add(other.reasoning);
    }

    /// The largest of each count: records of one request repeat its usage, growing.
    pub fn max_each(&mut self, other: &Tokens) {
        self.input = self.input.max(other.input);
        self.cache_read = self.cache_read.max(other.cache_read);
        self.cache_write_5m = self.cache_write_5m.max(other.cache_write_5m);
        self.cache_write_1h = self.cache_write_1h.max(other.cache_write_1h);
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
    /// Fast mode (`usage.speed == "fast"`).
    pub fast: bool,
    /// US-only inference (`usage.inference_geo == "us"`).
    pub geo_us: bool,
    pub tokens: Tokens,
}

/// A [`Row`] in the cache: `[key, ts (0: none), model, flags (bit 0: copy, bit 1: fast, bit 2:
/// US-only), input, cache read, cache write 5m, cache write 1h, output, reasoning]`. The cache
/// holds a row per request (~600K), so it is kept compact.
type RowRepr = (u64, i64, u32, u8, u64, u64, u64, u64, u64, u64);

impl From<RowRepr> for Row {
    fn from(
        (key, ts, model, flags, input, cache_read, write_5m, write_1h, output, reasoning): RowRepr,
    ) -> Self {
        Row {
            key,
            ts: (ts != 0).then_some(ts),
            model,
            copy: flags & 1 != 0,
            fast: flags & 2 != 0,
            geo_us: flags & 4 != 0,
            tokens: Tokens {
                input,
                cache_read,
                cache_write_5m: write_5m,
                cache_write_1h: write_1h,
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
            u8::from(r.copy) | u8::from(r.fast) << 1 | u8::from(r.geo_us) << 2,
            t.input,
            t.cache_read,
            t.cache_write_5m,
            t.cache_write_1h,
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

    /// After [`refresh`] reported `refreshed`: writes the cache unless the refresh changed
    /// nothing and the file exists (it is tens of MB on a large corpus). Returns whether it
    /// was written.
    pub fn save_if_changed(&self, path: &Path, refreshed: &RefreshStats) -> Result<bool> {
        let changed = refreshed.reused != refreshed.files || refreshed.removed > 0;
        if !changed && path.exists() {
            return Ok(false);
        }
        self.save(path)?;
        Ok(true)
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
pub(crate) fn fnv1a(parts: &[&[u8]]) -> u64 {
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
    /// The cache write by lifetime.
    cache_creation: Option<CacheCreation>,
    /// `"fast"` in fast mode.
    speed: Option<String>,
    /// `"us"` for US-only inference.
    inference_geo: Option<String>,
    iterations: Option<Vec<ClaudeIteration>>,
}

/// `usage.cache_creation`: the cache write by lifetime.
#[derive(Deserialize)]
struct CacheCreation {
    ephemeral_5m_input_tokens: Option<u64>,
    ephemeral_1h_input_tokens: Option<u64>,
}

impl CacheCreation {
    /// (5-minute, 1-hour), missing counts 0.
    fn split(&self) -> (u64, u64) {
        (
            self.ephemeral_5m_input_tokens.unwrap_or(0),
            self.ephemeral_1h_input_tokens.unwrap_or(0),
        )
    }
}

/// A request's cache write as (5-minute, 1-hour): `total` (else the split's sum), of which the
/// split's 1-hour part, at most `total`, is 1-hour and the rest 5-minute (R20).
fn cache_writes(total: Option<u64>, split: Option<(u64, u64)>) -> (u64, u64) {
    let total = total.unwrap_or_else(|| split.map_or(0, |(m5, h1)| m5.saturating_add(h1)));
    let h1 = split.map_or(0, |(_, h1)| h1).min(total);
    (total - h1, h1)
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
    cache_creation: Option<CacheCreation>,
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
                old.fast |= row.fast;
                old.geo_us |= row.geo_us;
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
        // With several `message` entries, the top-level split is the first one's; their sum is
        // the message's (R20).
        let split = usage
            .iterations
            .iter()
            .flatten()
            .filter(|it| it.kind.as_deref() == Some("message"))
            .filter_map(|it| it.cache_creation.as_ref().map(CacheCreation::split))
            .fold(None, |sum: Option<(u64, u64)>, (m5, h1)| {
                let (a, b) = sum.unwrap_or_default();
                Some((a.saturating_add(m5), b.saturating_add(h1)))
            })
            .or_else(|| usage.cache_creation.as_ref().map(CacheCreation::split));
        let (cache_write_5m, cache_write_1h) =
            cache_writes(usage.cache_creation_input_tokens, split);
        self.put(Row {
            key: fnv1a(&[b"claude:", id.as_bytes()]),
            ts,
            model,
            copy,
            fast: usage.speed.as_deref() == Some("fast"),
            geo_us: usage.inference_geo.as_deref() == Some("us"),
            tokens: Tokens {
                input: usage.input_tokens.unwrap_or(0),
                cache_read: usage.cache_read_input_tokens.unwrap_or(0),
                cache_write_5m,
                cache_write_1h,
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
            let (cache_write_5m, cache_write_1h) = cache_writes(
                it.cache_creation_input_tokens,
                it.cache_creation.as_ref().map(CacheCreation::split),
            );
            // An advisor call records neither speed nor inference_geo: standard, global.
            self.put(Row {
                key: fnv1a(&[b"advisor:", id.as_bytes(), b":", index.as_bytes()]),
                ts,
                model,
                copy,
                fast: false,
                geo_us: false,
                tokens: Tokens {
                    input: it.input_tokens.unwrap_or(0),
                    cache_read: it.cache_read_input_tokens.unwrap_or(0),
                    cache_write_5m,
                    cache_write_1h,
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
                    fast: false,
                    geo_us: false,
                    tokens: Tokens {
                        input: input.saturating_sub(cached),
                        cache_read: cached,
                        cache_write_5m: 0,
                        cache_write_1h: 0,
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

/// An estimated cost (R20), exact, in picodollars (10⁻¹² USD).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cost {
    /// Of the requests that could be priced.
    pub pico_usd: u128,
    /// Tokens (total) of the requests that could not be priced.
    pub unpriced_tokens: u64,
}

impl Cost {
    pub fn add(&mut self, other: &Cost) {
        self.pico_usd = self.pico_usd.saturating_add(other.pico_usd);
        self.unpriced_tokens = self.unpriced_tokens.saturating_add(other.unpriced_tokens);
    }

    /// As shown: `-` when nothing is priced (and something is not), else [`human_usd`],
    /// followed by `+` when requests are left out.
    pub fn cell(&self) -> String {
        if self.pico_usd == 0 && self.unpriced_tokens > 0 {
            return "-".to_string();
        }
        let partial = if self.unpriced_tokens > 0 { "+" } else { "" };
        format!("{}{partial}", human_usd(self.pico_usd))
    }
}

/// The granularity of a chart's buckets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Hour,
    Day,
    Week,
    Month,
}

impl Step {
    pub fn noun(self) -> &'static str {
        match self {
            Step::Hour => "hour",
            Step::Day => "day",
            Step::Week => "week",
            Step::Month => "month",
        }
    }
}

/// Usage from `start` to the next bucket's start (the last: to the end of today).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bucket {
    pub start: Timestamp,
    pub tokens: Tokens,
    pub cost: Cost,
}

/// Tokens and estimated cost of one model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRow {
    pub provider: Provider,
    /// As recorded.
    pub model: String,
    pub tokens: Tokens,
    pub cost: Cost,
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

    pub fn cost(&self) -> Cost {
        let mut total = Cost::default();
        for m in &self.models {
            total.add(&m.cost);
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
    /// The period over time, overall (R20): hourly for today, else daily (all: from the local
    /// day of the first request with a timestamp, at most 3,660 days back), ascending, empty
    /// buckets included; empty when nothing has a timestamp.
    pub series: Vec<Bucket>,
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
    /// Whether any copy used fast mode, or US-only inference.
    fast: bool,
    geo_us: bool,
    tokens: Tokens,
}

impl Winner<'_> {
    /// Lower wins: not a copy first, then the earliest timestamp (the path sorting first wins
    /// ties, as files are visited in path order).
    fn rank(&self) -> (bool, i64) {
        (self.copy, self.ts.unwrap_or(i64::MAX))
    }
}

/// Model tokens and cost keyed by provider and model.
type Models<'a> = BTreeMap<(Provider, &'a str), (Tokens, Cost)>;

/// The statistics of `cache` for each period, counting each request once and pricing it with
/// `prices` (R20). A claude session's accounts come from `attribution`, a codex rollout's from
/// the source it was listed under; `accounts` is the registry, in order.
pub fn report(
    cache: &Cache,
    sources: &[Source],
    attribution: &Attribution,
    accounts: &[Account],
    prices: &Prices,
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
                fast: row.fast,
                geo_us: row.geo_us,
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
                    let (fast, geo_us) = (winner.fast || row.fast, winner.geo_us || row.geo_us);
                    if candidate.rank() < winner.rank() {
                        *winner = candidate;
                    }
                    winner.tokens = tokens;
                    winner.fast = fast;
                    winner.geo_us = geo_us;
                }
            }
        }
    }
    let first: Option<i64> = winners.values().filter_map(|w| w.ts).min();

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
    // Per period: the chart's bucket starts (seconds), its end, and its buckets.
    let mut charts: Vec<(Vec<i64>, i64, Vec<Bucket>)> = Period::ALL
        .iter()
        .map(|&p| {
            let (starts, end) = bucket_starts(p, first, now, tz);
            let buckets = starts
                .iter()
                .map(|&start| Bucket {
                    start,
                    tokens: Tokens::default(),
                    cost: Cost::default(),
                })
                .collect();
            let seconds = starts.iter().map(|t| t.as_second()).collect();
            (seconds, end.as_second(), buckets)
        })
        .collect();
    let mut rates: HashMap<(Provider, &str), Option<Rate>> = HashMap::new();

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
        let rate = *rates
            .entry((file.provider, model))
            .or_insert_with(|| prices.rate(file.provider, model));
        let cost = match rate.and_then(|r| r.cost(&winner.tokens, winner.fast, winner.geo_us)) {
            Some(pico_usd) => Cost {
                pico_usd,
                unpriced_tokens: 0,
            },
            None => Cost {
                pico_usd: 0,
                unpriced_tokens: winner.tokens.total(),
            },
        };
        for ((period, start), (starts, end, buckets)) in
            counted.iter_mut().zip(&starts).zip(&mut charts)
        {
            let within = match (start, winner.ts) {
                (None, _) => true,
                (Some(start), Some(ts)) => ts >= *start,
                (Some(_), None) => false,
            };
            if !within {
                continue;
            }
            let (tokens, sum) = period
                .entry(group)
                .or_default()
                .entry((file.provider, model))
                .or_default();
            tokens.add(&winner.tokens);
            sum.add(&cost);
            if let Some(ts) = winner.ts.filter(|ts| ts < end) {
                let i = starts.partition_point(|&s| s <= ts);
                if i > 0 {
                    buckets[i - 1].tokens.add(&winner.tokens);
                    buckets[i - 1].cost.add(&cost);
                }
            }
        }
    }

    let tables = Period::ALL
        .iter()
        .zip(counted)
        .zip(charts)
        .map(|((&period, by_group), (_, _, series))| {
            let mut overall: Models<'_> = BTreeMap::new();
            for models in by_group.values() {
                for (key, (tokens, cost)) in models {
                    let (t, c) = overall.entry(*key).or_default();
                    t.add(tokens);
                    c.add(cost);
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
                series,
            }
        })
        .collect();
    Report {
        tables,
        files: cache.files.len(),
    }
}

/// The local midnight of `date` in `tz`.
fn midnight(date: jiff::civil::Date, tz: &TimeZone) -> Result<Timestamp, jiff::Error> {
    Ok(date.to_zoned(tz.clone())?.start_of_day()?.timestamp())
}

/// The starts of `period`'s chart buckets, ascending, and the end of the last (the start of
/// tomorrow): today, each hour from local midnight (23 to 25); 7 and 30 days, each local day
/// from the period's start; all, each local day from the one of `first` (at most 3,660 days
/// before today) to today, none without `first`. Computed with zoned arithmetic, so DST days
/// have their real length.
fn bucket_starts(
    period: Period,
    first: Option<i64>,
    now: Timestamp,
    tz: &TimeZone,
) -> (Vec<Timestamp>, Timestamp) {
    let starts = || -> Result<(Vec<Timestamp>, Timestamp), jiff::Error> {
        let today = now.to_zoned(tz.clone()).date();
        let end = midnight(today.tomorrow()?, tz)?;
        let from = match period {
            Period::Today => {
                let mut hours = Vec::new();
                let mut hour = midnight(today, tz)?;
                while hour < end {
                    hours.push(hour);
                    hour = hour.checked_add(1.hour())?;
                }
                return Ok((hours, end));
            }
            Period::Week => today.checked_sub(6.days())?,
            Period::Month => today.checked_sub(29.days())?,
            Period::All => {
                let Some(first) = first else {
                    return Ok((Vec::new(), end));
                };
                let first = Timestamp::from_second(first)?.to_zoned(tz.clone()).date();
                first.max(today.checked_sub(3_660.days())?)
            }
        };
        let mut days = Vec::new();
        let mut day = from;
        while day <= today {
            days.push(midnight(day, tz)?);
            day = day.tomorrow()?;
        }
        Ok((days, end))
    };
    starts().unwrap_or((Vec::new(), now))
}

/// The buckets of `table.series` for a chart at most `room` bars wide, and their step: today by
/// hour, 7 and 30 days by day; all by day, else by week (from Monday), else by month, the finest
/// that fits. Buckets that still do not fit are dropped from the oldest.
pub fn chart_series(table: &Table, room: usize, tz: &TimeZone) -> (Step, Vec<Bucket>) {
    let (step, buckets) = match table.period {
        Period::Today => (Step::Hour, table.series.clone()),
        Period::Week | Period::Month => (Step::Day, table.series.clone()),
        Period::All if table.series.len() <= room => (Step::Day, table.series.clone()),
        Period::All => {
            let weeks = regroup(&table.series, Step::Week, tz);
            match weeks.len() <= room {
                true => (Step::Week, weeks),
                false => (Step::Month, regroup(&table.series, Step::Month, tz)),
            }
        }
    };
    let dropped = buckets.len().saturating_sub(room);
    (step, buckets[dropped..].to_vec())
}

/// `series` merged by the local week (from Monday) or month of each bucket's start; a merged
/// bucket starts at local midnight of that Monday or first of the month.
fn regroup(series: &[Bucket], step: Step, tz: &TimeZone) -> Vec<Bucket> {
    let key = |b: &Bucket| {
        let date = b.start.to_zoned(tz.clone()).date();
        match step {
            Step::Week => date
                .checked_sub(i64::from(date.weekday().to_monday_zero_offset()).days())
                .unwrap_or(date),
            _ => date.first_of_month(),
        }
    };
    let mut out: Vec<(jiff::civil::Date, Bucket)> = Vec::new();
    for b in series {
        let k = key(b);
        match out.last_mut() {
            Some((last, merged)) if *last == k => {
                merged.tokens.add(&b.tokens);
                merged.cost.add(&b.cost);
            }
            _ => out.push((
                k,
                Bucket {
                    start: midnight(k, tz).unwrap_or(b.start),
                    ..*b
                },
            )),
        }
    }
    out.into_iter().map(|(_, b)| b).collect()
}

/// Column headers of [`format`].
const COLUMNS: [&str; 8] = [
    "MODEL",
    "INPUT",
    "CACHE READ",
    "CACHE WRITE",
    "OUTPUT",
    "REASONING",
    "TOTAL",
    "COST",
];

/// `table` as plain text for `remuda stats`: a title, the column headers, then each section
/// (its accounts joined by ` + `, or `unattributed`) with its models and their total, and last
/// the `overall` section; then a line saying the cost is an estimate at API list prices, and
/// one naming the models not priced, if any. With `filter` (`provider:name`), only the sections
/// including that account, and no overall section. Counts are [`human_count`]s; a count the
/// providers of a row do not record is `-` (cache write: claude only; reasoning: codex only);
/// costs are [`Cost::cell`]s. Columns align over the whole output.
pub fn format(table: &Table, filter: Option<&str>, tz: &TimeZone) -> String {
    let sections: Vec<&Section> = table
        .sections
        .iter()
        .filter(|s| filter.is_none_or(|f| s.accounts.iter().any(|a| a == f)))
        .collect();
    let mut blocks: Vec<(String, Vec<[String; 8]>)> = sections
        .iter()
        .map(|s| {
            let label = match s.accounts.is_empty() {
                true => "unattributed".to_string(),
                false => s.accounts.join(" + "),
            };
            (label, format_rows(&s.models))
        })
        .collect();
    let mut printed: Vec<&ModelRow> = sections.iter().flat_map(|s| &s.models).collect();
    if filter.is_none() {
        blocks.push(("overall".to_string(), format_rows(&table.overall)));
        printed.extend(&table.overall);
    }
    let header = COLUMNS.map(str::to_string);
    let mut widths = [0; 8];
    for row in blocks.iter().flat_map(|(_, rows)| rows).chain([&header]) {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(text::width(cell));
        }
    }
    let line = |row: &[String; 8]| {
        let mut line = text::pad(&row[0], widths[0]);
        for (cell, &width) in row.iter().zip(&widths).skip(1) {
            line.push_str("  ");
            line.push_str(&" ".repeat(width.saturating_sub(text::width(cell))));
            line.push_str(cell);
        }
        line + "\n"
    };

    let mut out = format!("Tokens · {}", table.period.label());
    if let Some(since) = table.since {
        let since = since.to_zoned(tz.clone()).strftime("%Y-%m-%d %H:%M");
        out.push_str(&format!(" (since {since})"));
    }
    out.push_str("\n\n");
    out.push_str(&line(&header));
    for (i, (label, rows)) in blocks.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(label);
        out.push('\n');
        if rows.is_empty() {
            out.push_str("  no tokens\n");
        }
        for row in rows {
            out.push_str(&line(row));
        }
    }
    out.push_str(&format!(
        "\nCost ≈ API list price (prices as of {PRICES_AS_OF}): an estimate, not a bill.\n"
    ));
    let unpriced = unpriced_models(printed);
    if !unpriced.is_empty() {
        out.push_str(&format!(
            "Not priced: {} (add [prices.\"<model>\"] to config.toml)\n",
            unpriced.join(", ")
        ));
    }
    out
}

/// The cells of each model, then of their total; none without models.
fn format_rows(models: &[ModelRow]) -> Vec<[String; 8]> {
    if models.is_empty() {
        return Vec::new();
    }
    let row = |name: String, counts: [String; 7]| {
        let [a, b, c, d, e, f, g] = counts;
        [name, a, b, c, d, e, f, g]
    };
    let mut rows: Vec<[String; 8]> = models
        .iter()
        .map(|m| {
            let counts = counts(&m.tokens, &m.cost, &[m.provider]);
            row(format!("  {}", m.model), counts)
        })
        .collect();
    let (total, cost, providers) = sum(models);
    rows.push(row(
        "  total".to_string(),
        counts(&total, &cost, &providers),
    ));
    rows
}

/// The counts of `tokens` as shown, in column order (input, cache read, cache write, output,
/// reasoning, total), as [`human_count`]s, then `cost` as its [`Cost::cell`]. A count none of
/// `providers` records is `-`: cache write is claude's, reasoning codex's.
pub fn counts(tokens: &Tokens, cost: &Cost, providers: &[Provider]) -> [String; 7] {
    let count = |n: u64, recorded_by: Provider| match providers.contains(&recorded_by) {
        true => human_count(n),
        false => "-".to_string(),
    };
    [
        human_count(tokens.input),
        human_count(tokens.cache_read),
        count(tokens.cache_write(), Provider::Claude),
        human_count(tokens.output),
        count(tokens.reasoning, Provider::Codex),
        human_count(tokens.total()),
        cost.cell(),
    ]
}

/// The tokens and cost of `models` together, and the providers they come from.
pub fn sum(models: &[ModelRow]) -> (Tokens, Cost, Vec<Provider>) {
    let mut total = Tokens::default();
    let mut cost = Cost::default();
    let mut providers = Vec::new();
    for m in models {
        total.add(&m.tokens);
        cost.add(&m.cost);
        if !providers.contains(&m.provider) {
            providers.push(m.provider);
        }
    }
    (total, cost, providers)
}

/// The models among `rows` with requests that could not be priced, sorted, once each.
pub fn unpriced_models<'a>(rows: impl IntoIterator<Item = &'a ModelRow>) -> Vec<&'a str> {
    let names: BTreeSet<&str> = rows
        .into_iter()
        .filter(|m| m.cost.unpriced_tokens > 0)
        .map(|m| m.model.as_str())
        .collect();
    names.into_iter().collect()
}

/// Most tokens first, then by provider and model.
fn model_rows(models: &Models<'_>) -> Vec<ModelRow> {
    let mut rows: Vec<ModelRow> = models
        .iter()
        .map(|(&(provider, model), &(tokens, cost))| ModelRow {
            provider,
            model: model.to_string(),
            tokens,
            cost,
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
            fast: true,
            geo_us: true,
            tokens: Tokens {
                input: 1,
                cache_read: 2,
                cache_write_5m: 3,
                cache_write_1h: 6,
                output: 4,
                reasoning: 5,
            },
        };
        let json = serde_json::to_string(&row).unwrap();
        assert_eq!(json, "[18446744073709551615,1790000000,2,7,1,2,3,6,4,5]");
        assert_eq!(serde_json::from_str::<Row>(&json).unwrap(), row);
        let bare = Row {
            ts: None,
            copy: false,
            fast: false,
            geo_us: false,
            model: PENDING,
            ..row
        };
        let json = serde_json::to_string(&bare).unwrap();
        assert_eq!(json, "[18446744073709551615,0,4294967295,0,1,2,3,6,4,5]");
        assert_eq!(serde_json::from_str::<Row>(&json).unwrap(), bare);
        for (flags, fast, geo_us) in [(2, true, false), (4, false, true)] {
            let json = format!("[1,0,0,{flags},0,0,0,0,1,0]");
            let row = serde_json::from_str::<Row>(&json).unwrap();
            assert_eq!((row.copy, row.fast, row.geo_us), (false, fast, geo_us));
        }
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

    /// A model row whose cache write is all 1-hour, not priced.
    fn model(provider: Provider, model: &str, t: [u64; 5]) -> ModelRow {
        let [input, cache_read, cache_write_1h, output, reasoning] = t;
        let tokens = Tokens {
            input,
            cache_read,
            cache_write_5m: 0,
            cache_write_1h,
            output,
            reasoning,
        };
        ModelRow {
            provider,
            model: model.into(),
            tokens,
            cost: cost_of(0, tokens.total()),
        }
    }

    /// R20: the COST column shows each model's cost, `-` for one not priced, and `+` on a total
    /// that leaves some out, named below.
    #[test]
    fn format_aligns_columns_and_dashes_what_a_provider_does_not_record() {
        let claude = ModelRow {
            cost: cost_of(12_340_000_000_000, 0),
            ..model(Provider::Claude, "claude-test", [8, 1_234_567, 160, 90, 0])
        };
        let codex = model(Provider::Codex, "gpt-test", [1500, 200, 0, 30, 12]);
        let table = Table {
            period: Period::Week,
            since: Some("2026-09-17T16:00:00Z".parse().unwrap()),
            sections: vec![
                Section {
                    accounts: vec!["claude:default".into()],
                    models: vec![claude.clone()],
                },
                Section {
                    accounts: vec!["claude:max".into()],
                    models: vec![],
                },
                Section {
                    accounts: vec!["codex:work".into()],
                    models: vec![codex.clone()],
                },
                Section {
                    accounts: vec!["claude:default".into(), "claude:max".into()],
                    models: vec![claude.clone()],
                },
                Section {
                    accounts: vec![],
                    models: vec![codex.clone()],
                },
            ],
            overall: vec![claude, codex],
            series: vec![],
        };
        let tz = TimeZone::fixed(jiff::tz::offset(8));
        assert_eq!(
            format(&table, None, &tz),
            "\
Tokens · last 7 days (since 2026-09-18 00:00)

MODEL          INPUT  CACHE READ  CACHE WRITE  OUTPUT  REASONING  TOTAL     COST
claude:default
  claude-test      8        1.2M          160      90          -   1.2M   $12.34
  total            8        1.2M          160      90          -   1.2M   $12.34

claude:max
  no tokens

codex:work
  gpt-test      1.5K         200            -      30         12   1.7K        -
  total         1.5K         200            -      30         12   1.7K        -

claude:default + claude:max
  claude-test      8        1.2M          160      90          -   1.2M   $12.34
  total            8        1.2M          160      90          -   1.2M   $12.34

unattributed
  gpt-test      1.5K         200            -      30         12   1.7K        -
  total         1.5K         200            -      30         12   1.7K        -

overall
  claude-test      8        1.2M          160      90          -   1.2M   $12.34
  gpt-test      1.5K         200            -      30         12   1.7K        -
  total         1.5K        1.2M          160     120         12   1.2M  $12.34+

Cost ≈ API list price (prices as of 2026-09-24): an estimate, not a bill.
Not priced: gpt-test (add [prices.\"<model>\"] to config.toml)
"
        );
        let filtered = format(&table, Some("claude:max"), &tz);
        let labels: Vec<&str> = filtered
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with(' '))
            .collect();
        assert_eq!(
            labels,
            [
                "Tokens · last 7 days (since 2026-09-18 00:00)",
                "MODEL          INPUT  CACHE READ  CACHE WRITE  OUTPUT  REASONING  TOTAL    COST",
                "claude:max",
                "claude:default + claude:max",
                "Cost ≈ API list price (prices as of 2026-09-24): an estimate, not a bill.",
            ]
        );
        let all = Table {
            period: Period::All,
            since: None,
            sections: vec![],
            overall: vec![],
            series: vec![],
        };
        assert_eq!(
            format(&all, None, &tz),
            "Tokens · all time\n\n\
             MODEL  INPUT  CACHE READ  CACHE WRITE  OUTPUT  REASONING  TOTAL  COST\n\
             overall\n  no tokens\n\n\
             Cost ≈ API list price (prices as of 2026-09-24): an estimate, not a bill.\n"
        );
    }

    /// R20: `-` when nothing is priced, `+` when requests are left out.
    #[test]
    fn cost_cell_marks_partial_and_unpriced() {
        let cost = |pico_usd, unpriced_tokens| {
            Cost {
                pico_usd,
                unpriced_tokens,
            }
            .cell()
        };
        assert_eq!(cost(0, 0), "$0.00");
        assert_eq!(cost(0, 5), "-");
        assert_eq!(cost(12_340_000_000_000, 0), "$12.34");
        assert_eq!(cost(12_340_000_000_000, 3), "$12.34+");
        assert_eq!(cost(1, 3), "<$0.01+");
        let mut sum = cost_of(u128::MAX, u64::MAX);
        sum.add(&cost_of(1, 1));
        assert_eq!(sum, cost_of(u128::MAX, u64::MAX), "saturating");
    }

    fn cost_of(pico_usd: u128, unpriced_tokens: u64) -> Cost {
        Cost {
            pico_usd,
            unpriced_tokens,
        }
    }

    fn chart_table(period: Period, series: Vec<Bucket>) -> Table {
        Table {
            period,
            since: None,
            sections: vec![],
            overall: vec![],
            series,
        }
    }

    /// R20: the finest step that fits; what still does not fit is dropped from the oldest.
    #[test]
    fn chart_series_picks_the_finest_step_that_fits() {
        let tz = TimeZone::UTC;
        let day = 24.hours();
        let days = |start: &str, n| {
            let mut at: Timestamp = start.parse().unwrap();
            (0..n)
                .map(|i: u64| {
                    let b = Bucket {
                        start: at,
                        tokens: Tokens {
                            input: i + 1,
                            ..Tokens::default()
                        },
                        cost: cost_of(u128::from(i) + 1, 0),
                    };
                    at = at.checked_add(day).unwrap();
                    b
                })
                .collect::<Vec<_>>()
        };
        let total = |buckets: &[Bucket]| -> (u64, u128) {
            buckets.iter().fold((0, 0), |(t, c), b| {
                (t + b.tokens.input, c + b.cost.pico_usd)
            })
        };

        let twenty = days("2026-09-01T00:00:00Z", 20);
        let (step, got) = chart_series(&chart_table(Period::All, twenty.clone()), 30, &tz);
        assert_eq!((step, got), (Step::Day, twenty));

        // 100 days from a Wednesday: 15 weeks, the first from the Monday before.
        let hundred = days("2026-06-03T00:00:00Z", 100);
        let (step, weeks) = chart_series(&chart_table(Period::All, hundred.clone()), 30, &tz);
        assert_eq!(step, Step::Week);
        assert_eq!(weeks.len(), 15);
        assert_eq!(weeks[0].start.to_string(), "2026-06-01T00:00:00Z");
        assert_eq!(weeks[1].start.to_string(), "2026-06-08T00:00:00Z");
        assert_eq!(weeks[0].tokens.input, 1 + 2 + 3 + 4 + 5);
        assert_eq!(total(&weeks), total(&hundred));

        // 400 days: months, the last 10.
        let (step, months) = chart_series(
            &chart_table(Period::All, days("2025-08-20T00:00:00Z", 400)),
            10,
            &tz,
        );
        assert_eq!(step, Step::Month);
        let starts: Vec<String> = months.iter().map(|b| b.start.to_string()).collect();
        assert_eq!(starts.len(), 10);
        assert_eq!(starts[0], "2025-12-01T00:00:00Z");
        assert_eq!(starts[9], "2026-09-01T00:00:00Z");
        assert!(months.windows(2).all(|w| w[0].start < w[1].start));

        // Today by hour, the last that fit.
        let mut hours = Vec::new();
        let mut at: Timestamp = "2026-09-24T00:00:00Z".parse().unwrap();
        for i in 0..24u64 {
            hours.push(Bucket {
                start: at,
                tokens: Tokens {
                    input: i,
                    ..Tokens::default()
                },
                cost: Cost::default(),
            });
            at = at.checked_add(1.hour()).unwrap();
        }
        let (step, got) = chart_series(&chart_table(Period::Today, hours.clone()), 10, &tz);
        assert_eq!((step, got), (Step::Hour, hours[14..].to_vec()));
        // 7 and 30 days by day, whatever the room.
        let week = days("2026-09-18T00:00:00Z", 7);
        let (step, got) = chart_series(&chart_table(Period::Week, week.clone()), 5, &tz);
        assert_eq!((step, got), (Step::Day, week[2..].to_vec()));
    }

    /// R20: a cache write split by lifetime; the 1-hour part at most the total, the rest (also
    /// one recorded without lifetimes) 5-minute.
    #[test]
    fn cache_writes_split_by_lifetime() {
        assert_eq!(cache_writes(Some(100), Some((0, 60))), (40, 60));
        assert_eq!(cache_writes(Some(100), None), (100, 0));
        assert_eq!(cache_writes(Some(50), Some((0, 80))), (0, 50));
        assert_eq!(cache_writes(None, Some((5, 7))), (5, 7));
        assert_eq!(cache_writes(None, None), (0, 0));
    }

    #[test]
    fn tokens_total_and_max_each() {
        let mut a = Tokens {
            input: 3,
            cache_read: 300,
            cache_write_5m: 40,
            cache_write_1h: 60,
            output: 10,
            reasoning: 4,
        };
        assert_eq!(a.cache_write(), 100);
        assert_eq!(a.total(), 413, "reasoning is part of output");
        a.max_each(&Tokens {
            input: 1,
            cache_read: 400,
            cache_write_5m: 50,
            cache_write_1h: 0,
            output: 25,
            reasoning: 2,
        });
        assert_eq!(
            a,
            Tokens {
                input: 3,
                cache_read: 400,
                cache_write_5m: 50,
                cache_write_1h: 60,
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
