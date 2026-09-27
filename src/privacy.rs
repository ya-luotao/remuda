//! Account names as private mode shows them (SPEC R21): [`Aliases`] numbers the accounts per
//! provider, and [`alias_words`] replaces names with their aliases in free text.

use std::collections::BTreeMap;

use crate::provider::Provider;
use crate::registry::DEFAULT_NAME;

/// Aliases of account names (`claude:max` → `claude:account-2`), per provider in the order the
/// accounts were first noted; `default` stays itself. Grow-only: an alias never changes once
/// given.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Aliases {
    /// `provider:name` → `provider:account-<n>`.
    map: BTreeMap<String, String>,
    /// Aliases given so far, per provider.
    given: BTreeMap<String, usize>,
}

impl Aliases {
    /// Gives `qualified` (`provider:name`) the next alias of its provider, unless it has one.
    pub fn note(&mut self, qualified: &str) {
        if self.map.contains_key(qualified) {
            return;
        }
        let Some((provider, name)) = qualified.split_once(':') else {
            return;
        };
        let alias = if name == DEFAULT_NAME {
            qualified.to_string()
        } else {
            let n = self.given.entry(provider.to_string()).or_default();
            *n += 1;
            format!("{provider}:account-{n}")
        };
        self.map.insert(qualified.to_string(), alias);
    }

    /// The alias of `qualified`; one never noted is `<provider>:account-?` (`account-?` when
    /// the prefix is not a provider). Never the name itself, unless it is `default`.
    pub fn qualified(&self, qualified: &str) -> String {
        if let Some(alias) = self.map.get(qualified) {
            return alias.clone();
        }
        match qualified.split_once(':') {
            Some((provider, name)) if Provider::parse(provider).is_some() => {
                if name == DEFAULT_NAME {
                    qualified.to_string()
                } else {
                    format!("{provider}:account-?")
                }
            }
            _ => "account-?".to_string(),
        }
    }

    /// The alias without its provider prefix (`account-2`).
    pub fn name(&self, qualified: &str) -> String {
        let alias = self.qualified(qualified);
        match alias.split_once(':') {
            Some((_, name)) => name.to_string(),
            None => alias,
        }
    }

    /// Every noted name with its alias.
    pub fn pairs(&self) -> impl Iterator<Item = (&str, &str)> {
        self.map.iter().map(|(q, a)| (q.as_str(), a.as_str()))
    }
}

/// `text` with each `(name, alias)` of `names` replaced where the name is a whole word (letters,
/// digits, `_` and `-` make a word), in one pass: an alias is never replaced again. At each word
/// start the first name in `names` that matches wins, so the caller orders them (longest first,
/// qualified before bare).
pub fn alias_words(text: &str, names: &[(String, String)]) -> String {
    let word = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
    let mut out = String::new();
    let mut rest = text;
    let mut prev: Option<char> = None;
    'scan: while let Some(c) = rest.chars().next() {
        if prev.is_none_or(|p| !word(p)) {
            for (name, alias) in names {
                if let Some(after) = rest.strip_prefix(name.as_str())
                    && after.chars().next().is_none_or(|n| !word(n))
                {
                    out.push_str(alias);
                    prev = alias.chars().next_back();
                    rest = after;
                    continue 'scan;
                }
            }
        }
        out.push(c);
        prev = Some(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_are_per_provider_stable_and_never_the_name() {
        let mut aliases = Aliases::default();
        for q in [
            "claude:default",
            "claude:max",
            "codex:work",
            "claude:team",
            "claude:max",
        ] {
            aliases.note(q);
        }
        assert_eq!(aliases.qualified("claude:default"), "claude:default");
        assert_eq!(aliases.qualified("claude:max"), "claude:account-1");
        assert_eq!(aliases.qualified("claude:team"), "claude:account-2");
        assert_eq!(aliases.qualified("codex:work"), "codex:account-1");
        assert_eq!(aliases.name("claude:team"), "account-2");
        assert_eq!(aliases.name("codex:work"), "account-1");
        assert_eq!(aliases.qualified("claude:gone"), "claude:account-?");
        assert_eq!(aliases.qualified("codex:default"), "codex:default");
        assert_eq!(aliases.qualified("secret"), "account-?");
        assert_eq!(aliases.qualified("secret:thing"), "account-?");
        // Grow-only: a later account takes the next number.
        aliases.note("claude:new");
        assert_eq!(aliases.qualified("claude:new"), "claude:account-3");
        assert_eq!(aliases.qualified("claude:max"), "claude:account-1");
    }

    /// R21: names are replaced as whole words only, the first matching entry wins, and an alias
    /// is not replaced again.
    #[test]
    fn alias_words_replaces_whole_words_once() {
        let names: Vec<(String, String)> = [
            ("claude:max", "claude:account-1"),
            ("max", "account-1"),
            ("account-1", "leaked"),
        ]
        .iter()
        .map(|(n, a)| (n.to_string(), a.to_string()))
        .collect();
        assert_eq!(
            alias_words("claude:max, max: maximum max_x x-max (max)", &names),
            "claude:account-1, account-1: maximum max_x x-max (account-1)"
        );
        assert_eq!(alias_words("", &names), "");
        assert_eq!(alias_words("nothing here", &[]), "nothing here");
    }
}
