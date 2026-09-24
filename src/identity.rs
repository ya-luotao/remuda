//! Login identity of an account (SPEC R4, R10a): `claude auth status --json`, or
//! `codex login status`.

use std::fs;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::Env;
use crate::launch;
use crate::probe::{self, Outcome};
use crate::provider::Provider;
use crate::registry::Account;

pub const AUTH_STATUS_ARGS: &[&str] = &["auth", "status", "--json"];
pub const LOGIN_STATUS_ARGS: &[&str] = &["login", "status"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    LoggedIn {
        email: Option<String>,
        org: Option<String>,
        plan: Option<String>,
        /// How it is logged in, when that is all there is to show (codex: `ChatGPT`,
        /// `API key`).
        method: Option<String>,
        /// Read from `.claude.json` because `claude auth status` failed.
        cached: bool,
    },
    NotLoggedIn,
    Unknown,
}

/// Parses `claude auth status --json`; `None` if the output is not recognizable.
pub fn parse_auth_status(stdout: &str) -> Option<Identity> {
    let v: Value = serde_json::from_str(stdout).ok()?;
    if !v.get("loggedIn")?.as_bool()? {
        return Some(Identity::NotLoggedIn);
    }
    Some(Identity::LoggedIn {
        email: string(&v, "email"),
        org: string(&v, "orgName"),
        plan: string(&v, "subscriptionType"),
        method: None,
        cached: false,
    })
}

/// Reads `oauthAccount` from a `.claude.json`; `None` if there is no usable account.
pub fn parse_claude_json(text: &str) -> Option<Identity> {
    let v: Value = serde_json::from_str(text).ok()?;
    let account = v.get("oauthAccount")?;
    Some(Identity::LoggedIn {
        email: Some(string(account, "emailAddress")?),
        org: string(account, "organizationName"),
        plan: None,
        method: None,
        cached: true,
    })
}

/// Parses `codex login status` (codex 0.155.1 writes it to stderr and exits 1 when not logged
/// in): `Not logged in`, or `Logged in using <method>` (for an API key followed by ` - ` and a
/// masked key, which is not kept). `None` if the output is not recognizable.
pub fn parse_login_status(text: &str) -> Option<Identity> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    if line == "Not logged in" {
        return Some(Identity::NotLoggedIn);
    }
    let method = line.strip_prefix("Logged in using ")?;
    let method = method.split(" - ").next().unwrap_or(method).trim();
    let method = ["an ", "a "]
        .iter()
        .find_map(|article| method.strip_prefix(article))
        .unwrap_or(method);
    (!method.is_empty()).then(|| Identity::LoggedIn {
        email: None,
        org: None,
        plan: None,
        method: Some(method.to_string()),
        cached: false,
    })
}

impl Identity {
    /// The account column of `list` and the accounts view: the email, else how it is logged
    /// in (`logged in (ChatGPT)`), else `-`; `(cached)` when read from `.claude.json`.
    pub fn who(&self) -> String {
        match self {
            Identity::LoggedIn {
                email,
                method,
                cached,
                ..
            } => {
                let mut who = match (email, method) {
                    (Some(email), _) => email.clone(),
                    (None, Some(method)) => format!("logged in ({method})"),
                    (None, None) => "-".to_string(),
                };
                if *cached {
                    who.push_str(" (cached)");
                }
                who
            }
            Identity::NotLoggedIn => "not logged in".to_string(),
            Identity::Unknown => "unknown".to_string(),
        }
    }
}

fn string(v: &Value, key: &str) -> Option<String> {
    v.get(key)?.as_str().map(str::to_string)
}

/// Identity of `account` under the account's environment: `claude auth status --json`
/// (falling back to its `.claude.json`) or `codex login status`. `program` is the provider's
/// executable, `None` when it is not on PATH. Returns a warning when the command ran but could
/// not be used.
pub fn identify(
    account: &Account,
    program: Option<&Path>,
    env: &Env,
    timeout: Duration,
) -> (Identity, Option<String>) {
    match account.provider {
        Provider::Claude => identify_claude(account, program, env, timeout),
        Provider::Codex => identify_codex(account, program, timeout),
    }
}

/// `codex login status`; codex's credentials (`auth.json`) are never read (R4).
fn identify_codex(
    account: &Account,
    program: Option<&Path>,
    timeout: Duration,
) -> (Identity, Option<String>) {
    let Some(program) = program else {
        return (Identity::Unknown, None);
    };
    let change = launch::env_change(account);
    let outcome = probe::run_captured(program, LOGIN_STATUS_ARGS, &change, timeout);
    let why = match &outcome {
        Outcome::Exited {
            code: Some(_),
            stdout,
            stderr,
        } => match parse_login_status(&format!("{stderr}\n{stdout}")) {
            Some(identity) => return (identity, None),
            None if outcome.success_stdout().is_some() => {
                "printed output remuda does not understand".to_string()
            }
            None => outcome.describe(timeout),
        },
        _ => outcome.describe(timeout),
    };
    let warning = format!(
        "{}: `codex {}` {why}; identity unknown",
        account.qualified(),
        LOGIN_STATUS_ARGS.join(" ")
    );
    (Identity::Unknown, Some(warning))
}

