//! Read-only problem checks shown in the accounts view (SPEC R11). Login state is checked by
//! the caller from identities; everything here looks only at the environment snapshot and
//! the file system.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::Env;
use crate::home_items::{self, Id, Source};
use crate::index::Store;
use crate::privacy::Marked;
use crate::registry::{Account, CLAUDE, Home, Sharing};
use crate::share::{self, Installs};
use crate::{launch, usage};

/// One problem worth a warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// `provider:name` of the account concerned; `None` for machine-wide problems.
    pub account: Option<String>,
    /// What to tell, in its pieces: each path in it is a piece of its own, and so is each name
    /// read from a file (a plugin, a settings key), so private mode masks those and leaves
    /// what the message says around them (R21).
    pub message: Marked,
}

/// A message that starts with remuda's own `words`.
fn say(words: impl AsRef<str>) -> Marked {
    Marked::default().words(words)
}

/// `message` with `names` after it, each a piece of its own (they are read from a file),
/// `, ` between them.
fn listed(mut message: Marked, names: &[String]) -> Marked {
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            message = message.words(", ");
        }
        message = message.text(name);
    }
    message
}

pub const API_KEY_VAR: &str = "ANTHROPIC_API_KEY";
pub const AUTH_TOKEN_VAR: &str = "ANTHROPIC_AUTH_TOKEN";
pub const OAUTH_TOKEN_VAR: &str = "CLAUDE_CODE_OAUTH_TOKEN";
pub const BASE_URL_VAR: &str = "ANTHROPIC_BASE_URL";

/// Variables that change what claude does for every account (R11), in the order they are told,
/// with what they do. A value is never told: the tokens are secrets, and a URL is not
/// remuda's to show.
const ENV_OVERRIDES: &[(&str, &str)] = &[
    (API_KEY_VAR, "it overrides every account's /login"),
    (AUTH_TOKEN_VAR, "it overrides every account's /login"),
    (OAUTH_TOKEN_VAR, "it overrides every account's /login"),
    (
        BASE_URL_VAR,
        "every account's requests go to that endpoint instead of Anthropic's",
    ),
    (
        usage::NONESSENTIAL_TRAFFIC_VAR,
        "claude's /usage then only repeats a reading taken within the hour, if any; remuda \
         removes it for its own live usage queries, but claude launched as any account keeps it",
    ),
];

/// All file-system and environment checks, in a stable order: environment, then per account
/// (registry order), then shared stores. `stores` is [`crate::index::stores`] of `accounts`.
pub fn run(accounts: &[Account], env: &Env, stores: &[Store]) -> Vec<Check> {
    let mut checks = Vec::new();
    for (var, what) in ENV_OVERRIDES {
        if env.get(*var).is_some_and(|v| !v.is_empty()) {
            checks.push(Check {
                account: None,
                message: say(format!("{var} is set: {what}")),
            });
        }
    }
    // Last of the environment: it needs two variables, the marker and claude's override (R6).
    if launch::child_session_unsaved(env) {
        checks.push(Check {
            account: None,
            message: say(launch::child_session_warning()),
        });
    }
    // Claude and codex homes alike: only directory listings and `lstat`/`stat`/`readlink`, so
    // codex's `auth.json` (credentials, R4) is never opened.
    for account in accounts {
        let Some(home) = account.home_dir(env) else {
            continue;
        };
        let qualified = account.qualified();
        if matches!(account.home, Home::Path(_)) && !home.is_dir() {
            checks.push(Check {
                account: Some(qualified),
                message: say("home ").path(home.display()).words(" does not exist"),
            });
            continue;
        }
        for (link, target) in dangling_symlinks(&home) {
            checks.push(Check {
                account: Some(qualified.clone()),
                message: say("dangling symlink ")
                    .path(link.display())
                    .words(" -> ")
                    .path(target.display()),
            });
        }
    }
    // `cleanupPeriodDays` is claude's: a shared codex `sessions` store is not a problem.
    for store in stores
        .iter()
        .filter(|s| s.provider == CLAUDE && s.accounts.len() > 1)
    {
        let missing: Vec<&str> = store
            .accounts
            .iter()
            .filter(|name| {
                let account = accounts.iter().find(|a| &a.qualified() == *name);
                !account.is_some_and(|a| has_cleanup_period(a, env))
            })
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            checks.push(Check {
                account: None,
                message: say("projects ").path(store.path.display()).words(format!(
                    " is shared by {}, but {} {} no cleanupPeriodDays in settings.json \
                         (the default 30-day cleanup deletes everyone's sessions)",
                    store.accounts.join(", "),
                    missing.join(", "),
                    if missing.len() == 1 { "has" } else { "have" },
                )),
            });
        }
    }
    checks
}

