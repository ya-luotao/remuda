//! Fuzzy search over history entries (R8): only the columns shown — the display title (the
//! `ai-title`, else the first user text), `cwd_last` and the accounts.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::index::Entry;

/// Entries matching `query`, best first; equal scores keep the order of `entries` (newest
/// first). `accounts` gives the searchable account names of an entry.
pub fn rank<'a>(
    query: &str,
    entries: &[&'a Entry],
    accounts: impl Fn(&Entry) -> String,
) -> Vec<&'a Entry> {
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, &'a Entry)> = entries
        .iter()
        .filter_map(|e| {
            let haystack = haystack(e, &accounts(e));
            let score = pattern.score(Utf32Str::new(&haystack, &mut buf), &mut matcher)?;
            Some((score, *e))
        })
        .collect();
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(_, e)| e).collect()
}

fn haystack(e: &Entry, accounts: &str) -> String {
    [e.display_title(), e.cwd_last.as_deref(), Some(accounts)]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    pub(crate) fn entry(id: &str, title: Option<&str>, first: Option<&str>, cwd: &str) -> Entry {
        Entry {
            provider: crate::provider::Provider::Claude,
            session_id: id.into(),
            path: PathBuf::from(format!("/s/p/{id}.jsonl")),
            store: PathBuf::from("/s"),
            size: 1,
            mtime_ns: 0,
            ino: 1,
            scanned_offset: 1,
            gap: false,
            title: title.map(str::to_string),
            first_user_text: first.map(str::to_string),
            cwd_first: Some(cwd.into()),
            cwd_last: Some(cwd.into()),
            ts_first: None,
            ts_last: None,
            entrypoint: Some("cli".into()),
            source: None,
            originator: None,
        }
    }

    fn ids(v: Vec<&Entry>) -> Vec<&str> {
        v.into_iter().map(|e| e.session_id.as_str()).collect()
    }

    #[test]
    fn matches_displayed_title_cwd_and_accounts() {
        let a = entry("a", Some("Fix index scan"), Some("please fix"), "/w/remuda");
        let b = entry("b", None, Some("write the README"), "/w/docs");
        let c = entry("c", Some("Tune TUI colors"), None, "/w/remuda");
        let all = [&a, &b, &c];
        let acct = |e: &Entry| {
            if e.session_id == "b" {
                "claude:team".to_string()
            } else {
                "claude:max".to_string()
            }
        };
        // b shows its first user text as the title.
        assert_eq!(ids(rank("readme", &all, acct)), ["b"]);
        // Fuzzy: "remuda" is also a subsequence of b's row, but the cwd matches rank first.
        let mut top: Vec<&str> = ids(rank("remuda", &all, acct))[..2].to_vec();
        top.sort();
        assert_eq!(top, ["a", "c"]);
        assert_eq!(ids(rank("docs", &all, acct)), ["b"]);
        assert_eq!(ids(rank("claude:team", &all, acct)), ["b"]);
        assert_eq!(ids(rank("idxscan", &all, acct)), ["a"]);
        assert!(rank("zzzz", &all, acct).is_empty());
    }

    #[test]
    fn text_hidden_behind_an_ai_title_is_not_searched() {
        let long = format!(
            "{} zanzibar {}",
            "lorem ipsum ".repeat(10),
            "dolor ".repeat(10)
        );
        let titled = entry("titled", Some("Fix index scan"), Some(&long), "/w/remuda");
        let untitled = entry("untitled", None, Some(&long), "/w/docs");
        let all = [&titled, &untitled];
        assert_eq!(ids(rank("zanzibar", &all, |_| String::new())), ["untitled"]);
    }

    #[test]
    fn better_matches_rank_first() {
        let loose = entry("loose", Some("t u i are letters here"), None, "/x");
        let exact = entry("exact", Some("the tui view"), None, "/x");
        let all = [&loose, &exact];
        assert_eq!(
            ids(rank("tui", &all, |_| String::new())),
            ["exact", "loose"]
        );
    }
}
