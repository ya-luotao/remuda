//! Background work for [`Effect`]s: each runs on its own thread and reports back as
//! [`Event`]s on the event loop's channel. A send error means the TUI has quit; the result
//! is dropped.

use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use crate::index::{self, Index};
use crate::provider::{Provider, codex};
use crate::registry::{self, Account, Registry};
use crate::{attribution, checks, identity, live, stats, transcript, usage};

use super::Deps;
use super::app::{self, Effect, Event, LaunchRequest, PREVIEW_MESSAGES};

/// `claude auth status` / `codex login status` per account; the same default as `remuda list`.
const IDENTITY_TIMEOUT: Duration = Duration::from_secs(15);
/// `claude -p /usage` / `codex app-server` per account; the same default as
/// `remuda usage --live` (R10).
const LIVE_USAGE_TIMEOUT: Duration = Duration::from_secs(90);
/// `claude stop|rm <id>`.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
/// Index and statistics progress is sent at most this often (index entries are batched in
/// between).
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// Starts `effect` in the background. Leaving the TUI ([`Effect::Quit`], [`Effect::Pick`])
/// and the foreground runs ([`Effect::Launch`], [`Effect::Relay`], [`Effect::Setup`]) are the
/// event loop's own business.
pub fn spawn(effect: Effect, deps: &Arc<Deps>, tx: &Sender<Event>) {
    let deps = Arc::clone(deps);
    let tx = tx.clone();
    match effect {
        Effect::Quit
        | Effect::Pick(_)
        | Effect::Launch(_)
        | Effect::Relay { .. }
        | Effect::Setup { .. } => {}
        Effect::RefreshIndex => {
            thread::spawn(move || refresh_index(&deps, &tx));
        }
        Effect::Identities => {
            for account in &deps.accounts {
                let (deps, tx, account) = (Arc::clone(&deps), tx.clone(), account.clone());
                thread::spawn(move || {
                    let (identity, _warning) = identity::identify(
                        &account,
                        deps.program(account.provider),
                        &deps.env,
                        IDENTITY_TIMEOUT,
                    );
                    let _ = tx.send(Event::Identity { account, identity });
                });
            }
        }
        Effect::CachedUsage => {
            thread::spawn(move || {
                for account in &deps.accounts {
                    let result = usage::cached_usage(account, &deps.env);
                    let _ = tx.send(Event::CachedUsage {
                        account: account.clone(),
                        result,
                    });
                }
            });
        }
        Effect::LiveUsage(which) => {
            for account in which {
                let (deps, tx) = (Arc::clone(&deps), tx.clone());
                thread::spawn(move || {
                    let result = match deps.program(account.provider) {
                        Some(program) => usage::live_usage(&account, program, LIVE_USAGE_TIMEOUT),
                        None => Err(format!(
                            "`{}` not found on PATH",
                            account.provider.program()
                        )),
                    };
                    let _ = tx.send(Event::LiveUsage { account, result });
                });
            }
        }
        Effect::Live => {
            thread::spawn(move || {
                let sessions = live::collect(
                    &deps.accounts,
                    deps.claude.as_deref(),
                    deps.ps.as_deref(),
                    &deps.env,
                    live::TIMEOUT,
                );
                let _ = tx.send(Event::Live(sessions));
            });
        }
        Effect::Attribution => {
            thread::spawn(move || {
                let log = deps.state_dir.join("launches.jsonl");
                let base = attribution::collect(&deps.accounts, &deps.env, &log, &[]);
                let _ = tx.send(Event::Attribution(base));
            });
        }
        Effect::Stats => {
            thread::spawn(move || compute_stats(&deps, &tx));
        }
        Effect::Checks => {
            thread::spawn(move || {
                let stores = index::stores(&deps.accounts, &deps.env);
                let mut found = checks::run(&deps.accounts, &deps.env, &stores);
                // A registry that cannot be read shares nothing (the launch says why).
                if let Ok(registry) = Registry::load(&deps.config) {
                    found.extend(checks::sharing(
                        &deps.accounts,
                        &deps.env,
                        &registry.sharing,
                    ));
                }
                let _ = tx.send(Event::Checks(found));
            });
        }
        Effect::Logs { account, short_id } => {
            thread::spawn(move || {
                let result = match &deps.claude {
                    Some(claude) => live::logs(claude, &account, &short_id, live::LOGS_TIMEOUT),
                    None => Err("`claude` not found on PATH".to_string()),
                };
                let _ = tx.send(Event::Logs { short_id, result });
            });
        }
        Effect::Control {
            account,
            verb,
            short_id,
        } => {
            thread::spawn(move || {
                let result = match &deps.claude {
                    Some(claude) => {
                        live::control(claude, &account, verb, &short_id, CONTROL_TIMEOUT)
                    }
                    None => Err("`claude` not found on PATH".to_string()),
                };
                let _ = tx.send(Event::ControlDone {
                    verb,
                    short_id,
                    result,
                });
            });
        }
        Effect::CheckLaunch { check, request } => {
            thread::spawn(move || {
                let error = check_launch(&deps, &request, &tx);
                let _ = tx.send(Event::LaunchChecked {
                    check,
                    request,
                    error,
                });
            });
        }
        Effect::RolloutWritten(path) => {
            thread::spawn(move || {
                let at = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| jiff::Timestamp::try_from(t).ok());
                let _ = tx.send(Event::RolloutWritten { path, at });
            });
        }
        // The accounts are read again first; the same sender keeps them ahead of the result,
        // so the rows are rebuilt by the time it is told.
        Effect::RemoveAccount(account) => {
            thread::spawn(move || {
                let result =
                    registry::unregister(&deps.config, &account).map_err(|e| format!("{e:#}"));
                if let Ok(registry) = Registry::load(&deps.config) {
                    let accounts = registry.all(&deps.env);
                    if accounts != deps.accounts {
                        let _ = tx.send(Event::Accounts(accounts));
                    }
                }
                let _ = tx.send(Event::AccountRemoved { account, result });
            });
        }
        Effect::Preview(path, provider) => {
            thread::spawn(move || {
                let result = match provider {
                    Provider::Claude => transcript::preview(&path, PREVIEW_MESSAGES),
                    Provider::Codex => codex::preview(&path, PREVIEW_MESSAGES),
                }
                .map_err(|e| e.to_string());
                let _ = tx.send(Event::Preview { path, result });
            });
        }
    }
}

