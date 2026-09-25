//! Shared configuration, injected at launch (SPEC R18): a claude account gets the source
//! account's instructions, settings, enabled plugins and auto-memory location as launch
//! options. Nothing is written into any home (R13); remuda keeps only the item links under
//! `$REMUDA_HOME/shared/claude/.claude/` and the injected settings in
//! `$REMUDA_HOME/state/settings/`.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::Env;
use crate::registry::{Account, Sharing};

/// With it set, `--add-dir` also loads `CLAUDE.md` from the added directory (R18).
pub const CLAUDE_MD_VAR: &str = "CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD";
/// The instruction items of a home that `--add-dir=$REMUDA_HOME/shared/claude` shares.
pub const INSTRUCTIONS: [&str; 4] = ["CLAUDE.md", "skills", "commands", "agents"];
/// The settings key that moves auto-memory.
pub const MEMORY_KEY: &str = "autoMemoryDirectory";
/// Roots longer than this many UTF-16 code units are truncated and hashed by claude
/// (**[unverified]** in R18): no auto-memory is injected for them.
pub const MAX_PROJECT_NAME: usize = 200;

/// `$REMUDA_HOME/shared/claude`, next to `config.toml` (R13, R18).
pub fn dir(config: &Path) -> PathBuf {
    config.with_file_name("shared").join("claude")
}

/// What a launch gets from the source account.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Shared {
    /// Options in the `--option=value` form, to go before the user's arguments.
    pub args: Vec<String>,
    /// Variables added to the child's environment.
    pub env: Vec<(String, String)>,
    /// One-line messages for the user (stderr for `run`, the status bar in the TUI).
    pub notices: Vec<String>,
}

impl Shared {
    /// The injected options as the launch log records them: names and value sizes, not the
    /// values (R18).
    pub fn logged(&self) -> Vec<Injected> {
        self.args
            .iter()
            .map(|arg| match arg.split_once('=') {
                Some((option, value)) => Injected {
                    option: option.to_string(),
                    bytes: value.len(),
                },
                None => Injected {
                    option: arg.clone(),
                    bytes: 0,
                },
            })
            .collect()
    }
}

/// One injected option in the launch log (R18).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Injected {
    pub option: String,
    /// Byte size of its value.
    pub bytes: usize,
}

/// What a session launch of an account gets from the source (R18), decided without writing
/// anything: [`apply`] turns it into launch options, and the accounts view shows it (R22).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// The source (`provider:name`) and its home; `None`: nothing is injected (not a member, or
    /// the source's home is missing).
    pub source: Option<(String, PathBuf)>,
    /// The home's instruction items against the source's: `--add-dir` when
    /// [`Instructions::needs_injection`].
    pub instructions: Instructions,
    /// Components the home already shares with the source (same realpath, R12): not injected.
    pub settings_shared: bool,
    pub memory_shared: bool,
    pub plugins_shared: bool,
    /// The content of the single `--settings` (authentication removed, `autoMemoryDirectory`
    /// included when remuda adds it); empty: no `--settings`.
    pub settings: Map<String, Value>,
    /// The `autoMemoryDirectory` remuda adds (also in `settings`), if any.
    pub memory: Option<String>,
    /// Each plugin the source enables, in order, injected as `--plugin-dir` or why not; empty
    /// when the plugins are shared by realpath or not part of the launch.
    pub plugins: Vec<PluginPlan>,
    /// Messages for the user that need no write (a `--settings` / `--setting-sources` given).
    pub notices: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginPlan {
    /// `name@marketplace`.
    pub name: String,
    pub outcome: Result<PluginDir, Skip>,
}

/// The source's install of a plugin, passed as `--plugin-dir`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginDir {
    pub path: PathBuf,
    pub version: Option<String>,
}

/// Why a plugin the source enables is not injected (R18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// `false` in the `enabledPlugins` of the home or of the project's settings.
    TurnedOff,
    /// The home has installed it itself in a way claude loads here.
    OwnInstall,
    /// The source has no `user` install whose path exists (R11).
    NotInstalled,
    /// The home's or the source's `installed_plugins.json` is not recognized (R11).
    UnrecognizedList,
}

/// What `account` gets from the source of `sharing` for a session launched with `user_args`
/// in `cwd` (R18). Nothing for the source itself, an account with `share = false`, a codex
/// account, or a source whose home is missing (R11 warns about that). Each component is
/// skipped when the home already shares it with the source (same realpath, R12). `config` is
/// `$REMUDA_HOME/config.toml`: the shared directory and the settings files live next to it.
///
/// Only a settings file of the source or the home that is not a JSON object fails: everything
/// else that cannot be read or made degrades to injecting less, with a notice where the user
/// would miss it.
pub fn inject(
    sharing: &Sharing,
    account: &Account,
    user_args: &[String],
    cwd: Option<&Path>,
    env: &Env,
    config: &Path,
) -> Result<Shared> {
    apply(&plan(sharing, account, user_args, cwd, env)?, config)
}

/// The reads of [`inject`], without its writes (R18, R22): what `account` gets from the source
/// of `sharing` for a session launched with `user_args` in `cwd`, and why each plugin the
/// source enables is or is not injected. Fails as [`inject`] does: only on a settings file of
/// the source or the home that is not a JSON object.
pub fn plan(
    sharing: &Sharing,
    account: &Account,
    user_args: &[String],
    cwd: Option<&Path>,
    env: &Env,
) -> Result<Plan> {
    let mut plan = Plan::default();
    let Some(source) = sharing.source_for(account) else {
        return Ok(plan);
    };
    let (Some(from), Some(home)) = (source.home_dir(env), account.home_dir(env)) else {
        return Ok(plan);
    };
    if !from.is_dir() {
        return Ok(plan);
    }
    let name = source.qualified();
    plan.source = Some((name.clone(), from.clone()));
    plan.instructions = instructions(&from, &home);

    // claude takes only the last `--settings`, and `--setting-sources` may leave out the
    // user's layer the shared settings stand in for: either way, the user decides (R18).
    let user_settings = user_args.iter().find_map(|a| {
        ["--settings", "--setting-sources"]
            .into_iter()
            .find(|o| a == o || a.strip_prefix(o).is_some_and(|v| v.starts_with('=')))
    });
    let own_path = home.join("settings.json");
    let settings_shared = resolves_to(&own_path, &from.join("settings.json"));
    let memory_shared = resolves_to(&home.join("projects"), &from.join("projects"));
    let plugins_shared = resolves_to(&home.join("plugins"), &from.join("plugins"));
    plan.settings_shared = settings_shared;
    plan.memory_shared = memory_shared;
    plan.plugins_shared = plugins_shared;
    let plugins_part = !plugins_shared;
    if let Some(option) = user_settings
        && !(settings_shared && memory_shared)
    {
        plan.notices.push(format!(
            "{option} given: settings and auto-memory from {name} are not injected"
        ));
    }
    let settings_part = !settings_shared && user_settings.is_none();
    let memory_part = !memory_shared && user_settings.is_none();
    // Before any settings file is read: a launch that shares everything by symlink does not
    // fail on a malformed one.
    if !(settings_part || memory_part || plugins_part) {
        return Ok(plan);
    }

    let source_settings = read_settings(&from.join("settings.json"))?;
    // The home's own settings: read only when it is a file that is not the source's (a
    // directory or a dangling link defines nothing).
    let own = if settings_shared {
        source_settings.clone()
    } else if fs::metadata(&own_path).is_ok_and(|m| m.is_file()) {
        read_settings(&own_path)?
    } else {
        Map::new()
    };

    let project = Project::locate(cwd);
    let layers = project.settings(env);
    if settings_part || memory_part {
        let mut injected = Map::new();
        if settings_part {
            injected = missing_from(&source_settings, &own);
            for layer in &layers {
                injected = missing_from(&injected, layer);
            }
            strip_auth(&mut injected);
        }
        // A location the source, the home or the project's local settings chose is kept.
        let chosen = source_settings.contains_key(MEMORY_KEY)
            || own.contains_key(MEMORY_KEY)
            || layers.iter().any(|l| l.contains_key(MEMORY_KEY));
        if memory_part
            && !chosen
            && let Some(memory) = project.memory_dir(&from)
        {
            injected.insert(MEMORY_KEY.to_string(), Value::String(memory.clone()));
            plan.memory = Some(memory);
        }
        plan.settings = injected;
    }

    if plugins_part {
        let installs = installed_plugins(&from);
        // What the home or the project turned off, or the home installed itself for this
        // directory, would otherwise load twice.
        let own_installs = installed_plugins(&home);
        // Which of them the home has cannot be told: nothing is injected (R11 warns).
        let own_unknown = matches!(own_installs, Installs::Unrecognized(_));
        let off = |plugin: &str| {
            std::iter::once(&own).chain(&layers).any(|settings| {
                settings
                    .get("enabledPlugins")
                    .and_then(|p| p.get(plugin))
                    .is_some_and(|on| on.as_bool() == Some(false))
            })
        };
        for plugin in enabled_plugins(&source_settings) {
            let outcome = if own_unknown {
                Err(Skip::UnrecognizedList)
            } else if off(&plugin) {
                Err(Skip::TurnedOff)
            } else if own_installs.installed_for(&plugin, project.start.as_deref()) {
                Err(Skip::OwnInstall)
            } else if matches!(installs, Installs::Unrecognized(_)) {
                Err(Skip::UnrecognizedList)
            } else {
                match installs.user_install(&plugin) {
                    Some(Install {
                        path: Some(path),
                        version,
                        ..
                    }) => Ok(PluginDir {
                        path: path.clone(),
                        version: version.clone(),
                    }),
                    _ => Err(Skip::NotInstalled),
                }
            };
            plan.plugins.push(PluginPlan {
                name: plugin,
                outcome,
            });
        }
    }
    Ok(plan)
}

/// The writes of [`inject`] for `plan` (R18): the `.claude` item links next to `config` when
/// instructions are injected, and the settings file. What cannot be made is left out of the
/// launch, with a notice.
pub fn apply(plan: &Plan, config: &Path) -> Result<Shared> {
    let mut shared = Shared::default();
    let Some((name, from)) = &plan.source else {
        return Ok(shared);
    };
    if plan.instructions.needs_injection() {
        let shared_dir = dir(config);
        match ensure_links(&shared_dir, from) {
            Ok(()) => {
                shared
                    .args
                    .push(format!("--add-dir={}", shared_dir.display()));
                shared
                    .env
                    .push((CLAUDE_MD_VAR.to_string(), "1".to_string()));
            }
            Err(e) => shared.notices.push(format!(
                "instructions from {name} are not shared this time: {e:#}"
            )),
        }
    }
    shared.notices.extend(plan.notices.iter().cloned());
    if !plan.settings.is_empty() {
        let json = serde_json::to_string(&Value::Object(plan.settings.clone()))?;
        match write_settings(&settings_dir(config), &json, SystemTime::now()) {
            Ok(path) => shared.args.push(format!("--settings={}", path.display())),
            Err(e) => shared.notices.push(format!(
                "settings from {name} are not shared this time: {e:#}"
            )),
        }
    }
    for plugin in &plan.plugins {
        if let Ok(install) = &plugin.outcome {
            shared
                .args
                .push(format!("--plugin-dir={}", install.path.display()));
        }
    }
    Ok(shared)
}

/// `$REMUDA_HOME/state/settings`: the injected settings, one file per content (R18).
pub fn settings_dir(config: &Path) -> PathBuf {
    config.with_file_name("state").join("settings")
}

/// Settings files not used for this long are removed when a new one is written (R18).
pub const SETTINGS_KEPT: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Writes `json` to `<dir>/<sha256 of json>.json`, mode 0600, atomically (R18); a file with
/// that name is reused, and its mtime set to `now` to mark it used. Writing a new file first
/// removes the files of `dir` not used since [`SETTINGS_KEPT`] before `now`.
pub fn write_settings(dir: &Path, json: &str, now: SystemTime) -> Result<PathBuf> {
    let name = format!("{:x}.json", Sha256::digest(json.as_bytes()));
    let path = dir.join(&name);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("cannot create {}", dir.display()))?;
    // Held until this returns: a file is never pruned between being chosen and being marked
    // used, by this remuda or another.
    let _lock = SettingsLock::exclusive(dir)?;
    if fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|f| f.set_modified(now))
            .with_context(|| format!("cannot use {}", path.display()))?;
        return Ok(path);
    }
    prune_settings(dir, now);
    let tmp = dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let written = (|| -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, &path)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("cannot write {}", path.display()));
    }
    Ok(path)
}

