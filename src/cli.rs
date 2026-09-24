//! Command-line surface (SPEC R5).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::identity::{self, Identity};
use crate::index::{self, Index};
use crate::provider::Provider;
use crate::registry::{self, Account, Registry};
use crate::{Env, paths};
use crate::{attribution, launch, live, probe, setup, text, transcript, tui, usage};

#[derive(Debug, Parser)]
#[command(
    name = "remuda",
    version,
    about = "Multi-account and session manager for coding agents"
)]
pub struct Cli {
    /// Without a command: open the TUI
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List accounts with their login identity (`claude auth status --json`, `codex login status`)
    List {
        /// Seconds to wait for each account's `claude auth status`
        #[arg(long, value_name = "SECONDS", default_value = "15", value_parser = parse_timeout)]
        timeout: Duration,
    },
    /// Register an existing home directory as an account
    Add {
        /// Agent provider: `claude` or `codex`
        #[arg(long, default_value = "claude")]
        provider: String,
        /// Account name: [A-Za-z0-9_-]+
        name: String,
        /// Existing home directory; stored exactly as given (a leading `~` is expanded once)
        path: String,
    },
    /// Show usage limits: cached in each account's .claude.json, or queried live
    Usage {
        /// Only this account (`name` or `provider:name`)
        account: Option<String>,
        /// Ask claude now (`claude -p /usage`) instead of reading the cache
        #[arg(long)]
        live: bool,
        /// Seconds to wait for each account's live query (claude scans local session history: 2-20 s)
        #[arg(long, value_name = "SECONDS", default_value = "90", value_parser = parse_timeout)]
        timeout: Duration,
    },
    /// Recent sessions, newest first: time, accounts, title, cwd
    Sessions {
        /// How many sessions to show
        #[arg(long, value_name = "N", default_value = "30")]
        limit: usize,
    },
    /// Create a new home under $REMUDA_HOME/homes/<provider>/<name>, register it and log in
    Setup {
        /// Agent provider: `claude` (logs in with `claude auth login`) or `codex` (`codex login`)
        #[arg(long, default_value = "claude")]
        provider: String,
        /// Account name: [A-Za-z0-9_-]+
        name: String,
        /// Passed to `claude auth login --email` (claude only)
        #[arg(long)]
        email: Option<String>,
    },
    /// Launch claude as an account; all arguments after the account go to claude verbatim
    ///
    /// `-h/--help` is not handled here so that `remuda run <account> --help` reaches claude;
    /// use `remuda help run`.
    #[command(disable_help_flag = true)]
    Run {
        /// Account: `name` or `provider:name` (`default` is the native login). Without it the
        /// TUI's picker chooses one, and no other arguments are taken
        // Hyphen values are accepted here only to explain that (`remuda run --resume x`).
        #[arg(allow_hyphen_values = true)]
        account: Option<String>,
        /// Arguments passed to claude unchanged
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// Process context captured by `main`: the only place that reads the real environment,
/// the clock, the system time zone and whether the standard streams are terminals.
pub struct Context {
    pub env: Env,
    pub cwd: Option<PathBuf>,
    pub now: Timestamp,
    /// The clock, for the TUI (which keeps running); commands use `now`.
    pub clock: fn() -> Timestamp,
    pub tz: TimeZone,
    pub stdin_is_tty: bool,
    pub stdout_is_tty: bool,
    pub stderr_is_tty: bool,
}

/// Runs a parsed command line; user and runtime errors become `remuda: <message>`, exit 1.
pub fn run(cli: Cli, ctx: &Context) -> ExitCode {
    match dispatch(cli, ctx) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("remuda: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(cli: Cli, ctx: &Context) -> Result<ExitCode> {
    if cli.command.is_none() && !(ctx.stdin_is_tty && ctx.stdout_is_tty) {
        bail!("the TUI needs a terminal (try `remuda list`, `remuda sessions`, `remuda usage`)");
    }
    let config = paths::remuda_home(&ctx.env)?.join("config.toml");
    match cli.command {
        None => tui(&config, ctx),
        Some(Command::List { timeout }) => list(&config, timeout, ctx),
        Some(Command::Add {
            provider,
            name,
            path,
        }) => {
            let provider = parse_provider(&provider)?;
            let outcome = registry::add(&config, provider, &name, &path, &ctx.env)?;
            for warning in &outcome.warnings {
                eprintln!("remuda: warning: {warning}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Usage {
            account,
            live,
            timeout,
        }) => usage(&config, account, live, timeout, ctx),
        Some(Command::Sessions { limit }) => sessions(&config, limit, ctx),
        Some(Command::Setup {
            provider,
            name,
            email,
        }) => setup(&config, parse_provider(&provider)?, &name, email, ctx),
        Some(Command::Run { account, args }) => run_account(&config, account, args, ctx),
    }
}

/// `remuda run`: a fast path that reads only `config.toml` and then execs claude (R6).
/// Without an account, the TUI's account picker chooses one first (R5, R16); that form takes
/// no other arguments, so something that looks like an option where the account goes is a
/// usage error (exit 2) unless it names a registered account.
fn run_account(
    config: &Path,
    account: Option<String>,
    args: Vec<String>,
    ctx: &Context,
) -> Result<ExitCode> {
    let account = match account {
        Some(reference) if reference.starts_with('-') => {
            match Registry::load(config)?.resolve(&reference) {
                Ok(account) => account,
                Err(_) => {
                    eprintln!(
                        "remuda: `remuda run` without an account takes no other arguments \
                         (got {reference:?}): the account picker launches plain claude. \
                         To pass arguments, name the account: remuda run <account> [args...]"
                    );
                    return Ok(ExitCode::from(2));
                }
            }
        }
        Some(reference) => Registry::load(config)?.resolve(&reference)?,
        None => {
            if !(ctx.stdin_is_tty && ctx.stdout_is_tty) {
                bail!(
                    "no account given, and choosing one needs a terminal. \
                     Use: remuda run <account> [args...]"
                );
            }
            match open_tui(config, ctx, tui::app::Mode::PickForRun)? {
                Some(account) => account,
                // Cancelled: nothing was launched.
                None => return Ok(ExitCode::FAILURE),
            }
        }
    };
    exec_as(config, &account, args, ctx)
}

/// Execs the account's agent exactly like `remuda run <account> [args...]` (R6, R17).
fn exec_as(config: &Path, account: &Account, args: Vec<String>, ctx: &Context) -> Result<ExitCode> {
    let program = program(ctx, account.provider)?;
    let plan = launch::prepare(
        account,
        args,
        ctx.cwd.as_deref(),
        // After a pick, time has passed since `ctx.now`.
        (ctx.clock)().to_string(),
        || uuid::Uuid::new_v4().to_string(),
    );
    for warning in launch::env_warnings(&ctx.env) {
        eprintln!("remuda: warning: {warning}");
    }
    // The ID is on disk before claude starts (R6); a failed log write never blocks the launch.
    let log = state_dir(config).join("launches.jsonl");
    if let Err(e) = launch::append_log(&log, &plan.record) {
        eprintln!(
            "remuda: warning: cannot write launch log {}: {e:#}",
            log.display()
        );
    }
    let err = launch::exec(&program, &plan.args, &plan.env);
    Err(anyhow::Error::new(err).context(format!("cannot run {}", program.display())))
}

/// `$REMUDA_HOME/state`, the sibling of `config.toml` (R3).
fn state_dir(config: &Path) -> PathBuf {
    config.with_file_name("state")
}

/// `remuda list`: identities are queried in parallel; failures degrade to the cached
/// identity or `unknown`, never to an error.
fn list(config: &Path, timeout: Duration, ctx: &Context) -> Result<ExitCode> {
    let accounts = Registry::load(config)?.all(&ctx.env);
    let claude = claude_program(ctx).ok();
    if claude.is_none() {
        eprintln!(
            "remuda: warning: `claude` not found on PATH; showing identities cached in .claude.json"
        );
    }
    let codex = program(ctx, Provider::Codex).ok();
    if codex.is_none() && accounts.iter().any(|a| a.provider == Provider::Codex) {
        eprintln!("remuda: warning: `codex` not found on PATH; codex identities are unknown");
    }
    let results = probe::parallel(&accounts, |account| {
        let program = match account.provider {
            Provider::Claude => claude.as_deref(),
            Provider::Codex => codex.as_deref(),
        };
        identity::identify(account, program, &ctx.env, timeout)
    });
    for warning in results.iter().filter_map(|(_, w)| w.as_ref()) {
        eprintln!("remuda: warning: {warning}");
    }
    let mut rows = vec![
        ["ACCOUNT", "EMAIL", "ORG", "PLAN", "HOME"]
            .map(String::from)
            .to_vec(),
    ];
    for (account, (identity, _)) in accounts.iter().zip(&results) {
        let dash = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".to_string());
        let (org, plan) = match identity {
            Identity::LoggedIn { org, plan, .. } => (dash(org), dash(plan)),
            Identity::NotLoggedIn | Identity::Unknown => ("-".into(), "-".into()),
        };
        rows.push(vec![
            account.qualified(),
            identity.who(),
            org,
            plan,
            account.home.to_string(),
        ]);
    }
    print!("{}", format_table(&rows));
    Ok(ExitCode::SUCCESS)
}

/// `remuda usage`: cached by default; `--live` queries every account in parallel and exits 1
/// if any query failed.
fn usage(
    config: &Path,
    account: Option<String>,
    live: bool,
    timeout: Duration,
    ctx: &Context,
) -> Result<ExitCode> {
    let registry = Registry::load(config)?;
    let accounts = match account {
        Some(reference) => vec![registry.resolve(&reference)?],
        None => registry.all(&ctx.env),
    };
    let reports: Vec<(String, bool)> = if live {
        // Only claude has usage to query (R4).
        let program = match accounts.iter().any(|a| a.provider.has_usage()) {
            true => Some(claude_program(ctx)?),
            false => None,
        };
        probe::parallel(&accounts, |account| {
            usage::live_report(account, program.as_deref(), &ctx.tz, timeout)
        })
    } else {
        accounts
            .iter()
            .map(|account| {
                (
                    usage::cached_report(account, &ctx.env, &ctx.tz, ctx.now),
                    true,
                )
            })
            .collect()
    };
    let text: Vec<&str> = reports.iter().map(|(text, _)| text.as_str()).collect();
    print!("{}", text.join("\n"));
    Ok(if reports.iter().all(|(_, ok)| *ok) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Titles wider than this (in display columns) are shortened in `remuda sessions`.
const SESSION_TITLE_WIDTH: usize = 60;
/// `remuda sessions` reports indexing progress only when it takes longer than this.
const QUIET_INDEXING: Duration = Duration::from_secs(1);
/// ... and then at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(200);

/// Indexing progress for `remuda sessions` on stderr: only when stderr is a terminal (piped
/// output never sees a `\r`-rewritten line), only once indexing takes longer than
/// [`QUIET_INDEXING`], at most every [`PROGRESS_EVERY`]; the line is cleared at the end.
struct IndexingProgress {
    tty: bool,
    quiet: Duration,
    every: Duration,
    started: std::time::Instant,
    reported: Option<std::time::Instant>,
}

/// Carriage return plus "erase line", so a shorter line leaves no residue.
const REWRITE_LINE: &str = "\r\x1b[2K";

impl IndexingProgress {
    fn new(tty: bool) -> Self {
        IndexingProgress {
            tty,
            quiet: QUIET_INDEXING,
            every: PROGRESS_EVERY,
            started: std::time::Instant::now(),
            reported: None,
        }
    }

    fn report(&mut self, done: usize, total: usize, out: &mut impl std::io::Write) {
        if !self.tty || done >= total {
            return;
        }
        let due = match self.reported {
            None => self.started.elapsed() >= self.quiet,
            Some(last) => last.elapsed() >= self.every,
        };
        if due {
            let _ = write!(
                out,
                "{REWRITE_LINE}remuda: indexing transcripts {done}/{total}"
            );
            let _ = out.flush();
            self.reported = Some(std::time::Instant::now());
        }
    }

    fn finish(&self, indexed: usize, out: &mut impl std::io::Write) {
        if self.reported.is_some() {
            let _ = writeln!(out, "{REWRITE_LINE}remuda: indexed {indexed} transcripts");
        }
    }
}

/// `remuda sessions`: one index refresh (cached in `state/index.json`), live sessions and
/// attribution, then the newest `limit` sessions (R5, R8, R9).
fn sessions(config: &Path, limit: usize, ctx: &Context) -> Result<ExitCode> {
    let accounts = Registry::load(config)?.all(&ctx.env);
    let state = state_dir(config);
    let cache = state.join("index.json");
    let mut index = Index::load(&cache);
    let stores = index::stores(&accounts, &ctx.env);
    let mut progress = IndexingProgress::new(ctx.stderr_is_tty);
    let mut stderr = std::io::stderr();
    index::refresh(&mut index, &stores, |p| {
        progress.report(p.done, p.total, &mut stderr)
    });
    progress.finish(index.entries.len(), &mut stderr);
    if let Err(e) = index.save(&cache) {
        eprintln!(
            "remuda: warning: cannot write index cache {}: {e:#}",
            cache.display()
        );
    }

    let path_var = ctx.env.get("PATH").map(String::as_str);
    let claude = claude_program(ctx).ok();
    let ps = launch::find_on_path("ps", path_var).ok();
    let live = live::collect(
        &accounts,
        claude.as_deref(),
        ps.as_deref(),
        &ctx.env,
        live::TIMEOUT,
    );
    let owners = attribution::collect(&accounts, &ctx.env, &state.join("launches.jsonl"), &live);

    let mut rows = vec![
        ["TIME", "ACCOUNTS", "TITLE", "CWD"]
            .map(String::from)
            .to_vec(),
    ];
    for entry in index.sorted().into_iter().take(limit) {
        let when = entry
            .last_activity()
            .or_else(|| Timestamp::from_nanosecond(entry.mtime_ns).ok())
            .map_or("-".to_string(), |t| {
                t.to_zoned(ctx.tz.clone())
                    .strftime("%Y-%m-%d %H:%M")
                    .to_string()
            });
        let accounts = attribution::accounts_of(entry, &stores, &owners);
        let dash = |v: Option<&str>| v.map_or("-".to_string(), str::to_string);
        rows.push(vec![
            when,
            if accounts.is_empty() {
                "-".to_string()
            } else {
                accounts.join(",")
            },
            dash(
                entry
                    .display_title()
                    .map(|t| {
                        text::truncate(&transcript::one_line(t, usize::MAX), SESSION_TITLE_WIDTH)
                    })
                    .as_deref(),
            ),
            dash(entry.cwd_last.as_deref()),
        ]);
    }
    print!("{}", format_table(&rows));
    Ok(ExitCode::SUCCESS)
}

/// `remuda setup`: every check (including finding the agent and its login arguments) happens
/// before the directory is created. A failed login keeps the registration; the exit code is
/// the agent's.
fn setup(
    config: &Path,
    provider: Provider,
    name: &str,
    email: Option<String>,
    ctx: &Context,
) -> Result<ExitCode> {
    let account = setup::plan(config, provider, name, &ctx.env)?;
    let program = program(ctx, provider)?;
    let args = provider.login_args(email).map_err(anyhow::Error::msg)?;
    let change = launch::env_change(&account);
    setup::create_and_register(config, &account)?;
    let login = setup::login_command(provider);
    eprintln!(
        "remuda: registered {} at {}; running `{login}`",
        account.qualified(),
        account.home
    );

    // Unreadable now, the registry cannot say whether the bare name is unique: qualify it.
    let ambiguous = Registry::load(config).map_or(true, |registry| {
        setup::ambiguous(provider, name, &registry.all(&ctx.env))
    });
    let retry = format!(
        "retry with: {}",
        setup::retry_command(provider, name, ambiguous)
    );
    match launch::run_foreground(&program, &args, &change, None) {
        Ok(status) if status.success() => Ok(ExitCode::SUCCESS),
        Ok(status) => {
            let code = status.code();
            let how = code.map_or("was killed by a signal".to_string(), |c| {
                format!("exited with status {c}")
            });
            eprintln!(
                "remuda: warning: `{login}` {how}; {} stays registered, {retry}",
                account.qualified()
            );
            Ok(ExitCode::from(
                code.and_then(|c| u8::try_from(c).ok()).unwrap_or(1),
            ))
        }
        Err(e) => {
            eprintln!(
                "remuda: warning: cannot run {}: {e}; {} stays registered, {retry}",
                program.display(),
                account.qualified()
            );
            Ok(ExitCode::FAILURE)
        }
    }
}

/// Bare `remuda`: the TUI.
fn tui(config: &Path, ctx: &Context) -> Result<ExitCode> {
    open_tui(config, ctx, tui::app::Mode::Browse)?;
    Ok(ExitCode::SUCCESS)
}

fn open_tui(config: &Path, ctx: &Context, mode: tui::app::Mode) -> Result<Option<Account>> {
    let accounts = Registry::load(config)?.all(&ctx.env);
    let path_var = ctx.env.get("PATH").map(String::as_str);
    tui::run(tui::Deps {
        seen: accounts.clone(),
        accounts,
        env: ctx.env.clone(),
        claude: claude_program(ctx).ok(),
        codex: program(ctx, Provider::Codex).ok(),
        ps: launch::find_on_path("ps", path_var).ok(),
        tz: ctx.tz.clone(),
        clock: ctx.clock,
        config: config.to_path_buf(),
        state_dir: state_dir(config),
        cwd: ctx.cwd.clone(),
        mode,
    })
}

fn parse_provider(name: &str) -> Result<Provider> {
    Provider::parse(name).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown provider {name:?} (known: {})",
            Provider::ALL.map(Provider::name).join(", ")
        )
    })
}

fn claude_program(ctx: &Context) -> Result<PathBuf> {
    program(ctx, Provider::Claude)
}

/// The provider's executable on `PATH`.
fn program(ctx: &Context, provider: Provider) -> Result<PathBuf> {
    launch::find_on_path(provider.program(), ctx.env.get("PATH").map(String::as_str))
}

/// Left-aligned columns separated by two spaces; the last column is not padded.
fn format_table(rows: &[Vec<String>]) -> String {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|i| {
            rows.iter()
                .filter_map(|r| r.get(i))
                .map(|c| text::width(c))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            if i + 1 < row.len() {
                line.push_str(&text::pad(cell, widths[i]));
                line.push_str("  ");
            } else {
                line.push_str(cell);
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// A positive, finite number of seconds.
fn parse_timeout(s: &str) -> std::result::Result<Duration, String> {
    let secs: f64 = s.parse().map_err(|_| format!("not a number: {s:?}"))?;
    if !secs.is_finite() || secs <= 0.0 {
        return Err(format!("must be a positive number of seconds: {s:?}"));
    }
    Duration::try_from_secs_f64(secs).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(rows: &[&[&str]]) -> String {
        let rows: Vec<Vec<String>> = rows
            .iter()
            .map(|r| r.iter().map(|c| c.to_string()).collect())
            .collect();
        format_table(&rows)
    }

    #[test]
    fn table_aligns_by_display_width() {
        let out = table(&[&["TITLE", "CWD"], &["日本語", "/a"], &["abc", "/b"]]);
        assert_eq!(out, "TITLE   CWD\n日本語  /a\nabc     /b\n");
    }

    fn progress(tty: bool) -> IndexingProgress {
        IndexingProgress {
            tty,
            quiet: Duration::ZERO,
            every: Duration::ZERO,
            started: std::time::Instant::now(),
            reported: None,
        }
    }

    #[test]
    fn progress_is_silent_when_stderr_is_not_a_terminal() {
        let mut p = progress(false);
        let mut out = Vec::new();
        p.report(0, 10, &mut out);
        p.report(5, 10, &mut out);
        p.finish(10, &mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "");
    }

    #[test]
    fn progress_on_a_terminal_rewrites_one_line_and_ends_it() {
        let mut p = progress(true);
        let mut out = Vec::new();
        p.report(0, 6071, &mut out);
        p.report(1234, 6071, &mut out);
        p.report(6071, 6071, &mut out);
        p.finish(6071, &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\r\x1b[2Kremuda: indexing transcripts 0/6071\
             \r\x1b[2Kremuda: indexing transcripts 1234/6071\
             \r\x1b[2Kremuda: indexed 6071 transcripts\n"
        );
    }

    #[test]
    fn progress_stays_quiet_for_a_fast_index() {
        let mut p = progress(true);
        p.quiet = Duration::from_secs(3600);
        let mut out = Vec::new();
        p.report(1, 2, &mut out);
        p.finish(2, &mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "");
    }
}
