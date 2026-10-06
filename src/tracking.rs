//! Tracking the transcripts and rollouts below a set of directories with a cache: the
//! append-only reading of SPEC R8, which the session index and the token statistics (R20)
//! share.
//!
//! A cache has one adapter, a [`Files`]: how its directories are listed and how one file is
//! read. [`refresh`] owns the rest: which files are reused, read from where they were left or
//! read whole; opening a file and checking it again once open; the worker threads; dropping
//! what vanished; progress; and what a directory that cannot be listed means.

use std::collections::btree_map::Entry as Slot;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

/// Worker threads for reading files (IO bound).
const WORKERS: usize = 8;

/// Size, mtime and inode of a file, as listed or as opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stat {
    pub(crate) size: u64,
    /// Nanoseconds since the Unix epoch.
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

/// A file as listed: its path, the session id the path gives, and its stat.
#[derive(Debug, Clone)]
pub(crate) struct Listed {
    pub(crate) path: PathBuf,
    pub(crate) session_id: String,
    pub(crate) stat: Stat,
}

/// A directory that exists but could not be listed, or not to its end, or whose entries could
/// not be examined; or one to track whose real path could not be found (R8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable {
    /// The directory; as it was given, not a real path, when `unresolved`.
    pub path: PathBuf,
    /// The system's error, as text.
    pub error: String,
    /// Cached files below it, kept as they were last read. When `unresolved`: the cached
    /// files last read below the real path it had when it last resolved.
    pub kept: usize,
    /// The directory was one to track and its real path could not be found.
    pub unresolved: bool,
}

/// Whether `error` says that what was asked for is not there, rather than out of reach (R8).
fn gone(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// The real path of the directory at `path` (a store, a source): `Ok(None)` when there is
/// none (it does not exist, or is no directory); `Err` with the system's error as text when
/// that cannot be told, because the way to it cannot be searched or read: it may be there
/// (R8). An adapter then gives the directory to [`refresh`] all the same, and says so when
/// listing it ([`Listing::unresolved`]).
///
/// A cache remembers the real path of each directory it tracks, by the path it is given as
/// ([`Remembered`]), which is what tells which cached files were such a directory's.
pub(crate) fn real_dir(path: &Path) -> Result<Option<PathBuf>, String> {
    let found = fs::canonicalize(path).and_then(|real| Ok((fs::metadata(&real)?.is_dir(), real)));
    match found {
        Ok((true, real)) => Ok(Some(real)),
        Ok((false, _)) => Ok(None),
        Err(e) if gone(&e) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

impl fmt::Display for Unreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot read {}: {}", self.path.display(), self.error)
    }
}

/// An entry of a directory, as [`Listing::read_dir`] found it.
#[derive(Debug)]
pub(crate) struct Found {
    path: PathBuf,
    name: OsString,
    /// Of the entry itself: a symlink is not followed.
    kind: fs::FileType,
}

impl Found {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Its name; `None` when it is not UTF-8.
    pub(crate) fn name(&self) -> Option<&str> {
        self.name.to_str()
    }

    /// Whether the entry itself is a directory; a symlink to one is not.
    pub(crate) fn is_dir(&self) -> bool {
        self.kind.is_dir()
    }
}

/// What a cache remembers of the directories it tracks: the real path each was last found at,
/// by the path it is given as, which is the directory's whether or not it resolves. It tells
/// which cached files were read below a directory that cannot be resolved now, whatever else
/// of the same provider the cache holds (R8).
pub(crate) type Remembered = BTreeMap<PathBuf, PathBuf>;

/// A directory as it is given to be tracked, and where resolving it found it.
pub(crate) trait Resolved {
    /// The path it is given as.
    fn given(&self) -> &Path;
    /// Its real path; `None` when it could not be resolved ([`real_dir`] failed).
    fn real(&self) -> Option<&Path>;
}

/// Brings `remembered` up to date with `dirs`, the directories given to this refresh: one
/// that resolved is at its real path, one that could not be resolved stays where it was last
/// found, and any other is forgotten: it does not exist, or is no longer tracked. Returns
/// whether that changed anything.
pub(crate) fn remember<R: Resolved>(remembered: &mut Remembered, dirs: &[R]) -> bool {
    let now: Remembered = dirs
        .iter()
        .filter_map(|dir| {
            let real = dir
                .real()
                .or_else(|| Some(remembered.get(dir.given())?.as_path()))?;
            Some((dir.given().to_path_buf(), real.to_path_buf()))
        })
        .collect();
    let changed = now != *remembered;
    *remembered = now;
    changed
}

/// What listing a directory found. An adapter lists through it alone (it reads no directory
/// and examines no entry itself), so that what a failure means is decided here (R8): a
/// directory or an entry that does not exist is not there; a directory that exists but cannot
/// be listed, or whose entries cannot be examined (it can be read but not searched), is
/// recorded, and [`refresh`] keeps what the cache has below it. A single file out of reach (a
/// symlink whose target is) is left out like one that cannot be opened, and not recorded.
#[derive(Debug, Default)]
pub(crate) struct Listing {
    files: Vec<Listed>,
    unreadable: Vec<Unreadable>,
    /// The directory being listed is one whose real path could not be found.
    unresolved: bool,
}

impl Listing {
    /// The entries of `dir`: none when it does not exist; those read and examined when it
    /// cannot be listed, or not to its end, or an entry's type cannot be told, which is
    /// recorded.
    pub(crate) fn read_dir(&mut self, dir: &Path) -> Vec<Found> {
        let mut found = Vec::new();
        let listing = match fs::read_dir(dir) {
            Ok(listing) => listing,
            Err(e) => {
                self.failed(dir, &e);
                return found;
            }
        };
        for entry in listing {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    self.failed(dir, &e);
                    break;
                }
            };
            match entry.file_type() {
                Ok(kind) => found.push(Found {
                    path: entry.path(),
                    name: entry.file_name(),
                    kind,
                }),
                Err(e) => self.failed(dir, &e),
            }
        }
        found
    }

    /// Whether `entry` is a directory, following symlinks. `false` when that cannot be told,
    /// which is recorded unless it is gone: a symlink whose target is out of reach may be a
    /// directory, and nothing is known of what is below it.
    pub(crate) fn is_dir(&mut self, entry: &Found) -> bool {
        match fs::metadata(&entry.path) {
            Ok(meta) => meta.is_dir(),
            Err(e) => {
                if self.unexamined(entry, &e) {
                    self.failed(&entry.path, &e);
                }
                false
            }
        }
    }

    /// Lists `entry` for `session_id` if it is a regular file, following symlinks. Anything
    /// else is skipped, and so is an entry that cannot be examined: its directory is recorded
    /// when that is why, while a symlink whose target alone is out of reach is one file that
    /// cannot be read, left out like one that cannot be opened.
    pub(crate) fn file(&mut self, entry: &Found, session_id: String) {
        match fs::metadata(&entry.path) {
            Ok(meta) if meta.is_file() => self.push(Listed {
                path: entry.path.clone(),
                session_id,
                stat: Stat::of(&meta),
            }),
            Ok(_) => {}
            Err(e) => {
                self.unexamined(entry, &e);
            }
        }
    }

    pub(crate) fn push(&mut self, listed: Listed) {
        self.files.push(listed);
    }

    /// The directory being listed is `path` as it was given, and its real path could not be
    /// found for `error` ([`real_dir`]): nothing of it can be listed, and the cached files do
    /// not lie below `path`. [`refresh`] then keeps the cached files of no directory that
    /// could be listed which [`Files::listed_under`] says are its: those read below the real
    /// path it had when it last resolved ([`Remembered`]).
    pub(crate) fn unresolved(&mut self, path: &Path, error: &str) {
        self.unresolved = true;
        self.unreadable.push(Unreadable {
            path: path.to_path_buf(),
            error: error.to_string(),
            kept: 0,
            unresolved: true,
        });
    }

    /// The files listed, for a caller that keeps no cache.
    pub(crate) fn into_files(self) -> Vec<Listed> {
        self.files
    }

    /// `entry` was listed but cannot be examined, following symlinks. Either its directory
    /// cannot be searched, so that nothing is known about the files of that directory, which
    /// is recorded; or, returning `true`, `entry` is a symlink that is there and whose target
    /// alone is out of reach (or gone), which is for the caller to judge.
    fn unexamined(&mut self, entry: &Found, error: &io::Error) -> bool {
        // The link itself can be examined exactly when its directory can be searched.
        let link = entry
            .kind
            .is_symlink()
            .then(|| fs::symlink_metadata(&entry.path));
        match (link, entry.path.parent()) {
            (Some(Ok(_)), _) | (_, None) => true,
            (Some(Err(e)), Some(dir)) => {
                self.failed(dir, &e);
                false
            }
            (None, Some(dir)) => {
                self.failed(dir, error);
                false
            }
        }
    }

    /// `path` could not be read or examined: it is gone (nothing is below it), or unreadable,
    /// which is recorded once, and not below a directory already recorded (what cannot be
    /// searched fails again for each directory below it).
    fn failed(&mut self, path: &Path, error: &io::Error) {
        if gone(error) || self.unreadable.iter().any(|u| path.starts_with(&u.path)) {
            return;
        }
        self.unreadable.retain(|u| !u.path.starts_with(path));
        self.unreadable.push(Unreadable {
            path: path.to_path_buf(),
            error: error.to_string(),
            kept: 0,
            unresolved: false,
        });
    }
}

