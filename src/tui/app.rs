//! TUI state and its pure transition function: `update(&mut App, Event) -> Vec<Effect>`.
//! Nothing here touches the terminal, the clock, the environment or the file system; slow
//! work is requested as [`Effect`]s and comes back as [`Event`]s.

use std::collections::HashMap;
use std::fmt::Display;
use std::ops::Deref;
use std::path::{Path, PathBuf};

use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};

use crate::account_config::ConfigView;
use crate::attribution;
use crate::attribution::Attribution;
use crate::checks::Check;
use crate::identity::Identity;
use crate::index::{Entry, Index, Store};
use crate::launch::{self, Intent};
use crate::live::{self, Control, LiveId, LiveSession};
use crate::privacy::Aliases;
use crate::provider::Provider;
use crate::registry::{self, Account, CLAUDE, CODEX, Home};
use crate::setup;
use crate::stats::{self, Period};
use crate::transcript::Message;
use crate::usage::{CachedUsage, LiveResult, LiveUsage, UsageRow};

use super::work::{Claim, Round, Slot};
use super::{render, search};

/// Live sessions are re-collected this long after the last collection finished (R7).
pub const LIVE_EVERY: SignedDuration = SignedDuration::from_secs(5);
/// A selection must stay put for this many ticks before its preview is loaded.
pub const PREVIEW_DEBOUNCE_TICKS: u8 = 2;
/// Messages shown in a preview (R8).
pub const PREVIEW_MESSAGES: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Accounts,
    Live,
    History,
    /// Token statistics (R20).
    Stats,
}

impl View {
    pub const ALL: [View; 4] = [View::Accounts, View::Live, View::History, View::Stats];

    pub fn title(self) -> &'static str {
        match self {
            View::Accounts => "Accounts",
            View::Live => "Live",
            View::History => "History",
            View::Stats => "Stats",
        }
    }

    fn position(self) -> usize {
        View::ALL.iter().position(|v| *v == self).unwrap_or(0)
    }
}

/// What the TUI is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Bare `remuda`: everything.
    Browse,
    /// `remuda run` without an account (R5): choose one; the TUI then exits and claude is
    /// exec'd as that account.
    PickForRun,
}

/// A key press, already decoded from the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    /// Control + a letter.
    Ctrl(char),
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Key(Key),
    /// The clock, read by the event loop.
    Tick(Timestamp),
    /// Terminal size in columns and rows.
    Resize(u16, u16),
    /// The cached index, before any file is read.
    IndexLoaded(Vec<Entry>),
    /// Entries (re)read since the last progress event.
    IndexProgress {
        done: usize,
        total: usize,
        entries: Vec<Entry>,
    },
    /// The complete index after a refresh (vanished transcripts are gone from it).
    IndexDone {
        entries: Vec<Entry>,
        error: Option<String>,
    },
    Identity {
        account: Account,
        identity: Identity,
    },
    CachedUsage {
        account: Account,
        result: Result<CachedUsage, String>,
    },
    LiveUsage {
        account: Account,
        result: Result<LiveResult, String>,
    },
    Live(Vec<LiveSession>),
    /// Launch log and `history.jsonl` attribution (live sessions are merged in by the app).
    Attribution(Attribution),
    Checks(Vec<Check>),
    Preview {
        path: PathBuf,
        result: Result<Vec<Message>, String>,
    },
    /// [`Effect::Config`] number `request` answered for `account` (R22).
    Config {
        request: u64,
        account: Account,
        result: Result<Box<ConfigView>, String>,
    },
    /// The transcript stores (realpaths of `projects`) and the accounts sharing each.
    Stores(Vec<Store>),
    /// A pending launch was checked ([`Effect::CheckLaunch`] number `check`); `error` says
    /// why it cannot go ahead.
    LaunchChecked {
        check: u64,
        request: LaunchRequest,
        error: Option<Marked>,
    },
    /// `claude logs <short_id>` under `account` as plain text, or why it failed. Two accounts
    /// may each have a background session of one short id.
    Logs {
        account: Account,
        short_id: String,
        result: Result<String, String>,
    },
    /// `claude stop|rm <short_id>` finished: its output, or why it failed.
    ControlDone {
        verb: Control,
        short_id: String,
        result: Result<String, String>,
    },
    /// Transcripts read so far by [`Effect::Stats`] / transcripts that need reading.
    StatsProgress {
        done: usize,
        total: usize,
    },
    /// [`Effect::Stats`] finished; `error` says why the cache could not be written.
    Stats {
        report: stats::Report,
        error: Option<String>,
    },
    /// The registry was read again (after a setup or a removal, or by a pre-launch check).
    Accounts(Vec<Account>),
    /// [`Effect::RemoveAccount`] finished: done, or why not.
    AccountRemoved {
        account: Account,
        result: Result<(), String>,
    },
    /// [`Effect::RolloutWritten`]: when the rollout at `path` was last written, if it could be
    /// read.
    RolloutWritten {
        path: PathBuf,
        at: Option<Timestamp>,
    },
    /// A setup from the TUI ended: the login's exit, or why it did not get there, and what
    /// there is to say about the links of the new home (R18).
    SetupDone {
        provider: Provider,
        name: String,
        result: Result<Exit, String>,
        links: Vec<setup::Note>,
    },
    /// A foreground launch ended: claude's exit, or why it could not run.
    Launched {
        request: LaunchRequest,
        result: Result<Exit, String>,
        /// E.g. the launch log could not be written.
        warnings: Vec<String>,
    },
}

/// A claude run in the foreground while the TUI is suspended (R16). The event loop turns it
/// into a [`crate::launch::prepare`] plan, exactly like `remuda run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRequest {
    pub account: Account,
    /// Arguments as a user would pass them to `remuda run` (`--session-id` is injected later).
    pub args: Vec<String>,
    /// Where claude runs; `None`: remuda's own directory.
    pub cwd: Option<PathBuf>,
    /// For the status line, e.g. `resume 766560c5 as max`.
    pub what: String,
}

impl LaunchRequest {
    /// The claude session this launch continues in place (`--resume <id>` without
    /// `--fork-session`): it must not be running anywhere (R16). Forks, new sessions and
    /// attaches continue nothing in place. Codex has no running sessions to check against: the
    /// user is asked instead (R17).
    pub fn resumes(&self) -> Option<String> {
        if !self.account.provider.has_live() {
            return None;
        }
        match launch::classify(&self.args) {
            Intent::Existing {
                session_id: Some(id),
                fork_of: None,
            } => Some(id),
            _ => None,
        }
    }
}

/// How a foreground claude ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    Signal(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

/// Text the TUI puts together (a notice, a form's error), in the pieces it was made of: the
/// words and names remuda wrote, each path it put there, each message that came from elsewhere
/// (an agent's output, a system error). It reads as one string; private mode (R21) masks it
/// piece by piece, a path whole whatever characters it holds, and what it must guess in one
/// message never reaches into the next.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Marked {
    text: String,
    /// Where each piece ends in `text`, and whether it is a path.
    pieces: Vec<(usize, bool)>,
}

impl Marked {
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// With `text` after it, as a piece of its own.
    pub fn text(self, text: impl AsRef<str>) -> Self {
        self.piece(text.as_ref(), false)
    }

    /// With the path `path` after it.
    pub fn path(self, path: impl Display) -> Self {
        self.piece(&path.to_string(), true)
    }

    /// With the pieces of `other` after it.
    pub fn join(mut self, other: &Marked) -> Self {
        for (piece, path) in other.pieces() {
            self = self.piece(piece, path);
        }
        self
    }

    fn piece(mut self, piece: &str, path: bool) -> Self {
        if !piece.is_empty() {
            self.text.push_str(piece);
            self.pieces.push((self.text.len(), path));
        }
        self
    }

    /// Each piece in order, and whether it is a path.
    pub fn pieces(&self) -> impl Iterator<Item = (&str, bool)> {
        let mut start = 0;
        self.pieces.iter().map(move |(end, path)| {
            let piece = &self.text[start..*end];
            start = *end;
            (piece, *path)
        })
    }
}

impl From<String> for Marked {
    fn from(text: String) -> Self {
        Marked::default().text(text)
    }
}

impl From<&str> for Marked {
    fn from(text: &str) -> Self {
        Marked::default().text(text)
    }
}

impl Deref for Marked {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

impl PartialEq<str> for Marked {
    fn eq(&self, other: &str) -> bool {
        self.text == other
    }
}

impl PartialEq<&str> for Marked {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

/// A one-line message in the status bar; the next key press clears it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub text: Marked,
    pub level: Level,
}

/// Work for the event loop: background tasks, or (for [`Effect::Launch`]) a foreground run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    RefreshIndex,
    /// `claude auth status` / `codex login status` for these accounts.
    Identities(Vec<Account>),
    /// The cached usage of these accounts.
    CachedUsage(Vec<Account>),
    /// `claude -p /usage` for these accounts.
    LiveUsage(Vec<Account>),
    Live,
    Attribution,
    Checks,
    /// Token statistics (R20): the statistics cache brought up to date, then a report.
    Stats,
    /// The last messages of a transcript or rollout of this provider.
    Preview(PathBuf, Provider),
    /// The account's configuration for a new session in `cwd` (R22), answered by
    /// [`Event::Config`].
    Config {
        request: u64,
        account: Account,
        cwd: Option<PathBuf>,
    },
    /// Checks right before a launch, answered by [`Event::LaunchChecked`]: the request's
    /// directory exists, and a session resumed in place ([`LaunchRequest::resumes`]) is not
    /// running in any account by a fresh `agents --json` of every account (R16). `check`
    /// numbers the checks: only the answer to the one pending counts.
    CheckLaunch {
        check: u64,
        request: LaunchRequest,
    },
    /// The rollout's mtime, for [`Overlay::ResumeCodex`] (R17), answered by
    /// [`Event::RolloutWritten`]: read from the file itself, not from an index refresh that may
    /// queue behind a long scan.
    RolloutWritten(PathBuf),
    /// Suspend the TUI, run claude in the foreground and wait (R16).
    Launch(LaunchRequest),
    /// [`Mode::PickForRun`]: this account was chosen; the TUI exits.
    Pick(Account),
    /// `claude logs <short_id>` under the account's environment, captured.
    Logs {
        account: Account,
        short_id: String,
    },
    /// `claude stop|rm <short_id>` under the account's environment, captured.
    Control {
        account: Account,
        verb: Control,
        short_id: String,
    },
    /// `remuda setup --provider <p> <name> [--email]` (R5, R17): create and register the home,
    /// then run the login (`claude auth login`, `codex login`) in the foreground like a launch;
    /// the registry is read again after.
    Setup {
        provider: Provider,
        name: String,
        email: Option<String>,
    },
    /// `remuda remove` (R14a): unregister the account, then read the registry again.
    RemoveAccount(Account),
    Quit,
}

