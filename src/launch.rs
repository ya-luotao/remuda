//! Launching an agent for an account (SPEC R2, R6, R17, R18).

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use anyhow::{Result, bail};
use serde::Serialize;

use crate::Env;
use crate::provider::Provider;
use crate::registry::{Account, Home, Sharing};
use crate::relay::Relay;
use crate::share::{self, Injected, Shared};

pub const CONFIG_DIR_VAR: &str = "CLAUDE_CONFIG_DIR";
pub const SECURESTORAGE_VAR: &str = "CLAUDE_SECURESTORAGE_CONFIG_DIR";

/// The one change remuda makes to the inherited environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvChange {
    Set(String, String),
    Remove(String),
}

/// Named account: its provider's home variable (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`) set to the
/// home, byte-exact. Default: that variable removed (R2, R17).
pub fn env_change(account: &Account) -> EnvChange {
    let var = account.provider.home_var().to_string();
    match &account.home {
        Home::Default => EnvChange::Remove(var),
        Home::Path(home) => EnvChange::Set(var, home.clone()),
    }
}

/// Warnings about the inherited environment worth showing before a launch.
pub fn env_warnings(env: &Env) -> Vec<String> {
    let mut warnings = Vec::new();
    if let Some(value) = env.get(SECURESTORAGE_VAR) {
        warnings.push(format!(
            "{SECURESTORAGE_VAR} is set ({value:?}); passing it through unchanged, \
             so the Keychain entry is not keyed by the account home"
        ));
    }
    warnings
}

/// What a `claude` command line means for session tracking (R6). A `codex` command line is
/// only ever [`Intent::Existing`], through [`codex_intent`] (R17).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Starts a brand-new session: remuda pre-allocates its ID.
    NewSession,
    /// `--resume <of> --fork-session` without the user's own `--session-id`: a new session
    /// copied from `of`, whose ID remuda pre-allocates (R6).
    Fork { of: String },
    /// Resumes, continues or attaches to a session; the ID when the args name it, and for a
    /// fork, the session it copies when the args name it.
    Existing {
        session_id: Option<String>,
        fork_of: Option<String>,
    },
    /// Not a session at all (subcommand, help, version).
    NotASession,
}

/// claude 2.1.280 subcommands.
pub const SUBCOMMANDS: &[&str] = &[
    "agents",
    "attach",
    "auth",
    "auto-mode",
    "doctor",
    "gateway",
    "import",
    "install",
    "kill",
    "logs",
    "mcp",
    "plugin",
    "plugins",
    "project",
    "respawn",
    "rm",
    "setup-token",
    "stop",
    "ultrareview",
    "update",
    "upgrade",
];

/// Classifies passthrough args. When unsure, never [`Intent::NewSession`].
///
/// A subcommand name anywhere in the args (not only in first position) counts as a
/// subcommand: telling a prompt from an option value would need claude's full option table.
pub fn classify(args: &[String]) -> Intent {
    if args.iter().any(|a| SUBCOMMANDS.contains(&a.as_str())) {
        return Intent::NotASession;
    }
    if args.iter().any(|a| is_help_or_version(a)) {
        return Intent::NotASession;
    }

    let fork = args.iter().any(|a| a == "--fork-session");
    let mut existing = fork;
    // Resume or continue semantics other than `--resume <id>` / `--session-id <id>`.
    let mut other = false;
    let mut resume_id: Option<String> = None;
    let mut user_id: Option<String> = None;
    let mut user_id_given = false;
    let mut first_id: Option<String> = None;
    for (i, arg) in args.iter().enumerate() {
        let value = |v: Option<&String>| {
            v.filter(|next| !next.is_empty() && !next.starts_with('-'))
                .cloned()
        };
        let (found, is_session_id) = match arg.as_str() {
            "-r" | "--resume" => (value(args.get(i + 1)), false),
            "--session-id" => (value(args.get(i + 1)), true),
            "-c" | "--continue" | "--teleport" | "--from-pr" | "--cloud" => {
                other = true;
                continue;
            }
            _ => {
                if let Some(v) = arg.strip_prefix("--resume=") {
                    (Some(v.to_string()).filter(|v| !v.is_empty()), false)
                } else if let Some(v) = arg.strip_prefix("--session-id=") {
                    (Some(v.to_string()).filter(|v| !v.is_empty()), true)
                } else {
                    if ["--teleport=", "--from-pr=", "--cloud="]
                        .iter()
                        .any(|prefix| arg.starts_with(prefix))
                        || short_cluster(arg).is_some_and(|c| c.contains(['r', 'c']))
                    {
                        other = true;
                    }
                    continue;
                }
            }
        };
        existing = true;
        if is_session_id {
            user_id_given = true;
            user_id = user_id.or(found.clone());
        } else {
            resume_id = resume_id.or(found.clone());
        }
        first_id = first_id.or(found);
    }
    existing |= other;
    if !existing {
        return Intent::NewSession;
    }
    if !fork {
        return Intent::Existing {
            session_id: first_id,
            fork_of: None,
        };
    }
    // Only the shape verified with claude 2.1.281 gets an injected ID:
    // `--resume <id> --fork-session` with no `--session-id` and nothing else resuming.
    match resume_id {
        Some(of) if !user_id_given && !other => Intent::Fork { of },
        fork_of => Intent::Existing {
            session_id: user_id,
            fork_of,
        },
    }
}

