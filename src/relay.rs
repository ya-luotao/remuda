//! Relay: continuing a session under an account whose `projects` store does not hold it
//! (SPEC R19). The transcript and its checkpoints are copied into the target account's
//! store, and the copy is forked there; the original session is never modified.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::Env;
use crate::launch::{self, Launch};
use crate::registry::{Account, CLAUDE, Sharing};
use crate::transcript::{complete_lines, read_at};

/// The session a relay continues: the selected transcript and what the index knows of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// The transcript itself (R16: the row, not a lookup by id).
    pub transcript: PathBuf,
    /// Realpath of the `projects` store holding it.
    pub store: PathBuf,
    pub session_id: String,
    /// Where the session was last; the fork runs there.
    pub cwd_last: Option<String>,
}

/// What a relay copied, as the launch log records it (R19).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relay {
    /// The transcript copied.
    pub source: String,
    /// The copy, under the realpath of the target's store (as the index lists it).
    pub transcript: String,
    /// The checkpoint files copied (those already there are kept, and not listed).
    pub checkpoints: Vec<String>,
    /// The copy's size and mtime (nanoseconds) right after it was written: a later relay
    /// replaces the copy only while both still match.
    pub size: u64,
    pub mtime_ns: i128,
}

/// Where a relay copies to, decided by [`check`] before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    home: PathBuf,
    /// The directory holding the transcript in its store: the same name in the target's.
    dir: OsString,
    file: OsString,
    /// An earlier relay copy is there: its recorded size and mtime, checked again right before
    /// it is replaced.
    replaces: Option<(u64, i128)>,
}

/// The refusals of R19, before anything is written: the target is a claude account with a
/// home, the session id is a UUID (R16), `cwd_last` exists, the target's store does not already
/// hold the transcript (a fork does), and a transcript already at the destination is an earlier
/// relay copy that nothing has changed since (per `launch_log`).
pub fn check(source: &Source, target: &Account, env: &Env, launch_log: &Path) -> Result<Target> {
    let name = target.qualified();
    let id = &source.session_id;
    if target.provider != CLAUDE {
        bail!("{name} is not a claude account: only claude sessions are relayed");
    }
    if !launch::is_session_id(id) {
        bail!("session id {id:?} is not a UUID; remuda does not pass it to claude");
    }
    let file = source
        .transcript
        .file_name()
        .filter(|f| *f == format!("{id}.jsonl").as_str())
        .ok_or_else(|| anyhow!("{} is not session {id}", source.transcript.display()))?
        .to_os_string();
    let dir = source
        .transcript
        .parent()
        .and_then(Path::file_name)
        .ok_or_else(|| anyhow!("{} is not in a project", source.transcript.display()))?
        .to_os_string();
    let home = target
        .home_dir(env)
        .ok_or_else(|| anyhow!("the home of {name} is unknown (HOME is not set)"))?;
    if !home.is_dir() {
        bail!("home {} of {name} does not exist", home.display());
    }
    let store = fs::canonicalize(home.join("projects")).ok();
    if store.as_ref() == Some(&source.store) {
        bail!("the projects store of {name} already holds session {id}: fork it there instead");
    }
    let transcript = File::open(&source.transcript)
        .with_context(|| format!("cannot read {}", source.transcript.display()))?;
    if complete_len(&transcript)? == 0 {
        bail!(
            "{} has no complete record: nothing to continue",
            source.transcript.display()
        );
    }
    match source.cwd_last.as_deref() {
        None => bail!("session {id} has no recorded directory to continue in"),
        Some(cwd) if !Path::new(cwd).is_dir() => bail!("{cwd} does not exist"),
        Some(_) => {}
    }
    let mut replaces = None;
    if let Some(store) = store {
        let dest = store.join(&dir).join(&file);
        match fs::symlink_metadata(&dest) {
            Ok(meta) => match earlier_copy(launch_log, &dest) {
                Some(recorded) if meta.is_file() && stat(&meta) == recorded => {
                    replaces = Some(recorded);
                }
                _ => bail!(
                    "{} already exists and is not an unchanged earlier relay copy: remuda does \
                     not overwrite it",
                    dest.display()
                ),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("cannot inspect {}", dest.display())),
        }
    }
    Ok(Target {
        home,
        dir,
        file,
        replaces,
    })
}

