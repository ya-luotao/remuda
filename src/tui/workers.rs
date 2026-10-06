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
use crate::registry::{self, Account};
use crate::{account_config, attribution, checks, identity, live, owned, stats, transcript, usage};

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
        Effect::Identities(accounts) => {
            thread::spawn(move || {
                for account in listed(&deps, &tx, accounts) {
                    answer(&deps, &tx, move |deps| {
                        let (identity, _warning) = identity::identify(
                            &account,
                            &deps.agents(),
                            &deps.env,
                            IDENTITY_TIMEOUT,
                        );
                        Event::Identity { account, identity }
                    });
                }
            });
        }
        Effect::CachedUsage(accounts) => {
            thread::spawn(move || {
                for account in listed(&deps, &tx, accounts) {
                    let result = usage::cached_usage(&account, &deps.env);
                    let _ = tx.send(Event::CachedUsage { account, result });
                }
            });
        }
        Effect::LiveUsage(accounts) => {
            thread::spawn(move || {
                for account in listed(&deps, &tx, accounts) {
                    answer(&deps, &tx, move |deps| {
                        let result =
                            usage::live_usage(&account, &deps.agents(), LIVE_USAGE_TIMEOUT);
                        Event::LiveUsage {
                            account,
                            result,
                            answered_at: (deps.clock)(),
                        }
                    });
                }
            });
        }
        Effect::Live => {
            thread::spawn(move || {
                let sessions = live::collect(
                    &deps.listing.read(&tx).accounts,
                    &deps.agents(),
                    deps.ps.as_deref(),
                    &deps.env,
                    live::TIMEOUT,
                );
                let _ = tx.send(Event::Live(sessions));
            });
        }
        Effect::Attribution => {
            thread::spawn(move || {
                let log = owned::launch_log(&deps.state_dir);
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
        Effect::Logs { account, short_id } => answer(&deps, &tx, move |deps| {
            let result = live::logs(&deps.agents(), &account, &short_id, live::LOGS_TIMEOUT);
            Event::Logs {
                account,
                short_id,
                result,
            }
        }),
        Effect::Control {
            account,
            verb,
            short_id,
        } => answer(&deps, &tx, move |deps| {
            let result = live::control(&deps.agents(), &account, verb, &short_id, CONTROL_TIMEOUT);
            Event::ControlDone {
                verb,
                short_id,
                result,
            }
        }),
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
        Effect::RolloutWritten(path) => answer(&deps, &tx, move |_| {
            let at = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| jiff::Timestamp::try_from(t).ok());
            Event::RolloutWritten { path, at }
        }),
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
        Effect::Preview(path, provider) => answer(&deps, &tx, move |_| {
            let result = match provider {
                Provider::Claude => transcript::preview(&path, PREVIEW_MESSAGES),
                Provider::Codex => codex::preview(&path, PREVIEW_MESSAGES),
            }
            .map_err(|e| e.to_string());
            Event::Preview { path, result }
        }),
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
                            Marked::default()
                                .words("cannot read ")
                                .path(deps.listing.config().display())
                                .words(": ")
                                .join(e)
                                .words("; shared configuration unknown"),
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

/// Work that answers once: `work` runs on a thread of its own, and the event it returns is
/// sent. It reads nothing of the registry itself; what is asked of each account is given the
/// accounts by [`listed`].
fn answer(
    deps: &Arc<Deps>,
    tx: &Sender<Event>,
    work: impl FnOnce(&Deps) -> Event + Send + 'static,
) {
    let (deps, tx) = (Arc::clone(deps), tx.clone());
    thread::spawn(move || {
        let _ = tx.send(work(&deps));
    });
}

/// The accounts of `asked` (the app's rows when it asked) that the registry still lists, home
/// and all (R16): it is read first, which tells the app of a change, and an account that left
/// it meanwhile is not asked in its old home. Its row goes with that change, and the query
/// that was out for it with the row.
fn listed(deps: &Deps, tx: &Sender<Event>, asked: Vec<Account>) -> Vec<Account> {
    let listed = deps.listing.read(tx).accounts;
    asked.into_iter().filter(|a| listed.contains(a)).collect()
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
        return Some(gone);
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
        return Some(Marked::default().words(said).join(e));
    }
    let found = live::collect_report(
        &reading.seen,
        &deps.agents(),
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
    let mut said = Marked::default().words(format!(
        "cannot confirm that session {} is not running: ",
        app::short_id(&id)
    ));
    for (i, u) in found.unknown.iter().enumerate() {
        let sep = if i == 0 { "" } else { "; " };
        said = said
            .words(format!("{sep}{}: ", app::short_account(&u.account)))
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
        Ok(_) => Some(path().words(" is not a directory")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(path().words(" does not exist")),
        Err(e) => Some(
            Marked::default()
                .words("cannot use ")
                .join(&path())
                .words(": ")
                .text(e.to_string()),
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
    let (stores, given) = index::resolve(&deps.listing.read(tx).accounts, &deps.env);
    let _ = tx.send(Event::Stores(stores.clone()));
    let mut batch = Vec::new();
    let mut last = Instant::now();
    let refreshed = index::refresh_with(&mut index, &stores, &given, |p| {
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
    // A directory that could not be read first (R8), then a cache that could not be written:
    // the system's error, one text unless a cause says its pieces.
    let mut errors: Vec<Marked> = refreshed.incomplete().into_iter().collect();
    if let Err(e) = index.save(&cache) {
        errors.push(
            Marked::default()
                .words("index cache: ")
                .join(&Marked::from_error(&e)),
        );
    }
    let error = each_by_itself(&errors);
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
    let mut errors: Vec<Marked> = Vec::new();
    let prices_error = reading.unreadable.as_ref().map(|e| {
        Marked::default()
            .words("prices: ")
            .join(e)
            .words(" (built-in prices used)")
    });
    let (accounts, prices) = (&reading.accounts, &reading.prices);
    let path = deps.state_dir.join("stats.json");
    let mut cache = stats::Cache::load(&path);
    let (sources, given) = stats::resolve(accounts, &deps.env);
    let mut last = Instant::now();
    let refreshed = stats::refresh_with(&mut cache, &sources, &given, |done, total| {
        if done == 0 || done == total || last.elapsed() >= PROGRESS_EVERY {
            last = Instant::now();
            let _ = tx.send(Event::StatsProgress { done, total });
        }
    });
    errors.extend(refreshed.incomplete());
    if let Err(e) = cache.save_if_changed(&path, &refreshed) {
        errors.push(
            Marked::default()
                .words("stats cache: ")
                .join(&Marked::from_error(&e)),
        );
    }
    errors.extend(prices_error);
    let log = owned::launch_log(&deps.state_dir);
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
    let error = each_by_itself(&errors);
    Event::Stats { report, error }
}

/// What a refresh could not do, as one line for the status bar, ` · ` between the errors;
/// `None` when there is none. Each error keeps its pieces, so what private mode hides in one
/// does not reach the next (R21).
fn each_by_itself(errors: &[Marked]) -> Option<Marked> {
    errors.iter().fold(None, |said: Option<Marked>, error| {
        Some(match said {
            Some(said) => said.words(" · ").join(error),
            None => error.clone(),
        })
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc;

    use jiff::tz::TimeZone;

    use super::*;
    use crate::privacy::Piece;
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
            clock: answered,
            state_dir: root.join("state"),
            cwd: None,
            mode: crate::tui::app::Mode::Browse,
            private: false,
        })
    }

    /// `said` with `<>` for a path and `{}` for text from elsewhere: what is left is what
    /// private mode leaves readable (R21).
    fn shape(said: &Marked) -> String {
        said.pieces()
            .map(|(piece, kind)| match kind {
                Piece::Words => piece,
                Piece::Path => "<>",
                Piece::Text => "{}",
            })
            .collect()
    }

    /// The workers' clock: when a live query answers (R10).
    fn answered() -> jiff::Timestamp {
        "2026-09-24T12:00:40Z".parse().unwrap()
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

    /// R8, R20 (review #10): a store that exists but cannot be read is told with the result,
    /// and what was cached of it stays: the sessions listed, the tokens counted.
    #[test]
    fn a_store_that_cannot_be_read_is_told_and_keeps_what_was_cached() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let deps = deps(dir.path());
        let transcript = dir.path().join("max/projects/-w/s1.jsonl");
        let mut text = fs::read_to_string(&transcript).unwrap();
        text.push_str(
            "{\"type\":\"assistant\",\"timestamp\":\"2026-09-24T10:03:00Z\",\
             \"message\":{\"id\":\"msg_1\",\"model\":\"claude-test\",\"role\":\"assistant\",\
             \"content\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":40}}}\n",
        );
        fs::write(&transcript, text).unwrap();
        let indexed = |e: &Event| matches!(e, Event::IndexDone { .. });
        let counted = |e: &Event| matches!(e, Event::Stats { .. });
        collect(Effect::RefreshIndex, &deps, indexed);
        let complete = collect(Effect::Stats, &deps, counted);

        let store = dir.path().join("max/projects");
        let chmod = |mode| fs::set_permissions(&store, fs::Permissions::from_mode(mode)).unwrap();
        chmod(0o000);
        let index = collect(Effect::RefreshIndex, &deps, indexed);
        let stats = collect(Effect::Stats, &deps, counted);
        chmod(0o755);
        let said = |error: &Option<Marked>| {
            // R21: the directory is a path and the system's error a text of its own.
            let told = error.as_ref().expect("an incomplete refresh says so");
            assert_eq!(shape(told), "incomplete: cannot read <>: {}");
            let error = error.as_deref().expect("an incomplete refresh says so");
            assert!(
                error.starts_with("incomplete: cannot read ") && error.contains("max/projects: "),
                "{error}"
            );
        };
        let Some(Event::IndexDone { entries, error }) = index.last() else {
            panic!("{index:?}")
        };
        assert_eq!(entries.len(), 2);
        said(error);
        let (Some(Event::Stats { report, error }), Some(Event::Stats { report: before, .. })) =
            (stats.last(), complete.last())
        else {
            panic!("{stats:?}")
        };
        assert_eq!(report, before);
        assert_eq!(report.files, 2);
        said(error);

        // Readable again: nothing more to say.
        let index = collect(Effect::RefreshIndex, &deps, indexed);
        let stats = collect(Effect::Stats, &deps, counted);
        assert!(matches!(
            index.last(),
            Some(Event::IndexDone { error: None, .. })
        ));
        assert!(matches!(
            stats.last(),
            Some(Event::Stats { error: None, .. })
        ));
    }

    /// R8, R20 (review #10): a home that cannot be searched: its store cannot be resolved,
    /// which is told with the result, and what was cached of it stays.
    #[test]
    fn a_store_that_cannot_be_resolved_is_told_and_keeps_what_was_cached() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let deps = deps(dir.path());
        let indexed = |e: &Event| matches!(e, Event::IndexDone { .. });
        let counted = |e: &Event| matches!(e, Event::Stats { .. });
        collect(Effect::RefreshIndex, &deps, indexed);
        collect(Effect::Stats, &deps, counted);

        let home = dir.path().join("max");
        let chmod = |mode| fs::set_permissions(&home, fs::Permissions::from_mode(mode)).unwrap();
        chmod(0o000);
        let index = collect(Effect::RefreshIndex, &deps, indexed);
        let stats = collect(Effect::Stats, &deps, counted);
        chmod(0o755);
        let said = |error: &Option<Marked>| {
            let store = home.join("projects");
            // R21: the directory is a path and the system's error a text of its own.
            let told = error.as_ref().expect("an incomplete refresh says so");
            assert_eq!(shape(told), "incomplete: cannot read <>: {}");
            let path = store.display().to_string();
            assert_eq!(told.pieces().nth(1), Some((path.as_str(), Piece::Path)));
            let error = error.as_deref().expect("an incomplete refresh says so");
            assert!(
                error.starts_with(&format!("incomplete: cannot read {}: ", store.display())),
                "{error}"
            );
        };
        assert!(
            index.contains(&Event::Stores(vec![])),
            "no store is listed: {index:?}"
        );
        let Some(Event::IndexDone { entries, error }) = index.last() else {
            panic!("{index:?}")
        };
        assert_eq!(entries.len(), 2);
        said(error);
        let Some(Event::Stats { report, error }) = stats.last() else {
            panic!("{stats:?}")
        };
        assert_eq!(report.files, 2);
        said(error);

        // Resolved again: nothing more to say.
        let index = collect(Effect::RefreshIndex, &deps, indexed);
        assert!(matches!(
            index.last(),
            Some(Event::IndexDone { entries, error: None }) if entries.len() == 2
        ));
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
        // R21: the reason is the system's (it names the registry); what follows it is
        // remuda's and stays readable in private mode.
        assert_eq!(shape(&error), "prices: {} (built-in prices used)");
    }

    /// R21: an error of a cache is told in its pieces. What the system says (it names the
    /// file) is text from elsewhere, each error by itself, so what private mode hides in one
    /// does not reach the next; the index's is the system's whole.
    #[test]
    fn cache_errors_are_told_in_their_pieces() {
        let dir = tempfile::tempdir().unwrap();
        let deps = deps(dir.path());
        // Nothing can be written under a `state` that is a file.
        fs::write(&deps.state_dir, "").unwrap();
        fs::write(deps.listing.config(), "prices = 1\n").unwrap();

        let events = collect(Effect::Stats, &deps, |e| matches!(e, Event::Stats { .. }));
        let Some(Event::Stats {
            error: Some(error), ..
        }) = events.last()
        else {
            panic!("{events:?}")
        };
        assert_eq!(
            shape(error),
            "stats cache: {} · prices: {} (built-in prices used)"
        );
        let told: Vec<&str> = error
            .pieces()
            .filter(|(_, kind)| *kind == Piece::Text)
            .map(|(piece, _)| piece)
            .collect();
        assert_eq!(told.len(), 2, "{told:?}");
        assert!(told[1].contains("config.toml"), "{told:?}");

        let events = collect(Effect::RefreshIndex, &deps, |e| {
            matches!(e, Event::IndexDone { .. })
        });
        let Some(Event::IndexDone {
            error: Some(error), ..
        }) = events.last()
        else {
            panic!("{events:?}")
        };
        assert_eq!(shape(error), "index cache: {}");
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
                    .words(" does not exist")
            )
        );
        assert_eq!(
            check(Some(file.clone())),
            Some(
                Marked::default()
                    .path(file.display())
                    .words(" is not a directory")
            )
        );
    }

    /// R16, R21: a launch refused for its account, or for a registry that cannot be read, is
    /// told in pieces. That an account is no longer registered (or is, with another home) is
    /// remuda's own words; why the registry cannot be read is the system's text, after
    /// remuda's words when a resume cannot be confirmed for it.
    #[test]
    fn a_refused_launch_says_why_in_pieces() {
        let dir = tempfile::tempdir().unwrap();
        let deps = deps(dir.path());
        let id = "0badf00d-0000-4000-8000-000000000000";
        let check = |account: Account, args: &[&str]| {
            let request = LaunchRequest {
                account,
                args: args.iter().map(|a| a.to_string()).collect(),
                cwd: None,
                what: "x".into(),
            };
            let check = Effect::CheckLaunch { check: 7, request };
            let events = collect(check, &deps, |e| matches!(e, Event::LaunchChecked { .. }));
            let Some(Event::LaunchChecked { error, .. }) = events.last() else {
                panic!("{events:?}")
            };
            error.clone()
        };
        let listed = max(&deps);
        let gone = Account {
            name: "team".into(),
            ..listed.clone()
        };
        let moved = Account {
            home: Home::Path(dir.path().join("elsewhere").display().to_string()),
            ..listed.clone()
        };
        for (account, said) in [
            (gone, "team is no longer registered"),
            (moved, "max is now registered with another home"),
        ] {
            let error = check(account, &[]).expect("refused");
            assert_eq!(error.pieces().collect::<Vec<_>>(), [(said, Piece::Words)]);
        }

        fs::write(deps.listing.config(), "not toml [").unwrap();
        // A new session is refused when it starts; a resume in place cannot be confirmed.
        assert_eq!(check(listed.clone(), &[]), None);
        let error = check(listed, &["--resume", id]).expect("refused");
        assert_eq!(
            shape(&error),
            "cannot confirm that session 0badf00d is not running: cannot read the registry: {}"
        );
        let why = error.pieces().last().expect("a reason").0;
        assert!(
            why.contains(&deps.listing.config().display().to_string()),
            "{why}"
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
                result: Err("`claude` not found on PATH".into()),
                answered_at: answered(),
            }]
        );
    }

    /// Identities and cached usage are read for the accounts asked, not for every account:
    /// one that is still being asked is not asked twice.
    #[test]
    fn identities_and_cached_usage_answer_for_the_accounts_asked() {
        let dir = tempfile::tempdir().unwrap();
        let other = Account {
            provider: CLAUDE,
            name: "other".into(),
            home: Home::Path(dir.path().join("other").display().to_string()),
        };
        // The registry lists `default`, `max` and `other`; only `other` is asked.
        let registered = format!(
            "[[account]]\nprovider = \"claude\"\nname = \"other\"\nhome = \"{}\"\n",
            other.home
        );
        let deps = deps_with(dir.path(), &registered);
        let events = collect(Effect::Identities(vec![other.clone()]), &deps, |_| true);
        assert!(
            matches!(events.as_slice(), [Event::Identity { account, .. }] if *account == other),
            "{events:?}"
        );
        let (tx, rx) = mpsc::channel();
        spawn(Effect::CachedUsage(vec![other.clone()]), &deps, &tx);
        drop(tx);
        let events: Vec<Event> = rx.iter().collect();
        assert!(
            matches!(events.as_slice(), [Event::CachedUsage { account, .. }] if *account == other),
            "{events:?}"
        );
    }

    /// R16: identities and cached usage are asked of the accounts the app names that the
    /// registry still lists: it is read first, the change is told, and an account that left
    /// it since the app asked gets no query in its old home.
    #[test]
    fn identities_and_cached_usage_are_asked_of_accounts_still_listed() {
        let dir = tempfile::tempdir().unwrap();
        let deps = deps(dir.path());
        let (default, max) = (Account::default_for(CLAUDE), max(&deps));
        // `remuda remove max` in another terminal; the app still has its row.
        fs::write(deps.listing.config(), "").unwrap();
        let asked = vec![max, default.clone()];
        let answers = |effect: Effect| -> Vec<Event> {
            let (tx, rx) = mpsc::channel();
            spawn(effect, &deps, &tx);
            drop(tx);
            rx.iter().collect()
        };
        let events = answers(Effect::CachedUsage(asked.clone()));
        let [Event::Accounts(listed), Event::CachedUsage { account, .. }] = events.as_slice()
        else {
            panic!("the change, then the one account still listed: {events:?}")
        };
        assert_eq!((listed, account), (&vec![default.clone()], &default));
        // The app has been told: the identities are asked of the same account, and no more.
        let events = answers(Effect::Identities(asked));
        assert!(
            matches!(events.as_slice(), [Event::Identity { account, .. }] if *account == default),
            "{events:?}"
        );
    }

    /// R22: an account's configuration is read in the background and comes back with its
    /// request number; a codex account has none to list.
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
        // R21: the registry is a path, the reason is the system's, the rest remuda's words.
        assert_eq!(
            shape(&view.problems[0]),
            "cannot read <>: {}; shared configuration unknown"
        );
        let config = deps.listing.config().display().to_string();
        assert_eq!(
            view.problems[0].pieces().nth(1),
            Some((config.as_str(), Piece::Path))
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

    /// R22: the configuration asked for again (`r`, or the pane closed and opened) while a read
    /// of it is out. That read may have seen the files before they changed, and its answer
    /// may only reach the app after the key: the files are read once more when it answers,
    /// and what changed is shown.
    #[test]
    fn configuration_asked_again_during_a_read_shows_what_changed_meanwhile() {
        use crate::tui::app::{App, Key, update};
        let p = Key::Char('p');
        for again in [&[Key::Char('r')][..], &[p, p, p][..]] {
            let dir = tempfile::tempdir().unwrap();
            let deps = deps(dir.path());
            let max = max(&deps);
            let settings = dir.path().join("max/settings.json");
            fs::write(&settings, r#"{"model":"old-model"}"#).unwrap();
            let mut app = App::new(vec![max], TimeZone::UTC, None, jiff::Timestamp::now());
            update(&mut app, Event::Resize(100, 30));
            // The worker's answers to the configuration reads among `effects`.
            let read = |effects: Vec<Effect>| -> Vec<Event> {
                effects
                    .into_iter()
                    .filter(|e| matches!(e, Effect::Config { .. }))
                    .map(|e| collect(e, &deps, |_| true).remove(0))
                    .collect()
            };
            let model = |app: &App| {
                let view = app.config.loaded.clone()?.ok()?;
                view.own_settings.model
            };

            // The first read has answered, but the key reaches the app before the answer.
            let mut old = read(update(&mut app, Event::Key(p)));
            assert_eq!(old.len(), 1);
            fs::write(&settings, r#"{"model":"new-model"}"#).unwrap();
            let fx: Vec<Effect> = again
                .iter()
                .flat_map(|key| update(&mut app, Event::Key(*key)))
                .collect();
            assert_eq!(read(fx), [], "one read at a time");
            let answers = read(update(&mut app, old.remove(0)));
            assert_eq!(model(&app).as_deref(), Some("old-model"));
            assert_eq!(answers.len(), 1, "read once more");
            for answer in answers {
                assert_eq!(update(&mut app, answer), []);
            }
            assert_eq!(model(&app).as_deref(), Some("new-model"));
        }
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
                result: Err("`codex` not found on PATH".into()),
                answered_at: answered(),
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
                }),
                // Stamped by the worker when the query answered, not by whoever reads the
                // event later.
                answered_at: answered(),
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
                result: Err("`claude` not found on PATH".into()),
                answered_at: answered(),
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
