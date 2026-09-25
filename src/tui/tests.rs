//! `update` driven directly, and `render` into ratatui's `TestBackend`: no terminal.

use std::path::{Path, PathBuf};

use jiff::Timestamp;
use jiff::tz::TimeZone;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::app::{
    App, Confirm, Effect, Event, Exit, Form, FormKind, Key, LaunchRequest, Level, Mode, Notice,
    Overlay, Pick, PickFor, ResumeCodex, View, update,
};
use super::render;
use crate::attribution::Attribution;
use crate::checks::Check;
use crate::identity::Identity;
use crate::index::{Entry, Store};
use crate::live::{Control, LiveSession, Source};
use crate::registry::{Account, CLAUDE, CODEX, Home};
use crate::stats::{self, Cost, ModelRow, Period, Report, Section, Table, Tokens};
use crate::transcript::{Message, Role};
use crate::usage::{CachedUsage, LiveResult, LiveUsage, Resets, UsageRow};

const NOW: &str = "2026-09-24T12:00:00Z";

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

fn account(name: &str) -> Account {
    if name == "default" {
        return Account::default_for(CLAUDE);
    }
    Account {
        provider: CLAUDE,
        name: name.into(),
        home: Home::Path(format!("/h/{name}")),
    }
}

fn app() -> App {
    let mut app = App::new(
        vec![account("default"), account("max"), account("team")],
        TimeZone::UTC,
        Some("/Users/you".into()),
        ts(NOW),
    );
    update(&mut app, Event::Resize(80, 24));
    app.cwd = Some(PathBuf::from("/Users/you/space/remuda"));
    app
}

/// An entry last active `minute` minutes past 10:00 on Sep 24.
fn entry(id: &str, title: &str, minute: u32) -> Entry {
    Entry {
        provider: CLAUDE,
        session_id: id.into(),
        path: PathBuf::from(format!("/s/-w/{id}.jsonl")),
        store: PathBuf::from("/s"),
        size: 10,
        mtime_ns: 0,
        ino: 1,
        scanned_offset: 10,
        gap: false,
        title: None,
        first_user_text: Some(title.into()),
        cwd_first: Some("/Users/you/space/remuda".into()),
        cwd_last: Some("/Users/you/space/remuda".into()),
        ts_first: None,
        ts_last: Some(format!("2026-09-24T10:{minute:02}:00Z")),
        entrypoint: Some("cli".into()),
        source: None,
        originator: None,
    }
}

fn path(id: &str) -> PathBuf {
    PathBuf::from(format!("/s/-w/{id}.jsonl"))
}

fn live(account: &str, pid: u32, session: Option<&str>) -> LiveSession {
    LiveSession {
        account: account.into(),
        pid: Some(pid),
        short_id: None,
        cwd: Some("/Users/you/space/remuda".into()),
        kind: Some("interactive".into()),
        started_at: Some(ts("2026-09-24T11:57:00Z").as_millisecond()),
        session_id: session.map(str::to_string),
        name: Some("fix index".into()),
        status: Some("busy".into()),
        source: Source::Agents,
    }
}

fn row(label: &str, percent: f64, severity: Option<&str>, resets: Option<&str>) -> UsageRow {
    UsageRow {
        label: label.into(),
        percent,
        severity: severity.map(str::to_string),
        resets: resets.map(|r| Resets::At(ts(r))),
    }
}

fn cached(rows: Vec<UsageRow>) -> Result<CachedUsage, String> {
    Ok(CachedUsage {
        fetched_at: Some(ts("2026-09-24T11:55:00Z")),
        rows,
    })
}

fn logged_in(email: &str) -> Identity {
    Identity::LoggedIn {
        email: Some(email.into()),
        org: Some("Org".into()),
        plan: Some("max".into()),
        method: None,
        cached: false,
    }
}

fn keys(app: &mut App, keys: &[Key]) -> Vec<Effect> {
    keys.iter()
        .flat_map(|k| update(app, Event::Key(*k)))
        .collect()
}

fn type_str(app: &mut App, s: &str) -> Vec<Effect> {
    s.chars()
        .flat_map(|c| update(app, Event::Key(Key::Char(c))))
        .collect()
}

fn history_ids(app: &App) -> Vec<String> {
    app.history
        .rows
        .iter()
        .map(|p| p.file_stem().unwrap().to_str().unwrap().to_string())
        .collect()
}

fn tick(app: &mut App, secs: i64) -> Vec<Effect> {
    let now = app.now + jiff::SignedDuration::from_millis(secs * 1000);
    update(app, Event::Tick(now))
}

// ---- update ------------------------------------------------------------------------

#[test]
fn start_requests_everything_in_the_background() {
    let mut app = app();
    assert_eq!(
        app.start(),
        [
            Effect::RefreshIndex,
            Effect::Identities,
            Effect::CachedUsage,
            Effect::Live,
            Effect::Attribution,
            Effect::Checks
        ]
    );
    // `r` while everything is still running does not stack it.
    assert_eq!(keys(&mut app, &[Key::Char('r')]), []);
    update(
        &mut app,
        Event::IndexDone {
            entries: vec![],
            error: None,
        },
    );
    update(&mut app, Event::Live(vec![]));
    finish_accounts(&mut app);
    update(&mut app, Event::Attribution(Attribution::default()));
    update(&mut app, Event::Checks(vec![]));
    assert_eq!(
        keys(&mut app, &[Key::Char('r')]),
        [
            Effect::RefreshIndex,
            Effect::Identities,
            Effect::CachedUsage,
            Effect::Live,
            Effect::Attribution,
            Effect::Checks
        ]
    );
}

/// Identity and cached usage for every account of [`app`].
fn finish_accounts(app: &mut App) {
    let accounts: Vec<Account> = app.accounts.iter().map(|a| a.account.clone()).collect();
    for account in accounts {
        update(
            app,
            Event::Identity {
                account: account.clone(),
                identity: Identity::NotLoggedIn,
            },
        );
        update(
            app,
            Event::CachedUsage {
                account,
                result: Err("no cache".into()),
            },
        );
    }
}

#[test]
fn refresh_starts_each_kind_of_work_once_until_it_finishes() {
    let mut app = app();
    app.start();
    let r = [Key::Char('r'); 5];
    assert_eq!(keys(&mut app, &r), []);

    // Identities finish one account at a time: still running until the last one.
    for name in ["default", "max"] {
        update(
            &mut app,
            Event::Identity {
                account: account(name),
                identity: Identity::NotLoggedIn,
            },
        );
    }
    assert_eq!(keys(&mut app, &r), []);
    update(
        &mut app,
        Event::Identity {
            account: account("team"),
            identity: Identity::NotLoggedIn,
        },
    );
    assert_eq!(keys(&mut app, &r), [Effect::Identities]);

    for name in ["default", "max", "team"] {
        update(
            &mut app,
            Event::CachedUsage {
                account: account(name),
                result: Err("no cache".into()),
            },
        );
    }
    assert_eq!(keys(&mut app, &r), [Effect::CachedUsage]);

    update(&mut app, Event::Attribution(Attribution::default()));
    assert_eq!(keys(&mut app, &r), [Effect::Attribution]);
    update(&mut app, Event::Checks(vec![]));
    assert_eq!(keys(&mut app, &r), [Effect::Checks]);
}

#[test]
fn index_rows_arrive_before_the_refresh_ends() {
    let mut app = app();
    app.start();
    update(&mut app, Event::IndexLoaded(vec![entry("a", "old", 1)]));
    assert!(app.index_loaded);
    assert_eq!(history_ids(&app), ["a"]);
    update(
        &mut app,
        Event::IndexProgress {
            done: 1,
            total: 3,
            entries: vec![entry("b", "newer", 5)],
        },
    );
    assert_eq!(app.indexing, Some((1, 3)));
    assert_eq!(history_ids(&app), ["b", "a"]);
    // The final index replaces everything: `a` vanished, `c` is new.
    update(
        &mut app,
        Event::IndexDone {
            entries: vec![entry("b", "newer", 5), entry("c", "newest", 9)],
            error: Some("disk full".into()),
        },
    );
    assert_eq!(history_ids(&app), ["c", "b"]);
    assert_eq!(app.indexing, None);
    assert!(!app.index_in_flight);
    assert_eq!(app.index_refreshed, Some(ts(NOW)));
    assert_eq!(app.index_error.as_deref(), Some("disk full"));
}

#[test]
fn selection_follows_its_session_when_rows_move() {
    let mut app = app();
    update(
        &mut app,
        Event::IndexLoaded(vec![entry("a", "a", 1), entry("b", "b", 2)]),
    );
    keys(&mut app, &[Key::Char('3'), Key::Char('j')]);
    assert_eq!(app.selected_entry().unwrap().session_id, "a");
    update(
        &mut app,
        Event::IndexProgress {
            done: 1,
            total: 1,
            entries: vec![entry("c", "c", 9)],
        },
    );
    assert_eq!(history_ids(&app), ["c", "b", "a"]);
    assert_eq!(app.selected_entry().unwrap().session_id, "a");
}

fn noisy() -> Vec<Entry> {
    let teammate = entry("tm", "<teammate-message teammate_id=\"x\">do it", 8);
    let mut sdk = entry("sdk", "batch job", 7);
    sdk.entrypoint = Some("sdk-cli".into());
    vec![teammate, sdk, entry("real", "real work", 6)]
}

#[test]
fn teammate_and_sdk_sessions_are_hidden_until_show_all() {
    let mut app = app();
    update(&mut app, Event::IndexLoaded(noisy()));
    keys(&mut app, &[Key::Char('3')]);
    assert_eq!(history_ids(&app), ["real"]);
    assert_eq!(app.unfiltered_count(), 1);
    keys(&mut app, &[Key::Char('a')]);
    assert_eq!(history_ids(&app), ["tm", "sdk", "real"]);
    keys(&mut app, &[Key::Char('a')]);
    assert_eq!(history_ids(&app), ["real"]);
    // `a` belongs to history only.
    keys(&mut app, &[Key::Char('1'), Key::Char('a'), Key::Char('3')]);
    assert!(!app.history.show_all);
}

#[test]
fn search_prompt_takes_every_printable_key() {
    let mut app = app();
    update(
        &mut app,
        Event::IndexLoaded(vec![
            entry("a", "fix the index scan", 1),
            entry("b", "write docs", 2),
            entry("c", "quick question about jaq", 3),
        ]),
    );
    keys(&mut app, &[Key::Char('3'), Key::Char('/')]);
    assert!(app.history.searching);
    // q, j, a, u, r, ? and digits are text here, not commands.
    assert_eq!(type_str(&mut app, "qjau r?1"), []);
    assert_eq!(app.history.query, "qjau r?1");
    assert_eq!(app.view, View::History);
    assert!(!app.help);
    for _ in 0..8 {
        keys(&mut app, &[Key::Backspace]);
    }
    type_str(&mut app, "jaq");
    assert_eq!(history_ids(&app), ["c"]);
    // Enter keeps the filter; keys are commands again.
    keys(&mut app, &[Key::Enter]);
    assert!(!app.history.searching);
    assert_eq!(history_ids(&app), ["c"]);
    assert_eq!(keys(&mut app, &[Key::Char('q')]), [Effect::Quit]);
    // Esc outside the prompt clears the filter.
    keys(&mut app, &[Key::Esc]);
    assert_eq!(history_ids(&app), ["c", "b", "a"]);
    // Esc inside the prompt clears and closes it.
    keys(&mut app, &[Key::Char('/')]);
    type_str(&mut app, "docs");
    assert_eq!(history_ids(&app), ["b"]);
    keys(&mut app, &[Key::Esc]);
    assert!(!app.history.searching);
    assert_eq!(app.history.query, "");
    assert_eq!(history_ids(&app), ["c", "b", "a"]);
    // Ctrl-C quits even from the prompt.
    keys(&mut app, &[Key::Char('/')]);
    assert_eq!(keys(&mut app, &[Key::Ctrl('c')]), [Effect::Quit]);
}

#[test]
fn search_matches_accounts_and_cwd() {
    let mut app = app();
    let mut other = entry("b", "unrelated", 2);
    other.cwd_last = Some("/w/website".into());
    update(
        &mut app,
        Event::IndexLoaded(vec![entry("a", "one", 1), other]),
    );
    let mut attribution = Attribution::default();
    attribution.add("a", "claude:team");
    update(&mut app, Event::Attribution(attribution));
    keys(&mut app, &[Key::Char('3'), Key::Char('/')]);
    type_str(&mut app, "team");
    assert_eq!(history_ids(&app), ["a"]);
    keys(&mut app, &[Key::Esc, Key::Char('/')]);
    type_str(&mut app, "website");
    assert_eq!(history_ids(&app), ["b"]);
}

#[test]
fn preview_loads_after_the_selection_settles() {
    let mut app = app();
    app.start();
    update(
        &mut app,
        Event::IndexLoaded(vec![entry("a", "a", 1), entry("b", "b", 2)]),
    );
    keys(&mut app, &[Key::Char('3')]);
    assert_eq!(app.preview.target, Some(path("b")));
    // One tick is not enough; moving starts the wait over.
    assert_eq!(tick(&mut app, 0), []);
    keys(&mut app, &[Key::Char('j')]);
    assert_eq!(tick(&mut app, 0), []);
    assert_eq!(tick(&mut app, 0), [Effect::Preview(path("a"), CLAUDE)]);
    // Loading: no second request.
    assert_eq!(tick(&mut app, 0), []);
    // A result for a path that is no longer wanted is dropped.
    keys(&mut app, &[Key::Char('k')]);
    let msg = |t: &str| Message {
        role: Role::User,
        text: t.into(),
    };
    update(
        &mut app,
        Event::Preview {
            path: path("a"),
            result: Ok(vec![msg("from a")]),
        },
    );
    assert_eq!(app.preview.loaded, None);
    tick(&mut app, 0);
    assert_eq!(tick(&mut app, 0), [Effect::Preview(path("b"), CLAUDE)]);
    update(
        &mut app,
        Event::Preview {
            path: path("b"),
            result: Ok(vec![msg("from b")]),
        },
    );
    assert_eq!(
        app.preview.loaded,
        Some((path("b"), Ok(vec![msg("from b")])))
    );
    // Loaded: nothing more to do.
    assert_eq!(tick(&mut app, 0), []);
}

#[test]
fn live_sessions_refresh_every_five_seconds() {
    let mut app = app();
    assert_eq!(
        app.start().iter().filter(|e| **e == Effect::Live).count(),
        1
    );
    // Still in flight: no second collection, however long it takes.
    assert!(!tick(&mut app, 6).contains(&Effect::Live));
    update(
        &mut app,
        Event::Live(vec![live("claude:max", 7, Some("a"))]),
    );
    // Five seconds after it finished.
    assert!(!tick(&mut app, 4).contains(&Effect::Live));
    assert!(tick(&mut app, 1).contains(&Effect::Live));
    assert!(!tick(&mut app, 10).contains(&Effect::Live));
}

#[test]
fn live_sessions_feed_attribution_and_find_their_transcript() {
    let mut app = app();
    update(
        &mut app,
        Event::IndexLoaded(vec![entry("a", "a", 1), entry("b", "b", 2)]),
    );
    let mut base = Attribution::default();
    base.add("a", "claude:default");
    update(&mut app, Event::Attribution(base));
    update(
        &mut app,
        Event::Live(vec![
            live("claude:max", 7, Some("a")),
            live("claude:team", 8, Some("not-indexed")),
        ]),
    );
    assert_eq!(
        app.attribution.accounts("a"),
        ["claude:default", "claude:max"]
    );
    keys(&mut app, &[Key::Char('2')]);
    assert_eq!(app.preview.target, Some(path("a")));
    keys(&mut app, &[Key::Char('j')]);
    assert_eq!(app.preview.target, None);
    // A later collection keeps the same session selected even if it moved.
    update(
        &mut app,
        Event::Live(vec![
            live("claude:team", 8, Some("not-indexed")),
            live("claude:max", 7, Some("a")),
        ]),
    );
    assert_eq!(app.selected_live().unwrap().pid, Some(8));
    // Only the live part is replaced; the base attribution stays.
    update(&mut app, Event::Live(vec![]));
    assert_eq!(app.attribution.accounts("a"), ["claude:default"]);
}

#[test]
fn live_usage_is_per_account_and_replaces_the_cache() {
    let mut app = app();
    update(
        &mut app,
        Event::CachedUsage {
            account: account("max"),
            result: cached(vec![row("Session", 10.0, None, None)]),
        },
    );
    assert_eq!(
        keys(&mut app, &[Key::Char('u')]),
        [Effect::LiveUsage(vec![
            account("default"),
            account("max"),
            account("team")
        ])]
    );
    assert!(app.accounts.iter().all(|a| a.live_pending));
    update(
        &mut app,
        Event::LiveUsage {
            account: account("max"),
            result: Ok(LiveUsage::Rows(vec![row("Session", 55.0, None, None)]).into()),
        },
    );
    // Only the accounts that finished can be asked again.
    assert_eq!(
        keys(&mut app, &[Key::Char('u')]),
        [Effect::LiveUsage(vec![account("max")])]
    );
    update(
        &mut app,
        Event::LiveUsage {
            account: account("max"),
            result: Ok(LiveUsage::Rows(vec![row("Session", 56.0, None, None)]).into()),
        },
    );
    assert_eq!(app.accounts[1].rows()[0].percent, 56.0);
    update(
        &mut app,
        Event::LiveUsage {
            account: account("team"),
            result: Err("timed out after 90s".into()),
        },
    );
    update(
        &mut app,
        Event::LiveUsage {
            account: account("default"),
            result: Ok(LiveUsage::Unrecognized("??".into()).into()),
        },
    );
    assert_eq!(
        app.accounts[0].live,
        Some(Err("output not recognized".into()))
    );
    assert!(!app.accounts[2].live_pending);
    assert!(app.accounts[2].rows().is_empty());
}

/// Results name their account: when the list changes during a query, a late result lands on
/// its account's new row, and one for an account no longer listed is dropped.
#[test]
fn late_results_follow_their_account_when_rows_move() {
    let mut app = app();
    keys(&mut app, &[Key::Char('u')]);
    // max is removed: team moves up to row 1.
    update(
        &mut app,
        Event::Accounts(vec![account("default"), account("team")]),
    );
    assert_eq!(app.accounts[1].account, account("team"));
    update(
        &mut app,
        Event::LiveUsage {
            account: account("team"),
            result: Ok(LiveUsage::Rows(vec![row("Session", 42.0, None, None)]).into()),
        },
    );
    assert_eq!(app.accounts[1].rows()[0].percent, 42.0);
    assert!(!app.accounts[1].live_pending);
    assert!(app.accounts[0].live.is_none() && app.accounts[0].live_pending);

    let before = app.accounts.clone();
    update(
        &mut app,
        Event::LiveUsage {
            account: account("max"),
            result: Ok(LiveUsage::Rows(vec![row("Session", 99.0, None, None)]).into()),
        },
    );
    update(
        &mut app,
        Event::Identity {
            account: account("max"),
            identity: logged_in("max@example.com"),
        },
    );
    update(
        &mut app,
        Event::CachedUsage {
            account: account("max"),
            result: cached(vec![row("Session", 99.0, None, None)]),
        },
    );
    assert_eq!(app.accounts, before);
}

#[test]
fn quit_help_and_view_switching() {
    let mut app = app();
    assert_eq!(keys(&mut app, &[Key::Char('q')]), [Effect::Quit]);
    assert_eq!(keys(&mut app, &[Key::Ctrl('c')]), [Effect::Quit]);
    keys(&mut app, &[Key::Char('?')]);
    assert!(app.help);
    // Any key closes the help, and does nothing else.
    assert_eq!(keys(&mut app, &[Key::Char('q')]), []);
    assert!(!app.help);
    let mut seen = vec![app.view];
    for k in [Key::Tab, Key::Tab, Key::Tab, Key::Tab, Key::BackTab] {
        keys(&mut app, &[k]);
        seen.push(app.view);
    }
    assert_eq!(
        seen,
        [
            View::Accounts,
            View::Live,
            View::History,
            View::Stats,
            View::Accounts,
            View::Stats
        ]
    );
    keys(&mut app, &[Key::Char('2')]);
    assert_eq!(app.view, View::Live);
    keys(&mut app, &[Key::Char('4')]);
    assert_eq!(app.view, View::Stats);
    keys(&mut app, &[Key::Char('1')]);
    assert_eq!(app.view, View::Accounts);
}

#[test]
fn navigation_keys_move_and_scroll() {
    let mut app = app();
    let entries: Vec<Entry> = (0..40)
        .map(|i| entry(&format!("s{i:02}"), "t", i))
        .collect();
    update(&mut app, Event::IndexLoaded(entries));
    keys(&mut app, &[Key::Char('3')]);
    let height = render::list_height(&app, View::History);
    assert!(height > 3 && height < 40, "{height}");
    let sel = |app: &App| app.history.list.selected;
    keys(&mut app, &[Key::Char('j'), Key::Down]);
    assert_eq!(sel(&app), 2);
    keys(&mut app, &[Key::Char('k')]);
    assert_eq!(sel(&app), 1);
    keys(&mut app, &[Key::Char('G')]);
    assert_eq!(sel(&app), 39);
    assert_eq!(app.history.list.offset, 40 - height);
    keys(&mut app, &[Key::Char('g')]);
    assert_eq!((sel(&app), app.history.list.offset), (0, 0));
    keys(&mut app, &[Key::PageDown]);
    assert_eq!(sel(&app), height);
    keys(&mut app, &[Key::PageUp, Key::PageUp]);
    assert_eq!(sel(&app), 0);
    keys(&mut app, &[Key::End]);
    assert_eq!(sel(&app), 39);
    keys(&mut app, &[Key::Home, Key::Up]);
    assert_eq!(sel(&app), 0);
}

#[test]
fn p_or_space_expands_the_preview_and_scrolls_it() {
    let mut app = app();
    update(&mut app, Event::IndexLoaded(vec![entry("a", "a", 1)]));
    keys(&mut app, &[Key::Char('3')]);
    tick(&mut app, 0);
    tick(&mut app, 0);
    let long: Vec<Message> = (0..20)
        .map(|i| Message {
            role: Role::Assistant,
            text: format!("line {i}"),
        })
        .collect();
    update(
        &mut app,
        Event::Preview {
            path: path("a"),
            result: Ok(long),
        },
    );
    keys(&mut app, &[Key::Char(' ')]);
    assert!(app.preview.expanded);
    keys(&mut app, &[Key::Char('p')]);
    assert!(!app.preview.expanded);
    keys(&mut app, &[Key::Char('p')]);
    assert!(app.preview.expanded);
    keys(&mut app, &[Key::Char('k'), Key::Char('k'), Key::Char('j')]);
    assert_eq!(app.preview.scroll, 1);
    // Cannot scroll past the top.
    keys(&mut app, &[Key::Char('g')]);
    let top = app.preview.scroll;
    keys(&mut app, &[Key::Char('k')]);
    assert_eq!(app.preview.scroll, top);
    keys(&mut app, &[Key::Esc]);
    assert!(!app.preview.expanded);
    assert_eq!(app.preview.scroll, 0);
    // Moving still works after collapsing.
    assert_eq!(app.history.list.selected, 0);
}

// ---- launching -------------------------------------------------------------------

fn request(account_name: &str, args: &[&str], cwd: Option<&str>, what: &str) -> LaunchRequest {
    LaunchRequest {
        account: account(account_name),
        args: args.iter().map(|a| a.to_string()).collect(),
        cwd: cwd.map(PathBuf::from),
        what: what.into(),
    }
}

/// Everything [`App::start`] requested has finished.
fn idle_app() -> App {
    let mut app = app();
    app.start();
    update(
        &mut app,
        Event::IndexDone {
            entries: vec![],
            error: None,
        },
    );
    update(&mut app, Event::Live(vec![]));
    finish_accounts(&mut app);
    update(&mut app, Event::Attribution(Attribution::default()));
    update(&mut app, Event::Checks(vec![]));
    app
}

