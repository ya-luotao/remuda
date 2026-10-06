//! The items of a claude home and how a member's relate to the source's (SPEC R12, R18).
//!
//! Two questions are answered here, and only here. The catalog says what a home holds: each
//! item's name, whether `setup` links it and on what condition, whether the one `--add-dir`
//! of a launch carries it, when the source's counts as something to share, and what breaks
//! when it is another account's. The relations ([`Source::relate`], [`linked_elsewhere`]) say
//! what a member's home has of them right now. Shared configuration ([`crate::share`]), the
//! checks ([`crate::checks`]), the Configuration pane ([`crate::account_config`]) and `setup`
//! ([`crate::setup`]) read these answers; none of them spells an item's name or decides by
//! itself whether a home has the source's.
//!
//! How a launch injects the settings, the plugins and the memory locations is not said here:
//! [`crate::share::plan`] decides that, item by item, from the relations.
//!
//! Read-only: nothing is written (R13). A path is always the home as registered with the
//! item's name appended (R2); realpaths are compared here and never leave the module.

use std::cell::OnceCell;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::Env;
use crate::registry::{Account, CLAUDE, Sharing};
use crate::share;

/// An item of a claude home, in the order of the catalog: first the ones a member shares with
/// the source, in the order `setup` links them (R18), then what stays per account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Id {
    Projects,
    FileHistory,
    Settings,
    ClaudeMd,
    Skills,
    Commands,
    Agents,
    Hooks,
    Plugins,
    Rules,
    AgentMemory,
    OutputStyles,
    Keybindings,
    ClaudeJson,
    History,
    Sessions,
    RemoteSettings,
    PolicyLimits,
}

impl Id {
    /// The item's name in a home.
    pub const fn name(self) -> &'static str {
        ITEMS[self as usize].name
    }

    pub fn item(self) -> &'static Item {
        &ITEMS[self as usize]
    }

    /// `<home>/<name>`, from `home` as given (R2).
    pub fn at(self, home: &Path) -> PathBuf {
        home.join(self.name())
    }
}

/// Whether `setup` links an item of the source's home into a new member's (R18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Setup {
    /// When the source has it.
    Link,
    /// When the source has it and it sets no authentication at that moment: a home that links
    /// it reads all of it.
    LinkWithoutAuthentication,
    /// Whatever the source has: it stays per account. The login's identity and caches,
    /// attribution (R9), running sessions (R7), and what an organization sets for its
    /// accounts.
    Never,
}

/// How the `--add-dir=$REMUDA_HOME/shared/claude` of a launch carries an instruction item of
/// the source's (R18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddDir {
    /// As a link under its `.claude`.
    Link,
    /// As copies there: claude does not load rules through links.
    Copies,
}

/// When the source's item counts as something to share with a member's home.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Counts {
    /// Whatever is there (not missing, not a dangling link).
    Present,
    /// A directory.
    Directory,
    /// A `rules` directory with at least one rule file ([`share::rule_files`]): one without
    /// is not an item, even when the home links it.
    RuleFile,
}

/// One entry of the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Item {
    pub id: Id,
    pub name: &'static str,
    /// What a member that shares `projects` with the source loses without a link to this
    /// item too (R11).
    pub loses: Option<&'static str>,
    setup: Setup,
    /// `None`: not an instruction item.
    add_dir: Option<AddDir>,
    /// What breaks when the home's is a link to another account's (R11).
    breaks: Option<&'static str>,
    counts: Counts,
}

const fn shared(id: Id, name: &'static str, add_dir: Option<AddDir>, counts: Counts) -> Item {
    Item {
        id,
        name,
        loses: None,
        setup: Setup::Link,
        add_dir,
        breaks: None,
        counts,
    }
}

const fn per_account(id: Id, name: &'static str, breaks: Option<&'static str>) -> Item {
    Item {
        id,
        name,
        loses: None,
        setup: Setup::Never,
        add_dir: None,
        breaks,
        counts: Counts::Present,
    }
}