/// The lock file of the settings directory: never a settings file, so never pruned.
pub const SETTINGS_LOCK: &str = ".lock";

/// An exclusive `flock` on `<dir>/.lock` (a regular file, 0600), released when dropped. On a
/// file system without locking there is no lock, and remuda goes on without it (R18).
struct SettingsLock(Option<fs::File>);

impl SettingsLock {
    fn exclusive(dir: &Path) -> Result<SettingsLock> {
        let path = dir.join(SETTINGS_LOCK);
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        loop {
            // SAFETY: the descriptor is open for the call.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Ok(SettingsLock(Some(file)));
            }
            let e = io::Error::last_os_error();
            let code = e.raw_os_error().unwrap_or(0);
            if code == libc::EINTR {
                continue;
            }
            // Not a pattern: EOPNOTSUPP and ENOTSUP are one value on Linux.
            let unsupported = [libc::EBADF, libc::ENOLCK, libc::EOPNOTSUPP, libc::ENOTSUP];
            if unsupported.contains(&code) {
                return Ok(SettingsLock(None));
            }
            return Err(e).with_context(|| format!("cannot lock {}", path.display()));
        }
    }
}

impl Drop for SettingsLock {
    fn drop(&mut self) {
        if let Some(file) = &self.0 {
            // SAFETY: the descriptor is still open; closing it would release the lock anyway.
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

/// Removes the settings files of `dir` (`<64 hex digits>.json`) last used before
/// [`SETTINGS_KEPT`] ago; anything else there is left alone.
fn prune_settings(dir: &Path, now: SystemTime) {
    let Some(cutoff) = now.checked_sub(SETTINGS_KEPT) else {
        return;
    };
    let Ok(listing) = fs::read_dir(dir) else {
        return;
    };
    for item in listing.flatten() {
        let name = item.file_name();
        let ours = name
            .to_str()
            .and_then(|n| n.strip_suffix(".json"))
            .is_some_and(|stem| {
                stem.len() == 64 && stem.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            });
        let old = item
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t < cutoff);
        if ours && old && item.file_type().is_ok_and(|t| t.is_file()) {
            let _ = fs::remove_file(item.path());
        }
    }
}

/// Settings keys that choose credentials, provider or organization: never injected (R18).
pub const AUTH_KEYS: [&str; 8] = [
    "apiKeyHelper",
    "proxyAuthHelper",
    "otelHeadersHelper",
    "awsAuthRefresh",
    "awsCredentialExport",
    "gcpAuthRefresh",
    "forceLoginMethod",
    "forceLoginOrgUUID",
];
/// `env` names starting with one of these choose a provider, credentials, an endpoint or host
/// authentication (R18).
pub const AUTH_ENV_PREFIXES: [&str; 21] = [
    "ANTHROPIC_",
    "AWS_",
    "AZURE_",
    "GOOGLE_",
    "GCLOUD_",
    "GCE_",
    "CLOUDSDK_",
    "CLOUD_ML_",
    "VERTEX_",
    "METADATA_",
    "IDENTITY_",
    "IMDS_",
    "MSI_",
    "CLAUDE_CODE_USE_",
    "CLAUDE_CODE_SKIP_",
    "CLAUDE_CODE_HOST_",
    "CLAUDE_CODE_PROVIDER_",
    "CLAUDE_CODE_FEDERATION_",
    "CLAUDE_CODE_CERT",
    "CLAUDE_CODE_CLIENT_CERT",
    "_CLAUDE_CODE_",
];
/// `env` names containing one of these may hold a secret or an endpoint (R18).
pub const AUTH_ENV_PARTS: [&str; 10] = [
    "TOKEN",
    "KEY",
    "SECRET",
    "PASSWORD",
    "CREDENTIAL",
    "CREDS",
    "OAUTH",
    "UUID",
    "BASE_URL",
    "HEADERS",
];
/// `env` names withheld exactly: the account's own files (R2), and proxy URLs, which can carry
/// credentials (R18).
pub const AUTH_ENV_EXACT: [&str; 5] = [
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_SECURESTORAGE_CONFIG_DIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
];

/// Whether the settings `env` variable `name` is withheld from members (R18), matched
/// case-insensitively: an exact name, a prefix, a secret-like part, or an underscore-separated
/// part `AUTH`. Two kinds of names are exceptions to the one rule that would otherwise catch
/// them: the model-name variables (`ANTHROPIC_MODEL`, `ANTHROPIC_DEFAULT_MODEL`,
/// `ANTHROPIC_SMALL_FAST_MODEL`, `ANTHROPIC_DEFAULT_*_MODEL*`, `ANTHROPIC_CUSTOM_MODEL_OPTION*`,
/// excepted from the `ANTHROPIC_` prefix) and counts ending in `_TOKENS` (excepted from the
/// `TOKEN` part); any other rule still withholds them.
pub fn withheld_env(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if AUTH_ENV_EXACT.contains(&upper.as_str()) {
        return true;
    }
    let model = matches!(
        upper.as_str(),
        "ANTHROPIC_MODEL" | "ANTHROPIC_DEFAULT_MODEL" | "ANTHROPIC_SMALL_FAST_MODEL"
    ) || upper
        .strip_prefix("ANTHROPIC_DEFAULT_")
        .is_some_and(|rest| rest.contains("_MODEL"))
        || upper.starts_with("ANTHROPIC_CUSTOM_MODEL_OPTION");
    // The part of the name the part rules look at: a count's `_TOKENS` is not a token.
    let stem = upper.strip_suffix("_TOKENS").unwrap_or(&upper);
    let prefixed = !model && AUTH_ENV_PREFIXES.iter().any(|p| upper.starts_with(p));
    let part = AUTH_ENV_PARTS.iter().any(|p| stem.contains(p));
    let auth = stem.split('_').any(|p| p == "AUTH");
    prefixed || part || auth
}

/// The authentication settings `settings` has, as `key` or `env.VAR`: withheld from members
/// (R11, R18).
pub fn withheld(settings: &Map<String, Value>) -> Vec<String> {
    let mut keys: Vec<String> = AUTH_KEYS
        .iter()
        .filter(|k| settings.contains_key(**k))
        .map(|k| k.to_string())
        .collect();
    if let Some(Value::Object(vars)) = settings.get("env") {
        keys.extend(
            vars.keys()
                .filter(|k| withheld_env(k))
                .map(|k| format!("env.{k}")),
        );
    }
    keys
}

/// Removes the authentication settings from injected settings (an `env` left empty goes too).
fn strip_auth(settings: &mut Map<String, Value>) {
    for key in AUTH_KEYS {
        settings.remove(key);
    }
    if let Some(Value::Object(vars)) = settings.get_mut("env") {
        vars.retain(|name, _| !withheld_env(name));
        if vars.is_empty() {
            settings.remove("env");
        }
    }
}

/// Where a session starts, as claude sees it (R18): the launch directory made real and NFC,
/// and its project root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// `None` without a launch directory, or when it cannot be resolved.
    pub start: Option<PathBuf>,
    /// The project root of `start`, found as claude finds it ([`project_root`]).
    pub root: Option<PathBuf>,
}

impl Project {
    /// Resolves `cwd` and its project root; runs nothing.
    pub fn locate(cwd: Option<&Path>) -> Project {
        let start = cwd.and_then(start_dir);
        let root = start.as_deref().map(project_root);
        Project { start, root }
    }

    /// The project's settings that sit above the user's in claude's order (R18):
    /// `.claude/settings.json` and `.claude/settings.local.json` of the start directory, and
    /// the local settings of the project root when claude reads those too. Unreadable or
    /// invalid files count as absent.
    pub fn settings(&self, env: &Env) -> Vec<Map<String, Value>> {
        let Some(start) = &self.start else {
            return Vec::new();
        };
        let mut files = vec![
            start.join(".claude/settings.json"),
            start.join(".claude/settings.local.json"),
        ];
        if let Some(root) = &self.root
            && reads_root_local_settings(root, start, env)
        {
            files.push(root.join(".claude/settings.local.json"));
        }
        files
            .iter()
            .filter_map(|f| read_settings(f).ok())
            .filter(|m| !m.is_empty())
            .collect()
    }

    /// `<source>/projects/<project>/memory` (R18); `None` without a start directory, or when
    /// the name is too long for remuda to know claude's.
    pub fn memory_dir(&self, source: &Path) -> Option<String> {
        let project = encode_project(self.root.as_ref()?.to_str()?)?;
        Some(
            source
                .join("projects")
                .join(project)
                .join("memory")
                .display()
                .to_string(),
        )
    }
}

/// The start directory as claude takes its own cwd: absolute, real, NFC (R18).
pub fn start_dir(cwd: &Path) -> Option<PathBuf> {
    let real = fs::canonicalize(cwd).ok()?;
    Some(PathBuf::from(real.to_str()?.nfc().collect::<String>()))
}

/// claude also reads `.claude/settings.local.json` of the project root when it is not the
/// start directory, not the user's home, and it, its `.git` and its `.claude` (if any) belong
/// to the current user (read from the 2.1.281 bundle).
fn reads_root_local_settings(root: &Path, start: &Path, env: &Env) -> bool {
    if root == start {
        return false;
    }
    let home = crate::paths::user_home(env).and_then(|h| fs::canonicalize(h).ok());
    if home.as_deref() == Some(root) {
        return false;
    }
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    let uid = |p: &Path| fs::symlink_metadata(p).map(|m| m.uid());
    fs::metadata(root).is_ok_and(|m| m.uid() == me)
        && uid(&root.join(".git")).is_ok_and(|u| u == me)
        && match uid(&root.join(".claude")) {
            Ok(u) => u == me,
            Err(e) => e.kind() == io::ErrorKind::NotFound,
        }
}

/// Whether both paths exist and resolve to the same file or directory.
pub fn resolves_to(path: &Path, source: &Path) -> bool {
    match (fs::canonicalize(path), fs::canonicalize(source)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// How a home's instruction items relate to the source's: an item the source does not have
/// has nothing to share and counts as neither.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Instructions {
    /// Items that already resolve to the source's (symlinks).
    pub shared: Vec<&'static str>,
    /// Items of the source that the home does not reach.
    pub missing: Vec<&'static str>,
}

impl Instructions {
    /// `--add-dir` is injected unless every item the source has already resolves to it.
    pub fn needs_injection(&self) -> bool {
        !self.missing.is_empty()
    }

    /// Some items shared through symlinks and others not: with the injected `--add-dir`, the
    /// shared ones load twice (R11).
    pub fn partial(&self) -> bool {
        !self.shared.is_empty() && !self.missing.is_empty()
    }
}

pub fn instructions(source: &Path, home: &Path) -> Instructions {
    let mut out = Instructions::default();
    for item in INSTRUCTIONS {
        let from = source.join(item);
        if !from.exists() {
            continue;
        }
        if resolves_to(&home.join(item), &from) {
            out.shared.push(item);
        } else {
            out.missing.push(item);
        }
    }
    out
}

/// Makes `<dir>/.claude` a directory holding exactly one symlink per instruction item the
/// source has ([`INSTRUCTIONS`]), each pointing at `<source>/<item>` (R18). When it already
/// is, nothing is written. Inside an existing directory, a missing link is created, one with
/// another target is replaced atomically, and one for an item the source no longer has is
/// removed; an entry that is not a symlink is never replaced, and then nothing is changed.
/// Nothing else in `dir` or in `.claude` is touched. A `.claude` that is a symlink (the
/// earlier layout, a link to the whole source home) is migrated: the directory is built under
/// a temporary name in `dir`, the link is removed, and the directory is renamed into place.
pub fn ensure_links(dir: &Path, source: &Path) -> Result<()> {
    let root = dir.join(".claude");
    let old_link = match fs::symlink_metadata(&root) {
        Ok(meta) if meta.file_type().is_dir() => return fill(&root, source),
        Ok(meta) if meta.file_type().is_symlink() => true,
        Ok(_) => bail!(
            "{} is neither a directory nor a symlink; remuda does not replace it",
            root.display()
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(e).with_context(|| format!("cannot inspect {}", root.display())),
    };

    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let staged = Staged::build(dir, source)?;
    if old_link {
        // Only a symlink is removed, checked again right before: a concurrent remuda may have
        // put the directory in place already (then nothing is removed) and a directory or a
        // file is never removed.
        match fs::symlink_metadata(&root) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if let Err(e) = fs::remove_file(&root)
                    && e.kind() != io::ErrorKind::NotFound
                {
                    return match fs::symlink_metadata(&root) {
                        Ok(meta) if meta.file_type().is_dir() => fill(&root, source),
                        _ => Err(e).with_context(|| format!("cannot remove {}", root.display())),
                    };
                }
            }
            Ok(_) | Err(_) => {}
        }
    }
    // A directory cannot be renamed over a symlink or a file; over a directory only when it
    // is empty. On failure, the reason is read from what is there now, not from the errno
    // (which differs between systems): a directory means another remuda won the race, and
    // its items are checked instead.
    match fs::rename(&staged.path, &root) {
        Ok(()) => {
            staged.commit();
            Ok(())
        }
        Err(e) => match fs::symlink_metadata(&root) {
            Ok(meta) if meta.file_type().is_dir() => {
                drop(staged);
                fill(&root, source)
            }
            _ => {
                drop(staged);
                Err(e).with_context(|| format!("cannot create {}", root.display()))
            }
        },
    }
}

/// The item links of `source` under `root` (an existing `.claude` directory), changed only
/// where they differ: decided for all four items first, so an entry that is not a symlink
/// fails before anything is written, and a directory that is already right gets no write.
fn fill(root: &Path, source: &Path) -> Result<()> {
    enum Action {
        Create,
        Remove,
    }
    let mut actions = Vec::new();
    for item in INSTRUCTIONS {
        let link = root.join(item);
        let target = source.join(item);
        let wanted = fs::metadata(&target).is_ok();
        match fs::symlink_metadata(&link) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if !wanted {
                    actions.push((link, target, Action::Remove));
                } else if fs::read_link(&link).ok().as_deref() != Some(target.as_path()) {
                    actions.push((link, target, Action::Create));
                }
            }
            Ok(_) => bail!(
                "{} is not a symlink; remuda does not replace it",
                link.display()
            ),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if wanted {
                    actions.push((link, target, Action::Create));
                }
            }
            Err(e) => {
                return Err(e).with_context(|| format!("cannot inspect {}", link.display()));
            }
        }
    }
    for (link, target, action) in actions {
        match action {
            Action::Create => place_link(&target, &link)?,
            Action::Remove => {
                // Checked again right before: never anything but a symlink.
                if fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink())
                    && let Err(e) = fs::remove_file(&link)
                    && e.kind() != io::ErrorKind::NotFound
                {
                    return Err(e).with_context(|| format!("cannot remove {}", link.display()));
                }
            }
        }
    }
    Ok(())
}

