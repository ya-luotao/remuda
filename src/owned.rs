//! The files remuda owns: every write below `$REMUDA_HOME` (SPEC R3, R13, R18).
//!
//! `config.toml`, `state/`, `shared/claude/` and `homes/<provider>/<name>` are written through
//! this module and nowhere else. It owns what those writes have in common:
//!
//! - the way down from `$REMUDA_HOME`: one directory descriptor at a time, never through a
//!   symlink, except where the SPEC promises to write through one (`config.toml`, `state`, a
//!   file in `state/`);
//! - the modes of private directories and files, and the tightening of those from before;
//! - replacing a file or a link atomically: a temporary name in the same directory, renamed
//!   into place, and the removal of the temporary files a killed remuda left behind;
//! - exclusive locks, and what happens on a file system that has none;
//! - the launch log's rules: only a regular file, only one that is the user's alone.
//!
//! Reads do not come here: a command that only reads creates nothing.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

/// A directory that is the user's alone: `state/`, `state/settings/`, a new home (R3, R13).
const PRIVATE_DIR: u32 = 0o700;
/// A file that is the user's alone: the caches, the launch log, the injected settings (R3).
const PRIVATE_FILE: u32 = 0o600;
/// A copy that is not to be edited: the source's rules (R18).
const READ_ONLY_FILE: u32 = 0o400;
/// Left to the umask: the directories between `$REMUDA_HOME` and what is private.
const DEFAULT_DIR: u32 = 0o777;
/// Left to the umask: a new `config.toml` (R3).
const DEFAULT_FILE: u32 = 0o666;

/// The directory of homes that `setup` creates, below `$REMUDA_HOME` (R3).
pub const HOMES: &str = "homes";

/// `$REMUDA_HOME/state`, the sibling of `config.toml` (R3).
pub fn state_dir(config: &Path) -> PathBuf {
    config.with_file_name("state")
}

/// The launch log of the state directory `state` (R6).
pub fn launch_log(state: &Path) -> PathBuf {
    state.join("launches.jsonl")
}

/// `$REMUDA_HOME/state/settings`: the injected settings, one file per content (R18).
pub fn settings_dir(config: &Path) -> PathBuf {
    state_dir(config).join("settings")
}

/// `$REMUDA_HOME/shared/claude`, next to `config.toml` (R13, R18).
pub fn shared_dir(config: &Path) -> PathBuf {
    config.with_file_name("shared").join("claude")
}

/// What an entry of a directory is, without following it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    Symlink,
    /// A FIFO, a socket, a device.
    Other,
}

/// An entry of a [`Dir`], read without following a symlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    pub kind: Kind,
    /// The permission bits.
    pub mode: u32,
    pub modified: SystemTime,
    /// Device and inode: which file it is.
    file: (u64, u64),
}

impl Meta {
    // The field types of `stat` differ between systems.
    #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
    fn from_stat(stat: &libc::stat) -> Meta {
        let kind = match stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR => Kind::Dir,
            libc::S_IFREG => Kind::File,
            libc::S_IFLNK => Kind::Symlink,
            _ => Kind::Other,
        };
        let seconds = stat.st_mtime as i64;
        let nanos = Duration::from_nanos((stat.st_mtime_nsec as i64).clamp(0, 999_999_999) as u64);
        let whole = Duration::from_secs(seconds.unsigned_abs());
        let modified = if seconds >= 0 {
            UNIX_EPOCH.checked_add(whole)
        } else {
            UNIX_EPOCH.checked_sub(whole)
        }
        .and_then(|at| at.checked_add(nanos))
        .unwrap_or(UNIX_EPOCH);
        Meta {
            kind,
            mode: stat.st_mode as u32 & 0o7777,
            modified,
            file: (stat.st_dev as u64, stat.st_ino as u64),
        }
    }
}

/// Why a directory of remuda's own could not be opened: remuda creates, replaces and removes
/// below it, so it must be a real directory (R13).
#[derive(Debug)]
pub struct Blocked {
    pub path: PathBuf,
    pub why: Why,
}

#[derive(Debug)]
pub enum Why {
    /// A symlink is there: through it, the writes would land where it points.
    Symlink,
    /// Something else that is not a directory is there.
    NotADirectory,
    Create(io::Error),
    Open(io::Error),
}

impl Blocked {
    fn new(path: &Path, why: Why) -> Blocked {
        Blocked {
            path: path.to_path_buf(),
            why,
        }
    }

    /// Nothing is there (for an opening that creates nothing).
    pub fn is_missing(&self) -> bool {
        matches!(&self.why, Why::Open(e) if e.kind() == io::ErrorKind::NotFound)
    }
}

impl fmt::Display for Blocked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.path.display();
        match &self.why {
            Why::Symlink => write!(f, "{path} is a symlink; remuda does not write through it"),
            Why::NotADirectory => {
                write!(f, "{path} is not a directory; remuda does not replace it")
            }
            Why::Create(_) => write!(f, "cannot create {path}"),
            Why::Open(_) => write!(f, "cannot open {path}"),
        }
    }
}

impl std::error::Error for Blocked {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.why {
            Why::Create(e) | Why::Open(e) => Some(e),
            Why::Symlink | Why::NotADirectory => None,
        }
    }
}

/// How a file written by [`Dir::write`] is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMode {
    /// The user's alone (mode 0600), and on disk before it is renamed into place.
    Private,
    /// Read-only, for the user alone (mode 0400). Not synced: a copy is compared with its
    /// source again before every use.
    ReadOnly,
}

/// The file `path` names in its directory, and that directory (`.` for a bare name).
fn split(path: &Path) -> io::Result<(&Path, &OsStr)> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no file name", path.display()),
        )
    })?;
    let above = match path.parent() {
        Some(above) if !above.as_os_str().is_empty() => above,
        _ => Path::new("."),
    };
    Ok((above, name))
}

fn c_string(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in a path"))
}

/// `name` as one entry of a directory: never a path, so never a way out of the directory.
fn entry_name(name: &OsStr) -> io::Result<CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not the name of a directory entry: {name:?}"),
        ));
    }
    c_string(bytes)
}

fn check(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(result)
}

/// Sets errno to 0, so that a null `readdir` can be told apart: end of directory or error.
fn clear_errno() {
    // SAFETY: the errno location is valid for the calling thread.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe {
        *libc::__errno_location() = 0;
    }
    // SAFETY: as above.
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    unsafe {
        *libc::__error() = 0;
    }
}

/// Takes away every permission of an open file or directory of remuda's that is not in `keep`
/// (R3); nothing is ever added. Failing to is not an error here: a caller that must not write
/// into what stayed open to others looks at the mode afterwards (the launch log does).
fn tighten(file: &fs::File, keep: u32) {
    if let Ok(meta) = file.metadata() {
        let mode = meta.permissions().mode() & 0o7777;
        if mode & !keep != 0 {
            let _ = file.set_permissions(fs::Permissions::from_mode(mode & keep));
        }
    }
}

const TEMP_PREFIX: &[u8] = b".remuda-";
const TEMP_SUFFIX: &[u8] = b".tmp";

/// A new temporary name: `.remuda-<pid>-<32 hex digits>.tmp`. The one scheme for everything
/// remuda writes and renames into place. It names the process, so that what a killed one left
/// behind can be told from what a running one is writing ([`Dir::sweep`]); it does not name
/// the file it is for, so its length does not depend on that one's.
fn temp_name() -> CString {
    let name = format!(
        ".remuda-{}-{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    );
    CString::new(name).expect("a temporary name has no NUL")
}

/// Whether `name` is one of remuda's temporary names ([`temp_name`]).
pub fn is_temp_name(name: &OsStr) -> bool {
    temp_pid(name).is_some()
}

/// The process that made the temporary name `name`.
fn temp_pid(name: &OsStr) -> Option<u32> {
    let rest = name
        .as_bytes()
        .strip_prefix(TEMP_PREFIX)?
        .strip_suffix(TEMP_SUFFIX)?;
    let (pid, hex) = std::str::from_utf8(rest).ok()?.split_once('-')?;
    let named = !pid.is_empty()
        && pid.bytes().all(|b| b.is_ascii_digit())
        && hex.len() == 32
        && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !named {
        return None;
    }
    pid.parse().ok()
}

/// Whether process `pid` may still be running. Only "no such process" says it is gone: one
/// that belongs to another user is running, and so is whatever cannot be asked about.
fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return true;
    };
    if pid <= 0 {
        return true;
    }
    // SAFETY: signal 0 checks for the process and sends nothing.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

const DIR_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;

/// How a directory that cannot be read is opened all the same, to be worked in by name:
/// `O_PATH` on Linux, `O_SEARCH` elsewhere. Either gives a descriptor that `openat`,
/// `mkdirat`, `fstatat`, `renameat`, `unlinkat`, `symlinkat` and `readlinkat` take, each
/// checking the search permission as it runs, and that `fstat` reads. Listing through it
/// fails on both; on Linux `fchmod` and `flock` fail too (`EBADF`), which is why a lock is
/// taken on a descriptor of its own, opened for reading ([`Dir::reopen`]).
#[cfg(any(target_os = "linux", target_os = "android"))]
const SEARCH_ONLY: Option<libc::c_int> = Some(libc::O_PATH);
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
const SEARCH_ONLY: Option<libc::c_int> = Some(libc::O_SEARCH);
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd"
)))]
const SEARCH_ONLY: Option<libc::c_int> = None;

/// Opens the directory `name`: in `above`, or as a path. For reading where it can be read:
/// that is what listing it takes. A directory the user may search and write but not read
/// (mode 0300) is one remuda can still create, replace and remove in by name, as it could
/// when it wrote by path (R3), so it is then opened for that alone ([`SEARCH_ONLY`]): what
/// lists it (the cleanup of leftovers, a lock) fails or is skipped, and nothing else does.
/// Without `follow`, never through a symlink, whichever way it is opened.
fn open_dir(above: Option<&fs::File>, name: &CStr, follow: bool) -> io::Result<fs::File> {
    let open = |access: libc::c_int| {
        let nofollow = if follow { 0 } else { libc::O_NOFOLLOW };
        let flags = access | libc::O_DIRECTORY | libc::O_CLOEXEC | nofollow;
        // SAFETY: a NUL-terminated string and, with a directory above, its open descriptor.
        check(unsafe {
            match above {
                Some(above) => libc::openat(above.as_raw_fd(), name.as_ptr(), flags),
                None => libc::open(name.as_ptr(), flags),
            }
        })
        .map(adopt)
    };
    match open(libc::O_RDONLY) {
        Err(denied) if denied.kind() == io::ErrorKind::PermissionDenied => {
            // Checked on the open file: a directory, whatever the flags let through.
            match SEARCH_ONLY.map(open) {
                Some(Ok(dir)) if dir.metadata().is_ok_and(|meta| meta.is_dir()) => Ok(dir),
                _ => Err(denied),
            }
        }
        opened => opened,
    }
}

