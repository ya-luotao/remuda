//! `remuda setup [--provider <p>] <name>`: a fresh home under
//! `$REMUDA_HOME/homes/<provider>/<name>` (SPEC R3, R5, R13, R17), holding, for a claude member
//! of `[share.claude]`, the links that share the source's session store and configuration
//! (R18): the one write remuda makes inside a home (R12, R13). The home is made below
//! `$REMUDA_HOME` one directory descriptor at a time, never through a symlink, and its links
//! are made through its own descriptor. Logging in is left to the agent (`claude auth login`,
//! `codex login`). The command line and the TUI take the same steps: [`plan`] (with
//! [`Provider::login_args`]), [`create_and_register`], then the login in the foreground.

use std::ffi::{CStr, CString};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::provider::Provider;
use crate::registry::{self, Account, Home, Registry};
use crate::{Env, paths, share};

/// The items of the source's home that a new member's home links to (R18), in this order: the
/// session store and its file backups, then the configuration. An item the source does not
/// have gets no link.
pub const LINKS: [&str; 13] = [
    "projects",
    "file-history",
    "settings.json",
    "CLAUDE.md",
    "skills",
    "commands",
    "agents",
    "hooks",
    "plugins",
    "rules",
    "agent-memory",
    "output-styles",
    "keybindings.json",
];

/// Never linked, whatever the source has (R18): the login's identity and caches, attribution
/// (R9), running sessions (R7), and what an organization sets for its accounts. Nothing else
/// outside [`LINKS`] is linked either; these are the ones that break an account when shared.
pub const NEVER_LINKED: [&str; 5] = [
    ".claude.json",
    "history.jsonl",
    "sessions",
    "remote-settings.json",
    "policy-limits.json",
];

/// The source's settings file: linked only when it holds no authentication (R18).
const SETTINGS: &str = "settings.json";

/// A checked setup: the account to create and what its home is linked to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// `$REMUDA_HOME`: the home is made at `homes/<provider>/<name>` below it.
    pub root: PathBuf,
    pub account: Account,
    /// The source of `[share.claude]`, when the new account will be a member (R18); `None`:
    /// the home stays empty.
    pub share: Option<Share>,
}

/// Where the links of a new member's home point (R18).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    /// `provider:name` of the source.
    pub source: String,
    /// The source's home as registered (`$HOME/.claude` for `default`), not canonicalized;
    /// `None`: it is not a directory, and nothing is linked.
    pub from: Option<PathBuf>,
}

/// What [`share_links`] did, each list in [`LINKS`] order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Linked {
    pub linked: Vec<&'static str>,
    /// Items the source does not have (missing, or a dangling link).
    pub absent: Vec<&'static str>,
    /// Items whose link could not be made, with the reason.
    pub failed: Vec<(&'static str, String)>,
    /// Why `settings.json` got no link although the source has one.
    pub settings: Option<Unlinked>,
}

/// Why the source's `settings.json` is not linked (R18): a member that links it reads all of
/// it, and authentication stays per account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unlinked {
    /// It sets these authentication settings (`key` / `env.NAME`, [`share::withheld`]).
    Authentication(Vec<String>),
    /// It cannot be read as a JSON object, so it cannot be checked.
    Unreadable,
}

/// One line for the user about the links of a new home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub warning: bool,
    pub text: String,
}

