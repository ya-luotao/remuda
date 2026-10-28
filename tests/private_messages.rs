//! Private mode (SPEC R21) on the messages remuda puts together, from where they are made to
//! the screen: `checks` (R11) and `account_config` (R22) read the files of a sandboxed home,
//! what they tell goes into the TUI as the workers send it, and the screen is drawn with
//! private mode on. Each path is masked whole, and what remuda says around it stays.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::Sandbox;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use remuda::account_config::{self, ConfigView};
use remuda::privacy::Piece;
use remuda::registry::{Account, CLAUDE, Home, Sharing};
use remuda::tui::app::{App, Event, Key, update};
use remuda::tui::{privacy, render};
use remuda::{Env, checks, index};

const SIZE: (u16, u16) = (160, 40);

/// A home under the sandbox's `$HOME`, a source (the native login) and a member whose home
/// has every character a path may hold.
struct Homes {
    _sb: Sandbox,
    home: PathBuf,
    env: Env,
    source: Account,
    member: Account,
    sharing: Sharing,
}

impl Homes {
    fn new() -> Homes {
        let sb = Sandbox::new();
        let home = sb.home().canonicalize().unwrap();
        fs::create_dir_all(home.join(".claude/projects")).unwrap();
        let member_home = home.join("zq homes (old)").join("zqmember, x: y");
        fs::create_dir_all(&member_home).unwrap();
        let env: Env = [("HOME".to_string(), home.display().to_string())].into();
        let source = Account::default_for(CLAUDE);
        let member = Account {
            provider: CLAUDE,
            name: "zqmember".into(),
            home: Home::Path(member_home.display().to_string()),
        };
        let sharing = Sharing {
            source: Some(source.clone()),
            ..Sharing::default()
        };
        Homes {
            _sb: sb,
            home,
            env,
            source,
            member,
            sharing,
        }
    }

    fn member_home(&self) -> PathBuf {
        match &self.member.home {
            Home::Path(p) => PathBuf::from(p),
            Home::Default => unreachable!(),
        }
    }

    fn source_home(&self) -> PathBuf {
        self.home.join(".claude")
    }

    fn accounts(&self) -> Vec<Account> {
        vec![self.source.clone(), self.member.clone()]
    }

    /// The TUI on the Accounts view, the member selected.
    fn app(&self, accounts: Vec<Account>) -> App {
        let mut app = App::new(
            accounts,
            jiff::tz::TimeZone::UTC,
            Some(self.home.display().to_string()),
            jiff::Timestamp::from_second(1_790_000_000).unwrap(),
        );
        update(&mut app, Event::Resize(SIZE.0, SIZE.1));
        for key in [Key::Char('1'), Key::Char('j')] {
            update(&mut app, Event::Key(key));
        }
        app
    }

    /// The member's configuration, read from the files, shown in the Configuration pane.
    fn with_config(&self, sharing: &Sharing) -> (App, ConfigView) {
        let view = account_config::read(&self.member, sharing, None, &self.env);
        let mut app = self.app(self.accounts());
        update(&mut app, Event::Key(Key::Char('p')));
        let request = app.config.work.round();
        update(
            &mut app,
            Event::Config {
                request,
                account: self.member.clone(),
                result: Ok(Box::new(view.clone())),
            },
        );
        (app, view)
    }
}