/// The adapter of one cache: how the directories it tracks (`D`: a session store, a source) are
/// listed, and how one of their files is read.
pub(crate) trait Files<D>: Sync {
    /// What the cache keeps of one file.
    type Tracked: Clone + Send + Sync;

    /// Lists the files of `dir`, in any order. A path listed under two directories belongs to
    /// the first.
    fn list(&self, dir: &D, listing: &mut Listing);

    /// Size, mtime and inode of the file when `item` was read.
    fn stat(item: &Self::Tracked) -> Stat;

    /// Whether `item` was read as a file of `dir`. One read as another directory's, or another
    /// provider's, is read whole again; and only the items of `dir` are kept when a directory
    /// listed for it cannot be read. For a `dir` whose real path could not be found
    /// ([`Listing::unresolved`]): whether `item` was read as a file of it when it last
    /// resolved, by the real path remembered for it ([`Remembered`]); of none when nothing
    /// is remembered.
    fn listed_under(item: &Self::Tracked, dir: &D) -> bool;

    /// Reads `listed`, open as `file` and `stat` as it is now: on from `cached` when given
    /// (the file is a grown version of what `cached` was read from), else whole. Returns what
    /// to cache, whose [`Files::stat`] must be `stat`, and the bytes read; `None` when the
    /// file cannot be read. Called on worker threads.
    fn read(
        &self,
        dir: &D,
        listed: &Listed,
        file: &File,
        stat: Stat,
        cached: Option<&Self::Tracked>,
    ) -> Option<(Self::Tracked, u64)>;
}

/// Reported by [`refresh`] after listing (`item: None`) and after each file that had to be
/// read (`item: None` when it could not be).
#[derive(Debug)]
pub(crate) struct Progress<'a, T> {
    /// Files read so far / files that need reading (unchanged files are not counted).
    pub(crate) done: usize,
    pub(crate) total: usize,
    pub(crate) bytes_read: u64,
    pub(crate) item: Option<&'a T>,
}

/// What a refresh did. It left the cache as it was exactly when `reused + kept() == files`
/// (nothing was read into it), `removed == 0` (nothing was dropped from it) and `remembered`
/// is false.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshStats {
    /// Files in the cache after the refresh.
    pub files: usize,
    /// Listed and unchanged: not read.
    pub reused: usize,
    /// Listed as grown: to be read on from where the cache left it.
    pub incremental: usize,
    /// Listed as new or otherwise changed: to be read whole. Like `incremental`, counted when
    /// listed, whether or not the read then succeeded.
    pub cold: usize,
    /// Cached files dropped: no longer listed, or listed as changed and no longer readable.
    pub removed: usize,
    /// What the cache remembers of its directories ([`Remembered`]) changed. Set by the
    /// cache's own refresh, which keeps that beside the files.
    pub remembered: bool,
    pub bytes_read: u64,
    /// The directories that exist but could not be listed, in listing order: the refresh is
    /// incomplete, and the cached files below them were kept as they were (R8). Empty after a
    /// complete refresh.
    pub unreadable: Vec<Unreadable>,
}

impl RefreshStats {
    /// Cached files kept without being listed, below the directories of `unreadable`.
    pub fn kept(&self) -> usize {
        self.unreadable.iter().map(|u| u.kept).sum()
    }

    /// The directories that could not be read, as one line for a status bar; `None` after a
    /// complete refresh.
    pub fn incomplete(&self) -> Option<String> {
        let first = self.unreadable.first()?;
        Some(match self.unreadable.len() {
            1 => format!("incomplete: {first}"),
            n => format!(
                "incomplete: cannot read {n} directories, first {}: {}",
                first.path.display(),
                first.error
            ),
        })
    }
}

/// How a listed file is brought up to date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    /// Unchanged: not read.
    Reused,
    /// Grown: read from where the cache left it.
    Incremental,
    /// Anything else: read whole.
    Cold,
}

/// R8, from the stat of the file when it was cached and as listed now: the same inode with the
/// same size and mtime is unchanged; one that grew, its mtime not earlier, is read
/// incrementally; a file that shrank, kept its size with another mtime, went back in time or
/// was replaced (another inode) is read whole, like one not cached.
fn decide(cached: Option<Stat>, listed: Stat) -> Decision {
    match cached {
        Some(c) if c.ino != listed.ino => Decision::Cold,
        Some(c) if c.size == listed.size && c.mtime_ns == listed.mtime_ns => Decision::Reused,
        Some(c) if listed.size > c.size && listed.mtime_ns >= c.mtime_ns => Decision::Incremental,
        _ => Decision::Cold,
    }
}

/// Whether the file as `opened` is still a grown version of the one `cached`: the same inode,
/// larger, its mtime not earlier (R8).
fn grown(cached: Stat, opened: Stat) -> bool {
    opened.size > cached.size && opened.ino == cached.ino && opened.mtime_ns >= cached.mtime_ns
}

/// A file that has to be read.
struct Job<T> {
    /// Index into the directories given to [`refresh`].
    dir: usize,
    listed: Listed,
    /// Present when the listing decided on an incremental read; `None` means whole.
    cached: Option<T>,
}