/// A relay, ready to run (R19): checked, then planned like any launch (`--resume <id>
/// --fork-session` under `target` in `cwd_last`, with a new session id and the shared
/// configuration of R18), then copied; the plan's log record carries the copy and `fork_of`.
/// Nothing is written unless everything before the copy succeeded.
#[allow(clippy::too_many_arguments)]
pub fn prepare(
    source: &Source,
    target: &Account,
    accounts: &[Account],
    sharing: &Sharing,
    env: &Env,
    launch_log: &Path,
    config: &Path,
    ts: String,
) -> Result<Launch> {
    let dest = check(source, target, env, launch_log)?;
    let cwd = PathBuf::from(source.cwd_last.as_deref().unwrap_or_default());
    let mut plan = launch::plan(
        target,
        CLAUDE.resume_args(&source.session_id, &cwd, true),
        Some(&cwd),
        ts,
        || uuid::Uuid::new_v4().to_string(),
        sharing,
        env,
        config,
    )?;
    plan.record.relay = Some(copy(source, &dest, accounts, env)?);
    Ok(plan)
}

/// Copies the checkpoints of every home whose `projects` resolves to the transcript's store, as
/// a union, into the target's `file-history/<id>/`, then the transcript (up to its last
/// complete line) into the target's store, placed last (R19). Everything is written under a
/// temporary name and renamed into place; nothing but an unchanged earlier relay copy is
/// replaced, and checkpoints already there are kept. If the launch cannot go ahead after this,
/// [`discard`] removes the transcript copy.
pub fn copy(source: &Source, target: &Target, accounts: &[Account], env: &Env) -> Result<Relay> {
    let projects = target.home.join("projects");
    fs::create_dir_all(projects.join(&target.dir))
        .with_context(|| format!("cannot create {}", projects.join(&target.dir).display()))?;
    let store = fs::canonicalize(&projects)
        .with_context(|| format!("cannot resolve {}", projects.display()))?;
    if store == source.store {
        bail!("the target's projects store already holds the transcript");
    }
    let dir = store.join(&target.dir);
    let dest = dir.join(&target.file);

    // Checkpoints first: the transcript, placed last, is what makes a relay copy (R19).
    let checkpoints = copy_checkpoints(source, &target.home, accounts, env)?;
    let tmp = temp_name(&dir, &target.file);
    let written = (|| -> Result<()> {
        let mut from = File::open(&source.transcript)
            .with_context(|| format!("cannot read {}", source.transcript.display()))?;
        let len = complete_len(&from)?;
        if len == 0 {
            bail!("{} has no complete record", source.transcript.display());
        }
        let mut to = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        let copied = io::copy(&mut (&mut from).take(len), &mut to)?;
        if copied != len {
            bail!("{} shrank while it was copied", source.transcript.display());
        }
        to.sync_all()?;
        match target.replaces {
            Some(recorded) => {
                // Unchanged since `check`, or it is no longer ours to replace.
                match fs::symlink_metadata(&dest) {
                    Ok(meta) if meta.is_file() && stat(&meta) == recorded => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    _ => bail!("{} changed meanwhile: not overwritten", dest.display()),
                }
                fs::rename(&tmp, &dest)?;
            }
            None => place_new(&tmp, &dest)?,
        }
        Ok(())
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e.context(format!("cannot copy the transcript to {}", dest.display())));
    }
    let (size, mtime_ns) = stat(&fs::metadata(&dest)?);
    Ok(Relay {
        source: source.transcript.display().to_string(),
        transcript: dest.display().to_string(),
        checkpoints: checkpoints
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        size,
        mtime_ns,
    })
}

/// Removes the transcript copy of `relay` when the launch it was made for cannot go ahead
/// (R19): only while it is still the file remuda placed (same size and mtime). Checkpoints
/// stay: they are immutable, and one a later relay would copy again.
pub fn discard(relay: &Relay) -> Result<()> {
    let path = Path::new(&relay.transcript);
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() && stat(&meta) == (relay.size, relay.mtime_ns) => {
            fs::remove_file(path).with_context(|| format!("cannot remove {}", path.display()))
        }
        Ok(_) => bail!("{} changed meanwhile: not removed", path.display()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("cannot inspect {}", path.display())),
    }
}

