//! Login identity of an account (SPEC R4, R10a): `claude auth status --json`, or
//! `codex login status`; and codex's `account/read`, answered in a live usage query (R10).

use std::fs;
use std::time::Duration;

use serde_json::Value;

use crate::Env;
use crate::account_command::Runner;
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

/// Parses the result of codex's `account/read` (R10, R10a): `account: null` (or no account
/// while `requiresOpenaiAuth` is present) is not logged in; a `chatgpt` account has an email and
/// a plan (`unknown` counts as none), other types only their login method. `None` if the result
/// is not recognizable.
pub fn parse_account_read(result: &Value) -> Option<Identity> {
    let result = result.as_object()?;
    let account = match result.get("account") {
        Some(Value::Null) => return Some(Identity::NotLoggedIn),
        None if result.contains_key("requiresOpenaiAuth") => return Some(Identity::NotLoggedIn),
        None => return None,
        Some(account) => account,
    };
    let method = |m: &str| Identity::LoggedIn {
        email: None,
        org: None,
        plan: None,
        method: Some(m.to_string()),
        cached: false,
    };
    Some(match account.get("type")?.as_str()? {
        "chatgpt" => {
            let text = |key: &str| {
                account
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string)
            };
            Identity::LoggedIn {
                email: text("email"),
                org: None,
                plan: text("planType").filter(|plan| plan != "unknown"),
                method: Some("ChatGPT".to_string()),
                cached: false,
            }
        }
        "apiKey" => method("API key"),
        "amazonBedrock" => method("Amazon Bedrock"),
        other => method(other),
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

/// What a command that answered with something else is said to have done.
const NOT_UNDERSTOOD: &str = "printed output remuda does not understand";

/// Identity of `account` under the account's environment: `claude auth status --json`
/// (falling back to its `.claude.json`) or `codex login status`, run by `agents`. Returns a
/// warning when the command ran but could not be used; an agent that is not on PATH is no
/// warning of its own.
pub fn identify(
    account: &Account,
    agents: &dyn Runner,
    env: &Env,
    timeout: Duration,
) -> (Identity, Option<String>) {
    match account.provider {
        Provider::Claude => identify_claude(account, agents, env, timeout),
        Provider::Codex => identify_codex(account, agents, timeout),
    }
}

/// `codex login status`, which answers on stderr and exits 1 when not logged in; codex's
/// credentials (`auth.json`) are never read (R4).
fn identify_codex(
    account: &Account,
    agents: &dyn Runner,
    timeout: Duration,
) -> (Identity, Option<String>) {
    let failure = match agents.run(account, LOGIN_STATUS_ARGS, timeout) {
        Err(failure) if failure.is_missing() => return (Identity::Unknown, None),
        Err(failure) => failure,
        Ok(output) => match parse_login_status(&format!("{}\n{}", output.stderr, output.stdout)) {
            Some(identity) => return (identity, None),
            None if output.code == 0 => output.said(NOT_UNDERSTOOD),
            None => output.failed(),
        },
    };
    let warning = format!("{}: {failure}; identity unknown", account.qualified());
    (Identity::Unknown, Some(warning))
}

fn identify_claude(
    account: &Account,
    agents: &dyn Runner,
    env: &Env,
    timeout: Duration,
) -> (Identity, Option<String>) {
    let failure = match agents.run_ok(account, AUTH_STATUS_ARGS, timeout) {
        Ok(output) => match parse_auth_status(&output.stdout) {
            Some(identity) => return (identity, None),
            None => Some(output.said(NOT_UNDERSTOOD)),
        },
        Err(failure) if failure.is_missing() => None,
        Err(failure) => Some(failure),
    };

    let cache = account.claude_json(env);
    let cached = cache
        .as_deref()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| parse_claude_json(&text));
    let warning = failure.map(|failure| {
        let fallback = match (&cached, &cache) {
            (Some(_), Some(path)) => format!("showing the identity cached in {}", path.display()),
            _ => "identity unknown".to_string(),
        };
        format!("{}: {failure}; {fallback}", account.qualified())
    });
    (cached.unwrap_or(Identity::Unknown), warning)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_command::Scripted;
    use crate::probe::Outcome;

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

    /// R10, R10a: codex's `account/read` result.
    #[test]
    fn account_read_cases() {
        let read = |text: &str| parse_account_read(&serde_json::from_str(text).unwrap());
        let chatgpt = |email: Option<&str>, plan: Option<&str>| Identity::LoggedIn {
            email: email.map(str::to_string),
            org: None,
            plan: plan.map(str::to_string),
            method: Some("ChatGPT".into()),
            cached: false,
        };
        let method = |m: &str| Identity::LoggedIn {
            email: None,
            org: None,
            plan: None,
            method: Some(m.into()),
            cached: false,
        };
        assert_eq!(
            read(
                r#"{"account": {"type": "chatgpt", "email": "cx@example.com", "planType": "pro"},
                "requiresOpenaiAuth": true}"#
            ),
            Some(chatgpt(Some("cx@example.com"), Some("pro")))
        );
        let unknown =
            read(r#"{"account": {"type": "chatgpt", "email": null, "planType": "unknown"}}"#);
        assert_eq!(unknown, Some(chatgpt(None, None)));
        assert_eq!(unknown.unwrap().who(), "logged in (ChatGPT)");
        assert_eq!(
            read(r#"{"account": {"type": "chatgpt", "email": "", "planType": ""}}"#),
            Some(chatgpt(None, None))
        );
        assert_eq!(
            read(r#"{"account": {"type": "apiKey"}, "requiresOpenaiAuth": true}"#),
            Some(method("API key"))
        );
        assert_eq!(
            read(r#"{"account": {"type": "amazonBedrock"}}"#),
            Some(method("Amazon Bedrock"))
        );
        assert_eq!(
            read(r#"{"account": {"type": "somethingNew"}}"#),
            Some(method("somethingNew"))
        );
        for logged_out in [
            r#"{"account": null, "requiresOpenaiAuth": true}"#,
            r#"{"requiresOpenaiAuth": false}"#,
        ] {
            assert_eq!(
                read(logged_out),
                Some(Identity::NotLoggedIn),
                "{logged_out}"
            );
        }
        for bad in [
            "null",
            "[]",
            "{}",
            r#"{"account": {}}"#,
            r#"{"account": {"type": 3}}"#,
            r#"{"account": "x"}"#,
        ] {
            assert_eq!(read(bad), None, "{bad}");
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

    fn codex_account() -> Account {
        Account {
            provider: crate::registry::CODEX,
            name: "work".into(),
            home: crate::registry::Home::Path("/c/work".into()),
        }
    }

    const LOGIN: &str = "codex:work login status";
    const AUTH: &str = "claude:max auth status --json";
    const T: Duration = Duration::from_secs(15);

    /// R10a: `codex login status` answers on stderr, and exits 1 when not logged in; an answer
    /// that is not one, a failure or a timeout is a warning naming the command, and a codex
    /// that is not on PATH no warning at all.
    #[test]
    fn codex_identity_is_what_login_status_says() {
        let work = codex_account();
        let env = Env::new();
        let method = |m: &str| Identity::LoggedIn {
            email: None,
            org: None,
            plan: None,
            method: Some(m.into()),
            cached: false,
        };
        let asked = |outcome: Outcome| {
            let agents = Scripted::new().on(LOGIN, outcome);
            let got = identify(&work, &agents, &env, T);
            assert_eq!(agents.ran(), [LOGIN]);
            got
        };
        assert_eq!(
            asked(Scripted::exited(0, "", "Logged in using ChatGPT\n")),
            (method("ChatGPT"), None)
        );
        assert_eq!(
            asked(Scripted::exited(1, "", "Not logged in\n")),
            (Identity::NotLoggedIn, None)
        );
        // Another version might answer on stdout.
        assert_eq!(
            asked(Scripted::exited(
                0,
                "Logged in using an API key - sk-***\n",
                ""
            )),
            (method("API key"), None)
        );
        let warned = |outcome: Outcome| {
            let (identity, warning) = asked(outcome);
            assert_eq!(identity, Identity::Unknown);
            warning.unwrap()
        };
        assert_eq!(
            warned(Scripted::exited(0, "", "Welcome to codex\n")),
            "codex:work: `codex login status` printed output remuda does not understand; \
             identity unknown"
        );
        assert_eq!(
            warned(Scripted::exited(2, "", "error: unrecognized subcommand\n")),
            "codex:work: `codex login status` exited with status 2: error: unrecognized \
             subcommand; identity unknown"
        );
        assert_eq!(
            warned(Outcome::TimedOut),
            "codex:work: `codex login status` timed out after 15s; identity unknown"
        );
        let agents = Scripted::new().without(Provider::Codex);
        assert_eq!(identify(&work, &agents, &env, T), (Identity::Unknown, None));
        assert!(agents.ran().is_empty());
    }

    /// R10a: `claude auth status --json`, and when it fails the identity cached in the home's
    /// `.claude.json`, with a warning that says which command failed and how; without claude
    /// on PATH the cached identity, and no warning of the account's own.
    #[test]
    fn claude_identity_falls_back_to_the_cached_one() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("max");
        fs::create_dir(&home).unwrap();
        let max = Account {
            provider: crate::registry::CLAUDE,
            name: "max".into(),
            home: crate::registry::Home::Path(home.display().to_string()),
        };
        let env = Env::new();
        let answer = r#"{"loggedIn": true, "email": "a@example.com", "orgName": "Org"}"#;
        let agents = Scripted::new().on(AUTH, Scripted::exited(0, answer, ""));
        assert_eq!(
            identify(&max, &agents, &env, T),
            (
                logged_in(Some("a@example.com"), Some("Org"), None, false),
                None
            )
        );

        // No cache yet: unknown, and said so.
        let failing = || Scripted::new().on(AUTH, Scripted::exited(1, "", "boom\n"));
        assert_eq!(
            identify(&max, &failing(), &env, T),
            (
                Identity::Unknown,
                Some(
                    "claude:max: `claude auth status --json` exited with status 1: boom; \
                     identity unknown"
                        .to_string()
                )
            )
        );
        let cache = home.join(".claude.json");
        fs::write(
            &cache,
            r#"{"oauthAccount": {"emailAddress": "c@example.com"}}"#,
        )
        .unwrap();
        let cached = logged_in(Some("c@example.com"), None, None, true);
        let shown = format!("showing the identity cached in {}", cache.display());
        assert_eq!(
            identify(&max, &failing(), &env, T),
            (
                cached.clone(),
                Some(format!(
                    "claude:max: `claude auth status --json` exited with status 1: boom; {shown}"
                ))
            )
        );
        let agents = Scripted::new().on(AUTH, Outcome::TimedOut);
        assert_eq!(
            identify(&max, &agents, &env, T).1,
            Some(format!(
                "claude:max: `claude auth status --json` timed out after 15s; {shown}"
            ))
        );
        let agents = Scripted::new().on(AUTH, Scripted::exited(0, "Logged in.\n", ""));
        assert_eq!(
            identify(&max, &agents, &env, T),
            (
                cached.clone(),
                Some(format!(
                    "claude:max: `claude auth status --json` printed output remuda does not \
                     understand; {shown}"
                ))
            )
        );
        let agents = Scripted::new().without(Provider::Claude);
        assert_eq!(identify(&max, &agents, &env, T), (cached, None));
        assert!(agents.ran().is_empty());
    }
}