/// An open directory of remuda's own. Everything done through it is done relative to its
/// descriptor and to one entry name at a time, and none of it follows a symlink: what a
/// process of the same user does to the directory's path meanwhile (moving it, putting a
/// symlink in its place) does not move the write (R13).
#[derive(Debug)]
pub struct Dir {
    fd: fs::File,
    /// For messages, and for the one step that goes by path: writing through a file that is
    /// itself a symlink (R3).
    path: PathBuf,
}

impl Dir {
    /// The directory at `path`, as given: symlinks are followed. For `$REMUDA_HOME`, the
    /// user's path, and for the directory a symlink of the user's leads to (R3, R13).
    pub fn open(path: &Path) -> io::Result<Dir> {
        let name = c_string(path.as_os_str().as_bytes())?;
        Ok(Dir {
            fd: open_dir(None, &name, true)?,
            path: path.to_path_buf(),
        })
    }

    /// [`Dir::open`], creating the directory and what is above it as needed, with the
    /// default mode.
    pub fn root(path: &Path) -> Result<Dir, Blocked> {
        fs::create_dir_all(path).map_err(|e| Blocked::new(path, Why::Create(e)))?;
        Dir::open(path).map_err(|e| Blocked::new(path, Why::Open(e)))
    }

    /// The directory's path, for messages.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether `other` is this very directory, by whatever path each was opened.
    fn is(&self, other: &Dir) -> bool {
        match (self.fd.metadata(), other.fd.metadata()) {
            (Ok(this), Ok(that)) => (this.dev(), this.ino()) == (that.dev(), that.ino()),
            _ => false,
        }
    }

    fn open_at(&self, name: &CStr, flags: libc::c_int, mode: u32) -> io::Result<fs::File> {
        // SAFETY: a NUL-terminated string and an open directory descriptor.
        let fd = check(unsafe {
            libc::openat(
                self.fd.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC,
                mode as libc::c_uint,
            )
        })?;
        Ok(adopt(fd))
    }

    /// A second descriptor of this directory, with a position and locks of its own. Always
    /// for reading, which listing and locking take: a directory that cannot be read cannot
    /// be listed or locked.
    fn reopen(&self) -> io::Result<fs::File> {
        self.open_at(c".", DIR_FLAGS, 0)
    }

    fn make(&self, name: &OsStr, mode: u32) -> io::Result<()> {
        let name = entry_name(name)?;
        // SAFETY: a NUL-terminated string and an open directory descriptor.
        check(unsafe { libc::mkdirat(self.fd.as_raw_fd(), name.as_ptr(), mode as libc::mode_t) })
            .map(drop)
    }

    /// The directory `name` in this one, never through a symlink.
    pub fn sub(&self, name: impl AsRef<OsStr>) -> Result<Dir, Blocked> {
        let name = name.as_ref();
        let path = self.path.join(name);
        let opened = entry_name(name).and_then(|c| open_dir(Some(&self.fd), &c, false));
        match opened {
            Ok(fd) => Ok(Dir { fd, path }),
            // The reason is read from what is there, not from the errno, which differs
            // between systems for a symlink.
            Err(error) => {
                let why = match self.kind(name) {
                    Ok(Some(Kind::Symlink)) => Why::Symlink,
                    Ok(Some(Kind::File | Kind::Other)) => Why::NotADirectory,
                    Ok(Some(Kind::Dir) | None) | Err(_) => Why::Open(error),
                };
                Err(Blocked { path, why })
            }
        }
    }

    /// [`Dir::sub`], made with the default mode when it is missing.
    pub fn sub_or_create(&self, name: impl AsRef<OsStr>) -> Result<Dir, Blocked> {
        let name = name.as_ref();
        match self.sub(name) {
            Err(missing) if missing.is_missing() => {
                // Another remuda may have made it meanwhile: opened all the same.
                if let Err(e) = self.make(name, DEFAULT_DIR)
                    && e.kind() != io::ErrorKind::AlreadyExists
                {
                    return Err(Blocked::new(&missing.path, Why::Create(e)));
                }
                self.sub(name)
            }
            other => other,
        }
    }

    /// A new directory `name` with the default mode; fails where `name` exists.
    pub fn create_new(&self, name: impl AsRef<OsStr>) -> Result<Dir, Blocked> {
        let name = name.as_ref();
        self.make(name, DEFAULT_DIR)
            .map_err(|e| Blocked::new(&self.path.join(name), Why::Create(e)))?;
        self.sub(name)
    }

    /// A new directory `name` that is the user's alone (mode 0700, whatever the umask);
    /// fails where `name` exists (R13: the home `setup` creates).
    pub fn create_private(&self, name: impl AsRef<OsStr>) -> Result<Dir, Blocked> {
        let name = name.as_ref();
        let failed = |e| Blocked::new(&self.path.join(name), Why::Create(e));
        self.make(name, PRIVATE_DIR).map_err(failed)?;
        let dir = self.sub(name)?;
        // The umask may have removed bits; set the mode explicitly.
        dir.fd
            .set_permissions(fs::Permissions::from_mode(PRIVATE_DIR))
            .map_err(failed)?;
        Ok(dir)
    }

    /// The directory `name` as one that is the user's alone: made with mode 0700 or, where it
    /// exists, what the group and others may do with it taken away (R3). Never loosened, and
    /// never through a symlink.
    pub fn private(&self, name: impl AsRef<OsStr>) -> Result<Dir, Blocked> {
        self.private_dir(name.as_ref(), false)
    }

    /// [`Dir::private`]; with `through`, a `name` that is a symlink is used as it is: the
    /// directory it points at is where the user put it, and keeps its mode (R3: `state`).
    fn private_dir(&self, name: &OsStr, through: bool) -> Result<Dir, Blocked> {
        if let Err(e) = self.make(name, PRIVATE_DIR)
            && e.kind() != io::ErrorKind::AlreadyExists
        {
            return Err(Blocked::new(&self.path.join(name), Why::Create(e)));
        }
        match self.sub(name) {
            Ok(dir) => {
                // Changed through the descriptor of a directory opened without following a
                // symlink: never the directory a link points at. One that cannot be tightened
                // (another user's) is used as it is: its mode does not give away the contents
                // of the files in it, which are 0600 or not written (R3).
                tighten(&dir.fd, PRIVATE_DIR);
                Ok(dir)
            }
            Err(Blocked {
                path,
                why: Why::Symlink,
            }) if through => {
                match entry_name(name).and_then(|c| open_dir(Some(&self.fd), &c, true)) {
                    Ok(fd) => Ok(Dir { fd, path }),
                    Err(e) => Err(Blocked::new(&path, Why::Open(e))),
                }
            }
            Err(blocked) => Err(blocked),
        }
    }

    /// The entry `name`, not followed; `None` when there is none.
    pub fn meta(&self, name: impl AsRef<OsStr>) -> io::Result<Option<Meta>> {
        let name = entry_name(name.as_ref())?;
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        // SAFETY: a NUL-terminated string, an open directory descriptor, and room for the
        // result.
        let result = unsafe {
            libc::fstatat(
                self.fd.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result == 0 {
            // SAFETY: `fstatat` succeeded and filled it in.
            return Ok(Some(Meta::from_stat(&unsafe { stat.assume_init() })));
        }
        match io::Error::last_os_error() {
            e if e.kind() == io::ErrorKind::NotFound => Ok(None),
            e => Err(e),
        }
    }

    /// What the entry `name` is, not followed; `None` when there is none.
    pub fn kind(&self, name: impl AsRef<OsStr>) -> io::Result<Option<Kind>> {
        Ok(self.meta(name)?.map(|meta| meta.kind))
    }

    /// The names of the entries, in no order.
    pub fn names(&self) -> io::Result<Vec<OsString>> {
        /// A directory stream, closed when dropped.
        struct Stream(*mut libc::DIR);
        impl Drop for Stream {
            fn drop(&mut self) {
                // SAFETY: the stream came from a successful `fdopendir` and is closed once.
                unsafe { libc::closedir(self.0) };
            }
        }
        let copy = self.reopen()?;
        // SAFETY: `copy` is an open descriptor; on success the stream owns it.
        let stream = unsafe { libc::fdopendir(copy.as_raw_fd()) };
        if stream.is_null() {
            return Err(io::Error::last_os_error());
        }
        // Closed by `closedir` from here on.
        let _ = copy.into_raw_fd();
        let stream = Stream(stream);
        let mut names = Vec::new();
        loop {
            clear_errno();
            // SAFETY: `stream.0` is a valid directory stream until `stream` is dropped; the
            // entry is read before the next call on the stream.
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                // The end of the directory leaves errno alone; an error sets it.
                let error = io::Error::last_os_error();
                return match error.raw_os_error() {
                    Some(0) | None => Ok(names),
                    Some(_) => Err(error),
                };
            }
            // SAFETY: `d_name` of an entry `readdir` returned is NUL-terminated.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name != b"." && name != b".." {
                names.push(OsString::from_vec(name.to_vec()));
            }
        }
    }

    /// Whether the directory has no entry.
    pub fn is_empty(&self) -> io::Result<bool> {
        Ok(self.names()?.is_empty())
    }

    /// Where the symlink `name` points, as written.
    pub fn read_link(&self, name: impl AsRef<OsStr>) -> io::Result<PathBuf> {
        let name = entry_name(name.as_ref())?;
        let mut target = vec![0u8; 256];
        loop {
            // SAFETY: a NUL-terminated string, an open directory descriptor, and a buffer of
            // the length given.
            let length = unsafe {
                libc::readlinkat(
                    self.fd.as_raw_fd(),
                    name.as_ptr(),
                    target.as_mut_ptr().cast(),
                    target.len(),
                )
            };
            let Ok(length) = usize::try_from(length) else {
                return Err(io::Error::last_os_error());
            };
            // A full buffer may have cut the target short.
            if length < target.len() {
                target.truncate(length);
                return Ok(PathBuf::from(OsString::from_vec(target)));
            }
            target.resize(target.len() * 2, 0);
        }
    }

