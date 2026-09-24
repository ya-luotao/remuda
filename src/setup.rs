//! `remuda setup [--provider <p>] <name>`: a fresh home under
//! `$REMUDA_HOME/homes/<provider>/<name>` (SPEC R3, R5, R13, R17). Logging in is left to the
//! agent (`claude auth login`, `codex login`). The command line and the TUI take the same
//! steps: [`plan`] (with [`Provider::login_args`]), [`create_and_register`], then the login in
//! the foreground.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

use crate::provider::Provider;
use crate::registry::{self, Account, Home, Registry};
use crate::{Env, paths};

/// Validates everything `setup` needs before any side effect and returns the new account.
pub fn plan(config: &Path, provider: Provider, name: &str, env: &Env) -> Result<Account> {
    registry::check_new_name(name)?;
    let dir = paths::remuda_home(env)?
        .join("homes")
        .join(provider.name())
        .join(name);
    let home = dir
        .to_str()
        .ok_or_else(|| anyhow!("home path is not valid UTF-8: {}", dir.display()))?
        .to_string();
    paths::check_home_string(&home)?;
    let account = Account {
        provider,
        name: name.to_string(),
        home: Home::Path(home.clone()),
    };
    Registry::load(config)?.check_available(&account)?;
    if fs::symlink_metadata(&dir).is_ok() {
        bail!("{home} already exists; to register an existing directory use `remuda add`");
    }
    Ok(account)
}

/// Creates the planned home and registers it. A home that was created but could not be
/// registered is reported as such (it is left in place: remuda never deletes homes, R2).
pub fn create_and_register(config: &Path, account: &Account) -> Result<()> {
    let home = account.home.to_string();
    create_home(&home)?;
    registry::register(config, account)
        .with_context(|| format!("created {home} but could not register it"))
}

/// The login command as the user would type it: `claude auth login`, `codex login`.
pub fn login_command(provider: Provider) -> String {
    let args = provider.login_args(None).unwrap_or_default();
    format!("{} {}", provider.program(), args.join(" "))
}

/// How to name a new account in messages and commands: claude's by its bare name (as before
/// codex) unless another provider has an account of that name (`ambiguous`: the bare name
/// would not resolve, R1), others as `provider:name`.
pub fn reference(provider: Provider, name: &str, ambiguous: bool) -> String {
    match provider {
        Provider::Claude if !ambiguous => name.to_string(),
        other => format!("{other}:{name}"),
    }
}

/// How to log in again after a failed login: `remuda run work auth login`,
/// `remuda run claude:work auth login` (`ambiguous`, see [`reference`]),
/// `remuda run codex:work login`.
pub fn retry_command(provider: Provider, name: &str, ambiguous: bool) -> String {
    let args = provider.login_args(None).unwrap_or_default();
    format!(
        "remuda run {} {}",
        reference(provider, name, ambiguous),
        args.join(" ")
    )
}

/// Whether a bare `name` is ambiguous for the `provider` account of that name: another
/// provider has an account of the same name among `accounts` (R1).
pub fn ambiguous<'a>(
    provider: Provider,
    name: &str,
    accounts: impl IntoIterator<Item = &'a Account>,
) -> bool {
    accounts
        .into_iter()
        .any(|a| a.name == name && a.provider != provider)
}

/// Creates the (empty) home directory with mode 0700; its parents are created as needed.
pub fn create_home(home: &str) -> Result<()> {
    let dir = Path::new(home);
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    fs::DirBuilder::new()
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("cannot create {home}"))?;
    // The umask may have removed bits; set the mode explicitly.
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot set permissions on {home}"))
}