/// Why `request` cannot be launched now, if it cannot: its directory, then (for a resume in
/// place) the session's state in every account, collected afresh (R16). The accounts are the
/// registry's as it is now (one may have been added since the TUI started; the TUI is told
/// through `tx`) and every account seen before (one may have been unregistered meanwhile and
/// still run it, C2). An account that cannot be read may be running it, and so may one the
/// registry cannot say: both refuse too.
fn check_launch(deps: &Deps, request: &LaunchRequest, tx: &Sender<Event>) -> Option<String> {
    if let Some(error) = request.cwd.as_deref().and_then(check_dir) {
        return Some(error);
    }
    let id = request.resumes()?;
    let accounts = match Registry::load(&deps.config) {
        Ok(registry) => registry.all(&deps.env),
        Err(e) => {
            return Some(format!(
                "cannot confirm that session {} is not running: cannot read the registry: {e:#}",
                app::short_id(&id)
            ));
        }
    };
    if accounts != deps.accounts {
        let _ = tx.send(Event::Accounts(accounts.clone()));
    }
    let accounts = union(&[&accounts, &deps.accounts, &deps.seen]);
    let found = live::collect_report(
        &accounts,
        deps.claude.as_deref(),
        deps.ps.as_deref(),
        &deps.env,
        live::TIMEOUT,
    );
    if let Some(running) = found
        .sessions
        .iter()
        .find(|s| s.session_id.as_deref() == Some(id.as_str()) && !s.is_inactive())
    {
        return Some(app::running_text(running));
    }
    if found.unknown.is_empty() {
        return None;
    }
    let why: Vec<String> = found
        .unknown
        .iter()
        .map(|u| format!("{}: {}", app::short_account(&u.account), u.reason))
        .collect();
    Some(format!(
        "cannot confirm that session {} is not running: {}",
        app::short_id(&id),
        why.join("; ")
    ))
}