    /// Replaces the file `name` atomically: written under a temporary name in this directory
    /// and renamed into place. A failure leaves `name` as it was and removes the temporary
    /// file.
    pub fn write(&self, name: impl AsRef<OsStr>, bytes: &[u8], mode: FileMode) -> io::Result<()> {
        match mode {
            FileMode::Private => self.put(name.as_ref(), bytes, PRIVATE_FILE, None, true),
            FileMode::ReadOnly => self.put(name.as_ref(), bytes, READ_ONLY_FILE, None, false),
        }
    }

    /// [`Dir::write`]: the temporary file is created with mode `create`, then given `set` when
    /// there is one, and with `sync` it is on disk before the rename.
    fn put(
        &self,
        name: &OsStr,
        bytes: &[u8],
        create: u32,
        set: Option<u32>,
        sync: bool,
    ) -> io::Result<()> {
        let name = entry_name(name)?;
        let temp = temp_name();
        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW;
        let mut file = self.open_at(&temp, flags, create)?;
        let written = (|| {
            if let Some(mode) = set {
                file.set_permissions(fs::Permissions::from_mode(mode))?;
            }
            file.write_all(bytes)?;
            if sync {
                file.sync_all()?;
            }
            self.rename_entry(&temp, &name)
        })();
        if written.is_err() {
            let _ = self.unlink(&temp, 0);
        }
        written
    }

    /// A symlink `name` pointing at `target`; fails where `name` exists.
    pub fn link(&self, name: impl AsRef<OsStr>, target: &Path) -> io::Result<()> {
        self.symlink(&entry_name(name.as_ref())?, target)
    }

    /// A symlink `name` pointing at `target`, created or replaced atomically: a temporary
    /// link in this directory, renamed into place.
    pub fn replace_link(&self, name: impl AsRef<OsStr>, target: &Path) -> io::Result<()> {
        let name = entry_name(name.as_ref())?;
        let temp = temp_name();
        self.symlink(&temp, target)?;
        self.rename_entry(&temp, &name).inspect_err(|_| {
            let _ = self.unlink(&temp, 0);
        })
    }

    fn symlink(&self, name: &CStr, target: &Path) -> io::Result<()> {
        let target = c_string(target.as_os_str().as_bytes())?;
        // SAFETY: two NUL-terminated strings and an open directory descriptor.
        check(unsafe { libc::symlinkat(target.as_ptr(), self.fd.as_raw_fd(), name.as_ptr()) })
            .map(drop)
    }

    fn rename_entry(&self, from: &CStr, to: &CStr) -> io::Result<()> {
        let fd = self.fd.as_raw_fd();
        // SAFETY: two NUL-terminated strings and an open directory descriptor.
        check(unsafe { libc::renameat(fd, from.as_ptr(), fd, to.as_ptr()) }).map(drop)
    }

    fn unlink(&self, name: &CStr, flags: libc::c_int) -> io::Result<()> {
        // SAFETY: a NUL-terminated string and an open directory descriptor.
        check(unsafe { libc::unlinkat(self.fd.as_raw_fd(), name.as_ptr(), flags) }).map(drop)
    }

    /// Renames the entry `from` to `to`, both in this directory.
    pub fn rename(&self, from: impl AsRef<OsStr>, to: impl AsRef<OsStr>) -> io::Result<()> {
        self.rename_entry(&entry_name(from.as_ref())?, &entry_name(to.as_ref())?)
    }

    /// Removes the entry `name`: a file, or a symlink itself. Never a directory.
    pub fn remove_file(&self, name: impl AsRef<OsStr>) -> io::Result<()> {
        self.unlink(&entry_name(name.as_ref())?, 0)
    }

    /// Removes the directory `name`; fails when it holds anything.
    pub fn remove_dir(&self, name: impl AsRef<OsStr>) -> io::Result<()> {
        self.unlink(&entry_name(name.as_ref())?, libc::AT_REMOVEDIR)
    }

    /// Removes the entry `name` and, when it is a directory, everything in it. Symlinks are
    /// removed, not followed. Nothing there is not an error.
    pub fn remove_tree(&self, name: impl AsRef<OsStr>) -> io::Result<()> {
        let name = name.as_ref();
        match self.kind(name)? {
            None => Ok(()),
            Some(Kind::Dir) => {
                let dir = self.sub(name).map_err(io::Error::other)?;
                for entry in dir.names()? {
                    dir.remove_tree(&entry)?;
                }
                self.remove_dir(name)
            }
            Some(_) => self.remove_file(name),
        }
    }

    /// Sets when the regular file `name` was last modified.
    pub fn touch(&self, name: impl AsRef<OsStr>, modified: SystemTime) -> io::Result<()> {
        let flags = libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK;
        self.open_at(&entry_name(name.as_ref())?, flags, 0)?
            .set_modified(modified)
    }

    /// An exclusive lock on this directory itself, held until the result is dropped; nothing
    /// is created for it. Waits for whoever holds it.
    pub fn lock(&self, locks: &dyn Locks, lockless: Lockless) -> io::Result<Held> {
        Held::take(self.reopen()?, locks, lockless)
    }

    /// [`Dir::lock`] on the file `name` in this directory: a regular file that is the user's
    /// alone, made when missing, where a directory holds files that come and go.
    pub fn lock_file(
        &self,
        name: impl AsRef<OsStr>,
        locks: &dyn Locks,
        lockless: Lockless,
    ) -> io::Result<Held> {
        let flags = libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW;
        let file = self.open_at(&entry_name(name.as_ref())?, flags, PRIVATE_FILE)?;
        Held::take(file, locks, lockless)
    }

    /// Removes the temporary files and links in this directory that a remuda which is gone
    /// left behind (killed between creating one and renaming it into place). One whose
    /// process may still be running is being written and stays; age is not looked at. Best
    /// effort: what cannot be removed now is tried again by the next write here.
    pub fn sweep(&self) {
        let Ok(names) = self.names() else {
            return;
        };
        for name in names {
            if temp_pid(&name).is_some_and(|pid| !alive(pid))
                && matches!(self.kind(&name), Ok(Some(Kind::File | Kind::Symlink)))
            {
                let _ = self.remove_file(&name);
            }
        }
    }
}

fn adopt(fd: libc::c_int) -> fs::File {
    // SAFETY: `fd` was just opened and is owned by nothing else.
    fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// How an exclusive lock is taken: the seam between remuda and the file system's locks.
pub trait Locks {
    /// Waits until `file` is locked for this open file alone. `Ok(false)`: its file system
    /// has no locks.
    fn exclusive(&self, file: &fs::File) -> io::Result<bool>;
}

/// `flock(2)`.
#[derive(Debug, Clone, Copy)]
pub struct Flock;

impl Locks for Flock {
    fn exclusive(&self, file: &fs::File) -> io::Result<bool> {
        loop {
            // SAFETY: the descriptor is open for the call.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(true);
            }
            let error = io::Error::last_os_error();
            let code = error.raw_os_error().unwrap_or(0);
            if code == libc::EINTR {
                continue;
            }
            // Not a pattern: EOPNOTSUPP and ENOTSUP are one value on Linux.
            let unsupported = [libc::EBADF, libc::ENOLCK, libc::EOPNOTSUPP, libc::ENOTSUP];
            if unsupported.contains(&code) {
                return Ok(false);
            }
            return Err(error);
        }
    }
}

/// What to do on a file system that has no locks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lockless {
    /// Fail: what the lock protects is not worth losing (the registry, R3).
    Refuse,
    /// Go on without the lock (the injected settings and the shared instructions, R18).
    Proceed,
}

/// An exclusive lock, released when dropped.
#[derive(Debug)]
#[must_use = "the lock is released when this is dropped"]
pub struct Held(Option<fs::File>);

