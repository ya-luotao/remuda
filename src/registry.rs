//! Account registry: `$REMUDA_HOME/config.toml` (SPEC R1, R3, R14).

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

use crate::provider::Provider;
use crate::{Env, paths};

/// Shorthands for the providers (R4).
pub const CLAUDE: Provider = Provider::Claude;
pub const CODEX: Provider = Provider::Codex;
/// Reserved account name for a provider's native login (R1).
pub const DEFAULT_NAME: &str = "default";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Home {
    /// Native login: the isolation variable is unset (R2).
    Default,
    /// Registered home directory, stored and passed on byte-exact (R2).
    Path(String),
}

impl fmt::Display for Home {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Home::Default => f.write_str(DEFAULT_NAME),
            Home::Path(p) => f.write_str(p),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub provider: Provider,
    pub name: String,
    pub home: Home,
}

impl Account {
    pub fn default_for(provider: Provider) -> Self {
        Account {
            provider,
            name: DEFAULT_NAME.to_string(),
            home: Home::Default,
        }
    }

    /// `provider:name`.
    pub fn qualified(&self) -> String {
        format!("{}:{}", self.provider, self.name)
    }

    /// The account's home directory: the registered path, or the provider's native directory
    /// (`$HOME/.claude`, `$HOME/.codex`) for `default`. `None` when `HOME` is unknown.
    pub fn home_dir(&self, env: &Env) -> Option<PathBuf> {
        match &self.home {
            Home::Path(home) => Some(PathBuf::from(home)),
            Home::Default => self.provider.native_home(env),
        }
    }

    /// The account's `.claude.json`: `<home>/.claude.json`, or `$HOME/.claude.json` for the
    /// native login (R10a, R10). `None` when `HOME` is unknown.
    pub fn claude_json(&self, env: &Env) -> Option<PathBuf> {
        match &self.home {
            Home::Path(home) => Some(Path::new(home).join(".claude.json")),
            Home::Default => paths::user_home(env).map(|h| h.join(".claude.json")),
        }
    }
}

/// Registered accounts, in file order. The implicit defaults are not stored here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Registry {
    pub accounts: Vec<Account>,
}

impl Registry {
    /// Loads `config.toml`; a missing file is an empty registry.
    pub fn load(config: &Path) -> Result<Self> {
        let doc = read_document(config)?;
        Self::from_document(&doc).with_context(|| format!("invalid {}", config.display()))
    }

    /// Parses and validates a config document.
    pub fn from_document(doc: &DocumentMut) -> Result<Self> {
        let Some(item) = doc.get("account") else {
            return Ok(Registry::default());
        };
        let Some(tables) = item.as_array_of_tables() else {
            bail!("`account` must be an array of tables ([[account]])");
        };
        let mut accounts: Vec<Account> = Vec::new();
        for (i, table) in tables.iter().enumerate() {
            let field = |key: &str| {
                table
                    .get(key)
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("account #{}: missing or non-string `{key}`", i + 1))
            };
            let (provider, name, home) = (field("provider")?, field("name")?, field("home")?);
            let provider = Provider::parse(provider).ok_or_else(|| {
                anyhow!(
                    "account #{}: unknown provider {provider:?} (known: {})",
                    i + 1,
                    Provider::ALL.map(Provider::name).join(", ")
                )
            })?;
            check_name(name).with_context(|| format!("account #{}", i + 1))?;
            if name == DEFAULT_NAME {
                bail!(
                    "account #{}: the name `default` is reserved for the native login",
                    i + 1
                );
            }
            if home == DEFAULT_NAME {
                bail!("account {provider}:{name}: only `default` may use home = \"default\"");
            }
            paths::check_home_string(home).with_context(|| format!("account {provider}:{name}"))?;
            if accounts
                .iter()
                .any(|a| a.provider == provider && a.name == name)
            {
                bail!("duplicate account {provider}:{name}");
            }
            accounts.push(Account {
                provider,
                name: name.to_string(),
                home: Home::Path(home.to_string()),
            });
        }
        Ok(Registry { accounts })
    }

    /// Every account to list, grouped by provider (claude first): the provider's implicit
    /// `default` when it is listed ([`Provider::default_listed`]: claude's always, codex's
    /// when codex is around, R17), then its registered accounts in file order.
    pub fn all(&self, env: &Env) -> Vec<Account> {
        self.with_defaults(|p| p.default_listed(env))
    }

    /// Every account `provider:name` can name: both implicit defaults, listed or not (R1).
    fn candidates(&self) -> Vec<Account> {
        self.with_defaults(|_| true)
    }

    fn with_defaults(&self, listed: impl Fn(Provider) -> bool) -> Vec<Account> {
        let mut all = Vec::new();
        for provider in Provider::ALL {
            if listed(provider) {
                all.push(Account::default_for(provider));
            }
            all.extend(
                self.accounts
                    .iter()
                    .filter(|a| a.provider == provider)
                    .cloned(),
            );
        }
        all
    }

    /// Resolves `name` or `provider:name` (R1). A bare `default` is always `claude:default`.
    pub fn resolve(&self, reference: &str) -> Result<Account> {
        if reference == DEFAULT_NAME {
            return Ok(Account::default_for(CLAUDE));
        }
        let (provider, name) = match reference.split_once(':') {
            Some((p, n)) => (Some(p), n),
            None => (None, reference),
        };
        let candidates = self.candidates();
        let matches: Vec<Account> = candidates
            .iter()
            .filter(|a| a.name == name && provider.is_none_or(|p| p == a.provider.name()))
            .cloned()
            .collect();
        match matches.as_slice() {
            [one] => Ok(one.clone()),
            [] => bail!(
                "unknown account {reference:?} (known: {})",
                qualified_list(&candidates)
            ),
            several => bail!(
                "account name {name:?} exists under several providers ({}); use provider:name",
                qualified_list(several)
            ),
        }
    }
}

