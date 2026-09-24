//! The TUI (SPEC R5, R7-R11, R16): accounts with usage and reset timeline, live sessions,
//! session history with search and preview, and launching claude from them.
//!
//! [`app`] holds the state and the pure `update`; [`render`] draws it; this module owns the
//! terminal and the event loop (including foreground launches, which suspend the TUI), and
//! [`workers`] run the slow parts in the background.

pub mod app;
pub mod render;
pub mod search;
pub mod timeline;
pub mod workers;

#[cfg(test)]
mod tests;

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use crossterm::event::{self as term, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::provider::Provider;
use crate::registry::{Account, Registry};
use crate::{Env, launch, paths, setup, share};

use app::{App, Effect, Event, Exit, Key, LaunchRequest, Mode};

/// Everything the TUI needs from the process, captured once by the caller.
#[derive(Clone)]
pub struct Deps {
    pub accounts: Vec<Account>,
    pub env: Env,
    pub claude: Option<PathBuf>,
    pub codex: Option<PathBuf>,
    pub ps: Option<PathBuf>,
    /// Every account shown during this session (grow-only): a pre-launch check asks these as
    /// well as the registry's, so an account unregistered meanwhile is still asked (C2).
    pub seen: Vec<Account>,
    pub tz: TimeZone,
    pub clock: fn() -> Timestamp,
    /// `$REMUDA_HOME/config.toml`.
    pub config: PathBuf,
    pub state_dir: PathBuf,
    /// The directory remuda was started in: the default for new sessions.
    pub cwd: Option<PathBuf>,
    pub mode: Mode,
}

impl Deps {
    /// The provider's executable, when it was found on `PATH`.
    pub fn program(&self, provider: Provider) -> Option<&Path> {
        match provider {
            Provider::Claude => self.claude.as_deref(),
            Provider::Codex => self.codex.as_deref(),
        }
    }
}

/// How often the clock ticks (drives the preview debounce and the live refresh).
const TICK: Duration = Duration::from_millis(100);
/// The longest the loop waits for a key before looking at background results.
const INPUT_POLL: Duration = Duration::from_millis(20);
/// At most this many queued events are applied before a frame is drawn.
const BATCH: usize = 256;

/// Set by the panic hook: the terminal has been restored and the loop must stop.
static PANICKED: AtomicBool = AtomicBool::new(false);

/// Runs the TUI until `q` or Ctrl-C, or (in [`Mode::PickForRun`]) until an account is chosen,
/// which is returned. The terminal is restored on every exit path: normal, error, and panic
/// (via the panic hook).
pub fn run(deps: Deps) -> Result<Option<Account>> {
    install_panic_hook();
    enter().context("cannot set up the terminal")?;
    let result = Terminal::new(CrosstermBackend::new(io::stdout()))
        .map_err(anyhow::Error::from)
        .and_then(|mut terminal| event_loop(&mut terminal, deps));
    restore();
    result
}

/// The terminal mode found when the TUI started: restored before every child gets the
/// terminal and on the final exit (R16), whatever mode a child left behind.
static SAVED_MODE: OnceLock<libc::termios> = OnceLock::new();

fn enter() -> io::Result<()> {
    let saved = take_terminal(&mut RealTty)?;
    let _ = SAVED_MODE.set(saved);
    Ok(())
}

/// Best effort: leaves raw mode and the alternate screen, shows the cursor, and puts back the
/// mode the TUI started in.
fn restore() {
    give_back(&mut RealTty, SAVED_MODE.get());
}

/// The terminal state the TUI changes. A trait so that the order of the changes can be
/// tested; [`RealTty`] is the terminal.
pub trait Tty {
    /// A whole terminal mode (termios).
    type Mode: Clone;
    fn mode(&mut self) -> io::Result<Self::Mode>;
    fn set_mode(&mut self, mode: &Self::Mode) -> io::Result<()>;
    /// crossterm's raw mode: turning it on remembers the mode it starts from, turning it off
    /// goes back to that.
    fn raw(&mut self, on: bool) -> io::Result<()>;
    /// The alternate screen; leaving it also shows the cursor.
    fn alternate(&mut self, on: bool) -> io::Result<()>;
}

/// Takes the terminal for the TUI and returns its mode from before, saved once for every later
/// hand-over and the final exit: a child killed in raw or no-echo mode (e.g. by SIGKILL)
/// cannot then become the mode remuda restores.
fn take_terminal<T: Tty>(tty: &mut T) -> io::Result<T::Mode> {
    let saved = tty.mode()?;
    tty.raw(true)?;
    if let Err(e) = tty.alternate(true) {
        give_back(tty, Some(&saved));
        return Err(e);
    }
    Ok(saved)
}

/// Hands the terminal to a child in the saved mode.
fn hand_over<T: Tty>(tty: &mut T, saved: Option<&T::Mode>) -> io::Result<()> {
    tty.raw(false)?;
    if let Some(mode) = saved {
        tty.set_mode(mode)?;
    }
    tty.alternate(false)
}

/// Takes the terminal back from a child: raw mode starts from the saved mode, not from what
/// the child left.
fn take_back<T: Tty>(tty: &mut T, saved: Option<&T::Mode>) -> io::Result<()> {
    if let Some(mode) = saved {
        tty.set_mode(mode)?;
    }
    tty.raw(true)?;
    tty.alternate(true)
}

/// Best effort, on every way out: the saved mode, the normal screen, a visible cursor.
fn give_back<T: Tty>(tty: &mut T, saved: Option<&T::Mode>) {
    let _ = tty.raw(false);
    if let Some(mode) = saved {
        let _ = tty.set_mode(mode);
    }
    let _ = tty.alternate(false);
}

/// The process's terminal: stdin when it is one, else `/dev/tty` (as crossterm does).
pub struct RealTty;

impl RealTty {
    fn with_fd<R>(f: impl FnOnce(RawFd) -> io::Result<R>) -> io::Result<R> {
        // SAFETY: `isatty` only inspects the descriptor.
        if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
            return f(libc::STDIN_FILENO);
        }
        let tty = std::fs::File::open("/dev/tty")?;
        f(tty.as_raw_fd())
    }
}