/// Validates everything `setup` needs before any side effect and returns the new account,
/// with the source its home is to be linked to (R18).
pub fn plan(config: &Path, provider: Provider, name: &str, env: &Env) -> Result<Plan> {
    registry::check_new_name(name)?;
    let root = paths::remuda_home(env)?;
    let dir = root.join("homes").join(provider.name()).join(name);
    let home = dir
        .to_str()
        .ok_or_else(|| anyhow!("home path is not valid UTF-8: {}", dir.display()))?
        .to_string();
    paths::check_home_string(&home)?;
    let account = Account {
        provider,
        name: name.to_string(),
        home: Home::Path(home.clone()),
    };
    registry::check_registrable(config, &account)?;
    check_levels(&root, provider)?;
    if fs::symlink_metadata(&dir).is_ok() {
        bail!("{home} already exists; to register an existing directory use `remuda add`");
    }
    // A new account has no `share = false` and is not the source: a claude one is a member
    // whenever `[share.claude]` is set.
    let share = Registry::load(config)?
        .sharing
        .source_for(&account)
        .map(|source| Share {
            source: source.qualified(),
            from: source
                .home_dir(env)
                .filter(|from| from.is_absolute() && from.is_dir()),
        });
    Ok(Plan {
        root,
        account,
        share,
    })
}

/// The directories between `$REMUDA_HOME` and a new home: `homes`, then the provider's.
fn levels(provider: Provider) -> [&'static str; 2] {
    ["homes", provider.name()]
}

/// Why a level of [`levels`] that exists cannot hold a home: only a real directory does.
fn not_a_directory(path: &Path) -> String {
    let what = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => "a symbolic link",
        _ => "not a directory",
    };
    format!(
        "{} is {what}; remuda creates a home only below real directories of its own",
        path.display()
    )
}

/// Refuses, before any side effect, a `homes` or `homes/<provider>` below `root` that exists
/// and is not a real directory (R13): through a symlink there, the new home and its links
/// would land outside `$REMUDA_HOME`. [`create_home`] refuses the same as it opens each level.
fn check_levels(root: &Path, provider: Provider) -> Result<()> {
    let mut dir = root.to_path_buf();
    for level in levels(provider) {
        dir.push(level);
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.file_type().is_dir() => {}
            Ok(_) => bail!(not_a_directory(&dir)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => break,
            Err(e) => return Err(e).with_context(|| format!("cannot inspect {}", dir.display())),
        }
    }
    Ok(())
}

/// Creates the planned home, links it to the source's (R18) and registers it; the links that
/// were made, when there is a source home to link to. A home that was created but could not
/// be linked or registered is reported as such (it is left in place: remuda never deletes
/// homes, R2). A single link that fails does not stop the setup ([`Linked::failed`]).
pub fn create_and_register(config: &Path, plan: &Plan) -> Result<Option<Linked>> {
    let home = plan.account.home.to_string();
    let dir = create_home(&plan.root, plan.account.provider, &plan.account.name)?;
    let linked = match plan.share.as_ref().and_then(|share| share.from.as_deref()) {
        Some(from) => Some(
            share_links(&dir, from)
                .with_context(|| format!("created {home} but did not link or register it"))?,
        ),
        None => None,
    };
    registry::register(config, &plan.account)
        .with_context(|| format!("created {home} but could not register it"))?;
    Ok(linked)
}

/// Links each item of [`LINKS`] that `source` has into `home`, the directory [`create_home`]
/// has just made (R12, R13, R18): `<home>/<item>` pointing at `<source>/<item>`, written from
/// `source` as given, not canonicalized. The links are made through the home's descriptor,
/// not its path. Checked again first, through the same descriptor: the directory must be
/// empty, or nothing is linked and that is the error. `settings.json` is left out when the
/// source's sets authentication or cannot be read ([`Unlinked`]). A link that cannot be made
/// is listed and the others are still made. Nothing is replaced (`symlinkat` fails where a
/// name exists) and nothing is removed.
pub fn share_links(home: &NewHome, source: &Path) -> Result<Linked> {
    if !home
        .is_empty()
        .with_context(|| format!("cannot read {}", home.path.display()))?
    {
        bail!("{} is not empty", home.path.display());
    }
    let mut out = Linked::default();
    for item in LINKS {
        let target = source.join(item);
        if fs::metadata(&target).is_err() {
            out.absent.push(item);
            continue;
        }
        if item == SETTINGS {
            out.settings = match share::read_settings(&target) {
                Ok(settings) => Some(share::withheld(&settings))
                    .filter(|keys| !keys.is_empty())
                    .map(Unlinked::Authentication),
                Err(_) => Some(Unlinked::Unreadable),
            };
            if out.settings.is_some() {
                continue;
            }
        }
        match home.link(item, &target) {
            Ok(()) => out.linked.push(item),
            Err(e) => out.failed.push((item, e.to_string())),
        }
    }
    Ok(out)
}

