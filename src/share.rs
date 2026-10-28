//! Shared configuration (SPEC R18): accounts share one session store and their configuration
//! through symlinks in the member's home, made by `setup` ([`crate::setup`]) or by the user.
//! Nothing is injected at launch and nothing is written anywhere (R13): what a launch adds for
//! a member is one environment variable, [`MEMORY_DIR_VAR`], for a home whose `projects` is
//! the source's, so that claude writes its memory by the source's path and not through the
//! link, and a warning when the member reads the source's authentication settings through its
//! `settings.json` link. The readers here (settings files, rule files, plugin installs, the
//! project name claude gives a directory) serve `setup`, the checks (R11) and the account card
//! (R22).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Map, Value};
use unicode_normalization::UnicodeNormalization;

use crate::Env;
use crate::home_items::{self, Id, Membership, Relations};
use crate::privacy::Marked;
use crate::registry::{Account, Sharing};

/// Directory levels of `rules/` that are read.
const MAX_RULES_DEPTH: usize = 16;
/// Rule files read at most.
const MAX_RULES: usize = 1000;
/// With it set, claude keeps user-scope agent memory under it instead of the home, and
/// auto-memory under its `projects/` (R18).
pub const MEMORY_DIR_VAR: &str = "CLAUDE_CODE_REMOTE_MEMORY_DIR";
/// The settings key that moves auto-memory.
pub const MEMORY_KEY: &str = "autoMemoryDirectory";
/// Roots longer than this many UTF-16 code units are truncated and hashed by claude
/// (**[unverified]** in R22): the card does not know the memory directory for them.
const MAX_PROJECT_NAME: usize = 200;

/// What a launch of a member adds (R18): no option, only environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Shared {
    /// Variables added to the child's environment.
    pub env: Vec<(String, String)>,
    /// Variables removed from the child's environment.
    pub unset: Vec<String>,
    /// One-line messages for the user (stderr for `run`, the status bar in the TUI).
    pub notices: Vec<String>,
}

/// What a session launch of an account gets from the source (R18), decided without writing
/// anything: [`Plan::shared`] turns it into environment changes, and the account card reads
/// it (R22).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// The source (`provider:name`) and its home; `None`: not a member, or the source's home
    /// is missing.
    pub source: Option<(String, PathBuf)>,
    /// How each item of the home relates to the source's ([`home_items::Source::relate`]).
    pub items: Relations,
    /// The source's home, when [`MEMORY_DIR_VAR`] is set to it: the home's `projects` is the
    /// source's (linked), and nothing of the user's decides the memory location instead
    /// (their `--settings` or `--setting-sources`, their own variable, or a settings file that
    /// chooses `autoMemoryDirectory`). With it claude names its memory directory by the
    /// source's path, the same directory without the link, which its own permission check
    /// for that directory needs; the memory of user-scope subagents goes to the source's too.
    pub agent_memory: Option<PathBuf>,
    /// remuda's own environment has [`MEMORY_DIR_VAR`] with the source's home as its value: an
    /// outer remuda launch set it for its own session. It is decided again for this launch and
    /// removed when not set, never inherited.
    pub inherited_memory_dir: bool,
    /// Messages for the user: the authentication warning, a `--settings` that keeps the
    /// variable unset.
    pub notices: Vec<String>,
}

impl Plan {
    /// The environment changes of the plan (R18): the variable where it is set, its removal
    /// where an outer launch's value would be inherited, and the notices.
    pub fn shared(&self) -> Shared {
        let mut shared = Shared {
            notices: self.notices.clone(),
            ..Shared::default()
        };
        match &self.agent_memory {
            Some(dir) => shared
                .env
                .push((MEMORY_DIR_VAR.to_string(), dir.display().to_string())),
            None if self.inherited_memory_dir => shared.unset.push(MEMORY_DIR_VAR.to_string()),
            None => {}
        }
        shared
    }
}

/// What a session launch of `account` with `user_args` in `cwd` gets from the source of
/// `sharing` (R18): [`plan`] as environment changes. Nothing for the source itself, an account
/// with `share = false`, a codex account, or a source whose home is missing.
pub fn at_launch(
    sharing: &Sharing,
    account: &Account,
    user_args: &[String],
    cwd: Option<&Path>,
    env: &Env,
) -> Shared {
    plan(sharing, account, user_args, cwd, env).shared()
}