/// Every account of `lists`, once, in first-seen order.
pub(super) fn union(lists: &[&[Account]]) -> Vec<Account> {
    let mut all: Vec<Account> = Vec::new();
    for account in lists.iter().copied().flatten() {
        if !all.contains(account) {
            all.push(account.clone());
        }
    }
    all
}

/// Why `dir` cannot be a launch directory, if it cannot.
fn check_dir(dir: &Path) -> Option<String> {
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => None,
        Ok(_) => Some(format!("{} is not a directory", dir.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Some(format!("{} does not exist", dir.display()))
        }
        Err(e) => Some(format!("cannot use {}: {e}", dir.display())),
    }
}

/// Cached entries first (so the list paints before any transcript is read), then the
/// refresh in batches, then the full index; the cache is saved at the end.
fn refresh_index(deps: &Deps, tx: &Sender<Event>) {
    let cache = deps.state_dir.join("index.json");
    let mut index = Index::load(&cache);
    let _ = tx.send(Event::IndexLoaded(
        index.entries.values().cloned().collect(),
    ));
    let stores = index::stores(&deps.accounts, &deps.env);
    let _ = tx.send(Event::Stores(stores.clone()));
    let mut batch = Vec::new();
    let mut last = Instant::now();
    index::refresh(&mut index, &stores, |p| {
        if let Some(entry) = p.entry {
            batch.push(entry.clone());
        }
        if p.done == 0 || p.done == p.total || last.elapsed() >= PROGRESS_EVERY {
            last = Instant::now();
            let _ = tx.send(Event::IndexProgress {
                done: p.done,
                total: p.total,
                entries: std::mem::take(&mut batch),
            });
        }
    });
    let error = index.save(&cache).err().map(|e| format!("{e:#}"));
    let _ = tx.send(Event::IndexDone {
        entries: index.entries.into_values().collect(),
        error,
    });
}