/// What to tell the user about the links of the planned home (R18): what was linked, what the
/// source does not have, why `settings.json` was left out, what failed. `paths` adds the two
/// homes, for the command line; without them the lines name only the source account, the items
/// and settings keys (the TUI's notice, R21).
pub fn link_notes(plan: &Plan, linked: Option<&Linked>, paths: bool) -> Vec<Note> {
    let Some(share) = &plan.share else {
        return Vec::new();
    };
    let source = &share.source;
    let Some(from) = &share.from else {
        return vec![Note {
            warning: true,
            text: format!("the home of {source} does not exist: nothing linked"),
        }];
    };
    let Some(linked) = linked else {
        return Vec::new();
    };
    let mut notes = Vec::new();
    if !linked.linked.is_empty() {
        let n = linked.linked.len();
        let items = if n == 1 { "item" } else { "items" };
        let text = if paths {
            format!(
                "linked {n} {items} of {} to {source} ({}): {}",
                plan.account.home,
                from.display(),
                linked.linked.join(", ")
            )
        } else {
            format!(
                "linked {n} {items} to {source}: {}",
                linked.linked.join(", ")
            )
        };
        notes.push(Note {
            warning: false,
            text,
        });
    }
    if !linked.absent.is_empty() {
        notes.push(Note {
            warning: false,
            text: format!(
                "not linked ({source} has none): {}",
                linked.absent.join(", ")
            ),
        });
    }
    match &linked.settings {
        Some(Unlinked::Authentication(keys)) => notes.push(Note {
            warning: false,
            text: format!(
                "{SETTINGS} not linked: {source} sets authentication settings ({}); they stay \
                 per account, and the rest is injected at launch",
                keys.join(", ")
            ),
        }),
        Some(Unlinked::Unreadable) => notes.push(Note {
            warning: true,
            text: format!(
                "{SETTINGS} not linked: that of {source} cannot be read as a JSON object, so \
                 it cannot be checked for authentication settings"
            ),
        }),
        None => {}
    }
    // Settings injected at launch do not reach the agent's other runs (a login, a usage
    // query), and the cleanup period is read from the home's own file (R11): a home that
    // shares `projects` and has no `settings.json` of its own, because the source's was left
    // out or there was none, needs one.
    let no_settings = linked.settings.is_some() || linked.absent.contains(&SETTINGS);
    if no_settings && linked.linked.contains(&"projects") {
        notes.push(Note {
            warning: true,
            text: format!(
                "this account shares projects without a {SETTINGS} of its own: give it one \
                 that sets cleanupPeriodDays, or the default 30-day cleanup may delete the \
                 shared sessions"
            ),
        });
    }
    if !linked.failed.is_empty() {
        let failed: Vec<String> = linked
            .failed
            .iter()
            .map(|(item, why)| format!("{item} ({why})"))
            .collect();
        notes.push(Note {
            warning: true,
            text: format!("could not link to {source}: {}", failed.join(", ")),
        });
    }
    notes
}

/// The login command as the user would type it: `claude auth login`, `codex login`.
pub fn login_command(provider: Provider) -> String {
    let args = provider.login_args(None).unwrap_or_default();
    format!("{} {}", provider.program(), args.join(" "))
}

/// How to name a new account in messages and commands: claude's by its bare name (as before
/// codex) unless another provider has an account of that name (`ambiguous`: the bare name
/// would not resolve, R1), others as `provider:name`.
pub fn reference(provider: Provider, name: &str, ambiguous: bool) -> String {
    match provider {
        Provider::Claude if !ambiguous => name.to_string(),
        other => format!("{other}:{name}"),
    }
}