/// What `account` gets from the source of `sharing` for a session launched with `user_args`
/// in `cwd` (R18, R22), decided by reading only; it cannot fail: a settings file that cannot
/// be read chooses nothing (R11 reports it).
pub fn plan(
    sharing: &Sharing,
    account: &Account,
    user_args: &[String],
    cwd: Option<&Path>,
    env: &Env,
) -> Plan {
    let mut plan = Plan {
        inherited_memory_dir: sharing
            .source
            .as_ref()
            .and_then(|source| source.home_dir(env))
            .zip(env.get(MEMORY_DIR_VAR))
            .is_some_and(|(from, value)| Path::new(value) == from),
        ..Plan::default()
    };
    let Membership::Member {
        source: name,
        home: source,
    } = home_items::membership(sharing, account, env)
    else {
        return plan;
    };
    let Some(home) = account.home_dir(env) else {
        return plan;
    };
    let from = source.home().to_path_buf();
    plan.source = Some((name.clone(), from.clone()));
    plan.items = source.relate(&home);

    // A home that links the source's `settings.json` reads all of it, authentication
    // included: its sessions are told (R11 says it too). A file that cannot be read says
    // nothing here: R11 reports it.
    if plan.items.linked(Id::Settings) {
        let keys = source.withheld();
        if !keys.is_empty() {
            plan.notices.push(format!(
                "warning: {} reads the authentication settings of {name} through its \
                 settings.json link: {}",
                account.qualified(),
                keys.join(", ")
            ));
        }
    }

    // The variable goes with a linked `projects` only: auto-memory is where the source keeps
    // it already, and the variable names that directory by a path without the link.
    if !plan.items.linked(Id::Projects) {
        return plan;
    }
    // A variable the user set is theirs, as it is for claude.
    if env.contains_key(MEMORY_DIR_VAR) && !plan.inherited_memory_dir {
        return plan;
    }
    // claude takes only the last `--settings`, and `--setting-sources` may leave out the
    // user's layer: remuda cannot tell whether they choose the memory location, so the user
    // decides, and is told.
    let user_settings = user_args.iter().find_map(|a| {
        ["--settings", "--setting-sources"]
            .into_iter()
            .find(|o| a == o || a.strip_prefix(o).is_some_and(|v| v.starts_with('=')))
    });
    if let Some(option) = user_settings {
        plan.notices.push(format!(
            "{option} given: {MEMORY_DIR_VAR} is not set for this session, so memory is \
             written through the projects link"
        ));
        return plan;
    }
    // A location a settings file chose is kept: the source's, the home's own (the same file
    // where `settings.json` is linked), the project's. One that cannot be read chooses
    // nothing.
    let chooses = |settings: Result<Map<String, Value>>| {
        settings.is_ok_and(|settings| settings.contains_key(MEMORY_KEY))
    };
    let chosen = source.settings().is_ok_and(|s| s.contains_key(MEMORY_KEY))
        || chooses(read_settings(&Id::Settings.at(&home)))
        || Project::locate(cwd)
            .settings(env)
            .iter()
            .any(|layer| layer.contains_key(MEMORY_KEY));
    if !chosen {
        plan.agent_memory = Some(from);
    }
    plan
}