/// [`Effect::CheckLaunch`] of `request` under the number of the last check issued.
fn check_of(app: &App, request: LaunchRequest) -> Effect {
    Effect::CheckLaunch {
        check: app.launch_checks,
        request,
    }
}

/// The worker's answer to the last check issued.
fn answer(app: &mut App, request: LaunchRequest, error: Option<String>) -> Vec<Effect> {
    let check = app.launch_checks;
    update(
        app,
        Event::LaunchChecked {
            check,
            request,
            error,
        },
    )
}

fn notice(app: &App) -> Option<(&str, Level)> {
    app.notice.as_ref().map(|n| (n.text.as_str(), n.level))
}

#[test]
fn a_finished_launch_reports_its_exit_and_refreshes_sessions() {
    let mut app = idle_app();
    let resume = request("max", &["--resume", "a"], Some("/w"), "resume a as max");
    let fx = update(
        &mut app,
        Event::Launched {
            request: resume.clone(),
            result: Ok(Exit::Code(0)),
            warnings: vec![],
        },
    );
    // The index, live sessions and attribution (launch log) are read again (R16).
    assert_eq!(
        fx,
        [Effect::RefreshIndex, Effect::Live, Effect::Attribution]
    );
    assert_eq!(
        notice(&app),
        Some(("resume a as max: claude exited 0", Level::Info))
    );

    let mut app = idle_app();
    update(
        &mut app,
        Event::Launched {
            request: resume.clone(),
            result: Ok(Exit::Code(3)),
            warnings: vec!["cannot write launch log /x: denied".into()],
        },
    );
    assert_eq!(
        notice(&app),
        Some((
            "resume a as max: claude exited 3 · cannot write launch log /x: denied",
            Level::Warn
        ))
    );
    update(
        &mut app,
        Event::Launched {
            request: resume.clone(),
            result: Ok(Exit::Signal(9)),
            warnings: vec![],
        },
    );
    assert_eq!(
        notice(&app),
        Some((
            "resume a as max: claude was killed by signal 9",
            Level::Warn
        ))
    );
    update(
        &mut app,
        Event::Launched {
            request: resume,
            result: Err("`claude` not found on PATH".into()),
            warnings: vec![],
        },
    );
    assert_eq!(
        notice(&app),
        Some((
            "resume a as max: cannot run claude: `claude` not found on PATH",
            Level::Error
        ))
    );
    // The next key press clears the notice.
    keys(&mut app, &[Key::Char('j')]);
    assert_eq!(notice(&app), None);
}

#[test]
fn a_refresh_already_running_when_a_launch_ends_runs_again() {
    let mut app = app();
    app.start();
    let fx = update(
        &mut app,
        Event::Launched {
            request: request("max", &[], Some("/w"), "new session as max"),
            result: Ok(Exit::Code(0)),
            warnings: vec![],
        },
    );
    // Still running from the start: nothing stacks up...
    assert_eq!(fx, []);
    // ... but each runs once more when it finishes, to see the new transcript and log line.
    let done = Event::IndexDone {
        entries: vec![],
        error: None,
    };
    assert_eq!(update(&mut app, done.clone()), [Effect::RefreshIndex]);
    assert_eq!(update(&mut app, done), []);
    assert_eq!(
        update(&mut app, Event::Attribution(Attribution::default())),
        [Effect::Attribution]
    );
    assert_eq!(
        update(&mut app, Event::Attribution(Attribution::default())),
        []
    );
    assert_eq!(update(&mut app, Event::Live(vec![])), [Effect::Live]);
    assert_eq!(update(&mut app, Event::Live(vec![])), []);
}

// ---- resume and fork (R16) ---------------------------------------------------------

/// Session ids are UUIDs (R16 refuses anything else); `short_id` shows `aaaaaaaa`.
const A: &str = "aaaaaaaa-0000-4000-8000-000000000000";
const B: &str = "bbbbbbbb-0000-4000-8000-000000000000";

const CWD: &str = "/Users/you/space/remuda";

fn background(account: &str, short: &str, session: &str, state: &str) -> LiveSession {
    LiveSession {
        account: account.into(),
        pid: None,
        short_id: Some(short.into()),
        cwd: Some("/w/bg".into()),
        kind: Some("background".into()),
        started_at: Some(ts("2026-09-24T11:50:00Z").as_millisecond()),
        session_id: Some(session.into()),
        name: Some("bg task".into()),
        status: Some(state.into()),
        source: Source::Agents,
    }
}

/// `default` and `max` share the store `/s` (where every [`entry`] lives); `team` has `/t`.
fn stores() -> Vec<Store> {
    vec![
        Store {
            provider: CLAUDE,
            path: PathBuf::from("/s"),
            accounts: vec!["claude:default".into(), "claude:max".into()],
            thread_names: vec![],
        },
        Store {
            provider: CLAUDE,
            path: PathBuf::from("/t"),
            accounts: vec!["claude:team".into()],
            thread_names: vec![],
        },
    ]
}

/// History with sessions `a` (older) and `b` (newer), stores known, `a` selected, and `a`
/// attributed to `owners`.
fn history_with(owners: &[&str]) -> App {
    let mut app = idle_app();
    update(
        &mut app,
        Event::IndexLoaded(vec![entry(A, "a", 1), entry(B, "b", 2)]),
    );
    update(&mut app, Event::Stores(stores()));
    let mut attribution = Attribution::default();
    for owner in owners {
        attribution.add(A, &format!("claude:{owner}"));
    }
    update(&mut app, Event::Attribution(attribution));
    keys(&mut app, &[Key::Char('3'), Key::Char('j')]);
    assert_eq!(app.selected_entry().unwrap().session_id, A);
    app
}

fn resume_a(account_name: &str) -> LaunchRequest {
    request(
        account_name,
        &["--resume", A],
        Some(CWD),
        &format!("resume aaaaaaaa as {account_name}"),
    )
}

fn fork_a(account_name: &str) -> LaunchRequest {
    request(
        account_name,
        &["--resume", A, "--fork-session"],
        Some(CWD),
        &format!("fork aaaaaaaa as {account_name}"),
    )
}

#[test]
fn enter_resumes_with_the_one_attributed_account_in_cwd_last() {
    let mut app = history_with(&["max"]);
    // The directory is checked first (the file system is the workers' business) ...
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [check_of(&app, resume_a("max"))]
    );
    // ... and only then claude runs.
    assert_eq!(
        answer(&mut app, resume_a("max"), None),
        [Effect::Launch(resume_a("max"))]
    );
    // A second answer for the same check does nothing.
    assert_eq!(answer(&mut app, resume_a("max"), None), []);
}

#[test]
fn f_forks_the_selected_session() {
    let mut app = history_with(&["max"]);
    assert_eq!(
        keys(&mut app, &[Key::Char('f')]),
        [check_of(&app, fork_a("max"))]
    );
}

#[test]
fn a_missing_directory_is_refused() {
    let mut app = history_with(&["max"]);
    keys(&mut app, &[Key::Enter]);
    let fx = answer(
        &mut app,
        resume_a("max"),
        Some(format!("{CWD} does not exist")),
    );
    assert_eq!(fx, []);
    assert_eq!(
        notice(&app),
        Some((
            "resume aaaaaaaa as max: /Users/you/space/remuda does not exist",
            Level::Error
        ))
    );

    // No cwd recorded at all: refused before any check.
    let mut app = idle_app();
    let mut no_cwd = entry(A, "a", 1);
    no_cwd.cwd_last = None;
    update(&mut app, Event::IndexLoaded(vec![no_cwd]));
    update(&mut app, Event::Stores(stores()));
    let mut attribution = Attribution::default();
    attribution.add(A, "claude:max");
    update(&mut app, Event::Attribution(attribution));
    keys(&mut app, &[Key::Char('3')]);
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    assert_eq!(
        notice(&app),
        Some((
            "session aaaaaaaa has no recorded directory to resume in",
            Level::Error
        ))
    );
}

#[test]
fn several_or_no_accounts_open_the_account_picker() {
    let mut app = history_with(&["team", "max"]);
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    let Some(Overlay::Pick(pick)) = &app.overlay else {
        panic!("{:?}", app.overlay)
    };
    // C4: the accounts that can see the transcript's store first (attributed ones first
    // among them), then those that cannot (attributed first).
    assert_eq!(
        pick.options,
        ["claude:max", "claude:default", "claude:team"]
    );
    assert_eq!(pick.attributed, ["claude:max", "claude:team"]);
    // Esc cancels without launching.
    assert_eq!(keys(&mut app, &[Key::Esc]), []);
    assert_eq!(app.overlay, None);

    // `team`'s projects store is not the transcript's: it could not find the session.
    keys(&mut app, &[Key::Enter, Key::Char('j'), Key::Char('j')]);
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    assert_eq!(app.overlay, None);
    assert_eq!(
        notice(&app),
        Some((
            "team cannot find session aaaaaaaa: its projects store is /t, the transcript is in /s",
            Level::Error
        ))
    );
    // `default` shares max's store: fine, even though the session is not attributed to it.
    keys(&mut app, &[Key::Enter, Key::Char('j')]);
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [check_of(&app, resume_a("default"))]
    );

    // No attribution at all: every account, registry order.
    let mut app = history_with(&[]);
    keys(&mut app, &[Key::Char('f')]);
    let Some(Overlay::Pick(pick)) = &app.overlay else {
        panic!("{:?}", app.overlay)
    };
    assert_eq!(
        pick.options,
        ["claude:default", "claude:max", "claude:team"]
    );
    assert!(pick.attributed.is_empty());
    assert_eq!(
        keys(&mut app, &[Key::Char('j'), Key::Enter]),
        [check_of(&app, fork_a("max"))]
    );
}

#[test]
fn an_account_without_a_store_cannot_resume() {
    let mut app = history_with(&["max"]);
    update(&mut app, Event::Stores(vec![stores().remove(1)]));
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    assert_eq!(
        notice(&app),
        Some((
            "max cannot find session aaaaaaaa: it has no projects store",
            Level::Error
        ))
    );
}

#[test]
fn nothing_launches_before_the_stores_are_known() {
    let mut app = idle_app();
    update(&mut app, Event::IndexLoaded(vec![entry(A, "a", 1)]));
    keys(&mut app, &[Key::Char('3')]);
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    assert_eq!(
        notice(&app),
        Some((
            "still reading the transcript stores; try again in a moment",
            Level::Warn
        ))
    );
}

#[test]
fn a_running_interactive_session_is_not_resumed_but_can_be_forked() {
    let mut app = history_with(&["max"]);
    update(&mut app, Event::Live(vec![live("claude:max", 7, Some(A))]));
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    assert_eq!(
        notice(&app),
        Some((
            "session aaaaaaaa is running in max (pid 7): switch to its terminal \
             (two claudes writing one session overwrite each other)",
            Level::Warn
        ))
    );
    // A fork writes a new session: allowed.
    assert_eq!(
        keys(&mut app, &[Key::Char('f')]),
        [check_of(&app, fork_a("max"))]
    );
}

#[test]
fn a_running_background_session_is_attached_instead() {
    let mut app = history_with(&["max"]);
    update(
        &mut app,
        Event::Live(vec![background("claude:max", "766560c5", A, "blocked")]),
    );
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [Effect::Launch(request(
            "max",
            &["attach", "766560c5"],
            None,
            "attach 766560c5 as max"
        ))]
    );
    // Once it has stopped, it is an ordinary transcript to resume.
    update(
        &mut app,
        Event::Live(vec![background("claude:max", "766560c5", A, "stopped")]),
    );
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [check_of(&app, resume_a("max"))]
    );
}

#[test]
fn live_view_enter_and_fork() {
    let mut app = history_with(&["max"]);
    update(&mut app, Event::Live(vec![live("claude:max", 7, Some(A))]));
    keys(&mut app, &[Key::Char('2')]);
    // Interactive sessions can only be looked at here.
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    assert_eq!(
        notice(&app),
        Some((
            "session aaaaaaaa is running in max (pid 7): switch to its terminal \
             (two claudes writing one session overwrite each other)",
            Level::Warn
        ))
    );
    assert_eq!(
        keys(&mut app, &[Key::Char('f')]),
        [check_of(&app, fork_a("max"))]
    );
}

/// Until the first live collection arrives, `self.live` is empty; a resume
/// must not take that as "not running" (R16).
#[test]
fn resume_before_live_loaded() {
    let mut app = app();
    app.start();
    update(&mut app, Event::IndexLoaded(vec![entry(A, "a", 1)]));
    update(&mut app, Event::Stores(stores()));
    let mut at = Attribution::default();
    at.add(A, "claude:max");
    update(&mut app, Event::Attribution(at));
    keys(&mut app, &[Key::Char('3')]);
    assert!(!app.live_loaded);
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    assert_eq!(
        notice(&app),
        Some((
            "still reading the running sessions; try again in a moment",
            Level::Warn
        ))
    );
    // A fork only reads the session: allowed meanwhile.
    assert_eq!(
        keys(&mut app, &[Key::Char('f')]),
        [check_of(&app, fork_a("max"))]
    );
    // Once they are known, the resume goes ahead.
    update(&mut app, Event::Live(vec![]));
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [check_of(&app, resume_a("max"))]
    );
}

/// After a foreground launch the live list is from before it; a resume waits
/// for a collection started after the launch.
#[test]
fn resume_waits_for_live_sessions_collected_after_a_launch() {
    let mut app = history_with(&["max"]);
    // A collection already running when the launch ends predates it ...
    tick(&mut app, 6);
    assert!(app.live_in_flight);
    update(
        &mut app,
        Event::Launched {
            request: resume_a("max"),
            result: Ok(Exit::Code(0)),
            warnings: vec![],
        },
    );
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    assert_eq!(
        notice(&app),
        Some((
            "still reading the running sessions; try again in a moment",
            Level::Warn
        ))
    );
    // ... so its answer is not enough; the one it triggers is.
    assert_eq!(update(&mut app, Event::Live(vec![])), [Effect::Live]);
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    update(&mut app, Event::Live(vec![]));
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [check_of(&app, resume_a("max"))]
    );
}

/// The session starts running while the account picker is open.
#[test]
fn picker_open_while_session_starts_running() {
    let mut app = history_with(&["team", "max"]);
    assert_eq!(keys(&mut app, &[Key::Enter]), []); // picker open
    update(&mut app, Event::Live(vec![live("claude:max", 7, Some(A))])); // now running
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    assert_eq!(app.overlay, None);
    assert_eq!(
        notice(&app),
        Some((
            "session aaaaaaaa is running in max (pid 7): switch to its terminal \
             (two claudes writing one session overwrite each other)",
            Level::Warn
        ))
    );
}

/// The session starts running between the pre-launch check and its answer.
#[test]
fn a_session_seen_running_before_the_check_answers_is_not_launched() {
    let mut app = history_with(&["max"]);
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [check_of(&app, resume_a("max"))]
    );
    update(&mut app, Event::Live(vec![live("claude:max", 7, Some(A))]));
    let fx = answer(&mut app, resume_a("max"), None);
    assert_eq!(fx, []);
    assert_eq!(app.pending, None);
    assert!(
        notice(&app)
            .unwrap()
            .0
            .contains("is running in max (pid 7)"),
        "{:?}",
        app.notice
    );
    // A fork is not affected.
    assert_eq!(
        keys(&mut app, &[Key::Char('f')]),
        [check_of(&app, fork_a("max"))]
    );
    assert_eq!(
        answer(&mut app, fork_a("max"), None),
        [Effect::Launch(fork_a("max"))]
    );
}

/// One session id in two stores (a copied profile). The selected row is what
/// is resumed: its store decides which account can, its `cwd_last` where.
#[test]
fn duplicate_session_id_in_two_stores() {
    let mut app = idle_app();
    let in_s = entry(A, "copy in s", 2);
    let mut in_t = entry(A, "copy in t", 1);
    in_t.path = PathBuf::from(format!("/t/-w/{A}.jsonl"));
    in_t.store = PathBuf::from("/t");
    in_t.cwd_last = Some("/team/dir".into());
    update(&mut app, Event::IndexLoaded(vec![in_s, in_t]));
    update(&mut app, Event::Stores(stores()));
    let mut at = Attribution::default();
    at.add(A, "claude:max");
    at.add(A, "claude:team");
    update(&mut app, Event::Attribution(at));
    keys(&mut app, &[Key::Char('3')]);
    // The newer row is the copy in /s (max's store).
    assert_eq!(
        app.selected_entry().unwrap().path,
        PathBuf::from(format!("/s/-w/{A}.jsonl"))
    );
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    let Some(Overlay::Pick(_)) = &app.overlay else {
        panic!("{:?}", app.overlay)
    };
    // The picker judges the stores against the selected row.
    let lines = screen(&app);
    let (_, max) = line_with(&lines, "│› max");
    assert!(!max.contains("projects store"), "{max}");
    let (_, team) = line_with(&lines, "│  team");
    assert!(team.contains("other projects store"), "{team}");
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [check_of(&app, resume_a("max"))]
    );

    // The older row is the copy in /t: team (the one account that sees it comes first, C4),
    // in its own directory.
    keys(&mut app, &[Key::Char('j')]);
    assert_eq!(
        app.selected_entry().unwrap().path,
        PathBuf::from(format!("/t/-w/{A}.jsonl"))
    );
    keys(&mut app, &[Key::Enter]);
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [check_of(
            &app,
            request(
                "team",
                &["--resume", A],
                Some("/team/dir"),
                "resume aaaaaaaa as team"
            )
        )]
    );
}

/// Ids from file names and `agents --json` are passed to claude as arguments only when
/// they have the expected shape (R16).
#[test]
fn ids_that_are_not_uuids_are_not_passed_to_claude() {
    let mut app = idle_app();
    let evil = "--dangerously-skip-permissions";
    update(&mut app, Event::IndexLoaded(vec![entry(evil, "evil", 1)]));
    update(&mut app, Event::Stores(stores()));
    let mut at = Attribution::default();
    at.add(evil, "claude:max");
    update(&mut app, Event::Attribution(at));
    keys(&mut app, &[Key::Char('3')]);
    for key in [Key::Enter, Key::Char('f')] {
        assert_eq!(keys(&mut app, &[key]), [], "{key:?}");
        assert_eq!(app.overlay, None);
        assert_eq!(
            notice(&app),
            Some((
                "session id \"--dangerously-skip-permissions\" is not a UUID; \
                 remuda does not pass it to claude",
                Level::Error
            )),
            "{key:?}"
        );
    }
    // Not uppercase-only or unhyphenated variants either.
    for id in [
        "AAAAAAAA-0000-4000-8000-000000000000",
        "aaaaaaaa000040008000000000000000",
    ] {
        let mut app = idle_app();
        update(&mut app, Event::IndexLoaded(vec![entry(id, "x", 1)]));
        update(&mut app, Event::Stores(stores()));
        let mut at = Attribution::default();
        at.add(id, "claude:max");
        update(&mut app, Event::Attribution(at));
        keys(&mut app, &[Key::Char('3')]);
        assert_eq!(keys(&mut app, &[Key::Enter]), [], "{id}");
    }
}

/// Background short ids must be 8 lowercase hex digits before attach/logs/stop/rm.
#[test]
fn background_ids_that_are_not_short_ids_are_refused() {
    let mut app = history_with(&["max"]);
    for bad in ["--force", "766560C5", "766560c", "766560c5x"] {
        update(
            &mut app,
            Event::Live(vec![
                background("claude:max", bad, A, "blocked"),
                background("claude:max", bad, B, "stopped"),
            ]),
        );
        keys(&mut app, &[Key::Char('2'), Key::Char('a'), Key::Char('g')]);
        let refused = format!(
            "background session id {bad:?} is not 8 hex digits; remuda does not pass it to claude"
        );
        for key in [Key::Enter, Key::Char('l'), Key::Char('x')] {
            assert_eq!(keys(&mut app, &[key]), [], "{bad} {key:?}");
            assert_eq!(app.overlay, None, "{bad} {key:?}");
            assert_eq!(
                notice(&app),
                Some((refused.as_str(), Level::Error)),
                "{key:?}"
            );
        }
        keys(&mut app, &[Key::Char('G')]);
        assert_eq!(keys(&mut app, &[Key::Char('D')]), [], "{bad}");
        assert_eq!(app.overlay, None, "{bad}");
        assert_eq!(notice(&app), Some((refused.as_str(), Level::Error)));
        // History: a running background session is attached instead of resumed.
        keys(&mut app, &[Key::Char('a'), Key::Char('3')]);
        assert_eq!(keys(&mut app, &[Key::Enter]), [], "{bad}");
        assert_eq!(notice(&app), Some((refused.as_str(), Level::Error)));
    }
}

// ---- new session and setup forms (R16) ---------------------------------------------

/// A check answered while a foreground child (here an attach) had the terminal predates
/// whatever happened during it: the pending launch is cancelled when the child starts, and
/// the answer (queued ahead of `Launched`) is ignored.
#[test]
fn check_answer_queued_during_an_attach_is_applied_after_it() {
    let mut app = history_with(&["max"]);
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [check_of(&app, resume_a("max"))]
    );
    update(
        &mut app,
        Event::Live(vec![background("claude:max", "0badf00d", B, "running")]),
    );
    let fx = keys(&mut app, &[Key::Char('2'), Key::Enter]);
    assert!(
        matches!(fx.as_slice(), [Effect::Launch(r)] if r.args[0] == "attach"),
        "{fx:?}"
    );
    assert_eq!(app.pending, None);
    // What the live list says predates the child too.
    assert!(app.live_stale);
    // Queue order after the attach: LaunchChecked (sent during it), then Launched.
    assert_eq!(answer(&mut app, resume_a("max"), None), []);
    assert_eq!(app.pending, None);
    let attach = request(
        "max",
        &["attach", "0badf00d"],
        None,
        "attach 0badf00d as max",
    );
    update(
        &mut app,
        Event::Launched {
            request: attach,
            result: Ok(Exit::Code(0)),
            warnings: vec![],
        },
    );
    assert_eq!(
        notice(&app),
        Some((
            "attach 0badf00d as max: claude exited 0 · resume aaaaaaaa as max was cancelled: \
             start it again",
            Level::Info
        ))
    );
}

/// The same, through a setup (`claude auth login` in the foreground).
#[test]
fn check_answer_queued_during_a_setup_is_applied_after_it() {
    let mut app = history_with(&["max"]);
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [check_of(&app, resume_a("max"))]
    );
    let mut fx = keys(&mut app, &[Key::Char('1'), Key::Char('s')]);
    // Opening the form already cancelled the pending resume (C1).
    assert_eq!(app.pending, None);
    assert_eq!(
        notice(&app),
        Some(("resume aaaaaaaa as max cancelled", Level::Info))
    );
    fx.extend(type_str(&mut app, "newacct"));
    fx.extend(keys(&mut app, &[Key::Enter]));
    assert!(
        fx.iter().any(|e| matches!(e, Effect::Setup { .. })),
        "{fx:?}"
    );
    assert_eq!(app.pending, None);
    assert!(app.live_stale);
    assert_eq!(answer(&mut app, resume_a("max"), None), []);
    assert_eq!(app.pending, None);
    // The live list is collected again once the login ended (it may have run anything).
    let fx = update(
        &mut app,
        Event::SetupDone {
            provider: CLAUDE,
            name: "newacct".into(),
            result: Ok(Exit::Code(0)),
        },
    );
    assert_eq!(fx, [Effect::Live]);
    assert_eq!(
        notice(&app),
        Some(("set up newacct: claude auth login exited 0", Level::Info))
    );
}

/// A live collection that was running when the child started does not count as
/// current when it lands.
#[test]
fn a_live_collection_from_before_a_foreground_child_is_not_current() {
    let mut app = history_with(&["max"]);
    update(
        &mut app,
        Event::Live(vec![background("claude:max", "0badf00d", B, "running")]),
    );
    // A collection is running (the 5 s refresh) when the attach starts.
    assert_eq!(tick(&mut app, 6), [Effect::Live]);
    let fx = keys(&mut app, &[Key::Char('2'), Key::Enter]);
    assert!(matches!(fx.as_slice(), [Effect::Launch(_)]), "{fx:?}");
    // It lands after the child ran (queued ahead of `Launched`): collected again, still stale.
    assert_eq!(update(&mut app, Event::Live(vec![])), [Effect::Live]);
    assert!(app.live_stale);
    keys(&mut app, &[Key::Char('3')]);
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    assert_eq!(
        notice(&app),
        Some((
            "still reading the running sessions; try again in a moment",
            Level::Warn
        ))
    );
}