impl Tty for RealTty {
    type Mode = libc::termios;

    fn mode(&mut self) -> io::Result<libc::termios> {
        RealTty::with_fd(|fd| {
            // SAFETY: an all-zero termios is a valid value to be overwritten by `tcgetattr`.
            let mut mode: libc::termios = unsafe { std::mem::zeroed() };
            // SAFETY: `fd` is open for the call and `mode` is a valid termios to fill.
            if unsafe { libc::tcgetattr(fd, &mut mode) } == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(mode)
        })
    }

    fn set_mode(&mut self, mode: &libc::termios) -> io::Result<()> {
        RealTty::with_fd(|fd| {
            // SAFETY: `fd` is open for the call and `mode` came from `tcgetattr`.
            if unsafe { libc::tcsetattr(fd, libc::TCSANOW, mode) } == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        })
    }

    fn raw(&mut self, on: bool) -> io::Result<()> {
        if on {
            terminal::enable_raw_mode()
        } else {
            terminal::disable_raw_mode()
        }
    }

    fn alternate(&mut self, on: bool) -> io::Result<()> {
        if on {
            execute!(io::stdout(), EnterAlternateScreen)
        } else {
            execute!(io::stdout(), LeaveAlternateScreen, cursor::Show)
        }
    }
}

fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            PANICKED.store(true, Ordering::SeqCst);
            restore();
            previous(info);
        }));
    });
}

/// Leaving the TUI for a foreground child and coming back (R16).
pub trait Screen {
    /// Leaves the alternate screen and raw mode: the terminal is the child's.
    fn suspend(&mut self) -> io::Result<()>;
    /// Takes the terminal back; the next draw repaints everything.
    fn resume(&mut self) -> io::Result<()>;
}

impl Screen for Terminal<CrosstermBackend<io::Stdout>> {
    fn suspend(&mut self) -> io::Result<()> {
        hand_over(&mut RealTty, SAVED_MODE.get())
    }