fn is_help_or_version(arg: &str) -> bool {
    matches!(arg, "-h" | "--help" | "-v" | "--version")
        || short_cluster(arg).is_some_and(|c| c.contains(['h', 'v']))
}

/// Letters of a combined short-flag token such as `-pc`; `None` for anything else.
fn short_cluster(arg: &str) -> Option<&str> {
    let letters = arg.strip_prefix('-')?;
    (letters.len() >= 2 && letters.bytes().all(|b| b.is_ascii_alphabetic())).then_some(letters)
}

/// A session id as claude writes it: a lowercase hyphenated UUID (8-4-4-4-12 hex digits).
/// The TUI passes nothing else to claude as a session id: a transcript named `-x.jsonl`
/// would otherwise become an option (R16).
pub fn is_session_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => matches!(b, b'0'..=b'9' | b'a'..=b'f'),
        })
}

/// What a `codex` command line means for the launch log (R17): `resume <id> ...` continues
/// `<id>` in place (its `session_id`); `fork <id> ...` copies `<id>` into a session whose id
/// codex decides (`fork_of`, no `session_id`). Everything else names no session: new sessions,
/// `resume` / `fork` without an id or with `--last` (the positional is then a prompt), help,
/// version. Never [`Intent::NewSession`] or [`Intent::Fork`]: codex takes no id to inject.
///
/// Only `resume` / `fork` as the very first argument counts: further in, it may be an option's
/// value or part of a prompt, and telling those apart would need codex's full option table.
pub fn codex_intent(args: &[String]) -> Intent {
    let none = Intent::Existing {
        session_id: None,
        fork_of: None,
    };
    let [first, id, rest @ ..] = args else {
        return none;
    };
    let unsure = rest
        .iter()
        .any(|a| matches!(a.as_str(), "--last" | "-h" | "--help" | "-V" | "--version"));
    if id.is_empty() || id.starts_with('-') || unsure {
        return none;
    }
    match first.as_str() {
        "resume" => Intent::Existing {
            session_id: Some(id.clone()),
            fork_of: None,
        },
        "fork" => Intent::Existing {
            session_id: None,
            fork_of: Some(id.clone()),
        },
        _ => none,
    }
}

/// Adds `--session-id <id>` to claude's args (before a `--` terminator, if any).
pub fn inject_session_id(args: &[String], session_id: &str) -> Vec<String> {
    let at = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let mut out = args[..at].to_vec();
    out.push("--session-id".to_string());
    out.push(session_id.to_string());
    out.extend_from_slice(&args[at..]);
    out
}

/// One line of `$REMUDA_HOME/state/launches.jsonl` (R6, R9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LaunchRecord {
    /// RFC 3339, UTC.
    pub ts: String,
    /// `provider:name`.
    pub account: String,
    /// Home string, or `"default"`.
    pub home: String,
    pub cwd: Option<String>,
    /// Arguments as the user passed them (without the injected `--session-id`).
    pub args: Vec<String>,
    pub session_id: Option<String>,
    /// The session a fork copies, when known: claude's `--fork-session` (R6), `codex fork <id>`
    /// (R17); absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fork_of: Option<String>,
    pub injected: bool,
    /// Shared configuration injected before `args` (R18): option names and value sizes;
    /// absent when nothing was.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub shared: Vec<Injected>,
    /// What a relay copied before this fork (R19); absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay: Option<Relay>,
}

/// A fully decided launch: what to exec and what to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub args: Vec<String>,
    pub env: EnvChange,
    /// Variables added on top of `env` (shared configuration, R18).
    pub extra_env: Vec<(String, String)>,
    /// One-line messages for the user about this launch.
    pub notices: Vec<String>,
    pub record: LaunchRecord,
}

/// Decides args, env and the log record. `new_session_id` is called only for new claude
/// sessions and forks that get an injected ID (R6); codex args are passed on verbatim, and its
/// launches are logged with the resumed id for an in-place resume and without one otherwise
/// (R17).
pub fn prepare(
    account: &Account,
    user_args: Vec<String>,
    cwd: Option<&Path>,
    ts: String,
    new_session_id: impl FnOnce() -> String,
) -> Launch {
    prepare_with(account, user_args, cwd, ts, new_session_id, |_| {
        Ok(Shared::default())
    })
    .expect("nothing to share cannot fail")
}