/// While the pre-launch check runs the status line says so, and Esc cancels it. An answer
/// to an earlier check of the very same launch is not taken for the current one.
#[test]
fn a_running_check_shows_in_the_status_line_and_esc_cancels_it() {
    let mut app = history_with(&["max"]);
    keys(&mut app, &[Key::Enter]);
    let first = app.launch_checks;
    let all = text(&app);
    assert!(
        all.contains("checking that aaaaaaaa is not running… (esc: cancel)"),
        "{all}"
    );
    assert_eq!(keys(&mut app, &[Key::Esc]), []);
    assert_eq!(app.pending, None);
    assert_eq!(
        notice(&app),
        Some(("resume aaaaaaaa as max cancelled", Level::Info))
    );
    assert_eq!(answer(&mut app, resume_a("max"), None), []);
    assert!(!text(&app).contains("checking that"));

    // Again: only the answer to the new check counts.
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [check_of(&app, resume_a("max"))]
    );
    assert_ne!(app.launch_checks, first);
    let old = Event::LaunchChecked {
        check: first,
        request: resume_a("max"),
        error: None,
    };
    assert_eq!(update(&mut app, old), []);
    assert!(app.pending.is_some());
    assert_eq!(
        answer(&mut app, resume_a("max"), None),
        [Effect::Launch(resume_a("max"))]
    );

    // A fork or a new session only has its directory checked.
    let mut app = history_with(&["max"]);
    keys(&mut app, &[Key::Char('f')]);
    let all = text(&app);
    assert!(
        all.contains("checking fork aaaaaaaa as max… (esc: cancel)"),
        "{all}"
    );
    // Esc still does what it did otherwise when nothing is pending.
    keys(&mut app, &[Key::Esc]);
    assert_eq!(app.pending, None);
    keys(&mut app, &[Key::Char('/'), Key::Char('a'), Key::Enter]);
    assert_eq!(app.history.query, "a");
    keys(&mut app, &[Key::Esc]);
    assert_eq!(app.history.query, "");
}

/// C1: opening an overlay cancels a pending launch (like Esc): its answer, when it comes,
/// neither launches nor touches the overlay.
#[test]
fn opening_an_overlay_cancels_a_pending_launch() {
    // A resume waiting for its check, then the new-session form.
    let mut app = history_with(&["max"]);
    keys(&mut app, &[Key::Enter]);
    assert!(app.pending.is_some());
    keys(&mut app, &[Key::Char('1'), Key::Char('n')]);
    assert_eq!(app.pending, None);
    assert_eq!(
        notice(&app),
        Some(("resume aaaaaaaa as max cancelled", Level::Info))
    );
    assert_eq!(answer(&mut app, resume_a("max"), None), []);
    assert_eq!(
        form(&app).kind,
        FormKind::NewSession {
            account: account("default")
        }
    );
    assert_eq!(answer(&mut app, resume_a("max"), Some("gone".into())), []);
    assert_eq!(form(&app).error, None);

    // ... or the account picker of another session.
    let mut app = history_with(&["max"]);
    let mut at = Attribution::default();
    at.add(A, "claude:max");
    at.add(B, "claude:max");
    at.add(B, "claude:team");
    update(&mut app, Event::Attribution(at));
    keys(&mut app, &[Key::Enter]);
    keys(&mut app, &[Key::Char('k'), Key::Enter]);
    assert!(
        matches!(app.overlay, Some(Overlay::Pick(_))),
        "{:?}",
        app.overlay
    );
    assert_eq!(app.pending, None);
    assert_eq!(answer(&mut app, resume_a("max"), None), []);
    assert!(
        matches!(app.overlay, Some(Overlay::Pick(_))),
        "{:?}",
        app.overlay
    );
}

/// C1: a check's error goes into the form only when that form started the check.
#[test]
fn a_check_error_goes_only_into_the_form_that_started_it() {
    let mut app = history_with(&["max"]);
    keys(&mut app, &[Key::Enter]);
    // A form that did not start the check (put there directly: the keys cannot get here).
    let Some(Overlay::Form(unrelated)) = new_session_form().overlay else {
        unreachable!()
    };
    app.overlay = Some(Overlay::Form(unrelated));
    answer(&mut app, resume_a("max"), Some("/x does not exist".into()));
    assert_eq!(form(&app).error, None);
    assert_eq!(
        notice(&app),
        Some(("resume aaaaaaaa as max: /x does not exist", Level::Error))
    );
}

/// A and its copy in `/t` (with its own directory), indexed together.
fn history_with_two_copies(owners: &[&str]) -> App {
    let mut app = idle_app();
    let mut t = entry(A, "a-team", 3);
    t.path = PathBuf::from(format!("/t/-w/{A}.jsonl"));
    t.store = PathBuf::from("/t");
    t.cwd_last = Some("/team/dir".into());
    update(&mut app, Event::IndexLoaded(vec![entry(A, "a", 1), t]));
    update(&mut app, Event::Stores(stores()));
    let mut at = Attribution::default();
    for owner in owners {
        at.add(A, &format!("claude:{owner}"));
    }
    update(&mut app, Event::Attribution(at));
    app
}

/// Live previews the copy that `f` forks: the one in the live session's store.
#[test]
fn the_live_preview_shows_the_copy_that_f_forks() {
    let mut app = history_with_two_copies(&[]);
    update(&mut app, Event::Live(vec![live("claude:max", 7, Some(A))]));
    keys(&mut app, &[Key::Char('2')]);
    assert_eq!(app.preview.target, Some(path(A)));
    assert_eq!(
        keys(&mut app, &[Key::Char('f')]),
        [check_of(&app, fork_a("max"))]
    );
    keys(&mut app, &[Key::Esc]);
    update(&mut app, Event::Live(vec![live("claude:team", 8, Some(A))]));
    assert_eq!(
        app.preview.target,
        Some(PathBuf::from(format!("/t/-w/{A}.jsonl")))
    );
}

/// The one attributed account cannot see the selected copy (another store): the picker
/// opens with the accounts that can first, instead of a dead end.
#[test]
fn single_owner_in_the_other_store_leaves_no_way_to_resume_the_selected_copy() {
    let mut app = history_with_two_copies(&["team"]);
    keys(&mut app, &[Key::Char('3')]);
    while app.selected_entry().unwrap().store != Path::new("/s") {
        keys(&mut app, &[Key::Char('j')]);
    }
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    let Some(Overlay::Pick(pick)) = &app.overlay else {
        panic!("{:?} {:?}", app.overlay, app.notice)
    };
    assert_eq!(
        pick.options,
        ["claude:default", "claude:max", "claude:team"]
    );
    assert_eq!(pick.attributed, ["claude:team"]);
    let lines = screen(&app);
    let (_, team) = line_with(&lines, "│  team");
    assert!(
        team.contains("●") && team.contains("other projects store"),
        "{team}"
    );
    let (_, default) = line_with(&lines, "│› default");
    assert!(!default.contains("●"), "{default}");
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [check_of(&app, resume_a("default"))]
    );

    // The owner's own copy still resumes with it directly.
    keys(&mut app, &[Key::Esc]);
    while app.selected_entry().unwrap().store != Path::new("/t") {
        keys(&mut app, &[Key::Char('k')]);
    }
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [check_of(
            &app,
            request(
                "team",
                &["--resume", A],
                Some("/team/dir"),
                "resume aaaaaaaa as team"
            )
        )]
    );
}

fn form(app: &App) -> &Form {
    match &app.overlay {
        Some(Overlay::Form(form)) => form,
        other => panic!("no form: {other:?}"),
    }
}

fn values(app: &App) -> Vec<String> {
    form(app).fields.iter().map(|f| f.value.clone()).collect()
}

/// `n` on `max` (the second account).
fn new_session_form() -> App {
    let mut app = idle_app();
    keys(&mut app, &[Key::Char('j'), Key::Char('n')]);
    assert_eq!(
        form(&app).kind,
        FormKind::NewSession {
            account: account("max")
        }
    );
    app
}

fn clear_field(app: &mut App) {
    for _ in 0..200 {
        keys(app, &[Key::Backspace]);
    }
}

#[test]
fn new_session_defaults_to_remudas_directory_and_checks_it() {
    let mut app = new_session_form();
    // Directory first (prefilled with where remuda started), then an optional name.
    assert_eq!(values(&app), [CWD, ""]);
    assert_eq!(form(&app).focus, 0);
    let fx = keys(&mut app, &[Key::Enter]);
    let want = request("max", &[], Some(CWD), "new session as max");
    assert_eq!(fx, [check_of(&app, want.clone())]);
    // The form stays up until the directory is known to exist.
    assert!(app.overlay.is_some());
    let fx = answer(&mut app, want.clone(), None);
    assert_eq!(fx, [Effect::Launch(want)]);
    assert_eq!(app.overlay, None);
}

#[test]
fn new_session_form_editing_name_and_tilde() {
    let mut app = new_session_form();
    // Typing goes to the focused field: every key is text here.
    clear_field(&mut app);
    assert_eq!(type_str(&mut app, "~/w q1?"), []);
    keys(&mut app, &[Key::Tab]);
    assert_eq!(form(&app).focus, 1);
    type_str(&mut app, "fix the index");
    assert_eq!(values(&app), ["~/w q1?", "fix the index"]);
    assert_eq!(app.view, View::Accounts);
    let fx = keys(&mut app, &[Key::Enter]);
    assert_eq!(
        fx,
        [check_of(
            &app,
            request(
                "max",
                &["-n", "fix the index"],
                Some("/Users/you/w q1?"),
                "new session “fix the index” as max"
            )
        )]
    );
    // BackTab goes back to the directory; a relative one is taken from remuda's directory.
    keys(&mut app, &[Key::BackTab]);
    clear_field(&mut app);
    type_str(&mut app, "sub dir");
    let fx = keys(&mut app, &[Key::Enter]);
    let Some(Effect::CheckLaunch { request: req, .. }) = fx.first() else {
        panic!("{fx:?}")
    };
    assert_eq!(req.cwd, Some(PathBuf::from(format!("{CWD}/sub dir"))));
}

#[test]
fn new_session_form_errors_stay_inline() {
    let mut app = new_session_form();
    let want = request("max", &[], Some(CWD), "new session as max");
    keys(&mut app, &[Key::Enter]);
    let fx = answer(&mut app, want, Some(format!("{CWD} does not exist")));
    assert_eq!(fx, []);
    assert_eq!(
        form(&app).error.as_deref(),
        Some("/Users/you/space/remuda does not exist")
    );
    assert_eq!(app.notice, None);
    // Typing clears the error.
    type_str(&mut app, "x");
    assert_eq!(form(&app).error, None);

    clear_field(&mut app);
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    assert_eq!(form(&app).error.as_deref(), Some("enter a directory"));

    // A name that would stop remuda from seeing a new session (R6) is refused.
    type_str(&mut app, "/w");
    keys(&mut app, &[Key::Tab]);
    type_str(&mut app, "update");
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    assert!(
        form(&app)
            .error
            .as_deref()
            .unwrap()
            .contains("choose another name"),
        "{:?}",
        form(&app).error
    );
}

/// A name starting with `-` would be read by claude as an option; with `--`
/// the injected id would even land after the terminator (R16).
#[test]
fn session_name_dash_dash() {
    for name in ["--", "-x", "-p", "--verbose", "-", "update", "attach"] {
        let mut app = new_session_form();
        keys(&mut app, &[Key::Tab]);
        type_str(&mut app, name);
        assert_eq!(keys(&mut app, &[Key::Enter]), [], "{name}");
        let error = form(&app).error.clone().unwrap_or_default();
        assert!(error.contains("choose another name"), "{name}: {error}");
    }
    // Dashes inside a name are fine.
    let mut app = new_session_form();
    keys(&mut app, &[Key::Tab]);
    type_str(&mut app, "fix-the-index --now");
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [check_of(
            &app,
            request(
                "max",
                &["-n", "fix-the-index --now"],
                Some(CWD),
                "new session “fix-the-index --now” as max"
            )
        )]
    );
}

#[test]
fn esc_cancels_a_form_and_its_pending_check() {
    let mut app = new_session_form();
    let want = request("max", &[], Some(CWD), "new session as max");
    keys(&mut app, &[Key::Enter, Key::Esc]);
    assert_eq!(app.overlay, None);
    // The answer to the cancelled check launches nothing.
    let fx = answer(&mut app, want, None);
    assert_eq!(fx, []);
    // `q` in a form is text, not quit.
    keys(&mut app, &[Key::Char('n')]);
    assert_eq!(keys(&mut app, &[Key::Char('q')]), []);
    assert!(values(&app)[0].ends_with('q'));
    // Ctrl-C still quits.
    assert_eq!(keys(&mut app, &[Key::Ctrl('c')]), [Effect::Quit]);
}

#[test]
fn n_and_s_belong_to_the_accounts_view() {
    let mut app = idle_app();
    keys(&mut app, &[Key::Char('3'), Key::Char('n'), Key::Char('s')]);
    assert_eq!(app.overlay, None);
}

#[test]
fn setup_form_validates_like_remuda_setup() {
    let mut app = idle_app();
    keys(&mut app, &[Key::Char('s')]);
    assert_eq!(form(&app).kind, FormKind::Setup);
    // Name, email, provider (R17).
    assert_eq!(values(&app), ["", "", "claude"]);
    for (name, error) in [
        ("", "invalid account name"),
        ("a.b", "invalid account name"),
        ("default", "reserved"),
        ("-x", "cannot start with `-`"),
        ("max", "claude:max is already registered"),
    ] {
        clear_field(&mut app);
        type_str(&mut app, name);
        assert_eq!(keys(&mut app, &[Key::Enter]), [], "{name}");
        let got = form(&app).error.clone().unwrap_or_default();
        assert!(got.contains(error), "{name}: {got}");
    }
    clear_field(&mut app);
    type_str(&mut app, "work");
    keys(&mut app, &[Key::Down]);
    type_str(&mut app, "me+work@example.com");
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [Effect::Setup {
            provider: CLAUDE,
            name: "work".into(),
            email: Some("me+work@example.com".into())
        }]
    );
    assert_eq!(app.overlay, None);

    // Without an email.
    keys(&mut app, &[Key::Char('s')]);
    type_str(&mut app, "solo");
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [Effect::Setup {
            provider: CLAUDE,
            name: "solo".into(),
            email: None
        }]
    );
}

/// R16, R14a: `D` in Accounts asks before removing the selected account; `default` is refused.
#[test]
fn d_in_accounts_asks_and_refuses_default() {
    let mut app = app();
    keys(&mut app, &[Key::Char('1')]);
    assert_eq!(keys(&mut app, &[Key::Char('D')]), []);
    assert_eq!(app.overlay, None);
    let (said, level) = notice(&app).unwrap();
    assert!(said.contains("implicit"), "{said}");
    assert_eq!(level, Level::Warn);

    assert_eq!(keys(&mut app, &[Key::Char('j'), Key::Char('D')]), []);
    assert_eq!(app.overlay, Some(Overlay::RemoveAccount(account("max"))));
    let all = text(&app);
    assert!(all.contains("Remove max"), "{all}");
    assert!(all.contains("/h/max"), "{all}");
    assert!(all.contains("y: yes · any other key: cancel"), "{all}");

    assert_eq!(keys(&mut app, &[Key::Char('n')]), []);
    assert_eq!(app.overlay, None);
    assert_eq!(notice(&app), Some(("cancelled", Level::Info)));

    assert_eq!(
        keys(&mut app, &[Key::Char('D'), Key::Char('y')]),
        [Effect::RemoveAccount(account("max"))]
    );
    assert_eq!(app.overlay, None);
}

/// R16: the rows are rebuilt from the registry read again, then the result is told.
#[test]
fn removal_result_is_told_and_rows_rebuilt() {
    let mut app = app();
    update(
        &mut app,
        Event::Accounts(vec![account("default"), account("team")]),
    );
    assert_eq!(app.accounts.len(), 2);
    update(
        &mut app,
        Event::AccountRemoved {
            account: account("max"),
            result: Ok(()),
        },
    );
    let (said, level) = notice(&app).unwrap();
    assert!(said.contains("removed max"), "{said}");
    assert!(said.contains("/h/max was left in place"), "{said}");
    assert_eq!(level, Level::Info);

    update(
        &mut app,
        Event::AccountRemoved {
            account: account("team"),
            result: Err(
                "claude:team is the source of shared configuration ([share.claude] from in \
                 /r/config.toml); change or remove `from` first"
                    .into(),
            ),
        },
    );
    let (said, level) = notice(&app).unwrap();
    assert!(said.starts_with("cannot remove team: "), "{said}");
    assert!(said.contains("source of shared configuration"), "{said}");
    assert_eq!(level, Level::Error);
}

/// R16: an account unregistered while its prompt is open is not removed again.
#[test]
fn y_after_the_account_vanished() {
    let mut app = app();
    keys(&mut app, &[Key::Char('1'), Key::Char('j'), Key::Char('D')]);
    assert_eq!(app.overlay, Some(Overlay::RemoveAccount(account("max"))));
    update(
        &mut app,
        Event::Accounts(vec![account("default"), account("team")]),
    );
    assert_eq!(keys(&mut app, &[Key::Char('y')]), []);
    assert_eq!(app.overlay, None);
    assert_eq!(
        notice(&app),
        Some(("max is no longer registered", Level::Error))
    );
}

#[test]
fn a_finished_setup_reloads_accounts_and_identities() {
    let mut app = idle_app();
    let mut accounts: Vec<Account> = app.accounts.iter().map(|a| a.account.clone()).collect();
    accounts.push(account("work"));
    let fx = update(&mut app, Event::Accounts(accounts));
    let names: Vec<&str> = app
        .accounts
        .iter()
        .map(|a| a.account.name.as_str())
        .collect();
    assert_eq!(names, ["default", "max", "team", "work"]);
    // Known accounts keep their state.
    assert_eq!(app.accounts[1].identity, Some(Identity::NotLoggedIn));
    assert_eq!(app.accounts[3].identity, None);
    for effect in [
        Effect::Identities,
        Effect::CachedUsage,
        Effect::Checks,
        Effect::RefreshIndex,
    ] {
        assert!(fx.contains(&effect), "{effect:?} in {fx:?}");
    }
    update(
        &mut app,
        Event::SetupDone {
            provider: CLAUDE,
            name: "work".into(),
            result: Ok(Exit::Code(0)),
        },
    );
    assert_eq!(
        notice(&app),
        Some(("set up work: claude auth login exited 0", Level::Info))
    );
    update(
        &mut app,
        Event::SetupDone {
            provider: CLAUDE,
            name: "x".into(),
            result: Err("/r/homes/claude/x already exists".into()),
        },
    );
    assert_eq!(
        notice(&app),
        Some(("set up x: /r/homes/claude/x already exists", Level::Error))
    );
}

// ---- background sessions in Live (R7, R16) -------------------------------------------

/// Live with an interactive session (`a` under max), a running background one (`b` under
/// team) and a stopped one (`c` under max), in that order; stores and index loaded.
fn live_with_background() -> App {
    let mut app = history_with(&["max"]);
    update(
        &mut app,
        Event::Live(vec![
            live("claude:max", 7, Some(A)),
            background("claude:team", "bbbbbbbb", "b", "blocked"),
            background("claude:max", "cccccccc", "c", "stopped"),
        ]),
    );
    keys(&mut app, &[Key::Char('2')]);
    app
}

fn live_names(app: &App) -> Vec<Option<String>> {
    app.live_rows
        .iter()
        .map(|&i| app.live[i].short_id.clone())
        .collect()
}

#[test]
fn stopped_background_sessions_show_with_a() {
    let mut app = live_with_background();
    assert_eq!(live_names(&app), [None, Some("bbbbbbbb".into())]);
    keys(&mut app, &[Key::Char('a')]);
    assert_eq!(
        live_names(&app),
        [None, Some("bbbbbbbb".into()), Some("cccccccc".into())]
    );
    keys(&mut app, &[Key::Char('G')]);
    assert_eq!(
        app.selected_live().unwrap().short_id.as_deref(),
        Some("cccccccc")
    );
    keys(&mut app, &[Key::Char('a')]);
    // The selection moves to a row that is still shown.
    assert_eq!(
        app.selected_live().unwrap().short_id.as_deref(),
        Some("bbbbbbbb")
    );
    // `a` in History is still its own toggle.
    keys(&mut app, &[Key::Char('3'), Key::Char('a')]);
    assert!(app.history.show_all);
    keys(&mut app, &[Key::Char('2')]);
    assert_eq!(live_names(&app).len(), 2);
}

#[test]
fn enter_attaches_background_sessions_even_stopped_ones() {
    let mut app = live_with_background();
    keys(&mut app, &[Key::Char('j')]);
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [Effect::Launch(request(
            "team",
            &["attach", "bbbbbbbb"],
            None,
            "attach bbbbbbbb as team"
        ))]
    );
    // claude: "resume it later with `claude attach <id>`".
    keys(&mut app, &[Key::Char('a'), Key::Char('G')]);
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [Effect::Launch(request(
            "max",
            &["attach", "cccccccc"],
            None,
            "attach cccccccc as max"
        ))]
    );
}

#[test]
fn logs_show_in_the_preview_until_the_selection_moves() {
    let mut app = live_with_background();
    // Interactive sessions have no logs.
    assert_eq!(keys(&mut app, &[Key::Char('l')]), []);
    assert!(
        notice(&app).unwrap().0.contains("background"),
        "{:?}",
        app.notice
    );
    keys(&mut app, &[Key::Char('j')]);
    assert_eq!(
        keys(&mut app, &[Key::Char('l')]),
        [Effect::Logs {
            account: account("team"),
            short_id: "bbbbbbbb".into()
        }]
    );
    assert!(text(&app).contains("loading logs…"), "{}", text(&app));
    update(
        &mut app,
        Event::Logs {
            short_id: "bbbbbbbb".into(),
            // Plain text already: `live::logs` interprets the terminal bytes (tests/live.rs).
            result: Ok("Building the index\nstep 2 of 3".into()),
        },
    );
    let all = text(&app);
    assert!(all.contains("Logs · bbbbbbbb"), "{all}");
    assert!(all.contains("Building the index"), "{all}");
    assert!(all.contains("step 2 of 3"), "{all}");
    // Esc goes back to the transcript preview; so does moving.
    keys(&mut app, &[Key::Esc]);
    assert_eq!(app.logs, None);
    keys(&mut app, &[Key::Char('l')]);
    update(
        &mut app,
        Event::Logs {
            short_id: "bbbbbbbb".into(),
            result: Err("exited with status 1: Couldn't read logs for bbbbbbbb".into()),
        },
    );
    assert!(text(&app).contains("cannot read logs: exited with status 1"));
    keys(&mut app, &[Key::Char('k')]);
    assert_eq!(app.logs, None);
    // A late answer for a session no longer selected is dropped.
    update(
        &mut app,
        Event::Logs {
            short_id: "bbbbbbbb".into(),
            result: Ok("late".into()),
        },
    );
    assert_eq!(app.logs, None);
}

