//! Read-only problem checks shown in the accounts view (SPEC R11). Login state is checked by
//! the caller from identities; everything here looks only at the environment snapshot and
//! the file system.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::Env;
use crate::home_items::{self, Id, Source};
use crate::index::Store;
use crate::registry::{Account, CLAUDE, Home, Sharing};
use crate::share::{self, Installs};

/// One problem worth a warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// `provider:name` of the account concerned; `None` for machine-wide problems.
    pub account: Option<String>,
    pub message: String,
}

pub const API_KEY_VAR: &str = "ANTHROPIC_API_KEY";

/// All file-system and environment checks, in a stable order: environment, then per account
/// (registry order), then shared stores. `stores` is [`crate::index::stores`] of `accounts`.
pub fn run(accounts: &[Account], env: &Env, stores: &[Store]) -> Vec<Check> {
    let mut checks = Vec::new();
    if env.get(API_KEY_VAR).is_some_and(|v| !v.is_empty()) {
        checks.push(Check {
            account: None,
            message: format!("{API_KEY_VAR} is set: it overrides every account's /login"),
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
                message: format!("home {} does not exist", home.display()),
            });
            continue;
        }
        for (link, target) in dangling_symlinks(&home) {
            checks.push(Check {
                account: Some(qualified.clone()),
                message: format!(
                    "dangling symlink {} -> {}",
                    link.display(),
                    target.display()
                ),
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
                message: format!(
                    "projects {} is shared by {}, but {} {} no cleanupPeriodDays in settings.json \
                     (the default 30-day cleanup deletes everyone's sessions)",
                    store.path.display(),
                    store.accounts.join(", "),
                    missing.join(", "),
                    if missing.len() == 1 { "has" } else { "have" },
                ),
            });
        }
    }
    checks
}

