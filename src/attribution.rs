//! Which accounts a session belongs to (SPEC R9): merged from remuda's launch log, live
//! sessions and each account's `history.jsonl`. A session may have several accounts; none is
//! normal.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use serde::Deserialize;

use crate::Env;
use crate::index::{Entry, Store};
use crate::live::LiveSession;
use crate::provider::Provider;
use crate::registry::{Account, CLAUDE};
use crate::transcript::complete_lines;

/// `session_id -> {provider:name}`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attribution {
    map: BTreeMap<String, BTreeSet<String>>,
}

impl Attribution {
    pub fn add(&mut self, session_id: &str, account: &str) {
        self.map
            .entry(session_id.to_string())
            .or_default()
            .insert(account.to_string());
    }

    /// Accounts of `session_id`, sorted; empty when unknown.
    pub fn accounts(&self, session_id: &str) -> Vec<&str> {
        self.map
            .get(session_id)
            .map(|set| set.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    /// Number of attributed sessions.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Every account name any session is attributed to.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        let Attribution { map } = self;
        map.values().flatten().map(String::as_str)
    }

    /// The same attribution with each account name through `name` (private mode, R21). Every
    /// field is named: a new one does not compile until it is decided what private mode does
    /// with it.
    pub fn redacted(&self, name: impl Fn(&str) -> String) -> Attribution {
        let Attribution { map } = self;
        Attribution {
            map: map
                .iter()
                .map(|(session, names)| (session.clone(), names.iter().map(|n| name(n)).collect()))
                .collect(),
        }
    }

    /// Adds `$REMUDA_HOME/state/launches.jsonl`: lines with a non-null `session_id`.
    pub fn add_launch_log(&mut self, path: &Path) {
        #[derive(Deserialize)]
        struct Launch {
            account: Option<String>,
            session_id: Option<String>,
        }
        for_each_line(path, |line| {
            if let Ok(Launch {
                account: Some(account),
                session_id: Some(id),
            }) = serde_json::from_slice(line)
            {
                self.add(&id, &account);
            }
        });
    }

    /// Adds sessions currently running under an account.
    pub fn add_live(&mut self, live: &[LiveSession]) {
        for session in live {
            if let Some(id) = &session.session_id {
                self.add(id, &session.account);
            }
        }
    }

    /// Adds every `sessionId` of an account's `history.jsonl`.
    pub fn add_history(&mut self, path: &Path, account: &str) {
        #[derive(Deserialize)]
        struct Entry {
            #[serde(rename = "sessionId")]
            session_id: Option<String>,
        }
        for_each_line(path, |line| {
            if let Ok(Entry {
                session_id: Some(id),
            }) = serde_json::from_slice(line)
            {
                self.add(&id, account);
            }
        });
    }
}

/// The accounts of an indexed session: a codex rollout belongs to the account whose home holds
/// it, exactly (R17); a claude transcript to the accounts it is attributed to (R9), sorted.
pub fn accounts_of<'a>(
    entry: &Entry,
    stores: &'a [Store],
    attribution: &'a Attribution,
) -> Vec<&'a str> {
    match entry.provider {
        Provider::Codex => stores
            .iter()
            .find(|s| s.provider == Provider::Codex && s.path == entry.store)
            .map(|s| s.accounts.iter().map(String::as_str).collect())
            .unwrap_or_default(),
        Provider::Claude => attribution.accounts(&entry.session_id),
    }
}

/// `<home>/history.jsonl`, or `$HOME/.claude/history.jsonl` for the native login.
pub fn history_path(account: &Account, env: &Env) -> Option<PathBuf> {
    account.home_dir(env).map(|d| d.join("history.jsonl"))
}

/// All three sources. A `history.jsonl` that several accounts share (same realpath) cannot
/// tell them apart and is not used.
pub fn collect(
    accounts: &[Account],
    env: &Env,
    launch_log: &Path,
    live: &[LiveSession],
) -> Attribution {
    let mut attribution = Attribution::default();
    attribution.add_launch_log(launch_log);
    attribution.add_live(live);

    let histories: Vec<(String, PathBuf, Option<PathBuf>)> = accounts
        .iter()
        .filter(|a| a.provider == CLAUDE)
        .filter_map(|a| {
            let path = history_path(a, env)?;
            let real = fs::canonicalize(&path).ok();
            Some((a.qualified(), path, real))
        })
        .collect();
    for (account, path, real) in &histories {
        let Some(real) = real else { continue };
        let shared = histories
            .iter()
            .filter(|(_, _, other)| other.as_ref() == Some(real))
            .count()
            > 1;
        if !shared {
            attribution.add_history(path, account);
        }
    }
    attribution
}