fn qualified_list(accounts: &[Account]) -> String {
    accounts
        .iter()
        .map(Account::qualified)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Checks an account name against `[A-Za-z0-9_-]+`.
pub fn check_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !valid {
        bail!("invalid name {name:?}: must match [A-Za-z0-9_-]+");
    }
    Ok(())
}

#[derive(Debug)]
pub struct AddOutcome {
    pub account: Account,
    /// Non-fatal problems to show the user.
    pub warnings: Vec<String>,
}

/// `remuda add` (R14, R17): validates and appends an account to `config`, atomically.
///
/// The home string is stored exactly as given (after a one-time `~` expansion); the
/// directory itself is only inspected, never modified.
pub fn add(
    config: &Path,
    provider: Provider,
    name: &str,
    raw_path: &str,
    env: &Env,
) -> Result<AddOutcome> {
    check_new_name(name)?;

    let home = paths::expand_tilde(raw_path, env)?;
    paths::check_home_string(&home)?;
    match fs::metadata(&home) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => bail!("not a directory: {home}"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => bail!("directory does not exist: {home}"),
        Err(e) => return Err(e).with_context(|| format!("cannot access {home}")),
    }
    let canonical = fs::canonicalize(&home).with_context(|| format!("cannot resolve {home}"))?;
    if let Some(native) = provider.native_home(env)
        && fs::canonicalize(&native).is_ok_and(|c| c == canonical)
    {
        // A bare `default` is claude's (R1).
        let default = match provider {
            Provider::Claude => DEFAULT_NAME.to_string(),
            other => Account::default_for(other).qualified(),
        };
        bail!(
            "{home} is the native login directory ({}): that is the implicit `{default}` \
             account; use `{default}` instead of registering it under another name",
            native.display()
        );
    }

    let warnings = provider
        .home_warning(Path::new(&home))
        .into_iter()
        .collect();
    let account = Account {
        provider,
        name: name.to_string(),
        home: Home::Path(home),
    };
    register(config, &account)?;
    Ok(AddOutcome { account, warnings })
}

/// Refuses a name that is invalid or reserved for the native login (R1), or that starts with
/// `-`: `remuda run <account>` would read it as an option. (A registry that already has one
/// still loads.)
pub fn check_new_name(name: &str) -> Result<()> {
    check_name(name).context("invalid account name")?;
    if name.starts_with('-') {
        bail!(
            "invalid account name {name:?}: a name cannot start with `-` (`remuda run` would \
             read it as an option)"
        );
    }
    if name == DEFAULT_NAME {
        bail!("the name `default` is reserved for the native login");
    }
    Ok(())
}