impl Held {
    fn take(file: fs::File, locks: &dyn Locks, lockless: Lockless) -> io::Result<Held> {
        if locks.exclusive(&file)? {
            return Ok(Held(Some(file)));
        }
        match lockless {
            Lockless::Proceed => Ok(Held(None)),
            Lockless::Refuse => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the file system has no locks",
            )),
        }
    }

    /// Whether there is a lock: `false` on a file system without locks ([`Lockless::Proceed`]).
    pub fn is_held(&self) -> bool {
        self.0.is_some()
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        if let Some(file) = &self.0 {
            // Released here, not left to the close: a child process started meanwhile holds
            // a copy of the descriptor until it execs, and the lock would last as long.
            // SAFETY: the descriptor is still open.
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

/// The state directory `dir` (`$REMUDA_HOME/state`), the user's alone ([`Dir::private`]).
/// What is above it is `$REMUDA_HOME`, the user's path: taken as given, and created as
/// needed. A `state` that is a symlink is written through, and the directory it points at
/// keeps its mode (R3, R13).
pub fn state(dir: &Path) -> Result<Dir, Blocked> {
    let (above, name) = split(dir).map_err(|e| Blocked::new(dir, Why::Open(e)))?;
    Dir::root(above)?.private_dir(name, true)
}

/// The directory `dir` (`state/settings`) of the injected settings, the user's alone, in the
/// state directory ([`state`]). remuda removes files there, so it is a directory of its own:
/// a symlink in its place is refused (R18).
pub fn settings(dir: &Path) -> Result<Dir, Blocked> {
    let (above, name) = split(dir).map_err(|e| Blocked::new(dir, Why::Open(e)))?;
    state(above)?.private(name)
}

/// `dir` (`shared/claude`) and the directory it is in (`shared`), each a directory of
/// remuda's own, made when missing (R13, R18). One that is a symlink is refused, like one
/// that is anything else: every write below it, and the removal of rule copies the source no
/// longer has, would happen where the link points. What is above `shared` is `$REMUDA_HOME`:
/// taken as given, and created as needed.
pub fn shared(dir: &Path) -> Result<Dir, Blocked> {
    let failed = |e| Blocked::new(dir, Why::Open(e));
    let (above, claude) = split(dir).map_err(failed)?;
    let (home, shared) = split(above).map_err(failed)?;
    Dir::root(home)?
        .sub_or_create(shared)?
        .sub_or_create(claude)
}

/// `homes/<provider>` below `root` (`$REMUDA_HOME`, taken as given and created as needed):
/// each level opened from the one above, never through a symlink, and made where it is
/// missing (R3, R13). A `homes` or `homes/<provider>` that is a symlink or not a directory
/// is refused, and nothing is made beyond it.
pub fn homes(root: &Path, provider: &str) -> Result<Dir, Blocked> {
    Dir::root(root)?
        .sub_or_create(HOMES)?
        .sub_or_create(provider)
}

/// What [`homes`] would refuse, creating nothing: a level that is missing is not refused.
pub fn check_homes(root: &Path, provider: &str) -> Result<(), Blocked> {
    let mut dir = match Dir::open(root) {
        Ok(dir) => dir,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Blocked::new(root, Why::Open(e))),
    };
    for level in [HOMES, provider] {
        dir = match dir.sub(level) {
            Ok(dir) => dir,
            Err(missing) if missing.is_missing() => return Ok(()),
            Err(blocked) => return Err(blocked),
        };
    }
    Ok(())
}

/// Where a write of the entry `name` of `dir` goes when that entry is a symlink (R3): the
/// directory of the file it points at, and that file's name there. `None`: it is not one,
/// and `name` in `dir` is the file. This is the one step by path: where a link of the
/// user's leads is the user's choice.
fn through(dir: &Dir, name: &OsStr) -> io::Result<Option<(Dir, OsString)>> {
    if dir.kind(name)? != Some(Kind::Symlink) {
        return Ok(None);
    }
    let target = fs::canonicalize(dir.path.join(name))?;
    let (above, file) = split(&target)?;
    Ok(Some((Dir::open(above)?, file.to_os_string())))
}

/// Replaces the file `name` of `dir` atomically, after removing what a killed write left
/// there ([`Dir::sweep`]): it keeps the mode it has, or gets the default one when it is new.
fn replace_keeping_mode(dir: &Dir, name: &OsStr, bytes: &[u8]) -> io::Result<()> {
    dir.sweep();
    match dir.meta(name)? {
        Some(meta) => dir.put(name, bytes, PRIVATE_FILE, Some(meta.mode), true),
        None => dir.put(name, bytes, DEFAULT_FILE, None, true),
    }
}

/// Changes the registry `config` (`$REMUDA_HOME/config.toml`) under an exclusive lock (R3):
/// `edit` reads the file, checks, and returns the new text, which is written atomically;
/// `None` leaves the file as it is. The lock spans the reading and the writing, so changes
/// made at the same time each see the other's. On a file system without locks nothing is
/// changed: that is an error. A `config` that is a symlink is written through, and existing
/// permissions are kept.
///
/// The lock is on the directory of the file that is replaced: `config`'s own or, where
/// `config` is a symlink, that of the file it points at. So every path to one registry takes
/// the same lock (two `$REMUDA_HOME`s that share one through a link), and the file is
/// written through the descriptor the lock is on.
///
/// `edit` may run twice and must have no effect of its own: where the directory does not
/// exist there is nothing to lock or to read, so it is asked first, and the directory is
/// created only for a change it accepts.
pub fn update_registry(
    config: &Path,
    locks: &dyn Locks,
    edit: impl Fn() -> Result<Option<String>>,
) -> Result<()> {
    let (above, name) = split(config)?;
    let home = match Dir::open(above) {
        Ok(home) => home,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if edit()?.is_none() {
                return Ok(());
            }
            Dir::root(above)?
        }
        Err(e) => return Err(e).with_context(|| format!("cannot open {}", above.display())),
    };
    let linked =
        through(&home, name).with_context(|| format!("cannot write {}", config.display()))?;
    let (dir, file) = match &linked {
        Some((there, file)) => (there, file.as_os_str()),
        None => (&home, name),
    };
    let _lock = dir.lock(locks, Lockless::Refuse).with_context(|| {
        format!(
            "cannot lock {} to change {}",
            dir.path().display(),
            config.display()
        )
    })?;
    // Looked at again under the lock: while this waited for it, the link may have been
    // pointed elsewhere, and then the lock held is not that of the file `edit` would read.
    let now = through(&home, name).with_context(|| format!("cannot write {}", config.display()))?;
    let unchanged = match (&linked, &now) {
        (None, None) => true,
        (Some((before, was)), Some((after, is))) => was == is && before.is(after),
        _ => false,
    };
    if !unchanged {
        bail!(
            "{} was pointed elsewhere while it was being changed; nothing was written",
            config.display()
        );
    }
    let Some(text) = edit()? else {
        return Ok(());
    };
    replace_keeping_mode(dir, file, text.as_bytes())
        .with_context(|| format!("cannot write {}", config.display()))
}

/// Replaces the cache `path`, a file of the state directory ([`state`]), atomically (R3): the
/// file is readable and writable by the user alone (mode 0600) from the moment it is created,
/// whatever the mode of the one it replaces. Only a `path` that is a symlink is written
/// through, and keeps the mode of the file it points at. What a killed write left where the
/// file goes is removed first ([`Dir::sweep`]).
pub fn save_cache(path: &Path, bytes: &[u8]) -> Result<()> {
    let (dir, name) = split(path)?;
    let dir = state(dir)?;
    match through(&dir, name)? {
        Some((there, file)) => replace_keeping_mode(&there, &file, bytes)?,
        None => {
            dir.sweep();
            dir.put(name, bytes, PRIVATE_FILE, None, true)?;
        }
    }
    Ok(())
}

/// Appends `line` to the launch log `log`, a file of the state directory ([`state`]). The log
/// holds the arguments as typed, prompts among them, so it is the user's alone (R3): created
/// with mode 0600, and one from before tightened to that. A log that is a symlink is written
/// through, and the file it points at keeps its mode. Nothing is appended to a log that is
/// not a regular file, or that the group or others can still access after that
/// ([`refuse_shared`]): that is an error, like a log that cannot be written. The log is
/// opened without blocking, so a FIFO in its place fails or is refused instead of holding up
/// the launch.
///
/// The directory is tightened as far as it can be and is not checked again: its mode does
/// not give away what is in a file of mode 0600. Like the other writes to the state
/// directory, this one removes what a killed write left there ([`Dir::sweep`]).
pub fn append_log(log: &Path, line: &str) -> Result<()> {
    let (dir, name) = split(log)?;
    let dir = state(dir)?;
    dir.sweep();
    // One write on an O_APPEND file: concurrent launches do not interleave lines.
    let flags = libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT | libc::O_NONBLOCK;
    let mut file = dir.open_at(&entry_name(name)?, flags, PRIVATE_FILE)?;
    // Tightened only as the regular file at `log` itself, which the open file is checked to be.
    if let (Ok(Some(at)), Ok(open)) = (dir.meta(name), file.metadata())
        && at.kind == Kind::File
        && at.file == (open.dev(), open.ino())
    {
        tighten(&file, PRIVATE_FILE);
    }
    refuse_shared(&file, log)?;
    file.write_all(line.as_bytes())?;
    Ok(())
}

/// Fails when the open launch log is not a regular file (a FIFO, a socket or a device would
/// pass the line to whoever reads it), or is one the group or others have any access to: one
/// that could not be tightened (it belongs to another user), or the target of a symlink, which
/// keeps its mode (R3). Read from the open file, so it is the file the line would go to.
fn refuse_shared(file: &fs::File, log: &Path) -> Result<()> {
    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        bail!(
            "{} is not a regular file; nothing was appended",
            log.display()
        );
    }
    let mode = meta.mode() & 0o7777;
    if mode & 0o077 != 0 {
        bail!(
            "{} can be accessed by the group or others (mode {mode:04o}) and was not made \
             private; nothing was appended",
            log.display()
        );
    }
    Ok(())
}

/// A file system without locks: the other side of the [`Locks`] seam, for tests.
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub struct NoLocks;

#[cfg(test)]
impl Locks for NoLocks {
    fn exclusive(&self, _file: &fs::File) -> io::Result<bool> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{OpenOptionsExt, symlink};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    use super::*;

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn chmod(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// Everything below `dir`, as `(path, what it is)`: a write anywhere shows.
    fn tree(dir: &Path) -> Vec<(PathBuf, String)> {
        let mut out = Vec::new();
        for name in entries(dir) {
            let path = dir.join(&name);
            let meta = fs::symlink_metadata(&path).unwrap();
            let kind = if meta.file_type().is_symlink() {
                format!("-> {}", fs::read_link(&path).unwrap().display())
            } else if meta.is_dir() {
                out.extend(tree(&path));
                "dir".to_string()
            } else {
                format!("file {}", meta.len())
            };
            out.push((path, kind));
        }
        out
    }

    /// A directory the user may search and write but not read (mode 0300) until this is
    /// dropped, so that a failed assertion leaves a tree its `TempDir` can still remove.
    struct Unreadable(PathBuf);

    impl Unreadable {
        fn new(path: &Path) -> Unreadable {
            chmod(path, 0o300);
            Unreadable(path.to_path_buf())
        }
    }

    impl Drop for Unreadable {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
        }
    }

    /// A process that has exited and been waited for: its temporary files are leftovers.
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    fn temp_of(pid: u32) -> String {
        format!(".remuda-{pid}-{}.tmp", uuid::Uuid::new_v4().simple())
    }

    fn write(config: &Path, text: &str) -> Result<()> {
        update_registry(config, &Flock, || Ok(Some(text.to_string())))
    }

    /// R3, R13: the directories remuda creates, replaces and removes in are opened from
    /// `$REMUDA_HOME` one level at a time, and a level that is a symlink (to a directory with
    /// the user's files, to an empty one, to nothing) or a file is refused: nothing is
    /// written where the link points, or next to it. A missing level is made.
    #[test]
    fn a_directory_of_remudas_own_is_never_opened_through_a_symlink() {
        type Open = fn(&Path) -> Result<Dir, Blocked>;
        let roots: [(&str, &str, Open); 6] = [
            ("shared", "shared/claude", |home| {
                shared(&home.join("shared/claude"))
            }),
            ("shared/claude", "shared/claude", |home| {
                shared(&home.join("shared/claude"))
            }),
            ("homes", "homes/claude", |home| homes(home, "claude")),
            ("homes/claude", "homes/claude", |home| homes(home, "claude")),
            ("state/settings", "state/settings", |home| {
                settings(&home.join("state/settings"))
            }),
            ("x", "x", |home| Dir::open(home).unwrap().sub_or_create("x")),
        ];
        for (level, whole, open) in roots {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("remuda");
            let at = home.join(level);
            fs::create_dir_all(at.parent().unwrap()).unwrap();
            let outside = dir.path().join("outside");
            fs::create_dir_all(outside.join("claude")).unwrap();
            fs::write(outside.join("claude/mine"), "mine").unwrap();
            let empty = dir.path().join("empty");
            fs::create_dir(&empty).unwrap();

            for target in [&outside, &empty, &dir.path().join("missing")] {
                symlink(target, &at).unwrap();
                let before = tree(dir.path());
                let blocked = open(&home).unwrap_err();
                assert!(matches!(blocked.why, Why::Symlink), "{level}: {blocked:?}");
                assert_eq!(blocked.path, at, "{level}");
                assert_eq!(
                    blocked.to_string(),
                    format!(
                        "{} is a symlink; remuda does not write through it",
                        at.display()
                    )
                );
                assert_eq!(tree(dir.path()), before, "{level} -> {}", target.display());
                fs::remove_file(&at).unwrap();
            }

            fs::write(&at, "a file").unwrap();
            let before = tree(dir.path());
            let blocked = open(&home).unwrap_err();
            assert!(
                matches!(blocked.why, Why::NotADirectory),
                "{level}: {blocked:?}"
            );
            assert_eq!(
                blocked.to_string(),
                format!(
                    "{} is not a directory; remuda does not replace it",
                    at.display()
                )
            );
            assert_eq!(tree(dir.path()), before, "{level}");

            // Missing: made, a real directory at every level.
            fs::remove_file(&at).unwrap();
            let opened = open(&home).unwrap();
            assert_eq!(opened.path(), home.join(whole), "{level}");
            for path in [&at, &home.join(whole)] {
                assert!(fs::symlink_metadata(path).unwrap().file_type().is_dir());
            }
        }
    }

