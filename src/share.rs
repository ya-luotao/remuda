//! Shared configuration, injected at launch (SPEC R18): a claude account gets the source
//! account's instructions, settings, enabled plugins and auto-memory location as launch
//! options. Nothing is written into any home (R13); remuda keeps only the `.claude` symlink in
//! `$REMUDA_HOME/shared/claude/` and the injected settings in `$REMUDA_HOME/state/settings/`.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::Env;
use crate::launch;
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
    let mut shared = Shared::default();
    let Some(source) = sharing.source_for(account) else {
        return Ok(shared);
    };
    let (Some(from), Some(home)) = (source.home_dir(env), account.home_dir(env)) else {
        return Ok(shared);
    };
    if !from.is_dir() {
        return Ok(shared);
    }
    let name = source.qualified();

    if instructions(&from, &home).needs_injection() {
        let shared_dir = dir(config);
        match ensure_link(&shared_dir, &from) {
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

    let user_settings = user_args
        .iter()
        .any(|a| a == "--settings" || a.starts_with("--settings="));
    let own_path = home.join("settings.json");
    let settings_shared = resolves_to(&own_path, &from.join("settings.json"));
    let memory_shared = resolves_to(&home.join("projects"), &from.join("projects"));
    let plugins_part = !resolves_to(&home.join("plugins"), &from.join("plugins"));
    if user_settings && !(settings_shared && memory_shared) {
        shared.notices.push(format!(
            "--settings given: settings and auto-memory from {name} are not injected \
             (claude uses only the last --settings)"
        ));
    }
    let settings_part = !settings_shared && !user_settings;
    let memory_part = !memory_shared && !user_settings;
    if !(settings_part || memory_part || plugins_part) {
        return Ok(shared);
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

    if settings_part || memory_part {
        let project = Project::locate(cwd, env);
        let layers = project.settings(env);
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
        if memory_part && !chosen {
            match project.memory_dir(&from) {
                Ok(Some(memory)) => {
                    injected.insert(MEMORY_KEY.to_string(), Value::String(memory));
                }
                Ok(None) => {}
                Err(why) => shared.notices.push(format!(
                    "auto-memory from {name} is not shared this time: {why}"
                )),
            }
        }
        if !injected.is_empty() {
            let json = serde_json::to_string(&Value::Object(injected))?;
            match write_settings(&settings_dir(config), &json, SystemTime::now()) {
                Ok(path) => shared.args.push(format!("--settings={}", path.display())),
                Err(e) => shared.notices.push(format!(
                    "settings from {name} are not shared this time: {e:#}"
                )),
            }
        }
    }

    if plugins_part {
        let installs = installed_plugins(&from);
        // What the home turned off or installed itself would otherwise load twice.
        let own_installs = match installed_plugins(&home) {
            Installs::Known(plugins) => plugins,
            _ => BTreeMap::new(),
        };
        let off = |plugin: &str| {
            own.get("enabledPlugins")
                .and_then(|p| p.get(plugin))
                .is_some_and(|on| on.as_bool() == Some(false))
        };
        for plugin in enabled_plugins(&source_settings) {
            if off(&plugin) || own_installs.contains_key(&plugin) {
                continue;
            }
            if let Some(path) = installs.install_path(&plugin) {
                shared.args.push(format!("--plugin-dir={}", path.display()));
            }
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
    if fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|f| f.set_modified(now))
            .with_context(|| format!("cannot use {}", path.display()))?;
        return Ok(path);
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("cannot create {}", dir.display()))?;
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
pub const AUTH_KEYS: [&str; 6] = [
    "apiKeyHelper",
    "awsAuthRefresh",
    "awsCredentialExport",
    "gcpAuthRefresh",
    "forceLoginMethod",
    "forceLoginOrgUUID",
];
/// Variables of `env` in settings that do the same: never injected (R18).
pub const AUTH_ENV: [&str; 9] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_SECURESTORAGE_CONFIG_DIR",
];

/// The authentication keys `settings` sets, as `key` or `env.VAR`: withheld from members
/// (R11, R18).
pub fn withheld(settings: &Map<String, Value>) -> Vec<String> {
    let mut keys: Vec<String> = AUTH_KEYS
        .iter()
        .filter(|k| settings.contains_key(**k))
        .map(|k| k.to_string())
        .collect();
    if let Some(Value::Object(vars)) = settings.get("env") {
        keys.extend(
            AUTH_ENV
                .iter()
                .filter(|k| vars.contains_key(**k))
                .map(|k| format!("env.{k}")),
        );
    }
    keys
}

/// Removes the authentication keys from injected settings (an `env` left empty goes too).
fn strip_auth(settings: &mut Map<String, Value>) {
    for key in AUTH_KEYS {
        settings.remove(key);
    }
    if let Some(Value::Object(vars)) = settings.get_mut("env") {
        for key in AUTH_ENV {
            vars.remove(key);
        }
        if vars.is_empty() {
            settings.remove("env");
        }
    }
}

/// Where a session starts, as claude sees it (R18): the launch directory made real and NFC,
/// and the project root one `git` call finds for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// `None` without a launch directory, or when it cannot be resolved.
    pub start: Option<PathBuf>,
    /// The project root, or why it is not known.
    pub root: std::result::Result<PathBuf, String>,
}

impl Project {
    /// Resolves `cwd` and runs `git` (from `PATH`) once for its project root.
    pub fn locate(cwd: Option<&Path>, env: &Env) -> Project {
        let start = cwd.and_then(start_dir);
        let root = match &start {
            None => Err(match cwd {
                None => "the launch directory is unknown".to_string(),
                Some(cwd) => format!("cannot resolve {}", cwd.display()),
            }),
            Some(start) => {
                let path_var = env.get("PATH").map(String::as_str);
                match launch::find_on_path("git", path_var) {
                    Ok(git) => project_root(&git, start),
                    Err(_) => Err("`git` not found on PATH".to_string()),
                }
            }
        };
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
        if let Ok(root) = &self.root
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

    /// `<source>/projects/<project>/memory` (R18); `Ok(None)` when the name is too long for
    /// remuda to know claude's; `Err` when the project root is not known.
    pub fn memory_dir(&self, source: &Path) -> std::result::Result<Option<String>, String> {
        let root = self
            .root
            .as_ref()
            .map_err(|why| format!("cannot find the project root ({why})"))?;
        let Some(project) = root.to_str().and_then(encode_project) else {
            return Ok(None);
        };
        Ok(Some(
            source
                .join("projects")
                .join(project)
                .join("memory")
                .display()
                .to_string(),
        ))
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

/// Makes `<dir>/.claude` a symlink to `source` (R18): created when missing, replaced
/// atomically when it points elsewhere; nothing else in `dir` is touched, and an entry that is
/// not a symlink is never replaced.
pub fn ensure_link(dir: &Path, source: &Path) -> Result<()> {
    let link = dir.join(".claude");
    match fs::symlink_metadata(&link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if fs::read_link(&link).is_ok_and(|target| target == source) {
                return Ok(());
            }
        }
        Ok(_) => bail!(
            "{} is not a symlink; remuda does not replace it",
            link.display()
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("cannot inspect {}", link.display())),
    }
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = dir.join(format!(".claude.{}.tmp", uuid::Uuid::new_v4().simple()));
    symlink(source, &tmp).with_context(|| format!("cannot create {}", tmp.display()))?;
    if let Err(e) = fs::rename(&tmp, &link) {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("cannot create {}", link.display()));
    }
    Ok(())
}

/// A `settings.json` as an object: missing is empty; anything but a JSON object is an error
/// for the launch (R18).
pub fn read_settings(path: &Path) -> Result<Map<String, Value>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(map)) => Ok(map),
        _ => bail!("{} is not a JSON object", path.display()),
    }
}