/// Token statistics (R20): the cache brought up to date with progress (the first and last
/// report, and at most every [`PROGRESS_EVERY`] in between), saved if that changed it, then
/// the report with attribution from the launch log and `history.jsonl`. Leaving the TUI does
/// not wait for it: the thread ends with the process, the cache unsaved.
fn compute_stats(deps: &Deps, tx: &Sender<Event>) {
    let path = deps.state_dir.join("stats.json");
    let mut cache = stats::Cache::load(&path);
    let sources = stats::sources(&deps.accounts, &deps.env);
    let mut last = Instant::now();
    let refreshed = stats::refresh(&mut cache, &sources, |done, total| {
        if done == 0 || done == total || last.elapsed() >= PROGRESS_EVERY {
            last = Instant::now();
            let _ = tx.send(Event::StatsProgress { done, total });
        }
    });
    let error = cache
        .save_if_changed(&path, &refreshed)
        .err()
        .map(|e| format!("{e:#}"));
    let log = deps.state_dir.join("launches.jsonl");
    let attribution = attribution::collect(&deps.accounts, &deps.env, &log, &[]);
    let report = stats::report(
        &cache,
        &sources,
        &attribution,
        &deps.accounts,
        (deps.clock)(),
        &deps.tz,
    );
    let _ = tx.send(Event::Stats { report, error });
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc;

    use jiff::tz::TimeZone;

    use super::*;
    use crate::registry::{Account, CLAUDE, Home};

    fn user(text: &str, minute: u32) -> String {
        format!(
            "{{\"type\":\"user\",\"cwd\":\"/w\",\"timestamp\":\"2026-09-24T10:{minute:02}:00Z\",\
             \"entrypoint\":\"cli\",\"message\":{{\"role\":\"user\",\"content\":\"{text}\"}}}}\n"
        )
    }

    fn deps(root: &std::path::Path) -> Arc<Deps> {
        let home = root.join("max");
        let project = home.join("projects/-w");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("s1.jsonl"), user("first", 1)).unwrap();
        fs::write(project.join("s2.jsonl"), user("second", 2)).unwrap();
        Arc::new(Deps {
            accounts: vec![Account {
                provider: CLAUDE,
                name: "max".into(),
                home: Home::Path(home.display().to_string()),
            }],
            env: [(
                "HOME".to_string(),
                root.join("nohome").display().to_string(),
            )]
            .into(),
            claude: None,
            codex: None,
            ps: None,
            seen: vec![],
            tz: TimeZone::UTC,
            clock: jiff::Timestamp::now,
            config: root.join("config.toml"),
            state_dir: root.join("state"),
            cwd: None,
            mode: crate::tui::app::Mode::Browse,
            private: false,
        })
    }

    fn collect(effect: Effect, deps: &Arc<Deps>, until: impl Fn(&Event) -> bool) -> Vec<Event> {
        let (tx, rx) = mpsc::channel();
        spawn(effect, deps, &tx);
        let mut events = Vec::new();
        while let Ok(e) = rx.recv_timeout(Duration::from_secs(10)) {
            let done = until(&e);
            events.push(e);
            if done {
                break;
            }
        }
        events
    }

    #[test]
    fn index_refresh_reports_cache_first_then_progress_then_done() {
        let dir = tempfile::tempdir().unwrap();
        let deps = deps(dir.path());
        let done = |e: &Event| matches!(e, Event::IndexDone { .. });
        let first = collect(Effect::RefreshIndex, &deps, done);
        assert!(
            matches!(&first[0], Event::IndexLoaded(v) if v.is_empty()),
            "{first:?}"
        );
        let streamed: usize = first
            .iter()
            .map(|e| match e {
                Event::IndexProgress { entries, .. } => entries.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(streamed, 2, "entries arrive with progress: {first:?}");
        let Some(Event::IndexDone { entries, error }) = first.last() else {
            panic!("{first:?}")
        };
        assert_eq!((entries.len(), error), (2, &None));
        assert!(deps.state_dir.join("index.json").is_file());

        // Second run: the cache is sent before anything is read.
        let second = collect(Effect::RefreshIndex, &deps, done);
        assert!(
            matches!(&second[0], Event::IndexLoaded(v) if v.len() == 2),
            "{second:?}"
        );
    }

    /// R20: progress first and last, then the report; the cache is written, and not rewritten
    /// when nothing changed.
    #[test]
    fn stats_report_progress_then_the_report_and_cache_the_counts() {
        let dir = tempfile::tempdir().unwrap();
        let deps = deps(dir.path());
        let transcript = dir.path().join("max/projects/-w/s1.jsonl");
        let mut text = fs::read_to_string(&transcript).unwrap();
        text.push_str(
            "{\"type\":\"assistant\",\"timestamp\":\"2026-09-24T10:03:00Z\",\
             \"message\":{\"id\":\"msg_1\",\"model\":\"claude-test\",\"role\":\"assistant\",\
             \"content\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":40,\
             \"cache_creation_input_tokens\":100,\"cache_read_input_tokens\":300}}}\n",
        );
        fs::write(&transcript, text).unwrap();
        let done = |e: &Event| matches!(e, Event::Stats { .. });
        let events = collect(Effect::Stats, &deps, done);
        assert_eq!(
            events.first(),
            Some(&Event::StatsProgress { done: 0, total: 2 })
        );
        assert!(
            events.contains(&Event::StatsProgress { done: 2, total: 2 }),
            "{events:?}"
        );
        let Some(Event::Stats { report, error }) = events.last() else {
            panic!("{events:?}")
        };
        assert_eq!(error, &None);
        assert_eq!(report.files, 2);
        let all = report.table(stats::Period::All);
        assert_eq!(all.sections[0].accounts, ["claude:max"]);
        let unattributed = all.sections.last().unwrap();
        assert!(unattributed.accounts.is_empty());
        assert_eq!(unattributed.models[0].model, "claude-test");
        assert_eq!(unattributed.models[0].tokens.total(), 443);
        let cache = deps.state_dir.join("stats.json");
        let written = fs::metadata(&cache).unwrap().modified().unwrap();

        let again = collect(Effect::Stats, &deps, done);
        assert_eq!(again.last(), events.last());
        assert_eq!(fs::metadata(&cache).unwrap().modified().unwrap(), written);
    }

    #[test]
    fn index_refresh_reports_the_stores() {
        let dir = tempfile::tempdir().unwrap();
        let deps = deps(dir.path());
        let events = collect(Effect::RefreshIndex, &deps, |e| {
            matches!(e, Event::IndexDone { .. })
        });
        let stores: Vec<&Vec<index::Store>> = events
            .iter()
            .filter_map(|e| match e {
                Event::Stores(s) => Some(s),
                _ => None,
            })
            .collect();
        let real = dir.path().join("max/projects").canonicalize().unwrap();
        assert_eq!(stores.len(), 1, "{events:?}");
        assert_eq!(stores[0][0].path, real);
        assert_eq!(stores[0][0].accounts, ["claude:max"]);
    }

    #[test]
    fn launch_directories_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let deps = deps(dir.path());
        let file = dir.path().join("file");
        fs::write(&file, "").unwrap();
        let check = |cwd: Option<std::path::PathBuf>| {
            let request = LaunchRequest {
                account: deps.accounts[0].clone(),
                args: vec![],
                cwd,
                what: "x".into(),
            };
            let check = Effect::CheckLaunch {
                check: 7,
                request: request.clone(),
            };
            let events = collect(check, &deps, |_| true);
            let [
                Event::LaunchChecked {
                    check: 7,
                    request: r,
                    error,
                },
            ] = events.as_slice()
            else {
                panic!("{events:?}")
            };
            assert_eq!(r, &request);
            error.clone()
        };
        assert_eq!(check(Some(dir.path().to_path_buf())), None);
        assert_eq!(check(None), None);
        let gone = dir.path().join("gone");
        assert_eq!(
            check(Some(gone.clone())),
            Some(format!("{} does not exist", gone.display()))
        );
        assert_eq!(
            check(Some(file.clone())),
            Some(format!("{} is not a directory", file.display()))
        );
    }

    /// Without claude and ps nothing can be confirmed: a resume in place is refused, a fork
    /// or a new session is not (R16).
    #[test]
    fn a_resume_that_cannot_be_checked_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let deps = deps(dir.path());
        // The registry has max, and the TUI started with it (and the default account).
        let max = deps.accounts[0].clone();
        let Home::Path(home) = &max.home else {
            unreachable!()
        };
        fs::write(
            &deps.config,
            format!("[[account]]\nprovider = \"claude\"\nname = \"max\"\nhome = \"{home}\"\n"),
        )
        .unwrap();
        let deps = Arc::new(Deps {
            accounts: vec![Account::default_for(CLAUDE), max.clone()],
            ..Deps::clone(&deps)
        });
        let id = "0badf00d-0000-4000-8000-000000000000";
        let check = |args: &[&str]| {
            let request = LaunchRequest {
                account: max.clone(),
                args: args.iter().map(|a| a.to_string()).collect(),
                cwd: Some(dir.path().to_path_buf()),
                what: "x".into(),
            };
            let events = collect(Effect::CheckLaunch { check: 1, request }, &deps, |_| true);
            let [Event::LaunchChecked { error, .. }] = events.as_slice() else {
                panic!("{events:?}")
            };
            error.clone()
        };
        let error = check(&["--resume", id]).unwrap();
        assert_eq!(
            error,
            "cannot confirm that session 0badf00d is not running: default: `claude` not found \
             on PATH, and no `ps` to check its sessions/ directory; max: `claude` not found on \
             PATH, and no `ps` to check its sessions/ directory"
        );
        assert_eq!(check(&["--resume", id, "--fork-session"]), None);
        assert_eq!(check(&["-n", "x"]), None);
    }

    #[test]
    fn cache_write_failure_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let deps = deps(dir.path());
        // `state` is a file: the cache cannot be written below it.
        fs::write(&deps.state_dir, "").unwrap();
        let events = collect(Effect::RefreshIndex, &deps, |e| {
            matches!(e, Event::IndexDone { .. })
        });
        let Some(Event::IndexDone { entries, error }) = events.last() else {
            panic!("{events:?}")
        };
        assert_eq!(entries.len(), 2);
        assert!(error.is_some());
    }

    #[test]
    fn preview_and_missing_claude() {
        let dir = tempfile::tempdir().unwrap();
        let deps = deps(dir.path());
        let path = dir.path().join("max/projects/-w/s1.jsonl");
        let events = collect(
            Effect::Preview(path.clone(), Provider::Claude),
            &deps,
            |_| true,
        );
        let [
            Event::Preview {
                path: p,
                result: Ok(messages),
            },
        ] = events.as_slice()
        else {
            panic!("{events:?}")
        };
        assert_eq!(p, &path);
        assert_eq!(messages[0].text, "first");
        let max = deps.accounts[0].clone();
        let events = collect(Effect::LiveUsage(vec![max.clone()]), &deps, |_| true);
        assert_eq!(
            events,
            [Event::LiveUsage {
                account: max,
                result: Err("`claude` not found on PATH".into())
            }]
        );
    }

    /// R10: a codex account's live usage goes to `codex app-server`, and brings its identity;
    /// without codex it fails on its own.
    #[test]
    fn live_usage_asks_each_accounts_agent() {
        let dir = tempfile::tempdir().unwrap();
        let work = Account {
            provider: Provider::Codex,
            name: "work".into(),
            home: Home::Path(dir.path().join("work").display().to_string()),
        };
        let without = collect(
            Effect::LiveUsage(vec![work.clone()]),
            &deps(dir.path()),
            |_| true,
        );
        assert_eq!(
            without,
            [Event::LiveUsage {
                account: work.clone(),
                result: Err("`codex` not found on PATH".into())
            }]
        );
        let codex = crate::probe::script(
            dir.path(),
            "codex",
            "read a; read b; read c; read d; \
             echo '{\"id\":2,\"result\":{\"rateLimits\":{\"primary\":{\"usedPercent\":4,\
             \"windowDurationMins\":300}}}}'; \
             echo '{\"id\":3,\"result\":{\"account\":{\"type\":\"chatgpt\",\
             \"email\":\"c@example.com\",\"planType\":\"plus\"}}}'; cat >/dev/null",
        );
        let deps = Arc::new(Deps {
            accounts: vec![work.clone()],
            codex: Some(codex),
            ..Deps::clone(&deps(dir.path()))
        });
        let events = collect(Effect::LiveUsage(vec![work.clone()]), &deps, |_| true);
        let identity = crate::identity::Identity::LoggedIn {
            email: Some("c@example.com".into()),
            org: None,
            plan: Some("plus".into()),
            method: Some("ChatGPT".into()),
            cached: false,
        };
        assert_eq!(
            events,
            [Event::LiveUsage {
                account: work,
                result: Ok(usage::LiveResult {
                    usage: usage::LiveUsage::Rows(vec![usage::UsageRow {
                        label: "Session".into(),
                        percent: 4.0,
                        severity: None,
                        resets: None,
                    }]),
                    identity: Some(identity),
                })
            }]
        );
    }

    /// R14a, R16: the account leaves config.toml (its home stays), and the registry read
    /// again arrives before the result.
    #[test]
    fn remove_account_rewrites_the_registry() {
        let dir = tempfile::tempdir().unwrap();
        let deps = deps(dir.path());
        let max = deps.accounts[0].clone();
        let Home::Path(home) = max.home.clone() else {
            unreachable!()
        };
        fs::write(
            &deps.config,
            format!("[[account]]\nprovider = \"claude\"\nname = \"max\"\nhome = \"{home}\"\n"),
        )
        .unwrap();
        let events = collect(Effect::RemoveAccount(max.clone()), &deps, |e| {
            matches!(e, Event::AccountRemoved { .. })
        });
        assert_eq!(
            events,
            [
                Event::Accounts(vec![Account::default_for(CLAUDE)]),
                Event::AccountRemoved {
                    account: max,
                    result: Ok(())
                }
            ]
        );
        assert!(!fs::read_to_string(&deps.config).unwrap().contains("max"));
        assert!(Path::new(&home).join("projects/-w/s1.jsonl").exists());
    }
}