/// A symlink to `target` at `link`, created or replaced atomically: a temporary link in the
/// same directory, renamed into place.
fn place_link(target: &Path, link: &Path) -> Result<()> {
    let name = link.file_name().and_then(|n| n.to_str()).unwrap_or("link");
    let tmp = link.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    symlink(target, &tmp).with_context(|| format!("cannot create {}", tmp.display()))?;
    if let Err(e) = fs::rename(&tmp, link) {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("cannot create {}", link.display()));
    }
    Ok(())
}

/// A `.claude` directory built under a temporary name, removed again when dropped without
/// [`Staged::commit`]: every error path after it exists cleans it up.
struct Staged {
    path: PathBuf,
    committed: bool,
}

impl Staged {
    /// `<dir>/.claude.<uuid>.tmp` holding a link for each item `source` has.
    fn build(dir: &Path, source: &Path) -> Result<Staged> {
        let path = dir.join(format!(".claude.{}.tmp", uuid::Uuid::new_v4().simple()));
        fs::create_dir(&path).with_context(|| format!("cannot create {}", path.display()))?;
        let staged = Staged {
            path,
            committed: false,
        };
        for item in INSTRUCTIONS {
            let target = source.join(item);
            if fs::metadata(&target).is_ok() {
                let link = staged.path.join(item);
                symlink(&target, &link)
                    .with_context(|| format!("cannot create {}", link.display()))?;
            }
        }
        Ok(staged)
    }

    /// The directory has been renamed into place: nothing to remove.
    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.committed {
            // Only what `build` created: a directory of symlinks, which are unlinked, not
            // followed.
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Opens `path` for reading only when it is a regular file (symlinks followed): a FIFO or a
/// device would block the reader, or read the terminal (R22). Checked again on the open file,
/// in case it was replaced in between.
pub fn open_regular(path: &Path) -> io::Result<fs::File> {
    let not_regular = || io::Error::new(io::ErrorKind::InvalidInput, "not a regular file");
    if !fs::metadata(path)?.is_file() {
        return Err(not_regular());
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(not_regular());
    }
    Ok(file)
}

/// The whole of the regular file `path` ([`open_regular`]).
pub fn read_regular(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    io::Read::read_to_end(&mut open_regular(path)?, &mut bytes)?;
    Ok(bytes)
}

/// A `settings.json` as an object: missing is empty; anything but a JSON object is an error
/// for the launch (R18).
pub fn read_settings(path: &Path) -> Result<Map<String, Value>> {
    let bytes = match read_regular(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(map)) => Ok(map),
        _ => bail!("{} is not a JSON object", path.display()),
    }
}

/// Keys claude's merge replaces instead of combining: compared whole (R18).
const REPLACED: [&str; 2] = ["fallbackModel", "modelPicker"];
/// Keys claude's merge combines one level deep: compared entry by entry, where an entry the
/// other side has is its (R18). The two marketplace keys are one setting.
const SHALLOW: [&str; 3] = [
    "extraKnownMarketplaces",
    "additionalMarketplaces",
    "managedMcpServers",
];

/// The part of `source` that `home` does not define, recursively (R18): keys the home lacks
/// are taken; objects on both sides recurse; for arrays on both sides, the source's elements
/// that equal none of the home's are taken; any other key the home defines is the home's.
/// Where claude's merge does not combine (`fallbackModel`, `modelPicker`) the home's value
/// wins whole; where it merges one level deep (`extraKnownMarketplaces` and its alias
/// `additionalMarketplaces`, `managedMcpServers`) an entry the home has wins whole.
pub fn missing_from(source: &Map<String, Value>, home: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for (key, value) in source {
        if SHALLOW.contains(&key.as_str())
            && let Value::Object(entries) = value
        {
            let others: Vec<&Value> = match key.as_str() {
                "managedMcpServers" => home.get(key).into_iter().collect(),
                _ => ["extraKnownMarketplaces", "additionalMarketplaces"]
                    .iter()
                    .filter_map(|k| home.get(*k))
                    .collect(),
            };
            if others.iter().any(|o| !o.is_object()) {
                continue;
            }
            let kept: Map<String, Value> = entries
                .iter()
                .filter(|(name, _)| !others.iter().any(|o| o.get(name.as_str()).is_some()))
                .map(|(name, entry)| (name.clone(), entry.clone()))
                .collect();
            if !kept.is_empty() || others.is_empty() {
                out.insert(key.clone(), Value::Object(kept));
            }
            continue;
        }
        let Some(own) = home.get(key) else {
            out.insert(key.clone(), value.clone());
            continue;
        };
        if REPLACED.contains(&key.as_str()) {
            continue;
        }
        match (value, own) {
            (Value::Object(value), Value::Object(own)) => {
                let part = missing_from(value, own);
                if !part.is_empty() {
                    out.insert(key.clone(), Value::Object(part));
                }
            }
            (Value::Array(value), Value::Array(own)) => {
                let extra: Vec<Value> =
                    value.iter().filter(|v| !own.contains(v)).cloned().collect();
                if !extra.is_empty() {
                    out.insert(key.clone(), Value::Array(extra));
                }
            }
            _ => {}
        }
    }
    out
}

/// The plugins `settings` enables: keys of `enabledPlugins` set to `true` or to an array (claude
/// treats both as enabled).
pub fn enabled_plugins(settings: &Map<String, Value>) -> Vec<String> {
    settings
        .get("enabledPlugins")
        .and_then(Value::as_object)
        .map(|plugins| {
            plugins
                .iter()
                .filter(|(_, on)| on.as_bool() == Some(true) || on.is_array())
                .map(|(name, _)| name.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// The source's `plugins/installed_plugins.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installs {
    /// No such file: nothing is installed.
    Missing,
    /// Not format version 2 as R18 describes it: no plugin is injected (R11 warns).
    Unrecognized(PathBuf),
    /// `plugins.<name@marketplace>`: the installs of each plugin, in file order.
    Known(BTreeMap<String, Vec<Install>>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    pub scope: Option<String>,
    pub path: Option<PathBuf>,
    /// The directory a `project` or `local` install is for.
    pub project: Option<PathBuf>,
    pub version: Option<String>,
}

impl Installs {
    /// The first `user`-scoped install of `plugin` whose `installPath` exists.
    pub fn install_path(&self, plugin: &str) -> Option<&Path> {
        self.user_install(plugin)?.path.as_deref()
    }

    /// The first `user`-scoped install of `plugin` whose `installPath` is absolute and exists:
    /// the one injected (R18).
    pub fn user_install(&self, plugin: &str) -> Option<&Install> {
        self.installs(plugin)
            .iter()
            .find(|i| i.scope.as_deref() == Some("user") && i.path.as_deref().is_some_and(present))
    }

    /// Whether an install of `plugin` here is one claude loads for a session in `start` (R18,
    /// its `wb`): with `user` or `managed` scope, with `projectPath` equal to `start`, or with
    /// `projectPath` and `start` in git repositories of the same root.
    pub fn installed_for(&self, plugin: &str, start: Option<&Path>) -> bool {
        let Installs::Known(plugins) = self else {
            return false;
        };
        let start_root = start.and_then(git_root);
        plugins.get(plugin).is_some_and(|installs| {
            installs
                .iter()
                .any(|i| loads(i, start, start_root.as_deref()))
        })
    }

    /// The install of `plugin` a session in `start` uses (R22): among those claude loads there
    /// ([`Installs::installed_for`]) whose `installPath` is absolute and exists, the most
    /// specific (`local`, `project`, `user`, `managed`, then any other scope), the first in the
    /// file among equals.
    pub fn effective(&self, plugin: &str, start: Option<&Path>) -> Option<&Install> {
        let start_root = start.and_then(git_root);
        let rank = |i: &Install| match i.scope.as_deref() {
            Some("local") => 0,
            Some("project") => 1,
            Some("user") => 2,
            Some("managed") => 3,
            _ => 4,
        };
        self.installs(plugin)
            .iter()
            .filter(|i| {
                loads(i, start, start_root.as_deref()) && i.path.as_deref().is_some_and(present)
            })
            .min_by_key(|i| rank(i))
    }

    /// The number of install records of `plugin`; 0 when the list is not known.
    pub fn count(&self, plugin: &str) -> usize {
        self.installs(plugin).len()
    }

    fn installs(&self, plugin: &str) -> &[Install] {
        match self {
            Installs::Known(plugins) => plugins.get(plugin).map_or(&[], Vec::as_slice),
            _ => &[],
        }
    }
}

/// An `installPath` that can be used: absolute and existing.
fn present(path: &Path) -> bool {
    path.is_absolute() && path.exists()
}

/// Whether claude loads install `i` for a session in `start`, whose git root is `start_root`
/// (its `wb`, [`Installs::installed_for`]).
fn loads(i: &Install, start: Option<&Path>, start_root: Option<&Path>) -> bool {
    if matches!(i.scope.as_deref(), Some("user" | "managed")) {
        return true;
    }
    let Some(project) = i.project.as_deref() else {
        return false;
    };
    start == Some(project)
        || start_root.is_some_and(|root| git_root(project).as_deref() == Some(root))
}

/// Reads `<home>/plugins/installed_plugins.json` (R18: format version 2).
pub fn installed_plugins(home: &Path) -> Installs {
    #[derive(Deserialize)]
    struct File {
        version: u64,
        plugins: BTreeMap<String, Vec<Value>>,
    }
    let path = home.join("plugins").join("installed_plugins.json");
    let bytes = match read_regular(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Installs::Missing,
        Err(_) => return Installs::Unrecognized(path),
    };
    match serde_json::from_slice::<File>(&bytes) {
        Ok(file) if file.version == 2 => Installs::Known(
            file.plugins
                .into_iter()
                .map(|(name, installs)| {
                    let installs = installs
                        .iter()
                        .map(|install| Install {
                            scope: install
                                .get("scope")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            path: install
                                .get("installPath")
                                .and_then(Value::as_str)
                                .map(PathBuf::from),
                            project: install
                                .get("projectPath")
                                .and_then(Value::as_str)
                                .map(PathBuf::from),
                            version: install
                                .get("version")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        })
                        .collect();
                    (name, installs)
                })
                .collect(),
        ),
        _ => Installs::Unrecognized(path),
    }
}

/// claude's name for a project directory (R18): every UTF-16 code unit that is not an ASCII
/// letter or digit becomes `-` (a character outside the Basic Multilingual Plane becomes `--`);
/// `None` beyond [`MAX_PROJECT_NAME`] code units, where claude truncates and hashes.
pub fn encode_project(root: &str) -> Option<String> {
    let units: Vec<u16> = root.nfc().collect::<String>().encode_utf16().collect();
    if units.len() > MAX_PROJECT_NAME {
        return None;
    }
    Some(
        units
            .iter()
            .map(|&u| match u8::try_from(u) {
                Ok(b) if b.is_ascii_alphanumeric() => char::from(b),
                _ => '-',
            })
            .collect(),
    )
}

/// The project root of `start` as claude 2.1.281 finds it, without running git (R18): its
/// [`git_root`], or with no `.git` anywhere up to `/`, `start` itself. NFC.
pub fn project_root(start: &Path) -> PathBuf {
    git_root(start).unwrap_or_else(|| nfc_path(start.to_path_buf()))
}

/// claude's canonical git root (its `Fr`: `Gt`, then `Ce`/`Ht`): the first directory from
/// `start` up to `/` with a `.git` entry, mapped from a linked worktree to its main repository
/// by [`linked_worktree_root`]; `None` when there is no `.git` up to `/`. NFC.
///
/// Not replicated (R18): claude refuses to follow a `.git` symlink, a `gitdir` or a `commondir`
/// into network locations (on macOS `/net`, `/Network`, `/home/<user>`, `/.vol`, `/.file`, and
/// `//` UNC paths) and keeps walking up; in those rare layouts remuda may choose a different
/// root.
pub fn git_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start;
    loop {
        if is_git_entry(&dir.join(".git")) {
            let root = linked_worktree_root(dir).unwrap_or_else(|| dir.to_path_buf());
            return Some(nfc_path(root));
        }
        dir = dir.parent()?;
    }
}

fn nfc_path(path: PathBuf) -> PathBuf {
    match path.to_str() {
        Some(p) => PathBuf::from(p.nfc().collect::<String>()),
        None => path,
    }
}

/// claude's `ke`: a `.git` that is a directory or a file, also through a symlink whose target
/// reads as UTF-8.
fn is_git_entry(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            fs::read_link(path).is_ok_and(|t| t.to_str().is_some())
                && fs::metadata(path).is_ok_and(|m| m.is_dir() || m.is_file())
        }
        Ok(meta) => meta.is_dir() || meta.is_file(),
        Err(_) => false,
    }
}

/// claude's `Ht`: for `dir` whose `.git` is a file `gitdir: <path>` naming a git dir with a
/// `commondir`, that sits in `<common>/worktrees/`, and whose own `gitdir` file points back (by
/// realpath) at this `.git`: the main repository's root, which is the parent of a common dir
/// named `.git` and the common dir itself otherwise (a bare repository's worktree, unless that
/// directory has a `.git` of its own). `None` on any other outcome: `dir` is the root.
fn linked_worktree_root(dir: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(dir.join(".git")).ok()?;
    let gitdir = text.trim().strip_prefix("gitdir:")?.trim();
    let git_dir = resolve(dir, gitdir);
    let common = resolve(
        &git_dir,
        read_plain_file(&git_dir.join("commondir"))?.trim(),
    );
    if git_dir.parent()? != common.join("worktrees") {
        return None;
    }
    let back = read_plain_file(&git_dir.join("gitdir"))?;
    let back = fs::canonicalize(resolve(&git_dir, back.trim())).ok()?;
    if back != fs::canonicalize(dir).ok()?.join(".git") {
        return None;
    }
    if common.file_name().is_some_and(|n| n == ".git") {
        return common.parent().map(Path::to_path_buf);
    }
    if is_git_entry(&common.join(".git")) {
        return None;
    }
    Some(common)
}

/// The contents of `path` when it is a regular file, not a symlink (claude's `YR`).
fn read_plain_file(path: &Path) -> Option<String> {
    if !fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }
    fs::read_to_string(path).ok()
}

/// Node's `path.resolve(base, p)`: `p` if absolute, else `base/p`, normalized lexically (no
/// `.` or `..` components, no trailing slash).
fn resolve(base: &Path, p: &str) -> PathBuf {
    let joined = if Path::new(p).is_absolute() {
        PathBuf::from(p)
    } else {
        base.join(p)
    };
    let mut out = PathBuf::new();
    for part in joined.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use serde_json::json;

    use super::*;
    use crate::registry::{CLAUDE, CODEX, Home};

    fn map(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(m) => m,
            other => panic!("{other}"),
        }
    }

