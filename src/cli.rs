//! Command-line surface (SPEC R5).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::account_command::OnPath;
use crate::identity::{self, Identity};
use crate::index::{self, Index, RefreshStats};
use crate::provider::Provider;
use crate::registry::{self, Account, Home, Registry};
use crate::stats::{self, By, Period};
use crate::usage::history;
use crate::{Env, owned, paths};
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
    /// Show usage limits: cached by the agent (claude: .claude.json; codex: its rollouts), or queried live
    Usage {
        /// Only this account (`name` or `provider:name`)
        account: Option<String>,
        /// Ask each agent now (`claude -p /usage`, `codex app-server`) instead of reading the cache
        #[arg(long)]
        live: bool,
        /// Seconds to wait for each account's live query (claude scans local session history: 2-20 s; codex: about 1-2 s)
        #[arg(long, value_name = "SECONDS", default_value = "90", value_parser = parse_timeout)]
        timeout: Duration,
        /// Show the recorded usage history (state/usage-history.jsonl) and each current window's pace instead
        #[arg(long, conflicts_with = "live")]
        history: bool,
        /// With --history: how many days back
        #[arg(long, value_name = "N", default_value = "7", requires = "history",
              value_parser = clap::value_parser!(u32).range(1..=3650))]
        days: u32,
    },
    /// Recent sessions, newest first: time, accounts, title, cwd
    Sessions {
        /// How many sessions to show
        #[arg(long, value_name = "N", default_value = "30")]
        limit: usize,
    },
    /// Tokens per account (or project) and model, from the transcripts (no agent is run)
    Stats {
        /// Only the sessions attributed to this account (`name` or `provider:name`)
        account: Option<String>,
        /// `today`, `7d`, `30d` or `all`; a period starts at local midnight
        #[arg(long, value_name = "PERIOD", default_value = "all", value_parser = parse_period)]
        period: Period,
        /// Sections by `account`, or by `project`: the directory each session started in
        #[arg(long, value_name = "SECTIONS", default_value = "account", value_parser = parse_by)]
        by: By,
        /// Print every request of the period as CSV instead, one line each (for reconciling
        /// with a bill); exit 1 when the report is incomplete
        #[arg(long, conflicts_with = "by")]
        csv: bool,
    },
    /// Create a new home under $REMUDA_HOME/homes/<provider>/<name>, register it and log in;
    /// with [share.claude], a claude home is first linked to the source's
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
    /// Unregister an account (remove it from config.toml); its home directory is left in place
    Remove {
        /// Account: `name` or `provider:name`
        account: String,
    },
    /// Launch claude as an account; all arguments after the account go to claude verbatim
    ///
    /// `-h/--help` is not handled here so that `remuda run <account> --help` reaches claude;
    /// use `remuda help run`. A `--` after the account goes to claude like any other argument.
    // What is launched is cut from the command line as typed (`run_arguments`): clap drops a
    // `--` right after the account. These fields check the arguments and describe them.
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
    /// The command line as typed, the program's name first: `run` cuts the agent's arguments
    /// from it (R5).
    pub args: Vec<OsString>,
    pub env: Env,
    pub cwd: Option<PathBuf>,
    pub now: Timestamp,
    /// The clock, for the TUI (which keeps running) and for a command that waits for an agent
    /// (`usage --live`): what the agent says is read when it has answered. Other commands use
    /// `now`.
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
            history,
            days,
        }) => match history {
            true => usage_history(&config, account, days, ctx),
            false => usage(&config, account, live, timeout, ctx),
        },
        Some(Command::Sessions { limit }) => sessions(&config, limit, ctx),
        Some(Command::Stats {
            account,
            period,
            by,
            csv,
        }) => stats(&config, account, period, (!csv).then_some(by), ctx),
        Some(Command::Setup {
            provider,
            name,
            email,
        }) => setup(&config, parse_provider(&provider)?, &name, email, ctx),
        Some(Command::Remove { account }) => remove(&config, &account),
        Some(Command::Run { account, args }) => {
            let typed = run_arguments(&ctx.args);
            // clap read `run` from this command line, so it is one: `run` comes first, since
            // remuda has no global options.
            debug_assert!(typed.is_some(), "`run` is not the first argument");
            let (account, args) = typed.unwrap_or((account, args));
            run_account(&config, account, args, ctx)
        }
    }
}

