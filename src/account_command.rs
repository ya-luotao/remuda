//! Agent commands run in an account's environment (SPEC R2, R4, R7, R10, R10a): give it the
//! account, the subcommand and a timeout; it picks the provider's agent, sets or removes the
//! home variable, runs it, and answers with what it printed or with why there is nothing to
//! read, already worded (`` `claude auth status --json` timed out after 15s ``). What the output
//! means is the caller's: [`crate::identity`], [`crate::usage`] and [`crate::live`] parse.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::launch::{self, EnvChange};
use crate::probe::{self, Outcome};
use crate::provider::{Provider, app_server};
use crate::registry::Account;

/// What one `codex app-server` run answered, as [`app_server::call`] gives it: each request's
/// result or its error; `Err` when it could not answer them all.
pub type Answers = Result<Vec<Result<Value, String>>, String>;

/// How an agent is run for an account: the seam between remuda and the agents' programs.
/// [`OnPath`] runs the ones found on `PATH`; tests script the answers instead.
pub trait Runner: Sync {
    /// Whether `provider`'s agent is there to run. The other two are asked only when it is.
    fn has(&self, provider: Provider) -> bool;

    /// `<agent> args...` in `account`'s environment without the variables named in `unset`,
    /// stdin closed, output captured, at most `timeout`.
    fn captured(
        &self,
        account: &Account,
        args: &[&str],
        unset: &[&str],
        timeout: Duration,
    ) -> Outcome;

    /// One `codex app-server` run in `account`'s environment, as [`app_server::call`]: each
    /// request's result or error, or why it could not answer them all.
    fn app_server(
        &self,
        account: &Account,
        requests: &[(&str, Value)],
        timeout: Duration,
    ) -> Answers;
}

impl dyn Runner + '_ {
    /// Runs `<agent> args...` for `account`. `Ok` is a run that exited, whatever its status.
    pub fn run(
        &self,
        account: &Account,
        args: &[&str],
        timeout: Duration,
    ) -> Result<Output, Failure> {
        self.run_without(account, args, &[], timeout)
    }

    /// [`run`](Self::run) with the variables named in `unset` removed from the agent's
    /// environment, for a command that an inherited setting would keep from answering.
    fn run_without(
        &self,
        account: &Account,
        args: &[&str],
        unset: &[&str],
        timeout: Duration,
    ) -> Result<Output, Failure> {
        let command = format!("{} {}", account.provider.program(), args.join(" "));
        if !self.has(account.provider) {
            let why = Why::Missing(account.provider);
            return Err(Failure { command, why });
        }
        match self.captured(account, args, unset, timeout) {
            Outcome::Exited {
                code: Some(code),
                stdout,
                stderr,
            } => Ok(Output {
                code,
                stdout,
                stderr,
                command,
                timeout,
            }),
            outcome => Err(Failure {
                command,
                why: Why::Ran { outcome, timeout },
            }),
        }
    }

    /// [`run`](Self::run), where only status 0 is `Ok`.
    pub fn run_ok(
        &self,
        account: &Account,
        args: &[&str],
        timeout: Duration,
    ) -> Result<Output, Failure> {
        self.run_ok_without(account, args, &[], timeout)
    }

    /// [`run_ok`](Self::run_ok), with the variables named in `unset` removed from the agent's
    /// environment.
    pub fn run_ok_without(
        &self,
        account: &Account,
        args: &[&str],
        unset: &[&str],
        timeout: Duration,
    ) -> Result<Output, Failure> {
        let output = self.run_without(account, args, unset, timeout)?;
        if output.code == 0 {
            Ok(output)
        } else {
            Err(output.failed())
        }
    }

    /// `requests` in one `codex app-server` run for `account` ([`Runner::app_server`]).
    pub fn requests(
        &self,
        account: &Account,
        requests: &[(&str, Value)],
        timeout: Duration,
    ) -> Answers {
        if !self.has(account.provider) {
            return Err(missing(account.provider));
        }
        self.app_server(account, requests, timeout)
    }
}

/// What a command that ran to its end printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    command: String,
    timeout: Duration,
}

impl Output {
    /// The failure a status other than 0 is.
    pub fn failed(self) -> Failure {
        let outcome = Outcome::Exited {
            code: Some(self.code),
            stdout: self.stdout,
            stderr: self.stderr,
        };
        Failure {
            command: self.command,
            why: Why::Ran {
                outcome,
                timeout: self.timeout,
            },
        }
    }