/// Every item of a claude home remuda knows (R18), an [`Id`] at its own position.
static ITEMS: [Item; 18] = {
    use Counts::{Directory, Present, RuleFile};
    const LINK: Option<AddDir> = Some(AddDir::Link);
    [
        shared(Id::Projects, "projects", None, Directory),
        Item {
            loses: Some(
                "/rewind does not find the file backups of a session resumed from another account",
            ),
            ..shared(Id::FileHistory, "file-history", None, Directory)
        },
        Item {
            setup: Setup::LinkWithoutAuthentication,
            ..shared(Id::Settings, "settings.json", None, Present)
        },
        shared(Id::ClaudeMd, "CLAUDE.md", LINK, Present),
        shared(Id::Skills, "skills", LINK, Present),
        shared(Id::Commands, "commands", LINK, Present),
        shared(Id::Agents, "agents", LINK, Present),
        shared(Id::Hooks, "hooks", None, Present),
        shared(Id::Plugins, "plugins", None, Present),
        shared(Id::Rules, "rules", Some(AddDir::Copies), RuleFile),
        Item {
            loses: Some("the memory of user-scope subagents is not shared with this account"),
            ..shared(Id::AgentMemory, "agent-memory", None, Directory)
        },
        shared(Id::OutputStyles, "output-styles", None, Present),
        shared(Id::Keybindings, "keybindings.json", None, Present),
        per_account(
            Id::ClaudeJson,
            ".claude.json",
            Some(
                "the two logins get mixed up (claude does not fetch the account's profile again \
                 within 24 hours)",
            ),
        ),
        per_account(
            Id::History,
            "history.jsonl",
            Some("sessions in a shared store lose their attribution"),
        ),
        per_account(
            Id::Sessions,
            "sessions",
            Some("running sessions cannot be told apart by account"),
        ),
        // Never linked either (R18), and nothing more is known of them: listed so that neither
        // is ever added above.
        per_account(Id::RemoteSettings, "remote-settings.json", None),
        per_account(Id::PolicyLimits, "policy-limits.json", None),
    ]
};

/// The items a member shares with the source: the first of [`ITEMS`].
const SHARED: usize = 13;

/// The items `setup` links into a new member's home when the source has them (R18), in
/// order.
fn linked_by_setup() -> impl Iterator<Item = &'static Item> {
    ITEMS.iter().filter(|item| item.setup != Setup::Never)
}

/// The instruction items, which one `--add-dir` gives a member together (R18): the linked
/// ones, then the rules.
pub fn instruction_items() -> impl Iterator<Item = &'static Item> {
    ITEMS.iter().filter(|item| item.add_dir.is_some())
}

/// The names of the `N` instruction items that `--add-dir` carries as `how`.
const fn add_dir_names<const N: usize>(how: AddDir) -> [&'static str; N] {
    let mut out = [""; N];
    let (mut i, mut n) = (0, 0);
    while i < ITEMS.len() {
        if matches!(ITEMS[i].add_dir, Some(a) if a as u8 == how as u8) {
            out[n] = ITEMS[i].name;
            n += 1;
        }
        i += 1;
    }
    assert!(n == N);
    out
}

/// The names of the instruction items [`crate::share`] keeps a link to each of.
pub const fn add_dir_links() -> [&'static str; 4] {
    add_dir_names(AddDir::Link)
}

/// The name of the instruction item [`crate::share`] keeps copies of: the rules.
pub const fn add_dir_copies() -> &'static str {
    add_dir_names::<1>(AddDir::Copies)[0]
}

/// Where `account` keeps an item: in its home, except the native login's `.claude.json` (R2).
/// `None` when `HOME` is unknown.
fn locate(account: &Account, id: Id, env: &Env) -> Option<PathBuf> {
    match id {
        Id::ClaudeJson => account.claude_json(env),
        _ => account.home_dir(env).map(|home| id.at(&home)),
    }
}

