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
    /// `[share.claude]` and the accounts that opt out of it (R3, R18).
    pub sharing: Sharing,
}

/// Shared configuration (R18): whose configuration other claude accounts get.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sharing {
    /// The account named by `[share.claude] from`; `None`: nothing is shared.
    pub source: Option<Account>,
    /// `provider:name` of every account with `share = false`.
    pub opted_out: Vec<String>,
}

impl Sharing {
    /// The source whose configuration `account` gets: every claude account other than the
    /// source itself, unless it sets `share = false` (R18).
    pub fn source_for(&self, account: &Account) -> Option<&Account> {
        let source = self.source.as_ref()?;
        let qualified = account.qualified();
        let member = account.provider == CLAUDE
            && qualified != source.qualified()
            && !self.opted_out.contains(&qualified);
        member.then_some(source)
    }
}

impl Registry {
    /// Loads `config.toml`; a missing file is an empty registry.
    pub fn load(config: &Path) -> Result<Self> {
        let doc = read_document(config)?;
        Self::from_document(&doc).with_context(|| format!("invalid {}", config.display()))
    }

    /// Parses and validates a config document.
    pub fn from_document(doc: &DocumentMut) -> Result<Self> {
        let mut registry = Registry::default();
        if let Some(item) = doc.get("account") {
            let Some(tables) = item.as_array_of_tables() else {
                bail!("`account` must be an array of tables ([[account]])");
            };
            registry.parse_accounts(tables)?;
        }
        registry.sharing.source = registry.share_source(doc)?;
        Ok(registry)
    }