    /// R18: keys the home lacks come from the source; the home wins on any key it defines;
    /// objects recurse; arrays get the source's elements the home does not have, so an
    /// identical hook entry is not added twice.
    #[test]
    fn settings_are_the_source_minus_what_the_home_defines() {
        let hook = json!({"matcher": "Bash", "hooks": [{"type": "command", "command": "lint"}]});
        let other_hook =
            json!({"matcher": "Edit", "hooks": [{"type": "command", "command": "fmt"}]});
        let source = map(json!({
            "model": "opus",
            "cleanupPeriodDays": 365,
            "env": {"A": "1", "B": "2"},
            "permissions": {"allow": ["Bash(ls)", "Read"], "defaultMode": "plan"},
            "hooks": {"PreToolUse": [hook.clone(), other_hook.clone()]},
            "statusLine": {"type": "command", "command": "x"},
            "enabledPlugins": {"a@m": true},
        }));
        let home = map(json!({
            "model": "sonnet",
            "env": {"B": "home", "C": "3"},
            "permissions": {"allow": ["Read"]},
            "hooks": {"PreToolUse": [hook.clone()]},
            "statusLine": "not an object",
        }));
        assert_eq!(
            Value::Object(missing_from(&source, &home)),
            json!({
                "cleanupPeriodDays": 365,
                "env": {"A": "1"},
                "permissions": {"allow": ["Bash(ls)"], "defaultMode": "plan"},
                "hooks": {"PreToolUse": [other_hook]},
                "enabledPlugins": {"a@m": true},
            })
        );
        // Everything already there: nothing to add, not even empty objects.
        assert_eq!(missing_from(&source, &source), Map::new());
        assert_eq!(missing_from(&Map::new(), &home), Map::new());
        assert_eq!(missing_from(&source, &Map::new()), source);
    }

    #[test]
    fn settings_files_must_be_json_objects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert_eq!(
            read_settings(&path).unwrap(),
            Map::new(),
            "missing is empty"
        );
        fs::write(&path, r#"{"model": "x"}"#).unwrap();
        assert_eq!(read_settings(&path).unwrap(), map(json!({"model": "x"})));
        for bad in ["[]", "1", "{", ""] {
            fs::write(&path, bad).unwrap();
            let e = read_settings(&path).unwrap_err().to_string();
            assert!(e.contains("is not a JSON object"), "{bad:?}: {e}");
        }
    }

    /// R18: every UTF-16 code unit that is not an ASCII letter or digit becomes `-` (a
    /// character outside the BMP is two units, so `--`); roots over 200 units are not encoded
    /// (claude truncates and hashes them, unverified); the root is NFC first.
    #[test]
    fn project_names_are_encoded_like_claude() {
        assert_eq!(
            encode_project("/Users/you/my repo.v2_x").as_deref(),
            Some("-Users-you-my-repo-v2-x")
        );
        assert_eq!(encode_project("/tmp/café").as_deref(), Some("-tmp-caf-"));
        assert_eq!(
            encode_project("/Users/me/📁x").as_deref(),
            Some("-Users-me---x")
        );
        // NFD input: `e` + combining accent is one unit once composed.
        assert_eq!(
            encode_project("/tmp/cafe\u{301}").as_deref(),
            Some("-tmp-caf-")
        );
        let at_limit = format!("/{}", "a".repeat(MAX_PROJECT_NAME - 1));
        assert_eq!(encode_project(&at_limit).map(|e| e.len()), Some(200));
        let over = format!("/{}", "a".repeat(MAX_PROJECT_NAME));
        assert_eq!(encode_project(&over), None);
        // Counted in UTF-16 units: 199 BMP characters fit, 100 emoji (200 units) plus `/` not.
        let wide = format!("/{}", "é".repeat(MAX_PROJECT_NAME - 1));
        assert_eq!(encode_project(&wide).map(|e| e.len()), Some(200));
        assert_eq!(encode_project(&format!("/{}", "📁".repeat(100))), None);
        assert_eq!(
            encode_project(&format!("/{}", "📁".repeat(99))).map(|e| e.len()),
            Some(199)
        );
    }

    fn git() -> PathBuf {
        let path = std::env::var("PATH").ok();
        crate::launch::find_on_path("git", path.as_deref()).expect("git on PATH")
    }