fn screen(app: &App) -> String {
    let (w, h) = SIZE;
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| render::render(app, f)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..h)
        .map(|y| {
            let line: String = (0..w).map(|x| buffer[(x, y)].symbol()).collect();
            line.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The problems of the Configuration pane as private mode keeps them.
fn private_problems(app: &App) -> Vec<String> {
    let copy = privacy::redacted(app);
    let Some(Ok(view)) = &copy.config.loaded else {
        panic!("no configuration loaded: {:?}", copy.config.loaded)
    };
    view.problems.iter().map(|p| p.to_string()).collect()
}

/// Nothing of the homes shows: not the account, not a name of a directory. (`settings.json`
/// alone is the pane's own name for the section, R22.)
fn leaks_nothing(private: &str) {
    assert!(private.contains("PRIVATE"), "{private}");
    for secret in ["zq", "homes", "(old)", "/settings.json", ".claude/"] {
        assert!(
            !private.to_lowercase().contains(secret),
            "{secret}:\n{private}"
        );
    }
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// R21, R22: a `settings.json` that is not a JSON object is a problem of the card, and in
/// private mode the file is masked as the path it is while what is wrong with it stays
/// readable: for the home's own file, whether or not it is the source's through a link, and
/// for one that cannot be read, whose reason is the system's. The source's file alone is no
/// problem of a member whose settings are its own.
#[test]
fn a_settings_problem_keeps_its_reason_in_private_mode() {
    let h = Homes::new();
    let source_settings = h.source_home().join("settings.json");
    let own_settings = h.member_home().join("settings.json");

    // The source's: the member's settings are its own.
    write(&source_settings, "[]");
    let (_, view) = h.with_config(&h.sharing);
    assert!(view.problems.is_empty(), "{:?}", view.problems);

    // Through a link, it is the member's own file.
    std::os::unix::fs::symlink(&source_settings, &own_settings).unwrap();
    let (mut app, view) = h.with_config(&h.sharing);
    assert_eq!(
        view.problems,
        [format!("{} is not a JSON object", own_settings.display())]
    );
    // With private mode off the file shows (the line is wrapped at the card's width).
    let open = screen(&app);
    assert!(
        open.contains("x: y/settings.json is not a JSON object"),
        "{open}"
    );
    app.private = true;
    assert_eq!(
        private_problems(&app),
        ["~/•••/•••/••• is not a JSON object"]
    );
    let private = screen(&app);
    assert!(
        private.contains("! ~/•••/•••/••• is not a JSON object"),
        "{private}"
    );
    leaks_nothing(&private);

    // The home's own, with a source and without.
    fs::remove_file(&own_settings).unwrap();
    write(&source_settings, "{}");
    write(&own_settings, "[]");
    for sharing in [&h.sharing, &Sharing::default()] {
        let (mut app, view) = h.with_config(sharing);
        assert_eq!(
            view.problems,
            [format!("{} is not a JSON object", own_settings.display())]
        );
        app.private = true;
        assert_eq!(
            private_problems(&app),
            ["~/•••/•••/••• is not a JSON object"]
        );
        let private = screen(&app);
        assert!(
            private.contains("! ~/•••/•••/••• is not a JSON object"),
            "{private}"
        );
        leaks_nothing(&private);
    }

    // One that cannot be read (a directory): the file is masked whole, and the reason, which
    // the system gives, follows it.
    fs::remove_file(&own_settings).unwrap();
    fs::create_dir(&own_settings).unwrap();
    let (mut app, view) = h.with_config(&h.sharing);
    let problem = view.problems[0].to_string();
    let said = format!("cannot read {}: ", own_settings.display());
    let reason = problem
        .strip_prefix(&said)
        .unwrap_or_else(|| panic!("{problem}"));
    assert!(!reason.is_empty() && !reason.contains('/'), "{problem}");
    app.private = true;
    assert_eq!(
        private_problems(&app),
        [format!("cannot read ~/•••/•••/•••: {reason}")]
    );
    leaks_nothing(&screen(&app));
}

/// R11, R21: the checks of the accounts view, from the files to the screen. In private mode
/// each path of a check is masked whole (a home, a store, both ends of the link to make, the
/// `<item>` placeholder with them), what the check says around them stays, and so does the
/// slash command its own words name.
#[test]
fn checks_keep_what_they_say_around_their_paths_in_private_mode() {
    let h = Homes::new();
    let gone = h.home.join("zq gone, (old)");
    let mut accounts = h.accounts();
    accounts.push(Account {
        provider: CLAUDE,
        name: "zqgone".into(),
        home: Home::Path(gone.display().to_string()),
    });
    let mut env = h.env.clone();
    env.insert(checks::API_KEY_VAR.into(), "sk-test".into());
    let stores = index::stores(&accounts, &env);
    let mut found = checks::run(&accounts, &env, &stores);
    found.extend(checks::sharing(&accounts, &env, &h.sharing));
    let said: Vec<String> = found.iter().map(|c| c.message.to_string()).collect();
    let source = h.source_home();
    assert_eq!(
        said,
        [
            "ANTHROPIC_API_KEY is set: it overrides every account's /login".to_string(),
            format!("home {} does not exist", gone.display()),
            format!(
                "does not share projects with claude:default: it does not see the sessions in \
                 {} and cannot resume them; to share an item: ln -s {}/<item> {}/<item>",
                source.join("projects").display(),
                source.display(),
                h.member_home().display()
            ),
        ]
    );

    let mut app = h.app(accounts);
    update(&mut app, Event::Checks(found));
    let open = screen(&app);
    assert!(open.contains("zq gone, (old) does not exist"), "{open}");
    app.private = true;
    let copy = privacy::redacted(&app);
    let masked: Vec<String> = copy
        .checks
        .iter()
        .flatten()
        .map(|c| c.message.to_string())
        .collect();
    assert_eq!(
        masked,
        [
            "ANTHROPIC_API_KEY is set: it overrides every account's /login",
            "home ~/••• does not exist",
            "does not share projects with claude:default: it does not see the sessions in \
             ~/•••/••• and cannot resume them; to share an item: ln -s ~/•••/••• \
             ~/•••/•••/•••",
        ]
    );
    let private = screen(&app);
    for said in [
        "! ANTHROPIC_API_KEY is set: it overrides every account's /login",
        "! account-2: home ~/••• does not exist",
        // A note (R11), after the warnings; wrapped at this width.
        "· account-1: does not share projects with claude:default: it does not see the \
         sessions in ~/•••/••• and cannot resume them",
        "~/•••/•••/•••",
    ] {
        assert!(private.contains(said), "{said}:\n{private}");
    }
    let at = |said: &str| private.find(said).unwrap();
    assert!(at("! account-2: home") < at("· account-1: does not share"));
    leaks_nothing(&private);
}

/// R11, R21, R22: a member's `plugins` link that the source's installs go through, from the
/// files to the screen. It is no check: the accounts view says nothing of it. The member's
/// account card says that the link must stay, with the number of plugins, under the card's
/// part in sharing; in private mode the link is masked whole, the source shown by its alias,
/// and what the line says around them stays.
#[test]
fn a_plugins_link_that_must_stay_is_said_in_the_card_not_among_the_checks() {
    use std::os::unix::fs::symlink;

    let h = Homes::new();
    let source_home = h.home.join("zq src (old)").join("zqsrc, a: b");
    fs::create_dir_all(source_home.join("plugins/cache/m")).unwrap();
    let source = Account {
        provider: CLAUDE,
        name: "zqsrc".into(),
        home: Home::Path(source_home.display().to_string()),
    };
    let through = h.member_home().join("plugins");
    symlink(source_home.join("plugins"), &through).unwrap();
    write(
        &source_home.join("plugins/installed_plugins.json"),
        &serde_json::json!({
            "version": 2,
            "plugins": {
                "a@m": [{"scope": "user", "installPath": through.join("cache/m/a")}],
                "b@m": [{"scope": "user", "installPath": through.join("cache/m/b")}],
                "c@m": [{"scope": "user", "installPath": source_home.join("plugins/cache/m/c")}],
            },
        })
        .to_string(),
    );
    let sharing = Sharing {
        source: Some(source.clone()),
        ..Sharing::default()
    };
    let accounts = vec![source, h.member.clone()];

    let found = checks::sharing(&accounts, &h.env, &sharing);
    assert!(
        !found
            .iter()
            .any(|c| c.message.contains("must stay a symlink")),
        "{found:?}"
    );
    let view = account_config::read(&h.member, &sharing, None, &h.env);
    assert_eq!(
        view.plugins_link.as_ref().map(|m| m.to_string()),
        Some(format!(
            "{} must stay a symlink: 2 plugins of claude:zqsrc are installed through it and \
             stop loading for every account without it",
            through.display()
        ))
    );

    let mut app = h.app(accounts);
    update(&mut app, Event::Checks(found));
    assert!(!screen(&app).contains("must stay a symlink"));
    update(&mut app, Event::Key(Key::Char('p')));
    let request = app.config.work.round();
    update(
        &mut app,
        Event::Config {
            request,
            account: h.member.clone(),
            result: Ok(Box::new(view)),
        },
    );
    app.private = true;
    let copy = privacy::redacted(&app);
    let Some(Ok(masked)) = &copy.config.loaded else {
        panic!("no configuration loaded: {:?}", copy.config.loaded)
    };
    assert_eq!(
        masked
            .plugins_link
            .as_ref()
            .map(|m| m.to_string())
            .as_deref(),
        Some(
            "~/•••/•••/••• must stay a symlink: 2 plugins of claude:account-1 are installed \
             through it and stop loading for every account without it"
        )
    );
    let private = screen(&app);
    let (shares, link) = {
        let lines: Vec<&str> = private.lines().collect();
        let at = |said: &str| {
            lines
                .iter()
                .position(|l| l.contains(said))
                .unwrap_or_else(|| panic!("{said}:\n{private}"))
        };
        (
            at("member of account-1: shares what its home links to it"),
            at("~/•••/•••/••• must stay a symlink: 2 plugins of claude:account-1"),
        )
    };
    assert_eq!(link, shares + 1, "{private}");
    leaks_nothing(&private);
}

/// R6, R11, R21: the check about an inherited `CLAUDE_CODE_CHILD_SESSION` is remuda's own words
/// with no path or account in them, last among the environment's: private mode shows it whole.
#[test]
fn the_child_session_check_stays_whole_in_private_mode() {
    let h = Homes::new();
    let accounts = h.accounts();
    let mut env = h.env.clone();
    env.insert(checks::API_KEY_VAR.into(), "sk-test".into());
    env.insert("CLAUDE_CODE_CHILD_SESSION".into(), "TRUE".into());
    let stores = index::stores(&accounts, &env);
    let found = checks::run(&accounts, &env, &stores);
    let said = "CLAUDE_CODE_CHILD_SESSION is set: claude saves no transcript or prompt history for \
                an interactive session started here, so what is said in it never reaches remuda's \
                session list and cannot be resumed, though the launch is logged; set \
                CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1 to keep it";
    let messages: Vec<(Option<String>, String)> = found
        .iter()
        .map(|c| (c.account.clone(), c.message.to_string()))
        .collect();
    assert_eq!(
        messages[..2],
        [
            (
                None,
                "ANTHROPIC_API_KEY is set: it overrides every account's /login".to_string()
            ),
            (None, said.to_string()),
        ]
    );

    let mut app = h.app(accounts);
    update(&mut app, Event::Checks(found));
    app.private = true;
    let copy = privacy::redacted(&app);
    let masked: Vec<String> = copy
        .checks
        .iter()
        .flatten()
        .map(|c| c.message.to_string())
        .collect();
    assert_eq!(masked[1], said);
    let private = screen(&app);
    assert!(!private.contains("TRUE"), "{private}");
    leaks_nothing(&private);
}

/// R11, R18, R21: the checks whose words come from the catalog of a home's items (what a
/// member loses without a link, what breaks when an item is another account's), from the
/// files to the screen. Each end of the link to make is a path of its own, a settings key read
/// from the source's file is a piece of its own, and everything the catalog says stays
/// readable in private mode: `/rewind` among it.
#[test]
fn checks_worded_by_the_item_catalog_keep_their_words_in_private_mode() {
    use std::os::unix::fs::symlink;

    let h = Homes::new();
    let source = h.source_home();
    let member = h.member_home();
    // The source: file backups, agent memory, two instruction items, and settings that choose
    // a memory directory and hold an authentication key.
    fs::create_dir_all(source.join("file-history")).unwrap();
    fs::create_dir_all(source.join("agent-memory")).unwrap();
    fs::create_dir_all(source.join("skills")).unwrap();
    write(&source.join("CLAUDE.md"), "x");
    write(
        &source.join("settings.json"),
        r#"{"autoMemoryDirectory": "m", "apiKeyHelper": "/zq/key", "env": {"ANTHROPIC_AUTH_TOKEN": "t"}}"#,
    );
    // The first member links nothing and keeps a history of its own.
    write(&member.join("history.jsonl"), "");
    // The second links `projects`, one instruction item of two and the settings file, and
    // its history is the first member's.
    let other_home = h.home.join("zq homes (old)").join("zqother (b), z");
    fs::create_dir_all(&other_home).unwrap();
    symlink(source.join("projects"), other_home.join("projects")).unwrap();
    symlink(source.join("CLAUDE.md"), other_home.join("CLAUDE.md")).unwrap();
    symlink(
        source.join("settings.json"),
        other_home.join("settings.json"),
    )
    .unwrap();
    symlink(
        member.join("history.jsonl"),
        other_home.join("history.jsonl"),
    )
    .unwrap();
    let mut accounts = h.accounts();
    accounts.push(Account {
        provider: CLAUDE,
        name: "zqother".into(),
        home: Home::Path(other_home.display().to_string()),
    });

    let found = checks::sharing(&accounts, &h.env, &h.sharing);
    let mut app = h.app(accounts);
    update(&mut app, Event::Checks(found.clone()));
    let open = screen(&app);
    assert!(open.contains("zqother (b), z/file-history"), "{open}");
    app.private = true;
    let copy = privacy::redacted(&app);
    let masked: Vec<(String, String)> = copy
        .checks
        .iter()
        .flatten()
        .map(|c| (c.account.clone().unwrap_or_default(), c.message.to_string()))
        .collect();
    let said = |account: &str, message: &str| (account.to_string(), message.to_string());
    assert_eq!(
        masked,
        [
            said(
                "claude:account-1",
                "does not share projects, file-history, CLAUDE.md, skills, agent-memory with \
                 claude:default: it does not see the sessions in ~/•••/••• and cannot resume \
                 them; to share an item: ln -s ~/•••/••• ~/•••/•••/•••; settings.json is not \
                 shared: that of claude:default sets authentication settings (apiKeyHelper, \
                 env.ANTHROPIC_AUTH_TOKEN), so this account needs one of its own"
            ),
            said(
                "claude:account-2",
                "history.jsonl is a symlink to that of claude:account-1: sessions in a shared \
                 store lose their attribution; each account needs its own"
            ),
            said(
                "claude:account-2",
                "shares projects with claude:default through a symlink but not file-history: \
                 /rewind does not find the file backups of a session resumed from another \
                 account; link it too (~/•••/•••/••• -> ~/•••/•••)"
            ),
            said(
                "claude:account-2",
                "shares projects with claude:default through a symlink but not agent-memory, \
                 and a settings file chooses autoMemoryDirectory, so a launch does not redirect \
                 memory: the memory of user-scope subagents is not shared with this account; \
                 link it too (~/•••/•••/••• -> ~/•••/•••)"
            ),
            said(
                "claude:account-2",
                "does not share skills with claude:default; to share an item: ln -s ~/•••/••• \
                 ~/•••/•••/•••"
            ),
            said(
                "claude:account-2",
                "settings.json is that of claude:default through a symlink, and it sets \
                 authentication settings (apiKeyHelper, env.ANTHROPIC_AUTH_TOKEN): this account \
                 reads them through the link; give it a settings.json of its own instead of the \
                 link, or move them out of that settings.json"
            ),
        ]
    );
    // With private mode off each of them names its paths: nothing was masked for want of one.
    let paths = |check: &checks::Check| {
        check
            .message
            .pieces()
            .filter(|(_, kind)| *kind == Piece::Path)
            .count()
    };
    assert_eq!(
        found.iter().map(paths).collect::<Vec<_>>(),
        [3, 0, 2, 2, 2, 0]
    );
    // On screen (the long ones are wrapped at this width).
    let private = screen(&app);
    for said in [
        "/rewind does not find the file backups of a session resumed from",
        "another account; link it too (~/•••/•••/••• -> ~/•••/•••)",
        "the memory of user-scope subagents is not shared with this account; link it too \
         (~/•••/•••/••• -> ~/•••/•••)",
        "! account-2: history.jsonl is a symlink to that of claude:account-1: sessions in a \
         shared store lose their attribution; each account needs its own",
        "so this account needs one of its own",
        "give it a settings.json of its own instead of the link, or move them out of that \
         settings.json",
    ] {
        assert!(private.contains(said), "{said}:\n{private}");
    }
    leaks_nothing(&private);
}