    fn parse_accounts(&mut self, tables: &ArrayOfTables) -> Result<()> {
        let accounts = &mut self.accounts;
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
            // `share = false` opts a claude account out of shared configuration (R18).
            if let Some(share) = table.get("share") {
                let Some(share) = share.as_bool() else {
                    bail!("account {provider}:{name}: `share` must be true or false");
                };
                if provider != CLAUDE {
                    bail!(
                        "account {provider}:{name}: `share` applies to claude accounts only \
                         (shared configuration is claude's)"
                    );
                }
                if !share {
                    self.sharing.opted_out.push(format!("{provider}:{name}"));
                }
            }
            accounts.push(Account {
                provider,
                name: name.to_string(),
                home: Home::Path(home.to_string()),
            });
        }
        Ok(())
    }

    /// `[share.claude] from`: `name` or `claude:name` of a claude account, registered or the
    /// implicit `default` (R3, R18).
    fn share_source(&self, doc: &DocumentMut) -> Result<Option<Account>> {
        let Some(share) = doc.get("share") else {
            return Ok(None);
        };
        let Some(share) = share.as_table_like() else {
            bail!("`share` must be a table ([share.claude])");
        };
        let Some(claude) = share.get(CLAUDE.name()) else {
            return Ok(None);
        };
        let Some(claude) = claude.as_table_like() else {
            bail!("`share.claude` must be a table ([share.claude])");
        };
        let Some(from) = claude.get("from").and_then(|v| v.as_str()) else {
            bail!("[share.claude]: missing or non-string `from`");
        };
        let name = from.strip_prefix("claude:").unwrap_or(from);
        match self
            .with_defaults(|_| true)
            .into_iter()
            .find(|a| a.provider == CLAUDE && a.name == name)
        {
            Some(source) => Ok(Some(source)),
            None => bail!(
                "[share.claude]: from = {from:?} names no claude account (known: {})",
                qualified_list(
                    &self
                        .with_defaults(|p| p == CLAUDE)
                        .into_iter()
                        .filter(|a| a.provider == CLAUDE)
                        .collect::<Vec<_>>()
                )
            ),
        }
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

/// `remuda remove` (R14a): deletes `account`'s `[[account]]` table from `config`, atomically
/// and keeping every other comment and unknown key (R3). The home is not touched (R2).
/// `default` is implicit, and the source of shared configuration cannot go while
/// `[share.claude] from` names it: the registry would no longer load.
pub fn unregister(config: &Path, account: &Account) -> Result<()> {
    let q = account.qualified();
    if account.home == Home::Default {
        bail!("{q} is the native login: it is implicit and cannot be removed");
    }
    let mut doc = read_document(config)?;
    let registry =
        Registry::from_document(&doc).with_context(|| format!("invalid {}", config.display()))?;
    if registry.sharing.source.as_ref() == Some(account) {
        bail!(
            "{q} is the source of shared configuration ([share.claude] from in {}); change or \
             remove `from` first",
            config.display()
        );
    }
    // The tables are in `accounts` order (`parse_accounts`).
    let Some(index) = registry.accounts.iter().position(|a| a == account) else {
        if let Some(other) = registry
            .accounts
            .iter()
            .find(|a| a.provider == account.provider && a.name == account.name)
        {
            bail!(
                "{q} is now registered with home {}; not removed",
                other.home
            );
        }
        bail!("{q} is not registered");
    };
    remove_account_table(&mut doc, index);
    Registry::from_document(&doc)
        .with_context(|| format!("removing {q} would leave {} invalid", config.display()))?;
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

/// Removes the `index`th `[[account]]` table (R3, R14a); the caller guarantees it exists. Its
/// own comments go with it: those inside it and the comment lines directly above its header.
/// A comment block separated from the header by a blank line is kept, moved in front of the
/// next table in the file, or to the end of the file when none follows.
pub fn remove_account_table(doc: &mut DocumentMut, index: usize) {
    let Some(tables) = doc
        .get_mut("account")
        .and_then(Item::as_array_of_tables_mut)
    else {
        return;
    };
    let removed = tables.remove(index);
    if tables.is_empty() {
        doc.remove("account");
    }
    let prefix = removed
        .decor()
        .prefix()
        .and_then(|p| p.as_str())
        .unwrap_or("");
    let kept = after_last_blank_line(prefix).map_or("", |end| &prefix[..end]);
    if kept.trim().is_empty() {
        return;
    }
    let kept = kept.to_string();
    if let Some(p) = removed
        .position()
        .and_then(|p| next_position(doc.as_table(), p))
        && prepend_at(doc.as_table_mut(), p, &kept)
    {
        return;
    }
    let old = doc.trailing().as_str().unwrap_or("").to_string();
    doc.set_trailing(join(&kept, &old));
}

/// The smallest position after `after` of an explicit table anywhere in `t`.
fn next_position(t: &Table, after: isize) -> Option<isize> {
    let mut best: Option<isize> = None;
    let mut consider = |sub: &Table| {
        let own = sub.position().filter(|&p| !sub.is_implicit() && p > after);
        for p in own.into_iter().chain(next_position(sub, after)) {
            if best.is_none_or(|b| p < b) {
                best = Some(p);
            }
        }
    };
    for (_, item) in t.iter() {
        match item {
            Item::Table(sub) => consider(sub),
            Item::ArrayOfTables(tables) => tables.iter().for_each(&mut consider),
            _ => {}
        }
    }
    best
}

/// Prepends `text` to the decor prefix of the explicit table at `pos` in `t`; whether it was
/// found.
fn prepend_at(t: &mut Table, pos: isize, text: &str) -> bool {
    for (_, item) in t.iter_mut() {
        let subs: Vec<&mut Table> = match item {
            Item::Table(sub) => vec![sub],
            Item::ArrayOfTables(tables) => tables.iter_mut().collect(),
            _ => vec![],
        };
        for sub in subs {
            if !sub.is_implicit() && sub.position() == Some(pos) {
                let old = sub.decor().prefix().and_then(|p| p.as_str()).unwrap_or("");
                let new = join(text, old);
                sub.decor_mut().set_prefix(new);
                return true;
            }
            if prepend_at(sub, pos, text) {
                return true;
            }
        }
    }
    false
}

/// `kept` (ending in a blank line) followed by `old` without doubling that blank line.
fn join(kept: &str, old: &str) -> String {
    let old = match old.split_inclusive('\n').next() {
        Some(first) if after_last_blank_line(kept) == Some(kept.len()) && is_blank_line(first) => {
            &old[first.len()..]
        }
        _ => old,
    };
    format!("{kept}{old}")
}

/// Where the last blank line of `text` ends (after its `\n`).
fn after_last_blank_line(text: &str) -> Option<usize> {
    let mut end = None;
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        at += line.len();
        if is_blank_line(line) {
            end = Some(at);
        }
    }
    end
}

/// A complete line (`\n` or `\r\n`) of nothing but spaces and tabs.
fn is_blank_line(line: &str) -> bool {
    line.strip_suffix('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .is_some_and(|l| l.bytes().all(|b| b == b' ' || b == b'\t'))
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
            ..Registry::default()
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
            ..Registry::default()
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
            ..Registry::default()
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
            ..Registry::default()
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
            ..Registry::default()
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
            ..Registry::default()
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

    /// R3, R18: `[share.claude] from` names a claude account (registered, or the implicit
    /// `default`), bare or as `claude:name`; `share = false` opts an account out.
    #[test]
    fn parses_shared_configuration() {
        let reg = parse(
            r#"
            [[account]]
            provider = "claude"
            name = "max"
            home = "/m"

            [[account]]
            provider = "claude"
            name = "solo"
            home = "/s"
            share = false

            [[account]]
            provider = "claude"
            name = "team"
            home = "/t"
            share = true

            [share.claude]
            from = "default"
            "#,
        )
        .unwrap();
        let sharing = &reg.sharing;
        assert_eq!(sharing.source, Some(Account::default_for(CLAUDE)));
        assert_eq!(sharing.opted_out, ["claude:solo"]);
        let default = Account::default_for(CLAUDE);
        assert_eq!(
            sharing.source_for(&default),
            None,
            "the source gets nothing"
        );
        assert_eq!(sharing.source_for(&reg.accounts[0]), Some(&default));
        assert_eq!(sharing.source_for(&reg.accounts[1]), None, "share = false");
        assert_eq!(sharing.source_for(&reg.accounts[2]), Some(&default));
        assert_eq!(
            sharing.source_for(&Account::default_for(CODEX)),
            None,
            "codex accounts are not affected"
        );

        for from in ["max", "claude:max"] {
            let reg = parse(&format!(
                "[[account]]\nprovider = \"claude\"\nname = \"max\"\nhome = \"/m\"\n\
                 [share.claude]\nfrom = \"{from}\"\n"
            ))
            .unwrap();
            assert_eq!(
                reg.sharing.source,
                Some(acc("claude", "max", "/m")),
                "{from}"
            );
            assert_eq!(reg.sharing.source_for(&reg.accounts[0]), None);
            assert_eq!(
                reg.sharing.source_for(&Account::default_for(CLAUDE)),
                Some(&acc("claude", "max", "/m"))
            );
        }
        // Without the table nothing is shared; `share = false` alone is harmless.
        let reg = parse(
            "[[account]]\nprovider = \"claude\"\nname = \"max\"\nhome = \"/m\"\nshare = false\n",
        )
        .unwrap();
        assert_eq!(reg.sharing.source, None);
        assert_eq!(reg.sharing.source_for(&reg.accounts[0]), None);
        assert_eq!(parse("[share]\n").unwrap().sharing.source, None);
    }

    /// R3: `share` on a codex account and a `from` that names no claude account are load
    /// errors, as are malformed values.
    #[test]
    fn rejects_invalid_shared_configuration() {
        let codex = "[[account]]\nprovider = \"codex\"\nname = \"cx\"\nhome = \"/c\"\n";
        let cases = [
            (
                format!("{codex}share = false\n"),
                "`share` applies to claude accounts only",
            ),
            (
                format!("{codex}share = true\n"),
                "`share` applies to claude accounts only",
            ),
            (
                "[[account]]\nprovider = \"claude\"\nname = \"x\"\nhome = \"/x\"\nshare = \"no\"\n"
                    .to_string(),
                "`share` must be true or false",
            ),
            (
                "[share.claude]\nfrom = \"nobody\"\n".to_string(),
                "from = \"nobody\" names no claude account (known: claude:default)",
            ),
            (
                format!("{codex}[share.claude]\nfrom = \"cx\"\n"),
                "names no claude account",
            ),
            (
                format!("{codex}[share.claude]\nfrom = \"codex:cx\"\n"),
                "names no claude account",
            ),
            (
                "[share.claude]\nfrom = \"codex:default\"\n".to_string(),
                "names no claude account",
            ),
            (
                "[share.claude]\nfrom = 1\n".to_string(),
                "missing or non-string `from`",
            ),
            (
                "[share.claude]\n".to_string(),
                "missing or non-string `from`",
            ),
            ("share = 1\n".to_string(), "`share` must be a table"),
            (
                "[share]\nclaude = \"default\"\n".to_string(),
                "`share.claude` must be a table",
            ),
        ];
        for (text, want) in cases {
            let err = format!("{:#}", parse(&text).unwrap_err());
            assert!(err.contains(want), "{text}\n=> {err}");
        }
    }

    /// Adding an account keeps `[share.claude]`, `share = false` and their comments (R3).
    #[test]
    fn append_preserves_shared_configuration() {
        let original = "[[account]]\nprovider = \"claude\"\nname = \"max\"\nhome = \"/m\"\n\
                        share = false # not this one\n\n# shared from the native login\n\
                        [share.claude]\nfrom = \"default\" # the source\n";
        let mut doc: DocumentMut = original.parse().unwrap();
        append_account(&mut doc, &acc("claude", "team", "/t")).unwrap();
        let out = doc.to_string();
        for kept in [
            "share = false # not this one",
            "# shared from the native login",
            "from = \"default\" # the source",
        ] {
            assert!(out.contains(kept), "{kept:?} lost:\n{out}");
        }
        let reg = parse(&out).unwrap();
        assert_eq!(
            reg.accounts,
            [acc("claude", "max", "/m"), acc("claude", "team", "/t")]
        );
        assert_eq!(reg.sharing.source, Some(Account::default_for(CLAUDE)));
        assert_eq!(reg.sharing.opted_out, ["claude:max"]);
    }

    fn removed(text: &str, index: usize) -> String {
        let mut doc: DocumentMut = text.parse().unwrap();
        remove_account_table(&mut doc, index);
        doc.to_string()
    }

    /// R3, R14a: removing an account keeps every other comment, table and unknown key; its own
    /// comments (inside it, and directly above its header) go with it.
    #[test]
    fn remove_keeps_other_comments_and_unknown_keys() {
        let original = "# my accounts\nunknown_top = \"keep\"\n\n# about max\n[[account]]\n\
                        provider = \"claude\"\nname = \"max\"\nhome = \"/m\"\nextra = 42 # note\n\n\
                        # ---- codex ----\n\n# about work\n[[account]]\n# inside work\n\
                        provider = \"codex\"\nname = \"work\"\nhome = \"/c\"\n\n\
                        # shared from the native login\n[share.claude]\n\
                        from = \"default\" # the source\n# end of file\n";
        assert_eq!(
            removed(original, 0),
            "# my accounts\nunknown_top = \"keep\"\n\n# ---- codex ----\n\n# about work\n\
             [[account]]\n# inside work\nprovider = \"codex\"\nname = \"work\"\nhome = \"/c\"\n\n\
             # shared from the native login\n[share.claude]\nfrom = \"default\" # the source\n\
             # end of file\n"
        );
        assert_eq!(
            removed(original, 1),
            "# my accounts\nunknown_top = \"keep\"\n\n# about max\n[[account]]\n\
             provider = \"claude\"\nname = \"max\"\nhome = \"/m\"\nextra = 42 # note\n\n\
             # ---- codex ----\n\n# shared from the native login\n[share.claude]\n\
             from = \"default\" # the source\n# end of file\n"
        );
        let reg = parse(&removed(original, 0)).unwrap();
        assert_eq!(reg.accounts, [acc("codex", "work", "/c")]);
        assert_eq!(reg.sharing.source, Some(Account::default_for(CLAUDE)));
    }

    /// R3, R14a: a file header above the first account stays, also when it was the only one.
    #[test]
    fn remove_keeps_a_file_header() {
        assert_eq!(
            removed(
                "# header\n\n# about max\n[[account]]\nprovider = \"claude\"\nname = \"max\"\n\
                 home = \"/m\"\n\n[[account]]\nprovider = \"claude\"\nname = \"team\"\n\
                 home = \"/t\"\n",
                0
            ),
            "# header\n\n[[account]]\nprovider = \"claude\"\nname = \"team\"\nhome = \"/t\"\n"
        );
        let out = removed(
            "# header\n\n[[account]]\nprovider = \"claude\"\nname = \"max\"\nhome = \"/m\"\n\
             # end\n",
            0,
        );
        assert_eq!(out, "# header\n\n# end\n");
        let doc: DocumentMut = out.parse().unwrap();
        assert!(doc.get("account").is_none());
        assert_eq!(Registry::from_document(&doc).unwrap(), Registry::default());

        // A blank line with CRLF endings, or with only spaces and tabs, separates as well
        // (CRLF comes out as LF, as with `add`).
        let team = "[[account]]\nprovider = \"claude\"\nname = \"team\"\nhome = \"/t\"\n";
        for (text, want) in [
            (
                "# header\r\n\r\n# about max\r\n[[account]]\r\nprovider = \"claude\"\r\n\
                 name = \"max\"\r\nhome = \"/m\"\r\n\r\n[[account]]\r\nprovider = \"claude\"\r\n\
                 name = \"team\"\r\nhome = \"/t\"\r\n",
                format!("# header\n\n{team}"),
            ),
            (
                "# header\n \t\n# about max\n[[account]]\nprovider = \"claude\"\nname = \"max\"\n\
                 home = \"/m\"\n\n[[account]]\nprovider = \"claude\"\nname = \"team\"\n\
                 home = \"/t\"\n",
                format!("# header\n \t\n{team}"),
            ),
        ] {
            let out = removed(text, 0);
            assert_eq!(out, want);
            assert_eq!(parse(&out).unwrap().accounts, [acc("claude", "team", "/t")]);
        }
        let out = removed(
            "# header\n  \n[[account]]\nprovider = \"claude\"\nname = \"max\"\nhome = \"/m\"\n",
            0,
        );
        assert!(out.starts_with("# header\n"), "{out:?}");
        assert_eq!(parse(&out).unwrap(), Registry::default());
    }

    /// R14a: `default`, the share source, an unregistered account and a stale home are refused;
    /// the file is left as it was.
    #[test]
    fn unregister_refusals() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let accounts = "[[account]]\nprovider = \"claude\"\nname = \"max\"\nhome = \"/m\"\n";
        let refused = |text: &str, account: &Account| {
            fs::write(&config, text).unwrap();
            let e = format!("{:#}", unregister(&config, account).unwrap_err());
            assert_eq!(fs::read_to_string(&config).unwrap(), text, "{e}");
            e
        };
        for provider in [CLAUDE, CODEX] {
            let e = refused(accounts, &Account::default_for(provider));
            assert!(e.contains("implicit"), "{e}");
        }
        for from in ["max", "claude:max"] {
            let text = format!("{accounts}\n[share.claude]\nfrom = \"{from}\"\n");
            let e = refused(&text, &acc("claude", "max", "/m"));
            assert!(e.contains("[share.claude]"), "{e}");
        }
        let e = refused(accounts, &acc("claude", "team", "/t"));
        assert_eq!(e, "claude:team is not registered");
        let e = refused(accounts, &acc("codex", "max", "/m"));
        assert_eq!(e, "codex:max is not registered");
        let e = refused(accounts, &acc("claude", "max", "/elsewhere"));
        assert!(e.contains("now registered with home /m"), "{e}");

        fs::write(&config, accounts).unwrap();
        unregister(&config, &acc("claude", "max", "/m")).unwrap();
        assert_eq!(fs::read_to_string(&config).unwrap(), "");
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