#[test]
fn x_stops_after_confirmation() {
    let mut app = live_with_background();
    keys(&mut app, &[Key::Char('j')]);
    assert_eq!(keys(&mut app, &[Key::Char('x')]), []);
    assert!(matches!(app.overlay, Some(Overlay::Confirm(_))));
    assert!(
        text(&app).contains("Stop background session bbbbbbbb (team)?"),
        "{}",
        text(&app)
    );
    // Anything but `y` cancels.
    assert_eq!(keys(&mut app, &[Key::Char('n')]), []);
    assert_eq!(app.overlay, None);
    assert_eq!(notice(&app), Some(("cancelled", Level::Info)));
    keys(&mut app, &[Key::Char('x')]);
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    assert_eq!(app.overlay, None);
    keys(&mut app, &[Key::Char('x')]);
    assert_eq!(
        keys(&mut app, &[Key::Char('y')]),
        [Effect::Control {
            account: account("team"),
            verb: Control::Stop,
            short_id: "bbbbbbbb".into()
        }]
    );
    // When it is done, the live list is collected again.
    let fx = update(
        &mut app,
        Event::ControlDone {
            verb: Control::Stop,
            short_id: "bbbbbbbb".into(),
            result: Ok(String::new()),
        },
    );
    assert_eq!(fx, [Effect::Live]);
    assert_eq!(notice(&app), Some(("stopped bbbbbbbb", Level::Info)));
    // A stopped session cannot be stopped again; interactive ones not at all.
    keys(&mut app, &[Key::Char('a'), Key::Char('G'), Key::Char('x')]);
    assert_eq!(app.overlay, None);
    assert!(
        notice(&app).unwrap().0.contains("already stopped"),
        "{:?}",
        app.notice
    );
    keys(&mut app, &[Key::Char('g'), Key::Char('x')]);
    assert_eq!(app.overlay, None);
    assert!(
        notice(&app).unwrap().0.contains("only be viewed"),
        "{:?}",
        app.notice
    );
}

#[test]
fn capital_d_removes_stopped_sessions_after_confirmation() {
    let mut app = live_with_background();
    // A running one: stop it first.
    keys(&mut app, &[Key::Char('j'), Key::Char('D')]);
    assert_eq!(app.overlay, None);
    assert!(
        notice(&app).unwrap().0.contains("stop it first"),
        "{:?}",
        app.notice
    );
    keys(&mut app, &[Key::Char('a'), Key::Char('G'), Key::Char('D')]);
    assert!(
        text(&app).contains("Remove background session cccccccc (max)?"),
        "{}",
        text(&app)
    );
    assert_eq!(
        keys(&mut app, &[Key::Char('y')]),
        [Effect::Control {
            account: account("max"),
            verb: Control::Remove,
            short_id: "cccccccc".into()
        }]
    );
    update(&mut app, Event::Live(vec![]));
    let fx = update(
        &mut app,
        Event::ControlDone {
            verb: Control::Remove,
            short_id: "cccccccc".into(),
            result: Err("exited with status 1: has unpushed commits".into()),
        },
    );
    assert_eq!(fx, [Effect::Live]);
    assert_eq!(
        notice(&app),
        Some((
            "claude rm cccccccc exited with status 1: has unpushed commits",
            Level::Error
        ))
    );
}

#[test]
fn live_view_lists_both_kinds() {
    let mut app = live_with_background();
    update(&mut app, Event::Resize(120, 24));
    let lines = screen(&app);
    let all = lines.join("\n");
    let (_, head) = line_with(&lines, "ACCOUNT");
    assert!(head.contains("PID/ID"), "{head}");
    let (_, bg) = line_with(&lines, "bbbbbbbb");
    assert!(bg.contains("blocked") && bg.contains("background"), "{bg}");
    let (_, fg) = line_with(&lines, "busy");
    assert!(fg.contains(" 7 ") && fg.contains("interactive"), "{fg}");
    assert!(!all.contains("cccccccc"), "stopped ones are hidden: {all}");
    assert!(all.contains("2 running · 1 stopped (a: show)"), "{all}");
    keys(&mut app, &[Key::Char('a')]);
    let all = text(&app);
    assert!(all.contains("cccccccc"), "{all}");
    assert!(all.contains("2 running · 1 stopped (a: hide)"), "{all}");
    assert!(all.contains("l: logs") && all.contains("x: stop"), "{all}");
}

// ---- `remuda run` without an account: pick one (R5, R16) --------------------------------

fn pick_app() -> App {
    let mut app = app();
    app.mode = Mode::PickForRun;
    app
}

#[test]
fn pick_mode_loads_only_what_choosing_an_account_needs() {
    let mut app = pick_app();
    assert_eq!(
        app.start(),
        [Effect::Identities, Effect::CachedUsage, Effect::Checks]
    );
    finish_accounts(&mut app);
    update(&mut app, Event::Checks(vec![]));
    assert_eq!(
        keys(&mut app, &[Key::Char('r')]),
        [Effect::Identities, Effect::CachedUsage, Effect::Checks]
    );
    // Live usage helps to choose.
    assert_eq!(
        keys(&mut app, &[Key::Char('u')]),
        [Effect::LiveUsage(vec![
            account("default"),
            account("max"),
            account("team")
        ])]
    );
}

#[test]
fn pick_mode_enter_chooses_and_esc_or_q_cancels() {
    let mut app = pick_app();
    assert_eq!(
        keys(&mut app, &[Key::Char('j'), Key::Enter]),
        [Effect::Pick(account("max"))]
    );
    assert_eq!(keys(&mut app, &[Key::Esc]), [Effect::Quit]);
    assert_eq!(keys(&mut app, &[Key::Char('q')]), [Effect::Quit]);
    // Nothing else to do here: no other views, no forms.
    keys(
        &mut app,
        &[Key::Char('3'), Key::Tab, Key::Char('n'), Key::Char('s')],
    );
    assert_eq!(app.view, View::Accounts);
    assert_eq!(app.overlay, None);
}

#[test]
fn pick_mode_says_what_it_is_for() {
    let mut app = pick_app();
    let all = text(&app);
    assert!(
        all.contains("choose an account to launch claude in ~/space/remuda"),
        "{all}"
    );
    assert!(
        all.contains("enter: launch claude") && all.contains("esc/q: cancel"),
        "{all}"
    );
    let lines = screen(&app);
    let (_, max) = line_with(&lines, "max ");
    assert!(max.starts_with("max"), "{max}");
    keys(&mut app, &[Key::Char('?')]);
    assert!(text(&app).contains("Keys"));
}

// ---- render ------------------------------------------------------------------------

fn screen(app: &App) -> Vec<String> {
    let (w, h) = app.size;
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| render::render(app, f)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..h)
        .map(|y| {
            let mut line = String::new();
            let mut skip = 0;
            for x in 0..w {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                let symbol = buffer[(x, y)].symbol();
                skip = crate::text::width(symbol).saturating_sub(1);
                line.push_str(symbol);
            }
            line.trim_end().to_string()
        })
        .collect()
}

fn text(app: &App) -> String {
    screen(app).join("\n")
}

fn line_with<'a>(lines: &'a [String], needle: &str) -> (usize, &'a str) {
    lines
        .iter()
        .enumerate()
        .find(|(_, l)| l.contains(needle))
        .map(|(i, l)| (i, l.as_str()))
        .unwrap_or_else(|| panic!("no line with {needle:?}:\n{}", lines.join("\n")))
}

/// Accounts with identities, cached usage, a live result and checks.
fn populated_accounts() -> App {
    let mut app = app();
    app.start();
    update(
        &mut app,
        Event::Identity {
            account: account("default"),
            identity: logged_in("me@example.com"),
        },
    );
    update(
        &mut app,
        Event::Identity {
            account: account("max"),
            identity: logged_in("max@example.com"),
        },
    );
    update(
        &mut app,
        Event::Identity {
            account: account("team"),
            identity: Identity::NotLoggedIn,
        },
    );
    update(
        &mut app,
        Event::CachedUsage {
            account: account("default"),
            result: cached(vec![
                row(
                    "Session",
                    34.0,
                    Some("normal"),
                    Some("2026-09-24T14:00:00Z"),
                ),
                row(
                    "Week (all models)",
                    77.0,
                    Some("warning"),
                    Some("2026-09-27T12:00:00Z"),
                ),
                row(
                    "Week (Fable)",
                    100.0,
                    Some("critical"),
                    Some("2026-09-26T12:00:00Z"),
                ),
            ]),
        },
    );
    update(
        &mut app,
        Event::CachedUsage {
            account: account("max"),
            result: cached(vec![row(
                "Session",
                5.0,
                None,
                Some("2026-09-24T12:30:00Z"),
            )]),
        },
    );
    update(
        &mut app,
        Event::CachedUsage {
            account: account("team"),
            result: Err("no /h/team/.claude.json".into()),
        },
    );
    update(
        &mut app,
        Event::LiveUsage {
            account: account("max"),
            result: Ok(LiveUsage::Rows(vec![
                UsageRow {
                    label: "Session".into(),
                    percent: 12.0,
                    severity: None,
                    resets: Some(Resets::Text("Sep 24 at 1pm (UTC)".into())),
                },
                UsageRow {
                    label: "Week (all models)".into(),
                    percent: 91.0,
                    severity: None,
                    resets: Some(Resets::Text("Sep 30 at 11:59am (UTC)".into())),
                },
            ])
            .into()),
        },
    );
    update(
        &mut app,
        Event::Checks(vec![Check {
            account: None,
            message: "ANTHROPIC_API_KEY is set: it overrides every account's /login".into(),
        }]),
    );
    app
}

#[test]
fn accounts_view_loading() {
    let app = app();
    let lines = screen(&app);
    let (_, header) = line_with(&lines, "remuda");
    assert!(
        header.contains("1 Accounts") && header.contains("2 Live") && header.contains("3 History")
    );
    let (_, row) = line_with(&lines, "max ");
    assert!(row.contains('…'), "{row}");
    let all = text(&app);
    assert!(all.contains("checking…"), "{all}");
    assert!(all.contains("loading index…"), "{all}");
    assert!(all.contains("u: live usage"), "{all}");
}

/// A tall terminal draws the mark around the line of views; a shorter one keeps the one-line
/// header, and the body starts right below it.
#[test]
fn header_shows_the_mark_when_tall() {
    let mut app = app();
    update(&mut app, Event::Resize(80, render::TALL));
    let lines = screen(&app);
    assert_eq!(lines[0], " ╭─────");
    assert!(
        lines[1].starts_with(" │╭──── remuda  1 Accounts  2 Live"),
        "{}",
        lines[1]
    );
    assert!(lines[1].ends_with("?: help · q: quit"), "{}", lines[1]);
    assert_eq!(lines[2], " ││");
    assert!(lines[3].starts_with("Accounts"), "{}", lines[3]);

    update(&mut app, Event::Resize(80, render::TALL - 1));
    let lines = screen(&app);
    assert!(lines[0].starts_with(" remuda  1 Accounts"), "{}", lines[0]);
    assert!(lines[1].starts_with("Accounts"), "{}", lines[1]);
}

#[test]
fn accounts_view_populated() {
    let app = populated_accounts();
    let lines = screen(&app);
    let all = lines.join("\n");
    let (_, head) = line_with(&lines, "ACCOUNT");
    for col in ["EMAIL", "ORG", "PLAN", "SESSION", "WEEK", "Fable", "SOURCE"] {
        assert!(head.contains(col), "{head}");
    }
    let (_, default) = line_with(&lines, "me@example.com");
    for cell in ["34%", "77%", "100%", "cached 5m ago"] {
        assert!(default.contains(cell), "{default}");
    }
    let (_, max) = line_with(&lines, "max@example.com");
    for cell in ["12%", "91%", "live 0s ago"] {
        assert!(max.contains(cell), "{max}");
    }
    assert!(
        !max.contains(" 5%"),
        "live replaced the cached number: {max}"
    );
    let (_, team) = line_with(&lines, "not logged in");
    assert!(team.contains("no cache"), "{team}");

    // Timeline: S and W where they reset, relative to now.
    let (axis_y, axis) = line_with(&lines, "now");
    assert!(axis.contains("+1d"), "{axis}");
    let (track_y, track) = lines
        .iter()
        .enumerate()
        .skip(axis_y + 1)
        .find(|(_, l)| l.starts_with("default"))
        .unwrap();
    assert!(
        track.contains("S 2h00m") && track.contains("W 3d00h"),
        "{track}"
    );
    let axis_col = axis.find("now").unwrap();
    // Columns on the axis (the account name comes first and may contain these letters).
    let on_axis = |c: char| track.chars().skip(axis_col).position(|x| x == c).unwrap() + axis_col;
    let s_col = on_axis('S');
    assert!(
        s_col - axis_col <= 1,
        "S resets in 2h: at the start of the axis\n{all}"
    );
    let w_col = on_axis('W');
    let f_col = on_axis('f');
    assert!(s_col < f_col && f_col < w_col, "{track}");
    // Live wording is placed too: max's week resets in ~6 days.
    let max_track = &lines[track_y + 1];
    assert!(max_track.starts_with("max"), "{all}");
    assert!(max_track.contains("S 1h00m W 5d23h"), "{max_track}");
    assert!(all.contains("f week (Fable)"), "{all}");

    // Checks: the env check, the login state.
    assert!(all.contains("! ANTHROPIC_API_KEY is set"), "{all}");
    assert!(all.contains("! team: not logged in"), "{all}");
}

#[test]
fn accounts_view_errors_and_pending_live_usage() {
    let mut app = populated_accounts();
    keys(&mut app, &[Key::Char('u')]);
    let all = text(&app);
    assert_eq!(all.matches("live…").count(), 3, "{all}");
    update(
        &mut app,
        Event::LiveUsage {
            account: account("team"),
            result: Err("`claude -p /usage --no-session-persistence` timed out after 90s".into()),
        },
    );
    update(&mut app, Event::Checks(vec![]));
    let all = text(&app);
    assert!(all.contains("live failed"), "{all}");
    assert!(
        all.contains("! team: live usage failed: `claude -p /usage"),
        "{all}"
    );
    assert!(!all.contains("no problems found"), "{all}");
}

#[test]
fn accounts_view_without_problems() {
    let mut app = app();
    update(&mut app, Event::Checks(vec![]));
    assert!(text(&app).contains("✓ no problems found"));
}

fn populated_history() -> App {
    let mut app = app();
    app.start();
    let mut titled = entry("a", "first words", 3);
    titled.title = Some("修复索引的增量扫描".into());
    let mut entries = noisy();
    entries.push(titled);
    entries.push(entry("b", "plain one", 1));
    update(&mut app, Event::IndexLoaded(entries));
    let mut attribution = Attribution::default();
    attribution.add("a", "claude:max");
    attribution.add("a", "claude:team");
    update(&mut app, Event::Attribution(attribution));
    keys(&mut app, &[Key::Char('3')]);
    app
}

#[test]
fn history_view_loading_and_empty() {
    let mut app = app();
    keys(&mut app, &[Key::Char('3')]);
    assert!(text(&app).contains("loading sessions…"));
    update(&mut app, Event::IndexLoaded(vec![]));
    app.index_in_flight = true;
    assert!(text(&app).contains("indexing…"));
    update(
        &mut app,
        Event::IndexProgress {
            done: 1234,
            total: 6071,
            entries: vec![],
        },
    );
    assert!(text(&app).contains("indexing 1234/6071"));
    update(
        &mut app,
        Event::IndexDone {
            entries: vec![],
            error: None,
        },
    );
    let all = text(&app);
    assert!(all.contains("no sessions found"), "{all}");
    assert!(all.contains("showing 0 of 0 (a: show all)"), "{all}");
    assert!(all.contains("refreshed 12:00:00"), "{all}");
}

#[test]
fn history_view_populated_narrow() {
    let mut app = populated_history();
    // Preview arrives for the selected (newest visible) session.
    tick(&mut app, 0);
    tick(&mut app, 0);
    update(
        &mut app,
        Event::Preview {
            path: path("real"),
            result: Ok(vec![
                Message {
                    role: Role::User,
                    text: "please check the scan".into(),
                },
                Message {
                    role: Role::Assistant,
                    text: "Looking.\n[tool: Read]".into(),
                },
            ]),
        },
    );
    let lines = screen(&app);
    let all = lines.join("\n");
    assert!(all.contains("showing 3 of 5 (a: show all)"), "{all}");
    assert!(
        all.contains("enter: resume") && all.contains("f: fork") && all.contains("p: preview"),
        "{all}"
    );
    assert!(!all.contains("M2"), "{all}");
    let (_, head) = line_with(&lines, "TIME");
    assert!(head.contains("ACCOUNTS") && head.contains("TITLE") && head.contains("CWD"));
    let (real_y, real) = line_with(&lines, "real work");
    assert!(real.starts_with("09-24 10:06"), "{real}");
    assert!(real.contains(" - "), "no accounts: {real}");
    assert!(real.contains("~/space/remuda"), "{real}");
    let (a_y, a) = line_with(&lines, "修复索引的增量扫描");
    assert!(a.contains("max,team"), "{a}");
    assert!(a_y > real_y, "newest first");
    assert!(!all.contains("teammate-message"), "{all}");
    // Narrow: the preview is below the list.
    let (p_y, _) = line_with(&lines, "Preview · real work");
    assert!(p_y > a_y, "{all}");
    let (u_y, u) = line_with(&lines, "please check the scan");
    assert!(u.starts_with("› "), "{u}");
    let (_, tool) = line_with(&lines, "[tool: Read]");
    assert!(u_y > p_y && tool.starts_with("  "), "{all}");
}

#[test]
fn history_view_wide_has_preview_beside() {
    let mut app = populated_history();
    update(&mut app, Event::Resize(160, 30));
    let lines = screen(&app);
    let (y, line) = line_with(&lines, "Preview");
    assert_eq!(y, 1, "preview starts at the top\n{}", lines.join("\n"));
    assert!(line.contains("TIME"), "list and preview share rows: {line}");
    let all = lines.join("\n");
    assert!(all.contains("loading preview…"), "{all}");
}

#[test]
fn history_search_prompt_and_no_matches() {
    let mut app = populated_history();
    keys(&mut app, &[Key::Char('/')]);
    type_str(&mut app, "修复");
    let lines = screen(&app);
    let (y, prompt) = line_with(&lines, "/修复▏");
    assert_eq!(y, 1, "{prompt}");
    assert!(lines.join("\n").contains("showing 1 of 5"), "{lines:?}");
    assert!(lines.join("\n").contains("type to filter"), "{lines:?}");
    type_str(&mut app, "zzz");
    keys(&mut app, &[Key::Enter]);
    let all = text(&app);
    assert!(all.contains("filter: 修复zzz"), "{all}");
    assert!(all.contains("no matches for “修复zzz”"), "{all}");
}

#[test]
fn history_preview_error() {
    let mut app = populated_history();
    tick(&mut app, 0);
    tick(&mut app, 0);
    update(
        &mut app,
        Event::Preview {
            path: path("real"),
            result: Err("No such file or directory (os error 2)".into()),
        },
    );
    assert!(text(&app).contains("cannot read transcript: No such file"));
}

#[test]
fn live_view_states() {
    let mut app = app();
    keys(&mut app, &[Key::Char('2')]);
    assert!(text(&app).contains("collecting live sessions…"));
    update(&mut app, Event::Live(vec![]));
    let all = text(&app);
    assert!(all.contains("no running sessions"), "{all}");
    assert!(all.contains("0 running"), "{all}");

    update(
        &mut app,
        Event::IndexLoaded(vec![entry("aaaaaaaa-1111", "indexed one", 1)]),
    );
    let mut idle = live("claude:team", 9, Some("bbbbbbbb-2222"));
    idle.status = Some("idle".into());
    idle.kind = Some("bg".into());
    update(
        &mut app,
        Event::Live(vec![live("claude:max", 7, Some("aaaaaaaa-1111")), idle]),
    );
    let lines = screen(&app);
    let all = lines.join("\n");
    let (_, head) = line_with(&lines, "ACCOUNT");
    for col in ["STATUS", "NAME", "KIND", "STARTED", "CWD", "SESSION"] {
        assert!(head.contains(col), "{head}");
    }
    let (_, max) = line_with(&lines, "busy");
    for cell in ["max", "fix index", "interactive", "3m ago", "aaaaaaaa"] {
        assert!(max.contains(cell), "{cell}: {max}");
    }
    assert!(!max.contains("aaaaaaaa-"), "short id: {max}");
    assert!(all.contains("Preview · fix index"), "{all}");
    assert!(all.contains("loading preview…"), "{all}");
    keys(&mut app, &[Key::Char('j')]);
    let all = text(&app);
    assert!(all.contains("transcript not indexed yet"), "{all}");
}

#[test]
fn help_overlay_lists_every_key() {
    let mut app = app();
    keys(&mut app, &[Key::Char('?')]);
    let all = text(&app);
    assert!(all.contains("Keys"), "{all}");
    for (key, _) in render::KEYS {
        assert!(all.contains(key), "{key}\n{all}");
    }
}

#[test]
fn tiny_terminal_does_not_panic() {
    for (w, h) in [(1, 1), (10, 3), (20, 5), (40, 10)] {
        let mut app = populated_history();
        update(&mut app, Event::Resize(w, h));
        update(
            &mut app,
            Event::Stats {
                report: stats_report(),
                error: Some("disk full".into()),
            },
        );
        for view in ['1', '2', '3', '4', '?'] {
            keys(&mut app, &[Key::Char(view)]);
            screen(&app);
            // Private mode draws a redacted copy (R21).
            keys(&mut app, &[Key::Ctrl('p')]);
            screen(&app);
            keys(&mut app, &[Key::Ctrl('p')]);
        }
    }
}

#[test]
fn accounts_table_stays_compact_on_a_wide_terminal() {
    let mut app = populated_accounts();
    update(
        &mut app,
        Event::CachedUsage {
            account: account("default"),
            result: Ok(CachedUsage {
                fetched_at: Some(ts("2026-09-24T11:34:00Z")),
                rows: vec![row("Session", 1.0, None, None)],
            }),
        },
    );
    update(&mut app, Event::Resize(300, 30));
    let lines = screen(&app);
    let (_, head) = line_with(&lines, "ACCOUNT");
    // Usage sits next to the identity instead of across the screen.
    assert!(head.find("SESSION").unwrap() < 100, "{head}");
    let (_, default) = line_with(&lines, "me@example.com");
    assert!(default.contains("cached 26m ago"), "not cut: {default}");
}

#[test]
fn account_picker_and_notices_render() {
    let mut app = history_with(&["team", "max"]);
    keys(&mut app, &[Key::Enter]);
    let lines = screen(&app);
    let all = lines.join("\n");
    assert!(all.contains("Resume aaaaaaaa as"), "{all}");
    let (max_y, max) = line_with(&lines, "│› max");
    assert!(max.contains("●"), "attributed: {max}");
    let (team_y, team) = line_with(&lines, "│  team");
    assert!(team.contains("●"), "{team}");
    assert!(team.contains("other projects store"), "{team}");
    let (default_y, default) = line_with(&lines, "│  default");
    assert!(!default.contains("●"), "{default}");
    // C4: the accounts that can see the transcript first.
    assert!(max_y < default_y && default_y < team_y, "{all}");
    assert!(all.contains("enter: choose · esc: cancel"), "{all}");

    keys(&mut app, &[Key::Esc]);
    update(&mut app, Event::Live(vec![live("claude:max", 7, Some(A))]));
    keys(&mut app, &[Key::Char('f'), Key::Esc, Key::Enter]);
    let lines = screen(&app);
    let (_, status) = line_with(&lines, "is running in max (pid 7)");
    assert!(status.starts_with(" session aaaaaaaa"), "{status}");
}

#[test]
fn help_lists_the_launch_keys() {
    let keys_text: Vec<&str> = render::KEYS.iter().map(|(k, _)| *k).collect();
    for key in ["enter", "f", "p / space"] {
        assert!(keys_text.contains(&key), "{key}: {keys_text:?}");
    }
    let enter = render::KEYS.iter().find(|(k, _)| *k == "enter").unwrap().1;
    assert!(!enter.contains("M2"), "{enter}");
}

#[test]
fn forms_render_with_their_fields_and_errors() {
    let mut app = new_session_form();
    let all = text(&app);
    assert!(all.contains("New session as max"), "{all}");
    let lines = screen(&app);
    let (dir_y, dir) = line_with(&lines, "Directory");
    assert!(
        dir.contains("~/space/remuda") || dir.contains("remuda▏"),
        "{dir}"
    );
    let (name_y, _) = line_with(&lines, "Name (optional)");
    assert!(name_y == dir_y + 1, "{all}");
    assert!(all.contains("enter: start claude · esc: cancel"), "{all}");
    keys(&mut app, &[Key::Tab]);
    type_str(&mut app, "update");
    keys(&mut app, &[Key::Enter]);
    assert!(
        text(&app).contains("cannot tell this is a new session"),
        "{}",
        text(&app)
    );

    let mut app = idle_app();
    keys(&mut app, &[Key::Char('s')]);
    let all = text(&app);
    assert!(all.contains("Set up a new account"), "{all}");
    assert!(
        all.contains("Account name") && all.contains("Email (optional)"),
        "{all}"
    );
}