    /// The failure output that cannot be read is: `what` is the rest of the sentence after the
    /// command (`printed output remuda does not understand`).
    pub fn said(&self, what: &str) -> Failure {
        Failure {
            command: self.command.clone(),
            why: Why::Said(what.to_string()),
        }
    }
}

/// Why a command gave nothing to read. Displayed as a whole clause:
/// `` `claude -p /usage --no-session-persistence` exited with status 1: <stderr's first line> ``,
/// or `` `codex` not found on PATH ``.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// `claude auth status --json`.
    command: String,
    why: Why,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Why {
    /// The provider's agent is not on `PATH`.
    Missing(Provider),
    Ran {
        outcome: Outcome,
        timeout: Duration,
    },
    Said(String),
}

impl Failure {
    /// The agent is not on `PATH`: nothing ran.
    pub fn is_missing(&self) -> bool {
        matches!(self.why, Why::Missing(_))
    }

    /// The failure without the command: `timed out after 15s`. A missing agent is named all the
    /// same.
    pub fn reason(&self) -> String {
        match &self.why {
            Why::Missing(provider) => missing(*provider),
            Why::Ran { outcome, timeout } => outcome.describe(*timeout),
            Why::Said(what) => what.clone(),
        }
    }

    /// [`reason`](Self::reason) for a command that reports its errors on stdout (`claude logs`,
    /// `stop`, `rm`): stdout's first line when it failed without writing to stderr.
    pub fn reason_or_stdout(&self) -> String {
        match &self.why {
            Why::Ran { outcome, timeout } => outcome.describe_or_stdout(*timeout),
            _ => self.reason(),
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.why {
            Why::Missing(provider) => f.write_str(&missing(*provider)),
            _ => write!(f, "`{}` {}", self.command, self.reason()),
        }
    }
}

fn missing(provider: Provider) -> String {
    format!("`{}` not found on PATH", provider.program())
}

/// The agents found on `PATH` (`None`: not there), run through [`probe`]. The home variable is
/// set to the account's home, or removed for `default`, by [`launch::env_change`] alone (R2);
/// the variables a caller names are removed after it.
#[derive(Debug, Clone, Copy, Default)]
pub struct OnPath<'a> {
    pub claude: Option<&'a Path>,
    pub codex: Option<&'a Path>,
}

impl OnPath<'_> {
    fn program(&self, provider: Provider) -> Option<&Path> {
        match provider {
            Provider::Claude => self.claude,
            Provider::Codex => self.codex,
        }
    }
}

impl Runner for OnPath<'_> {
    fn has(&self, provider: Provider) -> bool {
        self.program(provider).is_some()
    }

    fn captured(
        &self,
        account: &Account,
        args: &[&str],
        unset: &[&str],
        timeout: Duration,
    ) -> Outcome {
        match self.program(account.provider) {
            Some(program) => {
                let mut changes = vec![launch::env_change(account)];
                changes.extend(unset.iter().map(|var| EnvChange::Remove(var.to_string())));
                probe::run_captured_with(program, args, &changes, timeout)
            }
            None => Outcome::SpawnFailed(missing(account.provider)),
        }
    }

    fn app_server(
        &self,
        account: &Account,
        requests: &[(&str, Value)],
        timeout: Duration,
    ) -> Answers {
        match self.program(account.provider) {
            Some(program) => app_server::call(program, account, requests, timeout),
            None => Err(missing(account.provider)),
        }
    }
}

/// What a parser made of an agent's answer: the items it read, and how many more looked like
/// one and could not be read. Without the count, a caller can only tell "nothing" from
/// "something", and takes an answer read in part for the whole of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed<T> {
    pub items: Vec<T>,
    pub unrecognized: usize,
}

impl<T> Parsed<T> {
    /// Reads each of `candidates`: `None` is one that looked like an item and could not be
    /// read.
    pub fn of(candidates: impl IntoIterator<Item = Option<T>>) -> Self {
        let mut parsed = Parsed {
            items: Vec::new(),
            unrecognized: 0,
        };
        for candidate in candidates {
            match candidate {
                Some(item) => parsed.items.push(item),
                None => parsed.unrecognized += 1,
            }
        }
        parsed
    }

