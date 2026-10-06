//! Command-line surface (SPEC R5).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::identity::{self, Identity};
use crate::index::{self, Index, RefreshStats};
use crate::privacy::Aliases;
use crate::provider::Provider;
use crate::registry::{self, Account, Registry};
use crate::stats::{self, Period};
use crate::{Env, owned, paths};
use crate::{attribution, jev, launch, live, pick, probe, setup, text, transcript, tui, usage};

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
    },
    /// Recent sessions, newest first: time, accounts, title, cwd
    Sessions {
        /// How many sessions to show
        #[arg(long, value_name = "N", default_value = "30")]
        limit: usize,
    },
    /// Tokens per account and model, from the transcripts (no agent is run)
    Stats {
        /// Only the sections that include this account (`name` or `provider:name`)
        account: Option<String>,
        /// `today`, `7d`, `30d` or `all`; a period starts at local midnight
        #[arg(long, value_name = "PERIOD", default_value = "all", value_parser = parse_period)]
        period: Period,
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
    /// Recommend the account, model and effort to launch now, from usage and `[pick]` in
    /// config.toml; with TYPESAFE_API_KEY set and notes, TypeSafe's Jev chooses among the feasible
    /// options
    ///
    /// Without a key, `--offline`, or `[pick] notes`, nothing is sent and the rules decide. The
    /// request carries aliased usage and your notes as written: write accounts in the notes as
    /// `provider:name` to have them aliased too. `--print-request` shows it without sending.
    Pick {
        /// Only this provider's accounts: `claude` or `codex`
        #[arg(long)]
        provider: Option<String>,
        /// Query usage live first (`claude -p /usage`, `codex app-server`); a failed query falls back to the cache
        #[arg(long)]
        live: bool,
        /// Seconds to wait for each account's live query
        #[arg(long, value_name = "SECONDS", default_value = "90", value_parser = parse_timeout)]
        timeout: Duration,
        /// Never contact Jev: the rules decide
        #[arg(long)]
        offline: bool,
        /// Print the recommendation and every candidate as JSON
        #[arg(long, conflicts_with_all = ["run", "print_request"])]
        json: bool,
        /// Print the JSON body Jev would get (without the key) and send nothing
        #[arg(long, conflicts_with = "run")]
        print_request: bool,
        /// Launch the recommendation as `remuda run` does; arguments after `--` go to the agent
        #[arg(long)]
        run: bool,
        /// Arguments for the agent (with --run), after `--`
        #[arg(last = true, requires = "run", value_name = "ARGS")]
        args: Vec<String>,
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
    /// (`pick`, `usage --live`): what the agent says is read when it has answered. Other
    /// commands use `now`.
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
        Some(Command::Stats { account, period }) => stats(&config, account, period, ctx),
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
        Some(Command::Pick {
            provider,
            live,
            timeout,
            offline,
            json,
            print_request,
            run,
            args,
        }) => pick(
            &config,
            PickOptions {
                provider,
                live: live.then_some(timeout),
                offline,
                json,
                print_request,
                run,
                args,
            },
            ctx,
        ),
    }
}

struct PickOptions {
    provider: Option<String>,
    /// `--live`, with its timeout.
    live: Option<Duration>,
    offline: bool,
    json: bool,
    print_request: bool,
    run: bool,
    args: Vec<String>,
}