    /// Runs git in `dir` without any user or system configuration (to build fixtures only:
    /// remuda itself runs no git).
    fn run_git(dir: &Path, args: &[&str]) {
        use std::process::{Command, Stdio};
        let status = Command::new(git())
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args([
                "-c",
                "protocol.file.allow=always",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .env("HOME", dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} in {}", dir.display());
    }

    /// R18: the project root as claude finds it, by walking the file system (fixtures made
    /// with real git): a plain repository and its subdirectories (also inside `.git/hooks`), a
    /// linked worktree, a submodule, a separate git dir, a worktree of a bare repository, a
    /// bare repository itself, and no repository at all.
    #[test]
    fn project_roots_follow_claudes_rule() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = root.join("my repo");
        fs::create_dir_all(repo.join("src/deep")).unwrap();
        run_git(&repo, &["init", "-q"]);
        run_git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let tree = root.join("tree");
        run_git(
            &repo,
            &["worktree", "add", "-q", tree.to_str().unwrap(), "-b", "t"],
        );
        fs::create_dir_all(tree.join("x")).unwrap();
        let lib = root.join("lib");
        fs::create_dir_all(&lib).unwrap();
        run_git(&lib, &["init", "-q"]);
        run_git(&lib, &["commit", "-q", "--allow-empty", "-m", "lib"]);
        run_git(
            &repo,
            &[
                "submodule",
                "add",
                "-q",
                lib.to_str().unwrap(),
                "vendor/lib",
            ],
        );
        let sep = root.join("sep");
        run_git(
            &root,
            &[
                "init",
                "-q",
                "--separate-git-dir",
                root.join("sep-git").to_str().unwrap(),
                sep.to_str().unwrap(),
            ],
        );
        let bare = root.join("bare.git");
        run_git(
            &root,
            &[
                "clone",
                "-q",
                "--bare",
                repo.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        let bare_tree = root.join("bare-tree");
        run_git(
            &bare,
            &[
                "worktree",
                "add",
                "-q",
                bare_tree.to_str().unwrap(),
                "-b",
                "w",
            ],
        );
        fs::create_dir_all(bare_tree.join("x")).unwrap();
        let plain = root.join("plain");
        fs::create_dir_all(&plain).unwrap();

        let cases = [
            (repo.clone(), repo.clone()),
            (repo.join("src/deep"), repo.clone()),
            (repo.join(".git/hooks"), repo.clone()),
            (tree.clone(), repo.clone()),
            (tree.join("x"), repo.clone()),
            (repo.join("vendor/lib"), repo.join("vendor/lib")),
            (sep.clone(), sep.clone()),
            (bare_tree.join("x"), bare.clone()),
            (bare.clone(), bare.clone()),
            (plain.clone(), plain.clone()),
        ];
        for (start, want) in cases {
            assert_eq!(project_root(&start), want, "from {}", start.display());
        }
    }

    /// R18: a worktree whose main repository no longer points back at it (moved), and a
    /// gitfile that leads nowhere (the main repository moved), are their own roots.
    #[test]
    fn moved_worktrees_and_broken_gitfiles_are_their_own_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        run_git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let tree = root.join("tree");
        run_git(
            &repo,
            &["worktree", "add", "-q", tree.to_str().unwrap(), "-b", "t"],
        );
        assert_eq!(project_root(&tree), repo);
        // Moved: the back-pointer names the old place.
        let moved = root.join("moved");
        fs::rename(&tree, &moved).unwrap();
        assert_eq!(project_root(&moved), moved);
        // The main repository moved: the gitfile leads nowhere.
        let other = root.join("other");
        fs::rename(&repo, &other).unwrap();
        assert_eq!(project_root(&moved), moved);
        // A gitfile that is not `gitdir:` at all, with trailing whitespace.
        let odd = root.join("odd");
        fs::create_dir_all(odd.join("sub")).unwrap();
        fs::write(odd.join(".git"), "not a gitdir line \n").unwrap();
        assert_eq!(project_root(&odd.join("sub")), odd);
        // Relative paths and surrounding whitespace in the gitfile and commondir.
        let rel = root.join("rel");
        run_git(
            &other,
            &["worktree", "add", "-q", rel.to_str().unwrap(), "-b", "r"],
        );
        fs::write(
            rel.join(".git"),
            "gitdir:   ../other/.git/worktrees/rel  \n\n",
        )
        .unwrap();
        assert_eq!(project_root(&rel), other);
    }

    /// R18: the start directory is the launch directory made real and NFC: a symlinked path,
    /// `..`, a trailing slash and an NFD spelling all name one project.
    #[test]
    fn the_start_directory_is_real_and_nfc() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let proj = root.join("caf\u{e9}");
        fs::create_dir_all(proj.join("sub")).unwrap();
        symlink(&proj, root.join("link")).unwrap();
        for spelling in [
            proj.clone(),
            root.join("link"),
            proj.join("sub/.."),
            PathBuf::from(format!("{}/", proj.display())),
        ] {
            assert_eq!(
                start_dir(&spelling).as_deref(),
                Some(proj.as_path()),
                "{spelling:?}"
            );
        }
        // An NFD spelling resolves (APFS) or not (Linux); either way no NFD root comes out.
        let nfd = root.join("cafe\u{301}");
        if let Some(start) = start_dir(&nfd) {
            assert_eq!(start, proj);
        }
        assert_eq!(start_dir(&root.join("gone")), None);
    }

    /// R18: plugins come from `installed_plugins.json` version 2: the first `user` install
    /// whose path exists; anything else is unrecognized and yields no plugin.
    #[test]
    fn plugin_install_paths() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        assert_eq!(installed_plugins(home), Installs::Missing);
        fs::create_dir_all(home.join("plugins")).unwrap();
        let (gone, there, project) = (
            home.join("cache/gone"),
            home.join("cache/there"),
            home.join("cache/project"),
        );
        fs::create_dir_all(&there).unwrap();
        fs::create_dir_all(&project).unwrap();
        let file = home.join("plugins/installed_plugins.json");
        fs::write(
            &file,
            json!({
                "version": 2,
                "plugins": {
                    "a@m": [
                        {"scope": "project", "installPath": project, "projectPath": "/p"},
                        {"scope": "user", "installPath": gone},
                        {"scope": "user", "installPath": there, "version": "1.0.0"},
                    ],
                    "b@m": [{"scope": "user", "installPath": gone}],
                    "c@m": [{"scope": "local", "installPath": there}],
                    "d@m": [{"scope": "user", "installPath": "relative/path"}],
                },
            })
            .to_string(),
        )
        .unwrap();
        let installs = installed_plugins(home);
        assert_eq!(installs.install_path("a@m"), Some(there.as_path()));
        for none in ["b@m", "c@m", "d@m", "x@m"] {
            assert_eq!(installs.install_path(none), None, "{none}");
        }
        for bad in [
            json!({"version": 1, "plugins": {}}).to_string(),
            json!({"version": 2, "plugins": {"a@m": {"scope": "user"}}}).to_string(),
            json!({"version": 2}).to_string(),
            json!([]).to_string(),
            "{".to_string(),
        ] {
            fs::write(&file, &bad).unwrap();
            assert_eq!(
                installed_plugins(home),
                Installs::Unrecognized(file.clone()),
                "{bad}"
            );
            assert_eq!(installed_plugins(home).install_path("a@m"), None);
        }
        assert_eq!(
            enabled_plugins(&map(json!({
                "enabledPlugins": {"a@m": true, "b@m": false, "c@m": "yes", "d@m": true}
            }))),
            ["a@m", "d@m"]
        );
        assert!(enabled_plugins(&map(json!({"enabledPlugins": []}))).is_empty());
    }

    /// Sorted entry names of a directory.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// The item links under `<shared>/.claude`, as `(name, target)`, in item order.
    fn links(shared: &Path) -> Vec<(&'static str, PathBuf)> {
        let root = shared.join(".claude");
        assert!(
            fs::symlink_metadata(&root).unwrap().file_type().is_dir(),
            "{} is a directory",
            root.display()
        );
        INSTRUCTIONS
            .into_iter()
            .filter_map(|item| {
                let link = root.join(item);
                let meta = fs::symlink_metadata(&link).ok()?;
                assert!(meta.file_type().is_symlink(), "{}", link.display());
                Some((item, fs::read_link(&link).unwrap()))
            })
            .collect()
    }

    /// Every entry of `dir`, recursively, with its type and link target: a snapshot to prove
    /// a tree was not touched.
    fn tree(dir: &Path) -> Vec<(PathBuf, String)> {
        let mut out = Vec::new();
        for name in names(dir) {
            let path = dir.join(&name);
            let meta = fs::symlink_metadata(&path).unwrap();
            let kind = if meta.file_type().is_symlink() {
                format!("-> {}", fs::read_link(&path).unwrap().display())
            } else if meta.is_dir() {
                out.extend(tree(&path));
                "dir".to_string()
            } else {
                format!("file {}", meta.len())
            };
            out.push((path, kind));
        }
        out.sort();
        out
    }

    /// A source home with `CLAUDE.md`, `skills` and `agents` (no `commands`), plus a file
    /// that must never be reachable through the shared directory.
    fn source_home(root: &Path, name: &str) -> PathBuf {
        let home = root.join(name);
        fs::create_dir_all(home.join("skills/review")).unwrap();
        fs::create_dir_all(home.join("agents")).unwrap();
        fs::write(home.join("CLAUDE.md"), "be brief").unwrap();
        fs::write(home.join(".credentials.json"), "secret").unwrap();
        home
    }

    /// R18, R13: `.claude` is a directory of one link per item the source has, pointing at
    /// the source's path as registered; a second call writes nothing at all; the links follow
    /// the source when it changes, and one for an item the source lost is removed. Other
    /// entries in `shared/claude` and in `.claude` are never touched.
    #[test]
    fn the_item_links_are_created_and_kept_current() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("remuda/shared/claude");
        let a = source_home(dir.path(), "a");
        ensure_links(&shared, &a).unwrap();
        assert_eq!(
            links(&shared),
            [
                ("CLAUDE.md", a.join("CLAUDE.md")),
                ("skills", a.join("skills")),
                ("agents", a.join("agents")),
            ]
        );
        assert_eq!(names(&shared), [".claude"]);
        assert_eq!(
            names(&shared.join(".claude")),
            ["CLAUDE.md", "agents", "skills"]
        );

        // Already right: no write (same inodes and mtimes, no temporary entry).
        let stamp = |path: &Path| {
            let m = fs::symlink_metadata(path).unwrap();
            (m.ino(), m.modified().unwrap())
        };
        let root = shared.join(".claude");
        let before: Vec<_> = [
            ".claude",
            ".claude/CLAUDE.md",
            ".claude/skills",
            ".claude/agents",
        ]
        .iter()
        .map(|p| stamp(&shared.join(p)))
        .collect();
        std::thread::sleep(Duration::from_millis(20));
        ensure_links(&shared, &a).unwrap();
        let after: Vec<_> = [
            ".claude",
            ".claude/CLAUDE.md",
            ".claude/skills",
            ".claude/agents",
        ]
        .iter()
        .map(|p| stamp(&shared.join(p)))
        .collect();
        assert_eq!(before, after);
        assert_eq!(names(&shared), [".claude"]);

        // Another source: retargeted; an item the new source lacks loses its link, one it has
        // gains one. Entries remuda does not own stay.
        fs::write(shared.join("notes"), "mine").unwrap();
        fs::write(root.join("settings.json"), "{}").unwrap();
        let b = source_home(dir.path(), "b");
        fs::remove_dir(b.join("agents")).unwrap();
        fs::create_dir(b.join("commands")).unwrap();
        ensure_links(&shared, &b).unwrap();
        assert_eq!(
            links(&shared),
            [
                ("CLAUDE.md", b.join("CLAUDE.md")),
                ("skills", b.join("skills")),
                ("commands", b.join("commands")),
            ]
        );
        assert_eq!(names(&shared), [".claude", "notes"]);
        assert_eq!(
            names(&root),
            ["CLAUDE.md", "commands", "settings.json", "skills"]
        );
        assert_eq!(
            fs::read_to_string(root.join("settings.json")).unwrap(),
            "{}"
        );
        assert_eq!(fs::read_to_string(shared.join("notes")).unwrap(), "mine");