/// Shared configuration (R11, R18): a source account that is not listed or whose home is
/// missing; members whose `.claude.json`, `history.jsonl` or `sessions` is a link to another
/// account's (checked even without the source's home); members that share `projects` with
/// the source but not `file-history`, or not `agent-memory` where a settings file chooses
/// `autoMemoryDirectory` (a launch then does not redirect memory); once per member, the
/// items of the source it does not link, with the link that would share each (a notice: an
/// account whose `projects` is its own does not see the source's sessions, R16; a
/// `settings.json` the source sets authentication in is not for linking, and the account
/// needs one of its own); members whose `plugins` link the source's installs go through;
/// members that read the source's authentication settings through a linked `settings.json`.
pub fn sharing(accounts: &[Account], env: &Env, sharing: &Sharing) -> Vec<Check> {
    let mut checks = Vec::new();
    let Some(source) = &sharing.source else {
        return checks;
    };
    let name = source.qualified();
    if !accounts.iter().any(|a| a.qualified() == name) {
        checks.push(Check {
            account: Some(name),
            message: say("[share.claude] from names this account, which is not registered"),
        });
        return checks;
    }
    // Without the source's home nothing is shared, and only what does not depend on it is
    // checked: the members' own files.
    let from = Source::of(source, env);
    if from.is_none() {
        let home = source
            .home_dir(env)
            .map_or(source.home.to_string(), |d| d.display().to_string());
        checks.push(Check {
            account: Some(name.clone()),
            message: say("home ")
                .path(home)
                .words(" of the shared configuration source does not exist: nothing is shared"),
        });
    }
    let installs = from
        .as_ref()
        .map(|from| share::installed_plugins(from.home()));
    // A settings file of the source that cannot be read chooses nothing and sets no
    // authentication.
    let unread = Map::new();
    let settings = from
        .as_ref()
        .and_then(|from| from.settings().ok())
        .unwrap_or(&unread);
    let withheld = from.as_ref().map(Source::withheld).unwrap_or_default();
    let (projects, settings_file) = (Id::Projects.name(), Id::Settings.name());
    for account in accounts {
        if sharing.source_for(account).is_none() {
            continue;
        }
        let Some(home) = account.home_dir(env).filter(|d| d.is_dir()) else {
            continue;
        };
        // What stays per account (R18), when it is a link to another account's: checked
        // whether or not the source's home exists.
        for link in home_items::linked_elsewhere(account, accounts, env) {
            checks.push(Check {
                account: Some(account.qualified()),
                // The item's name and what breaks are the catalog's (`home_items`), remuda's
                // own words; the other account is a name.
                message: say(format!(
                    "{} is a symlink to that of {}: {}; each account needs its own",
                    link.name, link.account, link.breaks
                )),
            });
        }
        let Some(from) = &from else {
            continue;
        };
        // What the home has of the source's items, looked at once.
        let items = from.relate(&home);
        // `message` with ` (<home>/<item> -> <source home>/<item>)` after it: the link that
        // would share the item, each end a path of its own.
        let link = |message: Marked, id: Id| {
            message
                .words(" (")
                .path(id.at(&home).display())
                .words(" -> ")
                .path(id.at(from.home()).display())
                .words(")")
        };
        // An item that goes with `projects` and is not linked with it: what the account
        // loses (`why` adds when), and the link to make. All of it but the link is remuda's
        // own words: the names and what is lost are the catalog's (`home_items`).
        let apart = |id: Id, why: &str| {
            link(
                say(format!(
                    "shares {projects} with {name} through a symlink but not {}{why}: {}; link \
                     it too",
                    id.name(),
                    id.item().loses.unwrap_or_default(),
                )),
                id,
            )
        };
        let projects_linked = items.linked(Id::Projects);
        if projects_linked {
            if items.unlinked(Id::FileHistory) {
                checks.push(Check {
                    account: Some(account.qualified()),
                    message: apart(Id::FileHistory, ""),
                });
            }
            // Agent memory follows auto-memory (R18): a launch through a shared `projects` sets
            // the memory variable to the source's home, unless a settings file chooses
            // `autoMemoryDirectory`; then only a link shares the source's `agent-memory`.
            let chosen = settings.contains_key(share::MEMORY_KEY)
                || share::read_settings(&Id::Settings.at(&home))
                    .is_ok_and(|own| own.contains_key(share::MEMORY_KEY));
            if chosen && items.unlinked(Id::AgentMemory) {
                let why = format!(
                    ", and a settings file chooses {}, so a launch does not redirect memory",
                    share::MEMORY_KEY
                );
                checks.push(Check {
                    account: Some(account.qualified()),
                    message: apart(Id::AgentMemory, &why),
                });
            }
        }
        // What the home does not share, in one notice: the items, how to link one, and what
        // not having `projects` means (R16). `file-history` and `agent-memory` go with a
        // linked `projects` (above: the latter is shared by the launch where it is not said).
        // A `settings.json` the source sets authentication in is not for linking (`setup`
        // leaves it out, R18): the account needs one of its own, said while it has none.
        let own_settings = !withheld.is_empty() && items.unlinked(Id::Settings);
        let needs_own_settings =
            own_settings && fs::symlink_metadata(Id::Settings.at(&home)).is_err();
        let unlinked: Vec<&str> = items
            .unlinked_items()
            .filter(|item| {
                !(projects_linked && matches!(item.id, Id::FileHistory | Id::AgentMemory))
            })
            .filter(|item| !(own_settings && item.id == Id::Settings))
            .map(|item| item.name)
            .collect();
        let needs_own = |message: Marked| {
            listed(
                message.words(format!(
                    "{settings_file} is not shared: that of {name} sets authentication \
                     settings ("
                )),
                &withheld,
            )
            .words("), so this account needs one of its own")
        };
        if !unlinked.is_empty() {
            let mut message = say(format!(
                "does not share {} with {name}",
                unlinked.join(", ")
            ));
            if items.unlinked(Id::Projects) {
                message = message
                    .words(": it does not see the sessions in ")
                    .path(Id::Projects.at(from.home()).display())
                    .words(" and cannot resume them");
            }
            message = message
                .words("; to share an item: ln -s ")
                .path(format!("{}/<item>", from.home().display()))
                .words(" ")
                .path(format!("{}/<item>", home.display()));
            if needs_own_settings {
                message = needs_own(message.words("; "));
            }
            checks.push(Check {
                account: Some(account.qualified()),
                message,
            });
        } else if needs_own_settings {
            checks.push(Check {
                account: Some(account.qualified()),
                message: needs_own(Marked::default()),
            });
        }
        // A plugin installed from a member's home is recorded through that home's `plugins`
        // link, which every account then depends on.
        if let Some(Installs::Known(plugins)) = &installs
            && items.linked(Id::Plugins)
        {
            let through = Id::Plugins.at(&home);
            let held = plugins
                .values()
                .filter(|installs| {
                    installs
                        .iter()
                        .any(|i| i.path.as_deref().is_some_and(|p| p.starts_with(&through)))
                })
                .count();
            if held > 0 {
                let (plugins, are, stop) = if held == 1 {
                    ("plugin", "is", "stops")
                } else {
                    ("plugins", "are", "stop")
                };
                checks.push(Check {
                    account: Some(account.qualified()),
                    message: Marked::default().path(through.display()).words(format!(
                        " must stay a symlink: {held} {plugins} of {name} {are} installed \
                         through it and {stop} loading for every account without it"
                    )),
                });
            }
        }
        // A home that links the source's `settings.json` reads all of it, authentication
        // included (R18).
        if items.linked(Id::Settings) && !withheld.is_empty() {
            checks.push(Check {
                account: Some(account.qualified()),
                message: listed(
                    say(format!(
                        "{settings_file} is that of {name} through a symlink, and it sets \
                         authentication settings ("
                    )),
                    &withheld,
                )
                .words(format!(
                    "): this account reads them through the link; give it a {settings_file} \
                     of its own instead of the link, or move them out of that {settings_file}"
                )),
            });
        }
    }
    checks
}

/// Top-level entries of `dir` that are symlinks to nothing, with their targets.
fn dangling_symlinks(dir: &Path) -> Vec<(PathBuf, PathBuf)> {
    let Ok(listing) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(PathBuf, PathBuf)> = listing
        .flatten()
        .map(|e| e.path())
        .filter(|p| fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()))
        .filter(|p| fs::metadata(p).is_err())
        .filter_map(|p| {
            let target = fs::read_link(&p).ok()?;
            Some((p, target))
        })
        .collect();
    out.sort();
    out
}