/// Appends `account` to `config` atomically, after [`Registry::check_available`] (R3, R14).
pub fn register(config: &Path, account: &Account) -> Result<()> {
    let mut doc = read_document(config)?;
    Registry::from_document(&doc)
        .with_context(|| format!("invalid {}", config.display()))?
        .check_available(account)?;
    append_account(&mut doc, account)?;
    write_atomic(config, doc.to_string().as_bytes())
        .with_context(|| format!("cannot write {}", config.display()))
}

impl Registry {
    /// Refuses a taken name, or a home already registered under this or another spelling
    /// (R14). Different spellings of one directory would share its files under two Keychain
    /// entries; canonical paths are compared only here and never stored (R2). A home that
    /// does not exist yet can only match by its exact string.
    pub fn check_available(&self, account: &Account) -> Result<()> {
        let Home::Path(home) = &account.home else {
            bail!("the default account is implicit and never stored");
        };
        for existing in &self.accounts {
            if existing.provider == account.provider && existing.name == account.name {
                bail!("account {} already exists", existing.qualified());
            }
            if existing.home == account.home {
                bail!("{home} is already registered as {}", existing.qualified());
            }
        }
        let Ok(canonical) = fs::canonicalize(home) else {
            return Ok(());
        };
        for existing in &self.accounts {
            if let Home::Path(other) = &existing.home
                && fs::canonicalize(other).is_ok_and(|c| c == canonical)
            {
                bail!(
                    "{home} is the same directory as {} ({other})",
                    existing.qualified()
                );
            }
        }
        Ok(())
    }
}

/// Appends an `[[account]]` table, leaving everything else in the document untouched.
pub fn append_account(doc: &mut DocumentMut, account: &Account) -> Result<()> {
    let Home::Path(home) = &account.home else {
        bail!("the default account is implicit and never stored");
    };
    let mut table = Table::new();
    table.insert("provider", value(account.provider.name()));
    table.insert("name", value(account.name.as_str()));
    table.insert("home", value(home.as_str()));

    // A file's trailing comments are rendered after all tables; keep them where the user
    // wrote them by moving them in front of the new table.
    let trailing = doc.trailing().as_str().unwrap_or("").to_string();
    let item = doc
        .entry("account")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    let Some(tables) = item.as_array_of_tables_mut() else {
        bail!("`account` must be an array of tables ([[account]])");
    };
    if !trailing.is_empty() {
        table.decor_mut().set_prefix(format!("{trailing}\n"));
    }
    tables.push(table);
    doc.set_trailing("");
    Ok(())
}

fn read_document(config: &Path) -> Result<DocumentMut> {
    let text = match fs::read_to_string(config) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(DocumentMut::new()),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", config.display())),
    };
    text.parse()
        .with_context(|| format!("cannot parse {}", config.display()))
}