    /// R13: a check that creates nothing refuses the same levels, and a missing one passes.
    #[test]
    fn the_levels_of_a_home_are_checked_without_creating_any() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("remuda");
        check_homes(&home, "claude").unwrap();
        assert!(!home.exists());
        fs::create_dir(&home).unwrap();
        check_homes(&home, "claude").unwrap();
        fs::create_dir(home.join("homes")).unwrap();
        check_homes(&home, "claude").unwrap();
        assert_eq!(entries(&home.join("homes")), [] as [&str; 0]);
        symlink(dir.path(), home.join("homes/claude")).unwrap();
        let blocked = check_homes(&home, "claude").unwrap_err();
        assert!(matches!(blocked.why, Why::Symlink), "{blocked:?}");
        check_homes(&home, "codex").unwrap();
    }

    /// R13: what is done through a directory is done in the directory that was opened. A
    /// process of the same user that moves it away and puts a symlink at its path meanwhile
    /// redirects nothing: the file, the link, the directory and the removal all land in the
    /// directory remuda opened, and nothing where the symlink points.
    #[test]
    fn a_write_lands_in_the_directory_that_was_opened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remuda/shared/claude");
        let opened = shared(&path).unwrap();
        opened
            .write("stale.md", b"old", FileMode::ReadOnly)
            .unwrap();

        let moved = dir.path().join("moved");
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("stale.md"), "the user's").unwrap();
        fs::rename(&path, &moved).unwrap();
        symlink(&outside, &path).unwrap();
        let before = tree(&outside);

        opened
            .write("rule.md", b"be terse", FileMode::ReadOnly)
            .unwrap();
        opened
            .replace_link("skills", Path::new("/source/skills"))
            .unwrap();
        opened.link("agents", Path::new("/source/agents")).unwrap();
        opened
            .sub_or_create("rules")
            .unwrap()
            .write("deep.md", b"deep", FileMode::ReadOnly)
            .unwrap();
        opened.remove_file("stale.md").unwrap();
        opened.sweep();
        assert_eq!(tree(&outside), before);
        assert_eq!(entries(&moved), ["agents", "rule.md", "rules", "skills"]);
        assert_eq!(fs::read(moved.join("rules/deep.md")).unwrap(), b"deep");
    }

    /// An entry is named by one name: never a path, which could lead out of the directory.
    #[test]
    fn an_entry_is_one_name() {
        let dir = tempfile::tempdir().unwrap();
        let opened = Dir::open(dir.path()).unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        for name in ["", ".", "..", "a/b", "/etc", "../x", "a/"] {
            let invalid =
                |e: io::Error| assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{name:?}");
            invalid(opened.write(name, b"x", FileMode::Private).unwrap_err());
            invalid(opened.link(name, Path::new("/t")).unwrap_err());
            invalid(opened.replace_link(name, Path::new("/t")).unwrap_err());
            invalid(opened.remove_file(name).unwrap_err());
            invalid(opened.remove_dir(name).unwrap_err());
            invalid(opened.remove_tree(name).unwrap_err());
            invalid(opened.rename("a", name).unwrap_err());
            invalid(opened.meta(name).unwrap_err());
            invalid(opened.touch(name, SystemTime::now()).unwrap_err());
            assert!(opened.sub(name).is_err(), "{name:?}");
            assert!(opened.sub_or_create(name).is_err(), "{name:?}");
            assert!(opened.create_new(name).is_err(), "{name:?}");
            assert!(opened.private(name).is_err(), "{name:?}");
        }
        assert_eq!(entries(dir.path()), ["a"]);
        assert_eq!(entries(&dir.path().join("a")), ["b"]);
    }

    /// What a directory holds is read through its descriptor, without following anything.
    #[test]
    fn entries_are_read_without_following_them() {
        let dir = tempfile::tempdir().unwrap();
        let opened = Dir::open(dir.path()).unwrap();
        assert!(opened.is_empty().unwrap());
        assert_eq!(opened.kind("nothing").unwrap(), None);
        fs::create_dir(dir.path().join("d")).unwrap();
        fs::write(dir.path().join("f"), "x").unwrap();
        chmod(&dir.path().join("f"), 0o640);
        symlink(dir.path().join("d"), dir.path().join("to-dir")).unwrap();
        let long = dir.path().join("x".repeat(200)).join("y".repeat(200));
        symlink(&long, dir.path().join("dangling")).unwrap();
        let mut names = opened.names().unwrap();
        names.sort();
        assert_eq!(names, ["d", "dangling", "f", "to-dir"]);
        assert!(!opened.is_empty().unwrap());
        assert_eq!(opened.kind("d").unwrap(), Some(Kind::Dir));
        assert_eq!(opened.kind("to-dir").unwrap(), Some(Kind::Symlink));
        assert_eq!(opened.kind("dangling").unwrap(), Some(Kind::Symlink));
        assert_eq!(opened.read_link("dangling").unwrap(), long);
        assert!(opened.read_link("f").is_err());
        let meta = opened.meta("f").unwrap().unwrap();
        assert_eq!((meta.kind, meta.mode), (Kind::File, 0o640));
        assert_eq!(
            meta.modified,
            fs::metadata(dir.path().join("f"))
                .unwrap()
                .modified()
                .unwrap()
        );
        let then = SystemTime::now() - Duration::from_secs(40 * 24 * 60 * 60);
        opened.touch("f", then).unwrap();
        assert_eq!(opened.meta("f").unwrap().unwrap().modified, then);
        // Never through a link, and never a directory.
        assert!(opened.touch("to-dir", then).is_err());
        assert!(opened.touch("dangling", then).is_err());
        assert!(matches!(
            opened.sub("to-dir").unwrap_err().why,
            Why::Symlink
        ));
        assert!(matches!(
            opened.sub("f").unwrap_err().why,
            Why::NotADirectory
        ));
        assert!(opened.sub("nothing").unwrap_err().is_missing());
    }

    /// A file is replaced by a rename, with the mode asked for whatever it had, and no
    /// temporary file stays, after a success or after a failure, which leaves what was there.
    #[test]
    fn a_file_is_replaced_atomically_with_its_mode() {
        let dir = tempfile::tempdir().unwrap();
        let opened = Dir::open(dir.path()).unwrap();
        let file = dir.path().join("f");
        opened.write("f", b"one", FileMode::Private).unwrap();
        assert_eq!(mode(&file), 0o600);
        chmod(&file, 0o644);
        let before = fs::metadata(&file).unwrap().ino();
        opened.write("f", b"two", FileMode::Private).unwrap();
        assert_eq!(
            (mode(&file), fs::read(&file).unwrap()),
            (0o600, b"two".to_vec())
        );
        assert_ne!(
            fs::metadata(&file).unwrap().ino(),
            before,
            "a new file, renamed"
        );
        // A read-only copy is replaced all the same: the rename does not write to it.
        opened.write("f", b"three", FileMode::ReadOnly).unwrap();
        assert_eq!(mode(&file), 0o400);
        opened.write("f", b"four", FileMode::ReadOnly).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"four");
        // A symlink in the file's place is replaced, not followed.
        let target = dir.path().join("target");
        fs::write(&target, "kept").unwrap();
        fs::remove_file(&file).unwrap();
        symlink(&target, &file).unwrap();
        opened.write("f", b"five", FileMode::Private).unwrap();
        assert!(fs::symlink_metadata(&file).unwrap().is_file());
        assert_eq!(fs::read(&target).unwrap(), b"kept");
        assert_eq!(entries(dir.path()), ["f", "target"]);

        // A directory holding something cannot be renamed over: the write fails and cleans up.
        fs::create_dir(dir.path().join("d")).unwrap();
        fs::write(dir.path().join("d/keep"), "x").unwrap();
        assert!(opened.write("d", b"x", FileMode::Private).is_err());
        assert!(opened.replace_link("d", Path::new("/t")).is_err());
        assert_eq!(entries(dir.path()), ["d", "f", "target"]);
        assert_eq!(entries(&dir.path().join("d")), ["keep"]);

        // A link is replaced the same way; `link` never replaces.
        opened.replace_link("l", Path::new("/one")).unwrap();
        opened.replace_link("l", Path::new("/two")).unwrap();
        assert_eq!(
            fs::read_link(dir.path().join("l")).unwrap(),
            Path::new("/two")
        );
        let e = opened.link("l", Path::new("/three")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(entries(dir.path()), ["d", "f", "l", "target"]);
    }

    /// A tree is removed without following its links: what they point at stays.
    #[test]
    fn a_tree_is_removed_without_following_links() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), "x").unwrap();
        let built = dir.path().join("built");
        fs::create_dir_all(built.join("rules/lang")).unwrap();
        fs::write(built.join("rules/lang/rust.md"), "x").unwrap();
        chmod(&built.join("rules/lang/rust.md"), 0o400);
        symlink(&outside, built.join("skills")).unwrap();
        symlink(outside.join("keep"), built.join("CLAUDE.md")).unwrap();
        let opened = Dir::open(dir.path()).unwrap();
        opened.remove_tree("built").unwrap();
        opened.remove_tree("built").unwrap();
        assert_eq!(entries(dir.path()), ["outside"]);
        assert_eq!(entries(&outside), ["keep"]);
        // A link itself is removed as an entry.
        symlink(&outside, dir.path().join("link")).unwrap();
        opened.remove_tree("link").unwrap();
        assert_eq!(entries(&outside), ["keep"]);
    }

    /// R13: a new directory is made once. The private one is the user's alone whatever the
    /// umask left, and one that exists, whatever it is, is refused and left alone.
    #[test]
    fn a_new_directory_must_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let opened = Dir::open(dir.path()).unwrap();
        let home = opened.create_private("work").unwrap();
        assert_eq!(home.path(), dir.path().join("work"));
        assert_eq!(mode(&dir.path().join("work")), 0o700);
        assert!(home.is_empty().unwrap());
        opened.create_new("staged").unwrap();
        symlink(dir.path(), dir.path().join("link")).unwrap();
        fs::write(dir.path().join("file"), "").unwrap();
        for name in ["work", "staged", "link", "file"] {
            for blocked in [
                opened.create_private(name).unwrap_err(),
                opened.create_new(name).unwrap_err(),
            ] {
                assert!(
                    matches!(&blocked.why, Why::Create(e) if e.kind() == io::ErrorKind::AlreadyExists),
                    "{name}: {blocked:?}"
                );
                assert_eq!(
                    blocked.to_string(),
                    format!("cannot create {}", dir.path().join(name).display())
                );
            }
        }
        assert_eq!(entries(dir.path()), ["file", "link", "staged", "work"]);
    }

    /// R3: a file of `state/` is written with mode 0600 in a directory of mode 0700, whatever
    /// they were before; `config.toml` keeps the mode it has, and a new one gets the default.
    #[test]
    fn state_files_are_the_users_alone() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("remuda");
        let state_path = home.join("state");
        let file = state_path.join("index.json");
        save_cache(&file, b"one").unwrap();
        assert_eq!(mode(&state_path), 0o700);
        assert_eq!(mode(&file), 0o600);

        chmod(&state_path, 0o755);
        chmod(&file, 0o644);
        save_cache(&file, b"two").unwrap();
        assert_eq!(mode(&state_path), 0o700);
        assert_eq!(mode(&file), 0o600);
        assert_eq!(fs::read(&file).unwrap(), b"two");
        assert_eq!(
            entries(&state_path),
            ["index.json"],
            "no temporary file is left"
        );

        // Tightened, never loosened.
        chmod(&state_path, 0o2770);
        state(&state_path).unwrap();
        assert_eq!(mode(&state_path), 0o700);
        chmod(&state_path, 0o500);
        state(&state_path).unwrap();
        assert_eq!(mode(&state_path), 0o500);
        chmod(&state_path, 0o700);

        // The settings directory in it is private the same way.
        let settings_path = state_path.join("settings");
        settings(&settings_path).unwrap();
        assert_eq!(mode(&settings_path), 0o700);
        chmod(&settings_path, 0o755);
        chmod(&state_path, 0o755);
        settings(&settings_path).unwrap();
        assert_eq!((mode(&settings_path), mode(&state_path)), (0o700, 0o700));
        fs::remove_dir(&settings_path).unwrap();

        let config = home.join("config.toml");
        fs::write(&config, "").unwrap();
        chmod(&config, 0o640);
        chmod(&home, 0o755);
        write(&config, "# kept\n").unwrap();
        assert_eq!(mode(&config), 0o640);
        assert_eq!(mode(&home), 0o755);
        assert_eq!(fs::read_to_string(&config).unwrap(), "# kept\n");
        assert_eq!(entries(&home), ["config.toml", "state"]);

        // A new one: the mode any new file gets here.
        fs::remove_file(&config).unwrap();
        fs::write(home.join("other"), "").unwrap();
        write(&config, "# new\n").unwrap();
        assert_eq!(mode(&config), mode(&home.join("other")));
    }

    /// R3: no mode is changed through a symlink: a `state` that is one keeps its target's (a
    /// file written in it is private all the same), and a file that is one keeps its target's.
    /// Both are written through, and stay links.
    #[test]
    fn state_modes_are_not_changed_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("remuda");
        fs::create_dir(&home).unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        chmod(&outside, 0o755);
        fs::write(outside.join("index.json"), "old").unwrap();
        chmod(&outside.join("index.json"), 0o644);
        let state_path = home.join("state");
        symlink(&outside, &state_path).unwrap();
        for file in ["index.json", "stats.json"] {
            save_cache(&state_path.join(file), b"new").unwrap();
            assert_eq!(fs::read(outside.join(file)).unwrap(), b"new");
            assert_eq!(mode(&outside.join(file)), 0o600, "{file}");
        }
        assert_eq!(mode(&outside), 0o755);
        assert_eq!(fs::read_link(&state_path).unwrap(), outside);

        fs::remove_file(&state_path).unwrap();
        fs::create_dir(&state_path).unwrap();
        chmod(&state_path, 0o755);
        let target = outside.join("elsewhere.json");
        fs::write(&target, "old").unwrap();
        chmod(&target, 0o644);
        let link = state_path.join("stats.json");
        symlink(&target, &link).unwrap();
        save_cache(&link, b"new").unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), target);
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(mode(&target), 0o644);
        assert_eq!(mode(&outside), 0o755);
        assert_eq!(mode(&state_path), 0o700);

        // A link to nothing has nothing to write through to.
        fs::remove_file(&target).unwrap();
        assert!(save_cache(&link, b"new").is_err());
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!target.exists());
    }

    /// R3: a `config.toml` that is a symlink is written through: the file it points at is
    /// replaced in its own directory and keeps its mode, the link stays, and no temporary
    /// file is left on either side.
    #[test]
    fn the_registry_is_written_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("remuda");
        let dotfiles = dir.path().join("dotfiles");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&dotfiles).unwrap();
        let target = dotfiles.join("remuda.toml");
        fs::write(&target, "# tracked\n").unwrap();
        chmod(&target, 0o604);
        let config = home.join("config.toml");
        symlink(&target, &config).unwrap();
        write(&config, "# changed\n").unwrap();
        assert_eq!(fs::read_link(&config).unwrap(), target);
        assert_eq!(fs::read_to_string(&target).unwrap(), "# changed\n");
        assert_eq!(mode(&target), 0o604);
        assert_eq!(entries(&home), ["config.toml"]);
        assert_eq!(entries(&dotfiles), ["remuda.toml"]);
    }

    /// R3 (review #18): a write that was killed leaves its temporary file; the next write in
    /// that directory removes it once its process is gone. One whose process is running is
    /// being written and stays, however old; so does everything that is not named as
    /// remuda's temporary files are, and a directory, whatever its name.
    #[test]
    fn what_a_killed_write_left_is_removed_by_the_next_one() {
        let gone = dead_pid();
        let seed = |dir: &Path| -> Vec<String> {
            let mut kept = vec![
                temp_of(std::process::id()),
                // Another user's process: it cannot be asked, so it is running.
                temp_of(1),
                ".stats.json.0123456789abcdef0123456789abcdef.tmp".to_string(),
                format!(".remuda-{gone}-short.tmp"),
                format!(".remuda-{gone}-{}.tmp.md", "a".repeat(32)),
                format!("remuda-{gone}-{}.tmp", "a".repeat(32)),
                "notes.tmp".to_string(),
            ];
            for name in &kept {
                fs::write(dir.join(name), "theirs").unwrap();
                let old = SystemTime::now() - Duration::from_secs(400 * 24 * 60 * 60);
                let file = fs::File::options()
                    .write(true)
                    .open(dir.join(name))
                    .unwrap();
                file.set_modified(old).unwrap();
            }
            let a_directory = temp_of(gone);
            fs::create_dir(dir.join(&a_directory)).unwrap();
            kept.push(a_directory);
            // The leftovers: a file, a private one, and a link.
            fs::write(dir.join(temp_of(gone)), "half a cache").unwrap();
            fs::File::options()
                .write(true)
                .create_new(true)
                .mode(0o000)
                .open(dir.join(temp_of(gone)))
                .unwrap();
            symlink("/source/skills", dir.join(temp_of(gone))).unwrap();
            kept.sort();
            kept
        };
        let left = |dir: &Path, written: &[&str]| -> Vec<String> {
            let mut names: Vec<String> = entries(dir)
                .into_iter()
                .filter(|name| !written.contains(&name.as_str()))
                .collect();
            names.sort();
            names
        };
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("remuda");

        // A cache, in `state/`.
        let state_path = home.join("state");
        fs::create_dir_all(&state_path).unwrap();
        let kept = seed(&state_path);
        assert_eq!(entries(&state_path).len(), kept.len() + 3);
        save_cache(&state_path.join("stats.json"), b"{}").unwrap();
        assert_eq!(left(&state_path, &["stats.json"]), kept);

        // The registry, in `$REMUDA_HOME`.
        let kept = seed(&home);
        let config = home.join("config.toml");
        write(&config, "# one\n").unwrap();
        assert_eq!(left(&home, &["config.toml", "state"]), kept);

        // The registry through a symlink: where the file is, which is where it is written.
        let dotfiles = dir.path().join("dotfiles");
        fs::create_dir(&dotfiles).unwrap();
        fs::rename(&config, dotfiles.join("remuda.toml")).unwrap();
        symlink(dotfiles.join("remuda.toml"), &config).unwrap();
        let kept = seed(&dotfiles);
        write(&config, "# two\n").unwrap();
        assert_eq!(left(&dotfiles, &["remuda.toml"]), kept);

        // Nothing is removed by a write that changes nothing.
        seed(&dotfiles);
        let before = entries(&dotfiles);
        update_registry(&config, &Flock, || Ok(None)).unwrap();
        assert_eq!(entries(&dotfiles), before);
    }

    /// The one naming scheme of temporary files, and what is not one.
    #[test]
    fn temporary_names_name_their_process() {
        let name = temp_name().into_string().unwrap();
        assert_eq!(temp_pid(OsStr::new(&name)), Some(std::process::id()));
        assert!(is_temp_name(OsStr::new(&name)));
        assert_eq!(
            name.len(),
            ".remuda--.tmp".len() + 32 + std::process::id().to_string().len()
        );
        assert!(name.starts_with('.') && !name.ends_with(".md") && !name.ends_with(".json"));
        let hex = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            temp_pid(OsStr::new(&format!(".remuda-42-{hex}.tmp"))),
            Some(42)
        );
        for not in [
            format!(".remuda-42-{hex}"),
            format!("remuda-42-{hex}.tmp"),
            format!(".remuda--{hex}.tmp"),
            format!(".remuda-4x-{hex}.tmp"),
            format!(".remuda-42-{}.tmp", &hex[1..]),
            format!(".remuda-42-{}.tmp", hex.to_uppercase()),
            format!(".remuda-99999999999-{hex}.tmp"),
            format!(".{hex}.tmp"),
            format!(".config.toml.{hex}.tmp"),
        ] {
            assert!(!is_temp_name(OsStr::new(&not)), "{not}");
        }
        assert!(alive(std::process::id()));
        assert!(alive(1) && alive(0) && alive(u32::MAX));
        assert!(!alive(dead_pid()));
    }

    /// A lock excludes a second holder until it is dropped, between descriptors of one
    /// process too; the lock file is a regular file that is the user's alone, and stays.
    #[test]
    fn a_lock_is_exclusive_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let opened = Dir::open(dir.path()).unwrap();
        let try_lock = |file: &fs::File| {
            // SAFETY: the descriptor is open for the call.
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }
        };

        let held = opened.lock(&Flock, Lockless::Refuse).unwrap();
        assert!(held.is_held());
        assert_eq!(
            entries(dir.path()),
            [] as [&str; 0],
            "nothing is created for it"
        );
        let other = fs::File::open(dir.path()).unwrap();
        assert_eq!(try_lock(&other), -1, "held elsewhere");
        drop(held);
        assert_eq!(try_lock(&other), 0);
        drop(other);

        let held = opened
            .lock_file(".lock", &Flock, Lockless::Proceed)
            .unwrap();
        assert!(held.is_held());
        let lock = dir.path().join(".lock");
        assert!(fs::symlink_metadata(&lock).unwrap().is_file());
        assert_eq!(mode(&lock), 0o600);
        let other = fs::File::open(&lock).unwrap();
        assert_eq!(try_lock(&other), -1, "held elsewhere");
        drop(held);
        assert_eq!(try_lock(&other), 0);
        assert_eq!(entries(dir.path()), [".lock"]);

        // Waited for, not skipped: a second taker gets it only after the first let go.
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let first = opened.lock(&Flock, Lockless::Refuse).unwrap();
        let waiter = {
            let (path, order) = (dir.path().to_path_buf(), order.clone());
            std::thread::spawn(move || {
                let dir = Dir::open(&path).unwrap();
                let _held = dir.lock(&Flock, Lockless::Refuse).unwrap();
                order.lock().unwrap().push("second");
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        order.lock().unwrap().push("first");
        drop(first);
        waiter.join().unwrap();
        assert_eq!(*order.lock().unwrap(), ["first", "second"]);
    }

    /// On a file system without locks, a lock that must be had is refused, and one that is
    /// only wanted is gone without (R3, R18).
    #[test]
    fn without_locks_a_lock_is_refused_or_gone_without() {
        let dir = tempfile::tempdir().unwrap();
        let opened = Dir::open(dir.path()).unwrap();
        let e = opened.lock(&NoLocks, Lockless::Refuse).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Unsupported);
        assert_eq!(e.to_string(), "the file system has no locks");
        assert!(
            opened
                .lock_file(".lock", &NoLocks, Lockless::Refuse)
                .is_err()
        );
        assert!(!opened.lock(&NoLocks, Lockless::Proceed).unwrap().is_held());
        let held = opened
            .lock_file(".lock", &NoLocks, Lockless::Proceed)
            .unwrap();
        assert!(!held.is_held());
    }

    /// R3 (review #9): the registry is not changed without its lock. On a file system that
    /// has none, the change is refused before the file is read: nothing is written, and the
    /// file, its directory and the caller's edit are left alone.
    #[test]
    fn the_registry_is_not_changed_without_a_lock() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("remuda");
        fs::create_dir(&home).unwrap();
        let config = home.join("config.toml");
        fs::write(&config, "# as it was\n").unwrap();
        let asked = AtomicUsize::new(0);
        let edit = || {
            asked.fetch_add(1, Ordering::Relaxed);
            Ok(Some("# changed\n".to_string()))
        };
        let e = update_registry(&config, &NoLocks, edit).unwrap_err();
        assert_eq!(
            format!("{e:#}"),
            format!(
                "cannot lock {} to change {}: the file system has no locks",
                home.display(),
                config.display()
            )
        );
        assert_eq!(asked.load(Ordering::Relaxed), 0);
        assert_eq!(fs::read_to_string(&config).unwrap(), "# as it was\n");
        assert_eq!(entries(&home), ["config.toml"]);
        // A check that writes nothing is refused the same: it would not be able to write.
        assert!(update_registry(&config, &NoLocks, || Ok(None)).is_err());
        update_registry(&config, &Flock, edit).unwrap();
        assert_eq!(fs::read_to_string(&config).unwrap(), "# changed\n");
    }

    /// R3: a change that is refused, or that changes nothing, creates nothing: not
    /// `$REMUDA_HOME`, not a file in it. One that is accepted creates what it needs.
    #[test]
    fn a_refused_change_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("deep/remuda");
        let config = home.join("config.toml");
        let e = update_registry(&config, &Flock, || bail!("not registered")).unwrap_err();
        assert_eq!(e.to_string(), "not registered");
        update_registry(&config, &Flock, || Ok(None)).unwrap();
        assert_eq!(entries(dir.path()), [] as [&str; 0]);

        fs::create_dir_all(&home).unwrap();
        assert!(update_registry(&config, &Flock, || bail!("not registered")).is_err());
        update_registry(&config, &Flock, || Ok(None)).unwrap();
        assert_eq!(entries(&home), [] as [&str; 0]);
        fs::remove_dir(&home).unwrap();

        // Where there was no directory, the edit is asked before one is made, and again
        // under the lock.
        let asked = AtomicUsize::new(0);
        update_registry(&config, &Flock, || {
            asked.fetch_add(1, Ordering::Relaxed);
            Ok(Some("# new\n".to_string()))
        })
        .unwrap();
        assert_eq!(asked.load(Ordering::Relaxed), 2);
        assert_eq!(fs::read_to_string(&config).unwrap(), "# new\n");
        assert_eq!(entries(&home), ["config.toml"]);
    }

    /// R3 (review #9): changes made at the same time are all kept. Each reads the file and
    /// writes it back with a line of its own; the lock spans the two, so none is written
    /// over by another that read before it.
    #[test]
    fn registry_changes_at_the_same_time_are_all_kept() {
        const WRITERS: usize = 16;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("remuda/config.toml");
        let barrier = Arc::new(Barrier::new(WRITERS));
        let threads: Vec<_> = (0..WRITERS)
            .map(|n| {
                let (config, barrier) = (config.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    update_registry(&config, &Flock, || {
                        let text = fs::read_to_string(&config).unwrap_or_default();
                        // Long enough for every other writer to read the same text.
                        std::thread::sleep(Duration::from_millis(5));
                        Ok(Some(format!("{text}line {n}\n")))
                    })
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        let text = fs::read_to_string(&config).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        lines.sort();
        let mut want: Vec<String> = (0..WRITERS).map(|n| format!("line {n}")).collect();
        want.sort();
        assert_eq!(lines, want);
        assert_eq!(entries(config.parent().unwrap()), ["config.toml"]);
    }

    /// R3: each write cleans the directory it writes in, and no other. A cache and a launch
    /// log that are symlinks to files in a directory elsewhere: a line appended to the log
    /// cleans `state/`, where the log is named, not where it points; what a cache left there
    /// is removed by the next cache saved there.
    #[test]
    fn a_linked_log_cleans_where_it_is_named_and_a_linked_cache_where_it_points() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("remuda/state");
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir_all(&state_path).unwrap();
        fs::create_dir(&elsewhere).unwrap();
        for file in ["stats.json", "launches.jsonl"] {
            fs::write(elsewhere.join(file), "").unwrap();
            chmod(&elsewhere.join(file), 0o600);
            symlink(elsewhere.join(file), state_path.join(file)).unwrap();
        }
        let gone = dead_pid();
        let (here, there) = (
            state_path.join(temp_of(gone)),
            elsewhere.join(temp_of(gone)),
        );
        fs::write(&here, "half a cache").unwrap();
        fs::write(&there, "half a cache").unwrap();

        append_log(&launch_log(&state_path), "launch\n").unwrap();
        assert_eq!(
            fs::read_to_string(elsewhere.join("launches.jsonl")).unwrap(),
            "launch\n"
        );
        assert!(
            !here.exists(),
            "state/ is cleaned by the line appended there"
        );
        assert!(there.exists(), "where the log points is not");

        fs::write(&here, "half a cache").unwrap();
        save_cache(&state_path.join("stats.json"), b"{}").unwrap();
        assert!(!there.exists(), "the next cache saved there cleans it");
        assert!(here.exists(), "and that one does not clean state/");
        assert_eq!(entries(&elsewhere), ["launches.jsonl", "stats.json"]);
    }

    /// R3: every path to one registry takes the same lock. Two `$REMUDA_HOME`s share a
    /// registry, one through a `config.toml` that is a symlink to the other's: the lock is on
    /// the directory of the file that is replaced, not on the one the path went in by, so
    /// changes made through both at the same time are all kept.
    #[test]
    fn two_paths_to_one_registry_take_one_lock() {
        const WRITERS: usize = 16;
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (dir.path().join("first"), dir.path().join("second"));
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let real = first.join("config.toml");
        let linked = second.join("config.toml");
        fs::write(&real, "").unwrap();
        symlink(&real, &linked).unwrap();

        // While a change through the link is being made, the directory of the real file is
        // locked, and nothing is created in either.
        let try_lock = |path: &Path| {
            let file = fs::File::open(path).unwrap();
            // SAFETY: the descriptor is open for the call.
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }
        };
        update_registry(&linked, &Flock, || {
            assert_eq!(try_lock(&first), -1, "the lock every path takes");
            assert_eq!(entries(&first), ["config.toml"]);
            assert_eq!(entries(&second), ["config.toml"]);
            Ok(None)
        })
        .unwrap();
        assert_eq!(try_lock(&first), 0);

        let barrier = Arc::new(Barrier::new(WRITERS));
        let threads: Vec<_> = (0..WRITERS)
            .map(|n| {
                let config = if n % 2 == 0 {
                    real.clone()
                } else {
                    linked.clone()
                };
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    update_registry(&config, &Flock, || {
                        let text = fs::read_to_string(&config).unwrap_or_default();
                        // Long enough for every other writer to read the same text.
                        std::thread::sleep(Duration::from_millis(5));
                        Ok(Some(format!("{text}line {n}\n")))
                    })
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        let text = fs::read_to_string(&real).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        lines.sort();
        let mut want: Vec<String> = (0..WRITERS).map(|n| format!("line {n}")).collect();
        want.sort();
        assert_eq!(lines, want);
        assert_eq!(fs::read_link(&linked).unwrap(), real);
        assert_eq!(entries(&first), ["config.toml"]);
        assert_eq!(entries(&second), ["config.toml"]);

        // A link pointed elsewhere while its lock was awaited: the lock held is not that of
        // the file the edit would read, so nothing is read or written.
        struct Repoint<'a>(&'a Path, &'a Path);
        impl Locks for Repoint<'_> {
            fn exclusive(&self, file: &fs::File) -> io::Result<bool> {
                fs::remove_file(self.0)?;
                symlink(self.1, self.0)?;
                Flock.exclusive(file)
            }
        }
        let third = dir.path().join("third");
        fs::create_dir(&third).unwrap();
        fs::write(third.join("other.toml"), "# another registry\n").unwrap();
        let asked = AtomicUsize::new(0);
        let e = update_registry(
            &linked,
            &Repoint(&linked, &third.join("other.toml")),
            || {
                asked.fetch_add(1, Ordering::Relaxed);
                Ok(Some("# changed\n".to_string()))
            },
        )
        .unwrap_err();
        assert_eq!(
            e.to_string(),
            format!(
                "{} was pointed elsewhere while it was being changed; nothing was written",
                linked.display()
            )
        );
        assert_eq!(asked.load(Ordering::Relaxed), 0);
        assert_eq!(fs::read_to_string(&real).unwrap(), text);
        assert_eq!(
            fs::read_to_string(third.join("other.toml")).unwrap(),
            "# another registry\n"
        );
        fs::remove_file(&linked).unwrap();
        symlink(&real, &linked).unwrap();

        // Without locks where the file is, a change through the link is refused, whatever
        // the directory the link is in.
        let e = update_registry(&linked, &NoLocks, || Ok(Some(String::new()))).unwrap_err();
        assert!(
            format!("{e:#}").starts_with(&format!(
                "cannot lock {} ",
                fs::canonicalize(&first).unwrap().display()
            )),
            "{e:#}"
        );
        assert_eq!(fs::read_to_string(&real).unwrap(), text);
    }

    /// R3: a directory the user may search and write but not read (mode 0300) is written in
    /// by name all the same, as when remuda wrote by path: the launch log is appended to and
    /// a cache saved, through a `state` that is such a directory or a symlink to one, and its
    /// mode stays. Only what lists a directory does not happen there: the leftovers of a
    /// killed write stay until it can be read, and it cannot be locked.
    #[test]
    fn a_directory_that_cannot_be_read_is_still_written_in_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("remuda");
        let state_path = home.join("state");
        fs::create_dir_all(&state_path).unwrap();
        let log = launch_log(&state_path);
        fs::write(&log, "old\n").unwrap();
        chmod(&log, 0o600);
        let leftover = state_path.join(temp_of(dead_pid()));
        fs::write(&leftover, "half a cache").unwrap();
        let unreadable = Unreadable::new(&state_path);
        assert!(fs::read_dir(&state_path).is_err(), "run as a regular user");

        let linked = dir.path().join("linked-state");
        symlink(&state_path, &linked).unwrap();
        for through in [&state_path, &linked] {
            append_log(&launch_log(through), "new\n").unwrap();
            save_cache(&through.join("index.json"), b"{}").unwrap();
            assert_eq!(mode(&state_path), 0o300);
        }
        // What is in it is private as anywhere, and a directory made in it is too.
        let settings_dir = settings(&state_path.join("settings")).unwrap();
        settings_dir
            .write("s.json", b"{}", FileMode::Private)
            .unwrap();
        assert!(settings_dir.names().is_ok());
        let opened = state(&state_path).unwrap();
        assert_eq!(opened.kind("index.json").unwrap(), Some(Kind::File));
        assert!(opened.names().is_err(), "it cannot be listed");
        assert!(
            opened.lock(&Flock, Lockless::Proceed).is_err(),
            "nor locked"
        );
        assert_eq!(mode(&state_path), 0o300);

        drop(unreadable);
        assert_eq!(mode(&state_path), 0o700);
        assert_eq!(fs::read_to_string(&log).unwrap(), "old\nnew\nnew\n");
        assert_eq!(mode(&log), 0o600);
        assert_eq!(mode(&state_path.join("index.json")), 0o600);
        assert_eq!(mode(&state_path.join("settings")), 0o700);
        assert!(
            leftover.exists(),
            "not removed while the directory could not be listed"
        );
        // Readable again: the next write there removes it.
        append_log(&log, "last\n").unwrap();
        assert!(!leftover.exists());
        assert_eq!(
            entries(&state_path),
            ["index.json", "launches.jsonl", "settings"]
        );

        // The directories remuda only makes links and copies in are opened the same way.
        let shared_path = home.join("shared/claude");
        shared(&shared_path).unwrap();
        let unreadable = (
            Unreadable::new(&shared_path),
            Unreadable::new(&home.join("shared")),
        );
        let opened = shared(&shared_path).unwrap();
        opened
            .replace_link("skills", Path::new("/source/skills"))
            .unwrap();
        opened.sub_or_create("rules").unwrap();
        drop(unreadable);
        assert_eq!(entries(&shared_path), ["rules", "skills"]);

        // The registry needs its lock, and the lock a directory that can be read: refused,
        // and nothing is changed.
        let config = home.join("config.toml");
        fs::write(&config, "# as it was\n").unwrap();
        let unreadable = Unreadable::new(&home);
        let e = update_registry(&config, &Flock, || Ok(Some("# changed\n".to_string())));
        drop(unreadable);
        let e = format!("{:#}", e.unwrap_err());
        assert!(
            e.starts_with(&format!("cannot lock {} ", home.display())),
            "{e}"
        );
        assert_eq!(fs::read_to_string(&config).unwrap(), "# as it was\n");
    }

    /// R3: appending to the launch log is a write to `state/` like any other: it removes
    /// what a killed write left there, so a launch cleans up after a cache whose save was
    /// cut short, and keeps what a running process is writing.
    #[test]
    fn a_line_appended_to_the_log_cleans_up_too() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("remuda/state");
        fs::create_dir_all(&state_path).unwrap();
        let (gone, running) = (temp_of(dead_pid()), temp_of(std::process::id()));
        fs::write(state_path.join(&gone), "half a cache").unwrap();
        fs::write(state_path.join(&running), "half a cache").unwrap();
        append_log(&launch_log(&state_path), "launch\n").unwrap();
        let mut want = vec![running, "launches.jsonl".to_string()];
        want.sort();
        assert_eq!(entries(&state_path), want);
    }

    /// R3: a log that stayed open to the group or others after the attempt to tighten it (as
    /// another user's file does: the mode cannot be changed) is refused, by the mode of the
    /// open file; one that is the user's alone, or that is not a regular file, is not.
    #[test]
    fn a_log_that_is_not_private_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("launches.jsonl");
        fs::write(&log, "").unwrap();
        let open = || fs::OpenOptions::new().append(true).open(&log).unwrap();
        for mode in [0o666, 0o644, 0o640, 0o602, 0o610] {
            fs::set_permissions(&log, fs::Permissions::from_mode(mode)).unwrap();
            let e = refuse_shared(&open(), &log).unwrap_err().to_string();
            assert_eq!(
                e,
                format!(
                    "{} can be accessed by the group or others (mode {mode:04o}) and was not \
                     made private; nothing was appended",
                    log.display()
                )
            );
        }
        fs::set_permissions(&log, fs::Permissions::from_mode(0o600)).unwrap();
        refuse_shared(&open(), &log).unwrap();
        let null = fs::OpenOptions::new().append(true).open("/dev/null");
        let e = refuse_shared(&null.unwrap(), Path::new("/dev/null")).unwrap_err();
        assert_eq!(
            e.to_string(),
            "/dev/null is not a regular file; nothing was appended"
        );
    }

    /// R3: the launch log is created private in a private `state/`, a log and a `state/`
    /// from before are tightened, lines are appended whole, and a log that is a symlink is
    /// written through without its target's mode being changed, or not written at all when
    /// others can read that target.
    #[test]
    fn the_launch_log_is_appended_to_as_the_users_alone() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("remuda/state");
        let log = launch_log(&state_path);
        assert_eq!(log, state_path.join("launches.jsonl"));
        append_log(&log, "one\n").unwrap();
        assert_eq!((mode(&state_path), mode(&log)), (0o700, 0o600));
        chmod(&state_path, 0o755);
        chmod(&log, 0o644);
        append_log(&log, "two\n").unwrap();
        assert_eq!((mode(&state_path), mode(&log)), (0o700, 0o600));
        assert_eq!(fs::read_to_string(&log).unwrap(), "one\ntwo\n");
        assert_eq!(entries(&state_path), ["launches.jsonl"]);

        let target = dir.path().join("elsewhere.jsonl");
        fs::rename(&log, &target).unwrap();
        symlink(&target, &log).unwrap();
        append_log(&log, "three\n").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "one\ntwo\nthree\n");
        chmod(&target, 0o640);
        let e = append_log(&log, "a secret prompt\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("(mode 0640) and was not made private"), "{e}");
        assert_eq!(mode(&target), 0o640);
        assert_eq!(fs::read_to_string(&target).unwrap(), "one\ntwo\nthree\n");

        // `state` is a file: there is nowhere to write.
        let home = dir.path().join("other");
        fs::create_dir(&home).unwrap();
        fs::write(home.join("state"), "").unwrap();
        let e = append_log(&launch_log(&home.join("state")), "x\n").unwrap_err();
        assert_eq!(
            e.to_string(),
            format!(
                "{} is not a directory; remuda does not replace it",
                home.join("state").display()
            )
        );
    }

    /// R3: where things are below `$REMUDA_HOME`, from the path of `config.toml` in it.
    #[test]
    fn the_layout_is_derived_from_the_registry() {
        let config = Path::new("/r/config.toml");
        assert_eq!(state_dir(config), Path::new("/r/state"));
        assert_eq!(
            launch_log(&state_dir(config)),
            Path::new("/r/state/launches.jsonl")
        );
        assert_eq!(settings_dir(config), Path::new("/r/state/settings"));
        assert_eq!(shared_dir(config), Path::new("/r/shared/claude"));
    }
}