    fn resume(&mut self) -> io::Result<()> {
        take_back(&mut RealTty, SAVED_MODE.get())?;
        // Forget what ratatui thinks is on screen: the whole frame is drawn again.
        self.clear()
    }
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    deps: Deps,
) -> Result<Option<Account>> {
    let mut deps = Arc::new(deps);
    let (tx, rx) = mpsc::channel::<Event>();
    let size = terminal.size()?;
    let home = paths::user_home(&deps.env).map(|h| h.display().to_string());
    let mut app = App::new(deps.accounts.clone(), deps.tz.clone(), home, (deps.clock)());
    app.mode = deps.mode;
    app.cwd = deps.cwd.clone();
    app::update(&mut app, Event::Resize(size.width, size.height));
    for effect in app.start() {
        workers::spawn(effect, &deps, &tx);
    }
    terminal.draw(|f| render::render(&app, f))?;

    let mut next_tick = Instant::now() + TICK;
    loop {
        // Keys are read here, on the loop's own thread: while a launched claude has the
        // terminal, this thread is waiting for it, so nothing else reads its input.
        let mut batch = read_input(next_tick.saturating_duration_since(Instant::now()))?;
        batch.extend(drain(&rx, BATCH));
        if Instant::now() >= next_tick {
            batch.push(Event::Tick((deps.clock)()));
            next_tick = Instant::now() + TICK;
        }
        // Nothing happened (no key, result or tick): the screen is still current.
        let idle = batch.is_empty();
        for event in batch {
            // The registry was read again: every task started from now on (including those
            // this event starts) sees the same accounts as the app.
            if let Event::Accounts(accounts) = &event
                && *accounts != deps.accounts
            {
                deps = Arc::new(Deps {
                    accounts: accounts.clone(),
                    seen: workers::union(&[&deps.seen, accounts]),
                    ..Deps::clone(&deps)
                });
            }
            for effect in app::update(&mut app, event) {
                match effect {
                    Effect::Quit => return Ok(None),
                    Effect::Pick(account) => return Ok(Some(account)),
                    Effect::Launch(request) => {
                        let event = launch_in_foreground(terminal, &deps, request)?;
                        // Results come back through the queue, after this batch.
                        let _ = tx.send(event);
                        let size = terminal.size()?;
                        let _ = tx.send(Event::Resize(size.width, size.height));
                    }
                    Effect::Setup {
                        provider,
                        name,
                        email,
                    } => {
                        let event = setup_in_foreground(terminal, &deps, provider, &name, email)?;
                        // Whatever happened, the registry may have changed: the new account
                        // is picked up when this event is applied (above).
                        if let Ok(registry) = Registry::load(&deps.config) {
                            let accounts = registry.all(&deps.env);
                            if accounts != deps.accounts {
                                let _ = tx.send(Event::Accounts(accounts));
                            }
                        }
                        let _ = tx.send(event);
                        let size = terminal.size()?;
                        let _ = tx.send(Event::Resize(size.width, size.height));
                    }
                    other => workers::spawn(other, &deps, &tx),
                }
            }
        }
        if PANICKED.load(Ordering::SeqCst) {
            bail!("a background task panicked (see the message above)");
        }
        if idle {
            continue;
        }
        terminal.draw(|f| render::render(&app, f))?;
    }
}

fn drain(rx: &Receiver<Event>, max: usize) -> Vec<Event> {
    rx.try_iter().take(max).collect()
}

/// Key presses and resizes that arrive within `wait` (then whatever else is already queued).
fn read_input(wait: Duration) -> io::Result<Vec<Event>> {
    let mut events = Vec::new();
    let mut wait = wait.min(INPUT_POLL);
    while events.len() < BATCH && term::poll(wait)? {
        wait = Duration::ZERO;
        match term::read()? {
            term::Event::Key(key) => events.extend(decode(key).map(Event::Key)),
            term::Event::Resize(w, h) => events.push(Event::Resize(w, h)),
            _ => {}
        }
    }
    Ok(events)
}

/// Runs a [`LaunchRequest`] the way `remuda run` would (R6: same env change, injection and
/// launch log), in the foreground with the TUI suspended (R16), and reports how it ended.
/// Only a failure to take the terminal back is an error: the TUI cannot go on without it.
pub fn launch_in_foreground(
    screen: &mut impl Screen,
    deps: &Deps,
    request: LaunchRequest,
) -> Result<Event> {
    let (result, warnings) = run_launch(screen, deps, &request)?;
    Ok(Event::Launched {
        request,
        result,
        warnings,
    })
}