/// The launch path of `remuda run`, the TUI and relay alike (R6, R16, R18, R19): [`prepare`],
/// plus the shared configuration of `sharing` for session invocations of claude accounts.
/// `config` is `$REMUDA_HOME/config.toml`.
#[allow(clippy::too_many_arguments)]
pub fn plan(
    account: &Account,
    user_args: Vec<String>,
    cwd: Option<&Path>,
    ts: String,
    new_session_id: impl FnOnce() -> String,
    sharing: &Sharing,
    env: &Env,
    config: &Path,
) -> Result<Launch> {
    prepare_with(account, user_args, cwd, ts, new_session_id, |args| {
        share::inject(sharing, account, args, cwd, env, config)
    })
}

/// [`prepare`], with `shared` asked for what to inject only for a claude session invocation:
/// anything but a subcommand, help or version (R6, R18). Its options go before the user's
/// arguments, so a variadic option cannot swallow them; its variables are added to the
/// child's environment.
pub fn prepare_with(
    account: &Account,
    user_args: Vec<String>,
    cwd: Option<&Path>,
    ts: String,
    new_session_id: impl FnOnce() -> String,
    shared: impl FnOnce(&[String]) -> Result<Shared>,
) -> Result<Launch> {
    let env = env_change(account);
    let intent = match account.provider {
        Provider::Claude => classify(&user_args),
        Provider::Codex => codex_intent(&user_args),
    };
    let shared = match (account.provider, &intent) {
        (Provider::Claude, Intent::NewSession | Intent::Fork { .. } | Intent::Existing { .. }) => {
            shared(&user_args)?
        }
        _ => Shared::default(),
    };
    let (args, session_id, fork_of, injected) = match intent {
        Intent::NewSession => {
            let id = new_session_id();
            (inject_session_id(&user_args, &id), Some(id), None, true)
        }
        Intent::Fork { of } => {
            let id = new_session_id();
            (inject_session_id(&user_args, &id), Some(id), Some(of), true)
        }
        Intent::Existing {
            session_id,
            fork_of,
        } => (user_args.clone(), session_id, fork_of, false),
        Intent::NotASession => (user_args.clone(), None, None, false),
    };
    let record = LaunchRecord {
        ts,
        account: account.qualified(),
        home: account.home.to_string(),
        cwd: cwd.map(|p| p.to_string_lossy().into_owned()),
        args: user_args,
        session_id,
        fork_of,
        injected,
        shared: shared.logged(),
        relay: None,
    };
    Ok(Launch {
        args: [shared.args, args].concat(),
        env,
        extra_env: shared.env,
        notices: shared.notices,
        record,
    })
}

/// Appends one JSON line to the launch log, creating its directory.
pub fn append_log(log: &Path, record: &LaunchRecord) -> Result<()> {
    if let Some(dir) = log.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut line = serde_json::to_string(record)?;
    line.push('\n');
    // One write on an O_APPEND file: concurrent launches do not interleave lines.
    let mut file = fs::OpenOptions::new().create(true).append(true).open(log)?;
    file.write_all(line.as_bytes())?;
    Ok(())
}

/// Finds an executable `program` in a `PATH`-style list (empty entries mean the cwd).
pub fn find_on_path(program: &str, path_var: Option<&str>) -> Result<PathBuf> {
    let Some(path_var) = path_var else {
        bail!("PATH is not set; cannot find `{program}`");
    };
    for dir in path_var.split(':') {
        let dir = if dir.is_empty() { "." } else { dir };
        let candidate = Path::new(dir).join(program);
        if let Ok(meta) = candidate.metadata()
            && meta.is_file()
            && meta.permissions().mode() & 0o111 != 0
        {
            return Ok(candidate);
        }
    }
    bail!("`{program}` not found on PATH")
}

/// Replaces the current process with `program` running `plan` under the inherited
/// environment plus the plan's changes, in `cwd` (`None`: remuda's own). Only returns on
/// failure.
pub fn exec(program: &Path, plan: &Launch, cwd: Option<&Path>) -> io::Error {
    let mut cmd = command(program, &plan.args, &plan.env, cwd);
    cmd.envs(plan.extra_env.iter().map(|(k, v)| (k, v)));
    cmd.exec()
}

fn command(program: &Path, args: &[String], change: &EnvChange, cwd: Option<&Path>) -> Command {
    let mut cmd = Command::new(program);
    if let Some(name) = program.file_name() {
        cmd.arg0(name);
    }
    cmd.args(args);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    apply_env(&mut cmd, change);
    cmd
}

/// Runs `program args...` in the foreground (inherited stdio) under the inherited
/// environment plus `change`, in `cwd` (`None`: remuda's own), and waits for it.
///
/// Like `system(3)`: while the child runs, remuda ignores SIGINT and SIGQUIT (Ctrl-C and
/// Ctrl-\ go to the whole foreground process group), and the child gets their default
/// actions back.
pub fn run_foreground(
    program: &Path,
    args: &[String],
    change: &EnvChange,
    cwd: Option<&Path>,
) -> io::Result<ExitStatus> {
    foreground(command(program, args, change, cwd))
}

fn foreground(mut cmd: Command) -> io::Result<ExitStatus> {
    // SAFETY: the closure runs in the forked child before exec and only calls `signal`,
    // which is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGQUIT, libc::SIG_DFL);
            Ok(())
        });
    }
    let _ignored = IgnoreInterrupts::new();
    cmd.status()
}