// ---- codex (R17) ---------------------------------------------------------------------

const C: &str = "019c1e08-e4f6-7d70-a129-38ec744a3f3c";
const C_SUB: &str = "019c1e09-b0ff-7842-aca4-1397c3b7b047";
const C_CWD: &str = "/w/proj";

fn codex_account(name: &str) -> Account {
    if name == "default" {
        return Account::default_for(CODEX);
    }
    Account {
        provider: CODEX,
        name: name.into(),
        home: Home::Path(format!("/c/{name}")),
    }
}

fn codex_path(id: &str) -> PathBuf {
    PathBuf::from(format!("/c/work/sessions/2026/09/24/rollout-x-{id}.jsonl"))
}

/// A rollout of `codex:work`, last written an hour before [`NOW`].
fn codex_entry(id: &str, title: &str, minute: u32, source: &str) -> Entry {
    Entry {
        provider: CODEX,
        path: codex_path(id),
        store: PathBuf::from("/c/work/sessions"),
        mtime_ns: (ts(NOW) - jiff::SignedDuration::from_hours(1)).as_nanosecond(),
        cwd_first: Some(C_CWD.into()),
        cwd_last: Some(C_CWD.into()),
        entrypoint: None,
        source: Some(source.into()),
        originator: Some("codex-tui".into()),
        ..entry(id, title, minute)
    }
}

/// claude:default, max, codex:default, codex:work; everything loaded; history: claude `A`,
/// codex `C` (cli) and `C_SUB` (a subagent's); `C` selected in History.
fn codex_app() -> App {
    let mut app = App::new(
        vec![
            account("default"),
            account("max"),
            codex_account("default"),
            codex_account("work"),
        ],
        TimeZone::UTC,
        Some("/Users/you".into()),
        ts(NOW),
    );
    update(&mut app, Event::Resize(100, 30));
    app.cwd = Some(PathBuf::from(CWD));
    app.start();
    update(
        &mut app,
        Event::IndexDone {
            entries: vec![
                entry(A, "claude work", 1),
                codex_entry(C, "codex work", 2, "cli"),
                codex_entry(C_SUB, "a subagent", 3, "subagent"),
            ],
            error: None,
        },
    );
    update(&mut app, Event::Live(vec![]));
    let accounts: Vec<Account> = app.accounts.iter().map(|a| a.account.clone()).collect();
    for account in accounts {
        update(
            &mut app,
            Event::Identity {
                account: account.clone(),
                identity: Identity::NotLoggedIn,
            },
        );
        update(
            &mut app,
            Event::CachedUsage {
                account,
                result: Err("no cache".into()),
            },
        );
    }
    update(&mut app, Event::Attribution(Attribution::default()));
    update(&mut app, Event::Checks(vec![]));
    update(
        &mut app,
        Event::Stores(vec![
            stores().remove(0),
            Store {
                provider: CODEX,
                path: PathBuf::from("/c/default/sessions"),
                accounts: vec!["codex:default".into()],
                thread_names: vec![PathBuf::from("/c/default/session_index.jsonl")],
            },
            Store {
                provider: CODEX,
                path: PathBuf::from("/c/work/sessions"),
                accounts: vec!["codex:work".into()],
                thread_names: vec![PathBuf::from("/c/work/session_index.jsonl")],
            },
        ]),
    );
    keys(&mut app, &[Key::Char('3')]);
    assert_eq!(app.selected_entry().unwrap().session_id, C);
    app
}

fn resume_c(verb: &str) -> LaunchRequest {
    LaunchRequest {
        account: codex_account("work"),
        args: vec![verb.into(), C.into(), "-C".into(), C_CWD.into()],
        cwd: Some(PathBuf::from(C_CWD)),
        what: format!("{verb} 019c1e08 as codex:work"),
    }
}

/// R10, R10a: codex accounts have cached and live usage like claude's; the login method comes
/// from `codex login status`, and a live query also tells the email and plan.
#[test]
fn codex_accounts_show_identity_and_usage() {
    let mut app = codex_app();
    // Wide enough for the timeline's legend.
    update(&mut app, Event::Resize(160, 30));
    keys(&mut app, &[Key::Char('1')]);
    let method = Identity::LoggedIn {
        email: None,
        org: None,
        plan: None,
        method: Some("ChatGPT".into()),
        cached: false,
    };
    update(
        &mut app,
        Event::Identity {
            account: codex_account("work"),
            identity: method.clone(),
        },
    );
    update(
        &mut app,
        Event::CachedUsage {
            account: codex_account("work"),
            result: cached(vec![row(
                "Week (all models)",
                99.0,
                None,
                Some("2026-09-26T12:00:00Z"),
            )]),
        },
    );
    let lines = screen(&app);
    let (i, work) = line_with(&lines, "codex:work ");
    for part in ["logged in (ChatGPT)", "99%", "cached"] {
        assert!(work.contains(part), "{part}: {work}");
    }
    let (_, track) = line_with(&lines[i + 1..], "codex:work ");
    assert!(track.contains('W') && track.contains("W 2d"), "{track}");

    // `u` asks every account, codex's too.
    assert_eq!(
        keys(&mut app, &[Key::Char('u')]),
        [Effect::LiveUsage(vec![
            account("default"),
            account("max"),
            codex_account("default"),
            codex_account("work"),
        ])]
    );
    let spark = "GPT-5.3-Codex-Spark";
    update(
        &mut app,
        Event::LiveUsage {
            account: codex_account("work"),
            result: Ok(LiveResult {
                usage: LiveUsage::Rows(vec![
                    row(
                        "Week (all models)",
                        99.0,
                        None,
                        Some("2026-09-26T12:00:00Z"),
                    ),
                    row(
                        &format!("Session ({spark})"),
                        5.0,
                        None,
                        Some("2026-09-24T14:00:00Z"),
                    ),
                    row(
                        &format!("Week ({spark})"),
                        7.0,
                        None,
                        Some("2026-09-28T12:00:00Z"),
                    ),
                ]),
                identity: Some(Identity::LoggedIn {
                    email: Some("c@example.com".into()),
                    org: None,
                    plan: Some("pro".into()),
                    method: Some("ChatGPT".into()),
                    cached: false,
                }),
            }),
        },
    );
    // A live result without an identity leaves the row's alone.
    update(
        &mut app,
        Event::LiveUsage {
            account: codex_account("default"),
            result: Ok(LiveUsage::Rows(vec![row("Session", 1.0, None, None)]).into()),
        },
    );
    let lines = screen(&app);
    let (_, work) = line_with(&lines, "codex:work ");
    for part in ["c@example.com", "pro", "99%", "5%", "7%", "live"] {
        assert!(work.contains(part), "{part}: {work}");
    }
    let (_, header) = line_with(&lines, "ACCOUNT");
    assert!(header.contains(&format!("{spark} 5h")), "{header}");
    let (_, native) = line_with(&lines, "codex:default ");
    assert!(native.contains("not logged in"), "{native}");
    let all = lines.join("\n");
    for part in [format!("g session ({spark})"), format!("g week ({spark})")] {
        assert!(all.contains(&part), "{part}: {all}");
    }
    assert_eq!(all.matches(&format!("g week ({spark})")).count(), 1);

    // A later identity refresh (`codex login status`) shows the login method again.
    update(
        &mut app,
        Event::Identity {
            account: codex_account("work"),
            identity: method,
        },
    );
    let lines = screen(&app);
    let (_, work) = line_with(&lines, "codex:work ");
    assert!(work.contains("logged in (ChatGPT)"), "{work}");
}

#[test]
fn codex_sessions_are_in_history_with_their_account() {
    let mut app = codex_app();
    // The subagent's rollout is noise.
    let all = text(&app);
    assert!(all.contains("showing 2 of 3"), "{all}");
    let lines = screen(&app);
    let (_, c) = line_with(&lines, "codex work");
    assert!(c.contains("codex:work"), "{c}");
    assert!(!all.contains("a subagent"), "{all}");
    keys(&mut app, &[Key::Char('a')]);
    assert!(text(&app).contains("a subagent"));
    // The account is searchable.
    keys(&mut app, &[Key::Char('/')]);
    type_str(&mut app, "codex:work");
    keys(&mut app, &[Key::Enter]);
    assert_eq!(history_ids(&app).len(), 2);
    assert!(
        history_ids(&app)
            .iter()
            .all(|id| id.ends_with(C) || id.ends_with(C_SUB))
    );
    // The preview reads the rollout as codex.
    keys(&mut app, &[Key::Esc]);
    while app.selected_entry().unwrap().session_id != C {
        keys(&mut app, &[Key::Char('j')]);
    }
    tick(&mut app, 0);
    assert_eq!(tick(&mut app, 0), [Effect::Preview(codex_path(C), CODEX)]);
}

/// R17: nothing tells whether a codex session runs elsewhere: an in-place resume always asks
/// (`y` goes on, anything else cancels), under the rollout's own account, in its directory.
#[test]
fn resuming_a_codex_session_asks_first() {
    let mut app = codex_app();
    // Asking also reads the rollout's mtime: the prompt says whether it was just written.
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [Effect::RolloutWritten(codex_path(C))]
    );
    assert!(
        matches!(app.overlay, Some(Overlay::ResumeCodex(_))),
        "{:?}",
        app.overlay
    );
    let all = text(&app);
    assert!(all.contains("cannot confirm"), "{all}");
    assert!(!all.contains("being written"), "{all}");
    assert_eq!(keys(&mut app, &[Key::Char('n')]), []);
    assert_eq!(app.overlay, None);
    assert_eq!(notice(&app), Some(("cancelled", Level::Info)));

    keys(&mut app, &[Key::Enter]);
    assert_eq!(
        keys(&mut app, &[Key::Char('y')]),
        [check_of(&app, resume_c("resume"))]
    );
    assert_eq!(app.overlay, None);
    // No live check for codex: only the directory counts.
    assert_eq!(
        answer(&mut app, resume_c("resume"), None),
        [Effect::Launch(resume_c("resume"))]
    );
}

#[test]
fn the_prompt_says_when_the_rollout_was_just_written() {
    let mut app = codex_app();
    let mut fresh = codex_entry(C, "codex work", 2, "cli");
    fresh.mtime_ns = (ts(NOW) - jiff::SignedDuration::from_mins(3)).as_nanosecond();
    update(
        &mut app,
        Event::IndexProgress {
            done: 1,
            total: 1,
            entries: vec![fresh],
        },
    );
    keys(&mut app, &[Key::Enter]);
    let all = text(&app);
    assert!(all.contains("it is still being written"), "{all}");
    assert!(all.contains("last written 3m ago"), "{all}");
}

/// The prompt takes the rollout's own mtime, read when it opens, not the index's (a refresh
/// may queue behind a long scan).
#[test]
fn the_prompt_reads_the_rollouts_own_mtime() {
    let mut app = codex_app();
    keys(&mut app, &[Key::Enter]);
    assert!(!text(&app).contains("being written"));
    let written = |app: &mut App, path: PathBuf, mins: i64| {
        let at = Some(ts(NOW) - jiff::SignedDuration::from_mins(mins));
        update(app, Event::RolloutWritten { path, at })
    };
    // Another rollout's answer (from an earlier prompt) is not this one's.
    written(&mut app, codex_path(C_SUB), 1);
    assert!(!text(&app).contains("being written"));
    written(&mut app, codex_path(C), 2);
    let all = text(&app);
    assert!(all.contains("it is still being written"), "{all}");
    assert!(all.contains("last written 2m ago"), "{all}");
    // An unreadable file keeps what was known.
    update(
        &mut app,
        Event::RolloutWritten {
            path: codex_path(C),
            at: None,
        },
    );
    assert!(text(&app).contains("last written 2m ago"));
    // Without a prompt, nothing happens.
    keys(&mut app, &[Key::Char('n')]);
    assert_eq!(written(&mut app, codex_path(C), 1), []);
    assert_eq!(app.overlay, None);
}

#[test]
fn forking_a_codex_session_does_not_ask() {
    let mut app = codex_app();
    assert_eq!(
        keys(&mut app, &[Key::Char('f')]),
        [check_of(&app, resume_c("fork"))]
    );
    assert_eq!(app.overlay, None);
}

/// The rollout's home is its account: no picker, and no other account can resume it.
#[test]
fn a_codex_session_resumes_only_as_the_account_of_its_home() {
    let mut app = codex_app();
    // codex:work was unregistered meanwhile.
    let accounts: Vec<Account> = app.accounts[..3]
        .iter()
        .map(|a| a.account.clone())
        .collect();
    update(&mut app, Event::Accounts(accounts));
    keys(&mut app, &[Key::Char('f')]);
    assert_eq!(app.overlay, None);
    assert_eq!(app.pending, None);
    assert_eq!(
        notice(&app),
        Some((
            "session 019c1e08 is in /c/work/sessions, which no registered account has",
            Level::Error
        ))
    );
}

#[test]
fn a_new_codex_session_has_no_name_and_takes_its_directory_with_c() {
    let mut app = codex_app();
    keys(&mut app, &[Key::Char('1'), Key::Char('G'), Key::Char('n')]);
    assert_eq!(
        form(&app).kind,
        FormKind::NewSession {
            account: codex_account("work")
        }
    );
    assert_eq!(values(&app), [CWD]);
    let want = LaunchRequest {
        account: codex_account("work"),
        args: vec!["-C".into(), CWD.into()],
        cwd: Some(PathBuf::from(CWD)),
        what: "new session as codex:work".into(),
    };
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [check_of(&app, want.clone())]
    );
    assert_eq!(answer(&mut app, want.clone(), None), [Effect::Launch(want)]);
}

#[test]
fn the_setup_form_chooses_the_provider() {
    let mut app = codex_app();
    keys(&mut app, &[Key::Char('1'), Key::Char('s')]);
    assert_eq!(values(&app), ["", "", "claude"]);
    // codex:work exists; claude:work does not.
    type_str(&mut app, "work");
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [Effect::Setup {
            provider: CLAUDE,
            name: "work".into(),
            email: None
        }]
    );
    keys(&mut app, &[Key::Char('s')]);
    type_str(&mut app, "work");
    keys(&mut app, &[Key::Tab, Key::Tab]);
    clear_field(&mut app);
    type_str(&mut app, "codex");
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    assert!(
        form(&app)
            .error
            .as_deref()
            .unwrap()
            .contains("codex:work is already registered"),
        "{:?}",
        form(&app).error
    );
    keys(&mut app, &[Key::BackTab]);
    type_str(&mut app, "me@example.com");
    keys(&mut app, &[Key::BackTab]);
    clear_field(&mut app);
    type_str(&mut app, "solo");
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    assert!(
        form(&app)
            .error
            .as_deref()
            .unwrap()
            .contains("takes no email")
    );
    keys(&mut app, &[Key::Tab]);
    clear_field(&mut app);
    keys(&mut app, &[Key::Tab]);
    clear_field(&mut app);
    type_str(&mut app, "gemini");
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    assert!(
        form(&app)
            .error
            .as_deref()
            .unwrap()
            .contains("unknown provider")
    );
    clear_field(&mut app);
    type_str(&mut app, "codex");
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [Effect::Setup {
            provider: CODEX,
            name: "solo".into(),
            email: None
        }]
    );
}

#[test]
fn a_finished_codex_setup_names_codex_login() {
    let mut app = codex_app();
    update(
        &mut app,
        Event::SetupDone {
            provider: CODEX,
            name: "solo".into(),
            result: Ok(Exit::Code(1)),
        },
    );
    assert_eq!(
        notice(&app),
        Some((
            "set up solo: codex login exited 1; codex:solo stays registered, retry with: \
             remuda run codex:solo login",
            Level::Warn
        ))
    );
}

#[test]
fn live_view_says_codex_sessions_cannot_be_listed() {
    let mut app = codex_app();
    keys(&mut app, &[Key::Char('2')]);
    let all = text(&app);
    assert!(
        all.contains("codex sessions can't be listed as running"),
        "{all}"
    );
    // Without codex accounts there is nothing to say.
    let mut app = idle_app();
    keys(&mut app, &[Key::Char('2')]);
    assert!(!text(&app).contains("codex"));
}

/// A foreground child makes the answer to the confirmation stale, like a pending check.
#[test]
fn a_foreground_child_closes_a_codex_confirmation() {
    let mut app = codex_app();
    keys(&mut app, &[Key::Enter]);
    assert!(matches!(app.overlay, Some(Overlay::ResumeCodex(_))));
    // Not reachable by keys (the prompt takes them): put a pending launch there directly.
    let attach = request(
        "max",
        &["attach", "0badf00d"],
        None,
        "attach 0badf00d as max",
    );
    let check = app.launch_checks + 1;
    app.launch_checks = check;
    app.pending = Some((check, attach.clone()));
    answer(&mut app, attach, None);
    assert_eq!(app.overlay, None);
}

// ---- accounts held by name; shared codex stores ----------------------------------

/// The accounts of [`codex_app`] with `claude:team` added (a claude account added by
/// another terminal lands before every codex account: the list is grouped by provider).
fn with_team(app: &App) -> Vec<Account> {
    let mut accounts: Vec<Account> = app.accounts.iter().map(|a| a.account.clone()).collect();
    accounts.insert(2, account("team"));
    accounts
}

/// The new-session form keeps its account, not its row: a registry change while it is
/// open (a pre-launch check reads the registry again) moves the rows, not the account.
#[test]
fn form_index_shift() {
    let mut app = codex_app();
    keys(&mut app, &[Key::Char('1'), Key::Char('G'), Key::Char('n')]);
    let accounts = with_team(&app);
    update(&mut app, Event::Accounts(accounts));
    let want = LaunchRequest {
        account: codex_account("work"),
        args: vec!["-C".into(), CWD.into()],
        cwd: Some(PathBuf::from(CWD)),
        what: "new session as codex:work".into(),
    };
    assert_eq!(keys(&mut app, &[Key::Enter]), [check_of(&app, want)]);

    // The account went away meanwhile: refused, whatever row took its place.
    let mut app = codex_app();
    keys(&mut app, &[Key::Char('1'), Key::Char('G'), Key::Char('n')]);
    let mut accounts = with_team(&app);
    accounts.retain(|a| a.name != "work");
    update(&mut app, Event::Accounts(accounts));
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    assert_eq!(app.pending, None);
    assert_eq!(
        form(&app).error.as_deref(),
        Some("codex:work is no longer registered")
    );
}

/// The account picker keeps accounts, not rows: the chosen one is resolved by name, and
/// refused once it is gone.
#[test]
fn pick_resolves_accounts_by_name() {
    // Picker: max, default (both see /s), team.
    let mut app = history_with(&["team", "max"]);
    keys(&mut app, &[Key::Enter]);
    assert!(matches!(app.overlay, Some(Overlay::Pick(_))));
    // max is unregistered meanwhile: its row is now team's.
    update(
        &mut app,
        Event::Accounts(vec![account("default"), account("team")]),
    );
    let lines = screen(&app);
    let (_, max) = line_with(&lines, "│› max");
    assert!(max.contains("no longer registered"), "{max}");
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    assert_eq!(app.pending, None);
    assert_eq!(
        notice(&app),
        Some(("max is no longer registered", Level::Error))
    );

    // An account added before the chosen one does not change what is chosen.
    let mut app = history_with(&[]);
    keys(&mut app, &[Key::Enter, Key::Char('j')]);
    update(
        &mut app,
        Event::Accounts(vec![
            account("default"),
            account("alpha"),
            account("max"),
            account("team"),
        ]),
    );
    assert_eq!(
        keys(&mut app, &[Key::Enter]),
        [check_of(&app, resume_a("max"))]
    );
}

/// Codex homes whose `sessions` resolve to one directory share every rollout: the
/// rollout does not say which account it is, so the user picks among those accounts (and
/// still confirms a resume in place).
fn shared_codex_store(app: &mut App) {
    update(
        app,
        Event::Stores(vec![
            stores().remove(0),
            Store {
                provider: CODEX,
                path: PathBuf::from("/c/work/sessions"),
                accounts: vec!["codex:default".into(), "codex:work".into()],
                thread_names: vec![PathBuf::from("/c/default/session_index.jsonl")],
            },
        ]),
    );
}

#[test]
fn a_shared_codex_store_asks_which_account() {
    let mut app = codex_app();
    shared_codex_store(&mut app);
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    assert!(
        matches!(app.overlay, Some(Overlay::Pick(_))),
        "{:?}",
        app.overlay
    );
    let all = text(&app);
    assert!(all.contains("Resume 019c1e08 as"), "{all}");
    // codex:work, the second: the prompt follows the pick.
    keys(&mut app, &[Key::Char('j'), Key::Enter]);
    let Some(Overlay::ResumeCodex(confirm)) = &app.overlay else {
        panic!("{:?}", app.overlay)
    };
    assert_eq!(confirm.request, resume_c("resume"));
    assert_eq!(
        keys(&mut app, &[Key::Char('y')]),
        [check_of(&app, resume_c("resume"))]
    );

    // A fork picks too, and does not ask.
    let mut app = codex_app();
    shared_codex_store(&mut app);
    assert_eq!(keys(&mut app, &[Key::Char('f')]), []);
    assert!(matches!(app.overlay, Some(Overlay::Pick(_))));
    assert_eq!(
        keys(&mut app, &[Key::Char('j'), Key::Enter]),
        [check_of(&app, resume_c("fork"))]
    );
    let mut default = resume_c("fork");
    default.account = codex_account("default");
    default.what = "fork 019c1e08 as codex:default".into();
    keys(&mut app, &[Key::Esc]);
    keys(&mut app, &[Key::Char('f')]);
    assert_eq!(keys(&mut app, &[Key::Enter]), [check_of(&app, default)]);

    // The stores were read again while the picker was open: codex:default no longer shares
    // codex:work's; and a claude account never resumes a rollout.
    let mut app = codex_app();
    shared_codex_store(&mut app);
    keys(&mut app, &[Key::Char('f')]);
    let Some(Overlay::Pick(pick)) = &app.overlay else {
        panic!("{:?}", app.overlay)
    };
    let pick = pick.clone();
    update(
        &mut app,
        Event::Stores(vec![
            stores().remove(0),
            Store {
                provider: CODEX,
                path: PathBuf::from("/c/default/sessions"),
                accounts: vec!["codex:default".into()],
                thread_names: vec![],
            },
        ]),
    );
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    assert_eq!(
        notice(&app),
        Some((
            "codex:default cannot find session 019c1e08: the rollout is in /c/work/sessions, \
             which is not codex:default's sessions store",
            Level::Error
        ))
    );
    let mut claude = pick;
    claude.options = vec!["claude:max".into()];
    app.overlay = Some(Overlay::Pick(claude));
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    assert_eq!(app.pending, None);
    assert_eq!(
        notice(&app),
        Some((
            "max is not a codex account: it cannot resume codex session 019c1e08",
            Level::Error
        ))
    );
}

/// A claude session's picker offers claude accounts only.
#[test]
fn pick_offers_only_the_sessions_provider() {
    let mut app = codex_app();
    while app.selected_entry().unwrap().session_id != A {
        keys(&mut app, &[Key::Char('j')]);
    }
    keys(&mut app, &[Key::Enter]);
    let Some(Overlay::Pick(pick)) = &app.overlay else {
        panic!("{:?}", app.overlay)
    };
    assert_eq!(pick.options, ["claude:default", "claude:max"]);
    // Defensively, a codex account cannot resume a claude session even if offered.
    let mut pick = pick.clone();
    pick.options = vec!["codex:work".into()];
    app.overlay = Some(Overlay::Pick(pick));
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    assert_eq!(app.pending, None);
    assert_eq!(
        notice(&app),
        Some((
            "codex:work is not a claude account: it cannot resume claude session aaaaaaaa",
            Level::Error
        ))
    );
}