/// Whether both paths exist and resolve to the same file or directory (R12): the one way two
/// homes are compared.
pub(crate) fn resolves_to(path: &Path, source: &Path) -> bool {
    match (fs::canonicalize(path), fs::canonicalize(source)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// How one item of a member's home relates to the source's (R12, R18).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Relation {
    /// The home's resolves to the source's (same realpath).
    Linked,
    /// The source has it and the home's does not resolve to it: it has none, one of its own,
    /// or a link to somewhere else.
    Unlinked,
    /// The source has nothing to share.
    #[default]
    Absent,
}

/// The relation of each item a member shares with the source, for one home at one moment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Relations([Relation; SHARED]);

impl Relations {
    /// [`Relation::Absent`] for what stays per account: nothing of it is the source's to
    /// share ([`linked_elsewhere`] looks at those).
    fn get(&self, id: Id) -> Relation {
        self.0.get(id as usize).copied().unwrap_or_default()
    }

    /// The home's resolves to the source's (same realpath): nothing is injected for it.
    pub fn linked(&self, id: Id) -> bool {
        self.get(id) == Relation::Linked
    }

    /// The source has the item and the home does not reach it.
    pub fn unlinked(&self, id: Id) -> bool {
        self.get(id) == Relation::Unlinked
    }

    /// The source has the item to share, whether or not the home links it.
    pub fn source_has(&self, id: Id) -> bool {
        self.get(id) != Relation::Absent
    }

    /// The [`instruction_items`] by relation.
    pub fn instructions(&self) -> Instructions {
        let mut out = Instructions::default();
        for item in instruction_items() {
            match self.get(item.id) {
                Relation::Linked => out.linked.push(item.name),
                Relation::Unlinked => out.unlinked.push(item.name),
                Relation::Absent => {}
            }
        }
        out
    }
}

/// How a home's instruction items relate to the source's: an item the source does not have
/// has nothing to share and counts as neither.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Instructions {
    /// Items that already resolve to the source's (symlinks).
    pub linked: Vec<&'static str>,
    /// Items of the source that the home does not reach.
    pub unlinked: Vec<&'static str>,
}

impl Instructions {
    /// `--add-dir` is injected unless every item the source has already resolves to it.
    pub fn needs_injection(&self) -> bool {
        !self.unlinked.is_empty()
    }

    /// Some items shared through symlinks and others not: with the injected `--add-dir`, the
    /// linked ones load twice (R11).
    pub fn partial(&self) -> bool {
        !self.linked.is_empty() && !self.unlinked.is_empty()
    }
}

/// The source's home (R18), a directory when [`Source::of`] returns it; its `settings.json`
/// is read once, when first asked for. Everything else is looked at when asked.
#[derive(Debug)]
pub struct Source {
    home: PathBuf,
    settings: OnceCell<anyhow::Result<Map<String, Value>>>,
}

impl Source {
    /// The source's home at `home`, taken as given.
    pub fn at(home: &Path) -> Source {
        Source {
            home: home.to_path_buf(),
            settings: OnceCell::new(),
        }
    }

    /// The home of the source account, when it is a directory: without one nothing is shared.
    pub fn of(account: &Account, env: &Env) -> Option<Source> {
        let home = account.home_dir(env).filter(|home| home.is_dir())?;
        Some(Source::at(&home))
    }

    /// The home as registered, never canonicalized (R2).
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The source's `settings.json` ([`share::read_settings`]: a missing file is empty,
    /// anything but a JSON object in a regular file is an error).
    pub fn settings(&self) -> Result<&Map<String, Value>, &anyhow::Error> {
        self.settings
            .get_or_init(|| share::read_settings(&Id::Settings.at(&self.home)))
            .as_ref()
    }

    /// [`Source::settings`] with its error as it was raised, for a launch that fails on it.
    pub fn into_settings(self) -> anyhow::Result<Map<String, Value>> {
        let path = Id::Settings.at(&self.home);
        self.settings
            .into_inner()
            .unwrap_or_else(|| share::read_settings(&path))
    }

    /// The authentication settings of the source, as `key` or `env.NAME` (R18): never
    /// injected, and read whole by a home that links the file. None when it cannot be read.
    pub fn withheld(&self) -> Vec<String> {
        self.settings().map(share::withheld).unwrap_or_default()
    }

    /// What `setup` does about each item it links (R18): the first thirteen of the catalog,
    /// in order. Each is looked at when the iterator reaches it: a moment before its link is
    /// made.
    pub fn setup_links(&self) -> impl Iterator<Item = (&'static Item, SetupLink)> {
        linked_by_setup().map(|item| (item, self.setup_link(item)))
    }

    fn setup_link(&self, item: &Item) -> SetupLink {
        let target = item.id.at(&self.home);
        if fs::metadata(&target).is_err() {
            return SetupLink::Absent;
        }
        if item.setup == Setup::LinkWithoutAuthentication {
            match self.settings() {
                Ok(settings) => {
                    let keys = share::withheld(settings);
                    if !keys.is_empty() {
                        return SetupLink::Refused(Unlinked::Authentication(keys));
                    }
                }
                Err(_) => return SetupLink::Refused(Unlinked::Unreadable),
            }
        }
        SetupLink::Target(target)
    }

    /// Whether the source has `item` to share with a member's home.
    fn has(&self, item: &Item) -> bool {
        let path = item.id.at(&self.home);
        match item.counts {
            Counts::Present => fs::metadata(&path).is_ok(),
            Counts::Directory => path.is_dir(),
            Counts::RuleFile => !share::rule_files(&path).is_empty(),
        }
    }