/// SIGINT and SIGQUIT ignored for as long as any guard lives; the previous actions come back
/// when the last one is dropped (overlapping guards on several threads nest correctly).
struct IgnoreInterrupts;

/// Live guards, and the actions to restore after the last one.
static IGNORING: std::sync::Mutex<(usize, libc::sighandler_t, libc::sighandler_t)> =
    std::sync::Mutex::new((0, 0, 0));

impl IgnoreInterrupts {
    fn new() -> Self {
        let mut state = IGNORING.lock().unwrap_or_else(|e| e.into_inner());
        if state.0 == 0 {
            // SAFETY: installs SIG_IGN; the previous handlers are restored by the last drop.
            unsafe {
                state.1 = libc::signal(libc::SIGINT, libc::SIG_IGN);
                state.2 = libc::signal(libc::SIGQUIT, libc::SIG_IGN);
            }
        }
        state.0 += 1;
        IgnoreInterrupts
    }
}

impl Drop for IgnoreInterrupts {
    fn drop(&mut self) {
        let mut state = IGNORING.lock().unwrap_or_else(|e| e.into_inner());
        state.0 -= 1;
        if state.0 == 0 {
            // SAFETY: restores the handlers the first guard replaced.
            unsafe {
                libc::signal(libc::SIGINT, state.1);
                libc::signal(libc::SIGQUIT, state.2);
            }
        }
    }
}

/// What [`perform`] did.
#[derive(Debug)]
pub struct Ran {
    /// claude's exit status, or why it could not be started.
    pub status: io::Result<ExitStatus>,
    /// The launch log could not be written (claude ran anyway, as with `run`).
    pub log_error: Option<String>,
}

/// A launch from the TUI (R6, R16): `plan` is logged exactly as `remuda run` logs it, then
/// claude runs in the foreground in `cwd` and remuda waits for it. A directory that does not
/// exist fails before anything is logged or run.
pub fn perform(program: &Path, plan: &Launch, cwd: Option<&Path>, log: &Path) -> Ran {
    if let Some(dir) = cwd
        && !dir.is_dir()
    {
        return Ran {
            status: Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} is not an existing directory", dir.display()),
            )),
            log_error: None,
        };
    }
    let log_error = append_log(log, &plan.record)
        .err()
        .map(|e| format!("cannot write launch log {}: {e:#}", log.display()));
    let status = run_plan(program, plan, cwd);
    Ran { status, log_error }
}

/// Runs `plan` in the foreground like [`perform`], without logging it: the caller did.
pub fn run_plan(program: &Path, plan: &Launch, cwd: Option<&Path>) -> io::Result<ExitStatus> {
    let mut cmd = command(program, &plan.args, &plan.env, cwd);
    cmd.envs(plan.extra_env.iter().map(|(k, v)| (k, v)));
    foreground(cmd)
}