/// How to log in again after a failed login: `remuda run work auth login`,
/// `remuda run claude:work auth login` (`ambiguous`, see [`reference`]),
/// `remuda run codex:work login`.
pub fn retry_command(provider: Provider, name: &str, ambiguous: bool) -> String {
    let args = provider.login_args(None).unwrap_or_default();
    format!(
        "remuda run {} {}",
        reference(provider, name, ambiguous),
        args.join(" ")
    )
}

/// Whether a bare `name` is ambiguous for the `provider` account of that name: another
/// provider has an account of the same name among `accounts` (R1).
pub fn ambiguous<'a>(
    provider: Provider,
    name: &str,
    accounts: impl IntoIterator<Item = &'a Account>,
) -> bool {
    accounts
        .into_iter()
        .any(|a| a.name == name && a.provider != provider)
}

/// A home [`create_home`] has just made, held open: what remuda puts into it goes through this
/// descriptor, never through its path again (R13).
#[derive(Debug)]
pub struct NewHome {
    dir: fs::File,
    /// For messages only.
    path: PathBuf,
}

impl NewHome {
    /// Whether the directory has no entry, read through the descriptor.
    fn is_empty(&self) -> io::Result<bool> {
        /// A directory stream, closed when dropped.
        struct Stream(*mut libc::DIR);
        impl Drop for Stream {
            fn drop(&mut self) {
                // SAFETY: the stream came from a successful `fdopendir` and is closed once.
                unsafe { libc::closedir(self.0) };
            }
        }
        let copy = self.dir.try_clone()?;
        // SAFETY: `copy` is an open descriptor; on success the stream owns it.
        let stream = unsafe { libc::fdopendir(copy.as_raw_fd()) };
        if stream.is_null() {
            return Err(io::Error::last_os_error());
        }
        // Closed by `closedir` from here on.
        let _ = copy.into_raw_fd();
        let stream = Stream(stream);
        // The copy shares its position with the descriptor: read from the start.
        // SAFETY: `stream.0` is a valid directory stream until `stream` is dropped.
        unsafe { libc::rewinddir(stream.0) };
        loop {
            clear_errno();
            // SAFETY: as above; the entry is read before the next call on the stream.
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                // The end of the directory leaves errno alone; an error sets it.
                let error = io::Error::last_os_error();
                return match error.raw_os_error() {
                    Some(0) | None => Ok(true),
                    Some(_) => Err(error),
                };
            }
            // SAFETY: `d_name` of an entry `readdir` returned is NUL-terminated.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name != b"." && name != b".." {
                return Ok(false);
            }
        }
    }

    /// A symlink `name` in the directory pointing at `target`; fails where `name` exists.
    fn link(&self, name: &str, target: &Path) -> io::Result<()> {
        let (name, target) = (
            c_string(name.as_bytes())?,
            c_string(target.as_os_str().as_bytes())?,
        );
        // SAFETY: two NUL-terminated strings and an open directory descriptor.
        match unsafe { libc::symlinkat(target.as_ptr(), self.dir.as_raw_fd(), name.as_ptr()) } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }
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

fn c_string(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in a path"))
}