/// One account row of the accounts view.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountState {
    pub account: Account,
    /// `None` until `claude auth status` first answers.
    pub identity: Option<Identity>,
    /// `None` until first loaded.
    pub cached: Option<Result<CachedUsage, String>>,
    /// The last live query: rows and when they arrived, or why it failed.
    pub live: Option<Result<(Vec<UsageRow>, Timestamp), String>>,
    /// What is being asked of the account.
    pub work: AccountWork,
}

/// What is asked of one account in the background: a slot for each [`Query`]. A slot moves
/// with its account's row, and an account no longer listed takes its slots with it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountWork {
    /// `claude auth status` / `codex login status`.
    pub identity: Slot,
    /// The cached usage.
    pub cached: Slot,
    /// The live usage query.
    pub live: Slot,
}

impl AccountWork {
    fn slot(&mut self, query: Query) -> &mut Slot {
        match query {
            Query::Identity => &mut self.identity,
            Query::CachedUsage => &mut self.cached,
            Query::LiveUsage => &mut self.live,
        }
    }
}

/// What is asked of each account, answered by one event for each account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Query {
    Identity,
    CachedUsage,
    LiveUsage,
}

impl Query {
    fn effect(self, accounts: Vec<Account>) -> Effect {
        match self {
            Query::Identity => Effect::Identities(accounts),
            Query::CachedUsage => Effect::CachedUsage(accounts),
            Query::LiveUsage => Effect::LiveUsage(accounts),
        }
    }
}

/// A slot for each kind of background work that is neither an account's ([`AccountWork`]) nor
/// a pane's ([`ConfigPane::work`], [`Preview::work`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Background {
    pub index: Slot,
    /// Stale from a foreground child until a collection started after it has finished: `live`
    /// may miss what the child started (R16).
    pub live: Slot,
    pub attribution: Slot,
    pub checks: Slot,
    pub stats: Slot,
    /// The pre-launch check, and the launch waiting for it.
    pub launch: Slot<LaunchRequest>,
    /// `claude logs` of this account's background session of this short id.
    pub logs: Slot<(Account, String)>,
}

impl Background {
    fn slot(&mut self, work: Work) -> &mut Slot {
        match work {
            Work::Index => &mut self.index,
            Work::Live => &mut self.live,
            Work::Attribution => &mut self.attribution,
            Work::Checks => &mut self.checks,
            Work::Stats => &mut self.stats,
        }
    }
}

/// The background work that has no target: one round of each is out at most, and its result
/// carries no number (there is no other round it could be taken for).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Work {
    Index,
    Live,
    Attribution,
    Checks,
    Stats,
}

impl Work {
    fn effect(self) -> Effect {
        match self {
            Work::Index => Effect::RefreshIndex,
            Work::Live => Effect::Live,
            Work::Attribution => Effect::Attribution,
            Work::Checks => Effect::Checks,
            Work::Stats => Effect::Stats,
        }
    }
}

impl AccountState {
    fn new(account: Account) -> Self {
        AccountState {
            account,
            identity: None,
            cached: None,
            live: None,
            work: AccountWork::default(),
        }
    }

    /// Rows to show: the last successful live query, else the cache.
    pub fn rows(&self) -> &[UsageRow] {
        match (&self.live, &self.cached) {
            (Some(Ok((rows, _))), _) => rows,
            (_, Some(Ok(cached))) => &cached.rows,
            _ => &[],
        }
    }
}

/// A box over the view that takes the keys until it closes.
#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    /// Which account resumes or forks a session.
    Pick(Pick),
    Form(Form),
    /// `y` runs `claude stop|rm`; any other key cancels.
    Confirm(Confirm),
    /// `y` resumes a codex session in place; any other key cancels (R17).
    ResumeCodex(ResumeCodex),
    /// `y` unregisters the account (R14a, R16); any other key cancels.
    RemoveAccount(Account),
}

/// A codex resume waiting for the user: remuda cannot tell whether the session runs elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeCodex {
    /// The rollout.
    pub path: PathBuf,
    /// When the rollout was last written, which says whether codex may be writing it just now:
    /// the index's mtime until the file's own ([`Event::RolloutWritten`]) arrives.
    pub written: Option<Timestamp>,
    /// Its account is checked again on `y`: it may have been unregistered meanwhile.
    pub request: LaunchRequest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirm {
    pub verb: Control,
    /// `provider:name`.
    pub account: String,
    pub short_id: String,
}

/// `claude logs` of the selected background session, shown in the preview area.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Logs {
    pub key: (String, LiveId),
    pub short_id: String,
    /// `None` while loading.
    pub result: Option<Result<String, String>>,
}

/// A small form of text fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    pub kind: FormKind,
    pub fields: Vec<Field>,
    /// Index into `fields`.
    pub focus: usize,
    /// Why the last submit was refused.
    pub error: Option<Marked>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormKind {
    /// `n`: a new session for `account`: directory, optional name. The account itself, not its
    /// row: rows move when the registry changes while the form is open; on submit it is
    /// resolved by name again (R17).
    NewSession { account: Account },
    /// `s`: a new account: name, optional email.
    Setup,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub label: &'static str,
    pub value: String,
    /// How private mode shows the value (R21).
    pub mask: Mask,
}

/// What a form value is, for private mode (R21).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mask {
    /// Shown with each component masked.
    Path,
    /// Masked whole.
    Text,
    Email,
    /// Shown as it is (a provider).
    Plain,
}

impl Form {
    fn new(kind: FormKind, fields: &[(&'static str, String, Mask)]) -> Self {
        Form {
            kind,
            fields: fields
                .iter()
                .map(|(label, value, mask)| Field {
                    label,
                    value: value.clone(),
                    mask: *mask,
                })
                .collect(),
            focus: 0,
            error: None,
        }
    }

    fn value(&self, i: usize) -> &str {
        self.fields.get(i).map_or("", |f| f.value.trim())
    }
}

/// What a picked account does with the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickFor {
    Resume,
    Fork,
}

/// The accounts are `provider:name`, not rows of [`App::accounts`]: rows move when the
/// registry changes while the picker is open; the choice is resolved by name (R17).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pick {
    /// The transcript or rollout chosen in the list: the one resumed (R16).
    pub path: PathBuf,
    pub session_id: String,
    pub action: PickFor,
    /// Accounts of the session's provider only. Claude: the accounts that can see the
    /// transcript's store first, then the rest; the session's own accounts first within each.
    /// Codex: the accounts sharing the rollout's store (R17).
    pub options: Vec<String>,
    /// The accounts the session is attributed to.
    pub attributed: Vec<String>,
    pub selected: usize,
}

/// Selection and scroll position of a list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListState {
    pub selected: usize,
    pub offset: usize,
}