fn run_launch(
    screen: &mut impl Screen,
    deps: &Deps,
    request: &LaunchRequest,
) -> Result<(Result<Exit, String>, Vec<String>)> {
    let provider = request.account.provider;
    let Some(program) = deps.program(provider) else {
        let missing = format!("`{}` not found on PATH", provider.program());
        return Ok((Err(missing), Vec::new()));
    };
    let cwd = request.cwd.as_deref().or(deps.cwd.as_deref());
    // The registry as it is now, for its shared configuration (R18).
    let planned = Registry::load(&deps.config).and_then(|registry| {
        launch::plan(
            &request.account,
            request.args.clone(),
            cwd,
            (deps.clock)().to_string(),
            || uuid::Uuid::new_v4().to_string(),
            &registry.sharing,
            &deps.env,
            &share::dir(&deps.config),
        )
    });
    let plan = match planned {
        Ok(plan) => plan,
        Err(e) => return Ok((Err(format!("{e:#}")), Vec::new())),
    };
    let mut warnings = launch::env_warnings(&deps.env);
    warnings.extend(plan.notices.iter().cloned());
    let log = deps.state_dir.join("launches.jsonl");
    if let Err(e) = screen.suspend() {
        let _ = screen.resume();
        return Ok((Err(format!("cannot hand the terminal over: {e}")), warnings));
    }
    let where_ = cwd.map_or(String::new(), |d| format!(" in {}", d.display()));
    println!("remuda: {}{where_}", request.what);
    let ran = launch::perform(program, &plan, cwd, &log);
    screen
        .resume()
        .with_context(|| format!("cannot take the terminal back after {}", provider.program()))?;
    warnings.extend(ran.log_error);
    Ok((ran.status.map(exit_of).map_err(|e| e.to_string()), warnings))
}

/// `remuda setup --provider <p> <name>` from the TUI (R5, R16, R17): the same checks and steps
/// as the command line, with the login (`claude auth login`, `codex login`) in the foreground
/// while the TUI is suspended. Nothing is created when a check fails or the terminal cannot be
/// handed over; a failed login keeps the registration. Like a launch, only a failure to take
/// the terminal back is an error.
pub fn setup_in_foreground(
    screen: &mut impl Screen,
    deps: &Deps,
    provider: Provider,
    name: &str,
    email: Option<String>,
) -> Result<Event> {
    let result = run_setup(screen, deps, provider, name, email)?;
    Ok(Event::SetupDone {
        provider,
        name: name.to_string(),
        result,
    })
}

fn run_setup(
    screen: &mut impl Screen,
    deps: &Deps,
    provider: Provider,
    name: &str,
    email: Option<String>,
) -> Result<Result<Exit, String>> {
    let checked = || -> Result<_, String> {
        let account =
            setup::plan(&deps.config, provider, name, &deps.env).map_err(|e| format!("{e:#}"))?;
        let program = deps
            .program(provider)
            .ok_or_else(|| format!("`{}` not found on PATH", provider.program()))?;
        let args = provider.login_args(email)?;
        let change = launch::env_change(&account);
        Ok((account, program, args, change))
    };
    let (account, program, args, change) = match checked() {
        Ok(checked) => checked,
        Err(e) => return Ok(Err(e)),
    };
    if let Err(e) = screen.suspend() {
        let _ = screen.resume();
        return Ok(Err(format!("cannot hand the terminal over: {e}")));
    }
    let login = setup::login_command(provider);
    let status = match setup::create_and_register(&deps.config, &account) {
        Err(e) => Err(format!("{e:#}")),
        Ok(()) => {
            println!(
                "remuda: registered {} at {}; running `{login}`",
                account.qualified(),
                account.home
            );
            launch::run_foreground(program, &args, &change, None)
                .map(exit_of)
                .map_err(|e| e.to_string())
        }
    };
    screen
        .resume()
        .with_context(|| format!("cannot take the terminal back after `{login}`"))?;
    Ok(status)
}

fn exit_of(status: ExitStatus) -> Exit {
    match (status.code(), status.signal()) {
        (Some(code), _) => Exit::Code(code),
        (None, Some(signal)) => Exit::Signal(signal),
        (None, None) => Exit::Code(-1),
    }
}

/// Crossterm key event to [`Key`]; releases and unmapped keys are `None`.
fn decode(key: KeyEvent) -> Option<Key> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    Some(match key.code {
        KeyCode::Char(c) if ctrl => Key::Ctrl(c.to_ascii_lowercase()),
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        _ => return None,
    })
}

#[cfg(test)]
mod decode_tests {
    use super::*;