/// The part of `source` that `home` does not define, recursively (R18): keys the home lacks
/// are taken; objects on both sides recurse; for arrays on both sides, the source's elements
/// that equal none of the home's are taken; any other key the home defines is the home's.
pub fn missing_from(source: &Map<String, Value>, home: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for (key, value) in source {
        let Some(own) = home.get(key) else {
            out.insert(key.clone(), value.clone());
            continue;
        };
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

/// The plugins `settings` enables: keys of `enabledPlugins` set to `true`.
pub fn enabled_plugins(settings: &Map<String, Value>) -> Vec<String> {
    settings
        .get("enabledPlugins")
        .and_then(Value::as_object)
        .map(|plugins| {
            plugins
                .iter()
                .filter(|(_, on)| on.as_bool() == Some(true))
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
}

impl Installs {
    /// The first `user`-scoped install of `plugin` whose `installPath` exists.
    pub fn install_path(&self, plugin: &str) -> Option<&Path> {
        let Installs::Known(plugins) = self else {
            return None;
        };
        plugins
            .get(plugin)?
            .iter()
            .filter(|i| i.scope.as_deref() == Some("user"))
            .filter_map(|i| i.path.as_deref())
            .find(|p| p.is_absolute() && p.exists())
    }
}

/// Reads `<home>/plugins/installed_plugins.json` (R18: format version 2).
pub fn installed_plugins(home: &Path) -> Installs {
    #[derive(Deserialize)]
    struct File {
        version: u64,
        plugins: BTreeMap<String, Vec<Value>>,
    }
    let path = home.join("plugins").join("installed_plugins.json");
    let bytes = match fs::read(&path) {
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

/// The project root of `start` by claude's rule (R18), from one `git rev-parse`: the top level
/// when the git dir is the common dir (a plain repository, a submodule, a separate git dir);
/// for a linked worktree, the parent of a common dir named `.git`, else the common dir itself
/// (a worktree of a bare repository). Outside any repository, and inside a bare one, `start`
/// itself. `Err` when git cannot run or fails for another reason.
pub fn project_root(git: &Path, start: &Path) -> std::result::Result<PathBuf, String> {
    let out = Command::new(git)
        .arg("-C")
        .arg(start)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
            // Last: in a bare repository only this one fails.
            "--show-toplevel",
        ])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {}: {e}", git.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        if stderr.contains("not a git repository")
            || (lines.len() == 2 && stderr.contains("must be run in a work tree"))
        {
            return Ok(start.to_path_buf());
        }
        let why = stderr.lines().next().unwrap_or("").trim();
        return Err(format!("git rev-parse failed: {why}"));
    }
    let [git_dir, common, top] = lines.as_slice() else {
        return Err(format!("unexpected git rev-parse output: {stdout:?}"));
    };
    let (common_path, top_path) = (Path::new(common), Path::new(top));
    if !common_path.is_absolute() || !top_path.is_absolute() {
        return Err(format!("unexpected git rev-parse output: {stdout:?}"));
    }
    let root = if git_dir == common {
        top_path.to_path_buf()
    } else if common_path.file_name().is_some_and(|n| n == ".git") {
        common_path
            .parent()
            .ok_or_else(|| format!("unexpected git common dir {common}"))?
            .to_path_buf()
    } else {
        common_path.to_path_buf()
    };
    let root = root
        .to_str()
        .map(|r| PathBuf::from(r.nfc().collect::<String>()))
        .unwrap_or(root);
    Ok(root)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

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
        launch::find_on_path("git", path.as_deref()).expect("git on PATH")
    }

    /// Runs git in `dir` without any user or system configuration.
    fn run_git(dir: &Path, args: &[&str]) {
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

    /// An executable script, written through a `sh` child (no write descriptor of ours can
    /// leak into another test's child: ETXTBSY on Linux).
    fn script(path: &Path, body: &str) {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
            .arg(path)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("#!/bin/sh\n{body}\n").as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
    }

    /// R18: the project root by claude's rule, with real git: a plain repository (and its
    /// subdirectories), a linked worktree, a submodule, a separate git dir, a worktree of a
    /// bare repository, a cwd inside a bare repository, and no repository at all.
    #[test]
    fn project_roots_follow_claudes_rule() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let git = git();
        let repo = root.join("my repo");
        fs::create_dir_all(repo.join("src/deep")).unwrap();
        run_git(&repo, &["init", "-q"]);
        run_git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let tree = root.join("tree");
        run_git(
            &repo,
            &["worktree", "add", "-q", tree.to_str().unwrap(), "-b", "t"],
        );
        // A submodule: its own git dir lives in the superproject's `.git/modules`.
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
        // A separate git dir.
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
        // A bare repository and a worktree of it.
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
            (tree.clone(), repo.clone()),
            (repo.join("vendor/lib"), repo.join("vendor/lib")),
            (sep.clone(), sep.clone()),
            (bare_tree.join("x"), bare.clone()),
            (bare.clone(), bare.clone()),
            (plain.clone(), plain.clone()),
        ];
        for (start, want) in cases {
            assert_eq!(
                project_root(&git, &start),
                Ok(want),
                "from {}",
                start.display()
            );
        }
    }

    /// R18: when git is missing or fails for another reason, the root is not guessed: no
    /// auto-memory, and a reason to say.
    #[test]
    fn a_failing_git_is_not_a_project_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        script(
            &bin.join("git"),
            "echo 'fatal: detected dubious ownership in repository' >&2; exit 128",
        );
        let e = project_root(&bin.join("git"), &root).unwrap_err();
        assert!(e.contains("dubious ownership"), "{e}");
        let env: Env = [("PATH".to_string(), bin.display().to_string())].into();
        let project = Project::locate(Some(&root), &env);
        assert!(project.memory_dir(Path::new("/src")).is_err());

        let none: Env = [("PATH".to_string(), root.join("empty").display().to_string())].into();
        let project = Project::locate(Some(&root), &none);
        assert_eq!(project.start.as_deref(), Some(root.as_path()));
        let e = project.memory_dir(Path::new("/src")).unwrap_err();
        assert!(e.contains("`git` not found"), "{e}");
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

    /// R18: `.claude` is created, left alone when right, replaced when it points elsewhere,
    /// and never replaced when it is not a symlink; other entries are not touched.
    #[test]
    fn the_shared_link_is_created_and_corrected() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("remuda/shared/claude");
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        ensure_link(&shared, &a).unwrap();
        assert_eq!(fs::read_link(shared.join(".claude")).unwrap(), a);
        ensure_link(&shared, &a).unwrap();
        fs::write(shared.join("other"), "mine").unwrap();
        ensure_link(&shared, &b).unwrap();
        assert_eq!(fs::read_link(shared.join(".claude")).unwrap(), b);
        let mut names: Vec<String> = fs::read_dir(&shared)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, [".claude", "other"]);

        fs::remove_file(shared.join(".claude")).unwrap();
        fs::create_dir(shared.join(".claude")).unwrap();
        let e = ensure_link(&shared, &a).unwrap_err().to_string();
        assert!(e.contains("is not a symlink"), "{e}");
        assert!(shared.join(".claude").is_dir());
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
            fs::read_link(shared_dir.join(".claude")).unwrap(),
            source_home
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

    /// R18: authentication keys never travel to a member, whatever the home defines; R11
    /// lists them.
    #[test]
    fn authentication_is_never_injected() {
        let source = map(json!({
            "apiKeyHelper": "/bin/key",
            "forceLoginOrgUUID": "org",
            "awsAuthRefresh": "x",
            "model": "opus",
            "env": {"ANTHROPIC_API_KEY": "sk", "ANTHROPIC_BASE_URL": "https://x", "FOO": "1"},
        }));
        assert_eq!(
            withheld(&source),
            [
                "apiKeyHelper",
                "awsAuthRefresh",
                "forceLoginOrgUUID",
                "env.ANTHROPIC_API_KEY",
                "env.ANTHROPIC_BASE_URL"
            ]
        );
        let mut injected = missing_from(&source, &Map::new());
        strip_auth(&mut injected);
        assert_eq!(
            Value::Object(injected),
            json!({"model": "opus", "env": {"FOO": "1"}})
        );
        let mut only_auth = map(json!({"env": {"CLAUDE_CODE_USE_BEDROCK": "1"}}));
        strip_auth(&mut only_auth);
        assert_eq!(only_auth, Map::new(), "an emptied env goes too");
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
        // A new file now: `a` (40 days) and `b` (35 days) go; `recent` and `notes.json` stay.
        let d = write_settings(&settings, r#"{"d":1}"#, now).unwrap();
        let mut left: Vec<PathBuf> = fs::read_dir(&settings)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        left.sort();
        let mut want = vec![recent, d, settings.join("notes.json")];
        want.sort();
        assert_eq!(left, want);
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
}
