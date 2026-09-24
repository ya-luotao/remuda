//! Which accounts a session belongs to (SPEC R9): merged from remuda's launch log, live
//! sessions and each account's `history.jsonl`. A session may have several accounts; none is
//! normal.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

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

/// Calls `f` for each complete line of `path`; a missing or unreadable file has none.
fn for_each_line(path: &Path, mut f: impl FnMut(&[u8])) {
    let Ok(bytes) = fs::read(path) else { return };
    for line in complete_lines(&bytes, false).lines {
        f(line);
    }
}