/// The account and the agent's arguments of `remuda run`, cut from the command line as typed
/// (R5): every token after the account is the agent's, a `--` right after the account
/// included, which clap takes for its own terminator and drops. Only a `--` before the account
/// is remuda's (`remuda run -- -x`: a registered account whose name starts with `-`). `None`
/// when `argv` is not a `run` command line in UTF-8, which clap does not accept as one either.
fn run_arguments(argv: &[OsString]) -> Option<(Option<String>, Vec<String>)> {
    let [_, command, rest @ ..] = argv else {
        return None;
    };
    if command != "run" {
        return None;
    }
    let rest: Vec<String> = rest
        .iter()
        .map(|a| a.to_str().map(str::to_string))
        .collect::<Option<_>>()?;
    let mut rest = rest.into_iter().peekable();
    rest.next_if(|a| a == "--");
    let account = rest.next();
    Some((account, rest.collect()))
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
    let (account, registry) = match account {
        Some(reference) if reference.starts_with('-') => {
            let registry = Registry::load(config)?;
            match registry.resolve(&reference) {
                Ok(account) => (account, registry),
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
        Some(reference) => {
            let registry = Registry::load(config)?;
            (registry.resolve(&reference)?, registry)
        }
        None => {
            if !(ctx.stdin_is_tty && ctx.stdout_is_tty) {
                bail!(
                    "no account given, and choosing one needs a terminal. \
                     Use: remuda run <account> [args...]"
                );
            }
            match open_tui(config, ctx, tui::app::Mode::PickForRun)? {
                Some(account) => {
                    let registry = registry_for_picked(config, &account)?;
                    (account, registry)
                }
                // Cancelled: nothing was launched.
                None => return Ok(ExitCode::FAILURE),
            }
        }
    };
    exec_as(config, &registry, &account, args, ctx)
}

/// The registry as it is now, for launching the account chosen in the picker (R16): the picker
/// may have been open a while, and another `remuda` may have taken the account out meanwhile.
/// Then it is not launched; nor is one of its name registered again with another home, which
/// is another account (R2). A provider's `default` is implicit (R1): no registry holds it, so
/// none can lose it. The account is not named: private mode may have been on in the picker
/// (R21), and it is the one just chosen.
fn registry_for_picked(config: &Path, account: &Account) -> Result<Registry> {
    let registry = Registry::load(config)?;
    if account.home == Home::Default || registry.accounts.contains(account) {
        return Ok(registry);
    }
    let again = registry
        .accounts
        .iter()
        .any(|a| a.provider == account.provider && a.name == account.name);
    if again {
        bail!("the account chosen is now registered with another home; nothing was launched");
    }
    bail!("the account chosen is no longer registered; nothing was launched");
}

/// Execs the account's agent exactly like `remuda run <account> [args...]` (R6, R17), with the
/// shared configuration of `registry` (R18).
fn exec_as(
    config: &Path,
    registry: &Registry,
    account: &Account,
    args: Vec<String>,
    ctx: &Context,
) -> Result<ExitCode> {
    let program = program(ctx, account.provider)?;
    let plan = launch::plan(
        account,
        args,
        ctx.cwd.as_deref(),
        // After a pick, time has passed since `ctx.now`.
        (ctx.clock)().to_string(),
        || uuid::Uuid::new_v4().to_string(),
        &registry.sharing,
        &ctx.env,
        config,
    )?;
    exec_plan(config, &program, &plan, ctx)
}

/// Warnings and notices, the launch log, then exec (R6): the ID is on disk before claude
/// starts; a failed log write never blocks the launch.
fn exec_plan(
    config: &Path,
    program: &Path,
    plan: &launch::Launch,
    ctx: &Context,
) -> Result<ExitCode> {
    for warning in launch::env_warnings(&ctx.env, plan) {
        eprintln!("remuda: warning: {warning}");
    }
    for notice in &plan.notices {
        eprintln!("remuda: {notice}");
    }
    let log = owned::launch_log(&owned::state_dir(config));
    if let Err(e) = launch::append_log(&log, &plan.record) {
        eprintln!(
            "remuda: warning: cannot write launch log {}: {e:#}",
            log.display()
        );
    }
    let err = launch::exec(program, plan);
    Err(anyhow::Error::new(err).context(format!("cannot run {}", program.display())))
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
    let agents = OnPath {
        claude: claude.as_deref(),
        codex: codex.as_deref(),
    };
    let results = probe::parallel(&accounts, |account| {
        identity::identify(account, &agents, &ctx.env, timeout)
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
    let mut points = Vec::new();
    let reports: Vec<(String, bool)> = if live {
        // A missing claude fails the whole command; a missing codex, each codex account (R10).
        let claude = match accounts.iter().any(|a| a.provider == Provider::Claude) {
            true => Some(claude_program(ctx)?),
            false => None,
        };
        let codex = program(ctx, Provider::Codex).ok();
        let agents = OnPath {
            claude: claude.as_deref(),
            codex: codex.as_deref(),
        };
        let answers = probe::parallel(&accounts, |account| {
            let (text, ok, reading) =
                usage::live_attempt(account, &agents, &ctx.tz, ctx.clock, timeout);
            // Only usage that was told: not a failure, nor an answer without it (R24).
            (text, ok, recorded(account, reading.as_ref()))
        });
        answers
            .into_iter()
            .map(|(text, ok, mut more)| {
                points.append(&mut more);
                (text, ok)
            })
            .collect()
    } else {
        let (reports, mut more) = cached_reports(&accounts, ctx);
        points.append(&mut more);
        reports
    };
    let text: Vec<&str> = reports.iter().map(|(text, _)| text.as_str()).collect();
    print!("{}", text.join("\n"));
    record(config, points, ctx);
    Ok(if reports.iter().all(|(_, ok)| *ok) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// `remuda usage`'s reports of cached usage, and the points they record (R10, R24). Each
/// account's cache is read, then read at the time after that (`ctx.clock`), not when the
/// command started: a window whose reset fell in between has reset since, and neither its old
/// percentage nor a note about a reset already passed is printed or recorded.
fn cached_reports(
    accounts: &[Account],
    ctx: &Context,
) -> (Vec<(String, bool)>, Vec<history::Point>) {
    let mut points = Vec::new();
    let reports = accounts
        .iter()
        .map(|account| {
            let (text, reading) = usage::cached_attempt(account, &ctx.env, &ctx.tz, ctx.clock);
            points.extend(recorded(account, reading.as_ref()));
            (text, true)
        })
        .collect();
    (reports, points)
}

/// The points of `account`'s `reading` for the usage history (R24); none without one.
fn recorded(account: &Account, reading: Option<&usage::Reading>) -> Vec<history::Point> {
    reading.map_or_else(Vec::new, |r| history::points(&account.qualified(), r))
}

/// Records `points` in the usage history, compacting it, as `remuda usage` does (R24): once
/// what they come from has been printed, since the lock may be waited for. The history is a
/// by-product: what keeps it from being written is not told.
fn record(config: &Path, points: Vec<history::Point>, ctx: &Context) {
    let state = owned::state_dir(config);
    let _ = history::record(&state, points, (ctx.clock)(), true, &owned::Flock);
}

/// `remuda usage --history` (R24): what the history records for each account, or for one,
/// over the last `days` days. Records nothing itself.
fn usage_history(
    config: &Path,
    account: Option<String>,
    days: u32,
    ctx: &Context,
) -> Result<ExitCode> {
    let registry = Registry::load(config)?;
    let accounts = match account {
        Some(reference) => vec![registry.resolve(&reference)?],
        None => registry.all(&ctx.env),
    };
    let path = owned::usage_history(&owned::state_dir(config));
    let points = match history::load(&path) {
        Ok(Some(points)) => points,
        Ok(None) => {
            println!("no usage history yet ({})", path.display());
            return Ok(ExitCode::SUCCESS);
        }
        Err(e) => bail!("cannot read {}: {e:#}", path.display()),
    };
    let names: Vec<String> = accounts.iter().map(Account::qualified).collect();
    print!(
        "{}",
        history::report(&points, &names, days, ctx.now, &ctx.tz)
    );
    Ok(ExitCode::SUCCESS)
}

/// Titles wider than this (in display columns) are shortened in `remuda sessions`.
const SESSION_TITLE_WIDTH: usize = 60;
/// `remuda sessions` reports indexing progress only when it takes longer than this.
const QUIET_INDEXING: Duration = Duration::from_secs(1);
/// ... and then at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(200);

/// Progress reading transcripts (`remuda sessions`, `remuda stats`) on stderr: only when
/// stderr is a terminal (piped output never sees a `\r`-rewritten line), only once reading
/// takes longer than [`QUIET_INDEXING`], at most every [`PROGRESS_EVERY`]; the line is cleared
/// at the end.
struct IndexingProgress {
    tty: bool,
    /// `indexing transcripts`: what is being done, before `done/total`.
    doing: &'static str,
    /// `indexed`: what was done, before `N transcripts`.
    done: &'static str,
    quiet: Duration,
    every: Duration,
    started: std::time::Instant,
    reported: Option<std::time::Instant>,
}

/// Carriage return plus "erase line", so a shorter line leaves no residue.
const REWRITE_LINE: &str = "\r\x1b[2K";

impl IndexingProgress {
    fn new(tty: bool, doing: &'static str, done: &'static str) -> Self {
        IndexingProgress {
            tty,
            doing,
            done,
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
            let _ = write!(out, "{REWRITE_LINE}remuda: {} {done}/{total}", self.doing);
            let _ = out.flush();
            self.reported = Some(std::time::Instant::now());
        }
    }

    fn finish(&self, files: usize, out: &mut impl std::io::Write) {
        if self.reported.is_some() {
            let _ = writeln!(
                out,
                "{REWRITE_LINE}remuda: {} {files} transcripts",
                self.done
            );
        }
    }
}

/// More directories that could not be read than this are named up to it, the rest counted.
const UNREADABLE_NAMED: usize = 5;

/// What to say about the directories `refreshed` could not read (R8, R20), a line each: the
/// directory, the error, and how many cached files (`session`, `transcript`) below it are
/// still `shown` as they were. Beyond [`UNREADABLE_NAMED`] directories, one line for the rest.
fn unreadable_lines(refreshed: &RefreshStats, file: &str, shown: &str) -> Vec<String> {
    let kept = |n: usize, below: &str| match n {
        0 => String::new(),
        1 => format!("; 1 {file} {below} is {shown}"),
        n => format!("; {n} {file}s {below} are {shown}"),
    };
    let (named, rest) = match refreshed.unreadable.len() {
        n if n <= UNREADABLE_NAMED + 1 => (&refreshed.unreadable[..], &[][..]),
        _ => refreshed.unreadable.split_at(UNREADABLE_NAMED),
    };
    let mut lines: Vec<String> = named
        .iter()
        .map(|u| format!("{u}{}", kept(u.kept, "below it")))
        .collect();
    if !rest.is_empty() {
        lines.push(format!(
            "cannot read {} more directories{}",
            rest.len(),
            kept(rest.iter().map(|u| u.kept).sum(), "below them")
        ));
    }
    lines
}

/// `remuda sessions`: one index refresh (cached in `state/index.json`), live sessions and
/// attribution, then the newest `limit` sessions (R5, R8, R9).
fn sessions(config: &Path, limit: usize, ctx: &Context) -> Result<ExitCode> {
    let accounts = Registry::load(config)?.all(&ctx.env);
    let state = owned::state_dir(config);
    let cache = state.join("index.json");
    let mut index = Index::load(&cache);
    let (stores, given) = index::resolve(&accounts, &ctx.env);
    let mut progress = IndexingProgress::new(ctx.stderr_is_tty, "indexing transcripts", "indexed");
    let mut stderr = std::io::stderr();
    let refreshed = index::refresh_with(&mut index, &stores, &given, |p| {
        progress.report(p.done, p.total, &mut stderr)
    });
    progress.finish(index.entries.len(), &mut stderr);
    for line in unreadable_lines(&refreshed, "session", "listed as last indexed") {
        eprintln!("remuda: warning: {line}");
    }
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
        &OnPath {
            claude: claude.as_deref(),
            codex: None,
        },
        ps.as_deref(),
        &ctx.env,
        live::TIMEOUT,
    );
    let owners = attribution::collect(&accounts, &ctx.env, &owned::launch_log(&state), &live);

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

/// `remuda stats`: the statistics cache (`state/stats.json`) brought up to date and saved when
/// it changed (a failed save is a warning), attribution without live sessions, then one
/// period's table with sections `by` accounts or projects, only of `account` if given, or with
/// `by` `None`, its requests as CSV (R5, R20). Runs no agent.
fn stats(
    config: &Path,
    account: Option<String>,
    period: Period,
    by: Option<By>,
    ctx: &Context,
) -> Result<ExitCode> {
    let registry = Registry::load(config)?;
    let accounts = registry.all(&ctx.env);
    let filter = match account {
        Some(reference) => Some(registry.resolve(&reference)?.qualified()),
        None => None,
    };
    let state = owned::state_dir(config);
    let path = state.join("stats.json");
    let mut cache = stats::Cache::load(&path);
    let (sources, given) = stats::resolve(&accounts, &ctx.env);
    let mut progress = IndexingProgress::new(ctx.stderr_is_tty, "reading transcripts", "read");
    let mut stderr = std::io::stderr();
    let refreshed = stats::refresh_with(&mut cache, &sources, &given, |done, total| {
        progress.report(done, total, &mut stderr)
    });
    progress.finish(cache.files.len(), &mut stderr);
    if let Err(e) = cache.save_if_changed(&path, &refreshed) {
        eprintln!(
            "remuda: warning: cannot write statistics cache {}: {e:#}",
            path.display()
        );
    }
    let attribution = attribution::collect(&accounts, &ctx.env, &owned::launch_log(&state), &[]);
    let incomplete = unreadable_lines(&refreshed, "transcript", "counted as last read");
    let filter = filter.as_deref();
    let Some(by) = by else {
        let counted = stats::requests(&cache, &sources, &attribution, &accounts, &registry.prices);
        let mut out = std::io::BufWriter::new(std::io::stdout().lock());
        stats::csv(&mut out, &counted, period, ctx.now, &ctx.tz, filter)?;
        // A line of prose would break the CSV: the exit status says it is incomplete (R20).
        for line in &incomplete {
            eprintln!("Incomplete: {line}");
        }
        return Ok(match incomplete.is_empty() {
            true => ExitCode::SUCCESS,
            false => ExitCode::FAILURE,
        });
    };
    let text = match by {
        By::Account => {
            let report = stats::report(
                &cache,
                &sources,
                &attribution,
                &accounts,
                &registry.prices,
                ctx.now,
                &ctx.tz,
            );
            stats::format(report.table(period), filter, &ctx.tz)
        }
        By::Project => {
            let counted =
                stats::requests(&cache, &sources, &attribution, &accounts, &registry.prices);
            let table = stats::projects(&counted, period, ctx.now, &ctx.tz, filter);
            stats::format_projects(&table, &ctx.tz)
        }
    };
    print!("{text}");
    // On stdout, with the report: one that is piped must not pass for a complete one (R20).
    for line in incomplete {
        println!("Incomplete: {line}");
    }
    Ok(ExitCode::SUCCESS)
}

/// `remuda setup`: every check (including finding the agent and its login arguments) happens
/// before the directory is created. A claude home of a `[share.claude]` member is linked to
/// the source's before the login (R18). A failed login keeps the registration; the exit code
/// is the agent's.
fn setup(
    config: &Path,
    provider: Provider,
    name: &str,
    email: Option<String>,
    ctx: &Context,
) -> Result<ExitCode> {
    let plan = setup::plan(config, provider, name, &ctx.env)?;
    let account = &plan.account;
    let program = program(ctx, provider)?;
    let args = provider.login_args(email).map_err(anyhow::Error::msg)?;
    let change = launch::env_change(account);
    let linked = setup::create_and_register(config, &plan)?;
    for note in setup::link_notes(&plan, linked.as_ref(), true) {
        let warning = if note.warning { "warning: " } else { "" };
        eprintln!("remuda: {warning}{}", note.text);
    }
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

/// `remuda remove <account>` (R14a): unregisters the account and says how to register its
/// home, which is left in place, again. Stdout stays empty.
fn remove(config: &Path, reference: &str) -> Result<ExitCode> {
    let account = Registry::load(config)?.resolve(reference)?;
    registry::unregister(config, &account)?;
    let provider_flag = match account.provider {
        Provider::Claude => "",
        Provider::Codex => "--provider codex ",
    };
    eprintln!(
        "remuda: removed {}; its home {home} was left in place (to register it again: remuda \
         add {provider_flag}{} {home})",
        account.qualified(),
        account.name,
        home = account.home,
    );
    Ok(ExitCode::SUCCESS)
}

/// Bare `remuda`: the TUI.
fn tui(config: &Path, ctx: &Context) -> Result<ExitCode> {
    open_tui(config, ctx, tui::app::Mode::Browse)?;
    Ok(ExitCode::SUCCESS)
}

fn open_tui(config: &Path, ctx: &Context, mode: tui::app::Mode) -> Result<Option<Account>> {
    let listing = tui::accounts::Listing::open(config.to_path_buf(), ctx.env.clone())?;
    let path_var = ctx.env.get("PATH").map(String::as_str);
    tui::run(tui::Deps {
        listing: Arc::new(listing),
        env: ctx.env.clone(),
        claude: claude_program(ctx).ok(),
        codex: program(ctx, Provider::Codex).ok(),
        ps: launch::find_on_path("ps", path_var).ok(),
        tz: ctx.tz.clone(),
        clock: ctx.clock,
        state_dir: owned::state_dir(config),
        cwd: ctx.cwd.clone(),
        mode,
        private: false,
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

/// `today`, `7d`, `30d` or `all`.
fn parse_period(s: &str) -> std::result::Result<Period, String> {
    Period::parse(s).ok_or_else(|| format!("expected today, 7d, 30d or all, got {s:?}"))
}

fn parse_by(s: &str) -> std::result::Result<By, String> {
    By::parse(s).ok_or_else(|| format!("expected account or project, got {s:?}"))
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

    /// R10, R24 (PR review): cached usage is read at the time it was read, not when the command
    /// started: a window whose reset falls between the two has reset since. Its old percentage
    /// is neither printed nor recorded, and no note of a reset already passed is printed.
    #[test]
    fn cached_usage_is_read_when_it_was_read_not_when_remuda_started() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("max");
        std::fs::create_dir(&home).unwrap();
        let fetched: Timestamp = "2026-10-08T10:00:00Z".parse().unwrap();
        std::fs::write(
            home.join(".claude.json"),
            format!(
                r#"{{"cachedUsageUtilization": {{"fetchedAtMs": {}, "utilization": {{"limits": [
                  {{"kind": "session", "percent": 34, "resets_at": "2026-10-08T11:00:00Z"}},
                  {{"kind": "weekly_all", "percent": 77, "resets_at": "2099-01-01T00:00:00Z"}}]}}}}}}"#,
                fetched.as_millisecond()
            ),
        )
        .unwrap();
        let max = Account {
            provider: Provider::Claude,
            name: "max".into(),
            home: Home::Path(home.display().to_string()),
        };
        fn after_the_reset() -> Timestamp {
            "2026-10-08T11:00:30Z".parse().unwrap()
        }
        let ctx = Context {
            args: Vec::new(),
            env: [("HOME".to_string(), dir.path().display().to_string())].into(),
            cwd: None,
            // The command started half an hour before the session's reset; the cache is read
            // after it.
            now: "2026-10-08T10:30:00Z".parse().unwrap(),
            clock: after_the_reset,
            tz: TimeZone::UTC,
            stdin_is_tty: false,
            stdout_is_tty: false,
            stderr_is_tty: false,
        };
        let (reports, points) = cached_reports(std::slice::from_ref(&max), &ctx);
        let [(text, true)] = reports.as_slice() else {
            panic!("{reports:?}")
        };
        assert!(text.contains("reset since cached (Oct 8 11:00)"), "{text}");
        assert!(!text.contains("34%") && !text.contains("note:"), "{text}");
        let labels: Vec<&str> = points.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels, ["Week (all models)"]);
        assert_eq!(points[0].ts, fetched);
    }

    /// R16: the account chosen in the picker of `remuda run` is launched only if the registry
    /// as it is after the TUI still has it; the registry returned is that one (R18).
    #[test]
    fn a_picked_account_must_still_be_registered() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let named = |provider, name: &str, home: &str| Account {
            provider,
            name: name.into(),
            home: Home::Path(home.into()),
        };
        let (max, team) = (
            named(Provider::Claude, "max", "/p/max"),
            named(Provider::Claude, "team", "/p/team"),
        );
        let work = named(Provider::Codex, "work", "/c/work");
        let entry = |a: &Account| {
            format!(
                "[[account]]\nprovider = \"{}\"\nname = \"{}\"\nhome = \"{}\"\n",
                a.provider, a.name, a.home
            )
        };
        let refused = |account: &Account| {
            let error = registry_for_picked(&config, account).unwrap_err();
            format!("{error:#}")
        };
        const GONE: &str = "the account chosen is no longer registered; nothing was launched";

        // As when the picker opened: every account goes, with the registry's sharing.
        let all = entry(&max) + &entry(&team) + &entry(&work);
        std::fs::write(&config, all + "[share.claude]\nfrom = \"max\"\n").unwrap();
        for account in [&max, &team, &work] {
            let registry = registry_for_picked(&config, account).unwrap();
            assert_eq!(registry.sharing.source, Some(max.clone()));
        }

        // `remuda remove team` and `remuda remove codex:work` in another terminal meanwhile.
        std::fs::write(&config, entry(&max)).unwrap();
        assert!(registry_for_picked(&config, &max).is_ok());
        assert_eq!(refused(&team), GONE);
        assert_eq!(refused(&work), GONE);
        // The same name for the other provider is not that account.
        assert_eq!(refused(&named(Provider::Codex, "max", "/p/max")), GONE);

        // Registered again with another home, be it one `/` more: another account (R2).
        std::fs::write(&config, entry(&named(Provider::Claude, "max", "/p/max/"))).unwrap();
        assert_eq!(
            refused(&max),
            "the account chosen is now registered with another home; nothing was launched"
        );

        // A provider's `default` is implicit: no registry holds it, none loses it, whether or
        // not codex's is listed.
        for registry in ["", &entry(&max)] {
            std::fs::write(&config, registry).unwrap();
            for provider in Provider::ALL {
                assert!(registry_for_picked(&config, &Account::default_for(provider)).is_ok());
            }
        }
        std::fs::remove_file(&config).unwrap();
        assert!(registry_for_picked(&config, &Account::default_for(Provider::Claude)).is_ok());
        assert_eq!(refused(&max), GONE);

        // A registry that cannot be read launches nothing, `default` included.
        std::fs::write(&config, "[[account]]\nprovider = 7\n").unwrap();
        for account in [&max, &Account::default_for(Provider::Claude)] {
            assert!(refused(account).contains("config.toml"));
        }
    }

    fn table(rows: &[&[&str]]) -> String {
        let rows: Vec<Vec<String>> = rows
            .iter()
            .map(|r| r.iter().map(|c| c.to_string()).collect())
            .collect();
        format_table(&rows)
    }

    /// R8, R20: each directory that could not be read is named with what is kept below it;
    /// past a handful, the rest are counted.
    #[test]
    fn unreadable_directories_are_named_up_to_a_handful() {
        let unreadable = |i: usize, kept: usize, unresolved: bool| index::Unreadable {
            path: format!("/s/p{i}").into(),
            error: "denied".into(),
            kept,
            unresolved,
        };
        let refreshed = |kept: &[usize]| RefreshStats {
            unreadable: kept
                .iter()
                .enumerate()
                .map(|(i, &kept)| unreadable(i, kept, false))
                .collect(),
            ..RefreshStats::default()
        };
        // A store that could not be resolved is named the same way.
        let unresolved = RefreshStats {
            unreadable: vec![unreadable(0, 2, true), unreadable(1, 0, true)],
            ..RefreshStats::default()
        };
        assert_eq!(
            unreadable_lines(&unresolved, "session", "listed"),
            [
                "cannot read /s/p0: denied; 2 sessions below it are listed",
                "cannot read /s/p1: denied",
            ]
        );
        let lines = |kept: &[usize]| unreadable_lines(&refreshed(kept), "session", "listed");
        assert!(lines(&[]).is_empty());
        assert_eq!(
            lines(&[0, 1, 2]),
            [
                "cannot read /s/p0: denied",
                "cannot read /s/p1: denied; 1 session below it is listed",
                "cannot read /s/p2: denied; 2 sessions below it are listed",
            ]
        );
        assert_eq!(
            lines(&[0; UNREADABLE_NAMED + 1]).len(),
            UNREADABLE_NAMED + 1
        );
        let many = lines(&[1; UNREADABLE_NAMED + 3]);
        assert_eq!(many.len(), UNREADABLE_NAMED + 1);
        assert_eq!(
            many[UNREADABLE_NAMED],
            "cannot read 3 more directories; 3 sessions below them are listed"
        );
        assert_eq!(
            lines(&[0; UNREADABLE_NAMED + 2])[UNREADABLE_NAMED],
            "cannot read 2 more directories"
        );
    }

    #[test]
    fn table_aligns_by_display_width() {
        let out = table(&[&["TITLE", "CWD"], &["日本語", "/a"], &["abc", "/b"]]);
        assert_eq!(out, "TITLE   CWD\n日本語  /a\nabc     /b\n");
    }

    /// R5: everything after the account is the agent's; only a `--` before it is dropped.
    #[test]
    fn run_arguments_are_cut_after_the_account() {
        let cut = |argv: &[&str]| {
            let argv: Vec<OsString> = argv.iter().map(OsString::from).collect();
            run_arguments(&argv)
        };
        let some = |account: Option<&str>, args: &[&str]| {
            Some((
                account.map(str::to_string),
                args.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            ))
        };
        assert_eq!(cut(&["remuda", "run"]), some(None, &[]));
        assert_eq!(cut(&["remuda", "run", "--"]), some(None, &[]));
        assert_eq!(cut(&["remuda", "run", "work"]), some(Some("work"), &[]));
        assert_eq!(
            cut(&["remuda", "run", "work", "--", "--resume", "abc"]),
            some(Some("work"), &["--", "--resume", "abc"])
        );
        assert_eq!(
            cut(&["remuda", "run", "work", "-p", "", "--", "x", "--"]),
            some(Some("work"), &["-p", "", "--", "x", "--"])
        );
        assert_eq!(
            cut(&["remuda", "run", "--", "-x", "--", "-y"]),
            some(Some("-x"), &["--", "-y"])
        );
        assert_eq!(
            cut(&["remuda", "run", "--resume", "abc"]),
            some(Some("--resume"), &["abc"])
        );
        assert_eq!(cut(&["remuda", "run", "--", "--"]), some(Some("--"), &[]));
        // Not a `run` command line, or not UTF-8: clap's reading stands.
        assert_eq!(cut(&["remuda", "list"]), None);
        assert_eq!(cut(&["remuda"]), None);
        assert_eq!(cut(&[]), None);
        use std::os::unix::ffi::OsStringExt;
        let argv = ["remuda", "run", "work"].map(OsString::from);
        let argv = [&argv[..], &[OsString::from_vec(vec![0xff])]].concat();
        assert_eq!(run_arguments(&argv), None);
    }

    fn progress(tty: bool) -> IndexingProgress {
        IndexingProgress {
            tty,
            doing: "indexing transcripts",
            done: "indexed",
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

    #[test]
    fn progress_names_what_it_reads() {
        let mut p = progress(true);
        (p.doing, p.done) = ("reading transcripts", "read");
        let mut out = Vec::new();
        p.report(3, 20, &mut out);
        p.finish(20, &mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\r\x1b[2Kremuda: reading transcripts 3/20\r\x1b[2Kremuda: read 20 transcripts\n"
        );
    }

    /// R5 `--timeout`: a positive, finite number of seconds; not 0.
    #[test]
    fn timeout_takes_a_positive_number_of_seconds() {
        assert_eq!(parse_timeout("1.5"), Ok(Duration::from_millis(1500)));
        assert_eq!(parse_timeout("90"), Ok(Duration::from_secs(90)));
        for bad in ["0", "-1", "inf", "NaN", "soon", ""] {
            assert!(parse_timeout(bad).is_err(), "{bad:?}");
        }
    }
}