/// Settings keys that choose credentials, provider or organization: authentication, which
/// stays per account (R18: `setup` does not link a `settings.json` that sets any of it, and a
/// member that reads it through a link is warned).
const AUTH_KEYS: [&str; 8] = [
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
const AUTH_ENV_PREFIXES: [&str; 21] = [
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
const AUTH_ENV_PARTS: [&str; 10] = [
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
const AUTH_ENV_EXACT: [&str; 5] = [
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_SECURESTORAGE_CONFIG_DIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
];

/// Whether the settings `env` variable `name` is an authentication setting (R18), matched
/// case-insensitively: an exact name, a prefix, a secret-like part, or an underscore-separated
/// part `AUTH`. Two kinds of names are exceptions to the one rule that would otherwise catch
/// them: the model-name variables (`ANTHROPIC_MODEL`, `ANTHROPIC_DEFAULT_MODEL`,
/// `ANTHROPIC_SMALL_FAST_MODEL`, `ANTHROPIC_DEFAULT_*_MODEL*`, `ANTHROPIC_CUSTOM_MODEL_OPTION*`,
/// excepted from the `ANTHROPIC_` prefix) and counts ending in `_TOKENS` (excepted from the
/// `TOKEN` part); any other rule still catches them.
fn withheld_env(name: &str) -> bool {
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

/// The authentication settings `settings` has, as `key` or `env.VAR`: what keeps `setup`
/// from linking the file, and what a member that links it is warned it reads (R11, R18).
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

/// Where a session starts, as claude sees it (R22): the launch directory made real and NFC,
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

    /// The project's settings that sit above the user's in claude's order (R22):
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

    /// `<home>/projects/<project>/memory` (R22); `None` without a start directory, or when
    /// the name is too long for remuda to know claude's.
    pub fn memory_dir(&self, home: &Path) -> Option<String> {
        let project = encode_project(self.root.as_ref()?.to_str()?)?;
        Some(
            Id::Projects
                .at(home)
                .join(project)
                .join("memory")
                .display()
                .to_string(),
        )
    }
}

/// The start directory as claude takes its own cwd: absolute, real, NFC (R22).
fn start_dir(cwd: &Path) -> Option<PathBuf> {
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

/// The rule files of the `rules` directory `dir` (R18, R22): its `**/*.md` regular files down to
/// [`MAX_RULES_DEPTH`] levels, symlinks followed and a directory reached twice read once, as
/// paths below `dir`, sorted; the first [`MAX_RULES`] in name order, directory by directory.
/// Empty when `dir` is not a directory.
pub fn rule_files(dir: &Path) -> Vec<PathBuf> {
    fn walk(
        dir: &Path,
        rel: &Path,
        depth: usize,
        seen: &mut BTreeSet<PathBuf>,
        out: &mut Vec<PathBuf>,
    ) {
        let Ok(real) = fs::canonicalize(dir) else {
            return;
        };
        if !seen.insert(real) {
            return;
        }
        let Ok(listing) = fs::read_dir(dir) else {
            return;
        };
        let mut names: Vec<_> = listing.flatten().map(|entry| entry.file_name()).collect();
        names.sort();
        for name in names {
            if out.len() >= MAX_RULES {
                return;
            }
            let path = dir.join(&name);
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                if depth < MAX_RULES_DEPTH {
                    walk(&path, &rel.join(&name), depth + 1, seen, out);
                }
            } else if meta.is_file() && is_rule_name(&name) {
                out.push(rel.join(&name));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, Path::new(""), 1, &mut BTreeSet::new(), &mut out);
    out.sort();
    out
}

fn is_rule_name(name: &std::ffi::OsStr) -> bool {
    name.as_encoded_bytes().ends_with(b".md")
}

/// Whether a rule's frontmatter has a top-level `paths:` key: a rule limited to paths, which
/// the account card marks (R22).
pub fn rule_has_paths(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut lines = text.split('\n').map(|l| l.trim_end_matches('\r'));
    if lines.next().map(str::trim_end) != Some("---") {
        return false;
    }
    let mut paths = false;
    for line in lines {
        if matches!(line.trim_end(), "---" | "...") {
            return paths;
        }
        paths |= line.starts_with("paths:");
    }
    // A frontmatter that does not end is none.
    false
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
fn read_regular(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    io::Read::read_to_end(&mut open_regular(path)?, &mut bytes)?;
    Ok(bytes)
}

/// A `settings.json` as an object: missing is empty; anything but a JSON object is an error,
/// which `setup` and the account card report (R18, R22).
pub fn read_settings(path: &Path) -> Result<Map<String, Value>> {
    let bytes = match read_regular(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(e) => {
            let said = Marked::default().words("cannot read ").path(path.display());
            return Err(said.because(e));
        }
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(map)) => Ok(map),
        // In its pieces: the Configuration pane shows it (R22), and private mode masks the
        // file and keeps what is wrong with it (R21).
        _ => Err(Marked::default()
            .path(path.display())
            .words(" is not a JSON object")
            .into()),
    }
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

/// Whether the home's settings `own` or the project's `layers` above them (in claude's order,
/// each later layer above the earlier) turn `plugin` off: the highest layer that sets it in
/// `enabledPlugins` decides, as claude merges the key.
pub fn turned_off(plugin: &str, own: &Map<String, Value>, layers: &[Map<String, Value>]) -> bool {
    std::iter::once(own)
        .chain(layers)
        .rev()
        .find_map(|settings| settings.get("enabledPlugins")?.get(plugin))
        .is_some_and(|on| on.as_bool() == Some(false))
}

/// Whether `plugin` (`name@marketplace`) is one claude bundles: the `builtin` marketplace is
/// claude's own, its plugins are never installed, and claude loads them everywhere.
pub fn bundled(plugin: &str) -> bool {
    plugin
        .rsplit_once('@')
        .is_some_and(|(_, marketplace)| marketplace == "builtin")
}

/// A home's `plugins/installed_plugins.json` (R22).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installs {
    /// No such file: nothing is installed.
    Missing,
    /// Not format version 2 as R22 describes it: the card reports it.
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
    /// The first `user`-scoped install of `plugin` whose `installPath` is absolute and exists:
    /// the source's install a member's own may resolve to (R22).
    pub fn user_install(&self, plugin: &str) -> Option<&Install> {
        self.installs(plugin)
            .iter()
            .find(|i| i.scope.as_deref() == Some("user") && i.path.as_deref().is_some_and(present))
    }

    /// The install of `plugin` a session in `start` uses (R22): among those claude loads there
    /// ([`loads`]) whose `installPath` is absolute and exists, the most specific (`local`,
    /// `project`, `user`, `managed`, then any other scope), the first in the file among
    /// equals.
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
/// (its `wb`, read from the 2.1.281 bundle, R22): with `user` or `managed` scope, with
/// `projectPath` equal to `start`, or with `projectPath` and `start` in git repositories of
/// the same root.
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

/// Reads `<home>/plugins/installed_plugins.json` (R22: format version 2).
pub fn installed_plugins(home: &Path) -> Installs {
    #[derive(Deserialize)]
    struct File {
        version: u64,
        plugins: BTreeMap<String, Vec<Value>>,
    }
    let path = Id::Plugins.at(home).join("installed_plugins.json");
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

/// claude's name for a project directory (R22): every UTF-16 code unit that is not an ASCII
/// letter or digit becomes `-` (a character outside the Basic Multilingual Plane becomes `--`);
/// `None` beyond [`MAX_PROJECT_NAME`] code units, where claude truncates and hashes.
fn encode_project(root: &str) -> Option<String> {
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

/// The project root of `start` as claude 2.1.281 finds it, without running git (R22): its
/// [`git_root`], or with no `.git` anywhere up to `/`, `start` itself. NFC.
fn project_root(start: &Path) -> PathBuf {
    git_root(start).unwrap_or_else(|| nfc_path(start.to_path_buf()))
}

/// claude's canonical git root (its `Fr`: `Gt`, then `Ce`/`Ht`): the first directory from
/// `start` up to `/` with a `.git` entry, mapped from a linked worktree to its main repository
/// by [`linked_worktree_root`]; `None` when there is no `.git` up to `/`. NFC.
///
/// Not replicated (R22): claude refuses to follow a `.git` symlink, a `gitdir` or a `commondir`
/// into network locations (on macOS `/net`, `/Network`, `/home/<user>`, `/.vol`, `/.file`, and
/// `//` UNC paths) and keeps walking up; in those rare layouts remuda may choose a different
/// root.
fn git_root(start: &Path) -> Option<PathBuf> {
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
    use std::os::unix::fs::symlink;
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::registry::{CLAUDE, CODEX, Home};

    /// Where a home keeps the memory of its user-scope subagents.
    const AGENT_MEMORY: &str = Id::AgentMemory.name();

    fn map(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(m) => m,
            other => panic!("{other}"),
        }
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

    /// R21, R22: the error of a settings file reads as it did (what `setup` prints), and
    /// says which of its pieces is the file: the account card masks that whole in private
    /// mode and keeps what is wrong with it.
    #[test]
    fn a_settings_error_says_which_piece_is_the_file() {
        use crate::privacy::Piece;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a, b (old)").join("settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = path.display().to_string();
        fs::write(&path, "[]").unwrap();
        let e = read_settings(&path).unwrap_err();
        assert_eq!(format!("{e}"), format!("{file} is not a JSON object"));
        assert_eq!(format!("{e:#}"), format!("{file} is not a JSON object"));
        assert_eq!(
            Marked::from_error(&e).pieces().collect::<Vec<_>>(),
            [
                (file.as_str(), Piece::Path),
                (" is not a JSON object", Piece::Words)
            ]
        );
        // One that cannot be read: the reason follows as the cause, as with a context, and
        // is the system's text.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        let e = read_settings(&path).unwrap_err();
        assert_eq!(format!("{e}"), format!("cannot read {file}"));
        let reason = e.chain().nth(1).expect("a cause").to_string();
        assert_eq!(format!("{e:#}"), format!("cannot read {file}: {reason}"));
        assert_eq!(e.chain().count(), 2);
        assert_eq!(
            Marked::from_error(&e).pieces().collect::<Vec<_>>(),
            [
                ("cannot read ", Piece::Words),
                (file.as_str(), Piece::Path),
                (": ", Piece::Words),
                (reason.as_str(), Piece::Text)
            ]
        );
    }

    /// R22: every UTF-16 code unit that is not an ASCII letter or digit becomes `-` (a
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

    /// R22: the project root as claude finds it, by walking the file system (fixtures made
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

    /// R22: a worktree whose main repository no longer points back at it (moved), and a
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

    /// R22: the start directory is the launch directory made real and NFC: a symlinked path,
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

    /// R22: plugins come from `installed_plugins.json` version 2: the first `user` install
    /// whose path exists; anything else is unrecognized and yields no plugin. An
    /// `enabledPlugins` array counts as enabled, like `true`.
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
        let path = |installs: &Installs, plugin: &str| {
            installs.user_install(plugin).and_then(|i| i.path.clone())
        };
        assert_eq!(path(&installs, "a@m"), Some(there.clone()));
        for none in ["b@m", "c@m", "d@m", "x@m"] {
            assert_eq!(path(&installs, none), None, "{none}");
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
            assert_eq!(path(&installed_plugins(home), "a@m"), None);
        }
        assert_eq!(
            enabled_plugins(&map(json!({
                "enabledPlugins": {"a@m": true, "b@m": false, "c@m": "yes", "d@m": true,
                                   "e@m": ["skill-one"]}
            }))),
            ["a@m", "d@m", "e@m"]
        );
        assert!(enabled_plugins(&map(json!({"enabledPlugins": []}))).is_empty());
    }

    /// R22: the highest settings layer that sets a plugin decides whether it is turned off;
    /// an array there, like `true`, enables it.
    #[test]
    fn the_highest_layer_decides_whether_a_plugin_is_off() {
        let layer = |v: Value| map(json!({"enabledPlugins": {"a@m": v}}));
        let own = layer(json!(false));
        assert!(turned_off("a@m", &own, &[]));
        assert!(!turned_off("a@m", &map(json!({})), &[]));
        assert!(!turned_off("a@m", &own, &[layer(json!(true))]));
        assert!(!turned_off("a@m", &own, &[layer(json!([]))]));
        assert!(turned_off(
            "a@m",
            &own,
            &[layer(json!(true)), layer(json!(false))]
        ));
        // A layer that says nothing about it leaves the one below in force.
        assert!(turned_off("a@m", &own, &[map(json!({"model": "opus"}))]));
        assert!(!turned_off("b@m", &own, &[layer(json!(false))]));
    }

    /// R18, R22: `rules/**/*.md` regular files, symlinks followed, sorted; a link back into the
    /// tree ends the walk there; other files are not rules.
    #[test]
    fn rule_files_are_the_markdown_files_below_rules() {
        let dir = tempfile::tempdir().unwrap();
        let rules = dir.path().join("rules");
        assert!(rule_files(&rules).is_empty());
        fs::create_dir_all(rules.join("sub/deep")).unwrap();
        fs::write(rules.join("b.md"), "b").unwrap();
        fs::write(rules.join("notes.txt"), "no").unwrap();
        fs::write(rules.join("sub/a.md"), "a").unwrap();
        fs::write(rules.join("sub/deep/c.md"), "c").unwrap();
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir_all(elsewhere.join("more")).unwrap();
        fs::write(elsewhere.join("x.md"), "x").unwrap();
        fs::write(elsewhere.join("more/y.md"), "y").unwrap();
        symlink(elsewhere.join("x.md"), rules.join("linked.md")).unwrap();
        symlink(elsewhere.join("more"), rules.join("more")).unwrap();
        symlink("/nowhere.md", rules.join("gone.md")).unwrap();
        symlink(&rules, rules.join("sub/loop")).unwrap();
        fs::create_dir(rules.join("dir.md")).unwrap();
        assert_eq!(
            rule_files(&rules),
            [
                "b.md",
                "linked.md",
                "more/y.md",
                "sub/a.md",
                "sub/deep/c.md"
            ]
            .map(PathBuf::from)
        );
        // A file where `rules` should be is no rules.
        assert!(rule_files(&rules.join("b.md")).is_empty());
    }

    /// R22: a rule is limited to paths when its frontmatter has a top-level `paths` key.
    #[test]
    fn rules_limited_to_paths() {
        assert!(rule_has_paths(b"---\npaths:\n  - \"**/*.rs\"\n---\nbody"));
        assert!(rule_has_paths(
            b"\xef\xbb\xbf---\r\ndescription: x\r\npaths: [\"a\"]\r\n---\r\n"
        ));
        assert!(!rule_has_paths(
            b"---\ndescription: x\n---\npaths: not frontmatter\n"
        ));
        assert!(!rule_has_paths(b"---\n  paths: nested\n---\n"));
        assert!(!rule_has_paths(b"paths: x\n"));
        assert!(!rule_has_paths(b"---\npaths: never ends\n"));
        assert!(!rule_has_paths(b""));
    }

    /// R11, R18: a member whose `settings.json` is the source's reads all of it, so a session
    /// of it is told which authentication settings it reads that way, by name; never their
    /// values, and nothing when there are none or the file cannot be read.
    #[test]
    fn a_linked_settings_file_with_authentication_is_named_at_launch() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (source_home, max, team) = (root.join("src"), root.join("max"), root.join("team"));
        for d in [&source_home, &max, &team] {
            fs::create_dir_all(d.join("projects")).unwrap();
        }
        fs::create_dir_all(source_home.join("plugins")).unwrap();
        let settings = source_home.join("settings.json");
        fs::write(&settings, r#"{"model": "opus"}"#).unwrap();
        // `max` links everything the source has.
        fs::remove_dir(max.join("projects")).unwrap();
        for item in ["settings.json", "projects", "plugins"] {
            symlink(source_home.join(item), max.join(item)).unwrap();
        }
        let sharing = Sharing {
            source: Some(named("src", &source_home)),
            opted_out: vec![],
        };
        let env = Env::new();
        let notices =
            |home: &Path, name: &str| plan(&sharing, &named(name, home), &[], None, &env).notices;
        assert_eq!(notices(&max, "max"), Vec::<String>::new());

        fs::write(
            &settings,
            r#"{"model": "opus", "apiKeyHelper": "/k",
                "env": {"ANTHROPIC_API_KEY": "sk-zz", "EDITOR": "vi"}}"#,
        )
        .unwrap();
        assert_eq!(
            notices(&max, "max"),
            [
                "warning: claude:max reads the authentication settings of claude:src through its \
              settings.json link: apiKeyHelper, env.ANTHROPIC_API_KEY"
            ]
        );
        // `team` keeps its own settings: nothing to say.
        assert_eq!(notices(&team, "team"), Vec::<String>::new());
        // A file that cannot be read is R11's to report, and a fully linked home still starts.
        fs::write(&settings, "[]").unwrap();
        assert_eq!(notices(&max, "max"), Vec::<String>::new());
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

    /// R18 (third review): the env rules with claude's host-auth, metadata and Vertex groups,
    /// proxies and telemetry headers count as authentication; token counts and model names do
    /// not.
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

    /// R22: of the installs of one plugin, a session uses the most specific that claude loads
    /// there and whose path exists; `user_install` is the source's install a member's own
    /// install is compared with.
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
    }

    fn named(name: &str, home: &Path) -> Account {
        Account {
            provider: CLAUDE,
            name: name.into(),
            home: Home::Path(home.display().to_string()),
        }
    }

    /// Every entry below `root`, not following symlinks: path, kind and size.
    fn tree(root: &Path) -> Vec<(PathBuf, String)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for e in fs::read_dir(&dir).unwrap().flatten() {
                let meta = fs::symlink_metadata(e.path()).unwrap();
                out.push((e.path(), format!("{:?} {}", meta.file_type(), meta.len())));
                if meta.is_dir() {
                    stack.push(e.path());
                }
            }
        }
        out.sort();
        out
    }

    /// R18: the variable an outer remuda launch set (the source's home) is decided again, not
    /// inherited: removed for the source, an opted-out account, a member whose `projects` is
    /// its own and a member that passes its own `--settings`; set again for a member whose
    /// `projects` is the source's. A value of the user's own is left as it is.
    #[test]
    fn an_inherited_memory_variable_is_decided_again() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (source_home, max, solo, own) = (
            root.join("src"),
            root.join("max"),
            root.join("solo"),
            root.join("own"),
        );
        for d in [&source_home, &solo, &own] {
            fs::create_dir_all(d.join("projects")).unwrap();
        }
        fs::create_dir_all(&max).unwrap();
        symlink(source_home.join("projects"), max.join("projects")).unwrap();
        let source = named("src", &source_home);
        let sharing = Sharing {
            source: Some(source.clone()),
            opted_out: vec!["claude:solo".into()],
        };
        let inherited: Env = [(
            MEMORY_DIR_VAR.to_string(),
            source_home.display().to_string(),
        )]
        .into();
        let run = |account: &Account, args: &[&str], env: &Env| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            at_launch(&sharing, account, &args, Some(&root), env)
        };
        let var = |shared: &Shared| shared.env.iter().any(|(k, _)| k == MEMORY_DIR_VAR);
        let unset = [MEMORY_DIR_VAR.to_string()];

        let got = run(&named("max", &max), &[], &inherited);
        assert!(var(&got) && got.unset.is_empty(), "{got:?}");
        for (account, args) in [
            (&source, &[][..]),
            (&named("solo", &solo), &[][..]),
            (&named("own", &own), &[][..]),
            (&named("max", &max), &["--settings", "/mine.json"][..]),
        ] {
            let got = run(account, args, &inherited);
            assert!(!var(&got), "{got:?}");
            assert_eq!(got.unset, unset, "{}", account.qualified());
        }

        // The user's own value: neither set nor removed, for anyone, and nothing to say.
        let users: Env = [(MEMORY_DIR_VAR.to_string(), "/theirs".to_string())].into();
        for account in [&source, &named("max", &max), &named("solo", &solo)] {
            let got = run(account, &[], &users);
            assert_eq!(got, Shared::default(), "{}", account.qualified());
            let got = run(account, &["--settings", "/mine.json"], &users);
            assert_eq!(got, Shared::default(), "{}", account.qualified());
        }
        // Without a source there is nothing to compare it with.
        let alone = at_launch(
            &Sharing::default(),
            &named("max", &max),
            &[],
            Some(&root),
            &inherited,
        );
        assert_eq!(alone, Shared::default());
    }

    /// R18: a member whose `projects` is the source's gets the variable, whether or not
    /// `agent-memory` is linked too, and whatever the source's settings file is; not when the
    /// user passes `--settings` or `--setting-sources` (said in a notice), sets the variable
    /// themselves, or a settings file (the source's, the home's, the project's) chooses the
    /// auto-memory location; never for a member whose `projects` is its own.
    #[test]
    fn the_memory_variable_follows_a_linked_projects() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (source_home, max, work) = (root.join("src"), root.join("max"), root.join("work"));
        fs::create_dir_all(source_home.join("projects")).unwrap();
        fs::create_dir_all(source_home.join(AGENT_MEMORY)).unwrap();
        fs::create_dir_all(max.join("projects")).unwrap();
        fs::create_dir_all(&work).unwrap();
        let sharing = Sharing {
            source: Some(named("src", &source_home)),
            opted_out: vec![],
        };
        let account = named("max", &max);
        let launch = |args: &[&str], env: &Env| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            let plan = plan(&sharing, &account, &args, Some(&work), env);
            let shared = plan.shared();
            let var = shared
                .env
                .iter()
                .find(|(k, _)| k == MEMORY_DIR_VAR)
                .map(|(_, v)| v.clone());
            assert_eq!(var.is_some(), plan.agent_memory.is_some());
            assert!(var.is_none() || plan.items.linked(Id::Projects));
            (var, shared.notices)
        };
        let var = |args: &[&str], env: &Env| launch(args, env).0;
        let env = Env::new();
        let set = Some(source_home.display().to_string());

        // Its own `projects`: nothing, and nothing to say about the user's `--settings`.
        assert_eq!(launch(&[], &env), (None, vec![]));
        assert_eq!(launch(&["--settings", "/mine.json"], &env), (None, vec![]));

        fs::remove_dir(max.join("projects")).unwrap();
        symlink(source_home.join("projects"), max.join("projects")).unwrap();
        assert_eq!(var(&[], &env), set);
        // The user's own settings: remuda does not know whether they choose the location.
        let notice = |option: &str| {
            vec![format!(
                "{option} given: {MEMORY_DIR_VAR} is not set for this session, so memory is \
                 written through the projects link"
            )]
        };
        assert_eq!(
            launch(&["--settings", "/mine.json"], &env),
            (None, notice("--settings"))
        );
        assert_eq!(
            launch(&["-p", "hi", "--settings={\"a\":1}"], &env),
            (None, notice("--settings"))
        );
        assert_eq!(
            launch(&["--setting-sources", "user"], &env),
            (None, notice("--setting-sources"))
        );
        assert_eq!(
            launch(&["--setting-sources=user,local"], &env),
            (None, notice("--setting-sources"))
        );
        // An option that only begins like one is the user's business, not a `--settings`.
        assert_eq!(var(&["--settingsx"], &env), set);
        // Already set by the user: theirs, silently.
        let users: Env = [(MEMORY_DIR_VAR.to_string(), "/theirs".to_string())].into();
        assert_eq!(launch(&[], &users), (None, vec![]));
        assert_eq!(
            launch(&["--settings", "/mine.json"], &users),
            (None, vec![])
        );
        // A settings file chooses the auto-memory location: the home's own ...
        fs::write(max.join("settings.json"), r#"{"autoMemoryDirectory":"/m"}"#).unwrap();
        assert_eq!(launch(&[], &env), (None, vec![]));
        fs::remove_file(max.join("settings.json")).unwrap();
        assert_eq!(var(&[], &env), set);
        // ... the source's, linked or not ...
        let settings = source_home.join("settings.json");
        fs::write(&settings, r#"{"autoMemoryDirectory":"/m"}"#).unwrap();
        assert_eq!(var(&[], &env), None);
        symlink(&settings, max.join("settings.json")).unwrap();
        assert_eq!(var(&[], &env), None);
        fs::remove_file(max.join("settings.json")).unwrap();
        // ... or the project's.
        fs::write(&settings, r#"{"model":"opus"}"#).unwrap();
        assert_eq!(var(&[], &env), set);
        fs::create_dir_all(work.join(".claude")).unwrap();
        let project = work.join(".claude/settings.local.json");
        fs::write(&project, r#"{"autoMemoryDirectory":"/m"}"#).unwrap();
        assert_eq!(var(&[], &env), None);
        fs::remove_file(&project).unwrap();
        assert_eq!(var(&[], &env), set);

        // `agent-memory` linked too: still set (the same directory either way).
        symlink(source_home.join(AGENT_MEMORY), max.join(AGENT_MEMORY)).unwrap();
        let got = plan(&sharing, &account, &[], Some(&work), &env);
        assert!(got.items.linked(Id::AgentMemory) && got.agent_memory.is_some());
        // The source's settings malformed, or a directory in their place: no location is
        // seen, and no error.
        fs::write(&settings, "{not json").unwrap();
        assert_eq!(var(&[], &env), set);
        fs::remove_file(&settings).unwrap();
        fs::create_dir(&settings).unwrap();
        symlink(&settings, max.join("settings.json")).unwrap();
        assert_eq!(var(&[], &env), set);
        // A non-session gets nothing (R6): decided by the launch, not here.
        assert_eq!(var(&["auth", "status"], &env), set);
    }

    /// R18: only a member gets the variable; the source, an opted-out account, a codex
    /// account and a member of a source whose home is gone get nothing, whatever their
    /// `projects` links. Nothing is written anywhere by deciding it (R13).
    #[test]
    fn only_members_get_the_variable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (source_home, max, solo) = (root.join("src"), root.join("max"), root.join("solo"));
        fs::create_dir_all(source_home.join("projects")).unwrap();
        fs::write(source_home.join("CLAUDE.md"), "x").unwrap();
        fs::write(source_home.join("settings.json"), r#"{"model":"opus"}"#).unwrap();
        for home in [&max, &solo] {
            fs::create_dir_all(home).unwrap();
            symlink(source_home.join("projects"), home.join("projects")).unwrap();
        }
        let source = named("src", &source_home);
        let sharing = Sharing {
            source: Some(source.clone()),
            opted_out: vec!["claude:solo".into()],
        };
        let env: Env = [("PATH".to_string(), std::env::var("PATH").unwrap())].into();
        let before = tree(&root);
        let launch = |account: &Account| at_launch(&sharing, account, &[], Some(&root), &env);
        assert_eq!(
            launch(&named("max", &max)),
            Shared {
                env: vec![(
                    MEMORY_DIR_VAR.to_string(),
                    source_home.display().to_string()
                )],
                ..Shared::default()
            }
        );
        assert_eq!(launch(&source), Shared::default());
        assert_eq!(launch(&named("solo", &solo)), Shared::default());
        let codex = Account {
            provider: CODEX,
            ..named("cx", &max)
        };
        assert_eq!(launch(&codex), Shared::default());
        let gone = Sharing {
            source: Some(named("src", &root.join("gone"))),
            ..sharing.clone()
        };
        assert_eq!(
            at_launch(&gone, &named("max", &max), &[], Some(&root), &env),
            Shared::default()
        );
        assert_eq!(tree(&root), before, "nothing is written");
    }
}