/// Whether the account's `settings.json` (symlinks followed) sets `cleanupPeriodDays`.
fn has_cleanup_period(account: &Account, env: &Env) -> bool {
    let Some(home) = account.home_dir(env) else {
        return false;
    };
    fs::read(Id::Settings.at(&home))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .is_some_and(|v| v.get("cleanupPeriodDays").is_some_and(|d| !d.is_null()))
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::index;
    use crate::privacy::Piece;
    use crate::registry::CODEX;
    use crate::test_homes::{ClaudeHome, Root};

    /// [`super::run`], every message of it [`marked_right`].
    fn run(accounts: &[Account], env: &Env, stores: &[Store]) -> Vec<Check> {
        let checks = super::run(accounts, env, stores);
        marked_right(&checks);
        checks
    }

    /// [`super::sharing`], every message of it [`marked_right`].
    fn sharing_checks(accounts: &[Account], env: &Env, sharing: &Sharing) -> Vec<Check> {
        let checks = super::sharing(accounts, env, sharing);
        marked_right(&checks);
        checks
    }

    /// R21: no path hides in what a message gives as words or as a name: remuda's words hold
    /// no `/` but that of the three slash commands they name, and nothing outside a path piece
    /// holds the directory the fixtures are in. Every test here runs its checks through this,
    /// so a path formatted into the words of any message fails the test that produces it.
    fn marked_right(checks: &[Check]) {
        let tmp = std::env::temp_dir();
        let tmp = tmp.to_string_lossy();
        let tmp = tmp.trim_end_matches('/');
        for check in checks {
            for (piece, kind) in check.message.pieces() {
                match kind {
                    Piece::Path => continue,
                    Piece::Words => {
                        let said = piece
                            .replace("/login", "")
                            .replace("/rewind", "")
                            .replace("/usage", "");
                        assert!(!said.contains('/'), "{piece:?} in {check:?}");
                    }
                    Piece::Text => {}
                }
                assert!(!piece.contains(tmp), "{piece:?} in {check:?}");
            }
        }
    }

    /// Each message with `<>` for a path and `{}` for a name read from a file: what is left is
    /// what private mode leaves readable (R21).
    fn shapes(checks: &[Check]) -> Vec<String> {
        checks
            .iter()
            .map(|c| {
                c.message
                    .pieces()
                    .map(|(piece, kind)| match kind {
                        Piece::Words => piece,
                        Piece::Path => "<>",
                        Piece::Text => "{}",
                    })
                    .collect()
            })
            .collect()
    }

    fn named(name: &str, home: &Path) -> Account {
        Account {
            provider: CLAUDE,
            name: name.into(),
            home: Home::Path(home.display().to_string()),
        }
    }

    fn codex(name: &str, home: &Path) -> Account {
        Account {
            provider: CODEX,
            ..named(name, home)
        }
    }

    fn messages(checks: &[Check]) -> Vec<(Option<&str>, &str)> {
        checks
            .iter()
            .map(|c| (c.account.as_deref(), c.message.as_str()))
            .collect()
    }

    #[test]
    fn a_clean_setup_has_no_problems() {
        let f = Root::new();
        let max = f.root.join("max");
        fs::create_dir_all(max.join("projects")).unwrap();
        let work = f.root.join("codex-work");
        fs::create_dir_all(work.join("sessions")).unwrap();
        let accounts = vec![
            Account::default_for(CLAUDE),
            named("max", &max),
            Account::default_for(CODEX),
            codex("work", &work),
        ];
        let stores = index::stores(&accounts, &f.env);
        assert_eq!(run(&accounts, &f.env, &stores), []);
    }

    #[test]
    fn api_key_in_the_environment() {
        let mut f = Root::new();
        f.env.insert(API_KEY_VAR.into(), "sk-test".into());
        let got = run(&[], &f.env, &[]);
        assert_eq!(
            messages(&got),
            [(
                None,
                "ANTHROPIC_API_KEY is set: it overrides every account's /login"
            )]
        );
        // All of it is remuda's words: the slash command in it is not taken for a path.
        assert_eq!(
            shapes(&got),
            ["ANTHROPIC_API_KEY is set: it overrides every account's /login"]
        );
        f.env.insert(API_KEY_VAR.into(), String::new());
        assert_eq!(run(&[], &f.env, &[]), []);
    }

    /// R11: each variable that takes every account's requests away from its own login, or its
    /// live usage away from a new reading, is told once, in a fixed order, and never with its
    /// value; an empty one is not set.
    #[test]
    fn login_and_endpoint_overrides_in_the_environment() {
        let mut f = Root::new();
        let values = [
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "zq-traffic"),
            ("ANTHROPIC_BASE_URL", "https://zq-gateway.example/v1"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "sk-ant-oat01-zq"),
            ("CLAUDE_CODE_CHILD_SESSION", "TRUE"),
            ("ANTHROPIC_AUTH_TOKEN", "zq-bearer"),
        ];
        for (var, value) in values {
            f.env.insert(var.into(), value.into());
        }
        let got = run(&[], &f.env, &[]);
        let said = [
            "ANTHROPIC_AUTH_TOKEN is set: it overrides every account's /login",
            "CLAUDE_CODE_OAUTH_TOKEN is set: it overrides every account's /login",
            "ANTHROPIC_BASE_URL is set: every account's requests go to that endpoint instead \
             of Anthropic's",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC is set: claude's /usage then only repeats \
             a reading taken within the hour, if any; remuda removes it for its own live usage \
             queries, but claude launched as any account keeps it",
            // Last, whatever its place among the variables (R6).
            "CLAUDE_CODE_CHILD_SESSION is set: claude saves no transcript or prompt history for an \
             interactive session started here, so what is said in it never reaches remuda's \
             session list and cannot be resumed, though the launch is logged; set \
             CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1 to keep it",
        ];
        assert_eq!(
            messages(&got),
            said.iter().map(|m| (None, *m)).collect::<Vec<_>>()
        );
        assert_eq!(shapes(&got), said);
        for check in &got {
            for (_, value) in values {
                assert!(!check.message.as_str().contains(value), "{check:?}");
            }
        }
        // After the API key, which keeps its place first.
        f.env.insert(API_KEY_VAR.into(), "sk-test".into());
        let got = run(&[], &f.env, &[]);
        assert_eq!(got.len(), 6);
        assert_eq!(
            messages(&got)[0],
            (
                None,
                "ANTHROPIC_API_KEY is set: it overrides every account's /login"
            )
        );
        for (var, _) in values {
            f.env.insert(var.into(), String::new());
        }
        assert_eq!(run(&[], &f.env, &[]).len(), 1);
    }

    /// R11, R6: the inherited marker is told only while claude's override is off; each is read
    /// as claude reads a switch, so an empty value, `0` or `false` is off.
    #[test]
    fn child_session_check_needs_the_marker_without_the_override() {
        let said = launch::child_session_warning();
        let child = |marker: Option<&str>, force: Option<&str>| {
            let mut f = Root::new();
            if let Some(v) = marker {
                f.env.insert(launch::CHILD_SESSION_VAR.into(), v.into());
            }
            if let Some(v) = force {
                f.env.insert(launch::FORCE_PERSISTENCE_VAR.into(), v.into());
            }
            messages(&run(&[], &f.env, &[]))
                .into_iter()
                .map(|(account, m)| (account.map(str::to_string), m.to_string()))
                .collect::<Vec<_>>()
        };
        assert_eq!(child(Some("1"), None), [(None, said.clone())]);
        assert_eq!(child(Some("1"), Some("")), [(None, said.clone())]);
        assert_eq!(child(Some("on"), Some("0")), [(None, said.clone())]);
        assert_eq!(child(Some("1"), Some("false")), [(None, said)]);
        assert_eq!(child(Some("1"), Some("1")), []);
        assert_eq!(child(Some("1"), Some("YES")), []);
        assert_eq!(child(Some(""), None), []);
        assert_eq!(child(Some("0"), None), []);
        assert_eq!(child(None, Some("1")), []);
        assert_eq!(child(None, None), []);
    }

    #[test]
    fn missing_home_and_dangling_symlinks() {
        let f = Root::new();
        let gone = f.root.join("gone");
        let max = f.root.join("max");
        fs::create_dir_all(&max).unwrap();
        symlink(f.root.join("nowhere"), max.join("CLAUDE.md")).unwrap();
        symlink(&max, max.join("ok-link")).unwrap();
        symlink(
            f.root.join("missing-dir"),
            f.root.join("home/.claude/agents"),
        )
        .unwrap();
        let accounts = vec![
            Account::default_for(CLAUDE),
            named("gone", &gone),
            named("max", &max),
        ];
        let got = run(&accounts, &f.env, &[]);
        let native = f.root.join("home/.claude/agents");
        assert_eq!(
            messages(&got),
            [
                (
                    Some("claude:default"),
                    format!(
                        "dangling symlink {} -> {}",
                        native.display(),
                        f.root.join("missing-dir").display()
                    )
                    .as_str()
                ),
                (
                    Some("claude:gone"),
                    format!("home {} does not exist", gone.display()).as_str()
                ),
                (
                    Some("claude:max"),
                    format!(
                        "dangling symlink {} -> {}",
                        max.join("CLAUDE.md").display(),
                        f.root.join("nowhere").display()
                    )
                    .as_str()
                ),
            ]
        );
        // The link and its target are a path each; what is said around them is not.
        assert_eq!(
            shapes(&got),
            [
                "dangling symlink <> -> <>",
                "home <> does not exist",
                "dangling symlink <> -> <>"
            ]
        );
        let paths: Vec<&str> = got[1]
            .message
            .pieces()
            .filter(|(_, kind)| *kind == Piece::Path)
            .map(|(piece, _)| piece)
            .collect();
        assert_eq!(paths, [gone.display().to_string()]);
    }

    /// R11 for codex homes: a registered home that does not exist and dangling top-level
    /// symlinks, as for claude. `codex:default` without `~/.codex` is not a problem (it is
    /// listed when `codex` is on PATH, R17).
    #[test]
    fn codex_homes_are_checked_like_claude_homes() {
        let f = Root::new();
        let gone = f.root.join("codex-gone");
        let work = f.root.join("codex-work");
        fs::create_dir_all(work.join("sessions")).unwrap();
        fs::write(work.join("config.toml"), "").unwrap();
        symlink(f.root.join("nowhere"), work.join("AGENTS.md")).unwrap();
        symlink(work.join("config.toml"), work.join("ok-link")).unwrap();
        let accounts = vec![
            Account::default_for(CODEX),
            codex("gone", &gone),
            codex("work", &work),
        ];
        let stores = index::stores(&accounts, &f.env);
        let got = run(&accounts, &f.env, &stores);
        assert_eq!(
            messages(&got),
            [
                (
                    Some("codex:gone"),
                    format!("home {} does not exist", gone.display()).as_str()
                ),
                (
                    Some("codex:work"),
                    format!(
                        "dangling symlink {} -> {}",
                        work.join("AGENTS.md").display(),
                        f.root.join("nowhere").display()
                    )
                    .as_str()
                ),
            ]
        );

        // The native home is checked too once it exists.
        let native = f.root.join("home/.codex");
        fs::create_dir_all(&native).unwrap();
        symlink(f.root.join("gone-too"), native.join("rules")).unwrap();
        let got = run(&accounts[..1], &f.env, &[]);
        assert_eq!(
            messages(&got),
            [(
                Some("codex:default"),
                format!(
                    "dangling symlink {} -> {}",
                    native.join("rules").display(),
                    f.root.join("gone-too").display()
                )
                .as_str()
            )]
        );
    }

    /// `auth.json` holds credentials (R4): the checks never open it. A FIFO there would block
    /// any `open` for reading forever; the checks still finish, and say nothing about it.
    #[test]
    fn codex_checks_never_open_auth_json() {
        let f = Root::new();
        let work = f.root.join("codex-work");
        fs::create_dir_all(work.join("sessions")).unwrap();
        let fifo =
            std::ffi::CString::new(work.join("auth.json").into_os_string().into_vec()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let accounts = vec![codex("work", &work)];
        let (tx, rx) = std::sync::mpsc::channel();
        let env = f.env.clone();
        std::thread::spawn(move || {
            let stores = index::stores(&accounts, &env);
            let _ = tx.send(run(&accounts, &env, &stores));
        });
        let got = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the checks opened auth.json");
        assert_eq!(got, []);
    }

    fn sharing_from(source: Account) -> Sharing {
        Sharing {
            source: Some(source),
            opted_out: vec!["claude:solo".into()],
        }
    }

    /// R11, R18: nothing to say about a clean shared setup: a member that links everything
    /// the source has.
    #[test]
    fn a_clean_shared_setup_has_no_problems() {
        let f = Root::new();
        let native = f.root.join("home/.claude");
        fs::write(native.join("CLAUDE.md"), "x").unwrap();
        let max = f.root.join("max");
        fs::create_dir_all(&max).unwrap();
        symlink(native.join("CLAUDE.md"), max.join("CLAUDE.md")).unwrap();
        let accounts = vec![Account::default_for(CLAUDE), named("max", &max)];
        let sharing = sharing_from(Account::default_for(CLAUDE));
        assert_eq!(sharing_checks(&accounts, &f.env, &sharing), []);
        assert_eq!(sharing_checks(&accounts, &f.env, &Sharing::default()), []);
    }

    /// R11: the source's home is missing, or the source is not an account.
    #[test]
    fn a_missing_source() {
        let f = Root::new();
        let gone = f.root.join("gone");
        let accounts = vec![Account::default_for(CLAUDE), named("gone", &gone)];
        let got = sharing_checks(&accounts, &f.env, &sharing_from(named("gone", &gone)));
        assert_eq!(
            messages(&got),
            [(
                Some("claude:gone"),
                format!(
                    "home {} of the shared configuration source does not exist: nothing is shared",
                    gone.display()
                )
                .as_str()
            )]
        );
        assert_eq!(
            shapes(&got),
            ["home <> of the shared configuration source does not exist: nothing is shared"]
        );
        let got = sharing_checks(&accounts[..1], &f.env, &sharing_from(named("gone", &gone)));
        assert_eq!(
            messages(&got),
            [(
                Some("claude:gone"),
                "[share.claude] from names this account, which is not registered"
            )]
        );
    }

    /// R11, R18: a member whose `projects` is the source's shares the source's `agent-memory`
    /// through the memory variable set at launch, linked or not: nothing to say about it,
    /// unless a settings file chooses `autoMemoryDirectory`, which keeps the variable unset.
    #[test]
    fn agent_memory_goes_with_a_shared_projects() {
        let f = Root::new();
        let native = f.root.join("home/.claude");
        fs::create_dir_all(native.join("projects")).unwrap();
        fs::create_dir_all(native.join("agent-memory/reviewer")).unwrap();
        let max = f.root.join("max");
        fs::create_dir_all(&max).unwrap();
        symlink(native.join("projects"), max.join("projects")).unwrap();
        let accounts = vec![Account::default_for(CLAUDE), named("max", &max)];
        let sharing = sharing_from(Account::default_for(CLAUDE));
        assert_eq!(sharing_checks(&accounts, &f.env, &sharing), []);

        // The source's or the home's settings choose the location: only a link shares it.
        let warning = format!(
            "shares projects with claude:default through a symlink but not agent-memory, and \
             a settings file chooses autoMemoryDirectory, so a launch does not redirect \
             memory: the memory of user-scope subagents is not shared with this account; link \
             it too ({} -> {})",
            max.join("agent-memory").display(),
            native.join("agent-memory").display()
        );
        // (A `settings.json` of the source's is one more item the member does not link.)
        let unlinked_settings = format!(
            "does not share settings.json with claude:default; to share an item: ln -s \
             {}/<item> {}/<item>",
            native.display(),
            max.display()
        );
        for (home, expected) in [
            (&native, vec![warning.as_str(), unlinked_settings.as_str()]),
            (&max, vec![warning.as_str()]),
        ] {
            fs::write(
                home.join("settings.json"),
                r#"{"autoMemoryDirectory":"/m"}"#,
            )
            .unwrap();
            let got = sharing_checks(&accounts, &f.env, &sharing);
            assert_eq!(
                messages(&got),
                expected
                    .iter()
                    .map(|m| (Some("claude:max"), *m))
                    .collect::<Vec<_>>()
            );
            fs::remove_file(home.join("settings.json")).unwrap();
        }
        fs::write(max.join("settings.json"), r#"{"autoMemoryDirectory":"/m"}"#).unwrap();
        symlink(native.join("agent-memory"), max.join("agent-memory")).unwrap();
        assert_eq!(sharing_checks(&accounts, &f.env, &sharing), []);
        fs::remove_file(max.join("settings.json")).unwrap();
        assert_eq!(sharing_checks(&accounts, &f.env, &sharing), []);

        // Its own `projects`: no launch points the memory anywhere, and the account is told
        // it shares neither, sessions included.
        fs::remove_file(max.join("agent-memory")).unwrap();
        fs::remove_file(max.join("projects")).unwrap();
        fs::create_dir(max.join("projects")).unwrap();
        assert_eq!(
            messages(&sharing_checks(&accounts, &f.env, &sharing)),
            [(
                Some("claude:max"),
                not_sharing(&native, &max, "projects, agent-memory").as_str()
            )]
        );
    }

    /// The notice for a member `home` that does not share `items`, `projects` among them,
    /// with the source `from`.
    fn not_sharing(from: &Path, home: &Path, items: &str) -> String {
        format!(
            "does not share {items} with claude:default: it does not see the sessions in {} \
             and cannot resume them; to share an item: ln -s {}/<item> {}/<item>",
            from.join("projects").display(),
            from.display(),
            home.display()
        )
    }

    /// R11, R18: a member whose `projects` is not the source's does not share sessions. That
    /// is said once the source has a store, for a home with or without one of its own; not
    /// for the source or an account that opted out.
    #[test]
    fn a_member_that_does_not_share_projects() {
        let f = Root::new();
        let native = f.root.join("home/.claude");
        let (max, team, solo) = (f.root.join("max"), f.root.join("team"), f.root.join("solo"));
        for home in [&max, &team, &solo] {
            fs::create_dir_all(home).unwrap();
        }
        fs::create_dir(team.join("projects")).unwrap();
        let accounts = vec![
            Account::default_for(CLAUDE),
            named("max", &max),
            named("team", &team),
            named("solo", &solo),
        ];
        let sharing = sharing_from(Account::default_for(CLAUDE));
        assert_eq!(sharing_checks(&accounts, &f.env, &sharing), []);

        fs::create_dir(native.join("projects")).unwrap();
        assert_eq!(
            messages(&sharing_checks(&accounts, &f.env, &sharing)),
            [
                (
                    Some("claude:max"),
                    not_sharing(&native, &max, "projects").as_str()
                ),
                (
                    Some("claude:team"),
                    not_sharing(&native, &team, "projects").as_str()
                ),
            ]
        );
        // The store and both ends of the link are paths; what to do about it stays readable.
        assert_eq!(
            shapes(&sharing_checks(&accounts, &f.env, &sharing))[0],
            "does not share projects with claude:default: it does not see the sessions in <> \
             and cannot resume them; to share an item: ln -s <> <>"
        );
        symlink(native.join("projects"), max.join("projects")).unwrap();
        fs::remove_dir(team.join("projects")).unwrap();
        // Through another member's link is the source's store too.
        symlink(max.join("projects"), team.join("projects")).unwrap();
        assert_eq!(sharing_checks(&accounts, &f.env, &sharing), []);
    }

    /// R11, R18: a member that shares `projects` with the source but not `file-history`, once
    /// the source has one: `/rewind` across accounts misses the file backups.
    #[test]
    fn file_history_apart_from_a_shared_projects() {
        let f = Root::new();
        let native = f.root.join("home/.claude");
        fs::create_dir_all(native.join("projects")).unwrap();
        let max = f.root.join("max");
        fs::create_dir_all(max.join("file-history")).unwrap();
        symlink(native.join("projects"), max.join("projects")).unwrap();
        let accounts = vec![Account::default_for(CLAUDE), named("max", &max)];
        let sharing = sharing_from(Account::default_for(CLAUDE));
        assert_eq!(sharing_checks(&accounts, &f.env, &sharing), []);

        fs::create_dir(native.join("file-history")).unwrap();
        assert_eq!(
            messages(&sharing_checks(&accounts, &f.env, &sharing)),
            [(
                Some("claude:max"),
                format!(
                    "shares projects with claude:default through a symlink but not \
                     file-history: /rewind does not find the file backups of a session resumed \
                     from another account; link it too ({} -> {})",
                    max.join("file-history").display(),
                    native.join("file-history").display()
                )
                .as_str()
            )]
        );
        assert_eq!(
            shapes(&sharing_checks(&accounts, &f.env, &sharing)),
            [
                "shares projects with claude:default through a symlink but not file-history: \
                 /rewind does not find the file backups of a session resumed from another \
                 account; link it too (<> -> <>)"
            ]
        );
        fs::remove_dir(max.join("file-history")).unwrap();
        symlink(native.join("file-history"), max.join("file-history")).unwrap();
        assert_eq!(sharing_checks(&accounts, &f.env, &sharing), []);
    }

    /// R11, R18: `.claude.json`, `history.jsonl` and `sessions` stay per account. A member's
    /// that is a symlink to another registered account's is named with what breaks; a link
    /// to anything else, a file of its own, and an account that opted out are not.
    #[test]
    fn per_account_items_linked_to_another_account() {
        let f = Root::new();
        let native = f.root.join("home/.claude");
        fs::create_dir(native.join("sessions")).unwrap();
        // The native login's `.claude.json` is next to its home, not in it (R2).
        fs::write(f.root.join("home/.claude.json"), "{}").unwrap();
        fs::write(native.join(".claude.json"), "{}").unwrap();
        let (max, team, solo) = (f.root.join("max"), f.root.join("team"), f.root.join("solo"));
        for home in [&max, &team, &solo] {
            fs::create_dir_all(home).unwrap();
        }
        fs::write(team.join("history.jsonl"), "").unwrap();
        fs::write(team.join(".claude.json"), "{}").unwrap();
        let elsewhere = f.root.join("elsewhere");
        fs::write(&elsewhere, "").unwrap();
        let accounts = vec![
            Account::default_for(CLAUDE),
            named("max", &max),
            named("team", &team),
            named("solo", &solo),
        ];
        let sharing = sharing_from(Account::default_for(CLAUDE));

        // Not another account's: nothing to say.
        symlink(native.join(".claude.json"), max.join(".claude.json")).unwrap();
        symlink(&elsewhere, max.join("history.jsonl")).unwrap();
        symlink(native.join("sessions"), solo.join("sessions")).unwrap();
        assert_eq!(sharing_checks(&accounts, &f.env, &sharing), []);

        fs::remove_file(max.join(".claude.json")).unwrap();
        fs::remove_file(max.join("history.jsonl")).unwrap();
        symlink(f.root.join("home/.claude.json"), max.join(".claude.json")).unwrap();
        symlink(team.join("history.jsonl"), max.join("history.jsonl")).unwrap();
        symlink(native.join("sessions"), max.join("sessions")).unwrap();
        assert_eq!(
            messages(&sharing_checks(&accounts, &f.env, &sharing)),
            [
                (
                    Some("claude:max"),
                    ".claude.json is a symlink to that of claude:default: the two logins get \
                     mixed up (claude does not fetch the account's profile again within 24 \
                     hours); each account needs its own"
                ),
                (
                    Some("claude:max"),
                    "history.jsonl is a symlink to that of claude:team: sessions in a shared \
                     store lose their attribution; each account needs its own"
                ),
                (
                    Some("claude:max"),
                    "sessions is a symlink to that of claude:default: running sessions cannot \
                     be told apart by account; each account needs its own"
                ),
            ]
        );
    }

    /// R11: the per-account items are checked for every member whose home exists, also when
    /// the source's home is missing and nothing else can be.
    #[test]
    fn per_account_items_are_checked_without_the_source_home() {
        let f = Root::new();
        let gone = f.root.join("gone");
        let (max, team) = (f.root.join("max"), f.root.join("team"));
        for home in [&max, &team] {
            fs::create_dir_all(home).unwrap();
        }
        fs::write(team.join(".claude.json"), "{}").unwrap();
        symlink(team.join(".claude.json"), max.join(".claude.json")).unwrap();
        // What depends on the source is not checked: this would be a partial share.
        symlink(gone.join("skills"), team.join("skills")).unwrap();
        let accounts = vec![
            named("gone", &gone),
            named("max", &max),
            named("team", &team),
        ];
        let got = sharing_checks(&accounts, &f.env, &sharing_from(named("gone", &gone)));
        assert_eq!(
            messages(&got),
            [
                (
                    Some("claude:gone"),
                    format!(
                        "home {} of the shared configuration source does not exist: nothing is \
                         shared",
                        gone.display()
                    )
                    .as_str()
                ),
                (
                    Some("claude:max"),
                    ".claude.json is a symlink to that of claude:team: the two logins get \
                     mixed up (claude does not fetch the account's profile again within 24 \
                     hours); each account needs its own"
                ),
            ]
        );
    }

    /// R11, R18: a member whose `settings.json` is the source's reads its authentication
    /// settings through the link: said of that member, by key name, with what to do instead.
    /// A member without a `settings.json` is told it needs one of its own; one whose file is
    /// its own is told nothing. Without authentication in the source's there is nothing to
    /// say of the link, and the file is an item to share like any other.
    #[test]
    fn authentication_read_through_a_linked_settings_file() {
        let f = Root::new();
        let native = f.root.join("home/.claude");
        fs::write(
            native.join("settings.json"),
            r#"{"apiKeyHelper": "/k", "model": "x", "env": {"ANTHROPIC_AUTH_TOKEN": "t"}}"#,
        )
        .unwrap();
        let (max, team, own, solo) = (
            f.root.join("max"),
            f.root.join("team"),
            f.root.join("own"),
            f.root.join("solo"),
        );
        for home in [&max, &team, &own, &solo] {
            fs::create_dir_all(home).unwrap();
        }
        symlink(native.join("settings.json"), max.join("settings.json")).unwrap();
        symlink(native.join("settings.json"), solo.join("settings.json")).unwrap();
        fs::write(own.join("settings.json"), r#"{"model": "y"}"#).unwrap();
        let sharing = sharing_from(Account::default_for(CLAUDE));
        let linked = (
            Some("claude:max"),
            "settings.json is that of claude:default through a symlink, and it sets \
             authentication settings (apiKeyHelper, env.ANTHROPIC_AUTH_TOKEN): this account \
             reads them through the link; give it a settings.json of its own instead of the \
             link, or move them out of that settings.json",
        );
        let needs_own = (
            Some("claude:team"),
            "settings.json is not shared: that of claude:default sets authentication \
             settings (apiKeyHelper, env.ANTHROPIC_AUTH_TOKEN), so this account needs one of \
             its own",
        );

        // `solo` opted out: not checked.
        let accounts = vec![
            Account::default_for(CLAUDE),
            named("max", &max),
            named("solo", &solo),
        ];
        assert_eq!(
            messages(&sharing_checks(&accounts, &f.env, &sharing)),
            [linked]
        );
        let mut mixed = accounts.clone();
        mixed.push(named("team", &team));
        mixed.push(named("own", &own));
        let got = sharing_checks(&mixed, &f.env, &sharing);
        assert_eq!(messages(&got), [linked, needs_own]);
        assert_eq!(
            shapes(&got)[0],
            "settings.json is that of claude:default through a symlink, and it sets \
             authentication settings ({}, {}): this account reads them through the link; give \
             it a settings.json of its own instead of the link, or move them out of that \
             settings.json"
        );
        // Settings without authentication may be linked, and are an item to share.
        fs::write(native.join("settings.json"), r#"{"model": "x"}"#).unwrap();
        assert_eq!(
            messages(&sharing_checks(&mixed, &f.env, &sharing)),
            [
                (
                    Some("claude:team"),
                    format!(
                        "does not share settings.json with claude:default; to share an item: \
                         ln -s {}/<item> {}/<item>",
                        native.display(),
                        team.display()
                    )
                    .as_str()
                ),
                (
                    Some("claude:own"),
                    format!(
                        "does not share settings.json with claude:default; to share an item: \
                         ln -s {}/<item> {}/<item>",
                        native.display(),
                        own.display()
                    )
                    .as_str()
                ),
            ]
        );
    }

    /// R11, R18, R22: the checks and a launch read one table of how the member's home relates
    /// to the source's ([`crate::home_items`]), so for the same two homes they agree: what the
    /// accounts view says the home does not share is what it does not link, and the memory
    /// variable is set exactly where the checks have nothing to say about `agent-memory`.
    /// Over every combination of six links, with and without a settings file that chooses the
    /// auto-memory location.
    #[test]
    fn the_checks_agree_with_the_launch_plan() {
        const ITEMS: [&str; 6] = [
            "projects",
            "file-history",
            "agent-memory",
            "settings.json",
            "CLAUDE.md",
            "rules",
        ];
        // Bit `i` of `links`: the home links `ITEMS[i]`.
        let layout = |links: u32| -> Vec<&str> {
            let items = ITEMS.iter().enumerate();
            items
                .filter(|(i, _)| links >> i & 1 == 1)
                .map(|(_, item)| *item)
                .collect()
        };
        for (links, chosen) in (0..1u32 << ITEMS.len()).flat_map(|l| [(l, false), (l, true)]) {
            let linked = layout(links);
            let f = Root::new();
            // Chosen in the home's own settings, or in the source's where the home links them.
            let own_settings = !linked.contains(&"settings.json");
            let native = ClaudeHome::at(f.native())
                .dir("projects")
                .dir("file-history")
                .dir("agent-memory/reviewer")
                .dir("plugins")
                .claude_md("be brief")
                .file(
                    "rules/rust.md",
                    "---\npaths:\n  - \"**/*.rs\"\n---\nno unwrap",
                )
                .settings(if chosen && !own_settings {
                    r#"{"model": "opus", "apiKeyHelper": "/k", "autoMemoryDirectory": "/m"}"#
                } else {
                    r#"{"model": "opus", "apiKeyHelper": "/k"}"#
                })
                .into_path();
            let max = f.home("max").linked(&native, &linked);
            let max = if chosen && own_settings {
                max.settings(r#"{"autoMemoryDirectory": "/m"}"#)
            } else {
                max
            }
            .into_path();
            let member = named("max", &max);
            let accounts = vec![Account::default_for(CLAUDE), member.clone()];
            let sharing = sharing_from(Account::default_for(CLAUDE));

            let checks = sharing_checks(&accounts, &f.env, &sharing);
            let plan = share::plan(&sharing, &member, &[], Some(&f.root), &f.env);
            let shared = plan.shared();

            let case = format!("{linked:?}, chosen {chosen}:\n{checks:#?}\n{plan:#?}\n{shared:#?}");
            let said = |account: &str, part: &str| {
                checks
                    .iter()
                    .any(|c| c.account.as_deref() == Some(account) && c.message.contains(part))
            };
            let variable = shared
                .env
                .iter()
                .any(|(name, _)| name == share::MEMORY_DIR_VAR);
            let has = |item: &str| linked.contains(&item);

            // Sessions: an account whose `projects` is its own is told it does not see them.
            let apart = said("claude:max", "does not see the sessions");
            assert_eq!(apart, !has("projects"), "{case}");
            assert_eq!(apart, plan.items.unlinked(Id::Projects), "{case}");
            assert_eq!(
                said("claude:max", "but not file-history"),
                has("projects") && !has("file-history"),
                "{case}"
            );
            // Agent memory: warned about exactly where a launch through the linked `projects`
            // does not set the variable.
            let unredirected = said("claude:max", "so a launch does not redirect memory");
            assert_eq!(
                unredirected,
                has("projects") && !has("agent-memory") && chosen,
                "{case}"
            );
            assert_eq!(
                unredirected,
                plan.items.linked(Id::Projects)
                    && plan.items.unlinked(Id::AgentMemory)
                    && plan.agent_memory.is_none(),
                "{case}"
            );
            // The variable goes with a linked `projects`, unless a settings file chose the
            // location; nothing else is added to the launch.
            assert_eq!(variable, has("projects") && !chosen, "{case}");
            assert!(!(unredirected && variable), "{case}");
            assert!(shared.unset.is_empty(), "{case}");
            assert!(
                shared.notices.iter().all(|n| n.starts_with("warning:")),
                "{case}"
            );

            // What the home does not link is what it is told it does not share, in one
            // notice, in the catalog's order: `file-history` and `agent-memory` go with a
            // linked `projects` (above), and `settings.json` is not for linking while the
            // source's sets authentication; `plugins` is never linked here.
            let expected: Vec<&str> = [
                ("projects", !has("projects")),
                ("file-history", !has("projects") && !has("file-history")),
                ("CLAUDE.md", !has("CLAUDE.md")),
                ("plugins", true),
                ("rules", !has("rules")),
                ("agent-memory", !has("projects") && !has("agent-memory")),
            ]
            .into_iter()
            .filter(|(_, unlinked)| *unlinked)
            .map(|(item, _)| item)
            .collect();
            let notices: Vec<&Check> = checks
                .iter()
                .filter(|c| {
                    c.account.as_deref() == Some("claude:max")
                        && c.message.starts_with("does not share ")
                })
                .collect();
            assert_eq!(notices.len(), 1, "{case}");
            let notice = &notices[0].message;
            assert!(
                notice.starts_with(&format!(
                    "does not share {} with claude:default",
                    expected.join(", ")
                )),
                "{case}"
            );
            assert!(notice.contains("; to share an item: ln -s "), "{case}");
            // A member without a `settings.json` of its own needs one, said in the same
            // notice, unless it was given one (here: to choose the memory location).
            assert_eq!(
                notice.contains("needs one of its own"),
                !has("settings.json") && !chosen,
                "{case}"
            );

            // Settings: read whole through a link, and the launch and the checks say so.
            let through_link = said("claude:max", "reads them through the link");
            assert_eq!(through_link, has("settings.json"), "{case}");
            assert_eq!(
                through_link,
                plan.notices
                    .iter()
                    .any(|n| n.contains("reads the authentication settings of claude:default")),
                "{case}"
            );
            // Nothing is said of the source.
            assert!(
                !checks
                    .iter()
                    .any(|c| c.account.as_deref() == Some("claude:default")),
                "{case}"
            );
        }
    }

    /// R11, R18: installs recorded through a member's `plugins` link make that link one every
    /// account depends on. Not said without such an install, for a `plugins` of the member's
    /// own, or when the list is not recognized.
    #[test]
    fn a_plugins_link_that_installs_go_through() {
        let f = Root::new();
        let native = f.root.join("home/.claude");
        fs::create_dir_all(native.join("plugins/cache/m")).unwrap();
        let (max, team) = (f.root.join("max"), f.root.join("team"));
        fs::create_dir_all(&max).unwrap();
        fs::create_dir_all(team.join("plugins")).unwrap();
        symlink(native.join("plugins"), max.join("plugins")).unwrap();
        let file = native.join("plugins/installed_plugins.json");
        let write = |a: &Path, b: &Path| {
            fs::write(
                &file,
                serde_json::json!({
                    "version": 2,
                    "plugins": {
                        "a@m": [
                            {"scope": "project", "installPath": native.join("plugins/cache/m/a")},
                            {"scope": "user", "installPath": a},
                        ],
                        "b@m": [{"scope": "user", "installPath": b}],
                        "c@m": [{"scope": "user", "installPath": team.join("plugins/cache/m/c")}],
                        "d@m": [{"scope": "user"}],
                    },
                })
                .to_string(),
            )
            .unwrap();
        };
        let accounts = vec![
            Account::default_for(CLAUDE),
            named("max", &max),
            named("team", &team),
        ];
        let sharing = sharing_from(Account::default_for(CLAUDE));
        // `team`'s `plugins` is its own: not a link to keep, and the one item it does not
        // share.
        let team_notice = format!(
            "does not share plugins with claude:default; to share an item: ln -s {}/<item> \
             {}/<item>",
            native.display(),
            team.display()
        );
        let team_notice = (Some("claude:team"), team_notice.as_str());
        write(
            &native.join("plugins/cache/m/a"),
            &native.join("plugins/cache/m/b"),
        );
        assert_eq!(
            messages(&sharing_checks(&accounts, &f.env, &sharing)),
            [team_notice]
        );

        write(
            &max.join("plugins/cache/m/a"),
            &native.join("plugins/cache/m/b"),
        );
        assert_eq!(
            messages(&sharing_checks(&accounts, &f.env, &sharing)),
            [
                (
                    Some("claude:max"),
                    format!(
                        "{} must stay a symlink: 1 plugin of claude:default is installed \
                         through it and stops loading for every account without it",
                        max.join("plugins").display()
                    )
                    .as_str()
                ),
                team_notice,
            ]
        );
        assert_eq!(
            shapes(&sharing_checks(&accounts, &f.env, &sharing))[0],
            "<> must stay a symlink: 1 plugin of claude:default is installed through it and \
             stops loading for every account without it"
        );
        write(
            &max.join("plugins/cache/m/a"),
            &max.join("plugins/cache/m/b"),
        );
        let got = sharing_checks(&accounts, &f.env, &sharing);
        assert!(
            got[0].message.ends_with(
                "2 plugins of claude:default are installed through it and stop loading for \
                 every account without it"
            ),
            "{got:?}"
        );
        assert_eq!(got.len(), 2);

        // Another directory whose name begins the same way is not below `plugins`.
        write(
            &f.root.join("max/plugins-old/a"),
            &native.join("plugins/cache/m/b"),
        );
        assert_eq!(
            messages(&sharing_checks(&accounts, &f.env, &sharing)),
            [team_notice]
        );

        // A list that is not recognized is nothing for the checks to say: the card reports
        // it (R22).
        fs::write(&file, r#"{"version": 1}"#).unwrap();
        assert_eq!(
            messages(&sharing_checks(&accounts, &f.env, &sharing)),
            [team_notice]
        );
    }

    /// R11, R18: a member is told in one notice which items of the source it does not link,
    /// in the catalog's order, with the link that would share each; not sharing `projects`
    /// means not seeing the source's sessions (R16). Opted-out accounts and the source are
    /// not members. Nothing is said once everything is linked.
    #[test]
    fn a_member_is_told_what_it_does_not_share_in_one_notice() {
        let f = Root::new();
        let native = f.root.join("home/.claude");
        fs::write(native.join("CLAUDE.md"), "x").unwrap();
        fs::create_dir_all(native.join("projects")).unwrap();
        fs::create_dir_all(native.join("skills")).unwrap();
        fs::create_dir_all(native.join("agents")).unwrap();
        let (max, solo) = (f.root.join("max"), f.root.join("solo"));
        for home in [&max, &solo] {
            fs::create_dir_all(home).unwrap();
            symlink(native.join("CLAUDE.md"), home.join("CLAUDE.md")).unwrap();
            symlink(native.join("skills"), home.join("skills")).unwrap();
        }
        let accounts = vec![
            Account::default_for(CLAUDE),
            named("max", &max),
            named("solo", &solo),
        ];
        let sharing = sharing_from(Account::default_for(CLAUDE));
        let got = sharing_checks(&accounts, &f.env, &sharing);
        assert_eq!(
            messages(&got),
            [(
                Some("claude:max"),
                not_sharing(&native, &max, "projects, agents").as_str()
            )]
        );
        // The store and both ends of the link to make are paths; the rest stays readable.
        assert_eq!(
            shapes(&got),
            [
                "does not share projects, agents with claude:default: it does not see the \
                 sessions in <> and cannot resume them; to share an item: ln -s <> <>"
            ]
        );
        // Without `projects` among them, no word about sessions.
        symlink(native.join("projects"), max.join("projects")).unwrap();
        assert_eq!(
            messages(&sharing_checks(&accounts, &f.env, &sharing)),
            [(
                Some("claude:max"),
                format!(
                    "does not share agents with claude:default; to share an item: ln -s \
                     {}/<item> {}/<item>",
                    native.display(),
                    max.display()
                )
                .as_str()
            )]
        );
        symlink(native.join("agents"), max.join("agents")).unwrap();
        assert_eq!(sharing_checks(&accounts, &f.env, &sharing), []);
    }

    /// R11, R18: a `settings.json` the source sets authentication in is not for linking
    /// (`setup` leaves it out): a member without one of its own is told it needs one, in the
    /// notice of what it does not share, by key name; one that has its own is told nothing of
    /// it; without authentication in the source's, the file is an item to link like any.
    #[test]
    fn a_member_without_settings_needs_its_own_when_the_sources_set_authentication() {
        let f = Root::new();
        let native = f.root.join("home/.claude");
        fs::write(
            native.join("settings.json"),
            r#"{"apiKeyHelper": "/k", "model": "x", "env": {"ANTHROPIC_AUTH_TOKEN": "t"}}"#,
        )
        .unwrap();
        let max = f.root.join("max");
        fs::create_dir_all(&max).unwrap();
        let accounts = vec![Account::default_for(CLAUDE), named("max", &max)];
        let sharing = sharing_from(Account::default_for(CLAUDE));
        let got = sharing_checks(&accounts, &f.env, &sharing);
        assert_eq!(
            messages(&got),
            [(
                Some("claude:max"),
                "settings.json is not shared: that of claude:default sets authentication \
                 settings (apiKeyHelper, env.ANTHROPIC_AUTH_TOKEN), so this account needs one \
                 of its own"
            )]
        );
        // The keys are read from a file: each is a piece of its own.
        assert_eq!(
            shapes(&got),
            [
                "settings.json is not shared: that of claude:default sets authentication \
                 settings ({}, {}), so this account needs one of its own"
            ]
        );
        // With other items not shared, the same notice carries it.
        fs::write(native.join("CLAUDE.md"), "x").unwrap();
        let got = sharing_checks(&accounts, &f.env, &sharing);
        assert_eq!(
            messages(&got),
            [(
                Some("claude:max"),
                format!(
                    "does not share CLAUDE.md with claude:default; to share an item: ln -s \
                     {}/<item> {}/<item>; settings.json is not shared: that of claude:default \
                     sets authentication settings (apiKeyHelper, env.ANTHROPIC_AUTH_TOKEN), so \
                     this account needs one of its own",
                    native.display(),
                    max.display()
                )
                .as_str()
            )]
        );
        assert_eq!(
            shapes(&got),
            [
                "does not share CLAUDE.md with claude:default; to share an item: ln -s <> <>; \
                 settings.json is not shared: that of claude:default sets authentication \
                 settings ({}, {}), so this account needs one of its own"
            ]
        );
        // A `settings.json` of its own: nothing more to say about it.
        fs::write(max.join("settings.json"), r#"{"model": "y"}"#).unwrap();
        let got = sharing_checks(&accounts, &f.env, &sharing);
        assert_eq!(
            shapes(&got),
            ["does not share CLAUDE.md with claude:default; to share an item: ln -s <> <>"]
        );
        // Without authentication in the source's, the file is an item like the others.
        fs::remove_file(max.join("settings.json")).unwrap();
        fs::write(native.join("settings.json"), r#"{"model": "x"}"#).unwrap();
        let got = sharing_checks(&accounts, &f.env, &sharing);
        assert_eq!(
            shapes(&got),
            [
                "does not share settings.json, CLAUDE.md with claude:default; to share an \
                 item: ln -s <> <>"
            ]
        );
    }

    #[test]
    fn shared_projects_need_cleanup_period_everywhere() {
        let f = Root::new();
        let native = f.root.join("home/.claude");
        fs::create_dir_all(native.join("projects")).unwrap();
        let (max, team, solo) = (f.root.join("max"), f.root.join("team"), f.root.join("solo"));
        for home in [&max, &team, &solo] {
            fs::create_dir_all(home).unwrap();
        }
        symlink(native.join("projects"), max.join("projects")).unwrap();
        symlink(native.join("projects"), team.join("projects")).unwrap();
        fs::create_dir_all(solo.join("projects")).unwrap();
        // One real settings.json shared through symlinks (like the real machine) ...
        fs::write(
            native.join("settings.json"),
            r#"{"cleanupPeriodDays": 365}"#,
        )
        .unwrap();
        symlink(native.join("settings.json"), max.join("settings.json")).unwrap();
        // ... team's own lacks the key; solo does not share its store.
        fs::write(team.join("settings.json"), r#"{"model": "x"}"#).unwrap();
        let accounts = vec![
            Account::default_for(CLAUDE),
            named("max", &max),
            named("team", &team),
            named("solo", &solo),
        ];
        let stores = index::stores(&accounts, &f.env);
        let got = run(&accounts, &f.env, &stores);
        assert_eq!(
            messages(&got),
            [(
                None,
                format!(
                    "projects {} is shared by claude:default, claude:max, claude:team, but \
                     claude:team has no cleanupPeriodDays in settings.json (the default 30-day \
                     cleanup deletes everyone's sessions)",
                    native.join("projects").display()
                )
                .as_str()
            )]
        );
        assert_eq!(
            shapes(&got),
            [
                "projects <> is shared by claude:default, claude:max, claude:team, but \
                 claude:team has no cleanupPeriodDays in settings.json (the default 30-day \
                 cleanup deletes everyone's sessions)"
            ]
        );

        // Codex accounts sharing a `sessions` store are no concern of cleanupPeriodDays.
        let shared_sessions = f.root.join("codex-sessions");
        fs::create_dir_all(&shared_sessions).unwrap();
        let mut with_codex = accounts.clone();
        for name in ["cx1", "cx2"] {
            let home = f.root.join(name);
            fs::create_dir_all(&home).unwrap();
            symlink(&shared_sessions, home.join("sessions")).unwrap();
            with_codex.push(codex(name, &home));
        }
        let with_codex_stores = index::stores(&with_codex, &f.env);
        assert!(
            with_codex_stores
                .iter()
                .any(|s| s.provider == CODEX && s.accounts.len() == 2)
        );
        assert_eq!(run(&with_codex, &f.env, &with_codex_stores), got);

        // With the key everywhere, no warning.
        fs::write(team.join("settings.json"), r#"{"cleanupPeriodDays": 99}"#).unwrap();
        assert_eq!(run(&accounts, &f.env, &stores), []);
        // A malformed or missing settings.json counts as lacking it.
        fs::write(team.join("settings.json"), "{").unwrap();
        fs::remove_file(max.join("settings.json")).unwrap();
        let got = run(&accounts, &f.env, &stores);
        assert!(
            got[0]
                .message
                .contains("but claude:max, claude:team have no"),
            "{got:?}"
        );
    }
}