/// A codex prompt keeps its account too: gone by the time of `y`, it is refused.
#[test]
fn codex_prompt_account_gone_before_yes() {
    let mut app = codex_app();
    keys(&mut app, &[Key::Enter]);
    assert!(matches!(app.overlay, Some(Overlay::ResumeCodex(_))));
    let accounts: Vec<Account> = app.accounts[..3]
        .iter()
        .map(|a| a.account.clone())
        .collect();
    update(&mut app, Event::Accounts(accounts));
    assert_eq!(keys(&mut app, &[Key::Char('y')]), []);
    assert_eq!(app.pending, None);
    assert_eq!(
        notice(&app),
        Some(("codex:work is no longer registered", Level::Error))
    );
}

/// With `codex:work` registered, a bare `work` is ambiguous: the retry names
/// `claude:work`.
#[test]
fn setup_retry_is_unambiguous() {
    let mut app = codex_app();
    let mut accounts: Vec<Account> = app.accounts.iter().map(|a| a.account.clone()).collect();
    accounts.insert(2, account("work"));
    update(&mut app, Event::Accounts(accounts));
    update(
        &mut app,
        Event::SetupDone {
            provider: CLAUDE,
            name: "work".into(),
            result: Ok(Exit::Code(1)),
        },
    );
    assert_eq!(
        notice(&app),
        Some((
            "set up work: claude auth login exited 1; claude:work stays registered, retry \
             with: remuda run claude:work auth login",
            Level::Warn
        ))
    );
}

// ---- relay (R19) ------------------------------------------------------------------------

fn relay_a(account_name: &str) -> Effect {
    Effect::Relay {
        request: request(
            account_name,
            &["--resume", A, "--fork-session"],
            Some(CWD),
            &format!("continue aaaaaaaa as {account_name}"),
        ),
        source: crate::relay::Source {
            transcript: path(A),
            store: PathBuf::from("/s"),
            session_id: A.into(),
            cwd_last: Some(CWD.into()),
        },
    }
}

/// R16, R19: `c` offers the claude accounts whose store does not hold the transcript, and
/// the choice copies and forks it in the foreground, with no pre-launch check to wait for.
#[test]
fn c_continues_the_session_under_another_account() {
    let mut app = history_with(&["max"]);
    assert_eq!(keys(&mut app, &[Key::Char('c')]), []);
    let Some(Overlay::Pick(pick)) = &app.overlay else {
        panic!("{:?}", app.overlay)
    };
    assert_eq!(pick.action, super::app::PickFor::Relay);
    assert_eq!(pick.options, ["claude:team"]);
    assert!(pick.attributed.is_empty());
    let text = screen(&app).join("\n");
    assert!(text.contains("Continue aaaaaaaa as"), "{text}");
    assert_eq!(keys(&mut app, &[Key::Enter]), [relay_a("team")]);
    assert_eq!(app.overlay, None);
    assert_eq!(app.pending, None);
}

/// Plain `c` is the relay; Ctrl-C still quits, from the picker too.
#[test]
fn plain_c_relays_and_ctrl_c_still_quits() {
    let mut app = history_with(&["max"]);
    assert_eq!(keys(&mut app, &[Key::Ctrl('c')]), [Effect::Quit]);
    assert_eq!(keys(&mut app, &[Key::Char('c')]), []);
    assert!(matches!(app.overlay, Some(Overlay::Pick(_))));
    assert_eq!(keys(&mut app, &[Key::Ctrl('c')]), [Effect::Quit]);
}

/// R19: a running session may be continued elsewhere, like a fork; from Live, the copy in
/// its account's store is the one relayed.
#[test]
fn c_in_live_relays_a_running_session() {
    let mut app = history_with(&["max"]);
    update(&mut app, Event::Live(vec![live("claude:max", 7, Some(A))]));
    keys(&mut app, &[Key::Char('2')]);
    assert_eq!(keys(&mut app, &[Key::Char('c')]), []);
    assert_eq!(keys(&mut app, &[Key::Enter]), [relay_a("team")]);
}

/// R19: nothing to relay to when every claude account sees the store; the stores read again
/// while the picker is open are honored; codex sessions are not relayed.
#[test]
fn c_refusals() {
    let mut app = history_with(&["max"]);
    let mut shared = stores();
    shared[0].accounts.push("claude:team".into());
    shared.remove(1);
    update(&mut app, Event::Stores(shared.clone()));
    assert_eq!(keys(&mut app, &[Key::Char('c')]), []);
    assert_eq!(app.overlay, None);
    assert_eq!(
        notice(&app),
        Some((
            "every claude account can already find session aaaaaaaa: fork it instead (f)",
            Level::Warn
        ))
    );

    let mut app = history_with(&["max"]);
    keys(&mut app, &[Key::Char('c')]);
    update(&mut app, Event::Stores(shared));
    assert_eq!(keys(&mut app, &[Key::Enter]), []);
    assert_eq!(
        notice(&app),
        Some((
            "team can already find session aaaaaaaa: fork it instead (f)",
            Level::Warn
        ))
    );

    let mut app = codex_app();
    while app.selected_entry().unwrap().session_id != C {
        keys(&mut app, &[Key::Char('j')]);
    }
    assert_eq!(keys(&mut app, &[Key::Char('c')]), []);
    assert_eq!(app.overlay, None);
    assert_eq!(
        notice(&app),
        Some((
            "session 019c1e08 is a codex session: only claude sessions continue under another \
             account",
            Level::Warn
        ))
    );
}

/// R19: a transcript the launch log records as a relay copy is not listed in History.
#[test]
fn relay_copies_are_hidden_from_history() {
    let mut app = history_with(&["max"]);
    assert_eq!(history_ids(&app), [B, A]);
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("launches.jsonl");
    std::fs::write(
        &log,
        format!(
            "{}\n{}\n",
            serde_json::json!({"account": "claude:max", "session_id": A}),
            serde_json::json!({
                "account": "claude:team", "session_id": "cccccccc-0000-4000-8000-000000000000",
                "fork_of": B,
                "relay": {"source": "/t/-w/x.jsonl", "transcript": path(B), "checkpoints": [],
                          "size": 10, "mtime_ns": 0},
            })
        ),
    )
    .unwrap();
    let mut attribution = Attribution::default();
    attribution.add_launch_log(&log);
    update(&mut app, Event::Attribution(attribution));
    assert_eq!(history_ids(&app), [A]);
    assert_eq!(app.unfiltered_count(), 1);
    assert_eq!(
        app.entry_accounts(app.selected_entry().unwrap()),
        ["claude:max"]
    );
}

// ---- Stats (R20) ------------------------------------------------------------------

/// A model row whose cache write is all 1-hour, not priced.
fn model(provider: crate::provider::Provider, name: &str, t: [u64; 5]) -> ModelRow {
    let [input, cache_read, cache_write_1h, output, reasoning] = t;
    let tokens = Tokens {
        input,
        cache_read,
        cache_write_5m: 0,
        cache_write_1h,
        output,
        reasoning,
    };
    ModelRow {
        provider,
        model: name.into(),
        tokens,
        cost: Cost {
            pico_usd: 0,
            unpriced_tokens: tokens.total(),
        },
    }
}

/// `m` priced at `pico` picodollars.
fn priced(m: ModelRow, pico: u128) -> ModelRow {
    ModelRow {
        cost: Cost {
            pico_usd: pico,
            unpriced_tokens: 0,
        },
        ..m
    }
}

fn stats_section(accounts: &[&str], models: Vec<ModelRow>) -> Section {
    Section {
        accounts: accounts.iter().map(|a| a.to_string()).collect(),
        models,
    }
}

/// The same sections in every period, but today's has only `default`.
fn stats_report() -> Report {
    let test = model(CLAUDE, "claude-test", [8, 1_234_567, 160, 90, 0]);
    let haiku = priced(
        model(CLAUDE, "claude-haiku-4-5-20251001", [30, 0, 0, 5, 0]),
        12_340_000_000_000,
    );
    let gpt = model(CODEX, "gpt-test", [1500, 200, 0, 30, 12]);
    let sections = vec![
        stats_section(&["claude:default"], vec![test.clone(), haiku.clone()]),
        stats_section(&["claude:max"], vec![]),
        stats_section(&["claude:team"], vec![]),
        stats_section(&["codex:work"], vec![gpt.clone()]),
        stats_section(
            &["claude:default", "claude:max"],
            vec![model(CLAUDE, "claude-test", [5, 0, 0, 5, 0])],
        ),
        stats_section(&[], vec![model(CLAUDE, "claude-test", [9, 0, 0, 9, 0])]),
    ];
    let overall = vec![
        model(CLAUDE, "claude-test", [22, 1_234_567, 160, 104, 0]),
        gpt,
        haiku,
    ];
    Report {
        tables: Period::ALL
            .map(|period| Table {
                period,
                since: None,
                sections: match period {
                    Period::Today => sections[..1].to_vec(),
                    _ => sections.clone(),
                },
                overall: overall.clone(),
                series: vec![],
            })
            .to_vec(),
        files: 42,
    }
}

/// The app in Stats with the report computed.
fn stats_app() -> App {
    let mut app = app();
    keys(&mut app, &[Key::Char('4')]);
    update(
        &mut app,
        Event::Stats {
            report: stats_report(),
            error: None,
        },
    );
    app
}

