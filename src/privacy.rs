//! Account names as private mode shows them (SPEC R21): [`Aliases`] numbers the accounts per
//! provider, [`alias_words`] replaces names with their aliases in free text, and
//! [`alias_qualified`] every `provider:name` of a text, registered or not (R23).

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

/// What an account name is made of (R1): ASCII letters, digits, `_` and `-`. Any other character
/// ends a name, CJK text and full-width punctuation included.
fn name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// `text` with each `(name, alias)` of `names` replaced where the name is a whole word (ASCII
/// letters, digits, `_` and `-` make a word), in one pass: an alias is never replaced again. At
/// each word start the first name in `names` that matches wins, so the caller orders them
/// (longest first, qualified before bare).
pub fn alias_words(text: &str, names: &[(String, String)]) -> String {
    let word = name_char;
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

/// `text` with every qualified account name replaced by its alias, in one pass: a provider,
/// `:`, and the longest run of name characters after it, wherever the character before is not
/// an ASCII letter or digit (so also right after CJK text, `-` or `_`). A name `aliases` does
/// not know is given the next alias of its provider, in the order the text names them: nothing
/// that is written as an account goes through as written, except `default`.
pub fn alias_qualified(text: &str, aliases: &Aliases) -> String {
    let mut aliases = aliases.clone();
    let mut out = String::new();
    let mut rest = text;
    let mut prev: Option<char> = None;
    while let Some(c) = rest.chars().next() {
        if prev.is_none_or(|p| !p.is_ascii_alphanumeric())
            && let Some((qualified, after)) = qualified_name(rest)
        {
            aliases.note(qualified);
            let alias = aliases.qualified(qualified);
            prev = alias.chars().next_back();
            out.push_str(&alias);
            rest = after;
            continue;
        }
        out.push(c);
        prev = Some(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// The qualified name `text` starts with, if it starts with one, and what follows it.
fn qualified_name(text: &str) -> Option<(&str, &str)> {
    let provider = Provider::ALL.into_iter().find(|p| {
        text.strip_prefix(p.name())
            .is_some_and(|t| t.starts_with(':'))
    })?;
    let prefix = provider.name().len() + 1;
    let name = text[prefix..]
        .find(|c| !name_char(c))
        .unwrap_or(text.len() - prefix);
    (name > 0).then(|| text.split_at(prefix + name))
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
        // Only ASCII letters and digits, `_` and `-` make a word: CJK text and full-width
        // punctuation end one.
        assert_eq!(
            alias_words("把max留给claude:max（max）。 éclairmax maxé", &names),
            "把account-1留给claude:account-1（account-1）。 éclairmax account-1é"
        );
    }

    /// R23: every qualified name is replaced, whatever is written around it, the longest name
    /// each time; names not registered are numbered after the registered ones.
    #[test]
    fn alias_qualified_replaces_every_qualified_name() {
        let mut aliases = Aliases::default();
        for q in [
            "claude:default",
            "claude:max",
            "claude:team-alt",
            "codex:work",
        ] {
            aliases.note(q);
        }
        let alias = |text: &str| alias_qualified(text, &aliases);
        // Next to CJK text and full-width punctuation.
        assert_eq!(
            alias("把claude:team-alt留给大重构。别用claude:max（公司的），用codex:work。"),
            "把claude:account-2留给大重构。别用claude:account-1（公司的），用codex:account-1。"
        );
        // Next to `-`, `_` and other markup.
        assert_eq!(
            alias("-claude:max _claude:team-alt* (claude:max) `codex:work`, x=claude:max."),
            "-claude:account-1 _claude:account-2* (claude:account-1) `codex:account-1`, \
             x=claude:account-1."
        );
        // Not registered: the next aliases of the provider, in the order they are named, the
        // same one each time.
        assert_eq!(
            alias("claude:oldwork, codex:gone, claude:other then claude:oldwork"),
            "claude:account-3, codex:account-2, claude:account-4 then claude:account-3"
        );
        // The longest name: `max2` and `max_` are not `max`.
        assert_eq!(
            alias("claude:max2 claude:max_ _claude:max_ claude:max-"),
            "claude:account-3 claude:account-4 _claude:account-4 claude:account-5"
        );
        // `default` stays; a provider alone, a word that only ends in one, and bare names are
        // not qualified names.
        assert_eq!(
            alias("claude:default codex:default claude: codex:（x） xclaude:max 9codex:work max"),
            "claude:default codex:default claude: codex:（x） xclaude:max 9codex:work max"
        );
        // One pass: an alias is not read as a name again.
        assert_eq!(alias("claude:account-1"), "claude:account-3");
        assert_eq!(alias(""), "");
        // The caller's aliases are not changed.
        assert_eq!(aliases.qualified("claude:oldwork"), "claude:account-?");
    }
}