/// Opens a directory: `name` in the directory `parent`, never through a symlink, or the path
/// `name` itself (symlinks followed) without a parent.
fn open_dir(parent: Option<&OwnedFd>, name: &[u8]) -> io::Result<OwnedFd> {
    let name = c_string(name)?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    // SAFETY: a NUL-terminated string and, with a parent, an open directory descriptor.
    let fd = unsafe {
        match parent {
            Some(parent) => {
                libc::openat(parent.as_raw_fd(), name.as_ptr(), flags | libc::O_NOFOLLOW)
            }
            None => libc::open(name.as_ptr(), flags),
        }
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just opened and is owned by nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// A directory `name` in `parent` with `mode` (before the umask); fails where `name` exists.
fn make_dir(parent: &OwnedFd, name: &str, mode: libc::mode_t) -> io::Result<()> {
    let name = c_string(name.as_bytes())?;
    // SAFETY: a NUL-terminated string and an open directory descriptor.
    match unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), mode) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Creates the (empty) home `homes/<provider>/<name>` below `root` (`$REMUDA_HOME`, taken as
/// given and created as needed) with mode 0700, and returns it open (R13). Each level below
/// `root` is opened relative to the one above, never through a symlink, and created where it
/// is missing: a `homes` or `homes/<provider>` that is a symlink or not a directory is an
/// error, and nothing is made beyond it. The home itself must not exist.
pub fn create_home(root: &Path, provider: Provider, name: &str) -> Result<NewHome> {
    fs::create_dir_all(root).with_context(|| format!("cannot create {}", root.display()))?;
    let mut dir = open_dir(None, root.as_os_str().as_bytes())
        .with_context(|| format!("cannot open {}", root.display()))?;
    let mut path = root.to_path_buf();
    for level in levels(provider) {
        path.push(level);
        dir = match open_dir(Some(&dir), level.as_bytes()) {
            Ok(next) => next,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Another remuda may have made it meanwhile: opened all the same.
                if let Err(e) = make_dir(&dir, level, 0o777)
                    && e.kind() != io::ErrorKind::AlreadyExists
                {
                    return Err(e).with_context(|| format!("cannot create {}", path.display()));
                }
                open_dir(Some(&dir), level.as_bytes()).map_err(|e| level_error(e, &path))?
            }
            Err(e) => return Err(level_error(e, &path)),
        };
    }
    path.push(name);
    make_dir(&dir, name, 0o700).with_context(|| format!("cannot create {}", path.display()))?;
    let home = open_dir(Some(&dir), name.as_bytes())
        .with_context(|| format!("cannot open {}", path.display()))?;
    let home = fs::File::from(home);
    // The umask may have removed bits; set the mode explicitly.
    home.set_permissions(fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot set permissions on {}", path.display()))?;
    Ok(NewHome { dir: home, path })
}

/// Why the level at `path` could not be opened. The reason is read from what is there, not
/// from the errno, which differs between systems for a symlink.
fn level_error(error: io::Error, path: &Path) -> anyhow::Error {
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_dir() => anyhow!(not_a_directory(path)),
        _ => anyhow::Error::new(error).context(format!("cannot open {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::registry::CLAUDE;

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// `<root>/source` with `projects`, `settings.json` and a dangling `skills`, and the new,
    /// empty home `<root>/remuda/homes/claude/work`, held open, with its path.
    fn fixture() -> (tempfile::TempDir, PathBuf, NewHome, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        fs::create_dir_all(source.join("projects")).unwrap();
        fs::write(source.join("settings.json"), r#"{"model": "opus"}"#).unwrap();
        symlink(source.join("nowhere"), source.join("skills")).unwrap();
        let root = dir.path().join("remuda");
        let home = create_home(&root, CLAUDE, "work").unwrap();
        let path = root.join("homes/claude/work");
        (dir, source, home, path)
    }

    /// R18: nothing that is never linked is on the list.
    #[test]
    fn the_list_leaves_out_what_is_never_linked() {
        for item in NEVER_LINKED {
            assert!(!LINKS.contains(&item), "{item}");
        }
    }

    /// R13: the new home is an empty directory with mode 0700 below `$REMUDA_HOME`, which is
    /// created as needed; a home that exists, whatever it is, is refused and left alone.
    #[test]
    fn the_home_is_created_once() {
        let (dir, _source, _home, path) = fixture();
        let meta = fs::symlink_metadata(&path).unwrap();
        assert!(meta.is_dir());
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        assert_eq!(names(&path), [] as [&str; 0]);

        let root = dir.path().join("remuda");
        fs::write(path.join("keep"), "x").unwrap();
        let err = create_home(&root, CLAUDE, "work").unwrap_err();
        assert!(format!("{err:#}").contains("cannot create"), "{err:#}");
        assert_eq!(names(&path), ["keep"]);

        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        symlink(&elsewhere, root.join("homes/claude/link")).unwrap();
        fs::write(root.join("homes/claude/file"), "").unwrap();
        for name in ["link", "file"] {
            assert!(create_home(&root, CLAUDE, name).is_err(), "{name}");
        }
        assert_eq!(names(&elsewhere), [] as [&str; 0]);
        assert_eq!(names(&root.join("homes/claude")), ["file", "link", "work"]);
    }

    /// R13: `homes` and `homes/<provider>` are real directories or the setup stops: a symlink
    /// there would take the home, and its links, out of `$REMUDA_HOME`. Refused by the check
    /// that runs before any side effect and again when the home is created.
    #[test]
    fn a_home_is_never_created_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();

        // `homes/claude` is a link.
        let root = dir.path().join("a");
        fs::create_dir_all(root.join("homes")).unwrap();
        symlink(&outside, root.join("homes/claude")).unwrap();
        // `homes` is a link.
        let linked_homes = dir.path().join("b");
        fs::create_dir_all(&linked_homes).unwrap();
        symlink(&outside, linked_homes.join("homes")).unwrap();
        // A dangling link, and a file.
        let dangling = dir.path().join("c");
        fs::create_dir_all(&dangling).unwrap();
        symlink(dir.path().join("nowhere"), dangling.join("homes")).unwrap();
        let file = dir.path().join("d");
        fs::create_dir_all(file.join("homes")).unwrap();
        fs::write(file.join("homes/claude"), "").unwrap();

        for (root, level, what) in [
            (&root, "homes/claude", "is a symbolic link"),
            (&linked_homes, "homes", "is a symbolic link"),
            (&dangling, "homes", "is a symbolic link"),
            (&file, "homes/claude", "is not a directory"),
        ] {
            let expected = format!("{} {what}", root.join(level).display());
            let err = check_levels(root, CLAUDE).unwrap_err();
            assert!(err.to_string().starts_with(&expected), "{err}");
            let err = create_home(root, CLAUDE, "work").unwrap_err();
            assert!(format!("{err:#}").starts_with(&expected), "{err:#}");
        }
        assert_eq!(names(&outside), [] as [&str; 0]);
        // The other provider's level is its own.
        fs::create_dir(root.join("homes/codex")).unwrap();
        check_levels(&root, crate::registry::CODEX).unwrap();
        // `$REMUDA_HOME` itself is taken as given, a symlink included.
        let through = dir.path().join("root-link");
        symlink(dir.path().join("e"), &through).unwrap();
        fs::create_dir(dir.path().join("e")).unwrap();
        check_levels(&through, CLAUDE).unwrap();
        create_home(&through, CLAUDE, "work").unwrap();
        assert!(dir.path().join("e/homes/claude/work").is_dir());
    }

    /// R18: one link per item the source has, pointing at `<source>/<item>` as given.
    #[test]
    fn links_point_at_the_sources_items() {
        let (_dir, source, home, path) = fixture();
        let linked = share_links(&home, &source).unwrap();
        assert_eq!(linked.linked, ["projects", "settings.json"]);
        assert_eq!(linked.absent.len(), LINKS.len() - 2);
        assert!(linked.absent.contains(&"skills"), "a dangling item");
        assert_eq!(linked.failed, []);
        assert_eq!(linked.settings, None);
        assert_eq!(names(&path), ["projects", "settings.json"]);
        for item in ["projects", "settings.json"] {
            assert_eq!(fs::read_link(path.join(item)).unwrap(), source.join(item));
        }
    }

    /// R13, R18: the links go into the directory `setup` made, through its descriptor: moved
    /// away and replaced by a symlink to another directory, it still gets them, and the other
    /// directory gets nothing.
    #[test]
    fn links_follow_the_directory_not_its_path() {
        let (dir, source, home, path) = fixture();
        let moved = dir.path().join("moved");
        fs::rename(&path, &moved).unwrap();
        let other = dir.path().join("other");
        fs::create_dir(&other).unwrap();
        symlink(&other, &path).unwrap();
        let linked = share_links(&home, &source).unwrap();
        assert_eq!(linked.linked, ["projects", "settings.json"]);
        assert_eq!(names(&moved), ["projects", "settings.json"]);
        assert_eq!(names(&other), [] as [&str; 0]);
    }

    /// R12, R13, R18: only an empty directory is linked, checked through the descriptor each
    /// time; otherwise it is an error, and nothing is made or changed.
    #[test]
    fn only_an_empty_directory_is_linked() {
        let (_dir, source, home, path) = fixture();
        assert!(home.is_empty().unwrap());
        assert!(home.is_empty().unwrap(), "read from the start again");
        fs::write(path.join("keep"), "x").unwrap();
        for _ in 0..2 {
            let err = share_links(&home, &source).unwrap_err();
            assert!(err.to_string().ends_with("is not empty"), "{err}");
        }
        assert_eq!(names(&path), ["keep"]);
        assert_eq!(fs::read_to_string(path.join("keep")).unwrap(), "x");
        // A name that begins with a dot counts too.
        fs::remove_file(path.join("keep")).unwrap();
        fs::write(path.join(".claude.json"), "{}").unwrap();
        assert!(share_links(&home, &source).is_err());
        assert_eq!(names(&path), [".claude.json"]);
    }

    /// R18: a link that cannot be made is listed with the reason; it is not an error.
    #[test]
    fn links_that_cannot_be_made_are_listed() {
        let (_dir, source, home, path) = fixture();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o500)).unwrap();
        let linked = share_links(&home, &source).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(linked.linked, [] as [&str; 0]);
        let failed: Vec<&str> = linked.failed.iter().map(|(item, _)| *item).collect();
        assert_eq!(failed, ["projects", "settings.json"]);
        assert_eq!(names(&path), [] as [&str; 0]);
    }

    /// R18: a `settings.json` that sets authentication, or that cannot be read as a JSON
    /// object, is not linked: the account would read it whole. The rest is linked as usual.
    #[test]
    fn settings_with_authentication_are_not_linked() {
        for (settings, unlinked) in [
            (
                r#"{"model": "opus", "apiKeyHelper": "/k",
                    "env": {"ANTHROPIC_API_KEY": "sk-secret", "EDITOR": "vi"}}"#,
                Unlinked::Authentication(vec![
                    "apiKeyHelper".into(),
                    "env.ANTHROPIC_API_KEY".into(),
                ]),
            ),
            (r#"{"forceLoginOrgUUID": "o"}"#, {
                Unlinked::Authentication(vec!["forceLoginOrgUUID".into()])
            }),
            ("[]", Unlinked::Unreadable),
            ("{", Unlinked::Unreadable),
        ] {
            let (_dir, source, home, path) = fixture();
            fs::write(source.join("settings.json"), settings).unwrap();
            let linked = share_links(&home, &source).unwrap();
            assert_eq!(linked.linked, ["projects"], "{settings}");
            assert_eq!(linked.settings, Some(unlinked), "{settings}");
            assert!(!linked.absent.contains(&"settings.json"));
            assert_eq!(linked.failed, []);
            assert_eq!(names(&path), ["projects"]);
        }
        // An `env` without authentication is linked.
        let (_dir, source, home, path) = fixture();
        fs::write(
            source.join("settings.json"),
            r#"{"env": {"EDITOR": "vi", "MAX_THINKING_TOKENS": "1"}}"#,
        )
        .unwrap();
        let linked = share_links(&home, &source).unwrap();
        assert_eq!(linked.settings, None);
        assert_eq!(names(&path), ["projects", "settings.json"]);
    }

    fn planned(from: Option<&Path>) -> Plan {
        Plan {
            root: PathBuf::from("/r"),
            account: Account {
                provider: CLAUDE,
                name: "work".into(),
                home: Home::Path("/r/homes/claude/work".into()),
            },
            share: Some(Share {
                source: "claude:default".into(),
                from: from.map(Path::to_path_buf),
            }),
        }
    }

    fn texts(notes: &[Note]) -> Vec<(bool, &str)> {
        notes.iter().map(|n| (n.warning, n.text.as_str())).collect()
    }

    /// R18, R21: what is said about the links, with the homes for the command line and
    /// without any path for the TUI's notice.
    #[test]
    fn notes_about_the_links() {
        let plan = planned(Some(Path::new("/h/.claude")));
        let linked = Linked {
            linked: vec!["projects", "CLAUDE.md"],
            absent: vec!["rules"],
            failed: vec![("skills", "File exists (os error 17)".into())],
            settings: Some(Unlinked::Authentication(vec![
                "apiKeyHelper".into(),
                "env.ANTHROPIC_API_KEY".into(),
            ])),
        };
        let cleanup = "this account shares projects without a settings.json of its own: give \
                       it one that sets cleanupPeriodDays, or the default 30-day cleanup may \
                       delete the shared sessions";
        let rest = [
            (false, "not linked (claude:default has none): rules"),
            (
                false,
                "settings.json not linked: claude:default sets authentication settings \
                 (apiKeyHelper, env.ANTHROPIC_API_KEY); they stay per account, and the rest is \
                 injected at launch",
            ),
            (true, cleanup),
            (
                true,
                "could not link to claude:default: skills (File exists (os error 17))",
            ),
        ];
        let notes = link_notes(&plan, Some(&linked), true);
        assert_eq!(
            texts(&notes)[0],
            (
                false,
                "linked 2 items of /r/homes/claude/work to claude:default (/h/.claude): \
                 projects, CLAUDE.md"
            )
        );
        assert_eq!(texts(&notes)[1..], rest);
        let notes = link_notes(&plan, Some(&linked), false);
        assert_eq!(
            texts(&notes)[0],
            (
                false,
                "linked 2 items to claude:default: projects, CLAUDE.md"
            )
        );
        assert_eq!(texts(&notes)[1..], rest);
        assert!(notes.iter().all(|n| !n.text.contains('/')), "{notes:?}");

        let one = Linked {
            linked: vec!["projects"],
            settings: Some(Unlinked::Unreadable),
            ..Linked::default()
        };
        assert_eq!(
            texts(&link_notes(&plan, Some(&one), false)),
            [
                (false, "linked 1 item to claude:default: projects"),
                (
                    true,
                    "settings.json not linked: that of claude:default cannot be read as a JSON \
                     object, so it cannot be checked for authentication settings"
                ),
                (true, cleanup),
            ]
        );
        // Without a shared `projects` there is no shared store to clean up.
        let apart = Linked {
            linked: vec!["CLAUDE.md"],
            settings: Some(Unlinked::Unreadable),
            ..Linked::default()
        };
        assert_eq!(link_notes(&plan, Some(&apart), false).len(), 2);
        // A source without `settings.json` leaves the home without one too.
        let none = Linked {
            linked: vec!["projects"],
            absent: vec!["settings.json"],
            ..Linked::default()
        };
        assert_eq!(
            texts(&link_notes(&plan, Some(&none), false)),
            [
                (false, "linked 1 item to claude:default: projects"),
                (false, "not linked (claude:default has none): settings.json"),
                (true, cleanup),
            ]
        );
        // Nothing to say without a source, and one warning when its home is missing.
        assert_eq!(
            texts(&link_notes(&planned(None), None, true)),
            [(
                true,
                "the home of claude:default does not exist: nothing linked"
            )]
        );
        let alone = Plan {
            share: None,
            ..plan
        };
        assert_eq!(link_notes(&alone, None, true), []);
    }
}
