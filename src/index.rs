//! Session index over claude transcript stores and codex rollout stores, cached in
//! `$REMUDA_HOME/state/index.json` (SPEC R8, R17).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

use anyhow::Result;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::Env;
use crate::provider::{Provider, codex};
use crate::registry::{self, Account};
use crate::transcript::{self, Head, Tail, WINDOW, complete_lines, read_at};

/// Bump whenever [`Entry`] or the scanning rules change: a mismatching cache is rebuilt.
///
/// - 2: local commands (`/clear`, ...) no longer count as the first user text.
/// - 3: codex rollouts (`provider`, `source`, `originator`).
pub const SCHEMA_VERSION: u32 = 3;

/// A session store: the realpath of one or more accounts' `projects` (claude) or `sessions`
/// (codex) directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    pub provider: Provider,
    /// Realpath of the directory; used only to deduplicate (R8), never to launch (R2).
    pub path: PathBuf,
    /// `provider:name` of every account whose directory resolves here, in registry order.
    pub accounts: Vec<String>,
    /// Codex: the `session_index.jsonl` of every account's home sharing the store, in registry
    /// order, where the thread names are (R17). Empty for claude.
    pub thread_names: Vec<PathBuf>,
}

/// What the index knows about one transcript or rollout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub provider: Provider,
    /// Claude: the file stem. Codex: the first `session_meta`'s id, else the id in the file
    /// name.
    pub session_id: String,
    pub path: PathBuf,
    /// [`Store::path`] of the store the file was found in.
    pub store: PathBuf,
    pub size: u64,
    /// Nanoseconds since the Unix epoch.
    pub mtime_ns: i128,
    /// Inode: a replaced file is rescanned even if it looks like it grew.
    pub ino: u64,
    /// Offset of the first byte not yet parsed; always a line start.
    pub scanned_offset: u64,
    /// The cold scan skipped bytes between the head and tail windows.
    pub gap: bool,
    /// Claude: the last `ai-title` seen. Codex: the thread name (R17).
    pub title: Option<String>,
    pub first_user_text: Option<String>,
    pub cwd_first: Option<String>,
    pub cwd_last: Option<String>,
    /// RFC 3339, as written by claude.
    pub ts_first: Option<String>,
    pub ts_last: Option<String>,
    /// Claude only.
    pub entrypoint: Option<String>,
    /// Codex only: `session_meta.source` (`cli`, `vscode`, `exec`, `subagent`, …).
    pub source: Option<String>,
    /// Codex only: `session_meta.originator` (`codex-tui`, `codex_vscode`, …).
    pub originator: Option<String>,
}

impl Entry {
    /// The `ai-title`, else the first user text.
    pub fn display_title(&self) -> Option<&str> {
        self.title.as_deref().or(self.first_user_text.as_deref())
    }

    /// `ts_last` parsed; `None` when missing or unparseable.
    pub fn last_activity(&self) -> Option<Timestamp> {
        self.ts_last.as_deref()?.parse().ok()
    }

    /// Sort key: last activity, else the file's mtime (nanoseconds).
    fn activity_ns(&self) -> i128 {
        self.last_activity()
            .map_or(self.mtime_ns, |t| t.as_nanosecond())
    }
}

/// The cached index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    pub schema_version: u32,
    /// Keyed by transcript path.
    pub entries: BTreeMap<PathBuf, Entry>,
}

