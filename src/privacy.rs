//! What the library knows for private mode (SPEC R21): [`Marked`] is a message remuda puts
//! together, in the pieces it was made of, so that a path in it is masked as the path it is;
//! [`Aliases`] numbers the accounts per provider, [`alias_words`] replaces names with their
//! aliases in free text, and [`alias_qualified`] every `provider:name` of a text, registered or
//! not (R23).

use std::collections::BTreeMap;
use std::fmt::{self, Display};
use std::ops::Deref;

use crate::provider::Provider;
use crate::registry::DEFAULT_NAME;

/// What a piece of a [`Marked`] message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piece {
    /// Words remuda wrote itself: literals, with the account names, the numbers and remuda's own
    /// names for things (`file-history`, `settings.json`) in them. Never a path, and nothing
    /// read from a file or a child.
    Words,
    /// One path, whole, whatever characters it holds.
    Path,
    /// Text from elsewhere (an agent's output, a system error, a name read from a file), or
    /// text whose origin is not known.
    Text,
}

/// A message remuda puts together for a person to read (a notice, a form's error, a check, a
/// problem of the Configuration pane, the error of a cache), in the pieces it was made of. It
/// reads as one string, the same one `format!` would give. Private mode (R21) masks it piece
/// by piece: a path whole, where it ends being known; remuda's own words without looking for
/// paths in them; and only in text from elsewhere what must be guessed, which never reaches
/// into the next piece.
///
/// A string becomes a message as [`Piece::Text`]: saying that words are remuda's own
/// ([`Marked::words`]) is always a step of its own. When in doubt, [`Marked::text`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Marked {
    text: String,
    /// Where each piece ends in `text`, and what it is.
    pieces: Vec<(usize, Piece)>,
}

impl Marked {
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// With `words` after it: what remuda itself says ([`Piece::Words`]).
    pub fn words(self, words: impl AsRef<str>) -> Self {
        self.piece(words.as_ref(), Piece::Words)
    }

    /// With the path `path` after it.
    pub fn path(self, path: impl Display) -> Self {
        self.piece(&path.to_string(), Piece::Path)
    }

    /// With `text` after it, as a piece of its own: a message from elsewhere, or one remuda
    /// cannot vouch for ([`Piece::Text`]).
    pub fn text(self, text: impl AsRef<str>) -> Self {
        self.piece(text.as_ref(), Piece::Text)
    }

    /// With the pieces of `other` after it.
    pub fn join(mut self, other: &Marked) -> Self {
        for (piece, kind) in other.pieces() {
            self = self.piece(piece, kind);
        }
        self
    }

    fn piece(mut self, piece: &str, kind: Piece) -> Self {
        if !piece.is_empty() {
            self.text.push_str(piece);
            self.pieces.push((self.text.len(), kind));
        }
        self
    }

    /// This message as an error that `cause` caused. It reads as the message alone, and
    /// `cause` follows in its chain, as with a context of `anyhow`: `{:#}` gives
    /// `<message>: <cause>`. [`Marked::from_error`] finds the pieces again, those of `cause`
    /// too when it is a message itself (one returned with `.into()`, or by `because`): the
    /// cause is kept as the error it is.
    pub fn because(self, cause: impl Into<anyhow::Error>) -> anyhow::Error {
        anyhow::Error::new(Caused {
            said: self,
            cause: cause.into(),
        })
    }

    /// What `error` says with its causes, the string that `{error:#}` gives byte for byte, in
    /// pieces. A cause that is a message (returned as an error with `.into()` or
    /// [`Marked::because`]), wherever it is in the chain, keeps its pieces, with remuda's `: `
    /// before and after it. Every other cause is text from elsewhere, and causes of that kind
    /// that follow each other stay one text, `: ` included: an error nobody marked is one
    /// piece, masked as it was as a string.
    pub fn from_error(error: &anyhow::Error) -> Marked {
        let mut said = Marked::default();
        // The causes nobody marked that follow each other, so far. One that says nothing
        // still has its `: `, so this is not told from the text being empty.
        let mut text: Option<String> = None;
        for (i, cause) in error.chain().enumerate() {
            let marked = cause
                .downcast_ref::<Marked>()
                .or_else(|| cause.downcast_ref::<Caused>().map(|c| &c.said));
            match (marked, &mut text) {
                (Some(marked), _) => {
                    said = said.text(text.take().unwrap_or_default());
                    if i > 0 {
                        said = said.words(": ");
                    }
                    said = said.join(marked);
                }
                (None, Some(text)) => {
                    text.push_str(": ");
                    text.push_str(&cause.to_string());
                }
                (None, None) => {
                    if i > 0 {
                        said = said.words(": ");
                    }
                    text = Some(cause.to_string());
                }
            }
        }
        said.text(text.unwrap_or_default())
    }