/// Shared configuration (R11, R18): a source account that is not listed or whose home is
/// missing; members whose home shares some but not all instruction items with the source
/// through symlinks (those load twice); members whose `.claude.json`, `history.jsonl` or
/// `sessions` is a link to another account's (checked even without the source's home);
/// members that do not share `projects` with the source, or share it but not `file-history`,
/// or not `agent-memory` where a settings file chooses `autoMemoryDirectory` (a launch then
/// does not redirect memory); members whose `plugins` link the source's installs go through; members
/// that read the source's authentication settings through a linked `settings.json`; rules of
/// the source limited to paths, which claude ignores where the rules are injected;
/// authentication keys of the source's settings, which are withheld where settings are
/// injected; enabled plugins without an install path that exists, and an
/// `installed_plugins.json` whose format is not recognized.
pub fn sharing(accounts: &[Account], env: &Env, sharing: &Sharing) -> Vec<Check> {
    let mut checks = Vec::new();
    let Some(source) = &sharing.source else {
        return checks;
    };
    let name = source.qualified();
    if !accounts.iter().any(|a| a.qualified() == name) {
        checks.push(Check {
            account: Some(name),
            message: "[share.claude] from names this account, which is not registered".into(),
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
            message: format!(
                "home {home} of the shared configuration source does not exist: nothing is shared"
            ),
        });
    }
    let installs = from
        .as_ref()
        .map(|from| share::installed_plugins(from.home()));
    // A settings file of the source that cannot be read chooses nothing and withholds nothing.
    let unread = Map::new();
    let settings = from
        .as_ref()
        .and_then(|from| from.settings().ok())
        .unwrap_or(&unread);
    let withheld = from.as_ref().map(Source::withheld).unwrap_or_default();
    let (projects, settings_file) = (Id::Projects.name(), Id::Settings.name());
    let mut rules_injected = false;
    // Members whose `settings.json` is the source's, and members that get settings at launch.
    let (mut settings_linked, mut settings_injected) = (0, 0);
    for account in accounts {
        if sharing.source_for(account).is_none() {
            continue;
        }
        let Some(home) = account.home_dir(env).filter(|d| d.is_dir()) else {
            continue;
        };
        // What the home has of the source's items, looked at once.
        let items = from.as_ref().map(|from| from.relate(&home));
        if let Some(items) = &items {
            if !items.linked(Id::Plugins)
                && let Installs::Unrecognized(path) = share::installed_plugins(&home)
            {
                checks.push(Check {
                    account: Some(account.qualified()),
                    message: format!(
                        "{} is not in a recognized format: plugins from {name} are not shared \
                         with this account",
                        path.display()
                    ),
                });
            }
            let instructions = items.instructions();
            if instructions.partial() {
                checks.push(Check {
                    account: Some(account.qualified()),
                    message: format!(
                        "shares {} with {name} through symlinks but not {}: the shared ones \
                         load twice (with the injected --add-dir); link the others too, or none",
                        instructions.linked.join(", "),
                        instructions.unlinked.join(", ")
                    ),
                });
            }
            rules_injected |= instructions.unlinked.contains(&Id::Rules.name());
        }
        // What stays per account (R18), when it is a link to another account's: checked
        // whether or not the source's home exists.
        for link in home_items::linked_elsewhere(account, accounts, env) {
            checks.push(Check {
                account: Some(account.qualified()),
                message: format!(
                    "{} is a symlink to that of {}: {}; each account needs its own",
                    link.name, link.account, link.breaks
                ),
            });
        }
        let (Some(from), Some(items)) = (&from, &items) else {
            continue;
        };
        // `<home>/<item> -> <source home>/<item>`: the link that would share the item.
        let link = |id: Id| {
            format!(
                "{} -> {}",
                id.at(&home).display(),
                id.at(from.home()).display()
            )
        };
        // An item that goes with `projects` and is not linked with it: what the account
        // loses (`why` adds when), and the link to make.
        let apart = |id: Id, why: &str| {
            format!(
                "shares {projects} with {name} through a symlink but not {}{why}: {}; link it \
                 too ({})",
                id.name(),
                id.item().loses.unwrap_or_default(),
                link(id)
            )
        };
        if items.linked(Id::Projects) {
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
        } else if items.unlinked(Id::Projects) {
            // Not an error: memory is still shared by injection (R18). Sessions are not.
            checks.push(Check {
                account: Some(account.qualified()),
                message: format!(
                    "not sharing sessions with {name}: it does not see the sessions in {} and \
                     cannot resume them; link {projects} to share them ({})",
                    Id::Projects.at(from.home()).display(),
                    link(Id::Projects)
                ),
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
                    message: format!(
                        "{} must stay a symlink: {held} {plugins} of {name} {are} installed \
                         through it and {stop} loading for every account without it",
                        through.display()
                    ),
                });
            }
        }
        // A home that links the source's `settings.json` reads all of it: nothing of it is
        // withheld there, as it is from the settings injected at launch (R18).
        if items.linked(Id::Settings) {
            settings_linked += 1;
            if !withheld.is_empty() {
                checks.push(Check {
                    account: Some(account.qualified()),
                    message: format!(
                        "{settings_file} is that of {name} through a symlink, and it sets \
                         authentication settings ({}): this account reads them through the \
                         link; remove the link (the rest is then injected at launch), or move \
                         them out of that {settings_file}",
                        withheld.join(", ")
                    ),
                });
            }
        } else {
            settings_injected += 1;
        }
    }
    let (Some(from), Some(installs)) = (&from, installs) else {
        return checks;
    };
    if rules_injected {
        let scoped = share::scoped_rules(&Id::Rules.at(from.home()));
        if !scoped.is_empty() {
            let files: Vec<String> = scoped.iter().map(|p| p.display().to_string()).collect();
            checks.push(Check {
                account: Some(name.clone()),
                message: format!(
                    "rules limited to paths are not applied in accounts that get the rules at \
                     launch (claude ignores `paths` in an added directory): {}",
                    files.join(", ")
                ),
            });
        }
    }
    // Withheld where settings are injected: not said when every member links the file, and
    // said of the others when some do (those are named above).
    if !withheld.is_empty() && (settings_linked == 0 || settings_injected > 0) {
        let message = if settings_linked == 0 {
            format!(
                "settings keys withheld from shared configuration (authentication is never \
                 shared): {}",
                withheld.join(", ")
            )
        } else {
            format!(
                "settings keys withheld from the accounts that get settings at launch \
                 (authentication is never injected): {}",
                withheld.join(", ")
            )
        };
        checks.push(Check {
            account: Some(name.clone()),
            message,
        });
    }
    let plugins = share::enabled_plugins(settings);
    match installs {
        Installs::Unrecognized(path) => checks.push(Check {
            account: Some(name),
            message: format!(
                "{} is not in a recognized format: plugins are not shared",
                path.display()
            ),
        }),
        installs => {
            for plugin in plugins {
                if installs.install_path(&plugin).is_none() {
                    checks.push(Check {
                        account: Some(name.clone()),
                        message: format!(
                            "enabled plugin {plugin} has no user install whose path exists: \
                             it is not shared"
                        ),
                    });
                }
            }
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

    use super::sharing as sharing_checks;
    use super::*;
    use crate::index;
    use crate::registry::CODEX;
    use crate::test_homes::{ClaudeHome, Root};

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
        f.env.insert(API_KEY_VAR.into(), String::new());
        assert_eq!(run(&[], &f.env, &[]), []);
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

    /// R11, R18: nothing to say about a clean shared setup.
    #[test]
    fn a_clean_shared_setup_has_no_problems() {
        let f = Root::new();
        let native = f.root.join("home/.claude");
        fs::write(native.join("CLAUDE.md"), "x").unwrap();
        let max = f.root.join("max");
        fs::create_dir_all(&max).unwrap();
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
        let got = sharing_checks(&accounts[..1], &f.env, &sharing_from(named("gone", &gone)));
        assert_eq!(
            messages(&got),
            [(
                Some("claude:gone"),
                "[share.claude] from names this account, which is not registered"
            )]
        );
    }

    /// R11, R18: rules limited to paths are named when a member gets the rules at launch,
    /// not when its `rules` is the source's.
    #[test]
    fn rules_limited_to_paths_where_rules_are_injected() {
        let f = Root::new();
        let native = f.root.join("home/.claude");
        fs::create_dir_all(native.join("rules/lang")).unwrap();
        fs::write(native.join("rules/style.md"), "be terse").unwrap();
        let max = f.root.join("max");
        fs::create_dir_all(&max).unwrap();
        let accounts = vec![Account::default_for(CLAUDE), named("max", &max)];
        let sharing = sharing_from(Account::default_for(CLAUDE));
        assert_eq!(sharing_checks(&accounts, &f.env, &sharing), []);

        fs::write(
            native.join("rules/lang/rust.md"),
            "---\npaths:\n  - \"**/*.rs\"\n---\nno unwrap",
        )
        .unwrap();
        let got = sharing_checks(&accounts, &f.env, &sharing);
        assert_eq!(
            messages(&got),
            [(
                Some("claude:default"),
                "rules limited to paths are not applied in accounts that get the rules at launch \
                 (claude ignores `paths` in an added directory): lang/rust.md"
            )]
        );
        symlink(native.join("rules"), max.join("rules")).unwrap();
        assert_eq!(sharing_checks(&accounts, &f.env, &sharing), []);
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
        for home in [&native, &max] {
            fs::write(
                home.join("settings.json"),
                r#"{"autoMemoryDirectory":"/m"}"#,
            )
            .unwrap();
            assert_eq!(
                messages(&sharing_checks(&accounts, &f.env, &sharing)),
                [(Some("claude:max"), warning.as_str())]
            );
            fs::remove_file(home.join("settings.json")).unwrap();
        }
        fs::write(max.join("settings.json"), r#"{"autoMemoryDirectory":"/m"}"#).unwrap();
        symlink(native.join("agent-memory"), max.join("agent-memory")).unwrap();
        assert_eq!(sharing_checks(&accounts, &f.env, &sharing), []);
        fs::remove_file(max.join("settings.json")).unwrap();
        assert_eq!(sharing_checks(&accounts, &f.env, &sharing), []);

        // Its own `projects`: the memory is redirected at launch as well, and the only thing
        // to say is that the account does not share sessions.
        fs::remove_file(max.join("agent-memory")).unwrap();
        fs::remove_file(max.join("projects")).unwrap();
        fs::create_dir(max.join("projects")).unwrap();
        assert_eq!(
            messages(&sharing_checks(&accounts, &f.env, &sharing)),
            [(Some("claude:max"), not_sharing(&native, &max).as_str())]
        );
    }

    /// The notice for a member `home` whose `projects` is not the one of the source `from`.
    fn not_sharing(from: &Path, home: &Path) -> String {
        format!(
            "not sharing sessions with claude:default: it does not see the sessions in {} and \
             cannot resume them; link projects to share them ({} -> {})",
            from.join("projects").display(),
            home.join("projects").display(),
            from.join("projects").display()
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
                (Some("claude:max"), not_sharing(&native, &max).as_str()),
                (Some("claude:team"), not_sharing(&native, &team).as_str()),
            ]
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
    /// settings through the link: said of that member, by key name. They are withheld only
    /// where settings are injected, and that is said only while some member gets them so.
    #[test]
    fn authentication_read_through_a_linked_settings_file() {
        let f = Root::new();
        let native = f.root.join("home/.claude");
        fs::write(
            native.join("settings.json"),
            r#"{"apiKeyHelper": "/k", "model": "x", "env": {"ANTHROPIC_AUTH_TOKEN": "t"}}"#,
        )
        .unwrap();
        let (max, team, solo) = (f.root.join("max"), f.root.join("team"), f.root.join("solo"));
        for home in [&max, &team, &solo] {
            fs::create_dir_all(home).unwrap();
        }
        symlink(native.join("settings.json"), max.join("settings.json")).unwrap();
        symlink(native.join("settings.json"), solo.join("settings.json")).unwrap();
        let sharing = sharing_from(Account::default_for(CLAUDE));
        let linked = (
            Some("claude:max"),
            "settings.json is that of claude:default through a symlink, and it sets \
             authentication settings (apiKeyHelper, env.ANTHROPIC_AUTH_TOKEN): this account \
             reads them through the link; remove the link (the rest is then injected at \
             launch), or move them out of that settings.json",
        );

        // Every member links it: nothing is withheld from anyone. `solo` opted out.
        let accounts = vec![
            Account::default_for(CLAUDE),
            named("max", &max),
            named("solo", &solo),
        ];
        assert_eq!(
            messages(&sharing_checks(&accounts, &f.env, &sharing)),
            [linked]
        );
        // `team` gets its settings at launch, without them.
        let mut mixed = accounts.clone();
        mixed.push(named("team", &team));
        assert_eq!(
            messages(&sharing_checks(&mixed, &f.env, &sharing)),
            [
                linked,
                (
                    Some("claude:default"),
                    "settings keys withheld from the accounts that get settings at launch \
                     (authentication is never injected): apiKeyHelper, env.ANTHROPIC_AUTH_TOKEN"
                ),
            ]
        );
        // Without a member that links it, as before.
        let injected = vec![Account::default_for(CLAUDE), named("team", &team)];
        assert_eq!(
            messages(&sharing_checks(&injected, &f.env, &sharing)),
            [(
                Some("claude:default"),
                "settings keys withheld from shared configuration (authentication is never \
                 shared): apiKeyHelper, env.ANTHROPIC_AUTH_TOKEN"
            )]
        );
        // Settings without authentication may be linked.
        fs::write(native.join("settings.json"), r#"{"model": "x"}"#).unwrap();
        assert_eq!(sharing_checks(&mixed, &f.env, &sharing), []);
    }

    /// R11, R18, R22: the checks and a launch read one table of how the member's home relates
    /// to the source's ([`crate::home_items`]), so for the same two homes they agree: what the
    /// accounts view says is not shared is what the launch injects, and what it says is linked
    /// the launch leaves alone. Over every combination of six links, with and without a
    /// settings file that chooses the auto-memory location.
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
            let plan = share::plan(&sharing, &member, &[], Some(&f.root), &f.env).unwrap();
            let shared = share::apply(&plan, &f.config).unwrap();

            let case = format!("{linked:?}, chosen {chosen}:\n{checks:#?}\n{plan:#?}\n{shared:#?}");
            let said = |account: &str, part: &str| {
                checks
                    .iter()
                    .any(|c| c.account.as_deref() == Some(account) && c.message.contains(part))
            };
            let option = |name: &str| shared.args.iter().any(|a| a.starts_with(name));
            let variable = shared
                .env
                .iter()
                .any(|(name, _)| name == share::MEMORY_DIR_VAR);
            let has = |item: &str| linked.contains(&item);

            // Sessions and auto-memory: an account told it does not share sessions gets the
            // source's memory location injected instead, unless a settings file chose one.
            let apart = said("claude:max", "not sharing sessions");
            assert_eq!(apart, !has("projects"), "{case}");
            assert_eq!(apart, plan.items.unlinked(Id::Projects), "{case}");
            assert_eq!(plan.memory.is_some(), apart && !chosen, "{case}");
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
            // The variable goes with the memory location remuda decides: through a linked
            // `projects`, or with the injected one unless `agent-memory` is linked itself.
            assert_eq!(
                variable,
                !chosen && (has("projects") || !has("agent-memory")),
                "{case}"
            );
            assert!(!(unredirected && variable), "{case}");

            // Instructions: `--add-dir` for what is not linked; the linked ones then load
            // twice, and the rules limited to paths are not applied.
            let instructions = plan.items.instructions();
            let unlinked = ["CLAUDE.md", "rules"]
                .iter()
                .filter(|item| !has(item))
                .count();
            assert_eq!(option("--add-dir="), unlinked > 0, "{case}");
            assert_eq!(
                option("--add-dir="),
                instructions.needs_injection(),
                "{case}"
            );
            assert_eq!(
                said("claude:max", "the shared ones load twice"),
                unlinked == 1,
                "{case}"
            );
            assert_eq!(
                said("claude:default", "rules limited to paths"),
                option("--add-dir=") && !has("rules"),
                "{case}"
            );

            // Settings: read whole through a link, and the launch says so; injected without
            // the authentication otherwise, and the accounts view names what is withheld.
            let through_link = said("claude:max", "reads them through the link");
            assert_eq!(through_link, has("settings.json"), "{case}");
            assert_eq!(
                through_link,
                plan.notices
                    .iter()
                    .any(|n| n.contains("reads the authentication settings of claude:default")),
                "{case}"
            );
            assert_eq!(plan.settings.contains_key("model"), !through_link, "{case}");
            assert_eq!(
                said("claude:default", "settings keys withheld"),
                plan.settings.contains_key("model"),
                "{case}"
            );
            assert!(!plan.settings.contains_key("apiKeyHelper"), "{case}");
            assert_eq!(option("--settings="), !plan.settings.is_empty(), "{case}");
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
        // `team`'s `plugins` is its own: not a link to keep.
        write(
            &native.join("plugins/cache/m/a"),
            &native.join("plugins/cache/m/b"),
        );
        assert_eq!(sharing_checks(&accounts, &f.env, &sharing), []);

        write(
            &max.join("plugins/cache/m/a"),
            &native.join("plugins/cache/m/b"),
        );
        assert_eq!(
            messages(&sharing_checks(&accounts, &f.env, &sharing)),
            [(
                Some("claude:max"),
                format!(
                    "{} must stay a symlink: 1 plugin of claude:default is installed through \
                     it and stops loading for every account without it",
                    max.join("plugins").display()
                )
                .as_str()
            )]
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
        assert_eq!(got.len(), 1);

        // Another directory whose name begins the same way is not below `plugins`.
        write(
            &f.root.join("max/plugins-old/a"),
            &native.join("plugins/cache/m/b"),
        );
        assert_eq!(sharing_checks(&accounts, &f.env, &sharing), []);

        // A list that is not recognized is reported as such, and that is all.
        fs::write(&file, r#"{"version": 1}"#).unwrap();
        let got = sharing_checks(&accounts, &f.env, &sharing);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(
            got[0]
                .message
                .ends_with("is not in a recognized format: plugins are not shared"),
            "{got:?}"
        );
    }

    /// R11: a member sharing some instruction items through symlinks but not all; opted-out
    /// accounts and the source are not members.
    #[test]
    fn partly_symlinked_instructions() {
        let f = Root::new();
        let native = f.root.join("home/.claude");
        fs::write(native.join("CLAUDE.md"), "x").unwrap();
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
        let got = sharing_checks(
            &accounts,
            &f.env,
            &sharing_from(Account::default_for(CLAUDE)),
        );
        assert_eq!(
            messages(&got),
            [(
                Some("claude:max"),
                "shares CLAUDE.md, skills with claude:default through symlinks but not agents: \
                 the shared ones load twice (with the injected --add-dir); link the others \
                 too, or none"
            )]
        );
        symlink(native.join("agents"), max.join("agents")).unwrap();
        assert_eq!(
            sharing_checks(
                &accounts,
                &f.env,
                &sharing_from(Account::default_for(CLAUDE))
            ),
            []
        );
    }

    /// R11, R18: a member whose own `installed_plugins.json` is not recognized gets no
    /// plugins, and is told.
    #[test]
    fn a_members_unrecognized_plugin_list() {
        let f = Root::new();
        let max = f.root.join("max");
        fs::create_dir_all(max.join("plugins")).unwrap();
        let file = max.join("plugins/installed_plugins.json");
        fs::write(&file, "[]").unwrap();
        let accounts = vec![Account::default_for(CLAUDE), named("max", &max)];
        let got = sharing_checks(
            &accounts,
            &f.env,
            &sharing_from(Account::default_for(CLAUDE)),
        );
        assert_eq!(
            messages(&got),
            [(
                Some("claude:max"),
                format!(
                    "{} is not in a recognized format: plugins from claude:default are not \
                     shared with this account",
                    file.display()
                )
                .as_str()
            )]
        );
    }

    /// R11, R18: authentication keys in the source's settings are listed as withheld.
    #[test]
    fn withheld_authentication_keys() {
        let f = Root::new();
        let native = f.root.join("home/.claude");
        fs::write(
            native.join("settings.json"),
            r#"{"apiKeyHelper": "/k", "model": "x", "env": {"ANTHROPIC_AUTH_TOKEN": "t"}}"#,
        )
        .unwrap();
        let accounts = vec![Account::default_for(CLAUDE)];
        let got = sharing_checks(
            &accounts,
            &f.env,
            &sharing_from(Account::default_for(CLAUDE)),
        );
        assert_eq!(
            messages(&got),
            [(
                Some("claude:default"),
                "settings keys withheld from shared configuration (authentication is never \
                 shared): apiKeyHelper, env.ANTHROPIC_AUTH_TOKEN"
            )]
        );
    }

    /// R11: enabled plugins without an existing user install, and an unrecognized
    /// `installed_plugins.json`.
    #[test]
    fn plugin_problems() {
        let f = Root::new();
        let native = f.root.join("home/.claude");
        fs::create_dir_all(native.join("plugins/cache/ok")).unwrap();
        fs::write(
            native.join("settings.json"),
            r#"{"enabledPlugins": {"ok@m": true, "gone@m": true, "off@m": false}}"#,
        )
        .unwrap();
        let file = native.join("plugins/installed_plugins.json");
        fs::write(
            &file,
            serde_json::json!({
                "version": 2,
                "plugins": {
                    "ok@m": [{"scope": "user", "installPath": native.join("plugins/cache/ok")}],
                    "gone@m": [{"scope": "user", "installPath": native.join("plugins/cache/gone")}],
                },
            })
            .to_string(),
        )
        .unwrap();
        let accounts = vec![Account::default_for(CLAUDE)];
        let sharing = sharing_from(Account::default_for(CLAUDE));
        assert_eq!(
            messages(&sharing_checks(&accounts, &f.env, &sharing)),
            [(
                Some("claude:default"),
                "enabled plugin gone@m has no user install whose path exists: it is not shared"
            )]
        );
        fs::write(&file, r#"{"version": 1}"#).unwrap();
        assert_eq!(
            messages(&sharing_checks(&accounts, &f.env, &sharing)),
            [(
                Some("claude:default"),
                format!(
                    "{} is not in a recognized format: plugins are not shared",
                    file.display()
                )
                .as_str()
            )]
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
