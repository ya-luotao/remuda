//! Background work for [`Effect`]s: each runs on its own thread and reports back as
//! [`Event`]s on the event loop's channel. A send error means the TUI has quit; the result
//! is dropped. Work that goes over the accounts reads them when it starts
//! ([`super::accounts::Listing::read`]).

use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use crate::index::{self, Index};
use crate::provider::{Provider, codex};
use crate::registry;
use crate::{account_config, attribution, checks, identity, live, stats, transcript, usage};

use super::Deps;
use super::accounts::Reading;
use super::app::{self, Effect, Event, LaunchRequest, Marked, PREVIEW_MESSAGES};

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
/// and the foreground runs ([`Effect::Launch`], [`Effect::Setup`]) are the event loop's own
/// business.
pub fn spawn(effect: Effect, deps: &Arc<Deps>, tx: &Sender<Event>) {
    let deps = Arc::clone(deps);
    let tx = tx.clone();
    match effect {
        Effect::Quit | Effect::Pick(_) | Effect::Launch(_) | Effect::Setup { .. } => {}
        Effect::ReadAccounts => {
            thread::spawn(move || {
                deps.listing.read(&tx);
            });
        }
        Effect::RefreshIndex => {
            thread::spawn(move || refresh_index(&deps, &tx));
        }
        Effect::Identities => {
            thread::spawn(move || {
                for account in deps.listing.read(&tx).accounts {
                    let (deps, tx) = (Arc::clone(&deps), tx.clone());
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
            });
        }
        Effect::CachedUsage => {
            thread::spawn(move || {
                for account in &deps.listing.read(&tx).accounts {
                    let result = usage::cached_usage(account, &deps.env);
                    let _ = tx.send(Event::CachedUsage {
                        account: account.clone(),
                        result,
                    });
                }
            });
        }
        Effect::LiveUsage(which) => {
            thread::spawn(move || {
                // Only the accounts still listed, home and all, are asked: one that left the
                // registry since the app chose them is not queried in its old home (its row
                // goes with the change this read tells).
                let listed = deps.listing.read(&tx).accounts;
                for account in which.into_iter().filter(|a| listed.contains(a)) {
                    let (deps, tx) = (Arc::clone(&deps), tx.clone());
                    thread::spawn(move || {
                        let result = match deps.program(account.provider) {
                            Some(program) => {
                                usage::live_usage(&account, program, LIVE_USAGE_TIMEOUT)
                            }
                            None => Err(format!(
                                "`{}` not found on PATH",
                                account.provider.program()
                            )),
                        };
                        let _ = tx.send(Event::LiveUsage { account, result });
                    });
                }
            });
        }
        Effect::Live => {
            thread::spawn(move || {
                let sessions = live::collect(
                    &deps.listing.read(&tx).accounts,
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
                let accounts = deps.listing.read(&tx).accounts;
                let base = attribution::collect(&accounts, &deps.env, &log, &[]);
                let _ = tx.send(Event::Attribution(base));
            });
        }
        Effect::Stats => {
            thread::spawn(move || compute_stats(&deps, &tx));
        }
        Effect::Checks => {
            thread::spawn(move || {
                let reading = deps.listing.read(&tx);
                let stores = index::stores(&reading.accounts, &deps.env);
                let mut found = checks::run(&reading.accounts, &deps.env, &stores);
                // A registry that cannot be read shares nothing (the launch says why).
                found.extend(checks::sharing(
                    &reading.accounts,
                    &deps.env,
                    &reading.sharing,
                ));
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
                let result = registry::unregister(deps.listing.config(), &account)
                    .map_err(|e| format!("{e:#}"));
                deps.listing.read(&tx);
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
        Effect::Config {
            request,
            account,
            cwd,
        } => {
            thread::spawn(move || {
                let result = if account.provider != Provider::Claude {
                    Err("configuration listing is Claude-only".to_string())
                } else {
                    let reading = deps.listing.read(&tx);
                    let mut view =
                        account_config::read(&account, &reading.sharing, cwd.as_deref(), &deps.env);
                    // The account's own configuration still shows.
                    if let Some(e) = &reading.unreadable {
                        view.problems.insert(
                            0,
                            format!(
                                "cannot read {}: {e}; shared configuration unknown",
                                deps.listing.config().display()
                            ),
                        );
                    }
                    Ok(Box::new(view))
                };
                let _ = tx.send(Event::Config {
                    request,
                    account,
                    result,
                });
            });
        }
    }
}

/// Why `request` cannot be launched now, if it cannot. The registry is read first, whatever
/// the answer turns out to be (the TUI is told of a change through `tx`): the account must
/// still be listed; then the directory; then (for a resume in place) the session's state in
/// every account, collected afresh (R16). The accounts asked are the registry's and every
/// account listed before (one may have been unregistered meanwhile and still run it, C2). An
/// account that cannot be read may be running it, and so may one the registry cannot say: both
/// refuse too.
fn check_launch(deps: &Deps, request: &LaunchRequest, tx: &Sender<Event>) -> Option<Marked> {
    let reading = deps.listing.read(tx);
    // A registry that cannot be read says nothing of the account: see below.
    if reading.unreadable.is_none()
        && let Some(gone) = reading.refusal(&request.account)
    {
        return Some(gone.into());
    }
    if let Some(error) = request.cwd.as_deref().and_then(check_dir) {
        return Some(error);
    }
    let id = request.resumes()?;
    if let Some(e) = &reading.unreadable {
        // Any other launch is refused when it starts, for the same reason.
        let said = format!(
            "cannot confirm that session {} is not running: cannot read the registry: ",
            app::short_id(&id)
        );
        return Some(Marked::from(said).text(e));
    }
    let found = live::collect_report(
        &reading.seen,
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
        return Some(app::running_text(running).into());
    }
    if found.unknown.is_empty() {
        return None;
    }
    // Each account's reason is a piece of its own: what private mode masks in one does not
    // reach the next (R21).
    let mut said = Marked::from(format!(
        "cannot confirm that session {} is not running: ",
        app::short_id(&id)
    ));
    for (i, u) in found.unknown.iter().enumerate() {
        let sep = if i == 0 { "" } else { "; " };
        said = said
            .text(format!("{sep}{}: ", app::short_account(&u.account)))
            .text(&u.reason);
    }
    Some(said)
}

/// Why `dir` cannot be a launch directory, if it cannot; the directory is marked as the path
/// it is, so private mode masks it whole whatever its name holds (R21).
fn check_dir(dir: &Path) -> Option<Marked> {
    let path = || Marked::default().path(dir.display());
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => None,
        Ok(_) => Some(path().text(" is not a directory")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(path().text(" does not exist")),
        Err(e) => Some(
            Marked::from("cannot use ")
                .join(&path())
                .text(format!(": {e}")),
        ),
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
    let stores = index::stores(&deps.listing.read(tx).accounts, &deps.env);
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
/// the report with attribution from the launch log and `history.jsonl`, for the accounts and
/// with the prices of `config.toml` as it is now (the built-in ones when it cannot be read,
/// which is told). The accounts may change while the transcripts are read (a cold run is
/// long): the report is for the accounts the registry lists when it ends, computed again if
/// they are not the ones it started from. Leaving the TUI does not wait for it: the thread ends
/// with the process, the cache unsaved.
fn compute_stats(deps: &Deps, tx: &Sender<Event>) {
    loop {
        let reading = deps.listing.read(tx);
        let result = stats_for(deps, tx, &reading);
        if deps.listing.answer(tx, &reading.accounts, result) {
            return;
        }
    }
}

/// One computation of [`compute_stats`], from one reading of the registry.
fn stats_for(deps: &Deps, tx: &Sender<Event>, reading: &Reading) -> Event {
    let mut errors: Vec<String> = Vec::new();
    let prices_error = reading
        .unreadable
        .as_ref()
        .map(|e| format!("prices: {e} (built-in prices used)"));
    let (accounts, prices) = (&reading.accounts, &reading.prices);
    let path = deps.state_dir.join("stats.json");
    let mut cache = stats::Cache::load(&path);
    let sources = stats::sources(accounts, &deps.env);
    let mut last = Instant::now();
    let refreshed = stats::refresh(&mut cache, &sources, |done, total| {
        if done == 0 || done == total || last.elapsed() >= PROGRESS_EVERY {
            last = Instant::now();
            let _ = tx.send(Event::StatsProgress { done, total });
        }
    });
    if let Err(e) = cache.save_if_changed(&path, &refreshed) {
        errors.push(format!("stats cache: {e:#}"));
    }
    errors.extend(prices_error);
    let log = deps.state_dir.join("launches.jsonl");
    let attribution = attribution::collect(accounts, &deps.env, &log, &[]);
    let report = stats::report(
        &cache,
        &sources,
        &attribution,
        accounts,
        prices,
        (deps.clock)(),
        &deps.tz,
    );
    let error = (!errors.is_empty()).then(|| errors.join(" · "));
    Event::Stats { report, error }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc;

    use jiff::tz::TimeZone;

    use super::*;
    use crate::registry::{Account, CLAUDE, Home};
    use crate::tui::accounts::Listing;

    fn user(text: &str, minute: u32) -> String {
        format!(
            "{{\"type\":\"user\",\"cwd\":\"/w\",\"timestamp\":\"2026-09-24T10:{minute:02}:00Z\",\
             \"entrypoint\":\"cli\",\"message\":{{\"role\":\"user\",\"content\":\"{text}\"}}}}\n"
        )
    }

    /// `[[account]]` for [`max`], as `config.toml` has it.
    fn registered(root: &std::path::Path) -> String {
        let home = root.join("max");
        format!(
            "[[account]]\nprovider = \"claude\"\nname = \"max\"\nhome = \"{}\"\n",
            home.display()
        )
    }

    /// The account the registry of [`deps`] lists after the implicit default.
    fn max(deps: &Deps) -> Account {
        let (tx, _rx) = mpsc::channel();
        let accounts = deps.listing.read(&tx).accounts;
        assert_eq!(accounts[0], Account::default_for(CLAUDE));
        assert_eq!(accounts[1].name, "max");
        accounts[1].clone()
    }

    /// `max` is registered (two transcripts in its home); the native login has no home.
    fn deps(root: &std::path::Path) -> Arc<Deps> {
        deps_with(root, "")
    }

    /// [`deps`] with `more` in the registry after `max`.
    fn deps_with(root: &std::path::Path, more: &str) -> Arc<Deps> {
        let home = root.join("max");
        let project = home.join("projects/-w");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("s1.jsonl"), user("first", 1)).unwrap();
        fs::write(project.join("s2.jsonl"), user("second", 2)).unwrap();
        let env: crate::Env = [(
            "HOME".to_string(),
            root.join("nohome").display().to_string(),
        )]
        .into();
        let config = root.join("config.toml");
        fs::write(&config, registered(root) + more).unwrap();
        Arc::new(Deps {
            listing: Arc::new(Listing::open(config, env.clone()).unwrap()),
            env,
            claude: None,
            codex: None,
            ps: None,
            tz: TimeZone::UTC,
            clock: jiff::Timestamp::now,
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
        // A section for each account of the registry, the implicit default first.
        let accounts: Vec<_> = all.sections.iter().map(|s| s.accounts.join("+")).collect();
        assert_eq!(accounts, ["claude:default", "claude:max", ""]);
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

    /// R20, R3: each computation prices with `config.toml` as it is then; prices that cannot be
    /// read are told, and the built-in ones used.
    #[test]
    fn stats_use_config_prices_and_say_when_they_cannot() {
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
        let cost = |events: &[Event]| {
            let Some(Event::Stats { report, error }) = events.last() else {
                panic!("{events:?}")
            };
            let all = report.table(stats::Period::All);
            (all.sections.last().unwrap().models[0].cost, error.clone())
        };

        fs::write(
            deps.listing.config(),
            registered(dir.path())
                + "[prices.\"claude-test\"]\ninput = 1\noutput = 1\ncache_read = 1\n\
                   cache_write_5m = 1\ncache_write_1h = 1\n",
        )
        .unwrap();
        let (priced, error) = cost(&collect(Effect::Stats, &deps, done));
        assert_eq!(error, None);
        assert_eq!((priced.pico_usd, priced.unpriced_tokens), (443_000_000, 0));

        fs::write(deps.listing.config(), "prices = 1\n").unwrap();
        let (unpriced, error) = cost(&collect(Effect::Stats, &deps, done));
        let error = error.unwrap();
        assert!(
            error.starts_with("prices: ") && error.ends_with("(built-in prices used)"),
            "{error}"
        );
        assert!(error.contains("config.toml"), "{error}");
        assert_eq!((unpriced.pico_usd, unpriced.unpriced_tokens), (0, 443));
    }

    /// The registry [`a_clock_that_unregisters`] empties, once.
    static UNREGISTER: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

    /// The report reads the clock when the transcripts have been read: this one empties the
    /// registry then, as `remuda remove` in another terminal would while the statistics are
    /// computed.
    fn a_clock_that_unregisters() -> jiff::Timestamp {
        if let Some(config) = UNREGISTER.lock().unwrap().take() {
            fs::write(config, "").unwrap();
        }
        jiff::Timestamp::now()
    }

    /// R16, R20: the accounts change while the statistics are computed: the report for the
    /// accounts it started from is not sent; the change is told, and the report that arrives
    /// is for the accounts the registry lists then.
    #[test]
    fn stats_are_for_the_accounts_listed_when_they_end() {
        let dir = tempfile::tempdir().unwrap();
        let deps = Arc::new(Deps {
            clock: a_clock_that_unregisters,
            ..Deps::clone(&deps(dir.path()))
        });
        *UNREGISTER.lock().unwrap() = Some(deps.listing.config().to_path_buf());
        let events = collect(Effect::Stats, &deps, |e| matches!(e, Event::Stats { .. }));
        let default = Account::default_for(CLAUDE);
        let told: Vec<&Event> = events
            .iter()
            .filter(|e| matches!(e, Event::Accounts(_) | Event::Stats { .. }))
            .collect();
        let [Event::Accounts(accounts), Event::Stats { report, error }] = told.as_slice() else {
            panic!("one change, then one report: {events:?}")
        };
        assert_eq!(accounts, &[default]);
        assert_eq!(error, &None);
        let sections: Vec<String> = report
            .table(stats::Period::All)
            .sections
            .iter()
            .map(|s| s.accounts.join("+"))
            .collect();
        assert_eq!(sections, ["claude:default"], "max is gone from it");
        // Computed twice: max's two transcripts were read, then none.
        let read: Vec<&Event> = events
            .iter()
            .filter(|e| matches!(e, Event::StatsProgress { done: 0, .. }))
            .collect();
        assert_eq!(
            read,
            [
                &Event::StatsProgress { done: 0, total: 2 },
                &Event::StatsProgress { done: 0, total: 0 }
            ]
        );
        assert!(UNREGISTER.lock().unwrap().is_none(), "the clock was read");
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
                account: max(&deps),
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
            Some(
                Marked::default()
                    .path(gone.display())
                    .text(" does not exist")
            )
        );
        assert_eq!(
            check(Some(file.clone())),
            Some(
                Marked::default()
                    .path(file.display())
                    .text(" is not a directory")
            )
        );
    }

    /// Without claude and ps nothing can be confirmed: a resume in place is refused, a fork
    /// or a new session is not (R16).
    #[test]
    fn a_resume_that_cannot_be_checked_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let deps = deps(dir.path());
        // The registry has max, and the TUI started with it (and the default account).
        let max = max(&deps);
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
        let max = max(&deps);
        let events = collect(Effect::LiveUsage(vec![max.clone()]), &deps, |_| true);
        assert_eq!(
            events,
            [Event::LiveUsage {
                account: max,
                result: Err("`claude` not found on PATH".into())
            }]
        );
    }

    /// R22: an account's configuration is read in the background and comes back with its
    /// request number; a codex account has none to list.
    #[test]
    fn configuration_is_read_in_the_background() {
        let dir = tempfile::tempdir().unwrap();
        let deps = deps(dir.path());
        let max = max(&deps);
        let events = collect(
            Effect::Config {
                request: 3,
                account: max.clone(),
                cwd: None,
            },
            &deps,
            |_| true,
        );
        let [
            Event::Config {
                request: 3,
                account,
                result: Ok(view),
            },
        ] = events.as_slice()
        else {
            panic!("{events:?}")
        };
        assert_eq!(account, &max);
        assert_eq!(view.role, account_config::Role::Alone);
        assert_eq!(view.problems, Vec::<String>::new());

        // A registry that cannot be read: the account's own configuration, and why.
        fs::write(deps.listing.config(), "not toml [").unwrap();
        let events = collect(
            Effect::Config {
                request: 4,
                account: max.clone(),
                cwd: None,
            },
            &deps,
            |_| true,
        );
        let [
            Event::Config {
                result: Ok(view), ..
            },
        ] = events.as_slice()
        else {
            panic!("{events:?}")
        };
        assert!(
            view.problems[0].ends_with("; shared configuration unknown"),
            "{:?}",
            view.problems
        );

        let work = Account {
            provider: Provider::Codex,
            name: "work".into(),
            home: Home::Path(dir.path().join("work").display().to_string()),
        };
        let events = collect(
            Effect::Config {
                request: 5,
                account: work.clone(),
                cwd: None,
            },
            &deps,
            |_| true,
        );
        assert_eq!(
            events,
            [Event::Config {
                request: 5,
                account: work,
                result: Err("configuration listing is Claude-only".into()),
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
        let registered = format!(
            "[[account]]\nprovider = \"codex\"\nname = \"work\"\nhome = \"{}\"\n",
            work.home
        );
        let without = collect(
            Effect::LiveUsage(vec![work.clone()]),
            &deps_with(dir.path(), &registered),
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
            codex: Some(codex),
            ..Deps::clone(&deps_with(dir.path(), &registered))
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

    /// R16: reading the registry again is work of its own, so a refresh can ask for it while
    /// everything else is still running; a list that changed is the answer.
    #[test]
    fn read_accounts_tells_a_changed_registry() {
        let dir = tempfile::tempdir().unwrap();
        let deps = deps(dir.path());
        let max = max(&deps);
        fs::write(deps.listing.config(), "").unwrap();
        let events = collect(Effect::ReadAccounts, &deps, |_| true);
        assert_eq!(
            events,
            [Event::Accounts(vec![Account::default_for(CLAUDE)])]
        );
        fs::write(deps.listing.config(), registered(dir.path())).unwrap();
        let events = collect(Effect::ReadAccounts, &deps, |_| true);
        assert_eq!(
            events,
            [Event::Accounts(vec![Account::default_for(CLAUDE), max])]
        );
    }

    /// R10, R16: live usage is asked of the accounts the app chose that the registry still
    /// lists, home and all: the change is told first, and an account that left the registry
    /// or is registered again with another home is not queried in its old home.
    #[test]
    fn live_usage_asks_only_accounts_still_listed() {
        let dir = tempfile::tempdir().unwrap();
        let deps = deps(dir.path());
        let (default, max) = (Account::default_for(CLAUDE), max(&deps));
        let gone = Account {
            provider: CLAUDE,
            name: "team".into(),
            home: Home::Path("/p/team".into()),
        };
        let moved = Account {
            home: Home::Path("/p/elsewhere".into()),
            ..max.clone()
        };
        fs::write(
            deps.listing.config(),
            "[[account]]\nprovider = \"claude\"\nname = \"max\"\nhome = \"/p/elsewhere\"\n",
        )
        .unwrap();
        let (tx, rx) = mpsc::channel();
        // The app's rows from before the change, the ones to skip first.
        let which = vec![gone, max, default.clone()];
        spawn(Effect::LiveUsage(which), &deps, &tx);
        let wait = Duration::from_secs(10);
        assert_eq!(
            rx.recv_timeout(wait).unwrap(),
            Event::Accounts(vec![default.clone(), moved])
        );
        assert_eq!(
            rx.recv_timeout(wait).unwrap(),
            Event::LiveUsage {
                account: default,
                result: Err("`claude` not found on PATH".into())
            }
        );
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "nothing for the accounts no longer listed"
        );
    }

    /// R14a, R16: the account leaves config.toml (its home stays), and the registry read
    /// again arrives before the result.
    #[test]
    fn remove_account_rewrites_the_registry() {
        let dir = tempfile::tempdir().unwrap();
        let deps = deps(dir.path());
        let max = max(&deps);
        let Home::Path(home) = max.home.clone() else {
            unreachable!()
        };
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
        let left = fs::read_to_string(deps.listing.config()).unwrap();
        assert!(!left.contains("max"), "{left}");
        assert!(Path::new(&home).join("projects/-w/s1.jsonl").exists());
    }
}