    /// How each shared item of the member's home `home` relates to this source's.
    pub fn relate(&self, home: &Path) -> Relations {
        let mut relations = Relations::default();
        for (relation, item) in relations.0.iter_mut().zip(&ITEMS) {
            let linked = || resolves_to(&item.id.at(home), &item.id.at(&self.home));
            *relation = match item.counts {
                Counts::RuleFile if !self.has(item) => Relation::Absent,
                _ if linked() => Relation::Linked,
                Counts::RuleFile => Relation::Unlinked,
                _ if !self.has(item) => Relation::Absent,
                _ => Relation::Unlinked,
            };
        }
        relations
    }
}

/// What `setup` does about one item of the source's home (R18).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupLink {
    /// The source has none (missing, or a dangling link): no link.
    Absent,
    /// The source has it, and it is not linked.
    Refused(Unlinked),
    /// Linked, to this path: `<source home>/<item>`, not canonicalized.
    Target(PathBuf),
}

/// Why the source's `settings.json` is not linked (R18): a member that links it reads all of
/// it, and authentication stays per account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unlinked {
    /// It sets these authentication settings (`key` / `env.NAME`, [`Source::withheld`]).
    Authentication(Vec<String>),
    /// It cannot be read as a JSON object, so it cannot be checked.
    Unreadable,
}

/// An item that stays per account whose entry in a home is a link to another account's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Elsewhere {
    /// The item's name.
    pub name: &'static str,
    /// What breaks.
    pub breaks: &'static str,
    /// `provider:name` of the account it resolves to: the first of them in registry order.
    pub account: String,
}

/// What `member` keeps per account, of the items R11 names (`.claude.json`, `history.jsonl`,
/// `sessions`), that is a symlink resolving to the same item of another claude account among
/// `accounts`, the source included. This needs no source home.
pub fn linked_elsewhere(member: &Account, accounts: &[Account], env: &Env) -> Vec<Elsewhere> {
    let mut out = Vec::new();
    for item in &ITEMS {
        let Some(breaks) = item.breaks else {
            continue;
        };
        let Some(own) = locate(member, item.id, env) else {
            continue;
        };
        if !fs::symlink_metadata(&own).is_ok_and(|m| m.file_type().is_symlink()) {
            continue;
        }
        let other = accounts.iter().find(|other| {
            other.provider == CLAUDE
                && *other != member
                && locate(other, item.id, env).is_some_and(|theirs| resolves_to(&own, &theirs))
        });
        if let Some(other) = other {
            out.push(Elsewhere {
                name: item.name,
                breaks,
                account: other.qualified(),
            });
        }
    }
    out
}

/// An account's part in shared configuration (R18); `source` is the source account, as
/// `provider:name`.
#[derive(Debug)]
pub enum Membership {
    /// Nothing is shared with it: there is no `[share.claude]`, or it is not a claude account.
    Alone,
    /// The account others get their configuration from.
    Source,
    /// `share = false`.
    OptedOut { source: String },
    /// A member whose source's home does not exist: nothing is shared.
    SourceMissing { source: String },
    /// A member of `source`, whose home is there.
    Member { source: String, home: Source },
}