impl ListState {
    fn clamp(&mut self, len: usize, height: usize) {
        self.selected = self.selected.min(len.saturating_sub(1));
        let height = height.max(1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + height {
            self.offset = self.selected + 1 - height;
        }
        self.offset = self.offset.min(len.saturating_sub(height));
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct History {
    /// Transcript paths shown, in display order.
    pub rows: Vec<PathBuf>,
    pub list: ListState,
    /// Also show teammate and SDK sessions.
    pub show_all: bool,
    pub query: String,
    /// The `/` prompt is open.
    pub searching: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Preview {
    /// The transcript the current selection wants shown.
    pub target: Option<PathBuf>,
    /// Ticks the target has stayed the same.
    pub settled: u8,
    /// The transcript being read.
    pub work: Slot<PathBuf>,
    pub loaded: Option<(PathBuf, Result<Vec<Message>, String>)>,
    /// `Enter`: the preview takes the whole body.
    pub expanded: bool,
    /// Lines scrolled up from the bottom (expanded only).
    pub scroll: usize,
}

/// The Configuration pane of the accounts view (R22).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigPane {
    /// `p`: beside or below the view.
    pub open: bool,
    /// `p` again: the whole view; the movement keys scroll it.
    pub expanded: bool,
    /// First line shown.
    pub scroll: usize,
    /// The account shown: the selection's when last requested.
    pub account: Option<Account>,
    /// The last answer for `account`; kept while it is read again.
    pub loaded: Option<Result<ConfigView, String>>,
    /// The account being read, under the number of its [`Effect::Config`]: only the answer to
    /// that one is kept.
    pub work: Slot<Account>,
}

/// The Stats view (R20): computed the first time it opens, then on each `r`.
#[derive(Debug, Clone, PartialEq)]
pub struct StatsState {
    /// The last report; kept while the next one is computed.
    pub report: Option<stats::Report>,
    /// Why the last computation could not write the cache.
    pub error: Option<String>,
    /// The view has been opened: `r` computes again.
    pub requested: bool,
    /// `(done, total)` transcripts read by the computation running.
    pub progress: Option<(usize, usize)>,
    /// When the last report arrived.
    pub computed: Option<Timestamp>,
    pub period: Period,
    /// Lines scrolled down.
    pub scroll: usize,
}

impl Default for StatsState {
    fn default() -> Self {
        StatsState {
            report: None,
            error: None,
            requested: false,
            progress: None,
            computed: None,
            period: Period::All,
            scroll: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct App {
    pub mode: Mode,
    pub now: Timestamp,
    /// The directory remuda was started in: the default for new sessions.
    pub cwd: Option<PathBuf>,
    pub tz: TimeZone,
    /// `$HOME`, shown as `~` in paths.
    pub home: Option<String>,
    pub size: (u16, u16),
    pub view: View,
    pub help: bool,

    pub accounts: Vec<AccountState>,
    pub accounts_list: ListState,
    /// File-system and environment checks; `None` until they ran.
    pub checks: Option<Vec<Check>>,
    pub config: ConfigPane,

    pub index: Index,
    /// `(done, total)` while a refresh is running.
    pub indexing: Option<(usize, usize)>,
    /// The cache has been loaded (or found missing): an empty list now means no sessions.
    pub index_loaded: bool,
    pub index_refreshed: Option<Timestamp>,
    pub index_error: Option<String>,
    pub(super) by_session: HashMap<String, PathBuf>,

    /// Launch log + `history.jsonl`; `attribution` adds live sessions to it.
    pub(super) attribution_base: Attribution,
    pub attribution: Attribution,

    pub live: Vec<LiveSession>,
    /// Indices into `live` shown, in order: stopped background sessions only when
    /// `live_show_inactive`.
    pub live_rows: Vec<usize>,
    pub live_show_inactive: bool,
    pub live_list: ListState,
    pub logs: Option<Logs>,
    pub live_loaded: bool,
    /// When the last collection finished.
    pub live_updated: Option<Timestamp>,

    pub history: History,
    pub preview: Preview,
    pub stats: StatsState,

    pub notice: Option<Notice>,
    pub overlay: Option<Overlay>,
    /// `None` until the index worker has listed them.
    pub stores: Option<Vec<Store>>,
    /// What runs in the background: whether a round of each kind of work is out, and whether it
    /// runs once more when that round reports.
    pub work: Background,
    /// The pending launch a foreground child cancelled, told when the child ends.
    pub(super) cancelled: Option<String>,
    /// The check the open form started: only its answer belongs in the form (C1).
    pub(super) form_check: Option<Round>,
    /// `Ctrl-P`: the screen is drawn from [`super::privacy::redacted`] (R21).
    pub private: bool,
    /// Every account name seen, with its private-mode alias.
    pub aliases: Aliases,
}

impl App {
    pub fn new(accounts: Vec<Account>, tz: TimeZone, home: Option<String>, now: Timestamp) -> Self {
        // Aliases are numbered in registry order first (R21).
        let mut aliases = Aliases::default();
        for account in &accounts {
            aliases.note(&account.qualified());
        }
        App {
            mode: Mode::Browse,
            now,
            cwd: None,
            tz,
            home,
            size: (80, 24),
            view: View::Accounts,
            help: false,
            accounts: accounts.into_iter().map(AccountState::new).collect(),
            accounts_list: ListState::default(),
            checks: None,
            config: ConfigPane::default(),
            index: Index::default(),
            indexing: None,
            index_loaded: false,
            index_refreshed: None,
            index_error: None,
            by_session: HashMap::new(),
            attribution_base: Attribution::default(),
            attribution: Attribution::default(),
            live: Vec::new(),
            live_rows: Vec::new(),
            live_show_inactive: false,
            live_list: ListState::default(),
            logs: None,
            live_loaded: false,
            live_updated: None,
            history: History::default(),
            preview: Preview::default(),
            stats: StatsState::default(),
            notice: None,
            overlay: None,
            stores: None,
            work: Background::default(),
            cancelled: None,
            form_check: None,
            private: false,
            aliases,
        }
    }

    /// Effects for the first frame: everything loads in the background.
    pub fn start(&mut self) -> Vec<Effect> {
        let mut fx = Vec::new();
        self.refresh(&mut fx);
        fx
    }

    /// The live session selected in the live view.
    pub fn selected_live(&self) -> Option<&LiveSession> {
        let i = *self.live_rows.get(self.live_list.selected)?;
        self.live.get(i)
    }

    /// Recomputes the shown live rows, keeping the selected session selected when it is
    /// still shown.
    fn rebuild_live(&mut self, selected: Option<(String, LiveId)>) {
        self.live_rows = (0..self.live.len())
            .filter(|&i| self.live_show_inactive || !self.live[i].is_inactive())
            .collect();
        if let Some(selected) = selected
            && let Some(row) = self
                .live_rows
                .iter()
                .position(|&i| self.live[i].key() == selected)
        {
            self.live_list.selected = row;
        }
        self.clamp_lists();
    }

    /// The index entry selected in the history view.
    pub fn selected_entry(&self) -> Option<&Entry> {
        let path = self.history.rows.get(self.history.list.selected)?;
        self.index.entries.get(path)
    }

    /// The accounts of an indexed session: the home's for codex, the attributed ones for
    /// claude (R9, R17).
    pub fn entry_accounts(&self, entry: &Entry) -> Vec<&str> {
        let stores = self.stores.as_deref().unwrap_or_default();
        attribution::accounts_of(entry, stores, &self.attribution)
    }

    /// The indexed transcript of a session, if any.
    pub fn transcript_of(&self, session_id: &str) -> Option<&PathBuf> {
        self.by_session.get(session_id)
    }

    /// History entries after the noise filter (before any search).
    pub fn unfiltered_count(&self) -> usize {
        self.index
            .entries
            .values()
            .filter(|e| self.in_history(e))
            .count()
    }

    /// Shown in History: not noise, unless "show all" (R8).
    fn in_history(&self, entry: &Entry) -> bool {
        self.history.show_all || !is_noise(entry)
    }

    /// `r`: everything except live usage, skipping what is already running (an account still
    /// being asked is not asked twice).
    fn refresh(&mut self, fx: &mut Vec<Effect>) {
        // Choosing an account for `remuda run` needs identities, usage and checks only.
        let sessions = self.mode == Mode::Browse;
        if sessions {
            self.run(Work::Index, fx);
        }
        self.ask(Query::Identity, fx);
        self.ask(Query::CachedUsage, fx);
        if sessions {
            self.run(Work::Live, fx);
            self.run(Work::Attribution, fx);
        }
        self.run(Work::Checks, fx);
        if self.stats.requested {
            self.request_stats(fx);
        }
        // Transcripts may have grown: load the preview again.
        self.preview.loaded = None;
        self.preview.settled = 0;
        self.reload_config(fx);
    }

    /// Starts `work`, unless a round of it is out.
    fn run(&mut self, work: Work, fx: &mut Vec<Effect>) {
        if self.work.slot(work).start(()).is_some() {
            fx.push(work.effect());
        }
    }

    /// Runs `work` because what it reads has changed: now, or once more when the round that is
    /// out reports, so that it sees the change.
    fn rerun(&mut self, work: Work, fx: &mut Vec<Effect>) {
        if self.work.slot(work).restart(()).is_some() {
            fx.push(work.effect());
        }
    }

    /// A round of `work` has reported. `false` when the work was asked for again since that
    /// round began: what it found is from before, and the next round starts here.
    fn ran(&mut self, work: Work, fx: &mut Vec<Effect>) -> bool {
        match self.work.slot(work).settle(&()) {
            Claim::Again(_) => {
                fx.push(work.effect());
                false
            }
            // No other round can be out: a result is taken as it comes.
            Claim::Done | Claim::Stray => true,
        }
    }

    /// Asks `query` of every account that is not being asked it already (`r`, `u`).
    fn ask(&mut self, query: Query, fx: &mut Vec<Effect>) {
        self.ask_with(query, Slot::start, fx);
    }

    /// Asks `query` of every account because the account list has changed; an account that is
    /// being asked is asked once more when it answers.
    fn ask_again(&mut self, query: Query, fx: &mut Vec<Effect>) {
        self.ask_with(query, Slot::restart, fx);
    }

    fn ask_with(
        &mut self,
        query: Query,
        begin: fn(&mut Slot, ()) -> Option<Round>,
        fx: &mut Vec<Effect>,
    ) {
        let asked: Vec<Account> = self
            .accounts
            .iter_mut()
            .filter_map(|a| begin(a.work.slot(query), ()).map(|_| a.account.clone()))
            .collect();
        if !asked.is_empty() {
            fx.push(query.effect(asked));
        }
    }

    /// `account` has answered `query`: its row, wherever the account list has moved it, and
    /// `None` when it is no longer listed (the answer is dropped). An account asked again
    /// meanwhile is asked once more.
    fn answered(
        &mut self,
        query: Query,
        account: &Account,
        fx: &mut Vec<Effect>,
    ) -> Option<&mut AccountState> {
        let row = self.accounts.iter_mut().find(|a| a.account == *account)?;
        if let Claim::Again(_) = row.work.slot(query).settle(&()) {
            fx.push(query.effect(vec![account.clone()]));
        }
        Some(row)
    }

    /// Computes the statistics, unless a computation is running: they are never computed twice
    /// at once (a cold one reads every transcript whole, R20).
    fn request_stats(&mut self, fx: &mut Vec<Effect>) {
        self.stats.requested = true;
        self.run(Work::Stats, fx);
    }

    /// After a launch: transcripts, running sessions and the launch log have changed. Work
    /// that is already running is repeated when it finishes, so it sees the change.
    fn refresh_sessions(&mut self, fx: &mut Vec<Effect>) {
        for work in [Work::Index, Work::Live, Work::Attribution] {
            self.rerun(work, fx);
        }
        self.preview.loaded = None;
        self.preview.settled = 0;
    }

    /// Opens `overlay`. A launch waiting for its check is cancelled, as with Esc: its answer
    /// must not act on (or behind) the overlay (C1).
    fn open(&mut self, overlay: Overlay) {
        if let Some(request) = self.work.launch.cancel() {
            self.notify(Level::Info, format!("{} cancelled", request.what));
        }
        self.form_check = None;
        self.overlay = Some(overlay);
    }

    fn notify(&mut self, level: Level, text: impl Into<Marked>) {
        self.notice = Some(Notice {
            text: text.into(),
            level,
        });
    }

    fn on_launched(
        &mut self,
        request: LaunchRequest,
        result: Result<Exit, String>,
        warnings: Vec<String>,
        fx: &mut Vec<Effect>,
    ) {
        let failed = result_code(&result) != Some(0) || !warnings.is_empty();
        let agent = request.account.provider.program();
        let (mut level, what) = match result {
            Ok(Exit::Code(code)) => (Level::Info, format!("{agent} exited {code}")),
            Ok(Exit::Signal(signal)) => (
                Level::Warn,
                format!("{agent} was killed by signal {signal}"),
            ),
            Err(e) => (Level::Error, format!("cannot run {agent}: {e}")),
        };
        if level == Level::Info && failed {
            level = Level::Warn;
        }
        // What the agent or the system said, and each warning, are pieces of their own (R21).
        let mut text = Marked::from(format!("{}: ", request.what)).text(what);
        for w in warnings {
            text = text.text(" · ").text(w);
        }
        self.notify_after_child(level, text);
        self.work.live.invalidate();
        self.refresh_sessions(fx);
    }

    /// How a foreground child ended, and the pending launch it cancelled, if any.
    fn notify_after_child(&mut self, level: Level, mut text: Marked) {
        if let Some(what) = self.cancelled.take() {
            text = text.text(format!(" · {what} was cancelled: start it again"));
        }
        self.notify(level, text);
    }

    /// Starts a foreground child (R16): the terminal is its own until it ends, which can be
    /// any time later. A pending launch was checked before the child and is cancelled (its
    /// answer, still to come, no longer matches), and the live list (including a collection
    /// already running) no longer counts as current.
    fn foreground(&mut self, effect: Effect, fx: &mut Vec<Effect>) {
        if let Some(request) = self.work.launch.cancel() {
            self.cancelled = Some(request.what);
        }
        // A codex resume confirmed after the child would rest on a prompt from before it.
        if let Some(Overlay::ResumeCodex(confirm)) = &self.overlay {
            self.cancelled = Some(confirm.request.what.clone());
            self.overlay = None;
        }
        self.work.live.invalidate();
        fx.push(effect);
    }

    fn list_len(&self) -> usize {
        match self.view {
            View::Accounts => self.accounts.len(),
            View::Live => self.live_rows.len(),
            View::History => self.history.rows.len(),
            View::Stats => 0,
        }
    }

    fn list_mut(&mut self) -> &mut ListState {
        match self.view {
            View::Accounts => &mut self.accounts_list,
            View::Live => &mut self.live_list,
            View::History => &mut self.history.list,
            // Stats has no list: `navigate` scrolls it instead and never gets here.
            View::Stats => &mut self.accounts_list,
        }
    }

    /// Rows visible in the current view's list.
    fn list_height(&self) -> usize {
        render::list_height(self, self.view)
    }

    fn clamp_lists(&mut self) {
        self.config.scroll = self.config.scroll.min(self.config_max_scroll());
        let heights = View::ALL.map(|v| render::list_height(self, v));
        self.accounts_list.clamp(self.accounts.len(), heights[0]);
        self.live_list.clamp(self.live_rows.len(), heights[1]);
        self.history.list.clamp(self.history.rows.len(), heights[2]);
        self.stats.scroll = self.stats.scroll.min(self.stats_max_scroll());
    }

    fn stats_max_scroll(&self) -> usize {
        render::stats_line_count(self).saturating_sub(render::stats_height(self))
    }

    /// Movement keys in Stats scroll it.
    fn scroll_stats(&mut self, key: Key) -> bool {
        let page = render::stats_height(self).max(1);
        let max = self.stats_max_scroll();
        let scroll = self.stats.scroll.min(max);
        self.stats.scroll = match key {
            Key::Char('j') | Key::Down => scroll + 1,
            Key::Char('k') | Key::Up => scroll.saturating_sub(1),
            Key::Char('g') | Key::Home => 0,
            Key::Char('G') | Key::End => max,
            Key::PageDown => scroll + page,
            Key::PageUp => scroll.saturating_sub(page),
            _ => return false,
        }
        .min(max);
        true
    }

    fn navigate(&mut self, key: Key) -> bool {
        if self.view == View::Stats {
            return self.scroll_stats(key);
        }
        let len = self.list_len();
        let page = self.list_height().max(1);
        let list = self.list_mut();
        let last = len.saturating_sub(1);
        list.selected = match key {
            Key::Char('j') | Key::Down => (list.selected + 1).min(last),
            Key::Char('k') | Key::Up => list.selected.saturating_sub(1),
            Key::Char('g') | Key::Home => 0,
            Key::Char('G') | Key::End => last,
            Key::PageDown => (list.selected + page).min(last),
            Key::PageUp => list.selected.saturating_sub(page),
            _ => return false,
        };
        self.clamp_lists();
        true
    }

    fn scroll_preview(&mut self, key: Key) -> bool {
        let page = render::preview_height(self).max(1);
        let max = render::preview_line_count(self).saturating_sub(page);
        let scroll = &mut self.preview.scroll;
        *scroll = match key {
            Key::Char('k') | Key::Up => *scroll + 1,
            Key::Char('j') | Key::Down => scroll.saturating_sub(1),
            Key::PageUp => *scroll + page,
            Key::PageDown => scroll.saturating_sub(page),
            Key::Char('g') | Key::Home => max,
            Key::Char('G') | Key::End => 0,
            _ => return false,
        };
        *scroll = (*scroll).min(max);
        true
    }

    /// Points the Configuration pane at the selected account, reading its configuration when
    /// the account changed (R22).
    fn retarget_config(&mut self, fx: &mut Vec<Effect>) {
        if !self.config.open || self.mode != Mode::Browse {
            return;
        }
        let Some(account) = self
            .accounts
            .get(self.accounts_list.selected)
            .map(|a| a.account.clone())
        else {
            return;
        };
        if self.config.account.as_ref() == Some(&account) {
            return;
        }
        self.config.account = Some(account.clone());
        self.config.loaded = None;
        self.config.scroll = 0;
        if account.provider == CLAUDE {
            self.read_config(account, fx);
        }
    }

    /// `r`: the account shown is read again; what it showed stays until the answer.
    fn reload_config(&mut self, fx: &mut Vec<Effect>) {
        if !self.config.open {
            return;
        }
        if let Some(account) = self.config.account.clone()
            && account.provider == CLAUDE
        {
            self.read_config(account, fx);
        }
    }

    /// Reads `account`'s configuration (R22). While a read of it is out no second one starts,
    /// but that one may have read the files before this was asked: it is read once more when
    /// it answers.
    fn read_config(&mut self, account: Account, fx: &mut Vec<Effect>) {
        if let Some(request) = self.config.work.restart(account.clone()) {
            fx.push(Effect::Config {
                request,
                account,
                cwd: self.cwd.clone(),
            });
        }
    }

    /// `p` in Accounts: open, expanded, closed (R22).
    fn cycle_config(&mut self, fx: &mut Vec<Effect>) {
        if !self.config.open {
            self.config.open = true;
            self.retarget_config(fx);
        } else if !self.config.expanded {
            self.config.expanded = true;
            self.config.scroll = 0;
        } else {
            self.close_config();
        }
    }

    /// A read that is out stays out: its answer finds no account shown and is dropped, and a
    /// pane opened again for that account meanwhile reads once more when it answers, not twice
    /// at once.
    fn close_config(&mut self) {
        self.config = ConfigPane {
            work: std::mem::take(&mut self.config.work),
            ..ConfigPane::default()
        };
    }

    fn config_max_scroll(&self) -> usize {
        render::config_line_count(self).saturating_sub(render::config_height(self))
    }

    /// `PgUp` / `PgDn` scroll the pane; while it is expanded, the movement keys too.
    fn scroll_config(&mut self, key: Key) -> bool {
        let page = render::config_height(self).max(1);
        let max = self.config_max_scroll();
        let scroll = self.config.scroll.min(max);
        let expanded = self.config.expanded;
        self.config.scroll = match key {
            Key::PageDown => scroll + page,
            Key::PageUp => scroll.saturating_sub(page),
            Key::Char('j') | Key::Down if expanded => scroll + 1,
            Key::Char('k') | Key::Up if expanded => scroll.saturating_sub(1),
            Key::Char('g') | Key::Home if expanded => 0,
            Key::Char('G') | Key::End if expanded => max,
            _ => return false,
        }
        .min(max);
        true
    }

    fn switch(&mut self, view: View, fx: &mut Vec<Effect>) {
        self.view = view;
        self.preview.expanded = false;
        self.preview.scroll = 0;
        self.config.expanded = false;
        // Computed the first time the view opens, then on `r` (R20).
        if view == View::Stats && !self.stats.requested {
            self.request_stats(fx);
        }
    }

    fn on_key(&mut self, key: Key, fx: &mut Vec<Effect>) {
        // Anywhere, before anything else: it types nothing, closes nothing, and keeps the
        // notice (R21).
        if key == Key::Ctrl('p') {
            self.private = !self.private;
            return;
        }
        self.notice = None;
        if key == Key::Ctrl('c') {
            fx.push(Effect::Quit);
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        if self.overlay.is_some() {
            self.on_overlay_key(key, fx);
            return;
        }
        if self.mode == Mode::PickForRun {
            self.on_pick_mode_key(key, fx);
            return;
        }
        if self.view == View::History && self.history.searching {
            self.on_search_key(key);
            return;
        }
        match key {
            Key::Char('q') => fx.push(Effect::Quit),
            Key::Char('?') => self.help = true,
            Key::Char(c @ '1'..='4') => self.switch(View::ALL[c as usize - '1' as usize], fx),
            Key::Tab => {
                let next = (self.view.position() + 1) % View::ALL.len();
                self.switch(View::ALL[next], fx);
            }
            Key::BackTab => {
                let len = View::ALL.len();
                self.switch(View::ALL[(self.view.position() + len - 1) % len], fx);
            }
            Key::Char('r') => self.refresh(fx),
            Key::Char('u') => self.live_usage(fx),
            Key::Esc if self.work.launch.is_running() => {
                if let Some(request) = self.work.launch.cancel() {
                    self.notify(Level::Info, format!("{} cancelled", request.what));
                }
            }
            Key::Enter if self.view == View::History => self.act_on_entry(false, fx),
            Key::Char('f') if self.view == View::History => self.act_on_entry(true, fx),
            Key::Enter if self.view == View::Live => self.live_enter(fx),
            Key::Char('f') if self.view == View::Live => self.live_fork(fx),
            Key::Char('a') if self.view == View::Live => {
                self.live_show_inactive = !self.live_show_inactive;
                let selected = self.selected_live().map(LiveSession::key);
                self.rebuild_live(selected);
            }
            Key::Char('l') if self.view == View::Live => self.show_logs(fx),
            Key::Char('x') if self.view == View::Live => self.confirm(Control::Stop),
            Key::Char('D') if self.view == View::Live => self.confirm(Control::Remove),
            Key::Esc
                if self.view == View::Live && self.logs.is_some() && !self.preview.expanded =>
            {
                self.logs = None;
            }
            Key::Char('t') if self.view == View::Stats => {
                self.stats.period = self.stats.period.next();
                self.stats.scroll = 0;
            }
            Key::Char('p' | ' ') if matches!(self.view, View::Live | View::History) => {
                self.preview.expanded = !self.preview.expanded;
                self.preview.scroll = 0;
            }
            Key::Char('p' | ' ') if self.view == View::Accounts => self.cycle_config(fx),
            Key::Esc if self.view == View::Accounts && self.config.expanded => {
                self.config.expanded = false;
                self.config.scroll = 0;
            }
            Key::Esc if self.view == View::Accounts && self.config.open => self.close_config(),
            Key::PageUp | Key::PageDown
                if self.view == View::Accounts && self.config.open && !self.config.expanded =>
            {
                self.scroll_config(key);
            }
            // The whole view is the pane's: the other keys of Accounts wait.
            _ if self.view == View::Accounts && self.config.expanded => {
                self.scroll_config(key);
            }
            _ if self.preview.expanded => match key {
                Key::Esc => {
                    self.preview.expanded = false;
                    self.preview.scroll = 0;
                }
                other => {
                    self.scroll_preview(other);
                }
            },
            Key::Char('n') if self.view == View::Accounts => self.open_new_session(),
            Key::Char('D') if self.view == View::Accounts => self.confirm_remove(),
            Key::Char('s') if self.view == View::Accounts => {
                self.open(Overlay::Form(Form::new(
                    FormKind::Setup,
                    &[
                        ("Account name", String::new(), Mask::Text),
                        ("Email (optional)", String::new(), Mask::Email),
                        (
                            "Provider (claude/codex)",
                            CLAUDE.name().to_string(),
                            Mask::Plain,
                        ),
                    ],
                )));
            }
            Key::Char('/') if self.view == View::History => self.history.searching = true,
            Key::Char('a') if self.view == View::History => {
                self.history.show_all = !self.history.show_all;
                self.rebuild_history();
            }
            Key::Esc if self.view == View::History && !self.history.query.is_empty() => {
                self.history.query.clear();
                self.rebuild_history();
            }
            other => {
                self.navigate(other);
            }
        }
    }

    /// `D` in Accounts: asks before unregistering the selected account (R16); `default` is
    /// implicit (R14a).
    fn confirm_remove(&mut self) {
        let Some(account) = self
            .accounts
            .get(self.accounts_list.selected)
            .map(|a| a.account.clone())
        else {
            return;
        };
        if account.home == Home::Default {
            let text = format!(
                "{} is the native login: it is implicit and cannot be removed",
                display_name(&account)
            );
            self.notify(Level::Warn, text);
            return;
        }
        self.open(Overlay::RemoveAccount(account));
    }

    /// `u`: live usage for every account that is not already being queried.
    fn live_usage(&mut self, fx: &mut Vec<Effect>) {
        self.ask(Query::LiveUsage, fx);
    }

    fn on_pick_mode_key(&mut self, key: Key, fx: &mut Vec<Effect>) {
        match key {
            Key::Enter => {
                if let Some(a) = self.accounts.get(self.accounts_list.selected) {
                    fx.push(Effect::Pick(a.account.clone()));
                }
            }
            Key::Esc | Key::Char('q') => fx.push(Effect::Quit),
            Key::Char('?') => self.help = true,
            Key::Char('r') => self.refresh(fx),
            Key::Char('u') => self.live_usage(fx),
            other => {
                self.navigate(other);
            }
        }
    }

    fn on_overlay_key(&mut self, key: Key, fx: &mut Vec<Effect>) {
        let Some(overlay) = &mut self.overlay else {
            return;
        };
        match overlay {
            Overlay::Confirm(confirm) => {
                let confirm = confirm.clone();
                self.overlay = None;
                match (key, self.account_named(&confirm.account)) {
                    (Key::Char('y'), Some(account)) => fx.push(Effect::Control {
                        account,
                        verb: confirm.verb,
                        short_id: confirm.short_id,
                    }),
                    _ => self.notify(Level::Info, "cancelled"),
                }
            }
            Overlay::Form(form) => match key {
                Key::Esc => {
                    self.overlay = None;
                    self.work.launch.cancel();
                    self.form_check = None;
                }
                Key::Enter => self.submit(fx),
                Key::Tab | Key::Down => form.focus = (form.focus + 1) % form.fields.len(),
                Key::BackTab | Key::Up => {
                    form.focus = (form.focus + form.fields.len() - 1) % form.fields.len();
                }
                Key::Backspace => {
                    form.fields[form.focus].value.pop();
                    form.error = None;
                }
                Key::Char(c) => {
                    form.fields[form.focus].value.push(c);
                    form.error = None;
                }
                _ => {}
            },
            Overlay::ResumeCodex(confirm) => {
                let request = confirm.request.clone();
                self.overlay = None;
                match key {
                    Key::Char('y')
                        if !self.accounts.iter().any(|a| a.account == request.account) =>
                    {
                        let text =
                            format!("{} is no longer registered", display_name(&request.account));
                        self.notify(Level::Error, text);
                    }
                    Key::Char('y') => {
                        self.check_launch(request, fx);
                    }
                    _ => self.notify(Level::Info, "cancelled"),
                }
            }
            Overlay::RemoveAccount(account) => {
                let account = account.clone();
                self.overlay = None;
                match key {
                    Key::Char('y') if !self.accounts.iter().any(|a| a.account == account) => {
                        let text = format!("{} is no longer registered", display_name(&account));
                        self.notify(Level::Error, text);
                    }
                    Key::Char('y') => fx.push(Effect::RemoveAccount(account)),
                    _ => self.notify(Level::Info, "cancelled"),
                }
            }
            Overlay::Pick(pick) => match key {
                Key::Esc => self.overlay = None,
                Key::Char('j') | Key::Down => {
                    pick.selected = (pick.selected + 1).min(pick.options.len().saturating_sub(1));
                }
                Key::Char('k') | Key::Up => pick.selected = pick.selected.saturating_sub(1),
                Key::Enter => {
                    let (path, action) = (pick.path.clone(), pick.action);
                    let chosen = pick.options.get(pick.selected).cloned();
                    self.overlay = None;
                    if let Some(qualified) = chosen {
                        let fork = action == PickFor::Fork;
                        self.resume_picked(&path, fork, &qualified, fx);
                    }
                }
                _ => {}
            },
        }
    }

    /// `n`: a new session for the selected account, in remuda's directory by default; a name
    /// only where the agent takes one (claude, not codex).
    fn open_new_session(&mut self) {
        let Some(state) = self.accounts.get(self.accounts_list.selected) else {
            return;
        };
        let dir = self
            .cwd
            .as_ref()
            .map_or(String::new(), |d| d.display().to_string());
        let mut fields = vec![("Directory", dir, Mask::Path)];
        if state.account.provider.names_sessions() {
            fields.push(("Name (optional)", String::new(), Mask::Text));
        }
        let account = state.account.clone();
        self.open(Overlay::Form(Form::new(
            FormKind::NewSession { account },
            &fields,
        )));
    }

    fn submit(&mut self, fx: &mut Vec<Effect>) {
        let Some(Overlay::Form(form)) = &self.overlay else {
            return;
        };
        let outcome = match &form.kind {
            FormKind::NewSession { account } => match self.new_session_request(form, account) {
                // The form stays up until the check answers. Submitted again unchanged, the
                // check that is out is this form's already.
                Ok(request) => {
                    if let Some(check) = self.check_launch(request, fx) {
                        self.form_check = Some(check);
                    }
                    return;
                }
                Err(e) => Err(e),
            },
            FormKind::Setup => self.setup_effect(form),
        };
        match outcome {
            Ok(effect) => {
                // The name is told when the setup ends, registered or not (R21).
                if let Effect::Setup { provider, name, .. } = &effect {
                    self.aliases.note(&format!("{provider}:{name}"));
                }
                self.overlay = None;
                self.foreground(effect, fx);
            }
            Err(e) => {
                if let Some(Overlay::Form(form)) = &mut self.overlay {
                    form.error = Some(e.into());
                }
            }
        }
    }

    /// The launch for a filled-in new-session form: its account as registered now (by name),
    /// `~` expanded, a relative directory taken from remuda's own, the name passed as `-n` (R6).
    fn new_session_request(&self, form: &Form, account: &Account) -> Result<LaunchRequest, String> {
        let account = self
            .account_named(&account.qualified())
            .ok_or_else(|| format!("{} is no longer registered", display_name(account)))?;
        let raw = form.value(0);
        if raw.is_empty() {
            return Err("enter a directory".to_string());
        }
        let expanded = match raw.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                let home = self
                    .home
                    .as_deref()
                    .ok_or("cannot expand `~`: HOME is not set")?;
                format!("{}{rest}", home.strip_suffix('/').unwrap_or(home))
            }
            _ => raw.to_string(),
        };
        let dir = PathBuf::from(&expanded);
        let dir = if dir.is_absolute() {
            dir
        } else {
            self.cwd
                .as_ref()
                .ok_or("not an absolute directory")?
                .join(dir)
        };
        if !account.provider.names_sessions() {
            return Ok(LaunchRequest {
                what: format!("new session as {}", display_name(&account)),
                args: account.provider.new_session_args(&dir, None),
                account,
                cwd: Some(dir),
            });
        }
        let name = form.value(1);
        if name.starts_with('-') {
            return Err(format!(
                "a session name cannot start with `-` (claude would read {name:?} as an \
                 option); choose another name"
            ));
        }
        let args: Vec<String> = if name.is_empty() {
            Vec::new()
        } else {
            vec!["-n".to_string(), name.to_string()]
        };
        if launch::classify(&args) != Intent::NewSession {
            return Err(format!(
                "with the name {name:?} remuda cannot tell this is a new session (it looks \
                 like a claude subcommand or option); choose another name"
            ));
        }
        let label = if name.is_empty() {
            String::new()
        } else {
            format!(" “{name}”")
        };
        Ok(LaunchRequest {
            what: format!("new session{label} as {}", display_name(&account)),
            account,
            args,
            cwd: Some(dir),
        })
    }

    /// The same checks `remuda setup` makes before touching anything that are possible
    /// without the file system (the rest happen when it runs).
    fn setup_effect(&self, form: &Form) -> Result<Effect, String> {
        let name = form.value(0);
        registry::check_new_name(name).map_err(|e| format!("{e:#}"))?;
        let provider = Provider::parse(form.value(2)).ok_or_else(|| {
            format!(
                "unknown provider {:?} (known: {})",
                form.value(2),
                Provider::ALL.map(Provider::name).join(", ")
            )
        })?;
        if self
            .accounts
            .iter()
            .any(|a| a.account.provider == provider && a.account.name == name)
        {
            return Err(format!("{provider}:{name} is already registered"));
        }
        let email = form.value(1);
        let email = (!email.is_empty()).then(|| email.to_string());
        provider.login_args(email.clone())?;
        Ok(Effect::Setup {
            provider,
            name: name.to_string(),
            email,
        })
    }

    /// The registry changed: account rows are rebuilt (known accounts keep their state) and
    /// everything per account is read again. The same accounts again change nothing.
    fn set_accounts(&mut self, accounts: Vec<Account>, fx: &mut Vec<Effect>) {
        if self.accounts.iter().map(|a| &a.account).eq(accounts.iter()) {
            return;
        }
        for account in &accounts {
            self.aliases.note(&account.qualified());
        }
        let old = std::mem::take(&mut self.accounts);
        self.accounts = accounts
            .into_iter()
            .map(|account| {
                old.iter()
                    .find(|a| a.account == account)
                    .cloned()
                    .unwrap_or_else(|| AccountState::new(account))
            })
            .collect();
        self.ask_again(Query::Identity, fx);
        self.ask_again(Query::CachedUsage, fx);
        self.rerun(Work::Checks, fx);
        self.refresh_sessions(fx);
        // Read again at the end of the update: a setup or a removal may have changed it, and
        // a read that is out is from before.
        self.config.account = None;
        self.config.work.cancel();
        self.clamp_lists();
    }

    fn on_setup_done(
        &mut self,
        provider: Provider,
        name: &str,
        result: Result<Exit, String>,
        links: Vec<setup::Note>,
    ) {
        let login = setup::login_command(provider);
        let (mut level, text) = match result {
            Ok(Exit::Code(0)) => (Level::Info, format!("{login} exited 0")),
            Ok(exit) => {
                let how = match exit {
                    Exit::Code(code) => format!("exited {code}"),
                    Exit::Signal(signal) => format!("was killed by signal {signal}"),
                };
                // The registry as read after the setup (it arrives first).
                let ambiguous =
                    setup::ambiguous(provider, name, self.accounts.iter().map(|a| &a.account));
                (
                    Level::Warn,
                    format!(
                        "{login} {how}; {} stays registered, retry with: {}",
                        setup::reference(provider, name, ambiguous),
                        setup::retry_command(provider, name, ambiguous)
                    ),
                )
            }
            Err(e) => (Level::Error, e),
        };
        if level == Level::Info && links.iter().any(|note| note.warning) {
            level = Level::Warn;
        }
        let mut text = Marked::from(format!("set up {name}: ")).text(text);
        for note in links {
            text = text.text(" · ").text(note.text);
        }
        self.notify_after_child(level, text);
    }

    /// `l`: `claude logs` of the selected background session into the preview area.
    fn show_logs(&mut self, fx: &mut Vec<Effect>) {
        let Some(session) = self.selected_live().cloned() else {
            return;
        };
        let (true, Some(short)) = (session.is_background(), session.short_id.clone()) else {
            self.notify(
                Level::Warn,
                "logs are for background sessions; interactive ones are in their terminal",
            );
            return;
        };
        if !self.check_short_id(&short) {
            return;
        }
        let Some(account) = self.account_named(&session.account) else {
            return;
        };
        self.logs = Some(Logs {
            key: session.key(),
            short_id: short.clone(),
            result: None,
        });
        self.preview.scroll = 0;
        // While `claude logs` is out for this very session no second one starts; it runs once
        // more when that one answers, which may have read the log before this was asked.
        if self
            .work
            .logs
            .restart((account.clone(), short.clone()))
            .is_some()
        {
            fx.push(Effect::Logs {
                account,
                short_id: short,
            });
        }
    }

    /// `x` / `D`: asks before `claude stop|rm` (only background sessions; `rm` only once
    /// stopped).
    fn confirm(&mut self, verb: Control) {
        let Some(session) = self.selected_live().cloned() else {
            return;
        };
        let (true, Some(short)) = (session.is_background(), session.short_id.clone()) else {
            self.notify(
                Level::Warn,
                "interactive sessions can only be viewed here: use their terminal",
            );
            return;
        };
        if !self.check_short_id(&short) {
            return;
        }
        match (verb, session.is_inactive()) {
            (Control::Stop, true) => {
                let text = format!("background session {short} is already stopped");
                self.notify(Level::Warn, text);
            }
            (Control::Remove, false) => {
                let text =
                    format!("background session {short} is still running: stop it first (x)");
                self.notify(Level::Warn, text);
            }
            _ => {
                self.open(Overlay::Confirm(Confirm {
                    verb,
                    account: session.account.clone(),
                    short_id: short,
                }));
            }
        }
    }

    /// Enter (resume) or `f` (fork) on the selected history row: that transcript, not
    /// whichever one has its session id (one id can be in two stores, R16).
    fn act_on_entry(&mut self, fork: bool, fx: &mut Vec<Effect>) {
        if let Some(path) = self.selected_entry().map(|e| e.path.clone()) {
            self.resume(&path, fork, fx);
        }
    }

    /// Enter in Live: background sessions are attached; interactive ones only looked at.
    fn live_enter(&mut self, fx: &mut Vec<Effect>) {
        let Some(session) = self.selected_live().cloned() else {
            return;
        };
        if session.is_background() {
            self.attach(&session, fx);
        } else {
            self.refuse_running(&session);
        }
    }

    fn live_fork(&mut self, fx: &mut Vec<Effect>) {
        let Some(session) = self.selected_live().cloned() else {
            return;
        };
        let Some(id) = session.session_id.clone() else {
            self.notify(Level::Warn, "this session's id is unknown");
            return;
        };
        match self.transcript_for_live(&session) {
            Some(path) => self.resume(&path, true, fx),
            None => {
                let text = format!("session {} is not indexed yet", short_id(&id));
                self.notify(Level::Warn, text);
            }
        }
    }

    /// The transcript of a live session: the copy in its account's store when its id is in
    /// several stores.
    fn transcript_for_live(&self, session: &LiveSession) -> Option<PathBuf> {
        let id = session.session_id.as_deref()?;
        let store = self
            .stores
            .iter()
            .flatten()
            .find(|s| s.accounts.contains(&session.account))
            .map(|s| &s.path);
        self.index
            .entries
            .values()
            .find(|e| e.session_id == id && Some(&e.store) == store)
            .map(|e| e.path.clone())
            .or_else(|| self.transcript_of(id).cloned())
    }

    /// Resumes (or forks) the transcript at `path` (R16): not while it runs interactively; a
    /// running background session is attached instead; the account is the one the session
    /// belongs to, else the user picks.
    fn resume(&mut self, path: &Path, fork: bool, fx: &mut Vec<Effect>) {
        let Some(entry) = self.index.entries.get(path) else {
            return;
        };
        if entry.provider == CODEX {
            return self.resume_codex(path, fork, fx);
        }
        let session_id = entry.session_id.clone();
        if !self.check_session_id(&session_id, CLAUDE) {
            return;
        }
        if !fork {
            match self.running_check(&session_id) {
                Ok(()) => {}
                Err(Busy::Running(running)) if running.is_background() => {
                    self.attach(&running, fx);
                    return;
                }
                Err(busy) => {
                    self.notify(Level::Warn, busy.text());
                    return;
                }
            }
        }
        if self.stores.is_none() {
            self.notify(
                Level::Warn,
                "still reading the transcript stores; try again in a moment",
            );
            return;
        }
        let Some(entry) = self.index.entries.get(path) else {
            return;
        };
        if entry.cwd_last.is_none() {
            let text = format!(
                "session {} has no recorded directory to resume in",
                short_id(&session_id)
            );
            self.notify(Level::Error, text);
            return;
        }
        // Claude accounts only: no other agent can resume a claude transcript.
        let claude: Vec<String> = self
            .accounts
            .iter()
            .filter(|a| a.account.provider == CLAUDE)
            .map(|a| a.account.qualified())
            .collect();
        let owners = self.attribution.accounts(&session_id);
        let attributed: Vec<String> = claude
            .iter()
            .filter(|q| owners.contains(&q.as_str()))
            .cloned()
            .collect();
        let sees = |q: &String| self.store_problem(q, path).is_none();
        if let [only] = attributed.as_slice()
            && (sees(only) || !claude.iter().any(sees))
            && let Some(account) = self.account_named(only)
        {
            // Its one account, unless that one cannot see this copy and another can; when
            // none can, `resume_as` says why.
            self.resume_as(path, fork, account, fx);
            return;
        }
        // The accounts that can see this copy first, then the rest; the session's own
        // accounts first within each (C4).
        let mut options = claude.clone();
        options.sort_by_key(|q| (!sees(q), !attributed.contains(q)));
        self.open(Overlay::Pick(Pick {
            path: path.to_path_buf(),
            session_id,
            action: if fork { PickFor::Fork } else { PickFor::Resume },
            options,
            attributed,
            selected: 0,
        }));
    }

    /// Resumes (or forks) the codex rollout at `path` (R17), with `-C` its last directory, as the
    /// account whose home holds it. When the homes of several accounts share the store (their
    /// `sessions` resolve to one directory), the rollout does not say which one it is: the user
    /// picks among those, never among others (no other home has the rollout).
    fn resume_codex(&mut self, path: &Path, fork: bool, fx: &mut Vec<Effect>) {
        let Some(entry) = self.index.entries.get(path) else {
            return;
        };
        let (session_id, store, has_cwd) = (
            entry.session_id.clone(),
            entry.store.clone(),
            entry.cwd_last.is_some(),
        );
        if !self.check_session_id(&session_id, CODEX) {
            return;
        }
        let short = short_id(&session_id);
        if !has_cwd {
            let text = format!("session {short} has no recorded directory to resume in");
            self.notify(Level::Error, text);
            return;
        }
        let Some(stores) = &self.stores else {
            self.notify(
                Level::Warn,
                "still reading the transcript stores; try again in a moment",
            );
            return;
        };
        let owners: Vec<Account> = stores
            .iter()
            .filter(|s| s.provider == CODEX && s.path == store)
            .flat_map(|s| &s.accounts)
            .filter_map(|qualified| self.account_named(qualified))
            .collect();
        match owners.as_slice() {
            [] => {
                let text = Marked::from(format!("session {short} is in "))
                    .path(store.display())
                    .text(", which no registered account has");
                self.notify(Level::Error, text);
            }
            [only] => {
                let only = only.clone();
                self.resume_codex_as(path, fork, only, fx);
            }
            several => {
                let options: Vec<String> = several.iter().map(Account::qualified).collect();
                self.open(Overlay::Pick(Pick {
                    path: path.to_path_buf(),
                    session_id,
                    action: if fork { PickFor::Fork } else { PickFor::Resume },
                    attributed: options.clone(),
                    options,
                    selected: 0,
                }));
            }
        }
    }

    /// Resumes (or forks) the codex rollout at `path` as `account`, which must hold it. Nothing
    /// lists running codex sessions, so a resume in place asks first; a fork only reads the
    /// rollout.
    fn resume_codex_as(&mut self, path: &Path, fork: bool, account: Account, fx: &mut Vec<Effect>) {
        let Some(entry) = self.index.entries.get(path) else {
            return;
        };
        let (session_id, store, cwd, mtime_ns) = (
            entry.session_id.clone(),
            entry.store.clone(),
            entry.cwd_last.clone(),
            entry.mtime_ns,
        );
        if !self.check_session_id(&session_id, CODEX) {
            return;
        }
        let short = short_id(&session_id);
        let name = display_name(&account);
        let Some(cwd) = cwd.map(PathBuf::from) else {
            let text = format!("session {short} has no recorded directory to resume in");
            self.notify(Level::Error, text);
            return;
        };
        if account.provider != CODEX {
            let text =
                format!("{name} is not a codex account: it cannot resume codex session {short}");
            self.notify(Level::Error, text);
            return;
        }
        // The stores may have been read again while the picker was open.
        if self.store_problem(&account.qualified(), path).is_some() {
            let text = Marked::from(format!(
                "{name} cannot find session {short}: the rollout is in "
            ))
            .path(store.display())
            .text(format!(", which is not {name}'s sessions store"));
            self.notify(Level::Error, text);
            return;
        }
        let verb = if fork { "fork" } else { "resume" };
        let request = LaunchRequest {
            what: format!("{verb} {short} as {name}"),
            args: CODEX.resume_args(&session_id, &cwd, fork),
            account,
            cwd: Some(cwd),
        };
        if fork {
            self.check_launch(request, fx);
            return;
        }
        // The prompt says whether the rollout was written just now: the index's mtime until the
        // file's own arrives.
        fx.push(Effect::RolloutWritten(path.to_path_buf()));
        self.open(Overlay::ResumeCodex(ResumeCodex {
            path: path.to_path_buf(),
            written: Timestamp::from_nanosecond(mtime_ns).ok(),
            request,
        }));
    }

    /// The account picker's choice: resolved by name (the registry may have changed while it
    /// was open), then resumed as that account the way its provider does.
    fn resume_picked(&mut self, path: &Path, fork: bool, qualified: &str, fx: &mut Vec<Effect>) {
        let Some(account) = self.account_named(qualified) else {
            let text = format!("{} is no longer registered", short_account(qualified));
            self.notify(Level::Error, text);
            return;
        };
        match self.index.entries.get(path).map(|e| e.provider) {
            Some(CODEX) => self.resume_codex_as(path, fork, account, fx),
            Some(_) => self.resume_as(path, fork, account, fx),
            None => {}
        }
    }

    /// Resumes (or forks) the claude transcript at `path` as `account`: only a claude account,
    /// only if its `projects` store is the transcript's (else claude would not find it), in its
    /// `cwd_last`. Whether it runs is checked again: time may have passed in the picker.
    fn resume_as(&mut self, path: &Path, fork: bool, account: Account, fx: &mut Vec<Effect>) {
        let name = display_name(&account);
        let Some(entry) = self.index.entries.get(path) else {
            return;
        };
        let (session_id, entry_store, cwd) = (
            entry.session_id.clone(),
            entry.store.clone(),
            entry.cwd_last.clone(),
        );
        if !self.check_session_id(&session_id, CLAUDE) {
            return;
        }
        let short = short_id(&session_id);
        if account.provider != CLAUDE {
            let text =
                format!("{name} is not a claude account: it cannot resume claude session {short}");
            self.notify(Level::Error, text);
            return;
        }
        if !fork && let Err(busy) = self.running_check(&session_id) {
            self.notify(Level::Warn, busy.text());
            return;
        }
        let qualified = account.qualified();
        let store = self
            .stores
            .iter()
            .flatten()
            .find(|s| s.accounts.contains(&qualified));
        match store {
            None => {
                let text = format!("{name} cannot find session {short}: it has no projects store");
                self.notify(Level::Error, text);
                return;
            }
            Some(store) if store.path != entry_store => {
                // Accounts that link `projects` to one store can each resume its sessions (R18).
                let own = match &account.home {
                    Home::Path(home) => format!("{home}/projects"),
                    Home::Default => "~/.claude/projects".to_string(),
                };
                let text = Marked::from(format!(
                    "{name} cannot find session {short}: its projects store is "
                ))
                .path(store.path.display())
                .text(", the transcript is in ")
                .path(entry_store.display())
                .text("; link ")
                .path(own)
                .text(" to that store to share sessions");
                self.notify(Level::Error, text);
                return;
            }
            Some(_) => {}
        }
        let mut args = vec!["--resume".to_string(), session_id];
        if fork {
            args.push("--fork-session".to_string());
        }
        let verb = if fork { "fork" } else { "resume" };
        let request = LaunchRequest {
            account,
            args,
            cwd: cwd.map(PathBuf::from),
            what: format!("{verb} {short} as {name}"),
        };
        self.check_launch(request, fx);
    }

    /// Makes `request` the pending launch and starts its check, whose number is returned.
    /// `None` when that very launch is pending already: the check that is out is the one it
    /// waits for, and none is started (a held `Enter` checks once).
    fn check_launch(&mut self, request: LaunchRequest, fx: &mut Vec<Effect>) -> Option<Round> {
        let check = self.work.launch.start(request.clone())?;
        fx.push(Effect::CheckLaunch { check, request });
        Some(check)
    }

    /// Whether `session_id` may be resumed in place as far as the live list knows (R16).
    /// Not before that list is known, nor while it predates the last launch. The pre-launch
    /// check ([`Effect::CheckLaunch`]) asks every account again.
    fn running_check(&self, session_id: &str) -> Result<(), Busy> {
        if !self.live_loaded || self.work.live.stale() {
            return Err(Busy::Loading);
        }
        match self
            .live
            .iter()
            .find(|s| s.session_id.as_deref() == Some(session_id) && !s.is_inactive())
        {
            Some(running) => Err(Busy::Running(Box::new(running.clone()))),
            None => Ok(()),
        }
    }

    /// Session ids are passed to claude only when they look like one (R16).
    fn check_session_id(&mut self, id: &str, provider: Provider) -> bool {
        let ok = launch::is_session_id(id);
        if !ok {
            let agent = provider.program();
            let text =
                format!("session id {id:?} is not a UUID; remuda does not pass it to {agent}");
            self.notify(Level::Error, text);
        }
        ok
    }

    /// Background short ids likewise (R16).
    fn check_short_id(&mut self, id: &str) -> bool {
        let ok = live::is_short_id(id);
        if !ok {
            let text = format!(
                "background session id {id:?} is not 8 hex digits; \
                 remuda does not pass it to claude"
            );
            self.notify(Level::Error, text);
        }
        ok
    }

    /// `claude attach <id>` under the session's account, in the foreground.
    fn attach(&mut self, session: &LiveSession, fx: &mut Vec<Effect>) {
        let Some(short) = session.short_id.clone() else {
            self.notify(
                Level::Error,
                "this background session has no id to attach to",
            );
            return;
        };
        if !self.check_short_id(&short) {
            return;
        }
        let Some(account) = self.account_named(&session.account) else {
            let text = format!("{} is not a registered account", session.account);
            self.notify(Level::Error, text);
            return;
        };
        let what = format!("attach {short} as {}", display_name(&account));
        let request = LaunchRequest {
            account,
            args: vec!["attach".to_string(), short],
            cwd: None,
            what,
        };
        self.foreground(Effect::Launch(request), fx);
    }

    /// Two claudes writing one session overwrite each other (R16).
    fn refuse_running(&mut self, session: &LiveSession) {
        self.notify(Level::Warn, running_text(session));
    }

    /// Why the account `qualified` (`provider:name`) cannot see the transcript at `path`, if
    /// it cannot.
    pub fn store_problem(&self, qualified: &str, path: &Path) -> Option<&'static str> {
        let entry = self.index.entries.get(path)?;
        let store = self
            .stores
            .iter()
            .flatten()
            .find(|s| s.accounts.iter().any(|a| a == qualified));
        let codex = entry.provider == CODEX;
        match store {
            None if codex => Some("no sessions store"),
            None => Some("no projects store"),
            Some(store) if store.path == entry.store => None,
            Some(_) if codex => Some("other sessions store"),
            Some(_) => Some("other projects store"),
        }
    }

    fn account_named(&self, qualified: &str) -> Option<Account> {
        self.accounts
            .iter()
            .map(|a| &a.account)
            .find(|a| a.qualified() == qualified)
            .cloned()
    }

    fn on_search_key(&mut self, key: Key) {
        match key {
            Key::Esc => {
                self.history.searching = false;
                self.history.query.clear();
                self.rebuild_history();
            }
            Key::Enter => self.history.searching = false,
            Key::Backspace => {
                self.history.query.pop();
                self.history.list = ListState::default();
                self.rebuild_history();
            }
            Key::Char(c) => {
                self.history.query.push(c);
                self.history.list = ListState::default();
                self.rebuild_history();
            }
            Key::Up | Key::Down | Key::PageUp | Key::PageDown | Key::Home | Key::End => {
                self.navigate(key);
            }
            _ => {}
        }
    }

    fn on_tick(&mut self, now: Timestamp, fx: &mut Vec<Effect>) {
        self.now = now;
        if self
            .live_updated
            .is_some_and(|t| now.duration_since(t) >= LIVE_EVERY)
        {
            self.run(Work::Live, fx);
        }
        let Some(target) = self.preview.target.clone() else {
            return;
        };
        let loaded = self
            .preview
            .loaded
            .as_ref()
            .is_some_and(|(p, _)| *p == target);
        if loaded || self.preview.work.running() == Some(&target) {
            return;
        }
        self.preview.settled = self.preview.settled.saturating_add(1);
        if self.preview.settled >= PREVIEW_DEBOUNCE_TICKS
            && self.preview.work.start(target.clone()).is_some()
        {
            let provider = self
                .index
                .entries
                .get(&target)
                .map_or(CLAUDE, |e| e.provider);
            fx.push(Effect::Preview(target, provider));
        }
    }

    /// Merges newly read entries into the index.
    fn add_entries(&mut self, entries: Vec<Entry>) {
        for entry in entries {
            self.index.entries.insert(entry.path.clone(), entry);
        }
        self.reindex_sessions();
        self.rebuild_history();
    }

    fn reindex_sessions(&mut self) {
        self.by_session = self
            .index
            .entries
            .values()
            .map(|e| (e.session_id.clone(), e.path.clone()))
            .collect();
    }

    /// Recomputes the history rows (noise filter, search, order), keeping the selected
    /// session selected when it is still shown.
    fn rebuild_history(&mut self) {
        let selected = self.history.rows.get(self.history.list.selected).cloned();
        let visible: Vec<&Entry> = self
            .index
            .sorted()
            .into_iter()
            .filter(|e| self.in_history(e))
            .collect();
        let rows: Vec<PathBuf> = if self.history.query.trim().is_empty() {
            visible.iter().map(|e| e.path.clone()).collect()
        } else {
            search::rank(&self.history.query, &visible, |e| {
                self.entry_accounts(e).join(" ")
            })
            .into_iter()
            .map(|e| e.path.clone())
            .collect()
        };
        self.history.rows = rows;
        if let Some(selected) = selected
            && let Some(i) = self.history.rows.iter().position(|p| *p == selected)
        {
            self.history.list.selected = i;
        }
        self.clamp_lists();
    }

    fn merge_attribution(&mut self) {
        let mut merged = self.attribution_base.clone();
        merged.add_live(&self.live);
        self.attribution = merged;
    }

    /// Points the preview at the current selection.
    fn retarget_preview(&mut self) {
        if self.logs.as_ref().map(|l| &l.key) != self.selected_live().map(LiveSession::key).as_ref()
            || self.view != View::Live
        {
            self.logs = None;
        }
        let target = match self.view {
            View::History => self.selected_entry().map(|e| e.path.clone()),
            View::Live => self
                .selected_live()
                .and_then(|s| self.transcript_for_live(s)),
            View::Accounts | View::Stats => return,
        };
        if target != self.preview.target {
            self.preview.target = target;
            self.preview.settled = 0;
            self.preview.scroll = 0;
        }
    }
}

/// Why a session cannot be resumed in place right now.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Busy {
    /// The live list is not known yet, or predates the last launch.
    Loading,
    Running(Box<LiveSession>),
}

impl Busy {
    fn text(&self) -> String {
        match self {
            Busy::Loading => "still reading the running sessions; try again in a moment".into(),
            Busy::Running(session) => running_text(session),
        }
    }
}

/// Why a running session is not resumed (R16): where it runs, and how to get to it.
pub fn running_text(session: &LiveSession) -> String {
    let which = session
        .session_id
        .as_deref()
        .map_or("this session".to_string(), |id| {
            format!("session {}", short_id(id))
        });
    let account = short_account(&session.account);
    const WHY: &str = "(two claudes writing one session overwrite each other)";
    match (&session.short_id, session.is_background()) {
        (Some(short), true) => format!(
            "{which} is running as background session {short} in {account}: \
             attach to it from Live {WHY}"
        ),
        _ => {
            let pid = session.pid.map_or(String::new(), |p| format!(" (pid {p})"));
            format!("{which} is running in {account}{pid}: switch to its terminal {WHY}")
        }
    }
}

/// The first 8 characters of a session id, as claude shows it.
pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// `provider:name` without the `claude:` prefix.
pub fn short_account(qualified: &str) -> &str {
    qualified.strip_prefix("claude:").unwrap_or(qualified)
}

fn display_name(account: &Account) -> String {
    short_account(&account.qualified()).to_string()
}

fn result_code(result: &Result<Exit, String>) -> Option<i32> {
    match result {
        Ok(Exit::Code(code)) => Some(*code),
        _ => None,
    }
}

/// Hidden unless "show all" (R8, R17 noise filter): agent-team teammates and SDK sessions of
/// claude; codex sessions not started from its CLI or VS Code (subagents, `codex exec`, …).
/// A 2025 rollout has no source and is shown.
pub fn is_noise(entry: &Entry) -> bool {
    match entry.provider {
        Provider::Claude => {
            entry
                .first_user_text
                .as_deref()
                .is_some_and(|t| t.starts_with("<teammate-message"))
                || entry
                    .entrypoint
                    .as_deref()
                    .is_some_and(|e| e.starts_with("sdk"))
        }
        Provider::Codex => entry
            .source
            .as_deref()
            .is_some_and(|s| !matches!(s, "cli" | "vscode")),
    }
}

/// Applies one event; returns the work to start.
pub fn update(app: &mut App, event: Event) -> Vec<Effect> {
    let mut fx = Vec::new();
    match event {
        Event::Key(key) => app.on_key(key, &mut fx),
        Event::Tick(now) => app.on_tick(now, &mut fx),
        Event::Resize(w, h) => {
            app.size = (w, h);
            app.clamp_lists();
        }
        Event::IndexLoaded(entries) => {
            app.index.entries = entries.into_iter().map(|e| (e.path.clone(), e)).collect();
            app.index_loaded = true;
            app.reindex_sessions();
            app.rebuild_history();
        }
        Event::IndexProgress {
            done,
            total,
            entries,
        } => {
            app.indexing = Some((done, total));
            app.add_entries(entries);
        }
        Event::IndexDone { entries, error } => {
            app.index.entries = entries.into_iter().map(|e| (e.path.clone(), e)).collect();
            app.index_loaded = true;
            app.indexing = None;
            app.index_refreshed = Some(app.now);
            app.index_error = error;
            app.reindex_sessions();
            app.rebuild_history();
            app.ran(Work::Index, &mut fx);
        }
        // A result for an account that is no longer listed is dropped.
        Event::Identity { account, identity } => {
            if let Some(a) = app.answered(Query::Identity, &account, &mut fx) {
                a.identity = Some(identity);
            }
        }
        Event::CachedUsage { account, result } => {
            if let Some(a) = app.answered(Query::CachedUsage, &account, &mut fx) {
                a.cached = Some(result);
            }
        }
        Event::LiveUsage { account, result } => {
            let now = app.now;
            if let Some(a) = app.answered(Query::LiveUsage, &account, &mut fx) {
                a.live = Some(match result {
                    Ok(LiveResult { usage, identity }) => {
                        // Codex's live query also tells the email and plan (R10); a later
                        // identity refresh (`codex login status`) shows the login method again.
                        if let Some(identity) = identity {
                            a.identity = Some(identity);
                        }
                        match usage {
                            LiveUsage::Rows(rows) => Ok((rows, now)),
                            LiveUsage::Unrecognized(_) => Err("output not recognized".to_string()),
                        }
                    }
                    Err(e) => Err(e),
                });
            }
        }
        Event::Live(sessions) => {
            for s in &sessions {
                app.aliases.note(&s.account);
            }
            let selected = app.selected_live().map(LiveSession::key);
            app.live = sessions;
            app.live_loaded = true;
            app.live_updated = Some(app.now);
            app.rebuild_live(selected);
            app.merge_attribution();
            // Started before something changed (e.g. a launch ended): collected again, and
            // stale until then if a foreground child has run.
            app.ran(Work::Live, &mut fx);
        }
        Event::Attribution(base) => {
            // Names first seen here are numbered by name (R21).
            let names: std::collections::BTreeSet<&str> = base.names().collect();
            for name in names {
                app.aliases.note(name);
            }
            app.attribution_base = base;
            app.merge_attribution();
            // Accounts are searched.
            if !app.history.query.is_empty() {
                app.rebuild_history();
            }
            app.ran(Work::Attribution, &mut fx);
        }
        Event::Checks(checks) => {
            // Asked again since they started (the account list changed): these are the checks
            // of the list as it was, and those that start now are the ones shown.
            if app.ran(Work::Checks, &mut fx) {
                // A check may name an account that is not registered (`[share.claude] from`).
                for name in checks.iter().filter_map(|c| c.account.as_deref()) {
                    app.aliases.note(name);
                }
                app.checks = Some(checks);
            }
        }
        // Any reading of the transcript wanted is shown, whichever round it is.
        Event::Preview { path, result } => {
            app.preview.work.settle(&path);
            if app.preview.target.as_ref() == Some(&path) {
                app.preview.loaded = Some((path, result));
            }
        }
        Event::Config {
            request,
            account,
            result,
        } => {
            // The read that is out, by its number, for the account still shown.
            let claim = app.config.work.claim(request, &account);
            let shown = app.config.account.as_ref() == Some(&account);
            // Asked again since this read began (`r`, the pane opened again): what it read
            // may be from before, so the account is read once more, if it is still shown.
            if let Claim::Again(request) = claim {
                if shown {
                    fx.push(Effect::Config {
                        request,
                        account: account.clone(),
                        cwd: app.cwd.clone(),
                    });
                } else {
                    app.config.work.cancel();
                }
            }
            if claim != Claim::Stray && shown {
                // The source may be a name not seen yet (R21).
                if let Ok(view) = &result
                    && let Some(source) = view.role.source()
                {
                    app.aliases.note(source);
                }
                app.config.loaded = Some(result.map(|view| *view));
                app.clamp_lists();
            }
        }
        Event::Stores(stores) => {
            for name in stores.iter().flat_map(|s| &s.accounts) {
                app.aliases.note(name);
            }
            app.stores = Some(stores);
            // Codex rows' accounts come from the stores, and are searched.
            if !app.history.query.is_empty() {
                app.rebuild_history();
            }
        }
        Event::LaunchChecked {
            check,
            request,
            error,
        } => {
            // Only the check that is out, by its number and for this very launch: one that was
            // cancelled, or an earlier one of the same launch, starts nothing (R16).
            if app.work.launch.claim(check, &request) == Claim::Done {
                // What the app has seen meanwhile counts too (R16).
                let error = error.or_else(|| {
                    let id = request.resumes()?;
                    app.running_check(&id).err().map(|busy| busy.text().into())
                });
                // Only the form that started this check hears about it (C1).
                let form = match &mut app.overlay {
                    Some(Overlay::Form(form)) if app.form_check == Some(check) => Some(form),
                    _ => None,
                };
                match (error, form) {
                    (Some(e), Some(form)) => form.error = Some(e),
                    (Some(e), None) => {
                        let text = Marked::from(format!("{}: ", request.what)).join(&e);
                        app.notify(Level::Error, text);
                    }
                    (None, form) => {
                        if form.is_some() {
                            app.overlay = None;
                            app.form_check = None;
                        }
                        app.foreground(Effect::Launch(request), &mut fx);
                    }
                }
            }
        }
        // Any reading of the logs of the session shown is taken, whichever round it is: the
        // session of that account, since two accounts may each have one of this short id.
        Event::Logs {
            account,
            short_id,
            result,
        } => {
            let shown = app
                .logs
                .as_mut()
                .filter(|logs| logs.key.0 == account.qualified() && logs.short_id == short_id);
            let wanted = shown.is_some();
            if let Some(logs) = shown {
                logs.result = Some(result);
            }
            // Asked for again since this run began: once more, if they are still shown.
            let target = (account, short_id);
            if let Claim::Again(_) = app.work.logs.settle(&target) {
                if wanted {
                    let (account, short_id) = target;
                    fx.push(Effect::Logs { account, short_id });
                } else {
                    app.work.logs.cancel();
                }
            }
        }
        Event::ControlDone {
            verb,
            short_id,
            result,
        } => {
            let (level, text) = match result {
                Ok(out) => {
                    let done = match verb {
                        Control::Stop => "stopped",
                        Control::Remove => "removed",
                    };
                    let mut text = Marked::from(format!("{done} {short_id}"));
                    if let Some(line) = out.lines().find(|l| !l.trim().is_empty()) {
                        text = text.text(" · ").text(line.trim());
                    }
                    (Level::Info, text)
                }
                Err(e) => (
                    Level::Error,
                    Marked::from(format!("claude {} {short_id} ", verb.command())).text(e),
                ),
            };
            app.notify(level, text);
            app.rerun(Work::Live, &mut fx);
        }
        Event::StatsProgress { done, total } => app.stats.progress = Some((done, total)),
        Event::Stats { report, error } => {
            for table in &report.tables {
                for name in table.sections.iter().flat_map(|s| &s.accounts) {
                    app.aliases.note(name);
                }
            }
            app.stats.report = Some(report);
            app.stats.error = error;
            app.stats.progress = None;
            app.stats.computed = Some(app.now);
            app.clamp_lists();
            app.ran(Work::Stats, &mut fx);
        }
        Event::Accounts(accounts) => app.set_accounts(accounts, &mut fx),
        Event::AccountRemoved { account, result } => {
            let name = display_name(&account);
            match result {
                Ok(()) => {
                    let text = Marked::from(format!("removed {name}; its home "))
                        .path(&account.home)
                        .text(" was left in place");
                    app.notify(Level::Info, text);
                }
                Err(e) => {
                    let text = Marked::from(format!("cannot remove {name}: ")).text(e);
                    app.notify(Level::Error, text);
                }
            }
        }
        Event::RolloutWritten { path, at } => {
            if let Some(Overlay::ResumeCodex(confirm)) = &mut app.overlay
                && confirm.path == path
                && at.is_some()
            {
                confirm.written = at;
            }
        }
        Event::SetupDone {
            provider,
            name,
            result,
            links,
        } => {
            app.on_setup_done(provider, &name, result, links);
            // Like after a launch: only a collection started now makes the live list current.
            app.rerun(Work::Live, &mut fx);
        }
        Event::Launched {
            request,
            result,
            warnings,
        } => app.on_launched(request, result, warnings, &mut fx),
    }
    app.retarget_config(&mut fx);
    app.retarget_preview();
    fx
}
