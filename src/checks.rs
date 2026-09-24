//! Read-only problem checks shown in the accounts view (SPEC R11). Login state is checked by
//! the caller from identities; everything here looks only at the environment snapshot and
//! the file system.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::Env;
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
/// through symlinks (those load twice); authentication keys of the source's settings, which
/// are withheld; enabled plugins without an install path that exists, and an
/// `installed_plugins.json` whose format is not recognized.
pub fn sharing(accounts: &[Account], env: &Env, sharing: &Sharing) -> Vec<Check> {
    let mut checks = Vec::new();
    let Some(source) = &sharing.source else {
        return checks;
    };
    let name = source.qualified();
    if !accounts.iter().any(|a| a.qualified() == name) {
        checks.push(Check {
            account: None,
            message: format!("[share.claude] from = {name}: no such account"),
        });
        return checks;
    }
    let Some(from) = source.home_dir(env).filter(|d| d.is_dir()) else {
        let home = source
            .home_dir(env)
            .map_or(source.home.to_string(), |d| d.display().to_string());
        checks.push(Check {
            account: Some(name),
            message: format!(
                "home {home} of the shared configuration source does not exist: nothing is shared"
            ),
        });
        return checks;
    };
    for account in accounts {
        if sharing.source_for(account).is_none() {
            continue;
        }
        let Some(home) = account.home_dir(env).filter(|d| d.is_dir()) else {
            continue;
        };
        if !share::resolves_to(&home.join("plugins"), &from.join("plugins"))
            && let Installs::Unrecognized(path) = share::installed_plugins(&home)
        {
            checks.push(Check {
                account: Some(account.qualified()),
                message: format!(
                    "{} is not in a recognized format: plugins from {name} are not shared with \
                     this account",
                    path.display()
                ),
            });
        }
        let items = share::instructions(&from, &home);
        if items.partial() {
            checks.push(Check {
                account: Some(account.qualified()),
                message: format!(
                    "shares {} with {name} through symlinks but not {}: the shared ones load \
                     twice (with the injected --add-dir)",
                    items.shared.join(", "),
                    items.missing.join(", ")
                ),
            });
        }
    }
    let settings = share::read_settings(&from.join("settings.json")).unwrap_or_default();
    let withheld = share::withheld(&settings);
    if !withheld.is_empty() {
        checks.push(Check {
            account: Some(name.clone()),
            message: format!(
                "settings keys withheld from shared configuration (authentication is never \
                 shared): {}",
                withheld.join(", ")
            ),
        });
    }
    let plugins = share::enabled_plugins(&settings);
    match share::installed_plugins(&from) {
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
    fs::read(home.join("settings.json"))
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

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        env: Env,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        fs::create_dir_all(home.join(".claude")).unwrap();
        let env: Env = [("HOME".to_string(), home.display().to_string())].into();
        Fixture {
            _dir: dir,
            root,
            env,
        }
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
        let f = fixture();
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
        let mut f = fixture();
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
        let f = fixture();
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
        let f = fixture();
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
        let f = fixture();
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
        let f = fixture();
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
        let f = fixture();
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
            [(None, "[share.claude] from = claude:gone: no such account")]
        );
    }

    /// R11: a member sharing some instruction items through symlinks but not all; opted-out
    /// accounts and the source are not members.
    #[test]
    fn partly_symlinked_instructions() {
        let f = fixture();
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
                 the shared ones load twice (with the injected --add-dir)"
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
        let f = fixture();
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
        let f = fixture();
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
        let f = fixture();
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
        let f = fixture();
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