/// What to say after [`discard`]: the relay copy is gone, or where it was left.
pub fn discarded(relay: &Relay) -> String {
    match discard(relay) {
        Ok(()) => "the relay copy was removed".to_string(),
        Err(e) => format!("the relay copy could not be removed: {e:#}"),
    }
}

/// The union of `file-history/<id>/` over every claude home whose `projects` resolves to the
/// transcript's store (checkpoints are content-addressed and immutable, so one name is one
/// file), copied where missing into the target's; returns the files copied. None at all is
/// not an error.
fn copy_checkpoints(
    source: &Source,
    target_home: &Path,
    accounts: &[Account],
    env: &Env,
) -> Result<Vec<PathBuf>> {
    let mut found: BTreeMap<OsString, PathBuf> = BTreeMap::new();
    for account in accounts.iter().filter(|a| a.provider == CLAUDE) {
        let Some(home) = account.home_dir(env) else {
            continue;
        };
        if fs::canonicalize(home.join("projects")).ok().as_ref() != Some(&source.store) {
            continue;
        }
        let Ok(listing) = fs::read_dir(home.join("file-history").join(&source.session_id)) else {
            continue;
        };
        for item in listing.flatten() {
            let path = item.path();
            if fs::metadata(&path).is_ok_and(|m| m.is_file()) {
                found.entry(item.file_name()).or_insert(path);
            }
        }
    }
    let dir = target_home.join("file-history").join(&source.session_id);
    let mut copied = Vec::new();
    for (name, from) in found {
        let dest = dir.join(&name);
        if fs::symlink_metadata(&dest).is_ok() {
            continue;
        }
        fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let tmp = temp_name(&dir, &name);
        let placed = fs::copy(&from, &tmp)
            .map_err(anyhow::Error::from)
            .and_then(|_| place_new(&tmp, &dest));
        match placed {
            Ok(()) => copied.push(dest),
            // Written meanwhile: the same immutable checkpoint, kept.
            Err(_) if fs::symlink_metadata(&dest).is_ok() => {
                let _ = fs::remove_file(&tmp);
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e.context(format!("cannot copy {}", from.display())));
            }
        }
    }
    Ok(copied)
}

/// Moves `tmp` to `dest`, which must not exist: a hard link fails rather than replace
/// anything; where links are not supported, a rename after checking.
fn place_new(tmp: &Path, dest: &Path) -> Result<()> {
    match fs::hard_link(tmp, dest) {
        Ok(()) => {
            fs::remove_file(tmp)?;
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            bail!("{} appeared meanwhile: not overwritten", dest.display())
        }
        Err(_) if fs::symlink_metadata(dest).is_err() => Ok(fs::rename(tmp, dest)?),
        Err(e) => Err(e.into()),
    }
}

fn temp_name(dir: &Path, name: &std::ffi::OsStr) -> PathBuf {
    dir.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        uuid::Uuid::new_v4().simple()
    ))
}

/// The length of `file` up to and including its last newline: a record still being written
/// is left out (R19).
fn complete_len(file: &File) -> io::Result<u64> {
    const CHUNK: u64 = 64 * 1024;
    let mut end = file.metadata()?.len();
    while end > 0 {
        let start = end.saturating_sub(CHUNK);
        let buf = read_at(file, start, end - start)?;
        if let Some(at) = complete_lines(&buf, false).end {
            return Ok(start + at as u64);
        }
        end = start;
    }
    Ok(0)
}

/// Size and mtime (nanoseconds since the epoch).
fn stat(meta: &fs::Metadata) -> (u64, i128) {
    (
        meta.len(),
        i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
    )
}

/// The size and mtime the launch log recorded for the last relay copy at `dest`, if any.
fn earlier_copy(launch_log: &Path, dest: &Path) -> Option<(u64, i128)> {
    #[derive(Deserialize)]
    struct Line {
        relay: Option<Relay>,
    }
    let bytes = fs::read(launch_log).ok()?;
    let dest = dest.to_str()?;
    complete_lines(&bytes, false)
        .lines
        .iter()
        .filter_map(|line| serde_json::from_slice::<Line>(line).ok()?.relay)
        .filter(|relay| relay.transcript == dest)
        .map(|relay| (relay.size, relay.mtime_ns))
        .next_back()
}