impl Default for Index {
    fn default() -> Self {
        Index {
            schema_version: SCHEMA_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

/// Reported by [`refresh`] after listing the stores (`entry: None`) and after each file
/// that had to be read.
#[derive(Debug, Clone, Copy)]
pub struct Progress<'a> {
    /// Files read so far / files that need reading (unchanged files are not counted).
    pub done: usize,
    pub total: usize,
    pub bytes_read: u64,
    pub entry: Option<&'a Entry>,
}

/// What a [`refresh`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RefreshStats {
    /// Transcripts present after the refresh.
    pub files: usize,
    pub reused: usize,
    pub incremental: usize,
    pub cold: usize,
    pub removed: usize,
    pub bytes_read: u64,
}

impl Index {
    /// Loads the cache; a missing, unreadable, malformed or other-schema file is an empty index.
    pub fn load(path: &Path) -> Index {
        #[derive(Deserialize)]
        struct Version {
            schema_version: u32,
        }
        let Ok(text) = fs::read(path) else {
            return Index::default();
        };
        match serde_json::from_slice::<Version>(&text) {
            Ok(v) if v.schema_version == SCHEMA_VERSION => {
                serde_json::from_slice(&text).unwrap_or_default()
            }
            _ => Index::default(),
        }
    }

    /// Writes the cache atomically.
    pub fn save(&self, path: &Path) -> Result<()> {
        registry::write_atomic(path, &serde_json::to_vec(self)?)
    }

    /// Entries newest first: by `ts_last` (else mtime), then path.
    pub fn sorted(&self) -> Vec<&Entry> {
        let mut all: Vec<&Entry> = self.entries.values().collect();
        all.sort_by(|a, b| {
            b.activity_ns()
                .cmp(&a.activity_ns())
                .then_with(|| a.path.cmp(&b.path))
        });
        all
    }
}

/// Session stores of `accounts`: the realpath of each account's `projects` (claude) or
/// `sessions` (codex) directory, in its home or the native one (`$HOME/.claude`, `$HOME/.codex`)
/// for `default`. Missing directories are skipped; accounts sharing a store are grouped so it
/// is scanned once.
pub fn stores(accounts: &[Account], env: &Env) -> Vec<Store> {
    let mut stores: Vec<Store> = Vec::new();
    for account in accounts {
        let Some(home) = account.home_dir(env) else {
            continue;
        };
        let provider = account.provider;
        let Ok(real) = fs::canonicalize(home.join(provider.store_dir())) else {
            continue;
        };
        if !real.is_dir() {
            continue;
        }
        let names = (provider == Provider::Codex).then(|| home.join("session_index.jsonl"));
        match stores
            .iter_mut()
            .find(|s| s.provider == provider && s.path == real)
        {
            Some(store) => {
                store.accounts.push(account.qualified());
                store.thread_names.extend(names);
            }
            None => stores.push(Store {
                provider,
                path: real,
                accounts: vec![account.qualified()],
                thread_names: names.into_iter().collect(),
            }),
        }
    }
    stores
}

/// Size, mtime and inode of a transcript as listed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Stat {
    pub(crate) size: u64,
    pub(crate) mtime_ns: i128,
    pub(crate) ino: u64,
}

impl Stat {
    pub(crate) fn of(meta: &fs::Metadata) -> Stat {
        Stat {
            size: meta.len(),
            mtime_ns: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
            ino: meta.ino(),
        }
    }
}

/// A transcript or rollout that has to be read.
struct Job {
    provider: Provider,
    path: PathBuf,
    /// From the file name; a rollout's own record may name another.
    session_id: String,
    store: PathBuf,
    /// Index into the stores given to [`refresh`].
    store_index: usize,
    /// Present when [`refresh`] decided on an incremental scan; `None` means cold.
    cached: Option<Entry>,
}

/// The largest head window of a rollout. Measured on 1441 real rollouts (498 of them `cli` or
/// `vscode`): a 64 KB head finds a user text for 347 of those 498, 256 KB for 439, 1 MB for
/// 441 (the p99 of its offset is ~800 KB), 4 MB for 444 at 100 MB more read per cold index.
pub const CODEX_HEAD_CAP: u64 = 1024 * 1024;

/// Worker threads for reading transcripts (IO bound).
const WORKERS: usize = 8;