/// Applies `change` on top of the environment `cmd` inherits.
pub(crate) fn apply_env(cmd: &mut Command, change: &EnvChange) {
    match change {
        EnvChange::Set(key, value) => cmd.env(key, value),
        EnvChange::Remove(key) => cmd.env_remove(key),
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{CLAUDE, CODEX};

    fn account(name: &str, home: Home) -> Account {
        Account {
            provider: CLAUDE,
            name: name.into(),
            home,
        }
    }

    #[test]
    fn env_change_for_named_and_default() {
        assert_eq!(
            env_change(&account("max", Home::Path("/p/max/".into()))),
            EnvChange::Set("CLAUDE_CONFIG_DIR".into(), "/p/max/".into())
        );
        assert_eq!(
            env_change(&Account::default_for(CLAUDE)),
            EnvChange::Remove("CLAUDE_CONFIG_DIR".into())
        );
        // Codex: `CODEX_HOME`, set or removed the same way (R4, R17).
        let codex = Account {
            provider: CODEX,
            name: "x".into(),
            home: Home::Path("/c/".into()),
        };
        assert_eq!(
            env_change(&codex),
            EnvChange::Set("CODEX_HOME".into(), "/c/".into())
        );
        assert_eq!(
            env_change(&Account::default_for(CODEX)),
            EnvChange::Remove("CODEX_HOME".into())
        );
    }

    #[test]
    fn securestorage_warning() {
        let mut env = Env::new();
        assert!(env_warnings(&env).is_empty());
        env.insert(SECURESTORAGE_VAR.into(), "/s".into());
        let w = env_warnings(&env);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains(SECURESTORAGE_VAR));
    }

    #[test]
    fn path_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        fs::create_dir(&a).unwrap();
        fs::create_dir(&b).unwrap();
        // Not executable in `a`, executable in `b`: `b` wins.
        fs::write(a.join("claude"), "").unwrap();
        fs::write(b.join("claude"), "").unwrap();
        fs::set_permissions(b.join("claude"), fs::Permissions::from_mode(0o755)).unwrap();
        // A directory named claude is skipped too.
        fs::create_dir(dir.path().join("c")).unwrap();
        fs::create_dir(dir.path().join("c").join("claude")).unwrap();
        let path = format!(
            "{}:{}:{}",
            dir.path().join("c").display(),
            a.display(),
            b.display()
        );
        assert_eq!(
            find_on_path("claude", Some(&path)).unwrap(),
            b.join("claude")
        );
        assert!(find_on_path("claude", Some(&a.display().to_string())).is_err());
        assert!(find_on_path("claude", None).is_err());
    }
    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn existing(id: Option<&str>) -> Intent {
        forked(id, None)
    }

    fn forked(id: Option<&str>, of: Option<&str>) -> Intent {
        Intent::Existing {
            session_id: id.map(str::to_string),
            fork_of: of.map(str::to_string),
        }
    }

    fn fork(of: &str) -> Intent {
        Intent::Fork { of: of.into() }
    }

    #[test]
    fn session_ids_are_lowercase_hyphenated_uuids() {
        assert!(is_session_id("766560c5-74e6-45f5-89fd-d92926b14898"));
        for bad in [
            "",
            "a",
            "--dangerously-skip-permissions",
            "766560C5-74E6-45F5-89FD-D92926B14898",
            "766560c574e645f589fdd92926b14898",
            "{766560c5-74e6-45f5-89fd-d92926b14898}",
            "766560c5-74e6-45f5-89fd-d92926b1489g",
            "766560c5_74e6-45f5-89fd-d92926b14898",
            "-66560c5-74e6-45f5-89fd-d92926b14898",
        ] {
            assert!(!is_session_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn classify_table() {
        use Intent::*;
        let cases: &[(&[&str], Intent)] = &[
            // new sessions
            (&[], NewSession),
            (&["fix the bug"], NewSession),
            (&["update the readme"], NewSession),
            (&["-p", "hi there"], NewSession),
            (&["--print", "hi"], NewSession),
            (&["--model", "opus"], NewSession),
            (&["--model", "opus", "-p", "x", "-n", "name"], NewSession),
            (&["--", "hello"], NewSession),
            // subcommands
            (&["agents"], NotASession),
            (&["agents", "--json"], NotASession),
            (&["auth", "status", "--json"], NotASession),
            (&["mcp", "list"], NotASession),
            (&["setup-token"], NotASession),
            // a subcommand name anywhere is treated as "unsure": no injection
            (&["--model", "opus", "agents"], NotASession),
            (&["-p", "update"], NotASession),
            // resume / continue / attach semantics
            (&["-r"], existing(None)),
            (&["-r", "abc"], existing(Some("abc"))),
            (&["--resume"], existing(None)),
            (&["--resume", "abc"], existing(Some("abc"))),
            (&["--resume=abc"], existing(Some("abc"))),
            (&["--resume="], existing(None)),
            (&["--resume", "-p", "hi"], existing(None)),
            (&["-p", "hi", "--resume", "abc"], existing(Some("abc"))),
            (&["-c"], existing(None)),
            (&["--continue", "-p", "go on"], existing(None)),
            (&["--session-id", "u-1"], existing(Some("u-1"))),
            (&["--session-id=u-1"], existing(Some("u-1"))),
            (&["--session-id"], existing(None)),
            (&["--teleport"], existing(None)),
            (&["--teleport", "x"], existing(None)),
            (&["--from-pr", "12"], existing(None)),
            (&["--from-pr=12"], existing(None)),
            (&["--cloud"], existing(None)),
            (&["--cloud=x"], existing(None)),
            // combined short flags containing r/c are treated as resume/continue
            (&["-pc", "hi"], existing(None)),
            (&["-rp"], existing(None)),
            // help and version
            (&["-h"], NotASession),
            (&["--help"], NotASession),
            (&["-v"], NotASession),
            (&["--version"], NotASession),
            (&["--resume", "abc", "--help"], NotASession),
            (&["-ph"], NotASession),
            // --fork-session with a resume ID: remuda pre-allocates the fork's ID (R6)
            (&["--resume", "abc", "--fork-session"], fork("abc")),
            (&["--fork-session", "-r", "abc"], fork("abc")),
            (&["--resume=abc", "--fork-session"], fork("abc")),
            (
                &["--resume", "abc", "--fork-session", "-p", "hi"],
                fork("abc"),
            ),
            // ... unless the user chose the ID: it is logged, nothing is injected
            (
                &["--resume", "abc", "--fork-session", "--session-id", "u-1"],
                forked(Some("u-1"), Some("abc")),
            ),
            (
                &["--session-id=u-1", "--fork-session", "--resume", "abc"],
                forked(Some("u-1"), Some("abc")),
            ),
            (
                &["--resume", "abc", "--fork-session", "--session-id"],
                forked(None, Some("abc")),
            ),
            // ... or the forked session is not named, or other resume flags make it unclear
            (&["--resume", "--fork-session"], forked(None, None)),
            (&["-c", "--fork-session", "-p", "hi"], forked(None, None)),
            (
                &["-r", "abc", "-c", "--fork-session"],
                forked(None, Some("abc")),
            ),
            (
                &["--resume", "abc", "--teleport", "--fork-session"],
                forked(None, Some("abc")),
            ),
            (
                &["--session-id", "u-1", "--fork-session"],
                forked(Some("u-1"), None),
            ),
            (&["--fork-session"], forked(None, None)),
            (&["--fork-session", "--help"], NotASession),
        ];
        for (input, want) in cases {
            assert_eq!(&classify(&args(input)), want, "args {input:?}");
        }
    }

    #[test]
    fn every_listed_subcommand_is_not_a_session() {
        for sub in SUBCOMMANDS {
            assert_eq!(classify(&args(&[sub])), Intent::NotASession, "{sub}");
            assert_eq!(
                classify(&args(&[sub, "--help"])),
                Intent::NotASession,
                "{sub}"
            );
        }
    }

    #[test]
    fn prompt_that_merely_contains_a_flag_is_a_new_session() {
        assert_eq!(
            classify(&args(&["-p", "--resume is broken"])),
            Intent::NewSession
        );
        assert_eq!(
            classify(&args(&["why does --help hang"])),
            Intent::NewSession
        );
    }

    #[test]
    fn injection_appends_or_goes_before_double_dash() {
        assert_eq!(inject_session_id(&[], "U"), args(&["--session-id", "U"]));
        assert_eq!(
            inject_session_id(&args(&["-p", "hi"]), "U"),
            args(&["-p", "hi", "--session-id", "U"])
        );
        assert_eq!(
            inject_session_id(&args(&["--model", "opus", "--", "--x", "--"]), "U"),
            args(&["--model", "opus", "--session-id", "U", "--", "--x", "--"])
        );
    }

    #[test]
    fn prepare_new_session() {
        let acc = account("max", Home::Path("/p/max/".into()));
        let l = prepare(
            &acc,
            args(&["-p", "hi"]),
            Some(Path::new("/w d")),
            "T".into(),
            || "U".into(),
        );
        assert_eq!(l.args, args(&["-p", "hi", "--session-id", "U"]));
        assert_eq!(
            l.env,
            EnvChange::Set(CONFIG_DIR_VAR.into(), "/p/max/".into())
        );
        assert_eq!(
            l.record,
            LaunchRecord {
                ts: "T".into(),
                account: "claude:max".into(),
                home: "/p/max/".into(),
                cwd: Some("/w d".into()),
                args: args(&["-p", "hi"]),
                session_id: Some("U".into()),
                fork_of: None,
                injected: true,
                shared: vec![],
                relay: None,
            }
        );
    }

    #[test]
    fn prepare_fork_injects_a_new_id_and_records_the_original() {
        let acc = account("max", Home::Path("/p/max/".into()));
        let l = prepare(
            &acc,
            args(&["--resume", "abc", "--fork-session"]),
            Some(Path::new("/w")),
            "T".into(),
            || "U".into(),
        );
        assert_eq!(
            l.args,
            args(&["--resume", "abc", "--fork-session", "--session-id", "U"])
        );
        assert_eq!(l.record.args, args(&["--resume", "abc", "--fork-session"]));
        assert_eq!(l.record.session_id.as_deref(), Some("U"));
        assert_eq!(l.record.fork_of.as_deref(), Some("abc"));
        assert!(l.record.injected);

        // The user's own --session-id: kept, logged, nothing injected.
        let never = || -> String { panic!("must not allocate an id") };
        let user = args(&["--resume", "abc", "--fork-session", "--session-id", "Y"]);
        let l = prepare(&acc, user.clone(), None, "T".into(), never);
        assert_eq!(l.args, user);
        assert_eq!(l.record.session_id.as_deref(), Some("Y"));
        assert_eq!(l.record.fork_of.as_deref(), Some("abc"));
        assert!(!l.record.injected);
    }

    #[test]
    fn prepare_existing_and_non_sessions_do_not_generate_ids() {
        let acc = Account::default_for(CLAUDE);
        let never = || -> String { panic!("must not allocate an id") };
        let l = prepare(&acc, args(&["--resume", "abc"]), None, "T".into(), never);
        assert_eq!(l.args, args(&["--resume", "abc"]));
        assert_eq!(l.env, EnvChange::Remove(CONFIG_DIR_VAR.into()));
        assert_eq!(l.record.home, "default");
        assert_eq!(l.record.account, "claude:default");
        assert_eq!(l.record.cwd, None);
        assert_eq!(l.record.session_id.as_deref(), Some("abc"));
        assert_eq!(l.record.fork_of, None);
        assert!(!l.record.injected);

        let l = prepare(&acc, args(&["agents", "--json"]), None, "T".into(), never);
        assert_eq!(l.args, args(&["agents", "--json"]));
        assert_eq!(l.record.session_id, None);
        assert!(!l.record.injected);
    }

    /// R6, R18: shared configuration is asked for only for claude session invocations; its
    /// options go before the user's arguments (and `--session-id` after them), its variables
    /// into the child's environment, and the log keeps only option names and value sizes.
    #[test]
    fn shared_configuration_goes_before_the_users_arguments() {
        let acc = account("max", Home::Path("/p/max".into()));
        let shared = |_: &[String]| -> Result<Shared> {
            Ok(Shared {
                args: args(&["--add-dir=/r/shared/claude", "--settings={\"a\":1}"]),
                env: vec![("V".into(), "1".into())],
                notices: vec!["note".into()],
            })
        };
        let l = prepare_with(
            &acc,
            args(&["-p", "hi", "--", "x"]),
            None,
            "T".into(),
            || "U".into(),
            shared,
        )
        .unwrap();
        assert_eq!(
            l.args,
            args(&[
                "--add-dir=/r/shared/claude",
                "--settings={\"a\":1}",
                "-p",
                "hi",
                "--session-id",
                "U",
                "--",
                "x"
            ])
        );
        assert_eq!(l.extra_env, [("V".to_string(), "1".to_string())]);
        assert_eq!(l.notices, ["note"]);
        assert_eq!(l.record.args, args(&["-p", "hi", "--", "x"]));
        let v = serde_json::to_value(&l.record).unwrap();
        assert_eq!(
            v["shared"],
            serde_json::json!([
                {"option": "--add-dir", "bytes": 16},
                {"option": "--settings", "bytes": 7},
            ])
        );

        // Resumes, continues and forks are sessions too.
        for user in [
            args(&["--resume", "abc"]),
            args(&["-c"]),
            args(&["--resume", "abc", "--fork-session"]),
        ] {
            let l =
                prepare_with(&acc, user.clone(), None, "T".into(), || "U".into(), shared).unwrap();
            assert_eq!(l.args[0], "--add-dir=/r/shared/claude", "{user:?}");
        }
        // Subcommands, help and version are not; nor is anything codex runs.
        let never = |_: &[String]| -> Result<Shared> { panic!("must not inject") };
        for user in [
            args(&["agents", "--json"]),
            args(&["--help"]),
            args(&["-h"]),
            args(&["--version"]),
            args(&["-v"]),
            args(&["-p", "update"]),
        ] {
            let l =
                prepare_with(&acc, user.clone(), None, "T".into(), || "U".into(), never).unwrap();
            assert_eq!(l.args, user);
            assert!(l.extra_env.is_empty() && l.record.shared.is_empty());
        }
        let codex = Account {
            provider: CODEX,
            ..acc.clone()
        };
        let l = prepare_with(
            &codex,
            args(&["-p", "hi"]),
            None,
            "T".into(),
            || "U".into(),
            never,
        )
        .unwrap();
        assert_eq!(l.args, args(&["-p", "hi"]));
        // A failure (a settings file that is not an object) fails the launch.
        let failing = |_: &[String]| -> Result<Shared> { anyhow::bail!("bad settings") };
        assert!(prepare_with(&acc, vec![], None, "T".into(), || "U".into(), failing).is_err());
    }

    /// R17: `codex resume <id>` names the session it runs, `codex fork <id>` the session it
    /// copies; everything else (new sessions, a resume or fork without an id, help) names none.
    #[test]
    fn codex_intent_table() {
        let id = "019c1e08-e4f6-7d70-a129-38ec744a3f3c";
        let existing = |session_id: Option<&str>, fork_of: Option<&str>| Intent::Existing {
            session_id: session_id.map(str::to_string),
            fork_of: fork_of.map(str::to_string),
        };
        let r = |id: &str| existing(Some(id), None);
        let f = |id: &str| existing(None, Some(id));
        let none = existing(None, None);
        let cases: &[(&[&str], Intent)] = &[
            // in-place resumes and forks, as the TUI and `run` pass them
            (&["resume", id], r(id)),
            (&["resume", id, "-C", "/w"], r(id)),
            (&["resume", id, "fix the bug"], r(id)),
            (&["resume", "abc"], r("abc")),
            (&["fork", id], f(id)),
            (&["fork", id, "-C", "/w"], f(id)),
            (&["fork", id, "try another way"], f(id)),
            // no id: codex picks (or `--last` takes the newest; a positional is then a prompt)
            (&["resume"], none.clone()),
            (&["resume", "--last"], none.clone()),
            (&["resume", "--all"], none.clone()),
            (&["resume", "-C", "/w", id], none.clone()),
            (&["resume", id, "--last"], none.clone()),
            (&["resume", ""], none.clone()),
            (&["resume", "--", id], none.clone()),
            (&["fork"], none.clone()),
            (&["fork", "--last"], none.clone()),
            (&["fork", id, "--last"], none.clone()),
            (&["fork", "-C", "/w", id], none.clone()),
            // help and version are not sessions
            (&["resume", id, "--help"], none.clone()),
            (&["resume", id, "-h"], none.clone()),
            (&["resume", id, "--version"], none.clone()),
            (&["resume", id, "-V"], none.clone()),
            (&["fork", id, "--help"], none.clone()),
            // new sessions: codex decides the id
            (&[], none.clone()),
            (&["fix the bug"], none.clone()),
            (&["-C", "/w"], none.clone()),
            (&["login"], none.clone()),
            // `resume` / `fork` not in first position may be an option value or a prompt
            (&["-m", "o3", "resume", id], none.clone()),
            (&["-p", "resume", id], none.clone()),
            (&["-p", "fork", id], none.clone()),
            (&["--resume", id], none.clone()),
            (&["exec", "resume", id], none.clone()),
        ];
        for (input, want) in cases {
            assert_eq!(codex_intent(&args(input)), *want, "args {input:?}");
        }
    }

    /// R17: `codex resume <id>` is logged with the resumed id, args verbatim, nothing injected.
    #[test]
    fn prepare_codex_resume_logs_the_resumed_id() {
        let acc = Account {
            provider: CODEX,
            name: "work".into(),
            home: Home::Path("/c/work".into()),
        };
        let never = || -> String { panic!("must not allocate an id") };
        let id = "019c1e08-e4f6-7d70-a129-38ec744a3f3c";
        let user = args(&["resume", id, "-C", "/w"]);
        let l = prepare(&acc, user.clone(), Some(Path::new("/w")), "T".into(), never);
        assert_eq!(l.args, user);
        assert_eq!(l.env, EnvChange::Set("CODEX_HOME".into(), "/c/work".into()));
        assert_eq!(
            l.record,
            LaunchRecord {
                ts: "T".into(),
                account: "codex:work".into(),
                home: "/c/work".into(),
                cwd: Some("/w".into()),
                args: user,
                session_id: Some(id.into()),
                fork_of: None,
                injected: false,
                shared: vec![],
                relay: None,
            }
        );
    }

    /// R17: `codex fork <id>` is logged with the forked id as `fork_of` and no session id (codex
    /// decides it), args verbatim, nothing injected.
    #[test]
    fn prepare_codex_fork_logs_fork_of() {
        let acc = Account {
            provider: CODEX,
            name: "work".into(),
            home: Home::Path("/c/work".into()),
        };
        let never = || -> String { panic!("must not allocate an id") };
        let id = "019c1e08-e4f6-7d70-a129-38ec744a3f3c";
        let user = args(&["fork", id, "-C", "/w"]);
        let l = prepare(&acc, user.clone(), Some(Path::new("/w")), "T".into(), never);
        assert_eq!(l.args, user);
        assert_eq!(l.record.session_id, None);
        assert_eq!(l.record.fork_of.as_deref(), Some(id));
        assert!(!l.record.injected);
    }

    /// R17: codex args go through verbatim, nothing is injected, and new sessions (and anything
    /// that names no session) are logged without a session id (codex decides it).
    #[test]
    fn prepare_codex_passes_args_verbatim() {
        let acc = Account {
            provider: CODEX,
            name: "work".into(),
            home: Home::Path("/c/work".into()),
        };
        let never = || -> String { panic!("must not allocate an id") };
        for user in [
            args(&[]),
            args(&["-p", "hi"]),
            args(&["fork", "--last"]),
            args(&["resume", "--last"]),
            args(&["--resume", "abc", "--fork-session"]),
        ] {
            let l = prepare(&acc, user.clone(), Some(Path::new("/w")), "T".into(), never);
            assert_eq!(l.args, user);
            assert_eq!(l.env, EnvChange::Set("CODEX_HOME".into(), "/c/work".into()));
            assert_eq!(l.record.account, "codex:work");
            assert_eq!(l.record.args, user);
            assert_eq!(l.record.session_id, None);
            assert_eq!(l.record.fork_of, None);
            assert!(!l.record.injected);
        }
    }

    #[test]
    fn log_lines_append_as_json() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("state").join("launches.jsonl");
        let rec = LaunchRecord {
            ts: "2026-09-24T00:00:00Z".into(),
            account: "claude:max".into(),
            home: "/p/max/".into(),
            cwd: Some("/w".into()),
            args: args(&["-p", "multi\nline"]),
            session_id: None,
            fork_of: None,
            injected: false,
            shared: vec![],
            relay: None,
        };
        append_log(&log, &rec).unwrap();
        append_log(&log, &rec).unwrap();
        let text = fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(text.ends_with('\n'));
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["args"][1], "multi\nline");
        assert_eq!(v["session_id"], serde_json::Value::Null);
        assert_eq!(v["injected"], false);
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(
            keys.len(),
            7,
            "ts, account, home, cwd, args, session_id, injected: {keys:?}"
        );
        // `fork_of` appears only on forks.
        let fork = LaunchRecord {
            session_id: Some("U".into()),
            fork_of: Some("abc".into()),
            injected: true,
            ..rec
        };
        let v = serde_json::to_value(&fork).unwrap();
        assert_eq!(v["fork_of"], "abc");
        assert_eq!(v.as_object().unwrap().len(), 8);
    }
}