/// `remuda pick` (R23): the rules find what is feasible and rank it; Jev chooses among it when
/// there is a key, notes and a choice to make; the decision is printed, or launched with
/// `--run` exactly as `remuda run` would. Exits 1 when nothing is feasible.
fn pick(config: &Path, o: PickOptions, ctx: &Context) -> Result<ExitCode> {
    let registry = Registry::load(config)?;
    let settings = &registry.pick;
    let only = o.provider.as_deref().map(parse_provider).transpose()?;
    let accounts = registry.all(&ctx.env);
    let claude = claude_program(ctx).ok();
    let codex = program(ctx, Provider::Codex).ok();
    let sources = pick::Sources {
        env: &ctx.env,
        clock: ctx.clock,
        claude: claude.as_deref(),
        codex: codex.as_deref(),
        live: o.live,
        provider: only,
    };
    // Everything below reads the usage at `now`, the time it was all gathered: a live query
    // may have taken a while (R23).
    let (entries, now) = pick::gather(&accounts, settings, &sources);
    let candidates = pick::candidates(&entries, settings, now);
    let feasible = pick::ranked(&candidates);
    if feasible.is_empty() {
        if o.json {
            let report = pick::to_json(&entries, &candidates, None, settings);
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print!("{}", pick::format_not_feasible(&entries, &candidates));
        }
        eprintln!("remuda: nothing to recommend: no account and model is feasible");
        return Ok(ExitCode::FAILURE);
    }
    // Arguments that cannot start a new session are refused before anything is sent.
    if o.run {
        for c in &feasible {
            let provider = entries[candidates[*c].entry].account.provider;
            pick::run_args(provider, None, None, &o.args)?;
        }
    }
    let mut aliases = Aliases::default();
    for account in &accounts {
        aliases.note(&account.qualified());
    }
    let request = jev::request(&entries, &candidates, settings, &aliases, now, &ctx.tz);
    let key = ctx.env.get(jev::KEY_VAR).map(String::as_str);
    let skip = jev::skip_reason(o.offline, key, settings, &request);
    if o.print_request {
        println!("{}", request.body);
        if let Some(reason) = &skip {
            eprintln!(
                "remuda: note: this request would not be sent ({})",
                reason.name()
            );
        }
        return Ok(ExitCode::SUCCESS);
    }
    let asked = match skip {
        Some(reason) => pick::Asked::Skipped(reason),
        None => {
            let curl = launch::find_on_path("curl", ctx.env.get("PATH").map(String::as_str)).ok();
            jev::ask(curl.as_deref(), key.unwrap_or_default(), &request)
        }
    };
    let decision = pick::decide(&candidates, &entries, settings, asked)
        .expect("a feasible candidate was found above");
    if o.json {
        let report = pick::to_json(&entries, &candidates, Some(&decision), settings);
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(ExitCode::SUCCESS);
    }
    if !o.run {
        print!(
            "{}",
            pick::format_text(&entries, &candidates, &decision, settings, now)
        );
        return Ok(ExitCode::SUCCESS);
    }
    let chosen = &candidates[decision.chosen];
    let account = &entries[chosen.entry].account;
    let (args, notices) = pick::run_args(
        account.provider,
        chosen.model.as_deref(),
        decision.effort.as_deref(),
        &o.args,
    )?;
    eprintln!(
        "remuda: pick: {}",
        pick::summary(&entries, &candidates, &decision)
    );
    if let Some(hint) = pick::live_hint(&entries[chosen.entry], chosen) {
        eprintln!("remuda: pick: usage unknown: {hint}");
    }
    for notice in notices {
        eprintln!("remuda: {notice}");
    }
    exec_as(config, &registry, account, args, ctx)
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
                // The registry as it is now: the picker may have been open a while.
                Some(account) => (account, Registry::load(config)?),
                // Cancelled: nothing was launched.
                None => return Ok(ExitCode::FAILURE),
            }
        }
    };
    exec_as(config, &registry, &account, args, ctx)
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
    for warning in launch::env_warnings(&ctx.env) {
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
        // A missing claude fails the whole command; a missing codex, each codex account (R10).
        let claude = match accounts.iter().any(|a| a.provider == Provider::Claude) {
            true => Some(claude_program(ctx)?),
            false => None,
        };
        let codex = program(ctx, Provider::Codex).ok();
        probe::parallel(&accounts, |account| {
            let program = match account.provider {
                Provider::Claude => claude.as_deref(),
                Provider::Codex => codex.as_deref(),
            };
            usage::live_report(account, program, &ctx.tz, ctx.clock, timeout)
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
        claude.as_deref(),
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
/// period's table, only the sections of `account` if given (R5, R20). Runs no agent.
fn stats(
    config: &Path,
    account: Option<String>,
    period: Period,
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
    let report = stats::report(
        &cache,
        &sources,
        &attribution,
        &accounts,
        &registry.prices,
        ctx.now,
        &ctx.tz,
    );
    print!(
        "{}",
        stats::format(report.table(period), filter.as_deref(), &ctx.tz)
    );
    // On stdout, with the report: one that is piped must not pass for a complete one (R20).
    for line in unreadable_lines(&refreshed, "transcript", "counted as last read") {
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
}