/// Brings `index` up to date with `stores`: new files are scanned cold (head and tail
/// windows), grown files incrementally, unchanged files reused; vanished files and files of
/// stores no longer listed drop out. Codex titles are the thread names of the store's
/// `session_index.jsonl` files (every home sharing it) as they are now, also for unchanged rollouts (R17). Never fails:
/// unreadable files are skipped.
pub fn refresh(
    index: &mut Index,
    stores: &[Store],
    mut progress: impl FnMut(Progress<'_>),
) -> RefreshStats {
    let mut stats = RefreshStats::default();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut jobs: Vec<Job> = Vec::new();
    let names: Vec<HashMap<String, String>> = stores
        .iter()
        .map(|s| codex::thread_names(&s.thread_names))
        .collect();
    for (store_index, store) in stores.iter().enumerate() {
        for (path, session_id, stat) in list_store(store) {
            if !seen.insert(path.clone()) {
                continue;
            }
            let cached = index.entries.get(&path);
            let cached = match cached {
                Some(e)
                    if e.ino == stat.ino
                        && e.store == store.path
                        && e.provider == store.provider =>
                {
                    Some(e)
                }
                _ => None,
            };
            let incremental = match cached {
                Some(e) if e.size == stat.size && e.mtime_ns == stat.mtime_ns => {
                    stats.reused += 1;
                    continue;
                }
                Some(e) if stat.size > e.size && stat.mtime_ns >= e.mtime_ns => {
                    stats.incremental += 1;
                    cached.cloned()
                }
                _ => {
                    stats.cold += 1;
                    None
                }
            };
            jobs.push(Job {
                provider: store.provider,
                path,
                session_id,
                store: store.path.clone(),
                store_index,
                cached: incremental,
            });
        }
    }
    let before = index.entries.len();
    index.entries.retain(|path, _| seen.contains(path));
    stats.removed = before - index.entries.len();
    // Thread names change without the rollout changing.
    for (store, names) in stores.iter().zip(&names) {
        if store.provider == Provider::Codex {
            for entry in index.entries.values_mut() {
                if entry.store == store.path {
                    entry.title = names.get(&entry.session_id).cloned();
                }
            }
        }
    }

    let total = jobs.len();
    progress(Progress {
        done: 0,
        total,
        bytes_read: 0,
        entry: None,
    });
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<(usize, Option<(Entry, u64)>)>();
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
            let entry = match result {
                Some((mut entry, bytes)) => {
                    stats.bytes_read += bytes;
                    if entry.provider == Provider::Codex {
                        entry.title = names[jobs[i].store_index].get(&entry.session_id).cloned();
                    }
                    let path = entry.path.clone();
                    index.entries.insert(path.clone(), entry);
                    index.entries.get(&path)
                }
                // Vanished or unreadable since listing: drop it rather than keep a stale entry.
                None => {
                    index.entries.remove(&jobs[i].path);
                    None
                }
            };
            progress(Progress {
                done: done + 1,
                total,
                bytes_read: stats.bytes_read,
                entry,
            });
        }
    });
    stats.files = index.entries.len();
    stats
}

/// The files of a store with the session id their name gives, and their stat.
fn list_store(store: &Store) -> Vec<(PathBuf, String, Stat)> {
    match store.provider {
        Provider::Claude => list_projects(&store.path),
        Provider::Codex => {
            let mut out = Vec::new();
            list_rollouts(&store.path, &mut out);
            out
        }
    }
}

/// `<sessions>/**/rollout-*.jsonl` (R17): `sessions/YYYY/MM/DD/` in practice. Symlinked
/// directories below the store are not followed; unreadable entries and non-UTF-8 names are
/// skipped.
pub(crate) fn list_rollouts(dir: &Path, out: &mut Vec<(PathBuf, String, Stat)>) {
    let Ok(listing) = fs::read_dir(dir) else {
        return;
    };
    for item in listing.flatten() {
        let path = item.path();
        let Ok(kind) = item.file_type() else { continue };
        if kind.is_dir() {
            if item.file_name() != "archived_sessions" {
                list_rollouts(&path, out);
            }
            continue;
        }
        let Some(id) = item
            .file_name()
            .to_str()
            .and_then(codex::rollout_id)
            .map(str::to_string)
        else {
            continue;
        };
        match fs::metadata(&path) {
            Ok(meta) if meta.is_file() => out.push((path, id, Stat::of(&meta))),
            _ => {}
        }
    }
}

