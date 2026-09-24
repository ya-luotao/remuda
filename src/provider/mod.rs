//! Agent providers and what each one can do (SPEC R4): the isolation variable, the native
//! login's directory, the session store, launch arguments and login. Everything that differs
//! between claude and codex is decided here or in a module this points to.

pub mod app_server;
pub mod codex;

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Env, launch, paths};

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    #[default]
    Claude,
    Codex,
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl Provider {
    /// Every provider, in the order accounts are listed.
    pub const ALL: [Provider; 2] = [Provider::Claude, Provider::Codex];

    /// The name used in `config.toml` and in `provider:name`.
    pub fn name(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        }
    }

    pub fn parse(name: &str) -> Option<Provider> {
        Provider::ALL.into_iter().find(|p| p.name() == name)
    }

    /// The agent's executable, looked up on `PATH`.
    pub fn program(self) -> &'static str {
        self.name()
    }

    /// The variable that selects an account's home: set to the home, or removed for the native
    /// login (R2, R4).
    pub fn home_var(self) -> &'static str {
        match self {
            Provider::Claude => launch::CONFIG_DIR_VAR,
            Provider::Codex => CODEX_HOME_VAR,
        }
    }

    /// The native login's directory: `$HOME/.claude` or `$HOME/.codex`.
    pub fn native_home(self, env: &Env) -> Option<PathBuf> {
        match self {
            Provider::Claude => paths::native_claude_home(env),
            Provider::Codex => paths::user_home(env).map(|h| h.join(".codex")),
        }
    }

    /// Whether the implicit `<provider>:default` is listed (R1, R17): claude's always; codex's
    /// only when `$HOME/.codex` exists or `codex` is on `PATH`.
    pub fn default_listed(self, env: &Env) -> bool {
        match self {
            Provider::Claude => true,
            Provider::Codex => {
                self.native_home(env).is_some_and(|d| d.is_dir())
                    || launch::find_on_path(self.program(), env.get("PATH").map(String::as_str))
                        .is_ok()
            }
        }
    }

    /// The directory under a home that holds its sessions: claude's `projects` (R8), codex's
    /// `sessions` (R17).
    pub fn store_dir(self) -> &'static str {
        match self {
            Provider::Claude => "projects",
            Provider::Codex => "sessions",
        }
    }

    /// Whether a directory looks like this provider's home (R14): `None` when it does, else
    /// what is missing.
    pub fn home_warning(self, dir: &Path) -> Option<String> {
        let home = dir.display();
        match self {
            Provider::Claude => {
                (!dir.join(".claude.json").exists() && !dir.join("projects").is_dir()).then(|| {
                    format!(
                        "{home} has neither .claude.json nor projects/; it does not look like a \
                     Claude home (registered anyway)"
                    )
                })
            }
            // `auth.json` is credentials: not even looked at (R4).
            Provider::Codex => (!dir.join("config.toml").exists()
                && !dir.join("sessions").is_dir())
            .then(|| {
                format!(
                    "{home} has neither config.toml nor sessions/; it does not look like a Codex \
                     home (registered anyway)"
                )
            }),
        }
    }

    /// The login command (R4): `claude auth login [--email <email>]`, `codex login` (which
    /// takes no email).
    pub fn login_args(self, email: Option<String>) -> Result<Vec<String>, String> {
        match self {
            Provider::Claude => {
                let mut args = vec!["auth".to_string(), "login".to_string()];
                if let Some(email) = email {
                    args.extend(["--email".to_string(), email]);
                }
                Ok(args)
            }
            Provider::Codex => match email {
                Some(_) => Err("`codex login` takes no email".to_string()),
                None => Ok(vec!["login".to_string()]),
            },
        }
    }

    /// Usage limits can be read (R10); codex: not supported (R4).
    pub fn has_usage(self) -> bool {
        self == Provider::Claude
    }

    /// Running sessions can be listed (R7); codex has no machine-readable source (R4).
    pub fn has_live(self) -> bool {
        self == Provider::Claude
    }

    /// Arguments that resume (or fork) session `id` in `cwd` (R6, R16, R17). Claude finds the
    /// directory from the child's working directory; codex is also told with `-C`.
    pub fn resume_args(self, id: &str, cwd: &Path, fork: bool) -> Vec<String> {
        match self {
            Provider::Claude => {
                let mut args = vec!["--resume".to_string(), id.to_string()];
                if fork {
                    args.push("--fork-session".to_string());
                }
                args
            }
            Provider::Codex => vec![
                if fork { "fork" } else { "resume" }.to_string(),
                id.to_string(),
                "-C".to_string(),
                cwd.display().to_string(),
            ],
        }
    }

    /// Arguments that start a new session in `dir` (R16, R17): claude runs in `dir` and takes
    /// an optional `-n <name>`; codex is told the directory with `-C` and has no name flag.
    pub fn new_session_args(self, dir: &Path, name: Option<&str>) -> Vec<String> {
        match self {
            Provider::Claude => match name {
                Some(name) => vec!["-n".to_string(), name.to_string()],
                None => Vec::new(),
            },
            Provider::Codex => vec!["-C".to_string(), dir.display().to_string()],
        }
    }

    /// Whether sessions of this provider can be given a name when they start.
    pub fn names_sessions(self) -> bool {
        self == Provider::Claude
    }
}