fn identify_claude(
    account: &Account,
    program: Option<&Path>,
    env: &Env,
    timeout: Duration,
) -> (Identity, Option<String>) {
    let failure = match program {
        None => None,
        Some(program) => {
            let change = launch::env_change(account);
            let outcome = probe::run_captured(program, AUTH_STATUS_ARGS, &change, timeout);
            match outcome.success_stdout().map(parse_auth_status) {
                Some(Some(identity)) => return (identity, None),
                Some(None) => Some("printed output remuda does not understand".to_string()),
                None => Some(outcome.describe(timeout)),
            }
        }
    };

    let cache = account.claude_json(env);
    let cached = cache
        .as_deref()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| parse_claude_json(&text));
    let warning = failure.map(|why| {
        let fallback = match (&cached, &cache) {
            (Some(_), Some(path)) => format!("showing the identity cached in {}", path.display()),
            _ => "identity unknown".to_string(),
        };
        format!(
            "{}: `claude {}` {why}; {fallback}",
            account.qualified(),
            AUTH_STATUS_ARGS.join(" ")
        )
    });
    (cached.unwrap_or(Identity::Unknown), warning)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logged_in(
        email: Option<&str>,
        org: Option<&str>,
        plan: Option<&str>,
        cached: bool,
    ) -> Identity {
        Identity::LoggedIn {
            email: email.map(str::to_string),
            org: org.map(str::to_string),
            plan: plan.map(str::to_string),
            method: None,
            cached,
        }
    }

    #[test]
    fn login_status_cases() {
        let method = |m: &str| Identity::LoggedIn {
            email: None,
            org: None,
            plan: None,
            method: Some(m.into()),
            cached: false,
        };
        assert_eq!(
            parse_login_status("Logged in using ChatGPT\n"),
            Some(method("ChatGPT"))
        );
        assert_eq!(
            parse_login_status("\nLogged in using an API key - sk-proj-***ABCD\n"),
            Some(method("API key"))
        );
        assert_eq!(
            parse_login_status("Not logged in\n"),
            Some(Identity::NotLoggedIn)
        );
        for bad in ["", "\n", "Logged in using \n", "something else"] {
            assert_eq!(parse_login_status(bad), None, "{bad:?}");
        }
        assert_eq!(method("ChatGPT").who(), "logged in (ChatGPT)");
        assert_eq!(Identity::NotLoggedIn.who(), "not logged in");
    }

    #[test]
    fn auth_status_cases() {
        let full = r#"{"loggedIn": true, "authMethod": "claude.ai", "email": "a@example.com",
            "orgName": "Org", "subscriptionType": "max", "configDirectory": "/x", "extra": {}}"#;
        assert_eq!(
            parse_auth_status(full),
            Some(logged_in(
                Some("a@example.com"),
                Some("Org"),
                Some("max"),
                false
            ))
        );
        assert_eq!(
            parse_auth_status(r#"{"loggedIn": true, "email": null, "authMethod": "api_key"}"#),
            Some(logged_in(None, None, None, false))
        );
        assert_eq!(
            parse_auth_status(r#"{"loggedIn": false, "email": "stale@example.com"}"#),
            Some(Identity::NotLoggedIn)
        );
        for bad in [
            "",
            "not json",
            "[1]",
            "{}",
            r#"{"loggedIn": "yes"}"#,
            "null",
        ] {
            assert_eq!(parse_auth_status(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn claude_json_cases() {
        let text = r#"{"numStartups": 1, "oauthAccount": {"emailAddress": "c@example.com",
            "organizationName": "Cached", "accountUuid": "00000000-0000-0000-0000-000000000000"}}"#;
        assert_eq!(
            parse_claude_json(text),
            Some(logged_in(Some("c@example.com"), Some("Cached"), None, true))
        );
        assert_eq!(
            parse_claude_json(r#"{"oauthAccount": {"emailAddress": "c@example.com"}}"#),
            Some(logged_in(Some("c@example.com"), None, None, true))
        );
        for bad in [
            "",
            "{",
            "{}",
            r#"{"oauthAccount": null}"#,
            r#"{"oauthAccount": {}}"#,
        ] {
            assert_eq!(parse_claude_json(bad), None, "{bad:?}");
        }
    }
}