/// The part of `account` in `sharing` ([`Sharing::source_for`] decides who is a member).
pub fn membership(sharing: &Sharing, account: &Account, env: &Env) -> Membership {
    let Some(source) = &sharing.source else {
        return Membership::Alone;
    };
    let name = source.qualified();
    let qualified = account.qualified();
    if qualified == name {
        Membership::Source
    } else if sharing.opted_out.contains(&qualified) {
        Membership::OptedOut { source: name }
    } else if sharing.source_for(account).is_none() {
        Membership::Alone
    } else {
        match Source::of(source, env) {
            Some(home) => Membership::Member { source: name, home },
            None => Membership::SourceMissing { source: name },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::provider::Provider;
    use crate::registry::{CODEX, Home};
    use crate::test_homes::{ClaudeHome, Root};
    use crate::{attribution, live};

    fn named(name: &str, home: &Path) -> Account {
        Account {
            provider: CLAUDE,
            name: name.into(),
            home: Home::Path(home.display().to_string()),
        }
    }

    fn names<'a>(items: impl Iterator<Item = &'a Item>) -> Vec<&'static str> {
        items.map(|item| item.name).collect()
    }

    /// R18: the catalog is the list of R18. What `setup` links, in its order; what is never
    /// linked; nothing in both. The instruction items are the ones one `--add-dir` carries,
    /// the rules last and as copies. `settings.json` alone is linked on a condition.
    #[test]
    fn the_catalog_is_the_list_of_r18() {
        for (i, item) in ITEMS.iter().enumerate() {
            assert_eq!(item.id as usize, i, "{}", item.name);
            assert_eq!(item.id.name(), item.name);
            assert_eq!(item.id.item(), item);
            assert_eq!(
                ITEMS.iter().filter(|other| other.name == item.name).count(),
                1,
                "{}",
                item.name
            );
            // What `setup` never links stays per account: it comes last, has no relation to
            // the source's, and alone can break when it is another account's.
            let per_account = item.setup == Setup::Never;
            assert_eq!(per_account, i >= SHARED, "{}", item.name);
            if !per_account {
                assert_eq!(item.breaks, None, "{}", item.name);
            }
        }
        assert_eq!(
            names(linked_by_setup()),
            [
                "projects",
                "file-history",
                "settings.json",
                "CLAUDE.md",
                "skills",
                "commands",
                "agents",
                "hooks",
                "plugins",
                "rules",
                "agent-memory",
                "output-styles",
                "keybindings.json",
            ]
        );
        assert_eq!(
            names(ITEMS.iter().filter(|item| item.setup == Setup::Never)),
            [
                ".claude.json",
                "history.jsonl",
                "sessions",
                "remote-settings.json",
                "policy-limits.json",
            ]
        );
        // R11 says what breaks for three of them.
        assert_eq!(
            names(ITEMS.iter().filter(|item| item.breaks.is_some())),
            [".claude.json", "history.jsonl", "sessions"]
        );
        assert_eq!(
            names(
                ITEMS
                    .iter()
                    .filter(|item| item.setup == Setup::LinkWithoutAuthentication)
            ),
            ["settings.json"]
        );
        assert_eq!(
            names(instruction_items()),
            ["CLAUDE.md", "skills", "commands", "agents", "rules"]
        );
        assert_eq!(
            add_dir_links(),
            ["CLAUDE.md", "skills", "commands", "agents"]
        );
        assert_eq!(add_dir_copies(), "rules");
        // What R11 asks to link together with `projects`.
        assert_eq!(
            names(ITEMS.iter().filter(|item| item.loses.is_some())),
            ["file-history", "agent-memory"]
        );
    }

    /// R2, R7–R9: an item is where the modules that read it look for it: the session store,
    /// the running sessions, the history, and `.claude.json`, which the native login keeps in
    /// `$HOME` and every other account in its home.
    #[test]
    fn items_are_where_their_readers_look() {
        let f = Root::new();
        let work = named("work", &f.root.join("work"));
        let native = Account::default_for(CLAUDE);
        assert_eq!(Provider::Claude.store_dir(), Id::Projects.name());
        for account in [&work, &native] {
            assert_eq!(
                locate(account, Id::Sessions, &f.env),
                live::sessions_dir(account, &f.env)
            );
            assert_eq!(
                locate(account, Id::History, &f.env),
                attribution::history_path(account, &f.env)
            );
        }
        assert_eq!(
            locate(&work, Id::ClaudeJson, &f.env),
            Some(f.root.join("work/.claude.json"))
        );
        assert_eq!(
            locate(&native, Id::ClaudeJson, &f.env),
            Some(f.root.join("home/.claude.json"))
        );
        assert_eq!(
            locate(&native, Id::Settings, &f.env),
            Some(f.native().join("settings.json"))
        );
        assert_eq!(locate(&native, Id::Settings, &Env::new()), None);
    }

    /// R12, R18: each shared item of a home is the source's by realpath, not the source's
    /// although the source has one (nothing there, one of its own, a link to somewhere else,
    /// a dangling link), or nothing the source has to share; what only the home has is the
    /// last.
    #[test]
    fn each_item_relates_to_the_sources() {
        let f = Root::new();
        let source = f
            .home("src")
            .dir("projects")
            .dir("file-history")
            .dir("plugins")
            .dir("hooks")
            .dir("skills")
            .settings("{}")
            .into_path();
        let other = f.home("other").dir("plugins").into_path();
        let home = f
            .home("max")
            .linked(&source, &["projects"])
            .linked(&other, &["plugins"])
            .dangling("hooks")
            .dir("skills")
            .dir("output-styles")
            .settings("{}")
            .into_path();
        let got = Source::at(&source).relate(&home);
        for (id, relation) in [
            (Id::Projects, Relation::Linked),
            (Id::FileHistory, Relation::Unlinked),
            (Id::Settings, Relation::Unlinked),
            (Id::Plugins, Relation::Unlinked),
            (Id::Hooks, Relation::Unlinked),
            (Id::Skills, Relation::Unlinked),
            (Id::OutputStyles, Relation::Absent),
            (Id::Keybindings, Relation::Absent),
            (Id::ClaudeMd, Relation::Absent),
            // What stays per account has no relation to the source.
            (Id::ClaudeJson, Relation::Absent),
            (Id::Sessions, Relation::Absent),
        ] {
            assert_eq!(got.get(id), relation, "{id:?}");
            assert_eq!(got.linked(id), relation == Relation::Linked, "{id:?}");
            assert_eq!(got.unlinked(id), relation == Relation::Unlinked, "{id:?}");
            assert_eq!(got.source_has(id), relation != Relation::Absent, "{id:?}");
        }
        // Nothing is the source's for a home that is the source's own directory by another
        // path: every item it has resolves to itself.
        let alias = f.root.join("alias");
        symlink(&source, &alias).unwrap();
        let got = Source::at(&source).relate(&alias);
        for id in [Id::Projects, Id::FileHistory, Id::Settings, Id::Plugins] {
            assert_eq!(got.get(id), Relation::Linked, "{id:?}");
        }
        assert_eq!(
            Relations::default(),
            Source::at(&f.root.join("gone")).relate(&home)
        );
    }

    /// R11, R18: what R11 warns about once the source "has one" is a directory: a file named
    /// `projects`, `file-history` or `agent-memory` is nothing to share. A home that links it
    /// all the same has the source's by realpath, as a launch counts it.
    #[test]
    fn the_stores_count_when_they_are_directories() {
        let f = Root::new();
        let source = f
            .home("src")
            .file("projects", "")
            .file("file-history", "")
            .file("agent-memory", "")
            .into_path();
        let home = f.home("max").into_path();
        let got = Source::at(&source).relate(&home);
        for id in [Id::Projects, Id::FileHistory, Id::AgentMemory] {
            assert_eq!(got.get(id), Relation::Absent, "{id:?}");
        }
        let home = ClaudeHome::at(home)
            .linked(&source, &["projects", "agent-memory"])
            .into_path();
        let got = Source::at(&source).relate(&home);
        assert_eq!(got.get(Id::Projects), Relation::Linked);
        assert_eq!(got.get(Id::AgentMemory), Relation::Linked);
        assert_eq!(got.get(Id::FileHistory), Relation::Absent);
    }

    /// R11, R18: only the instruction items the source has count; an item that resolves to
    /// the source's is linked.
    #[test]
    fn instruction_items_by_realpath() {
        let f = Root::new();
        let source = f
            .home("source")
            .dir("skills")
            .dir("agents")
            .claude_md("be brief")
            .into_path();
        let home = f.home("home").dir("commands").into_path();
        let relate = || Source::at(&source).relate(&home).instructions();
        let got = relate();
        assert_eq!(got.linked, Vec::<&str>::new());
        assert_eq!(got.unlinked, ["CLAUDE.md", "skills", "agents"]);
        assert!(got.needs_injection() && !got.partial());

        symlink(source.join("CLAUDE.md"), home.join("CLAUDE.md")).unwrap();
        symlink(source.join("skills"), home.join("skills")).unwrap();
        let got = relate();
        assert_eq!(got.linked, ["CLAUDE.md", "skills"]);
        assert_eq!(got.unlinked, ["agents"]);
        assert!(got.needs_injection() && got.partial());

        // `commands` exists only in the home: nothing of the source's to share there.
        symlink(source.join("agents"), home.join("agents")).unwrap();
        let got = relate();
        assert!(!got.needs_injection() && !got.partial(), "{got:?}");
    }

    /// R11, R18: the rules are an instruction item only when the source has a rule file, and
    /// linked when the home's `rules` resolves to the source's. A `rules` without a rule file
    /// is no item even for a home that links it.
    #[test]
    fn rules_count_as_an_item_when_the_source_has_one() {
        let f = Root::new();
        let source = f
            .home("source")
            .dir("rules/empty")
            .file("rules/README", "no rule yet")
            .claude_md("be brief")
            .into_path();
        let home = f.home("home").linked(&source, &["CLAUDE.md"]).into_path();
        let relate = || Source::at(&source).relate(&home);
        let got = relate().instructions();
        assert!(!got.needs_injection(), "{got:?}");

        fs::write(source.join("rules/style.md"), "be terse").unwrap();
        let got = relate().instructions();
        assert_eq!(got.linked, ["CLAUDE.md"]);
        assert_eq!(got.unlinked, ["rules"]);
        assert!(got.partial());

        symlink(source.join("rules"), home.join("rules")).unwrap();
        let got = relate().instructions();
        assert_eq!(got.linked, ["CLAUDE.md", "rules"]);
        assert!(!got.needs_injection());

        fs::remove_file(source.join("rules/style.md")).unwrap();
        assert_eq!(relate().get(Id::Rules), Relation::Absent);
        assert_eq!(relate().instructions().linked, ["CLAUDE.md"]);
    }

    /// R11: what stays per account and is a symlink to that item of another claude account is
    /// found, the source and the native login's `$HOME/.claude.json` included, with or without
    /// a source home. A link to anything else, an item of the account's own, a codex account
    /// and the account itself are not; nor is an item R11 does not name.
    #[test]
    fn per_account_items_linked_to_another_account() {
        let f = Root::new();
        let max = f.home("max").into_path();
        let work = f
            .home("work")
            .dir("sessions")
            .file("history.jsonl", "")
            .file("remote-settings.json", "{}")
            .into_path();
        let stray = f.home("stray").file("history.jsonl", "").into_path();
        fs::write(f.root.join("home/.claude.json"), "{}").unwrap();
        symlink(f.root.join("home/.claude.json"), max.join(".claude.json")).unwrap();
        symlink(stray.join("history.jsonl"), max.join("history.jsonl")).unwrap();
        fs::create_dir(max.join("sessions")).unwrap();
        symlink(
            work.join("remote-settings.json"),
            max.join("remote-settings.json"),
        )
        .unwrap();
        let accounts = [
            Account::default_for(CLAUDE),
            named("max", &max),
            Account {
                provider: CODEX,
                ..named("cx", &work)
            },
            named("work", &work),
            named("again", &work),
        ];
        let found = |member: &Account| -> Vec<(&str, String)> {
            linked_elsewhere(member, &accounts, &f.env)
                .into_iter()
                .map(|link| (link.name, link.account))
                .collect()
        };
        assert_eq!(
            found(&accounts[1]),
            [(".claude.json", "claude:default".to_string())]
        );
        assert_eq!(found(&accounts[0]), []);
        // Not its own, however it is reached: `again` is `work`'s home, with no link in it.
        assert_eq!(found(&accounts[4]), []);

        fs::remove_file(max.join("history.jsonl")).unwrap();
        symlink(work.join("history.jsonl"), max.join("history.jsonl")).unwrap();
        fs::remove_dir(max.join("sessions")).unwrap();
        symlink(work.join("sessions"), max.join("sessions")).unwrap();
        let found = linked_elsewhere(&accounts[1], &accounts, &f.env);
        assert_eq!(
            found.iter().map(|link| link.name).collect::<Vec<_>>(),
            [".claude.json", "history.jsonl", "sessions"]
        );
        // With what breaks, as the catalog has it.
        for link in &found {
            assert!(!link.breaks.is_empty(), "{}", link.name);
        }
        assert_eq!(
            found[1].breaks,
            "sessions in a shared store lose their attribution"
        );
        // Among the account alone, nothing is another's.
        assert_eq!(linked_elsewhere(&accounts[1], &accounts[1..2], &f.env), []);
    }

    /// R18: every claude account other than the source is a member unless it opts out; a
    /// codex account is not affected; a member whose source has no home gets nothing. The
    /// source's home is the string that was registered (R2), not a real path.
    #[test]
    fn membership_is_decided_once() {
        let f = Root::new();
        f.home("src");
        let registered = f.root.join("x/../src");
        fs::create_dir(f.root.join("x")).unwrap();
        let source = named("src", &registered);
        let member = named("max", &f.root.join("max"));
        let sharing = Sharing {
            source: Some(source.clone()),
            opted_out: vec!["claude:solo".into()],
        };
        let part = |account: &Account, sharing: &Sharing| membership(sharing, account, &f.env);
        assert!(matches!(
            part(&member, &Sharing::default()),
            Membership::Alone
        ));
        assert!(matches!(part(&source, &sharing), Membership::Source));
        assert!(matches!(
            part(&named("solo", &f.root.join("solo")), &sharing),
            Membership::OptedOut { source } if source == "claude:src"
        ));
        let codex = Account {
            provider: CODEX,
            ..member.clone()
        };
        assert!(matches!(part(&codex, &sharing), Membership::Alone));
        match part(&member, &sharing) {
            Membership::Member { source, home } => {
                assert_eq!(source, "claude:src");
                assert_eq!(home.home(), registered);
                assert_eq!(
                    home.home().as_os_str(),
                    registered.as_os_str(),
                    "byte for byte"
                );
            }
            other => panic!("{other:?}"),
        }
        let gone = Sharing {
            source: Some(named("gone", &f.root.join("gone"))),
            opted_out: vec![],
        };
        assert!(matches!(
            part(&member, &gone),
            Membership::SourceMissing { source } if source == "claude:gone"
        ));
        assert!(Source::of(&named("gone", &f.root.join("gone")), &f.env).is_none());
        assert!(Source::of(&Account::default_for(CLAUDE), &Env::new()).is_none());
    }

    /// R11, R18: the source's settings are read once and its authentication settings named
    /// from that read; a file that cannot be read names none, and a launch gets the error of
    /// the read itself.
    #[test]
    fn the_sources_settings_are_read_once() {
        let f = Root::new();
        let home = f
            .home("src")
            .settings(r#"{"model": "opus", "apiKeyHelper": "/k", "env": {"AWS_PROFILE": "p"}}"#)
            .into_path();
        let source = Source::at(&home);
        assert_eq!(source.withheld(), ["apiKeyHelper", "env.AWS_PROFILE"]);
        fs::write(home.join("settings.json"), "{}").unwrap();
        assert_eq!(source.withheld(), ["apiKeyHelper", "env.AWS_PROFILE"]);
        assert!(source.into_settings().unwrap().contains_key("model"));
        // Not asked before: read now.
        assert_eq!(
            Source::at(&home).into_settings().unwrap(),
            Map::new(),
            "the file as it is now"
        );

        fs::write(home.join("settings.json"), "[]").unwrap();
        let source = Source::at(&home);
        assert_eq!(source.withheld(), Vec::<String>::new());
        assert!(source.settings().is_err());
        let direct = share::read_settings(&home.join("settings.json")).unwrap_err();
        assert_eq!(
            format!("{:#}", source.into_settings().unwrap_err()),
            format!("{direct:#}")
        );
        // No file: empty settings, not an error.
        let none = Source::at(&f.root.join("max"));
        assert_eq!(none.settings().unwrap(), &Map::new());
    }

    /// R18: `setup` gets a link target for each item the source has, written from the home as
    /// given; none for a missing item or a dangling link; and none for a `settings.json` that
    /// sets authentication or cannot be read. Each item is looked at when its turn comes.
    #[test]
    fn setup_links_what_the_source_has() {
        let f = Root::new();
        let home = f
            .home("src")
            .dir("projects")
            .dangling("skills")
            .settings(r#"{"forceLoginOrgUUID": "o"}"#)
            .file(".claude.json", "{}")
            .dir("sessions")
            .into_path();
        let source = Source::at(&home);
        let links: Vec<(&str, SetupLink)> = source
            .setup_links()
            .map(|(item, link)| (item.name, link))
            .collect();
        assert_eq!(
            links.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            names(linked_by_setup())
        );
        for (name, link) in &links {
            let want = match *name {
                "projects" => SetupLink::Target(home.join("projects")),
                "settings.json" => {
                    SetupLink::Refused(Unlinked::Authentication(vec!["forceLoginOrgUUID".into()]))
                }
                _ => SetupLink::Absent,
            };
            assert_eq!(link, &want, "{name}");
        }

        fs::write(home.join("settings.json"), "{").unwrap();
        let unreadable: Vec<SetupLink> = Source::at(&home)
            .setup_links()
            .filter(|(item, _)| item.id == Id::Settings)
            .map(|(_, link)| link)
            .collect();
        assert_eq!(unreadable, [SetupLink::Refused(Unlinked::Unreadable)]);

        // An item made while the links are being made is found when its turn comes.
        let source = Source::at(&home);
        let mut links = source.setup_links();
        assert_eq!(links.next().map(|(item, _)| item.name), Some("projects"));
        fs::create_dir(home.join("file-history")).unwrap();
        assert_eq!(
            links.next(),
            Some((
                Id::FileHistory.item(),
                SetupLink::Target(home.join("file-history"))
            ))
        );
    }
}