/// `<store>/*/*.jsonl`, top level of each project directory only (R8). Unreadable entries
/// and non-UTF-8 names are skipped.
fn list_projects(store: &Path) -> Vec<(PathBuf, String, Stat)> {
    let mut out = Vec::new();
    let Ok(projects) = fs::read_dir(store) else {
        return out;
    };
    for project in projects.flatten() {
        let dir = project.path();
        if !fs::metadata(&dir).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        let Ok(files) = fs::read_dir(&dir) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            let Some(session_id) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".jsonl"))
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let session_id = session_id.to_string();
            match fs::metadata(&path) {
                Ok(meta) if meta.is_file() => out.push((path, session_id, Stat::of(&meta))),
                _ => {}
            }
        }
    }
    out
}

/// Reads one transcript: incrementally from the cached offset, or cold. Returns the entry
/// and the bytes read; `None` if the file cannot be read.
fn scan(job: &Job) -> Option<(Entry, u64)> {
    let file = File::open(&job.path).ok()?;
    // Stat the open file: it may have changed since listing, and what we read must match.
    // The listing's decision can only be downgraded here (to cold, if the file is no longer
    // a grown version of the cached one), never upgraded.
    let stat = Stat::of(&file.metadata().ok()?);
    match &job.cached {
        Some(cached) if stat.size > cached.size && stat.ino == cached.ino => {
            scan_incremental(&file, stat, cached)
        }
        _ => scan_cold(job, &file, stat),
    }
}

fn scan_incremental(file: &File, stat: Stat, cached: &Entry) -> Option<(Entry, u64)> {
    let from = cached.scanned_offset;
    let buf = read_at(file, from, stat.size - from).ok()?;
    let bytes = buf.len() as u64;
    let lines = complete_lines(&buf, false);

    let mut entry = cached.clone();
    entry.size = stat.size;
    entry.mtime_ns = stat.mtime_ns;
    if let Some(end) = lines.end {
        entry.scanned_offset = from + end as u64;
    }
    // Without a gap everything before `from` was parsed, so head fields still missing are
    // genuinely absent there and the new bytes hold the first occurrence.
    if !cached.gap {
        let mut head = head_of(cached);
        head.feed(&lines.lines);
        set_head(&mut entry, head);
    }
    let mut tail = Tail::new(cached.provider);
    tail.feed_backwards(&lines.lines);
    entry.title = tail.title.or(entry.title);
    entry.cwd_last = tail.cwd_last.or(entry.cwd_last);
    entry.ts_last = tail.ts_last.or(entry.ts_last);
    finish(&mut entry);
    Some((entry, bytes))
}