fn squeezed(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// R20: the statistics are computed the first time Stats opens (never at start), then on each
/// `r`; never twice at once, whatever is pressed meanwhile.
#[test]
fn stats_view_computes_on_first_visit_then_on_r() {
    let mut app = app();
    assert!(!app.start().contains(&Effect::Stats));
    assert!(!keys(&mut app, &[Key::Char('r')]).contains(&Effect::Stats));
    assert_eq!(keys(&mut app, &[Key::Char('4')]), [Effect::Stats]);
    assert!(app.stats.in_flight);
    let all = text(&app);
    assert!(all.contains("computing…"), "{all}");
    let again = keys(
        &mut app,
        &[
            Key::Char('4'),
            Key::Char('1'),
            Key::Char('4'),
            Key::Tab,
            Key::BackTab,
            Key::Char('r'),
            Key::Char('r'),
        ],
    );
    assert!(!again.contains(&Effect::Stats), "{again:?}");

    update(&mut app, Event::StatsProgress { done: 3, total: 10 });
    let lines = screen(&app);
    line_with(
        &lines,
        "Tokens · all time · cost ≈ API list price (t: period)",
    );
    let (body, _) = line_with(&lines, "reading transcripts 3/10…");
    let (status, _) = line_with(&lines[body + 1..], "reading transcripts 3/10…");
    assert_eq!(body + 1 + status, 22, "and in the status line");

    update(
        &mut app,
        Event::Stats {
            report: stats_report(),
            error: None,
        },
    );
    assert!(!app.stats.in_flight);
    let all = text(&app);
    assert!(all.contains("computed 12:00:00 · 42 transcripts"), "{all}");
    assert!(!keys(&mut app, &[Key::Char('1'), Key::Char('4')]).contains(&Effect::Stats));
    // `r` computes again, once, from any view; the last report stays shown meanwhile.
    keys(&mut app, &[Key::Char('1')]);
    let fx = keys(&mut app, &[Key::Char('r'), Key::Char('r')]);
    assert_eq!(
        fx.iter().filter(|e| **e == Effect::Stats).count(),
        1,
        "{fx:?}"
    );
    keys(&mut app, &[Key::Char('4')]);
    update(&mut app, Event::StatsProgress { done: 0, total: 5 });
    let all = text(&app);
    assert!(all.contains("claude-test"), "{all}");
    assert!(all.contains("reading transcripts 0/5…"), "{all}");

    // A cache that cannot be written is told, the report still shown.
    update(
        &mut app,
        Event::Stats {
            report: stats_report(),
            error: Some("stats cache: disk full".into()),
        },
    );
    // The prices' date gives way to the error, which fits at 80 columns.
    let all = text(&app);
    assert!(
        all.contains("42 transcripts │ stats cache: disk full"),
        "{all}"
    );
    assert!(!all.contains("prices as of"), "{all}");
    assert!(all.contains("claude-test"), "{all}");
}

/// R20: every section, bold totals, models with `…` when too long, `-` for counts a provider
/// does not record and for a cost not priced, `no tokens` for an empty account, then Overall
/// and the models not priced. At 80 columns REASONING gives way.
#[test]
fn stats_view_shows_sections_models_and_totals() {
    let app = stats_app();
    let lines = screen(&app);
    let rows: Vec<String> = lines[2..].iter().map(|l| squeezed(l)).collect();
    let expected = [
        "ACCOUNT / MODEL INPUT CACHE READ CACHE WRITE OUTPUT TOTAL COST SHARE",
        "default 38 1.2M 160 95 1.2M $12.34+ ██████",
        "claude-test 8 1.2M 160 90 1.2M -",
        "claude-haiku-4… 30 0 0 5 35 $12.34",
        "max no tokens",
        "team no tokens",
        "codex:work 1.5K 200 - 30 1.7K -",
        "gpt-test 1.5K 200 - 30 1.7K -",
        "default + max 5 0 0 5 10 -",
        "claude-test 5 0 0 5 10 -",
        "unattributed 9 0 0 9 18 -",
        "claude-test 9 0 0 9 18 -",
        "",
        "Overall 1.6K 1.2M 160 139 1.2M $12.34+",
        "claude-test 22 1.2M 160 104 1.2M -",
        "gpt-test 1.5K 200 - 30 1.7K -",
        "claude-haiku-4… 30 0 0 5 35 $12.34",
        "not priced: claude-test, gpt-test",
    ];
    assert_eq!(rows[..expected.len()], expected, "{}", lines.join("\n"));
    // The numbers line up under their headers.
    let header = &lines[2];
    let (_, gpt) = line_with(&lines, "gpt-test");
    assert_eq!(
        gpt.find("1.7K").unwrap() + 4,
        header.find("TOTAL").unwrap() + 5
    );
    let (_, name) = line_with(&lines, "claude-haiku");
    assert!(name.starts_with("  claude-haiku-4… "), "{name}");
}

/// Where each run of non-blank characters ends, in columns (every character here is one
/// column wide).
fn token_ends(line: &str) -> Vec<usize> {
    let chars: Vec<char> = line.chars().collect();
    (0..chars.len())
        .filter(|&i| chars[i] != ' ' && chars.get(i + 1).is_none_or(|c| *c == ' '))
        .map(|i| i + 1)
        .collect()
}

/// R20: every count ends in the column its header ends in, in every row, whatever its width
/// (`-`, `0`, `512K`, `1.2M`), with long names cut, at every width.
#[test]
fn stats_columns_align_in_every_row() {
    let mut app = stats_app();
    let mut report = stats_report();
    for table in &mut report.tables {
        table.sections.push(stats_section(
            &["codex:other"],
            vec![
                model(
                    CODEX,
                    "gpt-5.3-codex",
                    [8_300_000, 161_000_000, 0, 808_000, 512_000],
                ),
                model(
                    CODEX,
                    "gpt-5.2",
                    [5_700_000, 225_000_000, 0, 1_600_000, 1_200_000],
                ),
            ],
        ));
    }
    update(
        &mut app,
        Event::Stats {
            report,
            error: None,
        },
    );
    // Numeric columns shown (SHARE, a bar, is not one of them).
    for (width, columns) in [(80, 6), (75, 6), (70, 5), (60, 5), (160, 7)] {
        update(&mut app, Event::Resize(width, render::TALL - 1));
        let lines = screen(&app);
        let header = &lines[2];
        let ends: Vec<usize> = [
            "INPUT",
            "CACHE READ",
            "CACHE WRITE",
            "OUTPUT",
            "REASONING",
            "TOTAL",
            "COST",
        ]
        .iter()
        .filter_map(|h| {
            header
                .find(h)
                .map(|at| header[..at].chars().count() + h.len())
        })
        .collect();
        assert_eq!(ends.len(), columns, "{width}: {header}");
        let share = header.find("SHARE").map(|at| header[..at].chars().count());
        let rows: Vec<String> = lines[3..]
            .iter()
            .filter(|l| !l.trim().is_empty() && !l.contains("no tokens"))
            .take_while(|l| !l.starts_with(" computed") && !l.contains("not priced"))
            .map(|l| l.chars().take(share.unwrap_or(usize::MAX)).collect())
            .collect();
        assert_eq!(rows.len(), 16, "{width}: {rows:?}");
        for row in &rows {
            let cells = token_ends(row);
            assert_eq!(
                cells[cells.len() - columns..],
                ends[..],
                "{width}: {row:?}\n{}",
                lines.join("\n")
            );
        }
    }
}

/// R20: narrow terminals give up REASONING, then SHARE, then CACHE WRITE, before model names
/// are cut short, never COST; a wide one keeps the numbers next to the names.
#[test]
fn stats_view_degrades_on_narrow_terminals() {
    let mut app = stats_app();
    for (width, dropped) in [
        (90, &[][..]),
        (80, &["REASONING"][..]),
        (75, &["REASONING", "SHARE"][..]),
        (70, &["REASONING", "SHARE", "CACHE WRITE"][..]),
    ] {
        update(&mut app, Event::Resize(width, 24));
        let lines = screen(&app);
        let header = &lines[2];
        for column in [
            "INPUT",
            "CACHE READ",
            "CACHE WRITE",
            "OUTPUT",
            "REASONING",
            "TOTAL",
            "COST",
            "SHARE",
        ] {
            assert_eq!(
                header.contains(column),
                !dropped.contains(&column),
                "{width}: {header}"
            );
        }
        let (_, test) = line_with(&lines, "  claude-test");
        assert!(test.contains("1.2M"), "{test}");
    }
    update(&mut app, Event::Resize(160, 40));
    let lines = screen(&app);
    assert!(lines[2].trim_end().len() < 100, "{}", lines[2]);
    let (_, haiku) = line_with(&lines, "claude-haiku-4-5-20251001 ");
    assert!(haiku.contains("35"), "not cut: {haiku}");
}

/// R20: `t` cycles the period (all, today, 7 days, 30 days) in Stats only; the preview keys do
/// nothing there, and neither do the session keys.
#[test]
fn t_cycles_the_period_and_p_is_inert_in_stats() {
    let mut app = stats_app();
    let mut titles = Vec::new();
    for _ in 0..5 {
        let lines = screen(&app);
        let (_, title) = line_with(&lines, "Tokens · ");
        titles.push(
            title
                .split(" (t: period)")
                .next()
                .unwrap()
                .trim()
                .to_string(),
        );
        keys(&mut app, &[Key::Char('t')]);
    }
    assert_eq!(
        titles,
        [
            "Tokens · all time · cost ≈ API list price",
            "Tokens · today · cost ≈ API list price",
            "Tokens · last 7 days · cost ≈ API list price",
            "Tokens · last 30 days · cost ≈ API list price",
            "Tokens · all time · cost ≈ API list price"
        ]
    );
    assert_eq!(app.stats.period, Period::Today);
    let all = text(&app);
    assert!(!all.contains("codex:work"), "today has only default: {all}");
    keys(&mut app, &[Key::Char('t'), Key::Char('t'), Key::Char('t')]);
    assert_eq!(app.stats.period, Period::All);

    let before = text(&app);
    for k in [
        Key::Char('p'),
        Key::Char(' '),
        Key::Enter,
        Key::Char('f'),
        Key::Char('c'),
    ] {
        assert_eq!(keys(&mut app, &[k]), [], "{k:?}");
    }
    assert!(!app.preview.expanded);
    assert_eq!(text(&app), before);
    assert!(
        before.contains("t: period · j/k: scroll · r: refresh"),
        "{before}"
    );

    // Elsewhere `t` is not the period key.
    keys(&mut app, &[Key::Char('1'), Key::Char('t')]);
    assert_eq!(app.stats.period, Period::All);
}

/// R20: movement keys scroll the Stats view within its lines; the title stays, and the column
/// header once scrolled past takes the first line's place.
#[test]
fn stats_scrolls() {
    let mut app = stats_app();
    update(&mut app, Event::Resize(80, 12));
    let height = render::stats_height(&app);
    let count = render::stats_line_count(&app);
    assert_eq!((height, count), (8, 18));
    let first_row = |app: &App| squeezed(&screen(app)[3]);
    assert_eq!(
        first_row(&app),
        "default 38 1.2M 160 95 1.2M $12.34+ ██████"
    );
    keys(&mut app, &[Key::Char('j'), Key::Down]);
    assert_eq!(app.stats.scroll, 2);
    assert_eq!(first_row(&app), "claude-haiku-4… 30 0 0 5 35 $12.34");
    let lines = screen(&app);
    line_with(&lines, "Tokens · all time");
    assert!(lines[2].starts_with("ACCOUNT / MODEL"), "{}", lines[2]);
    keys(&mut app, &[Key::Char('j')]);
    line_with(&screen(&app), "ACCOUNT / MODEL");
    keys(&mut app, &[Key::Char('G')]);
    assert_eq!(app.stats.scroll, count - height);
    let lines = screen(&app);
    line_with(&lines, "Overall");
    keys(&mut app, &[Key::Char('j'), Key::PageDown]);
    assert_eq!(app.stats.scroll, count - height, "bounded");
    keys(&mut app, &[Key::PageUp]);
    assert_eq!(app.stats.scroll, count - 2 * height);
    keys(&mut app, &[Key::Char('g')]);
    assert_eq!(app.stats.scroll, 0);
    keys(&mut app, &[Key::Char('G'), Key::Char('t')]);
    assert_eq!(app.stats.scroll, 0, "a new period starts at the top");
    // A taller terminal needs less scrolling.
    keys(
        &mut app,
        &[
            Key::Char('t'),
            Key::Char('t'),
            Key::Char('t'),
            Key::Char('G'),
        ],
    );
    update(&mut app, Event::Resize(80, 40));
    assert_eq!(app.stats.scroll, 0);
}

/// A report with the same `sections`, `overall` and `series` in every period.
fn report_of(sections: Vec<Section>, overall: Vec<ModelRow>, series: Vec<stats::Bucket>) -> Report {
    Report {
        tables: Period::ALL
            .map(|period| Table {
                period,
                since: None,
                sections: sections.clone(),
                overall: overall.clone(),
                series: series.clone(),
            })
            .to_vec(),
        files: 1,
    }
}

/// `n` buckets a day apart from `start` (UTC) with these costs in dollars and `tokens` each.
fn daily(start: &str, dollars: &[u128], tokens: u64) -> Vec<stats::Bucket> {
    let mut at = ts(start);
    dollars
        .iter()
        .map(|&d| {
            let bucket = stats::Bucket {
                start: at,
                tokens: Tokens {
                    input: tokens,
                    ..Tokens::default()
                },
                cost: Cost {
                    pico_usd: d * 1_000_000_000_000,
                    unpriced_tokens: 0,
                },
            };
            at = at
                .checked_add(jiff::SignedDuration::from_hours(24))
                .unwrap();
            bucket
        })
        .collect()
}

/// R20: each model's cost, `-` when not priced, `+` on a total that leaves some out, a share
/// bar on each section's row by its share of the period's cost, and the models not priced.
#[test]
fn stats_view_shows_cost_share_and_unpriced_models() {
    let mut app = stats_app();
    let haiku = priced(
        model(CLAUDE, "claude-small", [30, 0, 0, 5, 0]),
        30_000_000_000_000,
    );
    let test = model(CLAUDE, "claude-test", [8, 0, 0, 9, 0]);
    let gpt = priced(
        model(CODEX, "gpt-test", [1500, 200, 0, 30, 12]),
        10_000_000_000_000,
    );
    let report = report_of(
        vec![
            stats_section(&["claude:default"], vec![haiku.clone(), test.clone()]),
            stats_section(&["claude:max"], vec![]),
            stats_section(&["codex:work"], vec![gpt.clone()]),
            stats_section(&[], vec![test.clone()]),
        ],
        vec![gpt, haiku, test],
        vec![],
    );
    update(
        &mut app,
        Event::Stats {
            report,
            error: None,
        },
    );
    let lines = screen(&app);
    let (_, header) = line_with(&lines, "ACCOUNT / MODEL");
    let cost_end = header.find("COST").unwrap() + 4;
    let share_at = header.find("SHARE").unwrap();
    let (_, default) = line_with(&lines, "default ");
    assert_eq!(default.find("$30.00+").unwrap() + 7, cost_end, "{default}");
    // 30 of 40 dollars: 36 of 48 eighths.
    assert_eq!(default[share_at..].trim_end(), "████▌");
    let (_, work) = line_with(&lines, "codex:work");
    assert_eq!(work.find("$10.00").unwrap() + 6, cost_end, "{work}");
    assert_eq!(work[share_at..].trim_end(), "█▌");
    let (_, unattributed) = line_with(&lines, "unattributed");
    assert_eq!(squeezed(unattributed), "unattributed 8 0 0 9 17 -");
    let (at, _) = line_with(&lines, "claude-small");
    assert_eq!(squeezed(&lines[at]), "claude-small 30 0 0 5 35 $30.00");
    // Model rows and Overall have no bar.
    let (overall_at, overall) = line_with(&lines, "Overall");
    assert_eq!(squeezed(overall), "Overall 1.5K 200 0 44 1.8K $40.00+");
    for i in [at, at + 1, at + 4, at + 6, overall_at, overall_at + 1] {
        assert!(!lines[i].contains(['█', '▌']), "{}", lines[i]);
    }
    let (note, text) = line_with(&lines, "not priced:");
    assert_eq!(text.trim(), "not priced: claude-test");
    assert_eq!(note, overall_at + 4, "last, after Overall");
}

/// R20: above the table, the period's cost over time: a bar per bucket, scaled to the largest
/// (labeled), with the first and last bucket's dates; the tokens when nothing is priced; by
/// week or month when the days do not fit; nothing without timestamps.
#[test]
fn stats_chart_draws_the_period_over_time() {
    let mut app = stats_app();
    update(&mut app, Event::Resize(80, render::TALL - 1));
    let priced_overall = vec![priced(
        model(CLAUDE, "claude-haiku-test", [30, 0, 0, 5, 0]),
        79_000_000_000_000,
    )];
    let week = daily("2026-09-18T00:00:00Z", &[0, 1, 2, 4, 8, 16, 48], 10);
    let show = |app: &mut App, overall: Vec<ModelRow>, series: Vec<stats::Bucket>| {
        update(
            app,
            Event::Stats {
                report: report_of(vec![], overall, series),
                error: None,
            },
        );
    };
    show(&mut app, priced_overall.clone(), week.clone());
    keys(&mut app, &[Key::Char('t'), Key::Char('t')]);
    assert_eq!(app.stats.period, Period::Week);
    let lines = screen(&app);
    assert_eq!(lines[2].trim_end(), "Cost per day");
    // 72 columns for 7 bars: 3 wide, 1 apart, after the 7-column label and the axis.
    let bar = |row: usize, bucket: usize| -> String {
        lines[3 + row]
            .chars()
            .skip(8 + 4 * bucket)
            .take(3)
            .collect()
    };
    let label = |line: &str| -> String { line.chars().take(8).collect() };
    assert_eq!(label(&lines[3]), " $48.00│");
    for row in 0..6 {
        assert_eq!(bar(row, 6), "███", "{}", lines.join("\n"));
        assert_eq!(bar(row, 0), "   ");
        // $1 of $48 is 1 of 48 eighths.
        assert_eq!(bar(row, 1), if row == 5 { "▁▁▁" } else { "   " });
    }
    // $16 of $48: 16 eighths, two full rows.
    assert_eq!(
        (0..6).map(|r| bar(r, 5)).collect::<Vec<_>>(),
        ["   ", "   ", "   ", "   ", "███", "███"]
    );
    assert_eq!(lines[9].trim_end(), format!("       └{}", "─".repeat(28)));
    assert_eq!(
        lines[10].trim_end(),
        format!("{}09-18{}09-24", " ".repeat(8), " ".repeat(17))
    );
    assert_eq!(lines[11].trim(), "");
    assert!(lines[12].starts_with("ACCOUNT / MODEL"), "{}", lines[12]);

    // Nothing priced: the tokens.
    let unpriced = vec![model(CLAUDE, "claude-test", [30, 0, 0, 5, 0])];
    let free: Vec<stats::Bucket> = week
        .iter()
        .map(|b| stats::Bucket {
            cost: Cost::default(),
            ..*b
        })
        .collect();
    show(&mut app, unpriced.clone(), free);
    let lines = screen(&app);
    assert_eq!(lines[2].trim_end(), "Tokens per day (nothing priced)");
    assert_eq!(label(&lines[3]), "     10│");

    // All time, 400 days: by week at 80 columns, by month at 40.
    keys(&mut app, &[Key::Char('t'), Key::Char('t')]);
    assert_eq!(app.stats.period, Period::All);
    show(
        &mut app,
        priced_overall,
        daily("2025-08-21T00:00:00Z", &[1; 400], 10),
    );
    assert_eq!(screen(&app)[2].trim_end(), "Cost per week");
    let lines = screen(&app);
    let (_, labels) = line_with(&lines, "2025-");
    assert!(labels.trim_end().ends_with("2026-09-21"), "{labels}");
    update(&mut app, Event::Resize(40, render::TALL - 1));
    let lines = screen(&app);
    assert_eq!(lines[2].trim_end(), "Cost per month");
    assert!(lines[10].contains("2026-09"), "{}", lines[10]);

    // No timestamps: no chart.
    update(&mut app, Event::Resize(80, render::TALL - 1));
    show(&mut app, unpriced, vec![]);
    let lines = screen(&app);
    assert!(lines[2].starts_with("ACCOUNT / MODEL"), "{}", lines[2]);
    assert!(!text(&app).contains(" per "));
}

/// R20: scrolled past the chart, the column header stays in view in place of the first line.
#[test]
fn stats_header_stays_when_scrolled_past_the_chart() {
    let mut app = stats_app();
    update(&mut app, Event::Resize(80, 16));
    let mut report = stats_report();
    for table in &mut report.tables {
        table.series = daily("2026-09-24T00:00:00Z", &[1], 10);
    }
    update(
        &mut app,
        Event::Stats {
            report,
            error: None,
        },
    );
    let (body, header_at) = render::stats_lines(&app, 80);
    assert_eq!(header_at, Some(10));
    let body: Vec<String> = body.iter().map(|l| squeezed(&l.to_string())).collect();
    assert_eq!(body[0], "Cost per day");
    for _ in 0..5 {
        keys(&mut app, &[Key::Char('j')]);
    }
    let lines = screen(&app);
    assert_eq!(squeezed(&lines[2]), body[5]);
    assert_eq!(line_with(&lines, "ACCOUNT / MODEL").0, 7, "in its place");
    for _ in 0..6 {
        keys(&mut app, &[Key::Char('j')]);
    }
    assert_eq!(app.stats.scroll, 11);
    let lines = screen(&app);
    assert!(lines[2].starts_with("ACCOUNT / MODEL"), "{}", lines[2]);
    assert_eq!(squeezed(&lines[3]), body[12]);
    assert_eq!(body[12], "claude-test 8 1.2M 160 90 1.2M -");
}

// ---- Private mode (R21) --------------------------------------------------------------------

const ZQ_A: &str = "aaaaaaaa-0000-4000-8000-00000000000a";
const ZQ_B: &str = "bbbbbbbb-0000-4000-8000-00000000000b";
const ZQ_C: &str = "019c1e08-e4f6-7d70-a129-38ec744a3f3c";
/// Untitled: History shows its first message.
const ZQ_D: &str = "dddddddd-0000-4000-8000-00000000000d";
const ZQ_HOME: &str = "/Users/zquser";
const ZQ_CWD: &str = "/Users/zquser/zq work/zqproj";
const ZQ_STORE: &str = "/Users/zquser/.claude/projects";
const ZQ_CODEX_STORE: &str = "/Users/zquser/.zqhomes/zqbeta/sessions";

fn zq_account(provider: crate::provider::Provider, name: &str) -> Account {
    Account {
        provider,
        name: name.into(),
        home: Home::Path(format!("{ZQ_HOME}/.zqhomes/{name}")),
    }
}

fn zq_entry(provider: crate::provider::Provider, id: &str, title: &str, minute: u32) -> Entry {
    let (store, path) = match provider {
        CODEX => (
            ZQ_CODEX_STORE.to_string(),
            format!("{ZQ_CODEX_STORE}/2026/09/24/rollout-2026-09-24T10-00-00-{id}.jsonl"),
        ),
        _ => (
            ZQ_STORE.to_string(),
            format!("{ZQ_STORE}/-Users-zquser-zq-work-zqproj/{id}.jsonl"),
        ),
    };
    Entry {
        provider,
        path: PathBuf::from(path),
        store: PathBuf::from(store),
        title: Some(title.into()),
        first_user_text: Some("zqfirst words".into()),
        cwd_first: Some(ZQ_CWD.into()),
        cwd_last: Some(ZQ_CWD.into()),
        ..entry(id, title, minute)
    }
}

/// Every place the screen can show something personal, filled with sentinels containing `zq`
/// (which no UI text contains): names, homes, emails, organizations, the working directory,
/// titles, first messages, session names, logs, previews, free text, the stats.
fn secret_app() -> App {
    let accounts = vec![
        Account::default_for(CLAUDE),
        zq_account(CLAUDE, "zqalpha"),
        zq_account(CODEX, "zqbeta"),
    ];
    let mut app = App::new(accounts, TimeZone::UTC, Some(ZQ_HOME.into()), ts(NOW));
    update(&mut app, Event::Resize(80, 24));
    app.cwd = Some(PathBuf::from(ZQ_CWD));
    app.start();
    let identity =
        |email: Option<&str>, org: Option<&str>, method: Option<&str>| Identity::LoggedIn {
            email: email.map(str::to_string),
            org: org.map(str::to_string),
            plan: Some("max".into()),
            method: method.map(str::to_string),
            cached: false,
        };
    for (account, identity) in [
        (
            Account::default_for(CLAUDE),
            identity(Some("zqdefault@zqmail.example"), Some("Zqorg Inc"), None),
        ),
        (
            zq_account(CLAUDE, "zqalpha"),
            identity(Some("zqme@zqmail.example"), Some("Zqorg Inc"), None),
        ),
        (
            zq_account(CODEX, "zqbeta"),
            identity(Some("zqb@zqmail.example"), None, Some("ChatGPT")),
        ),
    ] {
        update(&mut app, Event::Identity { account, identity });
    }
    for account in [Account::default_for(CLAUDE), zq_account(CLAUDE, "zqalpha")] {
        update(
            &mut app,
            Event::CachedUsage {
                account,
                result: cached(vec![
                    row("Session", 34.0, None, Some("2026-09-24T14:00:00Z")),
                    row(
                        "Week (all models)",
                        77.0,
                        None,
                        Some("2026-09-27T10:00:00Z"),
                    ),
                ]),
            },
        );
    }
    update(
        &mut app,
        Event::LiveUsage {
            account: zq_account(CODEX, "zqbeta"),
            result: Err(format!(
                "cannot reach {ZQ_HOME}/.zqhomes/zqbeta: zqbeta timed out"
            )),
        },
    );
    update(
        &mut app,
        Event::Checks(vec![Check {
            account: Some("claude:zqalpha".into()),
            message: format!("home {ZQ_HOME}/.zqhomes/zqalpha is not readable"),
        }]),
    );
    update(
        &mut app,
        Event::Stores(vec![
            Store {
                provider: CLAUDE,
                path: PathBuf::from(ZQ_STORE),
                accounts: vec!["claude:default".into(), "claude:zqalpha".into()],
                thread_names: vec![],
            },
            Store {
                provider: CODEX,
                path: PathBuf::from(ZQ_CODEX_STORE),
                accounts: vec!["codex:zqbeta".into()],
                thread_names: vec![PathBuf::from(format!(
                    "{ZQ_HOME}/.zqhomes/zqbeta/session_index.jsonl"
                ))],
            },
        ]),
    );
    update(
        &mut app,
        Event::IndexDone {
            entries: vec![
                zq_entry(CLAUDE, ZQ_A, "zqtitle one", 3),
                zq_entry(CLAUDE, ZQ_B, "zqtitle two", 2),
                zq_entry(CODEX, ZQ_C, "zqcodex title", 1),
                Entry {
                    title: None,
                    ..zq_entry(CLAUDE, ZQ_D, "unused", 0)
                },
            ],
            error: Some(format!(
                "zqalpha: cannot write {ZQ_HOME}/.remuda/state/index.json"
            )),
        },
    );
    let mut attribution = Attribution::default();
    attribution.add(ZQ_A, "claude:zqalpha");
    attribution.add(ZQ_B, "claude:zqalpha");
    attribution.add(ZQ_B, "claude:default");
    // An account no longer registered, named by the launch log.
    attribution.add(ZQ_B, "claude:zqgone");
    update(&mut app, Event::Attribution(attribution));
    let mut interactive = live("claude:zqalpha", 7, Some(ZQ_A));
    interactive.cwd = Some(ZQ_CWD.into());
    interactive.name = Some("zqlivename".into());
    let background = LiveSession {
        pid: None,
        short_id: Some("0a1b2c3d".into()),
        kind: Some("background".into()),
        name: Some("zqbgname".into()),
        status: Some("running".into()),
        ..live("claude:zqalpha", 0, Some(ZQ_B))
    };
    update(&mut app, Event::Live(vec![interactive, background]));
    update(
        &mut app,
        Event::Stats {
            report: Report {
                tables: Period::ALL
                    .map(|period| Table {
                        period,
                        since: None,
                        sections: vec![
                            stats_section(&["claude:default"], vec![]),
                            stats_section(
                                &["claude:zqalpha"],
                                vec![model(CLAUDE, "claude-test", [8, 1_234_567, 160, 90, 0])],
                            ),
                            stats_section(
                                &["codex:zqbeta"],
                                vec![model(CODEX, "gpt-test", [1500, 200, 0, 30, 12])],
                            ),
                            stats_section(
                                &["claude:default", "claude:zqalpha"],
                                vec![model(CLAUDE, "claude-test", [5, 0, 0, 5, 0])],
                            ),
                            stats_section(
                                &["claude:zqgone"],
                                vec![model(CLAUDE, "claude-test", [9, 0, 0, 9, 0])],
                            ),
                        ],
                        overall: vec![
                            model(CLAUDE, "claude-test", [22, 1_234_567, 160, 104, 0]),
                            priced(
                                model(CLAUDE, "claude-haiku-test", [1, 0, 0, 1, 0]),
                                12_340_000_000_000,
                            ),
                        ],
                        series: vec![stats::Bucket {
                            start: ts(NOW),
                            tokens: Tokens::default(),
                            cost: Cost {
                                pico_usd: 12_340_000_000_000,
                                unpriced_tokens: 0,
                            },
                        }],
                    })
                    .to_vec(),
                files: 3,
            },
            error: Some(format!("cannot write {ZQ_HOME}/.remuda/state/stats.json")),
        },
    );
    app
}

/// The preview of the current selection, loaded.
fn load_preview(app: &mut App) {
    let target = app.preview.target.clone().expect("a preview target");
    let messages = vec![
        Message {
            role: Role::User,
            text: "zqpreview question".into(),
        },
        Message {
            role: Role::Assistant,
            text: "zqanswer\n[tool: Read]".into(),
        },
    ];
    update(
        app,
        Event::Preview {
            path: target,
            result: Ok(messages),
        },
    );
}

fn zq_request(what: &str, args: &[&str], account: Account) -> LaunchRequest {
    LaunchRequest {
        account,
        args: args.iter().map(|a| a.to_string()).collect(),
        cwd: Some(PathBuf::from(ZQ_CWD)),
        what: what.into(),
    }
}

/// A state of the TUI and what it shows of the secrets when private mode is off.
struct SecretState {
    name: &'static str,
    set: fn(&mut App),
    /// Shown (in private mode off) at every size, except where `hidden_at_80` says the state
    /// covers the whole 80×24 screen with text that has nothing personal (the help box).
    shows: &'static str,
    hidden_at_80: bool,
}

fn secret_states() -> Vec<SecretState> {
    let state = |name, set: fn(&mut App), shows| SecretState {
        name,
        set,
        shows,
        hidden_at_80: false,
    };
    vec![
        state("accounts", |app| drop(keys(app, &[Key::Char('1')])), "zqme"),
        state(
            "accounts configuration",
            |app| {
                // Expanded: all of it shows at 80×24.
                let keys_ = [
                    Key::Char('1'),
                    Key::Char('j'),
                    Key::Char('p'),
                    Key::Char('p'),
                ];
                keys(app, &keys_);
                let request = app.config.request;
                update(
                    app,
                    Event::Config {
                        request,
                        account: zq_account(CLAUDE, "zqalpha"),
                        result: Ok(Box::new(zq_config_view())),
                    },
                );
            },
            "Configuration · zqalpha",
        ),
        state(
            "live",
            |app| drop(keys(app, &[Key::Char('2')])),
            "zqlivename",
        ),
        state(
            "live preview expanded",
            |app| {
                keys(app, &[Key::Char('2')]);
                load_preview(app);
                keys(app, &[Key::Char('p')]);
            },
            "zqpreview",
        ),
        state(
            "live preview collapsed",
            |app| {
                keys(app, &[Key::Char('2')]);
                load_preview(app);
            },
            "zqanswer",
        ),
        state(
            "live logs",
            |app| {
                let fx = keys(app, &[Key::Char('2'), Key::Char('j'), Key::Char('l')]);
                assert!(
                    fx.iter().any(|e| matches!(e, Effect::Logs { .. })),
                    "{fx:?}"
                );
                update(
                    app,
                    Event::Logs {
                        short_id: "0a1b2c3d".into(),
                        result: Ok("zqlog line\n".into()),
                    },
                );
            },
            "zqlog line",
        ),
        state(
            "history",
            |app| drop(keys(app, &[Key::Char('3')])),
            "zqtitle",
        ),
        state(
            "history preview expanded",
            |app| {
                keys(app, &[Key::Char('3')]);
                load_preview(app);
                keys(app, &[Key::Char('p')]);
            },
            "zqpreview",
        ),
        state(
            "history preview collapsed",
            |app| {
                keys(app, &[Key::Char('3')]);
                load_preview(app);
            },
            "zqanswer",
        ),
        state("stats", |app| drop(keys(app, &[Key::Char('4')])), "zqalpha"),
        state(
            "search",
            |app| {
                keys(app, &[Key::Char('3'), Key::Char('/')]);
                type_str(app, "zqquery");
            },
            "zqquery",
        ),
        state(
            "search kept, no match",
            |app| {
                keys(app, &[Key::Char('3'), Key::Char('/')]);
                type_str(app, "zqnomatch");
                keys(app, &[Key::Enter]);
            },
            "zqnomatch",
        ),
        SecretState {
            name: "help",
            set: |app| drop(keys(app, &[Key::Char('?')])),
            shows: "zqalpha",
            hidden_at_80: true,
        },
        state("pick resume", |app| pick(app, PickFor::Resume), "zqalpha ●"),
        state("pick fork", |app| pick(app, PickFor::Fork), "zqalpha ●"),
        state("pick relay", |app| pick(app, PickFor::Relay), "zqalpha ●"),
        state(
            "new session form",
            |app| {
                keys(
                    app,
                    &[Key::Char('1'), Key::Char('j'), Key::Char('n'), Key::Tab],
                );
                type_str(app, "zqsess");
            },
            "zqsess",
        ),
        state(
            "setup form",
            |app| {
                keys(app, &[Key::Char('1'), Key::Char('s')]);
                type_str(app, "zqnew");
                keys(app, &[Key::Tab]);
                type_str(app, "zqnew@zqmail.example");
                if let Some(Overlay::Form(form)) = &mut app.overlay {
                    form.error = Some(format!(
                        "zqnew: {ZQ_HOME}/.remuda/homes/claude/zqnew exists"
                    ));
                }
            },
            "zqnew",
        ),
        state(
            "confirm",
            |app| {
                keys(app, &[Key::Char('2')]);
                app.overlay = Some(Overlay::Confirm(Confirm {
                    verb: Control::Stop,
                    account: "claude:zqalpha".into(),
                    short_id: "0a1b2c3d".into(),
                }));
            },
            "(zqalpha)",
        ),
        state(
            "remove account",
            |app| drop(keys(app, &[Key::Char('1'), Key::Char('j'), Key::Char('D')])),
            "Remove zqalpha",
        ),
        state(
            "resume codex",
            |app| {
                keys(app, &[Key::Char('3')]);
                app.overlay = Some(Overlay::ResumeCodex(ResumeCodex {
                    path: PathBuf::from(format!(
                        "{ZQ_CODEX_STORE}/2026/09/24/rollout-2026-09-24T10-00-00-{ZQ_C}.jsonl"
                    )),
                    written: Some(ts("2026-09-24T11:58:00Z")),
                    request: zq_request(
                        "resume 019c1e08 as codex:zqbeta",
                        &["resume", ZQ_C, "-C", ZQ_CWD],
                        zq_account(CODEX, "zqbeta"),
                    ),
                }));
            },
            "as codex:zqbeta?",
        ),
        state(
            "pick for run",
            |app| {
                app.mode = Mode::PickForRun;
                keys(app, &[Key::Char('1')]);
            },
            "zq work",
        ),
        state(
            "notice",
            |app| {
                keys(app, &[Key::Char('1')]);
                app.notice = Some(Notice {
                    text: format!(
                        "cannot use {ZQ_CWD} or /tmp/zqscratch/x: zqalpha, zqme@zqmail.example"
                    ),
                    level: Level::Error,
                });
            },
            ": zqalpha,",
        ),
        state(
            "pending check",
            |app| {
                keys(app, &[Key::Char('1')]);
                app.pending = Some((
                    1,
                    zq_request(
                        "new session “zqsess” as zqalpha",
                        &["--name", "zqsess"],
                        zq_account(CLAUDE, "zqalpha"),
                    ),
                ));
            },
            "“zqsess”",
        ),
        state(
            "untitled session",
            |app| drop(keys(app, &[Key::Char('3')])),
            "zqfirst words",
        ),
        state(
            "preview error",
            |app| {
                keys(app, &[Key::Char('3')]);
                let target = app.preview.target.clone().expect("a preview target");
                update(
                    app,
                    Event::Preview {
                        path: target,
                        result: Err(format!(
                            "zqalpha has no access to {ZQ_STORE}/-Users-zquser/x.jsonl"
                        )),
                    },
                );
            },
            "zqalpha has no access",
        ),
        state(
            "logs error",
            |app| {
                keys(app, &[Key::Char('2'), Key::Char('j'), Key::Char('l')]);
                update(
                    app,
                    Event::Logs {
                        short_id: "0a1b2c3d".into(),
                        result: Err(format!("zqalpha is logged out: exit 1 in {ZQ_CWD}")),
                    },
                );
            },
            "zqalpha is logged out",
        ),
        state(
            "setup failed",
            |app| {
                keys(app, &[Key::Char('1'), Key::Char('s')]);
                type_str(app, "zqnew");
                let fx = keys(app, &[Key::Enter]);
                assert!(
                    fx.iter().any(|e| matches!(e, Effect::Setup { .. })),
                    "{fx:?}"
                );
                update(
                    app,
                    Event::SetupDone {
                        provider: CLAUDE,
                        name: "zqnew".into(),
                        result: Err(format!(
                            "{ZQ_HOME}/.remuda/homes/claude/zqnew already exists"
                        )),
                    },
                );
            },
            "set up zqnew",
        ),
        state(
            "home with a trailing slash",
            |app| {
                keys(app, &[Key::Char('1')]);
                app.home = Some(format!("{ZQ_HOME}/"));
                app.notice = Some(Notice {
                    text: format!("cannot use {ZQ_HOME}/zqother/x or home:{ZQ_HOME}/zqelse/y"),
                    level: Level::Warn,
                });
            },
            "zqother",
        ),
        state(
            "home a prefix of another directory",
            |app| {
                keys(app, &[Key::Char('1')]);
                app.home = Some("/Users/zq".into());
                app.notice = Some(Notice {
                    text: format!("cannot use {ZQ_HOME}/zqproj"),
                    level: Level::Warn,
                });
            },
            "zquser/zqproj",
        ),
        state(
            "share source not registered",
            |app| {
                keys(app, &[Key::Char('1')]);
                let accounts: Vec<Account> =
                    app.accounts.iter().map(|a| a.account.clone()).collect();
                let sharing = crate::registry::Sharing {
                    source: Some(zq_account(CLAUDE, "zqshare")),
                    opted_out: vec![],
                };
                let found = crate::checks::sharing(&accounts, &crate::Env::new(), &sharing);
                update(app, Event::Checks(found));
            },
            "zqshare",
        ),
        state(
            "index error",
            |app| drop(keys(app, &[Key::Char('1')])),
            "index cache: zqalpha",
        ),
    ]
}

/// A picker for the claude session A.
fn pick(app: &mut App, action: PickFor) {
    keys(app, &[Key::Char('3')]);
    app.overlay = Some(Overlay::Pick(Pick {
        path: PathBuf::from(format!(
            "{ZQ_STORE}/-Users-zquser-zq-work-zqproj/{ZQ_A}.jsonl"
        )),
        session_id: ZQ_A.into(),
        action,
        options: vec!["claude:zqalpha".into(), "claude:default".into()],
        attributed: vec!["claude:zqalpha".into()],
        selected: 0,
    }));
}

/// R21: in private mode nothing personal shows anywhere, while every one of these states shows
/// something personal when it is off; numbers, model names and aliases still show.
#[test]
fn private_mode_leaks_nothing_anywhere() {
    for state in secret_states() {
        for (w, h) in [(80, 24), (160, 40)] {
            let mut app = secret_app();
            update(&mut app, Event::Resize(w, h));
            (state.set)(&mut app);
            let open = text(&app);
            let at = format!("{} at {w}x{h}", state.name);
            if state.hidden_at_80 && w == 80 {
                assert!(
                    !open.to_lowercase().contains("zq"),
                    "{at}: expected a full-screen box:\n{open}"
                );
            } else {
                assert!(
                    open.contains(state.shows),
                    "{at}: shows no {:?} when not private:\n{open}",
                    state.shows
                );
            }
            assert!(!open.contains("PRIVATE"), "{at}:\n{open}");

            app.private = true;
            let private = text(&app);
            assert!(
                !private.to_lowercase().contains("zq"),
                "{at}: leaks in private mode:\n{private}"
            );
            assert!(private.contains("PRIVATE"), "{at}:\n{private}");
        }
    }
}

/// R21: what private mode leaves: numbers, model names, plans, aliases, `default`, masks.
#[test]
fn private_mode_keeps_numbers_models_and_aliases() {
    let mut app = secret_app();
    update(&mut app, Event::Resize(160, 40));
    app.private = true;
    let accounts = text(&app);
    for shown in [
        "PRIVATE",
        "account-1",
        "codex:account-1",
        "default",
        "34%",
        "77%",
        "•••@•••",
        "max",
    ] {
        assert!(accounts.contains(shown), "{shown}:\n{accounts}");
    }
    keys(&mut app, &[Key::Char('4')]);
    let stats = text(&app);
    for shown in [
        "1.2M",
        "$12.34",
        "Cost per day",
        "claude-test",
        "gpt-test",
        "account-1",
        "account-2",
        "codex:account-1",
    ] {
        assert!(stats.contains(shown), "{shown}:\n{stats}");
    }
    keys(&mut app, &[Key::Char('3')]);
    let history = text(&app);
    for shown in [
        "09-24 10:03",
        "~/•••/•••",
        "account-1,ac",
        "codex:accoun",
        "•••",
    ] {
        assert!(history.contains(shown), "{shown}:\n{history}");
    }
    keys(&mut app, &[Key::Char('2')]);
    let live = text(&app);
    for shown in ["account-1", "0a1b2c3d", "7", "~/•••/•••"] {
        assert!(live.contains(shown), "{shown}:\n{live}");
    }
}

/// R21: Ctrl-P toggles private mode in every view and overlay, and is never anything else: it
/// types nothing, closes nothing, cancels nothing, and a notice survives it.
#[test]
fn ctrl_p_toggles_everywhere() {
    let toggles = |app: &mut App| {
        let before = app.clone();
        assert_eq!(keys(app, &[Key::Ctrl('p')]), []);
        assert!(app.private);
        assert_eq!(keys(app, &[Key::Ctrl('p')]), []);
        assert_eq!(*app, before, "nothing else changed");
    };
    for view in ['1', '2', '3', '4'] {
        let mut app = secret_app();
        keys(&mut app, &[Key::Char(view)]);
        toggles(&mut app);
    }
    for state in secret_states() {
        let mut app = secret_app();
        (state.set)(&mut app);
        toggles(&mut app);
        if app.help {
            // The help box stays open; the next key closes it.
            keys(&mut app, &[Key::Ctrl('p')]);
            assert!(app.help && app.private);
        }
    }

    // In a form and the search prompt, nothing is typed.
    let mut app = secret_app();
    keys(&mut app, &[Key::Char('1'), Key::Char('s')]);
    type_str(&mut app, "zq");
    keys(&mut app, &[Key::Ctrl('p')]);
    type_str(&mut app, "Pp");
    let Some(Overlay::Form(form)) = &app.overlay else {
        panic!("form closed")
    };
    assert_eq!((form.fields[0].value.as_str(), form.focus), ("zqPp", 0));
    // The cursor follows the masked value.
    let all = text(&app);
    assert!(all.contains("•••▏"), "{all}");
    keys(&mut app, &[Key::Ctrl('p')]);
    let all = text(&app);
    assert!(all.contains("zqPp▏"), "{all}");

    let mut app = secret_app();
    keys(&mut app, &[Key::Char('3'), Key::Char('/')]);
    type_str(&mut app, "zq");
    keys(&mut app, &[Key::Ctrl('p')]);
    type_str(&mut app, "x");
    assert_eq!(app.history.query, "zqx");
    assert!(app.history.searching);

    // A notice survives the toggle; any other key clears it.
    let mut app = secret_app();
    app.notice = Some(Notice {
        text: "hello".into(),
        level: Level::Info,
    });
    keys(&mut app, &[Key::Ctrl('p')]);
    assert!(app.notice.is_some());
    keys(&mut app, &[Key::Char('j')]);
    assert!(app.notice.is_none());

    // Choosing an account for `remuda run`.
    let mut app = secret_app();
    app.mode = Mode::PickForRun;
    toggles(&mut app);
    // And Ctrl-C still quits in private mode.
    keys(&mut app, &[Key::Ctrl('p')]);
    assert_eq!(keys(&mut app, &[Key::Ctrl('c')]), [Effect::Quit]);
}

/// R21: an alias never changes while the TUI runs: removing an account keeps the others',
/// and a new account gets the next number.
#[test]
fn aliases_stay_stable_when_accounts_change() {
    let mut app = secret_app();
    app.private = true;
    let alias = |app: &App, q: &str| app.aliases.qualified(q);
    assert_eq!(alias(&app, "claude:zqalpha"), "claude:account-1");
    assert_eq!(alias(&app, "claude:zqgone"), "claude:account-2");
    assert_eq!(alias(&app, "codex:zqbeta"), "codex:account-1");
    update(
        &mut app,
        Event::Accounts(vec![
            Account::default_for(CLAUDE),
            zq_account(CODEX, "zqbeta"),
            zq_account(CLAUDE, "zqnew"),
        ]),
    );
    assert_eq!(alias(&app, "claude:zqalpha"), "claude:account-1");
    assert_eq!(alias(&app, "codex:zqbeta"), "codex:account-1");
    assert_eq!(alias(&app, "claude:zqnew"), "claude:account-3");
    let all = text(&app);
    assert!(all.contains("account-3"), "{all}");
    assert!(!all.to_lowercase().contains("zq"), "{all}");
}

/// R21: the redacted copy kept between frames is made again whenever the app changes, and
/// follows the clock without being made again.
#[test]
fn private_snapshot_follows_every_change() {
    let mut app = secret_app();
    app.private = true;
    let mut snapshot = super::privacy::Snapshot::default();
    let first = snapshot.of(&app).clone();
    assert_eq!(first, super::privacy::redacted(&app));

    app.now = ts("2026-09-24T12:00:05Z");
    assert_eq!(*snapshot.of(&app), super::privacy::redacted(&app));

    app.notice = Some(Notice {
        text: "zqalpha exited".into(),
        level: Level::Info,
    });
    let copy = snapshot.of(&app).clone();
    assert_eq!(copy.notice.as_ref().unwrap().text, "account-1 exited");
    keys(&mut app, &[Key::Char('3'), Key::Char('j')]);
    assert_eq!(*snapshot.of(&app), super::privacy::redacted(&app));

    // What the event loop draws is what `render` draws.
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| render::render_with(&app, &mut snapshot, f))
        .unwrap();
    let kept = terminal.backend().buffer().clone();
    terminal.draw(|f| render::render(&app, f)).unwrap();
    assert_eq!(*terminal.backend().buffer(), kept);
}