    /// Each piece in order, and what it is.
    pub fn pieces(&self) -> impl Iterator<Item = (&str, Piece)> {
        let mut start = 0;
        self.pieces.iter().map(move |(end, kind)| {
            let piece = &self.text[start..*end];
            start = *end;
            (piece, *kind)
        })
    }
}

impl From<String> for Marked {
    fn from(text: String) -> Self {
        Marked::default().text(text)
    }
}

impl From<&str> for Marked {
    fn from(text: &str) -> Self {
        Marked::default().text(text)
    }
}

impl Deref for Marked {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

impl Display for Marked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// A message is an error as it is: `Err(message.into())` where a function returns
/// `anyhow::Result`. It prints as the string it reads as, so what the command line shows does
/// not change, and [`Marked::from_error`] gives the TUI its pieces back.
impl std::error::Error for Marked {}

/// [`Marked::because`]: a message, and the error that caused it.
#[derive(Debug)]
struct Caused {
    said: Marked,
    /// As the error it is, not boxed again: its own type is what [`Marked::from_error`]
    /// knows a message by.
    cause: anyhow::Error,
}

impl Display for Caused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.said, f)
    }
}

impl std::error::Error for Caused {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

impl PartialEq<str> for Marked {
    fn eq(&self, other: &str) -> bool {
        self.text == other
    }
}

impl PartialEq<&str> for Marked {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

impl PartialEq<String> for Marked {
    fn eq(&self, other: &String) -> bool {
        self.text == *other
    }
}

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

    /// R21: a message reads as the one string `format!` would give, and keeps the pieces it
    /// was made of: remuda's words, each path, each text from elsewhere.
    #[test]
    fn a_message_reads_as_one_string_and_keeps_its_pieces() {
        let said = Marked::default()
            .words("cannot read ")
            .path("/h/a, b (old)/config.toml")
            .words(": ")
            .text("No such file or directory (os error 2)")
            .words("; shared configuration unknown");
        let whole = "cannot read /h/a, b (old)/config.toml: No such file or directory (os error \
                     2); shared configuration unknown";
        assert_eq!(said.as_str(), whole);
        assert_eq!(said.to_string(), whole);
        assert_eq!(format!("! {said}"), format!("! {whole}"));
        assert_eq!(&*said, whole);
        assert_eq!(said, whole);
        assert_eq!(said, whole.to_string());
        assert_eq!(
            said.pieces().collect::<Vec<_>>(),
            [
                ("cannot read ", Piece::Words),
                ("/h/a, b (old)/config.toml", Piece::Path),
                (": ", Piece::Words),
                ("No such file or directory (os error 2)", Piece::Text),
                ("; shared configuration unknown", Piece::Words),
            ]
        );
        // Joined, each piece is what it was.
        let joined = Marked::default().words("x: ").join(&said);
        assert_eq!(joined.as_str(), format!("x: {whole}"));
        assert_eq!(
            joined.pieces().map(|(_, kind)| kind).collect::<Vec<_>>(),
            [
                Piece::Words,
                Piece::Words,
                Piece::Path,
                Piece::Words,
                Piece::Text,
                Piece::Words
            ]
        );
        // An empty piece is none.
        assert_eq!(
            Marked::default()
                .words("")
                .path("")
                .text("")
                .pieces()
                .count(),
            0
        );
        assert_eq!(Marked::default().as_str(), "");
    }

    /// R21: a string says nothing of who wrote it, so as a message it is text from elsewhere:
    /// private mode looks for paths in it. That words are remuda's own is always said.
    #[test]
    fn a_string_is_text_from_elsewhere_until_said_otherwise() {
        let text = [("see /login", Piece::Text)];
        assert_eq!(
            Marked::from("see /login").pieces().collect::<Vec<_>>(),
            text
        );
        assert_eq!(
            Marked::from("see /login".to_string())
                .pieces()
                .collect::<Vec<_>>(),
            text
        );
        assert_eq!(
            Marked::default()
                .text("see /login")
                .pieces()
                .collect::<Vec<_>>(),
            text
        );
        assert_eq!(
            Marked::default()
                .words("see /login")
                .pieces()
                .collect::<Vec<_>>(),
            [("see /login", Piece::Words)]
        );
        // The same string, and not the same message.
        assert_ne!(
            Marked::default().words("see /login"),
            Marked::from("see /login")
        );
    }