/// A launch that ran a session, as the launch log has it (R6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launched {
    /// `provider:name`.
    pub account: String,
    /// The home string it was launched with, as logged (R2).
    pub home: String,
    pub ts: Timestamp,
    /// The session this launch ran: the session itself, or for a fork the one it made (unknown
    /// for codex, which chooses the id).
    pub session_id: Option<String>,
    pub cwd: Option<String>,
}

/// The latest launch of `provider` that ran session `id` (R23): one whose `session_id` is `id`,
/// or that forked it (`fork_of`), since a fork reads the whole session it copies. Latest by
/// `ts`, and of equal times the later line; a line whose `ts` cannot be read tells no time, and
/// one without a `home` no login, and they are skipped. A missing or unreadable log has none.
pub fn last_launch(path: &Path, provider: Provider, id: &str) -> Option<Launched> {
    #[derive(Deserialize)]
    struct Launch {
        ts: Option<String>,
        account: Option<String>,
        home: Option<String>,
        cwd: Option<String>,
        session_id: Option<String>,
        fork_of: Option<String>,
    }
    let prefix = format!("{provider}:");
    let mut last: Option<Launched> = None;
    for_each_line(path, |line| {
        let Ok(launch) = serde_json::from_slice::<Launch>(line) else {
            return;
        };
        let runs =
            launch.session_id.as_deref() == Some(id) || launch.fork_of.as_deref() == Some(id);
        let (Some(account), Some(home), Some(ts)) = (launch.account, launch.home, launch.ts) else {
            return;
        };
        let Ok(ts) = ts.parse::<Timestamp>() else {
            return;
        };
        if !runs || !account.starts_with(&prefix) {
            return;
        }
        if last.as_ref().is_none_or(|l| ts >= l.ts) {
            last = Some(Launched {
                account,
                home,
                ts,
                session_id: launch.session_id,
                cwd: launch.cwd,
            });
        }
    });
    last
}

/// Calls `f` for each complete line of `path`; a missing or unreadable file has none.
fn for_each_line(path: &Path, mut f: impl FnMut(&[u8])) {
    let Ok(bytes) = fs::read(path) else { return };
    for line in complete_lines(&bytes, false).lines {
        f(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R23 (Resuming): the latest launch that ran the session, a fork of it included; another
    /// provider's launches, other sessions and lines without a readable time do not count.
    #[test]
    fn last_launch_counts_forks_and_takes_the_latest() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("launches.jsonl");
        let id = "766560c5-0000-4000-8000-000000000000";
        let lines = [
            format!(
                r#"{{"ts":"2026-10-08T10:00:00Z","account":"claude:max","home":"/homes/max","cwd":"/a","session_id":"{id}"}}"#
            ),
            // A fork of it, later: the fork's account read the whole session.
            format!(
                r#"{{"ts":"2026-10-08T10:30:00Z","account":"claude:team","home":"/homes/team","cwd":"/b","session_id":"f0","fork_of":"{id}"}}"#
            ),
            // Later still, but another session, another provider, or no time.
            r#"{"ts":"2026-10-08T11:00:00Z","account":"claude:max","home":"/homes/max","session_id":"other"}"#
                .to_string(),
            format!(
                r#"{{"ts":"2026-10-08T11:00:00Z","account":"codex:work","home":"/homes/work","session_id":"{id}"}}"#
            ),
            // No home: no login to tell, later though it is.
            format!(r#"{{"ts":"2026-10-08T12:00:00Z","account":"claude:max","session_id":"{id}"}}"#),
            format!(r#"{{"ts":"yesterday","account":"claude:max","home":"/homes/max","session_id":"{id}"}}"#),
            "not json".to_string(),
        ];
        fs::write(&log, lines.join("\n") + "\n").unwrap();
        let last = last_launch(&log, Provider::Claude, id).unwrap();
        assert_eq!(
            last,
            Launched {
                account: "claude:team".into(),
                home: "/homes/team".into(),
                ts: "2026-10-08T10:30:00Z".parse().unwrap(),
                session_id: Some("f0".into()),
                cwd: Some("/b".into()),
            }
        );
        assert_eq!(
            last_launch(&log, Provider::Codex, id).map(|l| l.account),
            Some("codex:work".into())
        );
        assert_eq!(last_launch(&log, Provider::Claude, "nothing"), None);
        assert_eq!(
            last_launch(&dir.path().join("missing"), Provider::Claude, id),
            None
        );
        // Of equal times, the later line.
        let tie = [
            format!(r#"{{"ts":"2026-10-08T10:00:00Z","account":"claude:a","home":"/homes/a","session_id":"{id}"}}"#),
            format!(r#"{{"ts":"2026-10-08T10:00:00Z","account":"claude:b","home":"/homes/b","session_id":"{id}"}}"#),
        ]
        .join("\n")
            + "\n";
        fs::write(&log, tie).unwrap();
        assert_eq!(
            last_launch(&log, Provider::Claude, id).unwrap().account,
            "claude:b"
        );
    }
}