// ---- Configuration (R22) ------------------------------------------------------------------

/// A member's configuration: an agent with its frontmatter, a linked skill, shared
/// instructions, a shared plugin, own settings with many hook events, withheld
/// authentication, injected auto-memory, an MCP server.
fn config_view() -> crate::account_config::ConfigView {
    use crate::account_config::{
        ConfigView, Content, Entry, Item, Mcp, Memory, Origin, Plugin, PluginContents, Role,
        Summary,
    };
    ConfigView {
        role: Role::Member {
            source: "claude:default".into(),
        },
        instructions: vec![
            Item {
                name: "agents",
                origin: Origin::Own,
                link: None,
                content: Content::Entries(vec![Entry {
                    name: "reviewer".into(),
                    description: Some("reviews diffs".into()),
                    model: Some("sonnet".into()),
                    effort: Some("low".into()),
                    tools: Some("Read, Grep".into()),
                    ..Entry::default()
                }]),
            },
            Item {
                name: "skills",
                origin: Origin::Own,
                link: None,
                content: Content::Entries(vec![Entry {
                    name: "pdf".into(),
                    link: Some(PathBuf::from("../../lib/skills/pdf")),
                    ..Entry::default()
                }]),
            },
            Item {
                name: "CLAUDE.md",
                origin: Origin::Shared,
                link: None,
                content: Content::File {
                    bytes: 812,
                    lines: 20,
                },
            },
        ],
        plugins: vec![Plugin {
            name: "ctx7@market".into(),
            origin: Origin::Shared,
            version: Some("1.2.0".into()),
            scope: Some("user".into()),
            installs: 1,
            path: Some(PathBuf::from("/Users/you/.claude/plugins/cache/ctx7")),
            contents: Some(PluginContents {
                hooks: vec![("PreToolUse".into(), 2)],
                mcp_servers: vec!["ctx".into()],
                ..PluginContents::default()
            }),
        }],
        own_settings: Summary {
            model: Some("opus".into()),
            env: vec!["DISABLE_TELEMETRY".into()],
            hooks: (1..=13).map(|i| (format!("Event{i:02}"), i)).collect(),
            ..Summary::default()
        },
        withheld: vec!["env.ANTHROPIC_API_KEY".into()],
        memory: Memory {
            dir: Some("/Users/you/.claude/projects/-Users-you-space-remuda/memory".into()),
            origin: Origin::Shared,
            files: Some(3),
        },
        mcp: Mcp {
            user: vec!["github".into()],
            project: vec![],
        },
        ..ConfigView::default()
    }
}

/// A configuration whose only `zq` is where private mode masks or aliases it: descriptions,
/// link targets, the memory directory, a problem, the source (R21, R22). Names have none:
/// private mode shows them.
fn zq_config_view() -> crate::account_config::ConfigView {
    use crate::account_config::{ConfigView, Content, Entry, Item, Memory, Origin, Role};
    ConfigView {
        role: Role::Member {
            source: "claude:zqshare".into(),
        },
        instructions: vec![
            Item {
                name: "agents",
                origin: Origin::Shared,
                link: None,
                content: Content::Entries(vec![Entry {
                    name: "helper".into(),
                    description: Some("zqdesc agent".into()),
                    model: Some("opus".into()),
                    ..Entry::default()
                }]),
            },
            Item {
                name: "skills",
                origin: Origin::Own,
                link: Some(PathBuf::from(format!("{ZQ_HOME}/zqlib/skills"))),
                content: Content::Entries(vec![Entry {
                    name: "pdf".into(),
                    description: Some("zqdesc skill".into()),
                    link: Some(PathBuf::from(format!("{ZQ_HOME}/zqlib/skills/pdf"))),
                    ..Entry::default()
                }]),
            },
        ],
        memory: Memory {
            dir: Some(format!("{ZQ_HOME}/.zqhomes/zqalpha/projects/-x/memory")),
            origin: Origin::Shared,
            files: Some(2),
        },
        problems: vec![format!(
            "zqalpha: cannot read {ZQ_HOME}/.zqhomes/zqalpha/settings.json"
        )],
        ..ConfigView::default()
    }
}

fn config_effect(request: u64, name: &str) -> Effect {
    Effect::Config {
        request,
        account: account(name),
        cwd: Some(PathBuf::from("/Users/you/space/remuda")),
    }
}

fn config_effects(fx: &[Effect]) -> Vec<&Effect> {
    fx.iter()
        .filter(|e| matches!(e, Effect::Config { .. }))
        .collect()
}

/// Answers the pane's last request for `name` with `view`.
fn answer_config(app: &mut App, name: &str, view: crate::account_config::ConfigView) {
    let request = app.config.request;
    update(
        app,
        Event::Config {
            request,
            account: account(name),
            result: Ok(Box::new(view)),
        },
    );
}

/// The pane's lines at `width`, as text.
fn config_text(app: &App, width: usize) -> String {
    render::config_lines(app, width)
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// R22: `p` (or Space) opens the pane and reads the selected account's configuration, again
/// gives it the whole view, again closes it; `Esc` steps back one; a pending launch is
/// cancelled first.
#[test]
fn p_cycles_the_configuration_pane_and_esc_steps_back() {
    let mut app = app();
    assert_eq!(
        keys(&mut app, &[Key::Char('p')]),
        [config_effect(1, "default")]
    );
    assert!(app.config.open && !app.config.expanded && app.config.loading);
    assert_eq!(keys(&mut app, &[Key::Char('p')]), []);
    assert!(app.config.open && app.config.expanded);
    assert_eq!(keys(&mut app, &[Key::Char('p')]), []);
    assert!(!app.config.open && !app.config.expanded);

    assert_eq!(
        keys(&mut app, &[Key::Char(' ')]),
        [config_effect(2, "default")]
    );
    keys(&mut app, &[Key::Char(' ')]);
    assert!(app.config.expanded);
    keys(&mut app, &[Key::Esc]);
    assert!(app.config.open && !app.config.expanded);
    keys(&mut app, &[Key::Esc]);
    assert!(!app.config.open);

    // A pending launch check goes first.
    keys(&mut app, &[Key::Char('p')]);
    app.pending = Some((1, request("default", &[], Some(CWD), "new session")));
    keys(&mut app, &[Key::Esc]);
    assert_eq!(app.pending, None);
    assert!(app.config.open);

    // Other views keep their own `p`; switching away leaves the pane open, not expanded.
    keys(&mut app, &[Key::Char('p'), Key::Char('2')]);
    assert!(app.config.open && !app.config.expanded);
    keys(&mut app, &[Key::Char('p')]);
    assert!(app.preview.expanded && app.config.open);
}

/// R22: while the pane is open it follows the selection, and only the answer to the last
/// request is kept.
#[test]
fn the_pane_follows_the_selection_and_drops_stale_answers() {
    let mut app = app();
    keys(&mut app, &[Key::Char('p')]);
    assert_eq!(keys(&mut app, &[Key::Char('j')]), [config_effect(2, "max")]);
    update(
        &mut app,
        Event::Config {
            request: 1,
            account: account("default"),
            result: Ok(Box::new(config_view())),
        },
    );
    assert_eq!(app.config.loaded, None);
    assert!(app.config.loading);
    answer_config(&mut app, "max", config_view());
    assert_eq!(app.config.loaded, Some(Ok(config_view())));
    assert!(!app.config.loading);
    // A late answer after closing is dropped too.
    keys(&mut app, &[Key::Char('p'), Key::Char('p')]);
    answer_config(&mut app, "max", config_view());
    assert_eq!(app.config.loaded, None);
}

/// R22: `r` and a new account list read the configuration again (what it showed stays until
/// the answer); with the pane closed, `r` reads none.
#[test]
fn r_and_a_new_account_list_read_the_configuration_again() {
    let mut app = app();
    keys(&mut app, &[Key::Char('p')]);
    answer_config(&mut app, "default", config_view());
    let fx = keys(&mut app, &[Key::Char('r')]);
    assert_eq!(config_effects(&fx), [&config_effect(2, "default")]);
    assert!(app.config.loaded.is_some() && app.config.loading);

    let fx = update(
        &mut app,
        Event::Accounts(vec![account("default"), account("max"), account("new")]),
    );
    assert_eq!(config_effects(&fx), [&config_effect(3, "default")]);
    // The same list again changes nothing.
    let fx = update(
        &mut app,
        Event::Accounts(vec![account("default"), account("max"), account("new")]),
    );
    assert_eq!(config_effects(&fx), Vec::<&Effect>::new());

    keys(&mut app, &[Key::Char('p'), Key::Char('p')]);
    let fx = keys(&mut app, &[Key::Char('r')]);
    assert_eq!(config_effects(&fx), Vec::<&Effect>::new());
}

/// R22: a codex account has no configuration listing: nothing is read.
#[test]
fn codex_accounts_have_no_configuration_listing() {
    let mut app = codex_app();
    keys(&mut app, &[Key::Char('1'), Key::Char('j'), Key::Char('j')]);
    assert_eq!(
        app.accounts[app.accounts_list.selected].account.provider,
        CODEX
    );
    let fx = keys(&mut app, &[Key::Char('p')]);
    assert_eq!(config_effects(&fx), Vec::<&Effect>::new());
    let all = text(&app);
    assert!(
        all.contains("configuration listing is Claude-only"),
        "{all}"
    );
}

/// R22: below the table on a narrow terminal (the timeline and checks give way), beside the
/// view from 120 columns on, over the whole view when expanded.
#[test]
fn the_pane_sits_below_the_table_or_beside_the_view() {
    let mut app = populated_accounts();
    keys(&mut app, &[Key::Char('p')]);
    answer_config(&mut app, "default", config_view());
    let lines = screen(&app);
    let (max_row, _) = line_with(&lines, "max ");
    let (title, _) = line_with(&lines, "Configuration · default · for ~/space/remuda");
    assert!(title > max_row, "{}", lines.join("\n"));
    assert!(!lines.join("\n").contains("Resets · next 7 days"));

    update(&mut app, Event::Resize(160, 40));
    let lines = screen(&app);
    line_with(&lines, "Resets · next 7 days");
    let (_, top) = line_with(&lines, "Accounts ─");
    assert!(top.contains("Configuration ·"), "{top}");

    update(&mut app, Event::Resize(80, 24));
    keys(&mut app, &[Key::Char('p')]);
    let all = text(&app);
    assert!(!all.contains("ACCOUNT"), "{all}");
    assert!(all.contains("(esc: back)"), "{all}");

    let body = config_text(&app, 200);
    for shown in [
        "reviewer · sonnet · effort low · tools Read, Grep",
        "      reviews diffs",
        "pdf -> ../../lib/skills/pdf",
        "CLAUDE.md · shared from default · 20 lines · 812 B",
        "ctx7@market · 1.2.0 · user · shared from default",
        "    hooks: PreToolUse 2",
        "    mcp: ctx",
        "shared from default",
        "not shared (authentication): env.ANTHROPIC_API_KEY",
        "~/.claude/projects/-Users-you-space-remuda/memory · shared from default · 3 files",
        "  user: github",
        "gets default's configuration at launch",
    ] {
        assert!(body.contains(shown), "{shown}:\n{body}");
    }
}

/// R22: `PgUp` / `PgDn` scroll the open pane while `j` / `k` move the selection (the pane then
/// starts at the top); expanded, the movement keys scroll it.
#[test]
fn the_pane_scrolls() {
    let mut app = app();
    keys(&mut app, &[Key::Char('p')]);
    answer_config(&mut app, "default", config_view());
    let height = render::config_height(&app);
    let max = render::config_line_count(&app) - height;
    assert!(height > 0 && max > 0, "{height} {max}");
    keys(&mut app, &[Key::PageDown]);
    assert_eq!(app.config.scroll, height.min(max));
    keys(&mut app, &[Key::PageDown, Key::PageDown, Key::PageDown]);
    assert_eq!(app.config.scroll, max);
    keys(&mut app, &[Key::PageUp]);
    assert_eq!(app.config.scroll, max.saturating_sub(height));
    keys(&mut app, &[Key::Char('j')]);
    assert_eq!((app.accounts_list.selected, app.config.scroll), (1, 0));

    answer_config(&mut app, "max", config_view());
    keys(&mut app, &[Key::Char('p')]);
    let max = render::config_line_count(&app) - render::config_height(&app);
    keys(&mut app, &[Key::Char('j')]);
    assert_eq!((app.accounts_list.selected, app.config.scroll), (1, 1));
    keys(&mut app, &[Key::Char('G')]);
    assert_eq!(app.config.scroll, max);
    keys(&mut app, &[Key::Char('g')]);
    assert_eq!(app.config.scroll, 0);
    // Accounts keys wait while it is expanded.
    assert_eq!(keys(&mut app, &[Key::Char('n'), Key::Char('D')]), []);
    assert_eq!(app.overlay, None);
}

/// R22: the hint line names the pane's keys in each state.
#[test]
fn accounts_hints_name_the_configuration() {
    let mut app = app();
    let lines = screen(&app);
    let (_, hints) = line_with(&lines, "n: new session");
    assert!(
        hints.contains("p: config") && hints.contains("u: live usage"),
        "{hints}"
    );
    keys(&mut app, &[Key::Char('p')]);
    line_with(&screen(&app), "pgup/pgdn: scroll");
    keys(&mut app, &[Key::Char('p')]);
    line_with(&screen(&app), "esc: back · r: refresh");
}

#[test]
fn the_pane_on_a_tiny_terminal_does_not_panic() {
    for (w, h) in [(1, 1), (10, 3), (20, 5), (40, 10)] {
        let mut app = populated_accounts();
        update(&mut app, Event::Resize(w, h));
        keys(&mut app, &[Key::Char('p')]);
        answer_config(&mut app, "default", config_view());
        for _ in 0..2 {
            keys(&mut app, &[Key::PageDown]);
            screen(&app);
            keys(&mut app, &[Key::Ctrl('p')]);
            screen(&app);
            keys(&mut app, &[Key::Ctrl('p'), Key::Char('p')]);
        }
    }
}

/// R21, R22: in private mode the pane keeps names, models and tools, and masks descriptions
/// and paths.
#[test]
fn private_mode_keeps_configuration_names_and_masks_the_rest() {
    let mut app = populated_accounts();
    update(&mut app, Event::Resize(160, 40));
    keys(&mut app, &[Key::Char('p'), Key::Char('p')]);
    answer_config(&mut app, "default", config_view());
    app.private = true;
    let all = text(&app);
    for shown in [
        "reviewer",
        "sonnet",
        "Read, Grep",
        "pdf",
        "ctx7@market",
        "DISABLE_TELEMETRY",
        "github",
        "PreToolUse 2",
        "•••",
        "~/•••/•••/•••/•••",
        "for ~/•••/•••",
    ] {
        assert!(all.contains(shown), "{shown}:\n{all}");
    }
    for hidden in [
        "reviews diffs",
        "lib/skills",
        "-Users-you-space-remuda",
        "space/remuda",
    ] {
        assert!(!all.contains(hidden), "{hidden}:\n{all}");
    }
}