    /// R21: an error keeps the pieces of the messages in its chain, and reads as `anyhow`
    /// prints it, whichever way: with a message in place of a string nothing that is printed
    /// changes.
    #[test]
    fn an_error_keeps_the_pieces_of_its_messages() {
        use anyhow::Context;
        let io = || std::io::Error::other("Permission denied (os error 13)");
        let said = || {
            Marked::default()
                .words("cannot read ")
                .path("/h/a: b/c.json")
        };
        // A message with a cause prints as a context does.
        let plain = anyhow::Error::from(io()).context("cannot read /h/a: b/c.json");
        let marked = said().because(io());
        for (new, old) in [
            (format!("{marked}"), format!("{plain}")),
            (format!("{marked:#}"), format!("{plain:#}")),
            (format!("{marked:?}"), format!("{plain:?}")),
        ] {
            assert_eq!(new, old);
        }
        // Nobody marked `plain`: one text, the string it prints as.
        assert_eq!(
            Marked::from_error(&plain).pieces().collect::<Vec<_>>(),
            [(
                "cannot read /h/a: b/c.json: Permission denied (os error 13)",
                Piece::Text
            )]
        );
        let pieces = [
            ("cannot read ", Piece::Words),
            ("/h/a: b/c.json", Piece::Path),
            (": ", Piece::Words),
            ("Permission denied (os error 13)", Piece::Text),
        ];
        assert_eq!(
            Marked::from_error(&marked).pieces().collect::<Vec<_>>(),
            pieces
        );
        // A message alone is an error as it is.
        let alone: anyhow::Error = Marked::default()
            .path("/h/a: b/c.json")
            .words(" is not a JSON object")
            .into();
        assert_eq!(alone.to_string(), "/h/a: b/c.json is not a JSON object");
        assert_eq!(format!("{alone:#}"), alone.to_string());
        assert_eq!(
            format!("{alone:?}"),
            format!(
                "{:?}",
                anyhow::anyhow!("/h/a: b/c.json is not a JSON object")
            )
        );
        assert_eq!(
            Marked::from_error(&alone).pieces().collect::<Vec<_>>(),
            [
                ("/h/a: b/c.json", Piece::Path),
                (" is not a JSON object", Piece::Words)
            ]
        );
        // Under contexts nobody marked: those stay one text, `: ` included, and the message
        // keeps its pieces after remuda's `: `.
        let deep = Err::<(), _>(said().because(io()))
            .context("cannot plan the launch")
            .context("cannot start /bin/x")
            .unwrap_err();
        let told = Marked::from_error(&deep);
        assert_eq!(told.as_str(), format!("{deep:#}"));
        let mut expected = vec![
            ("cannot start /bin/x: cannot plan the launch", Piece::Text),
            (": ", Piece::Words),
        ];
        expected.extend(pieces);
        assert_eq!(told.pieces().collect::<Vec<_>>(), expected);
    }