/// Brings `cache` (keyed by path) up to date with the files of `dirs` as `files` lists them:
/// new files are read whole, grown ones incrementally, unchanged ones reused ([`decide`]);
/// files that vanished and files of directories no longer given drop out. Where a directory
/// listed for one of `dirs` exists but could not be read, the cached files of that one below
/// it are kept as they are, and the directory is reported ([`RefreshStats::unreadable`]).
/// Where one of `dirs` is a directory whose real path could not be found, the cached files
/// that were its when it last resolved, and are of none of the directories listed now, are
/// kept, and it is reported likewise. `progress` is called on this thread. Never fails: a
/// file that cannot be read is dropped.
pub(crate) fn refresh<D: Sync, F: Files<D>>(
    cache: &mut BTreeMap<PathBuf, F::Tracked>,
    dirs: &[D],
    files: &F,
    mut progress: impl FnMut(Progress<'_, F::Tracked>),
) -> RefreshStats {
    let mut stats = RefreshStats::default();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut jobs: Vec<Job<F::Tracked>> = Vec::new();
    // Each with the index of the directory it was met listing.
    let mut unreadable: Vec<(usize, Unreadable)> = Vec::new();
    // Whether each directory was given by its real path: what it lists is told by path.
    let mut resolved: Vec<bool> = Vec::with_capacity(dirs.len());
    for (dir_index, dir) in dirs.iter().enumerate() {
        let mut listing = Listing::default();
        files.list(dir, &mut listing);
        resolved.push(!listing.unresolved);
        unreadable.extend(listing.unreadable.drain(..).map(|u| (dir_index, u)));
        for listed in listing.files {
            if !seen.insert(listed.path.clone()) {
                continue;
            }
            let cached = cache
                .get(&listed.path)
                .filter(|item| F::listed_under(item, dir));
            let cached = match decide(cached.map(F::stat), listed.stat) {
                Decision::Reused => {
                    stats.reused += 1;
                    continue;
                }
                Decision::Incremental => {
                    stats.incremental += 1;
                    cached.cloned()
                }
                Decision::Cold => {
                    stats.cold += 1;
                    None
                }
            };
            jobs.push(Job {
                dir: dir_index,
                listed,
                cached,
            });
        }
    }
    // A file not listed vanished, unless it is below a directory that could not be read and
    // was a file of the directory given that this one was met listing: nothing is known about
    // that one. A failure says nothing for the files of a directory no longer given, and none
    // for a file at the very path that failed: what the cache holds there was a file, not a
    // directory with files below it.
    //
    // Nor is anything known about the files of a directory whose real path could not be found,
    // and their paths are not below the path it was given as: its files are those the adapter
    // says were its when it last resolved. One of them that is also a file of a directory
    // given by its real path now (the two share a real path) is not kept for that: there it
    // was listed for, and it vanished.
    cache.retain(|path, item| {
        if seen.contains(path) {
            return true;
        }
        let of = |dir: usize| F::listed_under(item, &dirs[dir]);
        let below = unreadable.iter().position(|(dir, u)| {
            !u.unresolved && path != &u.path && path.starts_with(&u.path) && of(*dir)
        });
        let kept = below.or_else(|| {
            let last_of = |(dir, u): &(usize, Unreadable)| u.unresolved && of(*dir);
            let unresolved = unreadable.iter().position(last_of)?;
            let listed = (0..dirs.len()).any(|dir| resolved[dir] && of(dir));
            (!listed).then_some(unresolved)
        });
        match kept {
            Some(i) => {
                unreadable[i].1.kept += 1;
                true
            }
            None => {
                stats.removed += 1;
                false
            }
        }
    });
    stats.unreadable = unreadable.into_iter().map(|(_, u)| u).collect();

    let total = jobs.len();
    progress(Progress {
        done: 0,
        total,
        bytes_read: 0,
        item: None,
    });
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<(usize, Option<(F::Tracked, u64)>)>();
    thread::scope(|scope| {
        for _ in 0..WORKERS.min(total) {
            let tx = tx.clone();
            let (jobs, next) = (&jobs, &next);
            scope.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(i) else { break };
                    if tx.send((i, read(files, &dirs[job.dir], job))).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        for (done, (i, result)) in rx.into_iter().enumerate() {
            let path = &jobs[i].listed.path;
            let item = match result {
                Some((item, bytes)) => {
                    stats.bytes_read += bytes;
                    Some(&*match cache.entry(path.clone()) {
                        Slot::Occupied(mut slot) => {
                            slot.insert(item);
                            slot.into_mut()
                        }
                        Slot::Vacant(slot) => slot.insert(item),
                    })
                }
                // Vanished or unreadable since listing: drop it rather than keep it stale. What
                // the cache had of it is removed like a file that vanished.
                None => {
                    if cache.remove(path).is_some() {
                        stats.removed += 1;
                    }
                    None
                }
            };
            progress(Progress {
                done: done + 1,
                total,
                bytes_read: stats.bytes_read,
                item,
            });
        }
    });
    stats.files = cache.len();
    stats
}

/// Opens and reads the file of `job`; `None` if it cannot be opened or read.
fn read<D, F: Files<D>>(files: &F, dir: &D, job: &Job<F::Tracked>) -> Option<(F::Tracked, u64)> {
    let file = File::open(&job.listed.path).ok()?;
    // Stat the open file: it may have changed since listing, and what is read must match. The
    // listing's decision can only be downgraded here (to a whole read, if the file is no
    // longer a grown version of the cached one: replaced, cut back, or gone back in time),
    // never upgraded.
    let stat = Stat::of(&file.metadata().ok()?);
    let cached = job
        .cached
        .as_ref()
        .filter(|cached| grown(F::stat(cached), stat));
    files.read(dir, &job.listed, &file, stat, cached)
}

#[cfg(test)]
mod tests {
    //! R8's rules, driven by a fake adapter over files of a few bytes.

    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::transcript::read_at;

    /// How the fake read a file.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Read {
        Whole,
        /// On from this offset: the size of the cached item.
        From(u64),
    }

    /// What the fake caches of one file.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Item {
        stat: Stat,
        dir: PathBuf,
        provider: &'static str,
        read: Read,
        /// Everything read of the file so far.
        text: String,
    }

    /// A directory as the fake lists it.
    #[derive(Default)]
    struct Dir {
        path: PathBuf,
        provider: &'static str,
        /// Files, each listed with its real stat or with the one given.
        files: Vec<(PathBuf, Option<Stat>)>,
        /// Directories that fail to list, and how.
        failing: Vec<(PathBuf, io::ErrorKind)>,
        /// Also every file found below `path`, listed for real.
        walk: bool,
        /// `path` is not a real path: the directory could not be resolved, for this error.
        unresolved: Option<&'static str>,
        /// The real path it had when it last resolved, if that is remembered.
        last: Option<PathBuf>,
    }

    impl Dir {
        fn of(path: &Path, files: &[&Path]) -> Dir {
            Dir {
                path: path.to_path_buf(),
                files: files.iter().map(|f| (f.to_path_buf(), None)).collect(),
                ..Dir::default()
            }
        }

        /// Lists `file` as `stat`, whatever it is on disk.
        fn claiming(path: &Path, file: &Path, stat: Stat) -> Dir {
            Dir {
                path: path.to_path_buf(),
                files: vec![(file.to_path_buf(), Some(stat))],
                ..Dir::default()
            }
        }

        /// Lists what is below `path`, through the listing.
        fn walking(path: &Path) -> Dir {
            Dir {
                path: path.to_path_buf(),
                walk: true,
                ..Dir::default()
            }
        }

        fn failing(mut self, dir: &Path, kind: io::ErrorKind) -> Dir {
            self.failing.push((dir.to_path_buf(), kind));
            self
        }

        fn provider(mut self, provider: &'static str) -> Dir {
            self.provider = provider;
            self
        }

        /// A directory given as `path` whose real path could not be found; it was `last`
        /// when it last resolved, if that is remembered.
        fn unresolved(path: &Path, last: Option<&Path>) -> Dir {
            Dir {
                path: path.to_path_buf(),
                unresolved: Some("out of reach"),
                last: last.map(Path::to_path_buf),
                ..Dir::default()
            }
        }
    }

    /// Every file at any depth below `dir`.
    fn walk(dir: &Path, listing: &mut Listing) {
        for entry in listing.read_dir(dir) {
            if entry.is_dir() {
                walk(entry.path(), listing);
            } else if let Some(name) = entry.name() {
                let id = name.to_string();
                listing.file(&entry, id);
            }
        }
    }

    struct Fake;

    impl Files<Dir> for Fake {
        type Tracked = Item;

        fn list(&self, dir: &Dir, listing: &mut Listing) {
            if let Some(error) = dir.unresolved {
                listing.unresolved(&dir.path, error);
            }
            for (path, kind) in &dir.failing {
                listing.failed(path, &io::Error::from(*kind));
            }
            for (path, claimed) in &dir.files {
                let real = fs::metadata(path).ok().filter(|m| m.is_file());
                if let Some(stat) = claimed.or(real.map(|m| Stat::of(&m))) {
                    listing.push(Listed {
                        path: path.clone(),
                        session_id: path.file_name().unwrap().to_string_lossy().into_owned(),
                        stat,
                    });
                }
            }
            if dir.walk {
                walk(&dir.path, listing);
            }
        }

        fn stat(item: &Item) -> Stat {
            item.stat
        }

        fn listed_under(item: &Item, dir: &Dir) -> bool {
            let real = match dir.unresolved {
                Some(_) => dir.last.as_ref(),
                None => Some(&dir.path),
            };
            item.provider == dir.provider && real == Some(&item.dir)
        }

        fn read(
            &self,
            dir: &Dir,
            _listed: &Listed,
            file: &File,
            stat: Stat,
            cached: Option<&Item>,
        ) -> Option<(Item, u64)> {
            let from = cached.map_or(0, |c| c.stat.size);
            let bytes = read_at(file, from, stat.size - from).ok()?;
            let mut text = cached.map_or(String::new(), |c| c.text.clone());
            text.push_str(&String::from_utf8_lossy(&bytes));
            let item = Item {
                stat,
                dir: dir.path.clone(),
                provider: dir.provider,
                read: cached.map_or(Read::Whole, |_| Read::From(from)),
                text,
            };
            Some((item, bytes.len() as u64))
        }
    }

    type Cache = BTreeMap<PathBuf, Item>;

    fn run(cache: &mut Cache, dirs: &[Dir]) -> RefreshStats {
        refresh(cache, dirs, &Fake, |_| {})
    }

    /// `(reused, incremental, cold, removed)`.
    fn counts(s: &RefreshStats) -> (usize, usize, usize, usize) {
        (s.reused, s.incremental, s.cold, s.removed)
    }

    /// A temporary directory (realpath) with the directory `a` in it.
    fn sandbox() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().canonicalize().unwrap().join("a");
        fs::create_dir(&a).unwrap();
        (tmp, a)
    }

    fn write(path: &Path, text: &str) {
        fs::write(path, text).unwrap();
    }

    fn append(path: &Path, text: &str) {
        use std::io::Write;
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    fn stat(path: &Path) -> Stat {
        Stat::of(&fs::metadata(path).unwrap())
    }

    fn mtime(path: &Path) -> SystemTime {
        fs::metadata(path).unwrap().modified().unwrap()
    }

    fn set_mtime(path: &Path, to: SystemTime) {
        let file = File::options().write(true).open(path).unwrap();
        file.set_modified(to).unwrap();
    }

    fn chmod(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    const AT: Stat = Stat {
        size: 100,
        mtime_ns: 5_000,
        ino: 7,
    };

    /// R8: the whole decision, from the two stats alone.
    #[test]
    fn the_decision_follows_size_mtime_and_inode() {
        let with = |size: u64, mtime_ns: i128, ino: u64| Stat {
            size,
            mtime_ns,
            ino,
        };
        assert_eq!(decide(None, AT), Decision::Cold, "not cached");
        assert_eq!(decide(Some(AT), AT), Decision::Reused);
        for (listed, decision, what) in [
            (
                with(101, 5_000, 7),
                Decision::Incremental,
                "grown, same mtime",
            ),
            (with(101, 5_001, 7), Decision::Incremental, "grown, later"),
            (
                with(101, 4_999, 7),
                Decision::Cold,
                "grown, mtime went back",
            ),
            (with(99, 5_001, 7), Decision::Cold, "shrunk"),
            (with(99, 5_000, 7), Decision::Cold, "shrunk, same mtime"),
            (with(100, 5_001, 7), Decision::Cold, "same size, later"),
            (with(100, 4_999, 7), Decision::Cold, "same size, earlier"),
            (
                with(100, 5_000, 8),
                Decision::Cold,
                "replaced, looks unchanged",
            ),
            (with(101, 5_001, 8), Decision::Cold, "replaced, looks grown"),
            (with(0, 5_001, 7), Decision::Cold, "emptied"),
        ] {
            assert_eq!(decide(Some(AT), listed), decision, "{what}");
        }
    }

    /// R8: once open, a file is read incrementally only if it still is a grown version.
    #[test]
    fn an_open_file_is_incremental_only_while_grown() {
        assert!(grown(AT, Stat { size: 101, ..AT }));
        assert!(grown(
            AT,
            Stat {
                size: 101,
                mtime_ns: AT.mtime_ns + 1,
                ..AT
            }
        ));
        // Larger, but written before what was cached: not the same file going on.
        assert!(!grown(
            AT,
            Stat {
                size: 101,
                mtime_ns: AT.mtime_ns - 1,
                ..AT
            }
        ));
        assert!(!grown(AT, AT));
        assert!(!grown(AT, Stat { size: 99, ..AT }));
        assert!(!grown(
            AT,
            Stat {
                size: 101,
                ino: 8,
                ..AT
            }
        ));
    }

    /// R8: new files are read whole, unchanged ones not at all, grown ones from where they
    /// were left.
    #[test]
    fn new_unchanged_and_grown_files() {
        let (_tmp, a) = sandbox();
        let f = a.join("f");
        write(&f, "one\n");
        let dirs = [Dir::of(&a, &[&f])];
        let mut cache = Cache::new();

        let s = run(&mut cache, &dirs);
        assert_eq!((counts(&s), s.files, s.bytes_read), ((0, 0, 1, 0), 1, 4));
        assert_eq!(
            (cache[&f].read, cache[&f].text.as_str()),
            (Read::Whole, "one\n")
        );
        assert_eq!(cache[&f].stat, stat(&f));

        let before = cache.clone();
        let s = run(&mut cache, &dirs);
        assert_eq!((counts(&s), s.files, s.bytes_read), ((1, 0, 0, 0), 1, 0));
        assert_eq!(cache, before);

        append(&f, "two\n");
        let s = run(&mut cache, &dirs);
        assert_eq!((counts(&s), s.files, s.bytes_read), ((0, 1, 0, 0), 1, 4));
        assert_eq!(
            (cache[&f].read, cache[&f].text.as_str()),
            (Read::From(4), "one\ntwo\n")
        );
        assert_eq!(cache[&f].stat, stat(&f));
        assert!(s.unreadable.is_empty());
    }

    /// R8: a file that shrank, kept its size with another mtime, grew with an earlier mtime or
    /// was replaced is read whole.
    #[test]
    fn shrunk_same_size_older_and_replaced_files_are_read_whole() {
        let (_tmp, a) = sandbox();
        let f = a.join("f");
        let dirs = [Dir::of(&a, &[&f])];
        let whole = |cache: &mut Cache, what: &str| {
            let s = run(cache, &dirs);
            assert_eq!(counts(&s), (0, 0, 1, 0), "{what}");
            assert_eq!(cache[&f].read, Read::Whole, "{what}");
            cache[&f].text.clone()
        };
        write(&f, "a long first line\n");
        let mut cache = Cache::new();
        run(&mut cache, &dirs);
        let first = mtime(&f);

        write(&f, "short\n");
        set_mtime(&f, first + Duration::from_secs(1));
        assert_eq!(whole(&mut cache, "shrunk"), "short\n");

        write(&f, "other\n");
        set_mtime(&f, first + Duration::from_secs(2));
        assert_eq!(whole(&mut cache, "same size, later"), "other\n");

        write(&f, "again\n");
        set_mtime(&f, first - Duration::from_secs(60));
        assert_eq!(whole(&mut cache, "same size, earlier"), "again\n");

        write(&f, "AGAIN\nand more\n");
        set_mtime(&f, first - Duration::from_secs(120));
        assert_eq!(whole(&mut cache, "grown, earlier"), "AGAIN\nand more\n");

        let other = a.join("other");
        write(&other, "REPLACED and longer than before\n");
        set_mtime(&other, first + Duration::from_secs(600));
        fs::rename(&other, &f).unwrap();
        assert_eq!(
            whole(&mut cache, "replaced"),
            "REPLACED and longer than before\n"
        );
    }

    /// R8: a file cached as another directory's is read whole, though it looks grown.
    #[test]
    fn a_file_cached_under_another_directory_is_read_whole() {
        let (_tmp, a) = sandbox();
        let f = a.join("f");
        write(&f, "one\n");
        let mut cache = Cache::new();
        run(&mut cache, &[Dir::of(&a, &[&f])]);
        append(&f, "two\n");
        let elsewhere = a.parent().unwrap().join("elsewhere");
        let s = run(&mut cache, &[Dir::of(&elsewhere, &[&f])]);
        assert_eq!(counts(&s), (0, 0, 1, 0));
        assert_eq!((cache[&f].read, &cache[&f].dir), (Read::Whole, &elsewhere));
    }

    /// R8: the listing's decision is checked against the open file, and only downgraded.
    #[test]
    fn a_file_no_longer_grown_when_opened_is_read_whole() {
        let (_tmp, a) = sandbox();
        let f = a.join("f");
        write(&f, "one\n");
        let mut cache = Cache::new();
        run(&mut cache, &[Dir::of(&a, &[&f])]);
        let cached = cache[&f].stat;

        // Listed as grown, the same size as cached once open (it was cut back in between).
        let listed = Stat {
            size: cached.size + 4,
            ..cached
        };
        let s = run(&mut cache, &[Dir::claiming(&a, &f, listed)]);
        assert_eq!(counts(&s), (0, 1, 0, 0), "the listing decided");
        assert_eq!(
            (cache[&f].read, cache[&f].text.as_str()),
            (Read::Whole, "one\n")
        );
        assert_eq!(cache[&f].stat, stat(&f), "cached as opened, not as listed");

        // Listed as grown, another file once open (replaced in between), larger at that.
        let other = a.join("other");
        write(&other, "another, longer\n");
        fs::rename(&other, &f).unwrap();
        let s = run(&mut cache, &[Dir::claiming(&a, &f, listed)]);
        assert_eq!(counts(&s), (0, 1, 0, 0));
        assert_eq!(
            (cache[&f].read, cache[&f].text.as_str()),
            (Read::Whole, "another, longer\n")
        );

        // Listed as grown, rewritten once open: the same inode and larger, but with an mtime
        // before the cached one (restored by a sync, say). Its start is not what was cached.
        cache.clear();
        write(&f, "old\n");
        run(&mut cache, &[Dir::of(&a, &[&f])]);
        let cached = cache[&f].stat;
        write(&f, "NEW and longer\n");
        set_mtime(&f, mtime(&f) - Duration::from_secs(3600));
        assert_eq!(stat(&f).ino, cached.ino);
        let listed = Stat {
            size: cached.size + 4,
            ..cached
        };
        let s = run(&mut cache, &[Dir::claiming(&a, &f, listed)]);
        assert_eq!(counts(&s), (0, 1, 0, 0));
        assert_eq!(
            (cache[&f].read, cache[&f].text.as_str()),
            (Read::Whole, "NEW and longer\n")
        );
        assert_eq!(cache[&f].stat, stat(&f));
        assert_eq!(
            counts(&run(&mut cache, &[Dir::of(&a, &[&f])])),
            (1, 0, 0, 0)
        );

        // Listed as new while cached and unchanged: never upgraded to a reuse.
        cache.clear();
        run(&mut cache, &[Dir::of(&a, &[&f])]);
        let s = run(
            &mut cache,
            &[Dir::claiming(&a, &f, Stat { ino: 1, ..listed })],
        );
        assert_eq!(counts(&s), (0, 0, 1, 0));
        assert_eq!(s.bytes_read, 15);
    }

    /// R8: a file listed but gone when opened drops out, and is still counted as done. What
    /// the cache had of it is removed, and counted as removed: the cache changed.
    #[test]
    fn a_file_gone_when_opened_drops_out() {
        let (_tmp, a) = sandbox();
        let (f, gone) = (a.join("f"), a.join("gone"));
        write(&f, "one\n");
        write(&gone, "one\n");
        let mut cache = Cache::new();
        run(&mut cache, &[Dir::of(&a, &[&f, &gone])]);
        let listed = Stat {
            size: 99,
            ..cache[&gone].stat
        };
        fs::remove_file(&gone).unwrap();

        let mut seen = Vec::new();
        let dirs = [Dir {
            path: a.clone(),
            files: vec![(f.clone(), None), (gone.clone(), Some(listed))],
            ..Dir::default()
        }];
        let s = refresh(&mut cache, &dirs, &Fake, |p| {
            seen.push((p.done, p.total, p.item.is_some()))
        });
        assert_eq!(seen, [(0, 1, false), (1, 1, false)]);
        assert_eq!((counts(&s), s.files), ((1, 1, 0, 1), 1));
        assert_eq!(cache.keys().collect::<Vec<_>>(), [&f]);

        // Listed, never cached and gone when opened: nothing was there to remove.
        let before = cache.clone();
        let s = run(&mut cache, &dirs);
        assert_eq!((counts(&s), s.files), ((1, 0, 1, 0), 1));
        assert_eq!(cache, before);
    }

    /// R8: files that vanished and files of directories no longer given drop out.
    #[test]
    fn vanished_files_and_dropped_directories_drop_out() {
        let (_tmp, a) = sandbox();
        let b = a.parent().unwrap().join("b");
        fs::create_dir(&b).unwrap();
        let (f, g, h) = (a.join("f"), a.join("g"), b.join("h"));
        for file in [&f, &g, &h] {
            write(file, "x\n");
        }
        let mut cache = Cache::new();
        let s = run(&mut cache, &[Dir::of(&a, &[&f, &g]), Dir::of(&b, &[&h])]);
        assert_eq!((counts(&s), s.files), ((0, 0, 3, 0), 3));

        fs::remove_file(&g).unwrap();
        let s = run(&mut cache, &[Dir::of(&a, &[&f, &g]), Dir::of(&b, &[&h])]);
        assert_eq!((counts(&s), s.files), ((2, 0, 0, 1), 2));
        assert!(!cache.contains_key(&g));

        let s = run(&mut cache, &[Dir::of(&a, &[&f])]);
        assert_eq!((counts(&s), s.files), ((1, 0, 0, 1), 1));
        let s = run(&mut cache, &[]);
        assert_eq!((counts(&s), s.files), ((0, 0, 0, 1), 0));
        assert!(cache.is_empty());
    }

    /// A path two directories list is read once, as the first one's.
    #[test]
    fn a_path_listed_twice_belongs_to_the_first_directory() {
        let (_tmp, a) = sandbox();
        let f = a.join("f");
        write(&f, "x\n");
        let second = a.parent().unwrap().join("second");
        let dirs = [Dir::of(&a, &[&f, &f]), Dir::of(&second, &[&f])];
        let mut cache = Cache::new();
        let s = run(&mut cache, &dirs);
        assert_eq!((counts(&s), s.files, s.bytes_read), ((0, 0, 1, 0), 1, 2));
        assert_eq!(cache[&f].dir, a);
        assert_eq!(counts(&run(&mut cache, &dirs)), (1, 0, 0, 0));
    }

    /// Progress: once after listing, then once per file read, on more files than workers.
    #[test]
    fn progress_counts_the_files_that_need_reading() {
        let (_tmp, a) = sandbox();
        let files: Vec<PathBuf> = (0..3 * WORKERS)
            .map(|i| a.join(format!("f{i:02}")))
            .collect();
        for file in &files {
            write(file, "abc\n");
        }
        let listed: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();
        let dirs = [Dir::of(&a, &listed)];
        // Four of them are cached and grown, the others new.
        let mut cache = Cache::new();
        run(&mut cache, &[Dir::of(&a, &listed[..4])]);
        for file in &files[..4] {
            append(file, "more\n");
        }

        let total = files.len();
        let mut seen = Vec::new();
        let mut read = BTreeSet::new();
        let s = refresh(&mut cache, &dirs, &Fake, |p| {
            seen.push((p.done, p.total, p.bytes_read));
            read.extend(p.item.map(|item| item.text.clone()));
            assert_eq!(p.item.is_none(), p.done == 0);
        });
        assert_eq!(seen.len(), total + 1);
        assert_eq!(seen[0], (0, total, 0));
        assert!(
            seen.iter()
                .enumerate()
                .all(|(i, p)| (p.0, p.1) == (i, total))
        );
        assert!(seen.windows(2).all(|w| w[0].2 < w[1].2), "{seen:?}");
        assert_eq!(seen[total].2, s.bytes_read);
        assert_eq!((counts(&s), s.files), ((0, 4, total - 4, 0), total));
        assert_eq!(s.bytes_read, (4 * 5 + (total - 4) * 4) as u64);
        assert_eq!(read.len(), 2, "{read:?}");
        assert_eq!(cache.len(), total);
    }

    /// R8, review #10: what the cache has below a directory that exists but cannot be listed
    /// is kept as it is, and the directory reported; everything else goes on as usual.
    #[test]
    fn files_below_an_unreadable_directory_are_kept() {
        let (_tmp, a) = sandbox();
        let b = a.parent().unwrap().join("b");
        let sub = a.join("sub");
        fs::create_dir(&b).unwrap();
        fs::create_dir(&sub).unwrap();
        let (f, deep, g, h) = (a.join("f"), sub.join("deep"), b.join("g"), b.join("h"));
        for file in [&f, &deep, &g, &h] {
            write(file, "x\n");
        }
        let mut cache = Cache::new();
        run(
            &mut cache,
            &[Dir::of(&a, &[&f, &deep]), Dir::of(&b, &[&g, &h])],
        );
        let before = cache.clone();

        // `a` cannot be listed at all; in `b`, `h` vanished and `g` grew.
        fs::remove_file(&h).unwrap();
        append(&g, "y\n");
        let denied = io::ErrorKind::PermissionDenied;
        let dirs = [Dir::of(&a, &[]).failing(&a, denied), Dir::of(&b, &[&g, &h])];
        let s = run(&mut cache, &dirs);
        assert_eq!((counts(&s), s.files, s.kept()), ((0, 1, 0, 1), 3, 2));
        assert_eq!(
            s.unreadable,
            [Unreadable {
                path: a.clone(),
                error: io::Error::from(denied).to_string(),
                kept: 2,
                unresolved: false,
            }]
        );
        assert_eq!((&cache[&f], &cache[&deep]), (&before[&f], &before[&deep]));
        assert_eq!(cache[&g].text, "x\ny\n");
        assert!(!cache.contains_key(&h));

        // Only `sub` cannot be listed: `f`, beside it, vanished and is dropped.
        fs::remove_file(&f).unwrap();
        let dirs = [
            Dir::of(&a, &[&f]).failing(&sub, io::ErrorKind::Other),
            Dir::of(&b, &[&g]),
        ];
        let s = run(&mut cache, &dirs);
        assert_eq!((counts(&s), s.files, s.kept()), ((1, 0, 0, 1), 2, 1));
        assert_eq!(s.unreadable[0].path, sub);
        assert_eq!(cache[&deep], before[&deep]);

        // Listed again: nothing was lost, nothing is read.
        let dirs = [Dir::of(&a, &[&deep]), Dir::of(&b, &[&g])];
        let s = run(&mut cache, &dirs);
        assert_eq!((counts(&s), s.bytes_read), ((2, 0, 0, 0), 0));
        assert!(s.unreadable.is_empty());
    }

    /// R8: a directory that does not exist has no files; what the cache had there is dropped.
    #[test]
    fn files_below_a_missing_directory_are_dropped() {
        let (_tmp, a) = sandbox();
        let f = a.join("f");
        write(&f, "x\n");
        let mut cache = Cache::new();
        run(&mut cache, &[Dir::of(&a, &[&f])]);
        for gone in [io::ErrorKind::NotFound, io::ErrorKind::NotADirectory] {
            let mut cache = cache.clone();
            let s = run(&mut cache, &[Dir::of(&a, &[]).failing(&a, gone)]);
            assert_eq!((counts(&s), s.files, s.kept()), ((0, 0, 0, 1), 0, 0));
            assert!(s.unreadable.is_empty() && cache.is_empty());
        }
    }

    /// R8: a directory to track whose real path could not be found lists nothing, and the
    /// cached files are not below the path it is given as. Those read below the real path it
    /// last resolved to are kept, and no others: not the files of another directory of the
    /// same provider that is no longer given, nor of one that is listed, nor of another
    /// provider.
    #[test]
    fn an_unresolved_directory_keeps_the_files_of_its_last_real_path_alone() {
        let (tmp, a) = sandbox();
        let root = tmp.path().canonicalize().unwrap();
        let (b, c, d) = (root.join("b"), root.join("c"), root.join("d"));
        for dir in [&b, &c, &d] {
            fs::create_dir(dir).unwrap();
        }
        let (f, g, h) = (a.join("f"), a.join("g"), b.join("h"));
        let (gone, other) = (c.join("gone"), d.join("other"));
        for file in [&f, &g, &h, &gone, &other] {
            write(file, "x\n");
        }
        let all = [
            Dir::of(&a, &[&f, &g]),
            Dir::of(&b, &[&h]),
            Dir::of(&c, &[&gone]),
            Dir::of(&d, &[&other]).provider("other"),
        ];
        let mut cache = Cache::new();
        run(&mut cache, &all);
        let cached = cache.clone();

        // `b` is given as a path that cannot be resolved, and was at `b`. `c`, of the same
        // provider, and `d` are no longer given; in `a`, which is listed, `g` vanished.
        fs::remove_file(&g).unwrap();
        let given = root.join("home/b as given");
        let dirs = [Dir::of(&a, &[&f, &g]), Dir::unresolved(&given, Some(&b))];
        let s = run(&mut cache, &dirs);
        assert_eq!((counts(&s), s.files, s.kept()), ((1, 0, 0, 3), 2, 1));
        assert_eq!(
            s.unreadable,
            [Unreadable {
                path: given.clone(),
                error: "out of reach".into(),
                kept: 1,
                unresolved: true,
            }]
        );
        assert_eq!(cache.keys().collect::<Vec<_>>(), [&f, &h]);
        assert_eq!(cache[&h], cached[&h]);
        assert_eq!(
            s.incomplete().as_deref(),
            Some(format!("incomplete: cannot read {}: out of reach", given.display()).as_str())
        );

        // Resolved again: nothing was lost, nothing is read.
        let s = run(&mut cache, &[Dir::of(&a, &[&f]), Dir::of(&b, &[&h])]);
        assert_eq!((counts(&s), s.bytes_read), ((2, 0, 0, 0), 0));
        assert!(s.unreadable.is_empty());

        // It was at `a`, which another directory given lists: `g`, which vanished there, is
        // not kept for it.
        let mut cache = cached.clone();
        let dirs = [Dir::of(&a, &[&f, &g]), Dir::unresolved(&given, Some(&a))];
        let s = run(&mut cache, &dirs);
        assert_eq!((counts(&s), s.files, s.kept()), ((1, 0, 0, 4), 1, 0));
        assert_eq!(cache.keys().collect::<Vec<_>>(), [&f]);

        // Nothing is remembered of it: nothing is its to keep, and it is reported all the
        // same.
        let mut cache = cached.clone();
        let s = run(&mut cache, &[Dir::unresolved(&given, None)]);
        assert_eq!((counts(&s), s.files, s.kept()), ((0, 0, 0, 5), 0, 0));
        assert_eq!(s.unreadable.len(), 1);
        assert!(s.unreadable[0].unresolved);

        // Several that cannot be resolved: each keeps its own.
        let mut cache = cached.clone();
        let dirs = [
            Dir::unresolved(&given, Some(&c)),
            Dir::unresolved(&root.join("another"), Some(&b)),
        ];
        let s = run(&mut cache, &dirs);
        assert_eq!((counts(&s), s.files), ((0, 0, 0, 3), 2));
        let kept: Vec<usize> = s.unreadable.iter().map(|u| u.kept).collect();
        assert_eq!(kept, [1, 1]);
        assert_eq!(cache.keys().collect::<Vec<_>>(), [&h, &gone]);
    }

    /// R8: what a cache remembers of its directories, by the path each is given as: where one
    /// that resolved is, where one that cannot be resolved was, and nothing of any other.
    #[test]
    fn a_cache_remembers_where_its_directories_were_last_found() {
        /// Given as `.0`, found at `.1` or not resolved.
        struct At(&'static str, Option<&'static str>);
        impl Resolved for At {
            fn given(&self) -> &Path {
                Path::new(self.0)
            }
            fn real(&self) -> Option<&Path> {
                self.1.map(Path::new)
            }
        }
        let at = |pairs: &[(&str, &str)]| -> Remembered {
            let pair = |(given, real): &(&str, &str)| (PathBuf::from(given), PathBuf::from(real));
            pairs.iter().map(pair).collect()
        };
        let mut remembered = Remembered::new();
        // `c` never resolved: nothing is known of it.
        let dirs = [
            At("/h/a/projects", Some("/r/a")),
            At("/h/b/projects", Some("/r/b")),
            At("/h/c/projects", None),
        ];
        assert!(remember(&mut remembered, &dirs));
        let both = at(&[("/h/a/projects", "/r/a"), ("/h/b/projects", "/r/b")]);
        assert_eq!(remembered, both);
        assert!(!remember(&mut remembered, &dirs), "the same again");

        // `b` cannot be resolved: it stays where it was. `a` moved; `d` is new.
        let dirs = [
            At("/h/a/projects", Some("/r/a2")),
            At("/h/b/projects", None),
            At("/h/d/projects", Some("/r/d")),
        ];
        assert!(remember(&mut remembered, &dirs));
        assert_eq!(
            remembered,
            at(&[
                ("/h/a/projects", "/r/a2"),
                ("/h/b/projects", "/r/b"),
                ("/h/d/projects", "/r/d")
            ])
        );
        assert!(!remember(&mut remembered, &dirs));

        // Another directory, in another home, that cannot be resolved is not `b`, whoever
        // gives it: nothing is known of it, and `b`, no longer given, is forgotten.
        let dirs = [
            At("/h/a/projects", Some("/r/a2")),
            At("/elsewhere/b/projects", None),
        ];
        assert!(remember(&mut remembered, &dirs));
        assert_eq!(remembered, at(&[("/h/a/projects", "/r/a2")]));
        // Not given at all: gone, or no longer tracked.
        assert!(remember(&mut remembered, &[] as &[At]));
        assert!(remembered.is_empty());
    }

    /// R8: the real path of a directory to track: none when it is not there or no directory,
    /// an error when the way to it cannot be searched.
    #[test]
    fn a_directory_that_cannot_be_resolved_is_told_from_one_that_is_missing() {
        let (tmp, a) = sandbox();
        let root = tmp.path().canonicalize().unwrap();
        let inside = a.join("store");
        fs::create_dir(&inside).unwrap();
        write(&a.join("file"), "x\n");
        std::os::unix::fs::symlink(&inside, root.join("link")).unwrap();
        std::os::unix::fs::symlink(root.join("nowhere"), root.join("dangling")).unwrap();
        // As given, through a symlinked directory: resolved to the real path.
        assert_eq!(real_dir(&tmp.path().join("link")), Ok(Some(inside.clone())));
        assert_eq!(real_dir(&inside), Ok(Some(inside.clone())));
        assert_eq!(real_dir(&a.join("missing")), Ok(None));
        assert_eq!(real_dir(&a.join("file")), Ok(None), "no directory");
        assert_eq!(real_dir(&a.join("file/below")), Ok(None));
        assert_eq!(real_dir(&root.join("dangling")), Ok(None));

        chmod(&a, 0o000);
        let (direct, linked) = (real_dir(&inside), real_dir(&root.join("link")));
        let missing = real_dir(&a.join("missing"));
        chmod(&a, 0o755);
        for found in [direct, linked, missing] {
            let error = found.expect_err("it may be there: `a` cannot be searched");
            assert!(!error.is_empty());
        }
    }

    /// R8, review #10: a failure of one directory keeps the files of the directory it was met
    /// listing, and no others: not those of a directory no longer given, wherever they are,
    /// and not those cached for another provider at the same path.
    #[test]
    fn an_unreadable_directory_keeps_only_the_files_of_its_own() {
        let (_tmp, a) = sandbox();
        let nested = a.join("nested");
        fs::create_dir(&nested).unwrap();
        let (f, deep) = (a.join("f"), nested.join("deep"));
        write(&f, "x\n");
        write(&deep, "x\n");
        let denied = io::ErrorKind::PermissionDenied;
        let both = [Dir::of(&a, &[&f]), Dir::of(&nested, &[&deep])];
        let mut cache = Cache::new();
        run(&mut cache, &both);
        assert_eq!((&cache[&f].dir, &cache[&deep].dir), (&a, &nested));
        let cached = cache.clone();

        // `nested` is no longer given, and `a`, which it is below, cannot be listed.
        let s = run(&mut cache, &[Dir::of(&a, &[]).failing(&a, denied)]);
        assert_eq!((counts(&s), s.files, s.kept()), ((0, 0, 0, 1), 1, 1));
        assert_eq!(cache.keys().collect::<Vec<_>>(), [&f]);

        // Both given, `a` failing: each keeps or lists its own.
        let mut cache = cached.clone();
        let s = run(
            &mut cache,
            &[
                Dir::of(&a, &[]).failing(&a, denied),
                Dir::of(&nested, &[&deep]),
            ],
        );
        assert_eq!((counts(&s), s.files, s.kept()), ((1, 0, 0, 0), 2, 1));

        // The same path given as another provider's: what was cached is not its own.
        let mut cache = cached.clone();
        let other = Dir::of(&a, &[]).provider("other").failing(&a, denied);
        let s = run(&mut cache, &[other]);
        assert_eq!((counts(&s), s.files, s.kept()), ((0, 0, 0, 2), 0, 0));
        assert_eq!(s.unreadable.len(), 1);
        assert!(cache.is_empty());
    }

    /// R8, review #10: a directory that can be read but not searched lists its names, and
    /// nothing can be told of them. Its files are kept like those of one that cannot be
    /// listed, not taken for vanished; nothing is read again once it can be searched.
    #[test]
    fn files_of_a_directory_that_cannot_be_searched_are_kept() {
        let (_tmp, a) = sandbox();
        let sub = a.join("sub");
        fs::create_dir(&sub).unwrap();
        for inner in ["0", "z"] {
            fs::create_dir(sub.join(inner)).unwrap();
            write(&sub.join(inner).join("inside"), "x\n");
        }
        let (f, g, deep, deeper) = (a.join("f"), a.join("g"), sub.join("d1"), sub.join("d2"));
        for file in [&f, &g, &deep, &deeper] {
            write(file, "x\n");
        }
        let dirs = [Dir::walking(&a)];
        let mut cache = Cache::new();
        let s = run(&mut cache, &dirs);
        assert_eq!((counts(&s), s.files), ((0, 0, 6, 0), 6));
        let before = cache.clone();

        chmod(&sub, 0o444);
        fs::remove_file(&g).unwrap();
        let s = run(&mut cache, &dirs);
        chmod(&sub, 0o755);
        assert_eq!((counts(&s), s.files, s.kept()), ((1, 0, 0, 1), 5, 4));
        // Not per file, and not again for the directories below it, which fail too.
        assert_eq!(s.unreadable.len(), 1, "{:?}", s.unreadable);
        assert_eq!(
            (s.unreadable[0].path.as_path(), s.unreadable[0].kept),
            (sub.as_path(), 4)
        );
        assert_eq!(
            (&cache[&deep], &cache[&deeper]),
            (&before[&deep], &before[&deeper])
        );

        let s = run(&mut cache, &dirs);
        assert_eq!((counts(&s), s.bytes_read), ((5, 0, 0, 0), 0));
        assert!(s.unreadable.is_empty());
    }

    /// R8: a symlink whose target is out of reach is one file that cannot be read when it is
    /// listed as a file: what the cache has of it is dropped, and nothing is reported. A
    /// directory that cannot be searched keeps its files also when they all are symlinks.
    #[test]
    fn a_symlinked_file_out_of_reach_is_dropped_not_kept() {
        let (tmp, a) = sandbox();
        let root = tmp.path().canonicalize().unwrap();
        let (away, links) = (root.join("away"), a.join("links"));
        fs::create_dir(&away).unwrap();
        fs::create_dir(&links).unwrap();
        for target in ["t0", "t1", "t2"] {
            write(&away.join(target), "x\n");
        }
        let symlink = |target: &str, link: &Path| {
            std::os::unix::fs::symlink(away.join(target), link).unwrap();
        };
        let (f, link) = (a.join("f"), a.join("link"));
        write(&f, "x\n");
        symlink("t0", &link);
        symlink("t1", &links.join("l1"));
        symlink("t2", &links.join("l2"));
        let dirs = [Dir::walking(&a)];
        let mut cache = Cache::new();
        let s = run(&mut cache, &dirs);
        assert_eq!((counts(&s), s.files), ((0, 0, 4, 0), 4));
        let before = cache.clone();

        // The targets are out of reach, each link is there: single files, all dropped.
        chmod(&away, 0o000);
        let s = run(&mut cache, &dirs);
        chmod(&away, 0o755);
        assert_eq!((counts(&s), s.files, s.kept()), ((1, 0, 0, 3), 1, 0));
        assert!(s.unreadable.is_empty(), "{:?}", s.unreadable);
        assert_eq!(cache.keys().collect::<Vec<_>>(), [&f]);

        // The directory holding links cannot be searched: nothing is known of them. They are
        // kept, the directory is reported, and the link beside it is as usual.
        let mut cache = before.clone();
        chmod(&links, 0o444);
        let s = run(&mut cache, &dirs);
        chmod(&links, 0o755);
        assert_eq!((counts(&s), s.files, s.kept()), ((2, 0, 0, 0), 4, 2));
        assert_eq!(s.unreadable.len(), 1, "{:?}", s.unreadable);
        assert_eq!(s.unreadable[0].path, links);
        assert_eq!(cache, before);
    }

    /// R8: what is kept is what lies below the directory that could not be read. A cached
    /// file at the very path that failed was a file, not a directory: it is dropped.
    #[test]
    fn a_file_at_the_path_that_cannot_be_read_is_not_kept() {
        let (_tmp, a) = sandbox();
        let sub = a.join("sub");
        fs::create_dir(&sub).unwrap();
        let (f, deep) = (a.join("f"), sub.join("deep"));
        write(&f, "x\n");
        write(&deep, "x\n");
        let mut cache = Cache::new();
        run(&mut cache, &[Dir::of(&a, &[&f, &deep])]);

        let denied = io::ErrorKind::PermissionDenied;
        let dirs = [Dir::of(&a, &[]).failing(&f, denied).failing(&sub, denied)];
        let s = run(&mut cache, &dirs);
        assert_eq!((counts(&s), s.files, s.kept()), ((0, 0, 0, 1), 1, 1));
        let kept: Vec<(&Path, usize)> = s
            .unreadable
            .iter()
            .map(|u| (u.path.as_path(), u.kept))
            .collect();
        assert_eq!(kept, [(f.as_path(), 0), (sub.as_path(), 1)]);
        assert_eq!(cache.keys().collect::<Vec<_>>(), [&deep]);
    }

    /// R8: listing tells what is gone from what cannot be read: a directory that cannot be
    /// listed, one whose entries cannot be examined, a symlink to somewhere out of reach
    /// (asked as a directory: recorded; listed as a file: one file left out).
    #[test]
    fn a_listing_records_what_it_cannot_read() {
        let (_tmp, a) = sandbox();
        let (file, dir, locked, searchless, linksonly) = (
            a.join("file"),
            a.join("dir"),
            a.join("locked"),
            a.join("searchless"),
            a.join("linksonly"),
        );
        write(&file, "x\n");
        write(&a.join("soon gone"), "x\n");
        for d in [&dir, &locked, &searchless, &linksonly] {
            fs::create_dir(d).unwrap();
        }
        fs::create_dir(locked.join("inner")).unwrap();
        write(&locked.join("inside"), "x\n");
        write(&searchless.join("one"), "x\n");
        write(&searchless.join("two"), "x\n");
        let symlink = |target: &Path, link: &Path| {
            std::os::unix::fs::symlink(target, link).unwrap();
        };
        symlink(&locked.join("inside"), &a.join("link"));
        symlink(&locked.join("inner"), &a.join("dirlink"));
        symlink(&a.join("nowhere"), &a.join("dangling"));
        symlink(&file, &linksonly.join("l"));

        let mut listing = Listing::default();
        let found = listing.read_dir(&a);
        let entry = |name: &str| {
            found
                .iter()
                .find(|e| e.name() == Some(name))
                .unwrap_or_else(|| panic!("no {name}"))
        };
        assert_eq!(found.len(), 9);
        assert_eq!(entry("file").path(), file);
        assert!(entry("dir").is_dir() && !entry("file").is_dir() && !entry("dirlink").is_dir());
        assert!(listing.is_dir(entry("dir")) && !listing.is_dir(entry("file")));
        assert!(listing.is_dir(entry("dirlink")), "a symlink is followed");
        assert!(listing.read_dir(&a.join("missing")).is_empty());
        assert!(listing.read_dir(&file).is_empty(), "not a directory");
        // Gone since it was listed, or leading nowhere: not there.
        fs::remove_file(a.join("soon gone")).unwrap();
        listing.file(entry("soon gone"), "gone".into());
        listing.file(entry("dangling"), "dangling".into());
        assert!(!listing.is_dir(entry("dangling")));
        listing.file(entry("file"), "id".into());
        listing.file(entry("dir"), "a directory".into());
        assert!(listing.unreadable.is_empty(), "{:?}", listing.unreadable);

        chmod(&locked, 0o000);
        chmod(&searchless, 0o444);
        chmod(&linksonly, 0o444);
        assert!(listing.read_dir(&locked).is_empty());
        // Its target is out of reach, the link is there. Listed as a file, it is one file
        // that cannot be read: left out, and nothing recorded.
        listing.file(entry("link"), "link".into());
        assert_eq!(listing.unreadable.len(), 1, "{:?}", listing.unreadable);
        // Asked as a directory, nothing is known of what is below it: the link, not the
        // directory it is in.
        assert!(!listing.is_dir(entry("dirlink")));
        // Listed, but nothing in it can be examined: the directory, once.
        for inside in listing.read_dir(&searchless) {
            assert!(!listing.is_dir(&inside));
            listing.file(&inside, "inside".into());
        }
        // The same where what cannot be examined is a link: it is its directory that cannot
        // be searched, not its target that is out of reach.
        for inside in listing.read_dir(&linksonly) {
            listing.file(&inside, "inside".into());
        }
        for d in [&locked, &searchless, &linksonly] {
            chmod(d, 0o755);
        }
        let unreadable: Vec<&Path> = listing
            .unreadable
            .iter()
            .map(|u| u.path.as_path())
            .collect();
        assert_eq!(
            unreadable,
            [
                locked.as_path(),
                &a.join("dirlink"),
                searchless.as_path(),
                linksonly.as_path()
            ]
        );
        assert!(
            listing
                .unreadable
                .iter()
                .all(|u| u.kept == 0 && !u.error.is_empty())
        );
        let files = listing.into_files();
        assert_eq!(files.len(), 1);
        assert_eq!(
            (&files[0].path, files[0].session_id.as_str()),
            (&file, "id")
        );
        assert_eq!(files[0].stat, stat(&file));
    }

    /// What a status bar says of an incomplete refresh.
    #[test]
    fn an_incomplete_refresh_is_one_line() {
        let unreadable = |path: &str, kept: usize| Unreadable {
            path: path.into(),
            error: "Permission denied (os error 13)".into(),
            kept,
            unresolved: false,
        };
        let mut s = RefreshStats::default();
        assert_eq!((s.incomplete(), s.kept()), (None, 0));
        s.unreadable.push(unreadable("/h/projects", 3));
        assert_eq!(
            s.incomplete().as_deref(),
            Some("incomplete: cannot read /h/projects: Permission denied (os error 13)")
        );
        s.unreadable.push(unreadable("/h/sessions", 4));
        assert_eq!(
            s.incomplete().as_deref(),
            Some(
                "incomplete: cannot read 2 directories, first /h/projects: Permission denied \
                 (os error 13)"
            )
        );
        assert_eq!(s.kept(), 7);
    }
}