    /// The one rule for trusting an answer: the items, when there are some and nothing that
    /// looked like one was left unread. `Err` is how many were (0: there was no item at all).
    pub fn complete(self) -> Result<Vec<T>, usize> {
        if self.unrecognized == 0 && !self.items.is_empty() {
            Ok(self.items)
        } else {
            Err(self.unrecognized)
        }
    }
}

/// A [`Runner`] that answers from a script, for the tests of the modules that parse: no
/// process runs. A command without a script is a bug in the test.
#[cfg(test)]
pub(crate) struct Scripted {
    without: Vec<Provider>,
    outcomes: Vec<(String, Outcome)>,
    answers: Vec<(String, Answers)>,
    ran: std::sync::Mutex<Vec<String>>,
    unset: std::sync::Mutex<Vec<(String, Vec<String>)>>,
}

#[cfg(test)]
impl Scripted {
    pub(crate) fn new() -> Self {
        Scripted {
            without: Vec::new(),
            outcomes: Vec::new(),
            answers: Vec::new(),
            ran: std::sync::Mutex::new(Vec::new()),
            unset: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// `provider`'s agent is not on `PATH`.
    pub(crate) fn without(mut self, provider: Provider) -> Self {
        self.without.push(provider);
        self
    }

    /// `command` (`claude:max auth status --json`: the account, then the arguments) ends as
    /// `outcome`.
    pub(crate) fn on(mut self, command: &str, outcome: Outcome) -> Self {
        self.outcomes.push((command.to_string(), outcome));
        self
    }

    /// `account`'s `codex app-server` answers `answer`.
    pub(crate) fn app_server_says(mut self, account: &str, answer: Answers) -> Self {
        self.answers.push((account.to_string(), answer));
        self
    }

    /// What ran so far, in order: `claude:max auth status --json`,
    /// `codex:work app-server account/rateLimits/read account/read`.
    pub(crate) fn ran(&self) -> Vec<String> {
        self.ran.lock().unwrap().clone()
    }

    /// The variables each run of `command` was to go without, in order of the runs.
    pub(crate) fn unset_for(&self, command: &str) -> Vec<Vec<String>> {
        let unset = self.unset.lock().unwrap();
        unset
            .iter()
            .filter(|(c, _)| c == command)
            .map(|(_, vars)| vars.clone())
            .collect()
    }

    /// An [`Outcome`] that exited with `code`.
    pub(crate) fn exited(code: i32, stdout: &str, stderr: &str) -> Outcome {
        Outcome::Exited {
            code: Some(code),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        }
    }
}

#[cfg(test)]
impl Runner for Scripted {
    fn has(&self, provider: Provider) -> bool {
        !self.without.contains(&provider)
    }

    fn captured(
        &self,
        account: &Account,
        args: &[&str],
        unset: &[&str],
        _timeout: Duration,
    ) -> Outcome {
        let command = format!("{} {}", account.qualified(), args.join(" "));
        let outcome = self.outcomes.iter().find(|(c, _)| *c == command);
        self.ran.lock().unwrap().push(command.clone());
        let vars = unset.iter().map(|var| var.to_string()).collect();
        self.unset.lock().unwrap().push((command.clone(), vars));
        match outcome {
            Some((_, outcome)) => outcome.clone(),
            None => panic!("no script for {command:?}"),
        }
    }

    fn app_server(
        &self,
        account: &Account,
        requests: &[(&str, Value)],
        _timeout: Duration,
    ) -> Answers {
        let account = account.qualified();
        let methods: Vec<&str> = requests.iter().map(|(method, _)| *method).collect();
        let ran = format!("{account} app-server {}", methods.join(" "));
        self.ran.lock().unwrap().push(ran);
        match self.answers.iter().find(|(a, _)| *a == account) {
            Some((_, answer)) => answer.clone(),
            None => panic!("no app-server script for {account:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::script;
    use crate::registry::{CLAUDE, CODEX, Home};

    const T: Duration = Duration::from_secs(15);

    fn claude(name: &str) -> Account {
        Account {
            provider: CLAUDE,
            name: name.into(),
            home: Home::Path(format!("/h/{name}/")),
        }
    }

    fn run(agents: &Scripted, account: &Account, args: &[&str]) -> Result<Output, Failure> {
        (agents as &dyn Runner).run(account, args, T)
    }

    fn run_ok(agents: &Scripted, account: &Account, args: &[&str]) -> Result<Output, Failure> {
        (agents as &dyn Runner).run_ok(account, args, T)
    }

    /// R10, R10a: each way a command gives nothing to read, as the user reads it.
    #[test]
    fn failures_are_worded_with_the_command() {
        let max = claude("max");
        let agents = Scripted::new()
            .on("claude:max slow", Outcome::TimedOut)
            .on(
                "claude:max gone",
                Outcome::SpawnFailed("no such file".into()),
            )
            .on(
                "claude:max killed",
                Outcome::Exited {
                    code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            )
            .on(
                "claude:max bad --flag",
                Scripted::exited(2, "", "\nerror: no\nmore\n"),
            )
            .on("claude:max quiet", Scripted::exited(1, "", ""))
            .on(
                "claude:max chatty",
                Scripted::exited(1, "Refusing: unpushed\n", " \n"),
            );
        let said = |args: &[&str]| run_ok(&agents, &max, args).unwrap_err().to_string();
        assert_eq!(said(&["slow"]), "`claude slow` timed out after 15s");
        assert_eq!(
            said(&["gone"]),
            "`claude gone` could not be started: no such file"
        );
        assert_eq!(said(&["killed"]), "`claude killed` was killed by a signal");
        assert_eq!(
            said(&["bad", "--flag"]),
            "`claude bad --flag` exited with status 2: error: no"
        );
        assert_eq!(said(&["quiet"]), "`claude quiet` exited with status 1");
        // Stdout is not an error message unless the caller says the command writes it there.
        let chatty = run_ok(&agents, &max, &["chatty"]).unwrap_err();
        assert_eq!(chatty.to_string(), "`claude chatty` exited with status 1");
        assert_eq!(chatty.reason(), "exited with status 1");
        assert_eq!(
            chatty.reason_or_stdout(),
            "exited with status 1: Refusing: unpushed"
        );
        assert!(!chatty.is_missing());
        let slow = run(&agents, &max, &["slow"]).unwrap_err();
        assert_eq!(slow.reason(), "timed out after 15s");
        assert_eq!(slow.reason_or_stdout(), "timed out after 15s");
    }

    /// A run that exited is `Ok` whatever its status; `run_ok` wants 0. Output that cannot be
    /// read is a failure the caller words.
    #[test]
    fn an_exit_status_is_the_callers_to_judge() {
        let max = claude("max");
        let agents = Scripted::new()
            .on("claude:max status", Scripted::exited(1, "out\n", "err\n"))
            .on("claude:max ok", Scripted::exited(0, "{}", ""));
        let output = run(&agents, &max, &["status"]).unwrap();
        assert_eq!(
            (output.code, output.stdout.as_str(), output.stderr.as_str()),
            (1, "out\n", "err\n")
        );
        assert_eq!(
            output.failed().to_string(),
            "`claude status` exited with status 1: err"
        );
        let output = run_ok(&agents, &max, &["ok"]).unwrap();
        assert_eq!(output.stdout, "{}");
        let unread = output.said("printed output remuda does not understand");
        assert_eq!(
            unread.to_string(),
            "`claude ok` printed output remuda does not understand"
        );
        assert_eq!(unread.reason(), "printed output remuda does not understand");
        assert_eq!(agents.ran(), ["claude:max status", "claude:max ok"]);
    }

    /// An agent that is not on PATH is named, not its command; nothing runs.
    #[test]
    fn a_missing_agent_is_named() {
        let work = Account {
            provider: CODEX,
            name: "work".into(),
            home: Home::Path("/c/work".into()),
        };
        let agents = Scripted::new().without(CODEX);
        let failure = run(&agents, &work, &["login", "status"]).unwrap_err();
        assert!(failure.is_missing());
        assert_eq!(failure.to_string(), "`codex` not found on PATH");
        assert_eq!(failure.reason(), "`codex` not found on PATH");
        assert_eq!(failure.reason_or_stdout(), "`codex` not found on PATH");
        let runner: &dyn Runner = &agents;
        assert_eq!(
            runner.requests(&work, &[("account/read", Value::Null)], T),
            Err("`codex` not found on PATH".to_string())
        );
        assert!(agents.ran().is_empty());
        // The same from the real adapter.
        let none = OnPath::default();
        let runner: &dyn Runner = &none;
        assert_eq!(
            runner.run(&work, &["login", "status"], T).unwrap_err(),
            failure
        );
        assert_eq!(
            runner
                .run_ok(&claude("max"), &["agents"], T)
                .unwrap_err()
                .to_string(),
            "`claude` not found on PATH"
        );
    }

    /// R2: the real adapter sets the provider's home variable to the home, byte for byte, and
    /// removes an inherited one for `default`; each provider gets its own program.
    #[test]
    fn on_path_runs_the_providers_agent_in_the_accounts_environment() {
        let dir = tempfile::tempdir().unwrap();
        let claude_program = script(
            dir.path(),
            "claude",
            "printf 'claude %s [%s]' \"$*\" \"${CLAUDE_CONFIG_DIR-unset}\"",
        );
        let codex_program = script(
            dir.path(),
            "codex",
            "printf 'codex %s [%s]' \"$*\" \"${CODEX_HOME-unset}\" >&2; exit 1",
        );
        let agents = OnPath {
            claude: Some(&claude_program),
            codex: Some(&codex_program),
        };
        let runner: &dyn Runner = &agents;
        assert!(runner.has(CLAUDE) && runner.has(CODEX));
        let output = runner
            .run_ok(&claude("max"), &["auth", "status"], T)
            .unwrap();
        assert_eq!(output.stdout, "claude auth status [/h/max/]");
        // `default`: the variable is not in the child's environment, whatever remuda inherited
        // (cargo's own environment has none; `probe::applies_env_change` covers the removal).
        let output = runner
            .run_ok(&Account::default_for(CLAUDE), &["agents"], T)
            .unwrap();
        assert_eq!(output.stdout, "claude agents [unset]");
        let work = Account {
            provider: CODEX,
            name: "work".into(),
            home: Home::Path("/c/work/../work".into()),
        };
        let output = runner.run(&work, &["login", "status"], T).unwrap();
        assert_eq!(output.code, 1);
        assert_eq!(output.stderr, "codex login status [/c/work/../work]");
        assert_eq!(
            runner
                .run_ok(&work, &["login", "status"], T)
                .unwrap_err()
                .to_string(),
            "`codex login status` exited with status 1: codex login status [/c/work/../work]"
        );
    }

    /// R10: a variable the caller names is not in the agent's environment, whatever remuda
    /// inherited; the home variable is still the account's. (`CARGO_MANIFEST_DIR` stands for an
    /// inherited variable: cargo sets it for the tests it runs.)
    #[test]
    fn on_path_removes_the_variables_the_caller_names() {
        let dir = tempfile::tempdir().unwrap();
        let claude_program = script(
            dir.path(),
            "claude",
            "printf '[%s] [%s]' \"${CLAUDE_CONFIG_DIR-unset}\" \"${CARGO_MANIFEST_DIR-unset}\"",
        );
        let agents = OnPath {
            claude: Some(&claude_program),
            codex: None,
        };
        let runner: &dyn Runner = &agents;
        let inherited = env!("CARGO_MANIFEST_DIR");
        let output = runner.run_ok(&claude("max"), &["x"], T).unwrap();
        assert_eq!(
            output.stdout,
            format!("[/h/max/] [{inherited}]"),
            "the test runs through `cargo test`, which sets CARGO_MANIFEST_DIR"
        );
        let output = runner
            .run_ok_without(&claude("max"), &["x"], &["CARGO_MANIFEST_DIR"], T)
            .unwrap();
        assert_eq!(output.stdout, "[/h/max/] [unset]");
    }

    #[test]
    fn a_parse_is_complete_only_with_nothing_left_unread() {
        let parsed = Parsed::of([Some(1), None, Some(2), None]);
        assert_eq!(
            parsed,
            Parsed {
                items: vec![1, 2],
                unrecognized: 2
            }
        );
        assert_eq!(parsed.complete(), Err(2));
        assert_eq!(Parsed::of([Some(1), Some(2)]).complete(), Ok(vec![1, 2]));
        assert_eq!(Parsed::<u8>::of([]).complete(), Err(0));
        assert_eq!(Parsed::<u8>::of([None]).complete(), Err(1));
    }
}