    /// R21: a message keeps its pieces wherever it is in the chain of an error, also as the
    /// cause of another message, at any depth: `because` keeps its cause as the error it is.
    #[test]
    fn a_message_that_causes_a_message_keeps_its_pieces() {
        use anyhow::Context;
        let file = "/h/a: b/settings.json";
        let inner = || -> anyhow::Error {
            Marked::default()
                .path(file)
                .words(" is not a JSON object")
                .into()
        };
        let kept = [
            ("cannot plan", Piece::Words),
            (": ", Piece::Words),
            (file, Piece::Path),
            (" is not a JSON object", Piece::Words),
        ];
        // A message returned as an error, then the cause of another message.
        let outer = Marked::default().words("cannot plan").because(inner());
        let plain = inner().context("cannot plan");
        for (new, old) in [
            (format!("{outer}"), format!("{plain}")),
            (format!("{outer:#}"), format!("{plain:#}")),
            (format!("{outer:?}"), format!("{plain:?}")),
        ] {
            assert_eq!(new, old);
        }
        let told = Marked::from_error(&outer);
        assert_eq!(told.as_str(), format!("{outer:#}"));
        assert_eq!(told.pieces().collect::<Vec<_>>(), kept);
        // Under a context nobody marked, the same message keeps them too.
        assert_eq!(
            Marked::from_error(&plain).pieces().collect::<Vec<_>>(),
            [
                ("cannot plan", Piece::Text),
                (": ", Piece::Words),
                (file, Piece::Path),
                (" is not a JSON object", Piece::Words),
            ]
        );
        // Two messages with causes, one in the other, and the system's error at the end.
        let io = std::io::Error::other("Permission denied (os error 13)");
        let read = Marked::default()
            .words("cannot read ")
            .path(file)
            .because(io);
        let deep = Marked::default()
            .words("cannot plan the launch of ")
            .text("tools/x@market")
            .because(read);
        let told = Marked::from_error(&deep);
        assert_eq!(told.as_str(), format!("{deep:#}"));
        assert_eq!(
            told.pieces().collect::<Vec<_>>(),
            [
                ("cannot plan the launch of ", Piece::Words),
                ("tools/x@market", Piece::Text),
                (": ", Piece::Words),
                ("cannot read ", Piece::Words),
                (file, Piece::Path),
                (": ", Piece::Words),
                ("Permission denied (os error 13)", Piece::Text),
            ]
        );
        assert_eq!(deep.chain().count(), 3);
        // And a third level, with contexts between and around them.
        let deeper = Err::<(), _>(
            Marked::default().words("no launch").because(
                Err::<(), _>(deep)
                    .context("while starting /bin/x")
                    .unwrap_err(),
            ),
        )
        .context("outermost")
        .unwrap_err();
        let told = Marked::from_error(&deeper);
        assert_eq!(told.as_str(), format!("{deeper:#}"));
        assert_eq!(
            told.pieces()
                .filter(|(_, kind)| *kind == Piece::Path)
                .collect::<Vec<_>>(),
            [(file, Piece::Path)]
        );
        assert_eq!(
            told.pieces().map(|(_, kind)| kind).collect::<Vec<_>>(),
            [
                Piece::Text,  // outermost
                Piece::Words, // `: `
                Piece::Words, // no launch
                Piece::Words, // `: `
                Piece::Text,  // while starting /bin/x
                Piece::Words, // `: `
                Piece::Words, // cannot plan the launch of
                Piece::Text,  // the plugin
                Piece::Words, // `: `
                Piece::Words, // cannot read
                Piece::Path,
                Piece::Words, // `: `
                Piece::Text,  // the system's error
            ]
        );
    }

    /// `from_error` is `{:#}` byte for byte, whatever the causes say: one that says nothing
    /// still has its `: `, marked or not, first, last or in the middle.
    #[test]
    fn an_error_reads_the_same_in_pieces_when_a_cause_says_nothing() {
        use anyhow::Context;
        let leaf = || std::io::Error::other("leaf");
        let said = |words: &str| Marked::default().words(words);
        let cases: Vec<(anyhow::Error, &str)> = vec![
            (anyhow::anyhow!("leaf").context(""), ": leaf"),
            (Marked::default().because(leaf()), ": leaf"),
            (anyhow::anyhow!("").context("top"), "top: "),
            (said("top").because(anyhow::anyhow!("")), "top: "),
            (said("top").because(Marked::default()), "top: "),
            (anyhow::anyhow!("").context(""), ": "),
            (Marked::default().because(Marked::default()), ": "),
            (anyhow::Error::from(Marked::default()), ""),
            (anyhow::anyhow!(""), ""),
            (
                anyhow::anyhow!("leaf").context("").context("top"),
                "top: : leaf",
            ),
            (
                said("top").because(Marked::default().because(leaf())),
                "top: : leaf",
            ),
            (
                said("top").because(anyhow::anyhow!("leaf").context("")),
                "top: : leaf",
            ),
            (
                Err::<(), _>(Marked::default().because(leaf()))
                    .context("")
                    .context("top")
                    .unwrap_err(),
                "top: : : leaf",
            ),
            (
                Err::<(), _>(said("mid").because(anyhow::anyhow!("")))
                    .context("")
                    .unwrap_err(),
                ": mid: ",
            ),
        ];
        for (error, whole) in cases {
            assert_eq!(format!("{error:#}"), whole, "{error:?}");
            let told = Marked::from_error(&error);
            assert_eq!(told.as_str(), whole, "{:?}", told);
            // No piece is lost or doubled on the way: they add up to the string.
            let pieces: String = told.pieces().map(|(piece, _)| piece).collect();
            assert_eq!(pieces, whole);
        }
        // Causes nobody marked that follow each other are still one text, an empty one among
        // them or not.
        let unmarked = anyhow::anyhow!("leaf /a/b").context("").context("top /c");
        assert_eq!(
            Marked::from_error(&unmarked).pieces().collect::<Vec<_>>(),
            [("top /c: : leaf /a/b", Piece::Text)]
        );
    }

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