/// Codex's home variable (R4).
pub const CODEX_HOME_VAR: &str = "CODEX_HOME";

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn names_round_trip() {
        for p in Provider::ALL {
            assert_eq!(Provider::parse(p.name()), Some(p));
            assert_eq!(p.to_string(), p.name());
        }
        assert_eq!(Provider::parse("gemini"), None);
        assert_eq!(Provider::parse("Claude"), None);
    }

    #[test]
    fn isolation_variables() {
        assert_eq!(Provider::Claude.home_var(), "CLAUDE_CONFIG_DIR");
        assert_eq!(Provider::Codex.home_var(), "CODEX_HOME");
    }

    /// R17: `codex:default` is listed when `$HOME/.codex` exists or `codex` is on PATH.
    #[test]
    fn codex_default_is_listed_only_when_codex_is_around() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let bin = dir.path().join("bin");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let mut env: Env = [
            ("HOME".to_string(), home.display().to_string()),
            ("PATH".to_string(), bin.display().to_string()),
        ]
        .into();
        assert!(Provider::Claude.default_listed(&env));
        assert!(!Provider::Codex.default_listed(&env));

        fs::create_dir(home.join(".codex")).unwrap();
        assert!(Provider::Codex.default_listed(&env));
        fs::remove_dir(home.join(".codex")).unwrap();

        fs::write(bin.join("codex"), "").unwrap();
        fs::set_permissions(bin.join("codex"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Provider::Codex.default_listed(&env));
        env.remove("PATH");
        assert!(!Provider::Codex.default_listed(&env));
    }

    #[test]
    fn login_commands() {
        assert_eq!(
            Provider::Claude.login_args(Some("a@b".into())).unwrap(),
            ["auth", "login", "--email", "a@b"]
        );
        assert_eq!(
            Provider::Claude.login_args(None).unwrap(),
            ["auth", "login"]
        );
        assert_eq!(Provider::Codex.login_args(None).unwrap(), ["login"]);
        assert!(Provider::Codex.login_args(Some("a@b".into())).is_err());
    }

    #[test]
    fn launch_arguments() {
        let cwd = Path::new("/w d");
        let id = "019c1e08-e4f6-7d70-a129-38ec744a3f3c";
        assert_eq!(
            Provider::Claude.resume_args(id, cwd, false),
            ["--resume", id]
        );
        assert_eq!(
            Provider::Claude.resume_args(id, cwd, true),
            ["--resume", id, "--fork-session"]
        );
        assert_eq!(
            Provider::Codex.resume_args(id, cwd, false),
            ["resume", id, "-C", "/w d"]
        );
        assert_eq!(
            Provider::Codex.resume_args(id, cwd, true),
            ["fork", id, "-C", "/w d"]
        );
        assert_eq!(
            Provider::Claude.new_session_args(cwd, Some("x")),
            ["-n", "x"]
        );
        assert!(Provider::Claude.new_session_args(cwd, None).is_empty());
        assert_eq!(Provider::Codex.new_session_args(cwd, None), ["-C", "/w d"]);
    }
}