        // A dangling source item counts as absent; the registered path is used, not the
        // canonical one.
        fs::remove_file(b.join("CLAUDE.md")).unwrap();
        symlink("/nowhere", b.join("CLAUDE.md")).unwrap();
        let via = dir.path().join("via");
        symlink(&b, &via).unwrap();
        ensure_links(&shared, &via).unwrap();
        assert_eq!(
            links(&shared),
            [
                ("skills", via.join("skills")),
                ("commands", via.join("commands")),
            ]
        );
    }

    /// R18: a `.claude` that is a symlink to the whole source home (the earlier layout) is
    /// migrated in place to the directory of item links, whatever it pointed at; the source
    /// home and the other entries of `shared/claude` are untouched, and no temporary
    /// directory is left behind.
    #[test]
    fn the_whole_home_link_is_migrated() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("remuda/shared/claude");
        let a = source_home(dir.path(), "a");
        for old in [a.clone(), PathBuf::from("/elsewhere")] {
            let _ = fs::remove_dir_all(shared.join(".claude"));
            let _ = fs::remove_file(shared.join(".claude"));
            fs::create_dir_all(&shared).unwrap();
            symlink(&old, shared.join(".claude")).unwrap();
            fs::write(shared.join("notes"), "mine").unwrap();
            let source_before = tree(&a);
            ensure_links(&shared, &a).unwrap();
            assert_eq!(
                links(&shared),
                [
                    ("CLAUDE.md", a.join("CLAUDE.md")),
                    ("skills", a.join("skills")),
                    ("agents", a.join("agents")),
                ],
                "from {}",
                old.display()
            );
            assert_eq!(names(&shared), [".claude", "notes"]);
            assert_eq!(tree(&a), source_before);
            assert!(!shared.join(".claude/.credentials.json").exists());
        }
    }

    /// R18: an entry that is not a symlink is never replaced: an item inside `.claude` fails
    /// before anything else is changed, and a `.claude` that is neither a directory nor a
    /// symlink fails too; an unwritable `shared/claude` fails without leaving anything.
    #[test]
    fn entries_that_are_not_links_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("remuda/shared/claude");
        let a = source_home(dir.path(), "a");
        let root = shared.join(".claude");

        // A regular file where an item link would go, another item's link wrong: nothing
        // is created or corrected.
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("CLAUDE.md"), "copied").unwrap();
        symlink("/old/skills", root.join("skills")).unwrap();
        let before = tree(&shared);
        let e = ensure_links(&shared, &a).unwrap_err().to_string();
        assert!(e.contains("CLAUDE.md is not a symlink"), "{e}");
        assert_eq!(tree(&shared), before);

        // A directory named like an item.
        fs::remove_file(root.join("CLAUDE.md")).unwrap();
        fs::create_dir(root.join("agents")).unwrap();
        let before = tree(&shared);
        let e = ensure_links(&shared, &a).unwrap_err().to_string();
        assert!(e.contains("agents is not a symlink"), "{e}");
        assert_eq!(tree(&shared), before);

        // `.claude` itself a regular file.
        fs::remove_dir_all(&root).unwrap();
        fs::write(&root, "what").unwrap();
        let e = ensure_links(&shared, &a).unwrap_err().to_string();
        assert!(
            e.contains("neither a directory nor a symlink; remuda does not replace it"),
            "{e}"
        );
        assert_eq!(fs::read_to_string(&root).unwrap(), "what");
        assert_eq!(names(&shared), [".claude"]);

        // `shared/claude` unwritable: the build fails and leaves no temporary directory,
        // whether `.claude` is missing or the old link.
        fs::remove_file(&root).unwrap();
        for old in [None, Some(&a)] {
            if let Some(old) = old {
                symlink(old, &root).unwrap();
            }
            let before = tree(&shared);
            fs::set_permissions(&shared, fs::Permissions::from_mode(0o555)).unwrap();
            let result = ensure_links(&shared, &a);
            fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
            let e = result.unwrap_err().to_string();
            assert!(e.contains("cannot create"), "{e}");
            assert_eq!(tree(&shared), before);
            let _ = fs::remove_file(&root);
        }
    }

    /// R18: launches that migrate the same whole-home link, or create the directory, at the
    /// same time all succeed and leave one correct directory and no temporary one: the loser
    /// of the rename removes its own build and checks the winner's items.
    #[test]
    fn concurrent_launches_agree_on_the_links() {
        use std::sync::{Arc, Barrier};
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("remuda/shared/claude");
        let a = source_home(dir.path(), "a");
        for round in 0..20 {
            let _ = fs::remove_dir_all(shared.join(".claude"));
            fs::create_dir_all(&shared).unwrap();
            if round % 2 == 0 {
                symlink(&a, shared.join(".claude")).unwrap();
            }
            let barrier = Arc::new(Barrier::new(8));
            let threads: Vec<_> = (0..8)
                .map(|_| {
                    let (shared, a, barrier) = (shared.clone(), a.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        ensure_links(&shared, &a)
                    })
                })
                .collect();
            for t in threads {
                t.join()
                    .unwrap()
                    .unwrap_or_else(|e| panic!("round {round}: {e:#}"));
            }
            assert_eq!(
                links(&shared),
                [
                    ("CLAUDE.md", a.join("CLAUDE.md")),
                    ("skills", a.join("skills")),
                    ("agents", a.join("agents")),
                ],
                "round {round}"
            );
            assert_eq!(names(&shared), [".claude"], "round {round}");
            assert_eq!(
                names(&shared.join(".claude")),
                ["CLAUDE.md", "agents", "skills"],
                "round {round}"
            );
        }
    }

    /// R18: the staged directory is removed when it is dropped without being renamed into
    /// place, so no error path after it exists leaves it behind; its links are unlinked, not
    /// followed.
    #[test]
    fn a_staged_directory_is_removed_unless_committed() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        fs::create_dir_all(&shared).unwrap();
        let a = source_home(dir.path(), "a");
        let source_before = tree(&a);
        let staged = Staged::build(&shared, &a).unwrap();
        let path = staged.path.clone();
        assert!(path.starts_with(&shared) && path.is_dir());
        assert_eq!(names(&path), ["CLAUDE.md", "agents", "skills"]);
        drop(staged);
        assert!(!path.exists());
        assert_eq!(names(&shared), Vec::<String>::new());
        assert_eq!(tree(&a), source_before);

        let staged = Staged::build(&shared, &a).unwrap();
        let path = staged.path.clone();
        staged.commit();
        assert!(path.is_dir());
    }

    /// R18: a process killed between removing the old link and the rename leaves no `.claude`
    /// and a stale temporary directory; the next launch creates `.claude` and leaves the stale
    /// directory alone.
    #[test]
    fn a_migration_killed_midway_is_completed_by_the_next_launch() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        fs::create_dir_all(&shared).unwrap();
        let a = source_home(dir.path(), "a");
        let stale = Staged::build(&shared, &a).unwrap();
        let stale_path = stale.path.clone();
        stale.commit();
        let stale_before = tree(&stale_path);
        assert!(!shared.join(".claude").exists());
        ensure_links(&shared, &a).unwrap();
        assert_eq!(
            names(&shared.join(".claude")),
            ["CLAUDE.md", "agents", "skills"]
        );
        assert_eq!(tree(&stale_path), stale_before);
    }

    /// R11, R18: only the items the source has count; an item that resolves to the source's
    /// is shared.
    #[test]
    fn instruction_items_by_realpath() {
        let dir = tempfile::tempdir().unwrap();
        let (source, home) = (dir.path().join("source"), dir.path().join("home"));
        fs::create_dir_all(source.join("skills")).unwrap();
        fs::create_dir_all(source.join("agents")).unwrap();
        fs::write(source.join("CLAUDE.md"), "be brief").unwrap();
        fs::create_dir_all(home.join("commands")).unwrap();
        let got = instructions(&source, &home);
        assert_eq!(got.shared, Vec::<&str>::new());
        assert_eq!(got.missing, ["CLAUDE.md", "skills", "agents"]);
        assert!(got.needs_injection() && !got.partial());

        symlink(source.join("CLAUDE.md"), home.join("CLAUDE.md")).unwrap();
        symlink(source.join("skills"), home.join("skills")).unwrap();
        let got = instructions(&source, &home);
        assert_eq!(got.shared, ["CLAUDE.md", "skills"]);
        assert_eq!(got.missing, ["agents"]);
        assert!(got.needs_injection() && got.partial());

        // `commands` exists only in the home: nothing of the source's to share there.
        symlink(source.join("agents"), home.join("agents")).unwrap();
        let got = instructions(&source, &home);
        assert!(!got.needs_injection() && !got.partial(), "{got:?}");
    }

    fn named(name: &str, home: &Path) -> Account {
        Account {
            provider: CLAUDE,
            name: name.into(),
            home: Home::Path(home.display().to_string()),
        }
    }

    /// R18: members get the injection, in `--option=value` form; the source, an opted-out
    /// account and codex accounts get nothing, nor does anyone when the source home is gone.
    #[test]
    fn only_members_get_the_injection() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (source_home, max, solo) = (root.join("src"), root.join("max"), root.join("solo"));
        for d in [&source_home, &max, &solo] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(source_home.join("CLAUDE.md"), "x").unwrap();
        fs::write(source_home.join("settings.json"), r#"{"model":"opus"}"#).unwrap();
        let source = named("src", &source_home);
        let sharing = Sharing {
            source: Some(source.clone()),
            opted_out: vec!["claude:solo".into()],
        };
        let config = root.join("remuda/config.toml");
        let shared_dir = super::dir(&config);
        let env: Env = [("PATH".to_string(), std::env::var("PATH").unwrap())].into();
        let inject =
            |account: &Account| inject(&sharing, account, &[], Some(&root), &env, &config).unwrap();
        let got = inject(&named("max", &max));
        let want = json!({
            "autoMemoryDirectory": format!(
                "{}/projects/{}/memory",
                source_home.display(),
                encode_project(root.to_str().unwrap()).unwrap()
            ),
            "model": "opus",
        })
        .to_string();
        let file = settings_dir(&config).join(format!("{:x}.json", Sha256::digest(&want)));
        assert_eq!(
            got.args,
            [
                format!("--add-dir={}", shared_dir.display()),
                format!("--settings={}", file.display()),
            ]
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), want);
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
        assert_eq!(got.env, [(CLAUDE_MD_VAR.to_string(), "1".to_string())]);
        assert_eq!(got.notices, Vec::<String>::new());
        assert_eq!(
            links(&shared_dir),
            [("CLAUDE.md", source_home.join("CLAUDE.md"))]
        );
        let logged = got.logged();
        assert_eq!(logged[0].option, "--add-dir");
        assert_eq!(logged[0].bytes, shared_dir.display().to_string().len());
        assert_eq!(logged[1].option, "--settings");
        assert_eq!(logged[1].bytes, file.display().to_string().len());

        assert_eq!(inject(&source), Shared::default());
        assert_eq!(inject(&named("solo", &solo)), Shared::default());
        let codex = Account {
            provider: CODEX,
            ..named("cx", &max)
        };
        assert_eq!(inject(&codex), Shared::default());
        let gone = Sharing {
            source: Some(named("src", &root.join("gone"))),
            ..sharing.clone()
        };
        assert_eq!(
            super::inject(&gone, &named("max", &max), &[], None, &env, &config).unwrap(),
            Shared::default()
        );
    }

    /// R18: authentication settings never travel to a member, whatever the home defines: the
    /// listed keys, and `env` names by prefix, by secret-like part, or exactly; model names are
    /// shared. R11 lists them.
    #[test]
    fn authentication_is_never_injected() {
        let source = map(json!({
            "apiKeyHelper": "/bin/key",
            "proxyAuthHelper": "/bin/proxy",
            "forceLoginOrgUUID": "org",
            "model": "opus",
            "env": {
                "ANTHROPIC_API_KEY": "sk",
                "ANTHROPIC_AWS_API_KEY": "k",
                "ANTHROPIC_MODEL": "opus",
                "ANTHROPIC_SMALL_FAST_MODEL": "haiku",
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "s",
                "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME": "o",
                "ANTHROPIC_DEFAULT_OPUS_MODEL_TOKEN": "t",
                "AWS_BEARER_TOKEN_BEDROCK": "b",
                "AWS_REGION": "us-east-1",
                "CLAUDE_CODE_USE_MANTLE": "1",
                "CLAUDE_CODE_SKIP_BEDROCK_AUTH": "1",
                "_CLAUDE_CODE_X": "1",
                "GOOGLE_CLOUD_PROJECT": "p",
                "CLOUD_ML_REGION": "r",
                "github_token": "gh",
                "MY_SERVICE_BASE_URL": "https://x",
                "CLAUDE_CONFIG_DIR": "/c",
                "DISABLE_TELEMETRY": "1",
                "BASH_DEFAULT_TIMEOUT_MS": "1000",
            },
        }));
        assert_eq!(
            withheld(&source),
            [
                "apiKeyHelper",
                "proxyAuthHelper",
                "forceLoginOrgUUID",
                "env.ANTHROPIC_API_KEY",
                "env.ANTHROPIC_AWS_API_KEY",
                "env.ANTHROPIC_DEFAULT_OPUS_MODEL_TOKEN",
                "env.AWS_BEARER_TOKEN_BEDROCK",
                "env.AWS_REGION",
                "env.CLAUDE_CODE_SKIP_BEDROCK_AUTH",
                "env.CLAUDE_CODE_USE_MANTLE",
                "env.CLAUDE_CONFIG_DIR",
                "env.CLOUD_ML_REGION",
                "env.GOOGLE_CLOUD_PROJECT",
                "env.MY_SERVICE_BASE_URL",
                "env._CLAUDE_CODE_X",
                "env.github_token",
            ]
        );
        let mut injected = missing_from(&source, &Map::new());
        strip_auth(&mut injected);
        assert_eq!(
            Value::Object(injected),
            json!({"model": "opus", "env": {
                "ANTHROPIC_MODEL": "opus",
                "ANTHROPIC_SMALL_FAST_MODEL": "haiku",
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "s",
                "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME": "o",
                "DISABLE_TELEMETRY": "1",
                "BASH_DEFAULT_TIMEOUT_MS": "1000",
            }})
        );
        let mut only_auth = map(json!({"env": {"CLAUDE_CODE_USE_BEDROCK": "1"}}));
        strip_auth(&mut only_auth);
        assert_eq!(only_auth, Map::new(), "an emptied env goes too");
    }

    /// R18: where claude's merge replaces (`fallbackModel`, `modelPicker`), the other side's
    /// value wins whole; where it merges one level deep (`extraKnownMarketplaces` and its alias
    /// `additionalMarketplaces`, `managedMcpServers`), each entry the other side has wins whole.
    #[test]
    fn keys_claude_does_not_merge_deeply_are_compared_whole() {
        let source = map(json!({
            "fallbackModel": ["opus", "sonnet"],
            "modelPicker": {"options": [{"id": "opus"}]},
            "extraKnownMarketplaces": {
                "shared": {"source": {"source": "github", "repo": "a/b"}},
                "mine": {"source": {"source": "github", "repo": "src/m"}},
                "aliased": {"source": {"source": "github", "repo": "src/x"}},
            },
            "managedMcpServers": {
                "db": {"command": "db", "args": ["--src"]},
                "web": {"command": "web"},
            },
        }));
        let home = map(json!({
            "fallbackModel": ["haiku"],
            "modelPicker": {"default": "sonnet"},
            "extraKnownMarketplaces": {"mine": {"source": {"source": "directory"}}},
            "additionalMarketplaces": {"aliased": {"source": {"source": "directory"}}},
            "managedMcpServers": {"db": {"command": "db", "env": {"A": "1"}}},
        }));
        assert_eq!(
            Value::Object(missing_from(&source, &home)),
            json!({
                "extraKnownMarketplaces": {
                    "shared": {"source": {"source": "github", "repo": "a/b"}},
                },
                "managedMcpServers": {"web": {"command": "web"}},
            })
        );
        // Not defined on the other side: everything goes, as for any key.
        assert_eq!(missing_from(&source, &Map::new()), source);
        // Every entry taken: the key goes.
        let all = map(json!({"managedMcpServers": {"db": {}, "web": {}}}));
        assert_eq!(missing_from(&source, &all).get("managedMcpServers"), None);
        // The source's alias against the home's canonical spelling.
        let alias = map(json!({"additionalMarketplaces": {"mine": {}, "new": {}}}));
        let canonical = map(json!({"extraKnownMarketplaces": {"mine": {}}}));
        assert_eq!(
            Value::Object(missing_from(&alias, &canonical)),
            json!({"additionalMarketplaces": {"new": {}}})
        );
    }

    /// R18: settings travel as `<sha256>.json`, mode 0600; the same content reuses its file
    /// and marks it used; writing a new one removes files unused for 30 days, and nothing else.
    #[test]
    fn settings_files_are_content_addressed_and_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("state/settings");
        let day = Duration::from_secs(24 * 60 * 60);
        let now = SystemTime::now();
        let age = |path: &Path, ago: Duration| {
            let file = fs::File::options().write(true).open(path).unwrap();
            file.set_modified(now - ago).unwrap();
        };
        let a = write_settings(&settings, r#"{"a":1}"#, now).unwrap();
        age(&a, 40 * day);
        assert_eq!(
            a.file_name().unwrap().to_str().unwrap(),
            format!("{:x}.json", Sha256::digest(br#"{"a":1}"#))
        );
        assert_eq!(fs::read_to_string(&a).unwrap(), r#"{"a":1}"#);
        assert_eq!(fs::metadata(&a).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(&settings).unwrap().mode() & 0o777, 0o700);
        let b = write_settings(&settings, r#"{"b":1}"#, now).unwrap();
        age(&b, 40 * day);
        // Reused 35 days ago: `b` is marked used, `a` is not.
        assert_eq!(
            write_settings(&settings, r#"{"b":1}"#, now - 35 * day).unwrap(),
            b
        );
        assert_eq!(
            fs::metadata(&b).unwrap().modified().unwrap(),
            now - 35 * day
        );
        let recent = write_settings(&settings, r#"{"c":1}"#, now).unwrap();
        age(&recent, 10 * day);
        fs::write(settings.join("notes.json"), "mine").unwrap();
        age(&settings.join("notes.json"), 90 * day);
        age(&settings.join(SETTINGS_LOCK), 90 * day);
        // A new file now: `a` (40 days) and `b` (35 days) go; `recent`, `notes.json` and the
        // lock file stay.
        let d = write_settings(&settings, r#"{"d":1}"#, now).unwrap();
        let mut left: Vec<PathBuf> = fs::read_dir(&settings)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        left.sort();
        let mut want = vec![
            recent,
            d,
            settings.join("notes.json"),
            settings.join(SETTINGS_LOCK),
        ];
        want.sort();
        assert_eq!(left, want);
    }

    /// R18, R22: settings and plugin lists are read only from regular files, so a FIFO (or a
    /// device) fails at once instead of blocking a launch or the configuration pane.
    #[test]
    fn only_regular_files_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = |path: &Path| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        };
        let settings = dir.path().join("settings.json");
        fifo(&settings);
        let list = dir.path().join("plugins/installed_plugins.json");
        fifo(&list);
        let (tx, rx) = std::sync::mpsc::channel();
        let (s, home) = (settings.clone(), dir.path().to_path_buf());
        std::thread::spawn(move || {
            let _ = tx.send((read_settings(&s).is_err(), installed_plugins(&home)));
        });
        let (settings_err, installs) = rx.recv_timeout(Duration::from_secs(10)).expect("blocked");
        assert!(settings_err);
        assert_eq!(installs, Installs::Unrecognized(list));
        assert!(
            read_settings(&dir.path().join("missing.json"))
                .unwrap()
                .is_empty()
        );
    }

    /// R18: the injected settings also yield to the project's `.claude/settings.json` and
    /// `.claude/settings.local.json` of the start directory, and to the local settings of the
    /// project root when claude reads those; nothing that turns out empty is passed.
    #[test]
    fn project_settings_keep_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (source_home, max) = (root.join("src"), root.join("max"));
        fs::create_dir_all(&source_home).unwrap();
        fs::create_dir_all(&max).unwrap();
        fs::write(
            source_home.join("settings.json"),
            json!({
                "model": "opus",
                "effort": "high",
                "theme": "dark",
                "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say"}]}]},
            })
            .to_string(),
        )
        .unwrap();
        let repo = root.join("repo");
        let start = repo.join("app");
        fs::create_dir_all(start.join(".claude")).unwrap();
        fs::create_dir_all(repo.join(".claude")).unwrap();
        run_git(&repo, &["init", "-q"]);
        fs::write(start.join(".claude/settings.json"), r#"{"model": "haiku"}"#).unwrap();
        fs::write(
            start.join(".claude/settings.local.json"),
            json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say"}]}]}})
                .to_string(),
        )
        .unwrap();
        fs::write(
            repo.join(".claude/settings.local.json"),
            r#"{"effort": "low"}"#,
        )
        .unwrap();
        // An invalid project file counts as absent.
        fs::write(repo.join(".claude/settings.json"), "{").unwrap();
        let sharing = Sharing {
            source: Some(named("src", &source_home)),
            opted_out: vec![],
        };
        let config = root.join("remuda/config.toml");
        let env: Env = [
            ("PATH".to_string(), std::env::var("PATH").unwrap()),
            ("HOME".to_string(), root.join("home").display().to_string()),
        ]
        .into();
        let got = inject(
            &sharing,
            &named("max", &max),
            &[],
            Some(&start),
            &env,
            &config,
        )
        .unwrap();
        let settings = got
            .args
            .iter()
            .find_map(|a| a.strip_prefix("--settings="))
            .unwrap();
        let settings: Value = serde_json::from_str(&fs::read_to_string(settings).unwrap()).unwrap();
        assert_eq!(settings["theme"], json!("dark"));
        for gone in ["model", "effort", "hooks"] {
            assert!(settings.get(gone).is_none(), "{gone}: {settings}");
        }
        assert!(settings.get(MEMORY_KEY).is_some());

        // The project's local settings choose the memory location: none is injected.
        fs::write(
            start.join(".claude/settings.local.json"),
            json!({"autoMemoryDirectory": "/mine"}).to_string(),
        )
        .unwrap();
        let got = inject(
            &sharing,
            &named("max", &max),
            &[],
            Some(&start),
            &env,
            &config,
        )
        .unwrap();
        let settings = got
            .args
            .iter()
            .find_map(|a| a.strip_prefix("--settings="))
            .unwrap();
        let settings: Value = serde_json::from_str(&fs::read_to_string(settings).unwrap()).unwrap();
        assert!(settings.get(MEMORY_KEY).is_none(), "{settings}");
    }

    /// R18: plugins the home turns off, or installed itself, are not injected again.
    #[test]
    fn plugins_the_home_turns_off_or_has_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (source_home, max) = (root.join("src"), root.join("max"));
        let cache = source_home.join("plugins/cache");
        for p in ["a", "b", "c"] {
            fs::create_dir_all(cache.join(p)).unwrap();
        }
        fs::create_dir_all(max.join("plugins")).unwrap();
        fs::write(
            source_home.join("settings.json"),
            json!({"enabledPlugins": {"a@m": true, "b@m": true, "c@m": true}}).to_string(),
        )
        .unwrap();
        let install = |p: &str| json!([{"scope": "user", "installPath": cache.join(p)}]);
        fs::write(
            source_home.join("plugins/installed_plugins.json"),
            json!({"version": 2, "plugins": {"a@m": install("a"), "b@m": install("b"),
                                             "c@m": install("c")}})
            .to_string(),
        )
        .unwrap();
        fs::write(
            max.join("settings.json"),
            json!({"enabledPlugins": {"b@m": false}}).to_string(),
        )
        .unwrap();
        fs::write(
            max.join("plugins/installed_plugins.json"),
            json!({"version": 2, "plugins": {"c@m": [{"scope": "user", "installPath": "/own/c"}]}})
                .to_string(),
        )
        .unwrap();
        let sharing = Sharing {
            source: Some(named("src", &source_home)),
            opted_out: vec![],
        };
        let config = root.join("remuda/config.toml");
        let got = inject(
            &sharing,
            &named("max", &max),
            &["--settings".into(), "/x".into()],
            Some(&root),
            &Env::new(),
            &config,
        )
        .unwrap();
        assert_eq!(
            got.args,
            [format!("--plugin-dir={}", cache.join("a").display())]
        );
    }

    /// R18: a plugin is not injected when the project turns it off, or when the home installed
    /// it for this directory (`project` / `local` scope); a home install for another directory
    /// does not count.
    #[test]
    fn project_disables_and_scoped_home_installs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (source_home, max, work) = (root.join("src"), root.join("max"), root.join("work"));
        let cache = source_home.join("plugins/cache");
        for p in ["a", "b", "c", "d"] {
            fs::create_dir_all(cache.join(p)).unwrap();
        }
        fs::create_dir_all(max.join("plugins")).unwrap();
        fs::create_dir_all(work.join(".claude")).unwrap();
        fs::write(
            source_home.join("settings.json"),
            json!({"enabledPlugins": {"a@m": true, "b@m": true, "c@m": true, "d@m": true}})
                .to_string(),
        )
        .unwrap();
        let install = |p: &str| json!([{"scope": "user", "installPath": cache.join(p)}]);
        fs::write(
            source_home.join("plugins/installed_plugins.json"),
            json!({"version": 2, "plugins": {"a@m": install("a"), "b@m": install("b"),
                                             "c@m": install("c"), "d@m": install("d")}})
            .to_string(),
        )
        .unwrap();
        // The project turns `a` off (local settings) ...
        fs::write(
            work.join(".claude/settings.local.json"),
            json!({"enabledPlugins": {"a@m": false}}).to_string(),
        )
        .unwrap();
        // ... the home installed `b` for this directory, `c` for another one.
        fs::write(
            max.join("plugins/installed_plugins.json"),
            json!({"version": 2, "plugins": {
                "b@m": [{"scope": "project", "installPath": "/own/b", "projectPath": work}],
                "c@m": [{"scope": "local", "installPath": "/own/c", "projectPath": "/elsewhere"}],
            }})
            .to_string(),
        )
        .unwrap();
        let sharing = Sharing {
            source: Some(named("src", &source_home)),
            opted_out: vec![],
        };
        let config = root.join("remuda/config.toml");
        let got = inject(
            &sharing,
            &named("max", &max),
            &["--setting-sources=user".into()],
            Some(&work),
            &Env::new(),
            &config,
        )
        .unwrap();
        assert_eq!(
            got.args,
            [
                format!("--plugin-dir={}", cache.join("c").display()),
                format!("--plugin-dir={}", cache.join("d").display()),
            ]
        );
        // `--setting-sources` (either form), like `--settings`: no settings, a notice.
        assert_eq!(
            got.notices,
            ["--setting-sources given: settings and auto-memory from claude:src are not injected"]
        );
        let got = inject(
            &sharing,
            &named("max", &max),
            &["--setting-sources".into(), "user".into()],
            Some(&work),
            &Env::new(),
            &config,
        )
        .unwrap();
        assert!(!got.args.iter().any(|a| a.starts_with("--settings")));
        assert_eq!(got.notices.len(), 1);
        // Neither is a prefix match: `--settingsx` is not `--settings`.
        let got = inject(
            &sharing,
            &named("max", &max),
            &["--settingsx".into()],
            Some(&work),
            &Env::new(),
            &config,
        )
        .unwrap();
        assert!(
            got.args.iter().any(|a| a.starts_with("--settings=")),
            "{:?}",
            got.args
        );
    }

    /// R18: the settings lock is `.lock`, a regular 0600 file, and excludes a second holder
    /// until the first is dropped; pruning never removes it.
    #[test]
    fn the_settings_lock_is_exclusive_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let held = SettingsLock::exclusive(dir.path()).unwrap();
        assert!(held.0.is_some());
        let lock = dir.path().join(SETTINGS_LOCK);
        let meta = fs::symlink_metadata(&lock).unwrap();
        assert!(meta.is_file());
        assert_eq!(meta.mode() & 0o777, 0o600);
        let other = fs::File::open(&lock).unwrap();
        // SAFETY: the descriptor is open for the call.
        let try_lock = || unsafe { libc::flock(other.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(try_lock(), -1, "held elsewhere");
        drop(held);
        assert_eq!(try_lock(), 0);
    }

    /// R18 (third review): the env rules with claude's host-auth, metadata and Vertex groups,
    /// proxies and telemetry headers; token counts and model names are kept.
    #[test]
    fn the_env_rules_withhold_by_group_and_keep_counts() {
        for name in [
            "GCLOUD_PROJECT",
            "CLAUDE_CODE_HOST_CREDS_FILE",
            "CLAUDE_CODE_SDK_HAS_HOST_AUTH_REFRESH",
            "GCE_METADATA_HOST",
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "ALL_PROXY",
            "OTEL_EXPORTER_OTLP_HEADERS",
            "VERTEX_REGION_CLAUDE_4",
            "IMDS_ENDPOINT",
            "MSI_ENDPOINT",
            "IDENTITY_HEADER",
            "METADATA_SERVER",
            "CLAUDE_CODE_CLIENT_CERT",
            "CLAUDE_CODE_CERT_STORE",
            "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
            "CLAUDE_CODE_FEDERATION_ROLE",
            "MY_AUTH",
            "ANTHROPIC_MAX_TOKENS",
            "GITHUB_TOKEN_MAX_TOKENS",
            "ANTHROPIC_CUSTOM_MODEL_OPTION_API_KEY",
        ] {
            assert!(withheld_env(name), "{name} should be withheld");
        }
        for name in [
            "GIT_AUTHOR_NAME",
            "MAX_THINKING_TOKENS",
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
            "ANTHROPIC_DEFAULT_MODEL",
            "ANTHROPIC_CUSTOM_MODEL_OPTION",
            "ANTHROPIC_CUSTOM_MODEL_OPTION_NAME",
            "ANTHROPIC_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "DISABLE_TELEMETRY",
            "AUTHOR",
            "NO_PROXY",
        ] {
            assert!(!withheld_env(name), "{name} should be shared");
        }
        let settings = map(json!({"otelHeadersHelper": "/h", "env": {"HTTPS_PROXY": "x"}}));
        assert_eq!(
            withheld(&settings),
            ["otelHeadersHelper", "env.HTTPS_PROXY"]
        );
    }

    /// R18: a home install counts as the home's own when claude loads it here: `user` or
    /// `managed` scope, `projectPath` equal to the start directory, or both in git repositories
    /// of the same root (claude's `wb`); outside git, only the same path.
    #[test]
    fn home_installs_count_when_claude_would_load_them() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("a")).unwrap();
        fs::create_dir_all(repo.join("b")).unwrap();
        let other = root.join("other");
        fs::create_dir_all(other.join(".git")).unwrap();
        let (plain1, plain2) = (root.join("p1"), root.join("p2"));
        fs::create_dir_all(&plain1).unwrap();
        fs::create_dir_all(&plain2).unwrap();
        let install = |scope: &str, project: Option<&Path>| Install {
            scope: Some(scope.into()),
            path: Some(PathBuf::from("/i")),
            project: project.map(Path::to_path_buf),
            version: None,
        };
        let with = |i: Install| Installs::Known([("x@m".to_string(), vec![i])].into());
        let start = repo.join("a");
        let cases = [
            (install("user", None), true),
            (install("managed", None), true),
            (install("project", Some(&start)), true),
            (install("local", Some(&repo.join("b"))), true),
            (install("project", Some(&repo)), true),
            (install("project", Some(&other)), false),
            (install("project", None), false),
            (install("weird", Some(&plain1)), false),
        ];
        for (i, want) in cases {
            assert_eq!(
                with(i.clone()).installed_for("x@m", Some(&start)),
                want,
                "{i:?}"
            );
        }
        // Outside git only the same path counts.
        let plain = with(install("local", Some(&plain2)));
        assert!(!plain.installed_for("x@m", Some(&plain1)));
        assert!(plain.installed_for("x@m", Some(&plain2)));
        assert!(!plain.installed_for("y@m", Some(&plain2)));
        assert!(!Installs::Missing.installed_for("x@m", Some(&plain2)));
        assert_eq!(git_root(&plain1), None);
        assert_eq!(project_root(&plain1), plain1);
    }

    /// R18: an `enabledPlugins` array counts as enabled; a home whose own
    /// `installed_plugins.json` cannot be read gets no plugin at all.
    #[test]
    fn plugin_arrays_and_an_unreadable_home_list() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (source_home, max) = (root.join("src"), root.join("max"));
        let cache = source_home.join("plugins/cache");
        fs::create_dir_all(cache.join("a")).unwrap();
        fs::create_dir_all(max.join("plugins")).unwrap();
        fs::write(
            source_home.join("settings.json"),
            json!({"enabledPlugins": {"a@m": ["skill-one"]}}).to_string(),
        )
        .unwrap();
        fs::write(
            source_home.join("plugins/installed_plugins.json"),
            json!({"version": 2, "plugins": {
                "a@m": [{"scope": "user", "installPath": cache.join("a")}]}})
            .to_string(),
        )
        .unwrap();
        let sharing = Sharing {
            source: Some(named("src", &source_home)),
            opted_out: vec![],
        };
        let config = root.join("remuda/config.toml");
        let run = || {
            inject(
                &sharing,
                &named("max", &max),
                &["--settings=/x".into()],
                Some(&root),
                &Env::new(),
                &config,
            )
            .unwrap()
            .args
        };
        assert_eq!(
            run(),
            [format!("--plugin-dir={}", cache.join("a").display())]
        );
        fs::write(
            max.join("plugins/installed_plugins.json"),
            r#"{"version": 9}"#,
        )
        .unwrap();
        assert_eq!(run(), Vec::<String>::new());
    }

    /// R18, R22: the plan decides what a launch gets without writing anything; `inject` then
    /// writes exactly what it decided.
    #[test]
    fn plan_decides_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (source_home, max) = (root.join("src"), root.join("max"));
        for d in [&source_home, &max] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(source_home.join("CLAUDE.md"), "x").unwrap();
        fs::write(source_home.join("settings.json"), r#"{"model":"opus"}"#).unwrap();
        let sharing = Sharing {
            source: Some(named("src", &source_home)),
            opted_out: vec![],
        };
        let config = root.join("remuda/config.toml");
        let env: Env = [("PATH".to_string(), std::env::var("PATH").unwrap())].into();
        let memory = format!(
            "{}/projects/{}/memory",
            source_home.display(),
            encode_project(root.to_str().unwrap()).unwrap()
        );

        let got = plan(&sharing, &named("max", &max), &[], Some(&root), &env).unwrap();
        assert_eq!(
            got.source,
            Some(("claude:src".to_string(), source_home.clone()))
        );
        assert_eq!(got.instructions.missing, ["CLAUDE.md"]);
        assert_eq!(
            Value::Object(got.settings.clone()),
            json!({"autoMemoryDirectory": memory, "model": "opus"})
        );
        assert_eq!(got.memory.as_deref(), Some(memory.as_str()));
        assert_eq!(got.plugins, []);
        assert!(!super::dir(&config).exists());
        assert!(!settings_dir(&config).exists());

        let shared = inject(
            &sharing,
            &named("max", &max),
            &[],
            Some(&root),
            &env,
            &config,
        )
        .unwrap();
        let want = Value::Object(got.settings).to_string();
        let file = settings_dir(&config).join(format!("{:x}.json", Sha256::digest(&want)));
        assert_eq!(
            shared.args,
            [
                format!("--add-dir={}", super::dir(&config).display()),
                format!("--settings={}", file.display()),
            ]
        );
    }

    /// R18, R22: the plan records, for each plugin the source enables and in its order, the
    /// install injected or why none is.
    #[test]
    fn plan_records_why_each_plugin_is_or_is_not_injected() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (source_home, max, work) = (root.join("src"), root.join("max"), root.join("work"));
        let cache = source_home.join("plugins/cache");
        for p in ["a", "b", "c", "d"] {
            fs::create_dir_all(cache.join(p)).unwrap();
        }
        fs::create_dir_all(max.join("plugins")).unwrap();
        fs::create_dir_all(work.join(".claude")).unwrap();
        fs::write(
            source_home.join("settings.json"),
            json!({"enabledPlugins": {"a@m": true, "b@m": true, "c@m": true, "d@m": true}})
                .to_string(),
        )
        .unwrap();
        let install =
            |p: &str| json!([{"scope": "user", "installPath": cache.join(p), "version": "1.0.0"}]);
        let list = source_home.join("plugins/installed_plugins.json");
        fs::write(
            &list,
            json!({"version": 2, "plugins": {"a@m": install("a"), "b@m": install("b"),
                                             "c@m": install("c"), "d@m": install("d")}})
            .to_string(),
        )
        .unwrap();
        fs::write(
            work.join(".claude/settings.local.json"),
            json!({"enabledPlugins": {"a@m": false}}).to_string(),
        )
        .unwrap();
        let own_list = max.join("plugins/installed_plugins.json");
        fs::write(
            &own_list,
            json!({"version": 2, "plugins": {
                "b@m": [{"scope": "project", "installPath": "/own/b", "projectPath": work}],
                "c@m": [{"scope": "local", "installPath": "/own/c", "projectPath": "/elsewhere"}],
            }})
            .to_string(),
        )
        .unwrap();
        let sharing = Sharing {
            source: Some(named("src", &source_home)),
            opted_out: vec![],
        };
        let outcomes = || {
            plan(&sharing, &named("max", &max), &[], Some(&work), &Env::new())
                .unwrap()
                .plugins
                .into_iter()
                .map(|p| (p.name, p.outcome))
                .collect::<Vec<_>>()
        };
        let installed = |p: &str| {
            Ok(PluginDir {
                path: cache.join(p),
                version: Some("1.0.0".into()),
            })
        };
        assert_eq!(
            outcomes(),
            [
                ("a@m".to_string(), Err(Skip::TurnedOff)),
                ("b@m".to_string(), Err(Skip::OwnInstall)),
                ("c@m".to_string(), installed("c")),
                ("d@m".to_string(), installed("d")),
            ]
        );

        // The source's install of `d` is gone.
        fs::remove_dir_all(cache.join("d")).unwrap();
        assert_eq!(outcomes()[3], ("d@m".to_string(), Err(Skip::NotInstalled)));

        // The home's list cannot be read: none is injected.
        fs::write(&own_list, r#"{"version": 9}"#).unwrap();
        assert_eq!(
            outcomes()
                .into_iter()
                .map(|(_, outcome)| outcome)
                .collect::<Vec<_>>(),
            vec![Err(Skip::UnrecognizedList); 4]
        );
    }

    /// R22: of the installs of one plugin, a session uses the most specific that claude loads
    /// there and whose path exists; `user_install` is what a launch injects (R18).
    #[test]
    fn the_effective_install_is_the_most_specific_that_loads() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (start, other, plain) = (root.join("start"), root.join("other"), root.join("plain"));
        let (v1, v2, v3) = (root.join("i/v1"), root.join("i/v2"), root.join("i/v3"));
        for d in [&start, &other, &plain, &v1, &v2, &v3] {
            fs::create_dir_all(d).unwrap();
        }
        let install = |scope: &str, path: &Path, project: Option<&Path>, version: &str| Install {
            scope: Some(scope.into()),
            path: Some(path.to_path_buf()),
            project: project.map(Path::to_path_buf),
            version: Some(version.into()),
        };
        let records = vec![
            install("user", &v1, None, "1"),
            install("project", &v2, Some(&other), "2"),
            install("local", &v3, Some(&start), "3"),
            install("user", &root.join("i/gone"), None, "4"),
        ];
        let installs = Installs::Known([("x@m".to_string(), records.clone())].into());
        assert_eq!(installs.effective("x@m", Some(&start)), Some(&records[2]));
        assert_eq!(installs.effective("x@m", Some(&plain)), Some(&records[0]));
        assert_eq!(installs.effective("x@m", None), Some(&records[0]));
        assert_eq!(installs.effective("y@m", Some(&start)), None);
        assert_eq!(installs.count("x@m"), 4);
        assert_eq!(installs.count("y@m"), 0);
        assert_eq!(Installs::Missing.count("x@m"), 0);
        assert_eq!(installs.user_install("x@m"), Some(&records[0]));
        assert_eq!(installs.install_path("x@m"), Some(v1.as_path()));
    }
}