/// The relay copy a launch log line records, if any: hidden from History (R19).
pub fn copy_in(line: &[u8]) -> Option<PathBuf> {
    #[derive(Deserialize)]
    struct Line {
        relay: Option<Relay>,
    }
    serde_json::from_slice::<Line>(line)
        .ok()?
        .relay
        .map(|r| PathBuf::from(r.transcript))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::registry::{CODEX, Home};

    const ID: &str = "766560c5-74e6-45f5-89fd-d92926b14898";

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        env: Env,
        log: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        fs::create_dir_all(root.join("work")).unwrap();
        let env: Env = [("HOME".to_string(), root.join("home").display().to_string())].into();
        let log = root.join("state/launches.jsonl");
        Fixture {
            _dir: dir,
            root,
            env,
            log,
        }
    }

    fn named(name: &str, home: &Path) -> Account {
        Account {
            provider: CLAUDE,
            name: name.into(),
            home: Home::Path(home.display().to_string()),
        }
    }

    /// A home with a transcript for [`ID`] in `projects/-w/`, ending in `tail`.
    fn home_with_session(f: &Fixture, name: &str, tail: &str) -> (PathBuf, Source) {
        let home = f.root.join(name);
        let project = home.join("projects/-w");
        fs::create_dir_all(&project).unwrap();
        let transcript = project.join(format!("{ID}.jsonl"));
        fs::write(&transcript, format!("{{\"a\":1}}\n{{\"b\":2}}\n{tail}")).unwrap();
        let source = Source {
            store: fs::canonicalize(home.join("projects")).unwrap(),
            transcript: fs::canonicalize(&transcript).unwrap(),
            session_id: ID.into(),
            cwd_last: Some(f.root.join("work").display().to_string()),
        };
        (home, source)
    }

    fn target(f: &Fixture, name: &str) -> PathBuf {
        let home = f.root.join(name);
        fs::create_dir_all(&home).unwrap();
        home
    }

    #[test]
    fn complete_length_ends_at_the_last_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t");
        for (text, want) in [
            ("", 0),
            ("no newline", 0),
            ("a\n", 2),
            ("a\nb", 2),
            ("a\nbc\n", 5),
        ] {
            fs::write(&path, text).unwrap();
            assert_eq!(
                complete_len(&File::open(&path).unwrap()).unwrap(),
                want,
                "{text:?}"
            );
        }
        let long = format!("{}\n{}", "x".repeat(200_000), "y".repeat(100_000));
        fs::write(&path, &long).unwrap();
        assert_eq!(complete_len(&File::open(&path).unwrap()).unwrap(), 200_001);
    }

    /// R19: the copy ends at the last complete line; checkpoints are the union of every home
    /// sharing the transcript's store, and those already at the target are kept.
    #[test]
    fn copies_the_transcript_and_the_union_of_checkpoints() {
        let f = fixture();
        let (max, source) = home_with_session(&f, "max", "{\"partial\":");
        // `alt` shares max's store; `other` has its own and does not count.
        let (alt, other) = (target(&f, "alt"), target(&f, "other"));
        symlink(max.join("projects"), alt.join("projects")).unwrap();
        fs::create_dir_all(other.join("projects")).unwrap();
        for (home, names) in [
            (&max, &["h1@v1", "h2@v1"][..]),
            (&alt, &["h2@v1", "h3@v2"]),
            (&other, &["zz@v1"]),
        ] {
            let dir = home.join("file-history").join(ID);
            fs::create_dir_all(&dir).unwrap();
            for name in names {
                fs::write(dir.join(name), format!("{name} from {}", home.display())).unwrap();
            }
        }
        let team = target(&f, "team");
        let kept = team.join("file-history").join(ID);
        fs::create_dir_all(&kept).unwrap();
        fs::write(kept.join("h3@v2"), "already here").unwrap();
        let accounts = [
            named("max", &max),
            named("alt", &alt),
            named("other", &other),
            named("team", &team),
        ];
        let dest = check(&source, &accounts[3], &f.env, &f.log).unwrap();
        let relay = copy(&source, &dest, &accounts, &f.env).unwrap();

        let copy_path = team.join("projects/-w").join(format!("{ID}.jsonl"));
        assert_eq!(
            fs::read_to_string(&copy_path).unwrap(),
            "{\"a\":1}\n{\"b\":2}\n"
        );
        assert_eq!(relay.source, source.transcript.display().to_string());
        assert_eq!(
            relay.transcript,
            fs::canonicalize(&copy_path).unwrap().display().to_string()
        );
        let meta = fs::metadata(&copy_path).unwrap();
        assert_eq!((relay.size, relay.mtime_ns), stat(&meta));
        let mut names: Vec<String> = fs::read_dir(&kept)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["h1@v1", "h2@v1", "h3@v2"], "no temp files left");
        assert_eq!(
            fs::read_to_string(kept.join("h3@v2")).unwrap(),
            "already here"
        );
        assert!(
            fs::read_to_string(kept.join("h1@v1"))
                .unwrap()
                .contains("max")
        );
        assert_eq!(
            relay.checkpoints,
            [
                kept.join("h1@v1").display().to_string(),
                kept.join("h2@v1").display().to_string()
            ]
        );
        // The original is untouched.
        assert!(
            fs::read_to_string(&source.transcript)
                .unwrap()
                .ends_with("{\"partial\":")
        );
    }

    /// R19: without checkpoints anywhere nothing is created for them; a target without a
    /// `projects` directory gets one.
    #[test]
    fn missing_checkpoints_and_projects_are_fine() {
        let f = fixture();
        let (max, source) = home_with_session(&f, "max", "");
        let team = target(&f, "team");
        let accounts = [named("max", &max), named("team", &team)];
        let dest = check(&source, &accounts[1], &f.env, &f.log).unwrap();
        let relay = copy(&source, &dest, &accounts, &f.env).unwrap();
        assert!(relay.checkpoints.is_empty());
        assert!(!team.join("file-history").exists());
        assert!(
            team.join("projects/-w")
                .join(format!("{ID}.jsonl"))
                .is_file()
        );
    }

    fn log_relay(f: &Fixture, relay: &Relay) {
        fs::create_dir_all(f.log.parent().unwrap()).unwrap();
        let line =
            serde_json::json!({"account": "claude:team", "session_id": null, "relay": relay});
        let mut text = fs::read_to_string(&f.log).unwrap_or_default();
        text.push_str(&format!("{line}\n"));
        fs::write(&f.log, text).unwrap();
    }

    /// R19: an existing destination is replaced only when the launch log records it as an
    /// earlier relay copy with the same size and mtime.
    #[test]
    fn only_an_unchanged_earlier_copy_is_replaced() {
        let f = fixture();
        let (max, source) = home_with_session(&f, "max", "");
        let team = target(&f, "team");
        let accounts = [named("max", &max), named("team", &team)];
        let first = copy(
            &source,
            &check(&source, &accounts[1], &f.env, &f.log).unwrap(),
            &accounts,
            &f.env,
        )
        .unwrap();
        // Not in the log: someone else's file.
        let e = check(&source, &accounts[1], &f.env, &f.log).unwrap_err();
        assert!(
            e.to_string()
                .contains("is not an unchanged earlier relay copy"),
            "{e}"
        );
        log_relay(&f, &first);
        // The session grew meanwhile; the copy is replaced by a fresh one.
        let mut text = fs::read_to_string(&source.transcript).unwrap();
        text.push_str("{\"c\":3}\n");
        fs::write(&source.transcript, &text).unwrap();
        let dest = check(&source, &accounts[1], &f.env, &f.log).unwrap();
        let second = copy(&source, &dest, &accounts, &f.env).unwrap();
        assert_eq!(fs::read_to_string(&second.transcript).unwrap(), text);
        log_relay(&f, &second);
        // Changed since it was copied (e.g. claude wrote to it): not ours to replace.
        let mut changed = fs::read_to_string(&second.transcript).unwrap();
        changed.push_str("{\"d\":4}\n");
        fs::write(&second.transcript, changed).unwrap();
        assert!(check(&source, &accounts[1], &f.env, &f.log).is_err());
    }

    /// R19, R16: the refusals, all before anything is written.
    #[test]
    fn refusals() {
        let f = fixture();
        let (max, source) = home_with_session(&f, "max", "");
        let team = target(&f, "team");
        let err = |source: &Source, account: &Account| {
            check(source, account, &f.env, &f.log)
                .unwrap_err()
                .to_string()
        };
        let codex = Account {
            provider: CODEX,
            ..named("cx", &team)
        };
        assert!(err(&source, &codex).contains("is not a claude account"));
        // The target's store already holds the transcript: a fork does the job.
        let shared = target(&f, "shared");
        symlink(max.join("projects"), shared.join("projects")).unwrap();
        assert!(
            err(&source, &named("shared", &shared)).contains("fork it there instead"),
            "{}",
            err(&source, &named("shared", &shared))
        );
        assert!(err(&source, &named("max", &max)).contains("fork it there instead"));
        let gone = Source {
            cwd_last: Some(f.root.join("gone").display().to_string()),
            ..source.clone()
        };
        assert!(err(&gone, &named("team", &team)).contains("does not exist"));
        let nowhere = Source {
            cwd_last: None,
            ..source.clone()
        };
        assert!(err(&nowhere, &named("team", &team)).contains("no recorded directory"));
        let bad = Source {
            session_id: "-x".into(),
            ..source.clone()
        };
        assert!(err(&bad, &named("team", &team)).contains("is not a UUID"));
        assert!(err(&source, &named("nohome", &f.root.join("nohome"))).contains("does not exist"));
        // A symlink at the destination was not made by remuda.
        let dest_dir = team.join("projects/-w");
        fs::create_dir_all(&dest_dir).unwrap();
        symlink(&source.transcript, dest_dir.join(format!("{ID}.jsonl"))).unwrap();
        assert!(err(&source, &named("team", &team)).contains("remuda does not overwrite it"));
        assert!(!team.join("file-history").exists());
    }

    /// R19: a transcript with no complete line has nothing to continue: refused.
    #[test]
    fn a_transcript_without_a_complete_line_is_refused() {
        let f = fixture();
        let (_, mut source) = home_with_session(&f, "max", "");
        fs::write(&source.transcript, "{\"partial\":").unwrap();
        source.transcript = fs::canonicalize(&source.transcript).unwrap();
        let team = target(&f, "team");
        let e = check(&source, &named("team", &team), &f.env, &f.log).unwrap_err();
        assert!(e.to_string().contains("has no complete record"), "{e}");
    }

    /// R19: checkpoints go first; when one cannot be copied, no transcript is placed (and no
    /// temporary file is left).
    #[test]
    fn a_failed_checkpoint_copy_places_no_transcript() {
        use std::os::unix::fs::PermissionsExt;
        let f = fixture();
        let (max, source) = home_with_session(&f, "max", "");
        let checkpoints = max.join("file-history").join(ID);
        fs::create_dir_all(&checkpoints).unwrap();
        fs::write(checkpoints.join("locked@v1"), "x").unwrap();
        fs::set_permissions(
            checkpoints.join("locked@v1"),
            fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        let team = target(&f, "team");
        let accounts = [named("max", &max), named("team", &team)];
        let dest = check(&source, &accounts[1], &f.env, &f.log).unwrap();
        let e = copy(&source, &dest, &accounts, &f.env).unwrap_err();
        assert!(format!("{e:#}").contains("locked@v1"), "{e:#}");
        let project = team.join("projects/-w");
        assert_eq!(fs::read_dir(&project).unwrap().count(), 0, "nothing placed");
        let history = team.join("file-history").join(ID);
        assert_eq!(fs::read_dir(&history).unwrap().count(), 0, "no temp files");
    }

    /// R19: `discard` removes the copy remuda placed, and nothing that changed since.
    #[test]
    fn discard_removes_only_the_placed_copy() {
        let f = fixture();
        let (max, source) = home_with_session(&f, "max", "");
        let team = target(&f, "team");
        let accounts = [named("max", &max), named("team", &team)];
        let copy_of = || {
            let dest = check(&source, &accounts[1], &f.env, &f.log).unwrap();
            copy(&source, &dest, &accounts, &f.env).unwrap()
        };
        let relay = copy_of();
        discard(&relay).unwrap();
        assert!(!Path::new(&relay.transcript).exists());
        discard(&relay).unwrap();
        let relay = copy_of();
        fs::write(&relay.transcript, "changed\n").unwrap();
        assert!(discard(&relay).is_err());
        assert_eq!(fs::read_to_string(&relay.transcript).unwrap(), "changed\n");
    }
}