    #[test]
    fn decodes_keys() {
        let k = |code, mods| KeyEvent::new(code, mods);
        assert_eq!(
            decode(k(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Key::Ctrl('c'))
        );
        assert_eq!(
            decode(k(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(Key::Char('q'))
        );
        assert_eq!(
            decode(k(KeyCode::Char('G'), KeyModifiers::SHIFT)),
            Some(Key::Char('G'))
        );
        assert_eq!(
            decode(k(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some(Key::BackTab)
        );
        assert_eq!(decode(k(KeyCode::F(1), KeyModifiers::NONE)), None);
        let mut release = k(KeyCode::Char('q'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(decode(release), None);
    }
}

#[cfg(test)]
mod tty_tests {
    use super::*;

    const COOKED: u32 = 1;
    /// What a child killed in raw, no-echo mode leaves behind.
    const DIRTY: u32 = 99;
    /// Raw mode made from `mode`.
    fn raw_of(mode: u32) -> u32 {
        mode + 1000
    }

    /// A terminal with crossterm's raw-mode bookkeeping: `raw(true)` remembers the mode it
    /// starts from (once), `raw(false)` goes back to it and forgets it.
    struct FakeTty {
        mode: u32,
        prior: Option<u32>,
        snapshots: usize,
        alternate: bool,
        fail_alternate: bool,
    }

    impl FakeTty {
        fn new() -> Self {
            FakeTty {
                mode: COOKED,
                prior: None,
                snapshots: 0,
                alternate: false,
                fail_alternate: false,
            }
        }
    }

    impl Tty for FakeTty {
        type Mode = u32;
        fn mode(&mut self) -> io::Result<u32> {
            self.snapshots += 1;
            Ok(self.mode)
        }
        fn set_mode(&mut self, mode: &u32) -> io::Result<()> {
            self.mode = *mode;
            Ok(())
        }
        fn raw(&mut self, on: bool) -> io::Result<()> {
            match (on, self.prior) {
                (true, None) => {
                    self.prior = Some(self.mode);
                    self.mode = raw_of(self.mode);
                }
                (false, Some(prior)) => {
                    self.mode = prior;
                    self.prior = None;
                }
                _ => {}
            }
            Ok(())
        }
        fn alternate(&mut self, on: bool) -> io::Result<()> {
            if on && self.fail_alternate {
                return Err(io::Error::other("no alternate screen"));
            }
            self.alternate = on;
            Ok(())
        }
    }

    /// A child killed while the terminal is raw and silent must not become the
    /// mode handed to the next child or left to the shell.
    #[test]
    fn the_mode_from_before_the_tui_is_restored_whatever_a_child_left() {
        let mut tty = FakeTty::new();
        let saved = take_terminal(&mut tty).unwrap();
        assert_eq!(
            (saved, tty.mode, tty.alternate),
            (COOKED, raw_of(COOKED), true)
        );

        hand_over(&mut tty, Some(&saved)).unwrap();
        assert_eq!((tty.mode, tty.alternate), (COOKED, false));
        // The child is SIGKILLed in raw, no-echo mode.
        tty.mode = DIRTY;
        take_back(&mut tty, Some(&saved)).unwrap();
        assert_eq!(
            tty.mode,
            raw_of(COOKED),
            "raw mode is made from the saved mode"
        );
        assert!(tty.alternate);

        // The next child gets the saved mode ...
        hand_over(&mut tty, Some(&saved)).unwrap();
        assert_eq!(tty.mode, COOKED);
        tty.mode = DIRTY;
        take_back(&mut tty, Some(&saved)).unwrap();
        // ... and so does the shell.
        give_back(&mut tty, Some(&saved));
        assert_eq!((tty.mode, tty.alternate), (COOKED, false));
        assert_eq!(tty.snapshots, 1, "saved once, when the TUI started");
    }

    /// Without the snapshot (crossterm alone) the dirty mode wins: what the test above guards.
    #[test]
    fn without_a_saved_mode_the_childs_mode_would_stick() {
        let mut tty = FakeTty::new();
        take_terminal(&mut tty).unwrap();
        hand_over(&mut tty, None).unwrap();
        tty.mode = DIRTY;
        take_back(&mut tty, None).unwrap();
        give_back(&mut tty, None);
        assert_eq!(tty.mode, DIRTY);
    }

    #[test]
    fn a_failed_start_gives_the_terminal_back() {
        let mut tty = FakeTty::new();
        tty.fail_alternate = true;
        assert!(take_terminal(&mut tty).is_err());
        assert_eq!((tty.mode, tty.alternate, tty.prior), (COOKED, false, None));
    }
}