/// Replaces `path` via a temp file in the same directory plus `rename` (R3). A symlinked
/// `path` is written through, never replaced by a regular file; existing permissions are kept.
pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let target = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => fs::canonicalize(path)?,
        _ => path.to_path_buf(),
    };
    let dir = target
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", target.display()))?;
    fs::create_dir_all(dir)?;
    let file_name = target
        .file_name()
        .ok_or_else(|| anyhow!("{} has no file name", target.display()))?
        .to_string_lossy();
    let tmp = dir.join(format!(
        ".{file_name}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));

    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        if let Ok(meta) = fs::metadata(&target) {
            file.set_permissions(meta.permissions())?;
        }
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&tmp, &target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc(provider: &str, name: &str, home: &str) -> Account {
        Account {
            provider: Provider::parse(provider).unwrap(),
            name: name.into(),
            home: Home::Path(home.into()),
        }
    }

    fn parse(text: &str) -> Result<Registry> {
        Registry::from_document(&text.parse::<DocumentMut>().unwrap())
    }

    /// New names cannot start with `-` (`remuda run -x` would read them as options); the
    /// registry still loads one that does.
    #[test]
    fn new_names_cannot_start_with_a_dash() {
        for bad in ["-x", "--resume", "-"] {
            let e = format!("{:#}", check_new_name(bad).unwrap_err());
            assert!(e.contains("cannot start with `-`"), "{bad}: {e}");
        }
        assert!(check_new_name("x-").is_ok());
        let reg = parse(
            r#"
            [[account]]
            provider = "claude"
            name = "-x"
            home = "/h/x"
            "#,
        )
        .unwrap();
        assert_eq!(reg.accounts[0].name, "-x");
    }

    #[test]
    fn names() {
        for ok in ["max", "team-2", "A_b", "0"] {
            assert!(check_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "a.b", "a b", "a:b", "ünï", "a/b"] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn parses_accounts_in_order() {
        let reg = parse(
            r#"
            [[account]]
            provider = "claude"
            name = "max"
            home = "/h/max/"

            [[account]]
            provider = "claude"
            name = "team"
            home = "/h/team"
            "#,
        )
        .unwrap();
        assert_eq!(
            reg.accounts,
            [
                acc("claude", "max", "/h/max/"),
                acc("claude", "team", "/h/team")
            ]
        );
    }

    #[test]
    fn empty_document_is_empty_registry() {
        assert_eq!(parse("").unwrap(), Registry::default());
        assert_eq!(
            parse("# just a comment\nother = 1\n").unwrap(),
            Registry::default()
        );
    }

    #[test]
    fn rejects_invalid_entries() {
        let bad = [
            // reserved name
            "[[account]]\nprovider = \"claude\"\nname = \"default\"\nhome = \"/h\"\n",
            // named account pointing at the native login
            "[[account]]\nprovider = \"claude\"\nname = \"x\"\nhome = \"default\"\n",
            // relative or empty home
            "[[account]]\nprovider = \"claude\"\nname = \"x\"\nhome = \"rel/x\"\n",
            "[[account]]\nprovider = \"claude\"\nname = \"x\"\nhome = \"~/x\"\n",
            "[[account]]\nprovider = \"claude\"\nname = \"x\"\nhome = \"\"\n",
            // bad name
            "[[account]]\nprovider = \"claude\"\nname = \"a.b\"\nhome = \"/h\"\n",
            // missing field
            "[[account]]\nprovider = \"claude\"\nname = \"x\"\n",
            // wrong type
            "[[account]]\nprovider = \"claude\"\nname = \"x\"\nhome = 1\n",
            // not an array of tables
            "account = \"x\"\n",
            // duplicate within a provider
            "[[account]]\nprovider = \"claude\"\nname = \"x\"\nhome = \"/a\"\n\
             [[account]]\nprovider = \"claude\"\nname = \"x\"\nhome = \"/b\"\n",
        ];
        for text in bad {
            assert!(parse(text).is_err(), "should reject:\n{text}");
        }
    }

    #[test]
    fn all_lists_default_first() {
        let reg = Registry {
            accounts: vec![acc("claude", "max", "/m")],
        };
        let all = reg.all(&Env::new());
        assert_eq!(all[0], Account::default_for(CLAUDE));
        assert_eq!(all[0].home, Home::Default);
        assert_eq!(all[0].qualified(), "claude:default");
        assert_eq!(all[1], acc("claude", "max", "/m"));
    }

    #[test]
    fn resolves_references() {
        let reg = Registry {
            accounts: vec![acc("claude", "max", "/m"), acc("claude", "team", "/t")],
        };
        assert_eq!(reg.resolve("max").unwrap(), acc("claude", "max", "/m"));
        assert_eq!(
            reg.resolve("claude:team").unwrap(),
            acc("claude", "team", "/t")
        );
        assert_eq!(
            reg.resolve("default").unwrap(),
            Account::default_for(CLAUDE)
        );
        assert_eq!(
            reg.resolve("claude:default").unwrap(),
            Account::default_for(CLAUDE)
        );
    }

    #[test]
    fn unknown_reference_lists_known_accounts() {
        let reg = Registry {
            accounts: vec![acc("claude", "max", "/m")],
        };
        for bad in ["nope", "claude:nope", "codex:max", ""] {
            let msg = reg.resolve(bad).unwrap_err().to_string();
            assert!(
                msg.contains("claude:default") && msg.contains("claude:max"),
                "{bad:?}: {msg}"
            );
        }
    }

    #[test]
    fn bare_name_in_several_providers_is_ambiguous() {
        let reg = Registry {
            accounts: vec![acc("claude", "x", "/a"), acc("codex", "x", "/b")],
        };
        let msg = reg.resolve("x").unwrap_err().to_string();
        assert!(msg.contains("claude:x") && msg.contains("codex:x"), "{msg}");
        assert_eq!(reg.resolve("codex:x").unwrap(), acc("codex", "x", "/b"));
    }

    /// R1: a bare `default` is `claude:default`, even though every provider has one;
    /// `codex:default` resolves whether or not it is listed.
    #[test]
    fn bare_default_is_claude_and_codex_default_resolves() {
        let reg = Registry {
            accounts: vec![acc("claude", "max", "/m"), acc("codex", "max", "/c")],
        };
        assert_eq!(
            reg.resolve("default").unwrap(),
            Account::default_for(CLAUDE)
        );
        assert_eq!(
            reg.resolve("codex:default").unwrap(),
            Account::default_for(CODEX)
        );
        assert_eq!(
            reg.resolve("claude:default").unwrap(),
            Account::default_for(CLAUDE)
        );
        // Other bare names in both providers stay ambiguous.
        let msg = reg.resolve("max").unwrap_err().to_string();
        assert!(
            msg.contains("claude:max") && msg.contains("codex:max"),
            "{msg}"
        );
        assert_eq!(reg.resolve("codex:max").unwrap(), acc("codex", "max", "/c"));
    }

    /// Accounts are grouped by provider; `codex:default` only when codex is around (R17).
    #[test]
    fn all_groups_by_provider_with_codex_default_only_when_listed() {
        let reg = Registry {
            accounts: vec![
                acc("codex", "work", "/c/work"),
                acc("claude", "max", "/m"),
                acc("claude", "team", "/t"),
            ],
        };
        let names = |accounts: Vec<Account>| -> Vec<String> {
            accounts.iter().map(Account::qualified).collect()
        };
        assert_eq!(
            names(reg.all(&Env::new())),
            ["claude:default", "claude:max", "claude:team", "codex:work"]
        );
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".codex")).unwrap();
        let env: Env = [("HOME".to_string(), dir.path().display().to_string())].into();
        assert_eq!(
            names(reg.all(&env)),
            [
                "claude:default",
                "claude:max",
                "claude:team",
                "codex:default",
                "codex:work"
            ]
        );
    }

    #[test]
    fn unknown_providers_are_refused() {
        let err = parse("[[account]]\nprovider = \"gemini\"\nname = \"x\"\nhome = \"/h\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown provider \"gemini\""), "{err}");
    }

    #[test]
    fn append_preserves_comments_and_unknown_keys() {
        let original = "# my accounts\nunknown_top = \"keep\"\n\n[[account]]\n# the big one\nprovider = \"claude\"\nname = \"max\"\nhome = \"/m\"\nextra = 42 # note\n";
        let mut doc: DocumentMut = original.parse().unwrap();
        append_account(&mut doc, &acc("claude", "team", "/t/")).unwrap();
        let out = doc.to_string();
        assert!(out.starts_with(original), "prefix changed:\n{out}");
        let reg = parse(&out).unwrap();
        assert_eq!(
            reg.accounts,
            [acc("claude", "max", "/m"), acc("claude", "team", "/t/")]
        );
    }

    #[test]
    fn append_keeps_comment_only_file_in_front() {
        for original in [
            "# only a comment\n",
            "# a\n\n# b\n",
            "[[account]]\nprovider = \"claude\"\nname = \"max\"\nhome = \"/m\"\n# end of file\n",
        ] {
            let mut doc: DocumentMut = original.parse().unwrap();
            append_account(&mut doc, &acc("claude", "team", "/t")).unwrap();
            let out = doc.to_string();
            assert!(out.starts_with(original), "prefix changed:\n{out}");
            let reparsed = parse(&out).unwrap();
            assert_eq!(reparsed.accounts.last(), Some(&acc("claude", "team", "/t")));
        }
    }

    #[test]
    fn append_to_empty_document() {
        let mut doc = DocumentMut::new();
        append_account(&mut doc, &acc("claude", "max", "/m")).unwrap();
        assert_eq!(
            parse(&doc.to_string()).unwrap().accounts,
            [acc("claude", "max", "/m")]
        );
    }
}
