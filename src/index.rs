//! Session index over claude transcript stores and codex rollout stores, cached in
//! `$REMUDA_HOME/state/index.json` (SPEC R8, R17).

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::Result;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::Env;
use crate::provider::{Provider, codex};
use crate::registry::{self, Account};
use crate::tracking::{self, Files, Listed, Listing, Stat};
pub use crate::tracking::{RefreshStats, Unreadable};
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
    /// The real path of each store when it was last indexed, by the directory as its home
    /// gives it ([`Given::path`]): which entries are a store's while it cannot be resolved
    /// (R8). By the whole path, so that an account given another home does not take over what
    /// its name had, and one given another name keeps what its home has. Absent from a cache
    /// written before it was kept, which is read as remembering nothing; neither [`Entry`] nor
    /// the scanning rules changed, so the schema version did not.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stores: BTreeMap<PathBuf, PathBuf>,
}

impl Default for Index {
    fn default() -> Self {
        Index {
            schema_version: SCHEMA_VERSION,
            entries: BTreeMap::new(),
            stores: BTreeMap::new(),
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

    /// Writes the cache atomically, as a file of `$REMUDA_HOME/state`: readable by the user
    /// alone, in a directory made or tightened to be (R3).
    pub fn save(&self, path: &Path) -> Result<()> {
        registry::write_private(path, &serde_json::to_vec(self)?)
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

/// A directory an account's home gives as its session store (or, for the statistics, as a
/// source), and what resolving it found. One that does not exist is not given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Given {
    pub provider: Provider,
    /// The directory as the account's home gives it: not a real path, and what a cache
    /// remembers the directory by ([`Index::stores`]).
    pub path: PathBuf,
    /// `provider:name` of the account.
    pub account: String,
    /// Its real path. Or, when that cannot be found because the home or a directory on the
    /// way cannot be searched or read, the system's error as text: the directory is not known
    /// to be missing, and the files a cache has of it are those of the real path it last
    /// resolved to (R8).
    pub real: Result<PathBuf, String>,
}

impl Given {
    /// As given and the error, when it could not be resolved.
    pub(crate) fn unresolved(&self) -> Option<(&Path, &str)> {
        let error = self.real.as_ref().err()?;
        Some((&self.path, error))
    }
}

impl tracking::Resolved for Given {
    fn given(&self) -> &Path {
        &self.path
    }

    fn real(&self) -> Option<&Path> {
        self.real.as_deref().ok()
    }
}

/// Session stores of `accounts`: the realpath of each account's `projects` (claude) or
/// `sessions` (codex) directory, in its home or the native one (`$HOME/.claude`, `$HOME/.codex`)
/// for `default`. Missing directories are skipped; accounts sharing a store are grouped so it
/// is scanned once. A directory that may be there and cannot be resolved is left out too:
/// [`resolve`] tells of those.
pub fn stores(accounts: &[Account], env: &Env) -> Vec<Store> {
    resolve(accounts, env).0
}

/// The session stores of `accounts` ([`stores`]), and each directory a home gives as its
/// store with what resolving it found, in registry order: those that resolved, to a store of
/// the list, and those that may be there and whose real path cannot be found. For
/// [`refresh_with`], which remembers the former and keeps what the index has of the latter
/// (R8). A directory that does not exist is in neither.
pub fn resolve(accounts: &[Account], env: &Env) -> (Vec<Store>, Vec<Given>) {
    let mut stores: Vec<Store> = Vec::new();
    let mut given: Vec<Given> = Vec::new();
    for account in accounts {
        let Some(home) = account.home_dir(env) else {
            continue;
        };
        let provider = account.provider;
        let path = home.join(provider.store_dir());
        let real = match tracking::real_dir(&path) {
            Ok(None) => continue,
            Ok(Some(real)) => Ok(real),
            Err(error) => Err(error),
        };
        given.push(Given {
            provider,
            path,
            account: account.qualified(),
            real: real.clone(),
        });
        let Ok(real) = real else { continue };
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
    (stores, given)
}

/// A transcript or rollout to scan cold.
struct Job {
    provider: Provider,
    path: PathBuf,
    /// From the file name; a rollout's own record may name another.
    session_id: String,
    store: PathBuf,
}

/// The largest head window of a rollout. Measured on 1441 real rollouts (498 of them `cli` or
/// `vscode`): a 64 KB head finds a user text for 347 of those 498, 256 KB for 439, 1 MB for
/// 441 (the p99 of its offset is ~800 KB), 4 MB for 444 at 100 MB more read per cold index.
pub const CODEX_HEAD_CAP: u64 = 1024 * 1024;

/// A store as [`refresh`] tracks it: with the thread names of its `session_index.jsonl` files
/// (every home sharing it) as they are now (R17); none for claude.
struct Named<'a> {
    store: &'a Store,
    names: HashMap<String, String>,
}

/// A directory the index tracks.
enum Dir<'a> {
    Store(Named<'a>),
    /// One that may be a store and whose real path could not be found.
    Unresolved {
        given: &'a Given,
        error: &'a str,
        /// The real path it had when it was last indexed, if the index remembers one.
        last: Option<&'a Path>,
    },
}

/// How the index lists a store and reads one of its files ([`tracking::Files`]).
struct Sessions;

impl Files<Dir<'_>> for Sessions {
    type Tracked = Entry;

    fn list(&self, dir: &Dir<'_>, listing: &mut Listing) {
        match dir {
            Dir::Store(Named { store, .. }) => match store.provider {
                Provider::Claude => list_projects(&store.path, listing),
                Provider::Codex => list_rollouts(&store.path, listing),
            },
            Dir::Unresolved { given, error, .. } => listing.unresolved(&given.path, error),
        }
    }

    fn stat(entry: &Entry) -> Stat {
        Stat {
            size: entry.size,
            mtime_ns: entry.mtime_ns,
            ino: entry.ino,
        }
    }

    /// Of a directory that could not be resolved: of the store it was when last indexed.
    fn listed_under(entry: &Entry, dir: &Dir<'_>) -> bool {
        let (store, provider) = match dir {
            Dir::Store(Named { store, .. }) => (Some(store.path.as_path()), store.provider),
            Dir::Unresolved { given, last, .. } => (*last, given.provider),
        };
        store == Some(entry.store.as_path()) && entry.provider == provider
    }

    /// Incrementally from the cached offset, or cold (head and tail windows).
    fn read(
        &self,
        dir: &Dir<'_>,
        listed: &Listed,
        file: &File,
        stat: Stat,
        cached: Option<&Entry>,
    ) -> Option<(Entry, u64)> {
        // Nothing is listed for a directory that could not be resolved.
        let Dir::Store(dir) = dir else { return None };
        let (mut entry, bytes) = match cached {
            Some(cached) => scan_incremental(file, stat, cached)?,
            None => {
                let job = Job {
                    provider: dir.store.provider,
                    path: listed.path.clone(),
                    session_id: listed.session_id.clone(),
                    store: dir.store.path.clone(),
                };
                scan_cold(&job, file, stat)?
            }
        };
        if entry.provider == Provider::Codex {
            entry.title = dir.names.get(&entry.session_id).cloned();
        }
        Some((entry, bytes))
    }
}

/// Brings `index` up to date with `stores`: new files are scanned cold (head and tail
/// windows), grown files incrementally, unchanged files reused; vanished files and files of
/// stores no longer listed drop out, while those below a directory that exists but cannot be
/// listed stay as they were, the directory being reported in [`RefreshStats::unreadable`] (R8).
/// Codex titles are the thread names of the store's `session_index.jsonl` files (every home
/// sharing it) as they are now, also for unchanged rollouts (R17). Never fails: unreadable
/// files are skipped.
pub fn refresh(
    index: &mut Index,
    stores: &[Store],
    progress: impl FnMut(Progress<'_>),
) -> RefreshStats {
    track(index, stores, None, progress)
}

/// [`refresh`], given also what each home gives as its store ([`resolve`]). The index
/// remembers the real path of each directory that resolved ([`Index::stores`]). For one that
/// could not be resolved, the entries of the store it was when it was last indexed stay as
/// they were, unless another account lists that store, and the directory is reported in
/// [`RefreshStats::unreadable`]; the entries of any other store that is not listed drop out as
/// usual (R8). [`refresh`] itself leaves what is remembered alone.
pub fn refresh_with(
    index: &mut Index,
    stores: &[Store],
    given: &[Given],
    progress: impl FnMut(Progress<'_>),
) -> RefreshStats {
    track(index, stores, Some(given), progress)
}

fn track(
    index: &mut Index,
    stores: &[Store],
    given: Option<&[Given]>,
    mut progress: impl FnMut(Progress<'_>),
) -> RefreshStats {
    // As it was before this refresh: where each directory that cannot be resolved now was.
    let remembered = index.stores.clone();
    let named = stores.iter().map(|store| {
        Dir::Store(Named {
            store,
            names: codex::thread_names(&store.thread_names),
        })
    });
    let unresolved = given.unwrap_or_default().iter().filter_map(|given| {
        let (path, error) = given.unresolved()?;
        let last = remembered.get(path).map(PathBuf::as_path);
        Some(Dir::Unresolved { given, error, last })
    });
    let dirs: Vec<Dir<'_>> = named.chain(unresolved).collect();
    let mut stats = tracking::refresh(&mut index.entries, &dirs, &Sessions, |p| {
        progress(Progress {
            done: p.done,
            total: p.total,
            bytes_read: p.bytes_read,
            entry: p.item,
        })
    });
    // Thread names change without the rollout changing.
    for dir in &dirs {
        if let Dir::Store(Named { store, names }) = dir
            && store.provider == Provider::Codex
        {
            for entry in index.entries.values_mut() {
                if entry.store == store.path {
                    entry.title = names.get(&entry.session_id).cloned();
                }
            }
        }
    }
    if let Some(given) = given {
        stats.remembered = tracking::remember(&mut index.stores, given);
    }
    stats
}

/// `<sessions>/**/rollout-*.jsonl` (R17): `sessions/YYYY/MM/DD/` in practice. Symlinked
/// directories below the store are not followed; non-UTF-8 names are skipped. What cannot be
/// listed or examined is the listing's to tell (R8).
pub(crate) fn list_rollouts(dir: &Path, listing: &mut Listing) {
    for item in listing.read_dir(dir) {
        if item.is_dir() {
            if item.name() != Some("archived_sessions") {
                list_rollouts(item.path(), listing);
            }
            continue;
        }
        let Some(id) = item.name().and_then(codex::rollout_id).map(str::to_string) else {
            continue;
        };
        listing.file(&item, id);
    }
}

/// `<store>/*/*.jsonl`, top level of each project directory only (R8). Non-UTF-8 names are
/// skipped; what cannot be listed or examined is the listing's to tell.
fn list_projects(store: &Path, listing: &mut Listing) {
    for project in listing.read_dir(store) {
        if !listing.is_dir(&project) {
            continue;
        }
        for file in listing.read_dir(project.path()) {
            let Some(session_id) = file
                .name()
                .and_then(|n| n.strip_suffix(".jsonl"))
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let session_id = session_id.to_string();
            listing.file(&file, session_id);
        }
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