fn scan_cold(job: &Job, file: &File, stat: Stat) -> Option<(Entry, u64)> {
    let size = stat.size;
    let mut bytes = 0;
    let head_len = if size <= 2 * WINDOW { size } else { WINDOW };
    let mut head_buf = read_at(file, 0, head_len).ok()?;
    bytes += head_buf.len() as u64;
    let mut head = Head::new(job.provider);
    // Bytes of `head_buf` fed to `head` so far (a line start).
    let mut fed = 0;
    loop {
        let lines = complete_lines(&head_buf[fed..], false);
        head.feed(&lines.lines);
        fed += lines.end.unwrap_or(0);
        // Codex writes its instructions before the user's first words, which are past the first
        // 64 KB in most real rollouts: its head window grows ×4 until the head is complete, up
        // to [`CODEX_HEAD_CAP`] (and, like a small file, whole when little would be left). A
        // claude head is one window.
        let read = head_buf.len() as u64;
        if job.provider != Provider::Codex
            || head.complete()
            || read >= size
            || read >= CODEX_HEAD_CAP
        {
            break;
        }
        let mut next = (read * 4).min(CODEX_HEAD_CAP);
        if next + WINDOW >= size {
            next = size;
        }
        let more = read_at(file, read, next - read).ok()?;
        if more.is_empty() {
            break;
        }
        bytes += more.len() as u64;
        head_buf.extend_from_slice(&more);
    }
    let head_len = head_buf.len() as u64;
    let head_lines = complete_lines(&head_buf, false);
    let head_end = head_lines.end.unwrap_or(0) as u64;

    // Tail window: grows while it holds no record with a timestamp (e.g. it lies inside one
    // huge line), until it reaches the head window or the cap. A small file was read whole.
    let mut tail_buf = Vec::new();
    let mut tail_start = head_end;
    if head_len < size {
        let mut window = WINDOW;
        loop {
            let start = size.saturating_sub(window).max(head_end);
            // From one byte before `start` (unless that is the head's end, a known line
            // start), so a window that begins exactly at a line start keeps that line.
            let read_from = if start > head_end { start - 1 } else { start };
            tail_buf = read_at(file, read_from, size - read_from).ok()?;
            bytes += tail_buf.len() as u64;
            tail_start = read_from;
            let lines = complete_lines(&tail_buf, read_from > head_end);
            let mut probe = Tail::new(job.provider);
            probe.feed_backwards(&lines.lines);
            if probe.ts_last.is_some() || start <= head_end || window >= transcript::PREVIEW_CAP {
                break;
            }
            window *= 4;
        }
    }
    let cut = tail_start > head_end;
    let tail_lines = complete_lines(&tail_buf, cut);
    let gap = cut && tail_start + tail_lines.start as u64 > head_end;

    let mut tail = Tail::new(job.provider);
    tail.feed_backwards(&tail_lines.lines);
    if !gap {
        head.feed(&tail_lines.lines);
        tail.feed_backwards(&head_lines.lines);
    }
    let scanned_offset = match tail_lines.end {
        Some(end) if tail_start + (end as u64) > head_end => tail_start + end as u64,
        _ => head_end,
    };

    let mut entry = Entry {
        provider: job.provider,
        session_id: job.session_id.clone(),
        path: job.path.clone(),
        store: job.store.clone(),
        size,
        mtime_ns: stat.mtime_ns,
        ino: stat.ino,
        scanned_offset,
        gap,
        title: tail.title,
        first_user_text: None,
        cwd_first: None,
        cwd_last: tail.cwd_last,
        ts_first: None,
        ts_last: tail.ts_last,
        entrypoint: None,
        source: None,
        originator: None,
    };
    set_head(&mut entry, head);
    finish(&mut entry);
    Some((entry, bytes))
}

/// A rollout's last directory: its last `turn_context`'s, else its `session_meta`'s (they
/// differ in 3 of 1405 real rollouts, only by a trailing `/`).
fn finish(e: &mut Entry) {
    if e.provider == Provider::Codex && e.cwd_last.is_none() {
        e.cwd_last = e.cwd_first.clone();
    }
}

fn head_of(e: &Entry) -> Head {
    Head {
        provider: e.provider,
        cwd_first: e.cwd_first.clone(),
        ts_first: e.ts_first.clone(),
        entrypoint: e.entrypoint.clone(),
        first_user_text: e.first_user_text.clone(),
        // A complete line was parsed: the first record has been seen.
        started: e.scanned_offset > 0,
        session_id: Some(e.session_id.clone()),
        source: e.source.clone(),
        originator: e.originator.clone(),
    }
}

fn set_head(e: &mut Entry, head: Head) {
    e.cwd_first = head.cwd_first;
    e.ts_first = head.ts_first;
    e.entrypoint = head.entrypoint;
    e.first_user_text = head.first_user_text;
    e.source = head.source;
    e.originator = head.originator;
    if let Some(id) = head.session_id {
        e.session_id = id;
    }
}
