//! What a claude account's sessions load and where each part comes from (SPEC R22), read on
//! top of the launch's own plan ([`share::plan`]). Read-only: nothing is written, nothing
//! runs, credentials are never opened.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

use crate::Env;
use crate::registry::{Account, Sharing};
use crate::share::{self, Installs, MEMORY_KEY, Plan, Skip};

/// Frontmatter is read from this many bytes at the start of a file.
pub const FRONTMATTER_BYTES: u64 = 8192;
/// Entries listed per directory at most.
pub const MAX_ENTRIES: usize = 500;
/// Directory levels of `commands/` listed.
pub const MAX_DEPTH: usize = 4;
/// Characters of a description kept.
const MAX_DESCRIPTION: usize = 300;

/// What a new session of one claude account in one directory loads (R22).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigView {
    pub role: Role,
    /// The home's instruction items, then those `--add-dir` injects from the source.
    pub instructions: Vec<Item>,
    pub synced: Synced,
    /// `skillOverrides` keys that name no skill listed here.
    pub stale_overrides: Vec<String>,
    /// The home's enabled plugins, then the source's.
    pub plugins: Vec<Plugin>,
    /// Keys of the home's `enabledPlugins` set to `false`.
    pub disabled_plugins: usize,
    /// `Own`, or `AlreadySource` when `settings.json` resolves to the source's.
    pub settings_origin: Origin,
    pub own_settings: Summary,
    /// The injected part of the source's settings, without `autoMemoryDirectory`.
    pub shared_settings: Summary,
    /// Authentication settings of the source (`key` / `env.NAME`), never shared (R18).
    pub withheld: Vec<String>,
    pub memory: Memory,
    pub mcp: Mcp,
    /// What could not be read; first, what would fail a launch.
    pub problems: Vec<String>,
}

/// The account's part in shared configuration (R18).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Role {
    /// No `[share.claude]`.
    #[default]
    Alone,
    /// The account others get their configuration from.
    Source,
    /// `share = false`.
    OptedOut {
        source: String,
    },
    /// The source's home does not exist: nothing is shared.
    SourceMissing {
        source: String,
    },
    Member {
        source: String,
    },
}

impl Role {
    /// The source (`provider:name`), for the roles that have one besides the account.
    pub fn source(&self) -> Option<&str> {
        match self {
            Role::OptedOut { source }
            | Role::SourceMissing { source }
            | Role::Member { source } => Some(source),
            Role::Alone | Role::Source => None,
        }
    }
}

/// Where an item comes from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Origin {
    /// In the account's home.
    #[default]
    Own,
    /// Injected from the source at launch (R18).
    Shared,
    /// The home's item resolves to the source's (R12): nothing is injected for it.
    AlreadySource,
    /// The source's, not injected, and why.
    NotShared(Skip),
    /// Set by the project's settings.
    Project,
}

/// `CLAUDE.md`, `agents`, `skills` or `commands` of a home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub name: &'static str,
    pub origin: Origin,
    /// The symlink's target as recorded, when the item is one.
    pub link: Option<PathBuf>,
    pub content: Content,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    File {
        bytes: u64,
        lines: usize,
    },
    Entries(Vec<Entry>),
    /// A symlink whose target does not exist.
    Broken,
}

/// An agent, skill or command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub description: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub tools: Option<String>,
    pub link: Option<PathBuf>,
    /// A symlink whose target does not exist.
    pub broken: bool,
    /// Turned off by `skillOverrides`.
    pub off: bool,
}

/// The claude.ai skills in `skills/synced/` (R22); bucket names are never kept.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Synced {
    /// No `skills/synced`.
    #[default]
    None,
    /// The account's own bucket, and how many buckets are other logins'.
    Skills { skills: Vec<Entry>, others: usize },
    /// `.claude.json` names no login: which bucket is the account's cannot be told.
    Unmatched { buckets: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plugin {
    /// `name@marketplace`.
    pub name: String,
    pub origin: Origin,
    pub version: Option<String>,
    pub scope: Option<String>,
    /// Install records of the plugin in its `installed_plugins.json`.
    pub installs: usize,
    /// The install a session loads; `None`: not installed here.
    pub path: Option<PathBuf>,
    pub contents: Option<PluginContents>,
}

/// What an installed plugin adds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginContents {
    pub agents: Vec<Entry>,
    pub skills: Vec<Entry>,
    pub commands: Vec<Entry>,
    /// Hook events and the number of hooks of each.
    pub hooks: Vec<(String, usize)>,
    pub mcp_servers: Vec<String>,
}

/// A settings file by key names and counts: no value is kept.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub model: Option<String>,
    /// Rules in `permissions.allow`, `.ask`, `.deny`.
    pub permissions: [usize; 3],
    pub hooks: Vec<(String, usize)>,
    /// Names of the `env` variables.
    pub env: Vec<String>,
    pub status_line: bool,
    /// Every other top-level key.
    pub other: Vec<String>,
}

/// Keys [`Summary`] shows otherwise, or not at all (plugins, memory and skills have their own
/// parts of the pane).
const SUMMARIZED: [&str; 8] = [
    "model",
    "permissions",
    "hooks",
    "env",
    "statusLine",
    "enabledPlugins",
    MEMORY_KEY,
    "skillOverrides",
];

impl Summary {
    pub fn of(settings: &Map<String, Value>) -> Summary {
        let rules = |kind: &str| {
            settings
                .get("permissions")
                .and_then(|p| p.get(kind))
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
        };
        Summary {
            model: settings
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string),
            permissions: [rules("allow"), rules("ask"), rules("deny")],
            hooks: hook_counts(settings.get("hooks")),
            env: settings
                .get("env")
                .and_then(Value::as_object)
                .map(|vars| vars.keys().cloned().collect())
                .unwrap_or_default(),
            status_line: settings.contains_key("statusLine"),
            other: settings
                .keys()
                .filter(|k| !SUMMARIZED.contains(&k.as_str()))
                .cloned()
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Summary::default()
    }
}

/// Where auto-memory goes for a session there (R18).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Memory {
    pub dir: Option<String>,
    pub origin: Origin,
    /// `*.md` files in it; `None` when it does not exist (yet).
    pub files: Option<usize>,
}

/// Names of the account's MCP servers in `.claude.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mcp {
    pub user: Vec<String>,
    pub project: Vec<String>,
}

/// For each hook event whose value is an array: the number of hooks over its groups; by event.
fn hook_counts(hooks: Option<&Value>) -> Vec<(String, usize)> {
    let Some(Value::Object(events)) = hooks else {
        return Vec::new();
    };
    let mut counts: Vec<(String, usize)> = events
        .iter()
        .filter_map(|(event, groups)| {
            let n = groups
                .as_array()?
                .iter()
                .map(|g| g.get("hooks").and_then(Value::as_array).map_or(0, Vec::len))
                .sum();
            Some((event.clone(), n))
        })
        .collect();
    counts.sort();
    counts
}

/// What `account` (a claude account) loads for a new session in `cwd`, as `sharing` decides
/// (R22): the launch's own [`share::plan`], plus what the account's and the source's files
/// list. Everything that cannot be read is a problem in the view, never an error.
pub fn read(account: &Account, sharing: &Sharing, cwd: Option<&Path>, env: &Env) -> ConfigView {
    let mut view = ConfigView {
        role: role(account, sharing, env),
        ..ConfigView::default()
    };
    let Some(home) = account.home_dir(env) else {
        view.problems.push("HOME is not set".to_string());
        return view;
    };
    if !home.is_dir() {
        view.problems
            .push(format!("home {} does not exist", home.display()));
    }
    let project = share::Project::locate(cwd);
    let layers = project.settings(env);
    let plan = match share::plan(sharing, account, &[], cwd, env) {
        Ok(plan) => plan,
        Err(e) => {
            view.problems
                .insert(0, format!("sessions of this account fail to start: {e:#}"));
            Plan::default()
        }
    };
    let from = plan.source.as_ref().map(|(_, from)| from.clone());

    // Own settings.
    let own_path = home.join("settings.json");
    let own = if is_file(&own_path) {
        match share::read_settings(&own_path) {
            Ok(own) => own,
            Err(e) => {
                let problem = format!("{e:#}");
                // A launch fails on it too: said once.
                if !view.problems.iter().any(|p| p.ends_with(&problem)) {
                    view.problems.push(problem);
                }
                Map::new()
            }
        }
    } else {
        Map::new()
    };
    view.settings_origin = if plan.settings_shared {
        Origin::AlreadySource
    } else {
        Origin::Own
    };
    view.own_settings = Summary::of(&own);
    let mut injected = plan.settings.clone();
    injected.remove(MEMORY_KEY);
    view.shared_settings = Summary::of(&injected);

    // Withheld authentication.
    view.withheld = match &view.role {
        Role::Member { .. } => sharing
            .source
            .as_ref()
            .and_then(|s| s.home_dir(env))
            .map(|from| from.join("settings.json"))
            .filter(|p| is_file(p))
            .and_then(|p| share::read_settings(&p).ok())
            .map(|settings| share::withheld(&settings))
            .unwrap_or_default(),
        Role::Source => share::withheld(&own),
        _ => Vec::new(),
    };

    // `.claude.json`: names and the login's ids only.
    let claude_json = match account
        .claude_json(env)
        .and_then(|p| read_regular(&p, None))
    {
        None => ClaudeJson::default(),
        Some(bytes) => serde_json::from_slice::<ClaudeJson>(&bytes).unwrap_or_else(|_| {
            view.problems
                .push(".claude.json is not in a recognized format".to_string());
            ClaudeJson::default()
        }),
    };
    view.mcp.user = claude_json.mcp_servers.0;
    if let Some(root) = &project.root {
        view.mcp.project = claude_json
            .projects
            .unwrap_or_default()
            .remove(&root.display().to_string())
            .map(|p| p.mcp_servers.0)
            .unwrap_or_default();
    }
    // The login's bucket: kept here only, never in the view.
    let bucket = claude_json.oauth.and_then(|ids| {
        let org = ids.org?;
        let account = ids.account?;
        Some(format!("{}_{}", org.as_str()?, account.as_str()?))
    });

    // Instructions: the home's, then the source's that `--add-dir` injects.
    for name in share::INSTRUCTIONS {
        let origin = if plan.instructions.shared.contains(&name) {
            Origin::AlreadySource
        } else {
            Origin::Own
        };
        if let Some(item) = instruction(&home, name, origin) {
            view.instructions.push(item);
        }
    }
    if let Some(from) = &from
        && plan.instructions.needs_injection()
    {
        for name in share::INSTRUCTIONS {
            if from.join(name).exists()
                && let Some(item) = instruction(from, name, Origin::Shared)
            {
                view.instructions.push(item);
            }
        }
    }

    // Synced claude.ai skills.
    let synced = home.join("skills").join("synced");
    if synced.is_dir() {
        let buckets = subdirs(&synced);
        view.synced = match &bucket {
            Some(bucket) => {
                let own_bucket = synced.join(bucket);
                if own_bucket.is_dir() {
                    Synced::Skills {
                        skills: list_skills(&own_bucket),
                        others: buckets.saturating_sub(1),
                    }
                } else {
                    Synced::Skills {
                        skills: Vec::new(),
                        others: buckets,
                    }
                }
            }
            None if buckets > 0 => Synced::Unmatched { buckets },
            None => Synced::None,
        };
    }

    // Plugins: the home's, then the source's.
    let installs = share::installed_plugins(&home);
    if let Installs::Unrecognized(path) = &installs {
        view.problems
            .push(format!("{} is not in a recognized format", path.display()));
    }
    // The source's, when the launch injects from it (a member).
    let source_installs = match &from {
        Some(from) => share::installed_plugins(from),
        None => Installs::Missing,
    };
    for name in share::enabled_plugins(&own) {
        let install = installs.effective(&name, project.start.as_deref());
        let path = install.and_then(|i| i.path.clone());
        let already = plan.plugins_shared
            || path.as_deref().is_some_and(|own_path| {
                source_installs
                    .user_install(&name)
                    .and_then(|i| i.path.as_deref())
                    .is_some_and(|src| share::resolves_to(own_path, src))
            });
        view.plugins.push(Plugin {
            origin: if already {
                Origin::AlreadySource
            } else {
                Origin::Own
            },
            version: install.and_then(|i| i.version.clone()),
            scope: install.and_then(|i| i.scope.clone()),
            installs: installs.count(&name),
            contents: path.as_deref().map(plugin_contents),
            path,
            name,
        });
    }
    view.disabled_plugins = own
        .get("enabledPlugins")
        .and_then(Value::as_object)
        .map_or(0, |p| {
            p.values().filter(|on| **on == Value::Bool(false)).count()
        });
    for p in &plan.plugins {
        let plugin = match &p.outcome {
            Ok(dir) => Plugin {
                name: p.name.clone(),
                origin: Origin::Shared,
                version: dir.version.clone(),
                scope: Some("user".to_string()),
                installs: source_installs.count(&p.name),
                path: Some(dir.path.clone()),
                contents: Some(plugin_contents(&dir.path)),
            },
            Err(Skip::OwnInstall) if view.plugins.iter().any(|own| own.name == p.name) => {
                continue;
            }
            Err(skip) => Plugin {
                name: p.name.clone(),
                origin: Origin::NotShared(*skip),
                version: None,
                scope: None,
                installs: 0,
                path: None,
                contents: None,
            },
        };
        view.plugins.push(plugin);
    }

    // Skill overrides: the home's, the project's over it, then the injected ones it lacks.
    let mut overrides: BTreeMap<String, Value> = BTreeMap::new();
    let mut overlay = |settings: &Map<String, Value>, replace: bool| {
        if let Some(Value::Object(o)) = settings.get("skillOverrides") {
            for (k, v) in o {
                if replace || !overrides.contains_key(k) {
                    overrides.insert(k.clone(), v.clone());
                }
            }
        }
    };
    overlay(&own, true);
    for layer in &layers {
        overlay(layer, true);
    }
    overlay(&plan.settings, false);
    mark_overrides(&mut view, &overrides);

    // Auto-memory.
    let memory_of = |settings: &Map<String, Value>| {
        settings
            .get(MEMORY_KEY)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let (dir, origin) = if let Some(dir) = memory_of(&plan.settings) {
        (Some(dir), Origin::Shared)
    } else if let Some(dir) = layers.iter().rev().find_map(memory_of) {
        (Some(dir), Origin::Project)
    } else if let Some(dir) = memory_of(&own) {
        let origin = if plan.settings_shared {
            Origin::AlreadySource
        } else {
            Origin::Own
        };
        (Some(dir), origin)
    } else {
        let origin = if plan.memory_shared {
            Origin::AlreadySource
        } else {
            Origin::Own
        };
        (project.memory_dir(&home), origin)
    };
    view.memory = Memory {
        files: dir.as_deref().map(Path::new).and_then(markdown_files),
        dir,
        origin,
    };
    view
}

fn role(account: &Account, sharing: &Sharing, env: &Env) -> Role {
    let Some(source) = &sharing.source else {
        return Role::Alone;
    };
    let qualified = account.qualified();
    let name = source.qualified();
    if name == qualified {
        Role::Source
    } else if sharing.opted_out.contains(&qualified) {
        Role::OptedOut { source: name }
    } else if source.home_dir(env).is_some_and(|h| h.is_dir()) {
        Role::Member { source: name }
    } else {
        Role::SourceMissing { source: name }
    }
}

/// Marks the skills turned off by `overrides` (as `<skill>`, or `<plugin>:<skill>` for a
/// plugin's), and lists the overrides naming no skill.
fn mark_overrides(view: &mut ConfigView, overrides: &BTreeMap<String, Value>) {
    let off = |key: &str| overrides.get(key).and_then(Value::as_str) == Some("off");
    let mut known: Vec<String> = Vec::new();
    let mut mark = |entry: &mut Entry, names: Vec<String>| {
        entry.off = names.iter().any(|n| off(n));
        known.extend(names);
    };
    for item in &mut view.instructions {
        if item.name != "skills" {
            continue;
        }
        if let Content::Entries(entries) = &mut item.content {
            for e in entries {
                let names = vec![e.name.clone()];
                mark(e, names);
            }
        }
    }
    if let Synced::Skills { skills, .. } = &mut view.synced {
        for e in skills {
            let names = vec![e.name.clone()];
            mark(e, names);
        }
    }
    for plugin in &mut view.plugins {
        let short = plugin
            .name
            .split('@')
            .next()
            .unwrap_or_default()
            .to_string();
        if let Some(contents) = &mut plugin.contents {
            for e in &mut contents.skills {
                let names = vec![e.name.clone(), format!("{short}:{}", e.name)];
                mark(e, names);
            }
        }
    }
    view.stale_overrides = overrides
        .keys()
        .filter(|k| !known.contains(k))
        .cloned()
        .collect();
}

/// One instruction item of `home`, or `None` when there is none (or it is neither a file nor
/// a directory as expected).
fn instruction(home: &Path, name: &'static str, origin: Origin) -> Option<Item> {
    let path = home.join(name);
    let link = link_of(&path);
    let meta = match fs::metadata(&path) {
        Ok(meta) => meta,
        Err(_) if link.is_some() => {
            return Some(Item {
                name,
                origin,
                link,
                content: Content::Broken,
            });
        }
        Err(_) => return None,
    };
    let content = match name {
        "CLAUDE.md" => {
            let (bytes, lines) = file_stats(&path)?;
            Content::File { bytes, lines }
        }
        _ if !meta.is_dir() => return None,
        "agents" => Content::Entries(list_agents(&path)),
        "skills" => Content::Entries(list_skills(&path)),
        _ => Content::Entries(list_commands(&path)),
    };
    Some(Item {
        name,
        origin,
        link,
        content,
    })
}

/// The target of `path` as recorded, when it is a symlink.
fn link_of(path: &Path) -> Option<PathBuf> {
    fs::symlink_metadata(path)
        .ok()
        .filter(|m| m.file_type().is_symlink())
        .and_then(|_| fs::read_link(path).ok())
}

/// Whether `path` is a regular file (following symlinks).
fn is_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file())
}

/// Opens `path` only when it is a regular file: never a FIFO or a device, which could block.
fn open_regular(path: &Path) -> Option<fs::File> {
    share::open_regular(path).ok()
}

/// The contents of the regular file `path`, the first `limit` bytes of it when given.
fn read_regular(path: &Path, limit: Option<u64>) -> Option<Vec<u8>> {
    let file = open_regular(path)?;
    let mut bytes = Vec::new();
    match limit {
        Some(limit) => file.take(limit).read_to_end(&mut bytes).ok()?,
        None => (&file).read_to_end(&mut bytes).ok()?,
    };
    Some(bytes)
}

/// Size and number of lines of a regular file, read in pieces.
fn file_stats(path: &Path) -> Option<(u64, usize)> {
    let mut file = open_regular(path)?;
    let mut buf = [0u8; 64 * 1024];
    let (mut bytes, mut lines, mut last) = (0u64, 0usize, b'\n');
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        lines += buf[..n].iter().filter(|b| **b == b'\n').count();
        last = buf[n - 1];
    }
    if last != b'\n' {
        lines += 1;
    }
    Some((bytes, lines))
}

/// Subdirectories of `dir` (following symlinks), dot-names excepted.
fn subdirs(dir: &Path) -> usize {
    fs::read_dir(dir).map_or(0, |listing| {
        listing
            .flatten()
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .filter(|e| e.path().is_dir())
            .count()
    })
}

/// The names in `dir` (not dot-names), sorted, at most [`MAX_ENTRIES`].
fn names(dir: &Path) -> Vec<String> {
    let Ok(listing) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = listing
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    names.truncate(MAX_ENTRIES);
    names
}

/// A symlink whose target does not exist.
fn broken(path: &Path) -> bool {
    link_of(path).is_some() && fs::metadata(path).is_err()
}

/// `agents/*.md`, top level only.
fn list_agents(dir: &Path) -> Vec<Entry> {
    let mut entries: Vec<Entry> = names(dir)
        .into_iter()
        .filter_map(|file| {
            let stem = file.strip_suffix(".md")?.to_string();
            markdown_entry(&dir.join(&file), stem)
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// An entry for the markdown file `path` (named `fallback` without a frontmatter `name`),
/// broken when it is a dangling symlink; `None` when it is not a regular file.
fn markdown_entry(path: &Path, fallback: String) -> Option<Entry> {
    let link = link_of(path);
    if broken(path) {
        return Some(Entry {
            name: fallback,
            link,
            broken: true,
            ..Entry::default()
        });
    }
    let bytes = read_regular(path, Some(FRONTMATTER_BYTES))?;
    let front = frontmatter(&bytes);
    Some(Entry {
        name: front.name.unwrap_or(fallback),
        description: front.description,
        model: front.model,
        effort: front.effort,
        tools: front.tools,
        link,
        broken: false,
        off: false,
    })
}

/// `<dir>/<skill>/SKILL.md`; `synced` is never a skill.
fn list_skills(dir: &Path) -> Vec<Entry> {
    let mut entries: Vec<Entry> = names(dir)
        .into_iter()
        .filter(|n| n != "synced")
        .filter_map(|child| {
            let path = dir.join(&child);
            if broken(&path) {
                return Some(Entry {
                    name: child,
                    link: link_of(&path),
                    broken: true,
                    ..Entry::default()
                });
            }
            if !path.is_dir() {
                return None;
            }
            let mut entry = markdown_entry(&path.join("SKILL.md"), child)?;
            if entry.broken {
                return None;
            }
            entry.link = link_of(&path);
            Some(entry)
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// `commands/**/*.md` to [`MAX_DEPTH`] levels, a subdirectory shown as `dir:name`.
fn list_commands(dir: &Path) -> Vec<Entry> {
    fn walk(dir: &Path, prefix: &str, depth: usize, out: &mut Vec<Entry>) {
        for name in names(dir) {
            if out.len() >= MAX_ENTRIES {
                return;
            }
            let path = dir.join(&name);
            if let Some(stem) = name.strip_suffix(".md")
                && (broken(&path) || is_file(&path))
                && let Some(entry) = markdown_entry(&path, format!("{prefix}{stem}"))
            {
                out.push(Entry {
                    // Nested commands are named by their path, whatever the frontmatter says.
                    name: format!("{prefix}{stem}"),
                    ..entry
                });
            } else if depth < MAX_DEPTH && path.is_dir() {
                walk(&path, &format!("{prefix}{name}:"), depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, "", 1, &mut out);
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// `*.md` regular files at the top of `dir`, when it is an absolute directory.
fn markdown_files(dir: &Path) -> Option<usize> {
    if !dir.is_absolute() || !dir.is_dir() {
        return None;
    }
    let listing = fs::read_dir(dir).ok()?;
    Some(
        listing
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".md"))
            .filter(|e| is_file(&e.path()))
            .count(),
    )
}

/// `rel` under the plugin directory `root`, when it stays there: a manifest's path is relative
/// and may not climb out (`..`, an absolute path, or a symlink leading elsewhere), so a plugin
/// cannot make remuda open files outside it, such as credentials (R22).
fn within(root: &Path, rel: &str) -> Option<PathBuf> {
    use std::path::Component;
    let rel = Path::new(rel);
    if !rel
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return None;
    }
    let path = root.join(rel);
    let real = fs::canonicalize(&path).ok()?;
    real.starts_with(fs::canonicalize(root).ok()?)
        .then_some(path)
}

/// What an installed plugin at `root` adds; best-effort.
fn plugin_contents(root: &Path) -> PluginContents {
    let manifest: PluginJson = read_regular(&root.join(".claude-plugin/plugin.json"), None)
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let json_at =
        |path: &Path| -> Option<Value> { serde_json::from_slice(&read_regular(path, None)?).ok() };
    let hooks = match json_at(&root.join("hooks/hooks.json")) {
        Some(file) => hook_counts(file.get("hooks")),
        None => match &manifest.hooks {
            Some(Value::String(path)) => within(root, path)
                .and_then(|path| json_at(&path))
                .map(|file| hook_counts(file.get("hooks")))
                .unwrap_or_default(),
            hooks @ Some(Value::Object(_)) => hook_counts(hooks.as_ref()),
            _ => Vec::new(),
        },
    };
    let mcp_file = |path: &Path| -> Vec<String> {
        read_regular(path, None)
            .and_then(|bytes| serde_json::from_slice::<McpFile>(&bytes).ok())
            .map(McpFile::names)
            .unwrap_or_default()
    };
    let mut mcp_servers = mcp_file(&root.join(".mcp.json"));
    match manifest.mcp_servers {
        Some(KeysOrPath::Keys(keys)) => mcp_servers.extend(keys),
        Some(KeysOrPath::Path(path)) => {
            if let Some(path) = within(root, &path) {
                mcp_servers.extend(mcp_file(&path));
            }
        }
        None => {}
    }
    mcp_servers.sort();
    mcp_servers.dedup();
    PluginContents {
        agents: list_agents(&root.join("agents")),
        skills: list_skills(&root.join("skills")),
        commands: list_commands(&root.join("commands")),
        hooks,
        mcp_servers,
    }
}

/// The frontmatter fields the pane shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    tools: Option<String>,
}

/// The YAML frontmatter of a markdown file, read loosely: top-level `key: value` lines,
/// quotes stripped; a folded or literal block (`>`, `|`), or an empty value followed by
/// indented lines, joined with spaces; `- x` lines, or `[a, b]`, joined with `, `. No
/// frontmatter, or one that does not end, is none.
fn frontmatter(bytes: &[u8]) -> Frontmatter {
    let text = String::from_utf8_lossy(bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut lines = text.split('\n').map(|l| l.trim_end_matches('\r'));
    if lines.next().map(str::trim_end) != Some("---") {
        return Frontmatter::default();
    }
    let mut block: Vec<&str> = Vec::new();
    let mut ended = false;
    for line in lines {
        if matches!(line.trim_end(), "---" | "...") {
            ended = true;
            break;
        }
        block.push(line);
    }
    if !ended {
        return Frontmatter::default();
    }
    let mut out = Frontmatter::default();
    let mut i = 0;
    while i < block.len() {
        let line = block[i];
        i += 1;
        if line.starts_with([' ', '\t', '#', '-']) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        // The lines that belong to this key.
        let mut more: Vec<&str> = Vec::new();
        while i < block.len()
            && (block[i].starts_with([' ', '\t'])
                || block[i].starts_with("- ")
                || block[i].trim().is_empty())
        {
            more.push(block[i].trim());
            i += 1;
        }
        let more: Vec<&str> = more.into_iter().filter(|l| !l.is_empty()).collect();
        let joined = if matches!(value, ">" | "|" | ">-" | "|-" | ">+" | "|+") {
            more.join(" ")
        } else if value.is_empty() && more.iter().all(|l| l.starts_with('-')) && !more.is_empty() {
            more.iter()
                .map(|l| unquote(l.trim_start_matches('-').trim()))
                .collect::<Vec<_>>()
                .join(", ")
        } else if value.is_empty() {
            more.join(" ")
        } else if let Some(list) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
            list.split(',')
                .map(|v| unquote(v.trim()))
                .filter(|v| !v.is_empty())
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            let mut joined = unquote(value).to_string();
            // A plain scalar continued on indented lines.
            for l in &more {
                joined.push(' ');
                joined.push_str(l);
            }
            joined
        };
        if joined.is_empty() {
            continue;
        }
        let slot = match key.trim() {
            "name" => &mut out.name,
            "description" => &mut out.description,
            "model" => &mut out.model,
            "effort" => &mut out.effort,
            "tools" => &mut out.tools,
            _ => continue,
        };
        *slot = Some(joined);
    }
    if let Some(d) = &mut out.description
        && d.chars().count() > MAX_DESCRIPTION
    {
        *d = d.chars().take(MAX_DESCRIPTION).collect();
    }
    out
}

fn unquote(s: &str) -> &str {
    for q in ['"', '\''] {
        if let Some(inner) = s.strip_prefix(q).and_then(|s| s.strip_suffix(q)) {
            return inner;
        }
    }
    s
}

/// `.claude.json`, names and the login's ids only: every other value is skipped unread.
#[derive(Deserialize, Default)]
struct ClaudeJson {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: Keys,
    #[serde(default, rename = "oauthAccount")]
    oauth: Option<OAuthIds>,
    #[serde(default)]
    projects: Option<BTreeMap<String, ProjectEntry>>,
}

#[derive(Deserialize)]
struct OAuthIds {
    #[serde(default, rename = "organizationUuid")]
    org: Option<Value>,
    #[serde(default, rename = "accountUuid")]
    account: Option<Value>,
}

#[derive(Deserialize, Default)]
struct ProjectEntry {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: Keys,
}

/// The keys of a JSON object, values skipped unread ([`IgnoredAny`]); anything else is none.
#[derive(Default)]
struct Keys(Vec<String>);

impl<'de> Deserialize<'de> for Keys {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct KeysVisitor;
        impl<'de> Visitor<'de> for KeysVisitor {
            type Value = Keys;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Keys, A::Error> {
                let mut keys = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    map.next_value::<IgnoredAny>()?;
                    keys.push(key);
                }
                Ok(Keys(keys))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Keys, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Keys::default())
            }
            fn visit_bool<E>(self, _: bool) -> Result<Keys, E> {
                Ok(Keys::default())
            }
            fn visit_i64<E>(self, _: i64) -> Result<Keys, E> {
                Ok(Keys::default())
            }
            fn visit_u64<E>(self, _: u64) -> Result<Keys, E> {
                Ok(Keys::default())
            }
            fn visit_f64<E>(self, _: f64) -> Result<Keys, E> {
                Ok(Keys::default())
            }
            fn visit_str<E>(self, _: &str) -> Result<Keys, E> {
                Ok(Keys::default())
            }
            fn visit_unit<E>(self) -> Result<Keys, E> {
                Ok(Keys::default())
            }
            fn visit_none<E>(self) -> Result<Keys, E> {
                Ok(Keys::default())
            }
        }
        d.deserialize_any(KeysVisitor)
    }
}

/// `.claude-plugin/plugin.json`: the parts the pane reads.
#[derive(Deserialize, Default)]
struct PluginJson {
    #[serde(default)]
    hooks: Option<Value>,
    #[serde(default, rename = "mcpServers")]
    mcp_servers: Option<KeysOrPath>,
}

/// `mcpServers` of a plugin manifest: the servers' names, or the path of a file with them.
enum KeysOrPath {
    Keys(Vec<String>),
    Path(String),
}

impl<'de> Deserialize<'de> for KeysOrPath {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = KeysOrPath;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object or a path")
            }
            fn visit_str<E>(self, path: &str) -> Result<KeysOrPath, E> {
                Ok(KeysOrPath::Path(path.to_string()))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<KeysOrPath, A::Error> {
                let keys = Keys::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(KeysOrPath::Keys(keys.0))
            }
            // Anything else names no server (and the rest of the manifest still counts).
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<KeysOrPath, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(KeysOrPath::Keys(Vec::new()))
            }
            fn visit_bool<E>(self, _: bool) -> Result<KeysOrPath, E> {
                Ok(KeysOrPath::Keys(Vec::new()))
            }
            fn visit_i64<E>(self, _: i64) -> Result<KeysOrPath, E> {
                Ok(KeysOrPath::Keys(Vec::new()))
            }
            fn visit_u64<E>(self, _: u64) -> Result<KeysOrPath, E> {
                Ok(KeysOrPath::Keys(Vec::new()))
            }
            fn visit_f64<E>(self, _: f64) -> Result<KeysOrPath, E> {
                Ok(KeysOrPath::Keys(Vec::new()))
            }
            fn visit_unit<E>(self) -> Result<KeysOrPath, E> {
                Ok(KeysOrPath::Keys(Vec::new()))
            }
        }
        d.deserialize_any(V)
    }
}

/// An `.mcp.json`: the names under `mcpServers`, else the top-level keys whose values are
/// objects; values skipped unread.
#[derive(Default)]
struct McpFile {
    servers: Option<Vec<String>>,
    objects: Vec<String>,
}

impl McpFile {
    fn names(self) -> Vec<String> {
        self.servers.unwrap_or(self.objects)
    }
}

impl<'de> Deserialize<'de> for McpFile {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        /// Whether a value is an object; its contents skipped unread.
        struct IsObject(bool);
        impl<'de> Deserialize<'de> for IsObject {
            fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V;
                impl<'de> Visitor<'de> for V {
                    type Value = IsObject;
                    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                        f.write_str("any JSON value")
                    }
                    fn visit_map<A: MapAccess<'de>>(
                        self,
                        mut map: A,
                    ) -> Result<IsObject, A::Error> {
                        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                        Ok(IsObject(true))
                    }
                    fn visit_seq<A: SeqAccess<'de>>(
                        self,
                        mut seq: A,
                    ) -> Result<IsObject, A::Error> {
                        while seq.next_element::<IgnoredAny>()?.is_some() {}
                        Ok(IsObject(false))
                    }
                    fn visit_bool<E>(self, _: bool) -> Result<IsObject, E> {
                        Ok(IsObject(false))
                    }
                    fn visit_i64<E>(self, _: i64) -> Result<IsObject, E> {
                        Ok(IsObject(false))
                    }
                    fn visit_u64<E>(self, _: u64) -> Result<IsObject, E> {
                        Ok(IsObject(false))
                    }
                    fn visit_f64<E>(self, _: f64) -> Result<IsObject, E> {
                        Ok(IsObject(false))
                    }
                    fn visit_str<E>(self, _: &str) -> Result<IsObject, E> {
                        Ok(IsObject(false))
                    }
                    fn visit_unit<E>(self) -> Result<IsObject, E> {
                        Ok(IsObject(false))
                    }
                }
                d.deserialize_any(V)
            }
        }
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = McpFile;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<McpFile, A::Error> {
                let mut file = McpFile::default();
                while let Some(key) = map.next_key::<String>()? {
                    if key == "mcpServers" {
                        let keys = map.next_value::<Keys>()?.0;
                        file.servers = Some(keys);
                    } else if map.next_value::<IsObject>()?.0 {
                        file.objects.push(key);
                    }
                }
                Ok(file)
            }
        }
        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    use std::sync::mpsc;
    use std::time::{Duration, SystemTime};

    use serde_json::json;

    use super::*;
    use crate::registry::{CLAUDE, Home};

    const ORG: &str = "00000000-0000-4000-8000-00000000000a";
    const ACCT: &str = "00000000-0000-4000-8000-00000000000b";
    const OTHER: &str = "00000000-0000-4000-8000-00000000000c_00000000-0000-4000-8000-00000000000d";

    struct Fx {
        _dir: tempfile::TempDir,
        root: PathBuf,
        env: Env,
        config: PathBuf,
    }

    fn fx() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        let env: Env = [("HOME".to_string(), root.join("home").display().to_string())].into();
        let config = root.join("remuda/config.toml");
        Fx {
            _dir: dir,
            root,
            env,
            config,
        }
    }

    fn named(name: &str, home: &Path) -> Account {
        Account {
            provider: CLAUDE,
            name: name.into(),
            home: Home::Path(home.display().to_string()),
        }
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn sharing(source: &Account) -> Sharing {
        Sharing {
            source: Some(source.clone()),
            opted_out: vec![],
        }
    }

    fn entries<'a>(view: &'a ConfigView, name: &str, origin: Origin) -> &'a [Entry] {
        let item = view
            .instructions
            .iter()
            .find(|i| i.name == name && i.origin == origin)
            .unwrap_or_else(|| panic!("{name} {origin:?}: {view:#?}"));
        match &item.content {
            Content::Entries(entries) => entries,
            other => panic!("{other:?}"),
        }
    }

    fn names(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    fn shared_items(view: &ConfigView) -> Vec<&str> {
        view.instructions
            .iter()
            .filter(|i| i.origin == Origin::Shared)
            .map(|i| i.name)
            .collect()
    }

    fn mkfifo(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    /// R22: frontmatter fields as agents and skills write them.
    #[test]
    fn frontmatter_fields() {
        let got = frontmatter(
            b"---\nname: reviewer\ndescription: \"Reviews diffs\"\nmodel: 'sonnet'\n\
              effort: low\ntools: Read, Grep\n---\nbody\n",
        );
        assert_eq!(
            got,
            Frontmatter {
                name: Some("reviewer".into()),
                description: Some("Reviews diffs".into()),
                model: Some("sonnet".into()),
                effort: Some("low".into()),
                tools: Some("Read, Grep".into()),
            }
        );
        // Folded description, block list of tools, CRLF, a BOM.
        let got = frontmatter(
            "\u{feff}---\r\nname: x\r\ndescription: >\r\n  one\r\n  two\r\ntools:\r\n  - Read\r\n  \
             - Bash\r\n---\r\n"
                .as_bytes(),
        );
        assert_eq!(got.description.as_deref(), Some("one two"));
        assert_eq!(got.tools.as_deref(), Some("Read, Bash"));
        assert_eq!(got.name.as_deref(), Some("x"));
        // Flow list; an unindented block list; a literal block ending at `...`.
        let got = frontmatter(b"---\ntools: [Read, \"Glob\"]\ndescription: |-\n  a\n...\n");
        assert_eq!(got.tools.as_deref(), Some("Read, Glob"));
        assert_eq!(got.description.as_deref(), Some("a"));
        let got = frontmatter(b"---\ntools:\n- Read\n- Edit\nmodel: haiku\n---\n");
        assert_eq!(got.tools.as_deref(), Some("Read, Edit"));
        assert_eq!(got.model.as_deref(), Some("haiku"));
        // None, unterminated, not first.
        assert_eq!(frontmatter(b"# title\n"), Frontmatter::default());
        assert_eq!(frontmatter(b"---\nname: x\n"), Frontmatter::default());
        assert_eq!(
            frontmatter(b"\n---\nname: x\n---\n"),
            Frontmatter::default()
        );
        // Capped.
        let long = format!("---\ndescription: {}\n---\n", "d".repeat(400));
        assert_eq!(
            frontmatter(long.as_bytes()).description.map(|d| d.len()),
            Some(MAX_DESCRIPTION)
        );
    }

    /// R22: agents (top level) with their frontmatter, skills with `SKILL.md` (followed
    /// through symlinks, broken ones marked, `synced` never one), commands by path.
    #[test]
    fn instructions_are_listed_with_their_frontmatter() {
        let f = fx();
        let home = f.root.join("max");
        write(
            &home.join("agents/reviewer.md"),
            "---\nname: reviewer\ndescription: reviews diffs\nmodel: sonnet\neffort: low\n\
             tools: Read, Grep\n---\n",
        );
        write(&home.join("agents/plain.md"), "no frontmatter\n");
        write(&home.join("agents/sub/x.md"), "---\nname: nested\n---\n");
        write(&home.join("agents/notes.txt"), "not an agent");
        write(
            &home.join("skills/pdf/SKILL.md"),
            "---\nname: pdf\ndescription: reads PDFs\n---\n",
        );
        fs::create_dir_all(home.join("skills/empty")).unwrap();
        write(
            &home.join("skills/synced/a_b/s/SKILL.md"),
            "---\nname: s\n---\n",
        );
        write(
            &f.root.join("lib/linked/SKILL.md"),
            "---\ndescription: shared\n---\n",
        );
        symlink("../../lib/linked", home.join("skills/linked")).unwrap();
        symlink("../../lib/nothing", home.join("skills/gone")).unwrap();
        write(&home.join("skills/.hidden/SKILL.md"), "");
        write(
            &home.join("commands/git/commit.md"),
            "---\ndescription: commit\n---\n",
        );
        write(&home.join("commands/top.md"), "");
        write(&home.join("commands/.hidden.md"), "");
        write(&home.join("CLAUDE.md"), "one\ntwo\nthree");

        let view = read(&named("max", &home), &Sharing::default(), None, &f.env);
        assert_eq!(view.role, Role::Alone);
        assert_eq!(view.problems, Vec::<String>::new());
        let item = &view.instructions[0];
        assert_eq!(item.name, "CLAUDE.md");
        assert_eq!(
            item.content,
            Content::File {
                bytes: 13,
                lines: 3
            }
        );
        let agents = entries(&view, "agents", Origin::Own);
        assert_eq!(names(agents), ["plain", "reviewer"]);
        assert_eq!(
            agents[1],
            Entry {
                name: "reviewer".into(),
                description: Some("reviews diffs".into()),
                model: Some("sonnet".into()),
                effort: Some("low".into()),
                tools: Some("Read, Grep".into()),
                ..Entry::default()
            }
        );
        let skills = entries(&view, "skills", Origin::Own);
        assert_eq!(names(skills), ["gone", "linked", "pdf"]);
        assert!(skills[0].broken);
        assert_eq!(skills[0].link, Some(PathBuf::from("../../lib/nothing")));
        assert_eq!(skills[1].link, Some(PathBuf::from("../../lib/linked")));
        assert_eq!(skills[1].description.as_deref(), Some("shared"));
        assert!(!skills[1].broken);
        let commands = entries(&view, "commands", Origin::Own);
        assert_eq!(names(commands), ["git:commit", "top"]);
        assert_eq!(commands[0].description.as_deref(), Some("commit"));
        assert_eq!(view.synced, Synced::Unmatched { buckets: 1 });
    }

    /// R22: of `skills/synced/`, only the bucket of the account's own login is listed; the
    /// bucket names are never kept.
    #[test]
    fn synced_skills_are_the_accounts_own_bucket() {
        let f = fx();
        let home = f.root.join("max");
        let bucket = format!("{ORG}_{ACCT}");
        write(
            &home.join(format!("skills/synced/{bucket}/brief/SKILL.md")),
            "---\nname: brief\n---\n",
        );
        write(
            &home.join(format!("skills/synced/{OTHER}/theirs/SKILL.md")),
            "",
        );
        write(
            &home.join(".claude.json"),
            &json!({"oauthAccount": {"organizationUuid": ORG, "accountUuid": ACCT,
                                     "emailAddress": "someone@example.com"}})
            .to_string(),
        );
        let account = named("max", &home);
        let view = read(&account, &Sharing::default(), None, &f.env);
        let Synced::Skills { skills, others } = &view.synced else {
            panic!("{:?}", view.synced)
        };
        assert_eq!((names(skills), *others), (vec!["brief"], 1));
        let debug = format!("{view:?}");
        for secret in [ORG, ACCT, OTHER, "someone"] {
            assert!(!debug.contains(secret), "{secret} in {debug}");
        }
        assert_eq!(
            names(entries(&view, "skills", Origin::Own)),
            Vec::<&str>::new()
        );

        fs::write(home.join(".claude.json"), "{}").unwrap();
        let view = read(&account, &Sharing::default(), None, &f.env);
        assert_eq!(view.synced, Synced::Unmatched { buckets: 2 });
        assert!(!format!("{view:?}").contains(ORG));
    }

    /// R22: a skill turned off by `skillOverrides` is marked, a plugin's also as
    /// `<plugin>:<skill>`; an override naming no skill is stale.
    #[test]
    fn skill_overrides_mark_off_and_stale() {
        let f = fx();
        let home = f.root.join("max");
        write(&home.join("skills/pdf/SKILL.md"), "");
        write(&home.join("skills/docx/SKILL.md"), "");
        let plugin = f.root.join("cache/plug");
        write(&plugin.join("skills/tool/SKILL.md"), "");
        write(
            &home.join("plugins/installed_plugins.json"),
            &json!({"version": 2, "plugins": {
                "plug@m": [{"scope": "user", "installPath": plugin, "version": "1.0.0"}]}})
            .to_string(),
        );
        write(
            &home.join("settings.json"),
            &json!({
                "enabledPlugins": {"plug@m": true},
                "skillOverrides": {"pdf": "off", "plug:tool": "off", "ghost": "off",
                                   "docx": "on"},
            })
            .to_string(),
        );
        let view = read(&named("max", &home), &Sharing::default(), None, &f.env);
        let skills = entries(&view, "skills", Origin::Own);
        assert_eq!(
            skills
                .iter()
                .map(|e| (e.name.as_str(), e.off))
                .collect::<Vec<_>>(),
            [("docx", false), ("pdf", true)]
        );
        let contents = view.plugins[0].contents.as_ref().unwrap();
        assert!(contents.skills[0].off);
        assert_eq!(view.stale_overrides, ["ghost"]);
        assert!(
            !view
                .own_settings
                .other
                .contains(&"skillOverrides".to_string())
        );
    }

    /// R18, R22: a member sees what the launch injects and from where: shared instructions,
    /// the settings it lacks without authentication, the injected auto-memory; its own
    /// symlinked skills are already the source's.
    #[test]
    fn a_member_sees_what_is_shared_and_from_where() {
        let f = fx();
        let (src, max, work) = (f.root.join("src"), f.root.join("max"), f.root.join("work"));
        fs::create_dir_all(&work).unwrap();
        write(&src.join("CLAUDE.md"), "be brief\n");
        write(&src.join("skills/s1/SKILL.md"), "");
        write(
            &src.join("settings.json"),
            &json!({
                "model": "opus",
                "env": {"ANTHROPIC_API_KEY": "zqsecret", "DISABLE_TELEMETRY": "1"},
                "permissions": {"allow": ["Read", "Bash(ls)"]},
                "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "a"},
                                              {"type": "command", "command": "b"}]}]},
                "statusLine": {"type": "command", "command": "s"},
            })
            .to_string(),
        );
        write(
            &max.join("settings.json"),
            &json!({"model": "sonnet", "env": {"FOO": "1"}}).to_string(),
        );
        symlink(src.join("skills"), max.join("skills")).unwrap();
        let source = named("src", &src);
        let view = read(&named("max", &max), &sharing(&source), Some(&work), &f.env);
        assert_eq!(
            view.role,
            Role::Member {
                source: "claude:src".into()
            }
        );
        assert_eq!(
            names(entries(&view, "skills", Origin::AlreadySource)),
            ["s1"]
        );
        assert_eq!(shared_items(&view), ["CLAUDE.md", "skills"]);
        assert_eq!(view.own_settings.model.as_deref(), Some("sonnet"));
        assert_eq!(view.own_settings.env, ["FOO"]);
        let shared = &view.shared_settings;
        assert_eq!(shared.model, None);
        assert_eq!(shared.env, ["DISABLE_TELEMETRY"]);
        assert_eq!(shared.hooks, [("Stop".to_string(), 2)]);
        assert_eq!(shared.permissions, [2, 0, 0]);
        assert!(shared.status_line);
        assert_eq!(shared.other, Vec::<String>::new());
        assert!(view.withheld.contains(&"env.ANTHROPIC_API_KEY".to_string()));
        assert_eq!(view.memory.origin, Origin::Shared);
        assert!(
            view.memory
                .dir
                .as_deref()
                .is_some_and(|d| d.starts_with(&src.display().to_string())),
            "{:?}",
            view.memory
        );
        assert_eq!(view.memory.files, None);
        assert!(!format!("{view:?}").contains("zqsecret"));
    }

    /// R22: one row per enabled plugin: the install a session in the directory loads (the most
    /// specific), its version, scope and number of records; what it adds; `false` ones counted.
    #[test]
    fn plugins_are_deduplicated_to_the_effective_install() {
        let f = fx();
        let (home, work, other) = (f.root.join("max"), f.root.join("work"), f.root.join("o"));
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&other).unwrap();
        let cache = f.root.join("cache");
        for v in ["p1", "p2", "p3"] {
            fs::create_dir_all(cache.join(v)).unwrap();
        }
        let p3 = cache.join("p3");
        write(&p3.join("agents/a.md"), "---\nmodel: haiku\n---\n");
        write(&p3.join("skills/s/SKILL.md"), "");
        write(&p3.join("commands/c.md"), "");
        write(
            &p3.join("hooks/hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"hooks":[{},{}]}]}}"#,
        );
        write(
            &p3.join(".mcp.json"),
            r#"{"mcpServers":{"ctx":{"env":{"K":"zqsecret"}}}}"#,
        );
        write(
            &home.join("plugins/installed_plugins.json"),
            &json!({"version": 2, "plugins": {"p@m": [
                {"scope": "user", "installPath": cache.join("p1"), "version": "1"},
                {"scope": "project", "installPath": cache.join("p2"), "version": "2",
                 "projectPath": other},
                {"scope": "local", "installPath": p3, "version": "3", "projectPath": work},
            ]}})
            .to_string(),
        );
        write(
            &home.join("settings.json"),
            &json!({"enabledPlugins": {"p@m": true, "none@m": true, "x@m": false,
                                       "y@m": false}})
            .to_string(),
        );
        let view = read(
            &named("max", &home),
            &Sharing::default(),
            Some(&work),
            &f.env,
        );
        assert_eq!(view.disabled_plugins, 2);
        assert_eq!(
            view.plugins
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["none@m", "p@m"]
        );
        let none = &view.plugins[0];
        assert_eq!((none.path.as_ref(), none.installs), (None, 0));
        let p = &view.plugins[1];
        assert_eq!(p.origin, Origin::Own);
        assert_eq!(p.version.as_deref(), Some("3"));
        assert_eq!(p.scope.as_deref(), Some("local"));
        assert_eq!(p.installs, 3);
        assert_eq!(p.path.as_ref(), Some(&p3));
        let contents = p.contents.as_ref().unwrap();
        assert_eq!(names(&contents.agents), ["a"]);
        assert_eq!(contents.agents[0].model.as_deref(), Some("haiku"));
        assert_eq!(names(&contents.skills), ["s"]);
        assert_eq!(names(&contents.commands), ["c"]);
        assert_eq!(contents.hooks, [("PreToolUse".to_string(), 2)]);
        assert_eq!(contents.mcp_servers, ["ctx"]);
        assert!(!format!("{view:?}").contains("zqsecret"));

        // In the other project, its install; elsewhere, the user install.
        let view = read(
            &named("max", &home),
            &Sharing::default(),
            Some(&other),
            &f.env,
        );
        assert_eq!(view.plugins[1].version.as_deref(), Some("2"));
        let view = read(&named("max", &home), &Sharing::default(), None, &f.env);
        assert_eq!(view.plugins[1].version.as_deref(), Some("1"));
    }

    /// R22: without `hooks/hooks.json` and `.mcp.json`, a plugin's manifest names its hooks
    /// and MCP servers, inline or by a file; anything else there names none.
    #[test]
    fn plugin_manifests_name_hooks_and_servers() {
        let f = fx();
        let root = f.root.join("plugin");
        write(
            &root.join(".claude-plugin/plugin.json"),
            r#"{"name":"p","hooks":{"Stop":[{"hooks":[{}]}]},"mcpServers":{"b":{},"a":{}}}"#,
        );
        let got = plugin_contents(&root);
        assert_eq!(got.hooks, [("Stop".to_string(), 1)]);
        assert_eq!(got.mcp_servers, ["a", "b"]);

        write(
            &root.join(".claude-plugin/plugin.json"),
            r#"{"hooks":"./extra/hooks.json","mcpServers":"./extra/mcp.json"}"#,
        );
        write(
            &root.join("extra/hooks.json"),
            r#"{"hooks":{"PostToolUse":[{"hooks":[{},{}]},{"hooks":[{}]}]}}"#,
        );
        write(
            &root.join("extra/mcp.json"),
            r#"{"x":{"command":"zqsecret"},"n":1}"#,
        );
        let got = plugin_contents(&root);
        assert_eq!(got.hooks, [("PostToolUse".to_string(), 3)]);
        assert_eq!(got.mcp_servers, ["x"]);
        assert!(!format!("{got:?}").contains("zqsecret"));

        write(
            &root.join(".claude-plugin/plugin.json"),
            r#"{"hooks":{"Stop":[{"hooks":[{}]}]},"mcpServers":[]}"#,
        );
        let got = plugin_contents(&root);
        assert_eq!(got.hooks, [("Stop".to_string(), 1)]);
        assert_eq!(got.mcp_servers, Vec::<String>::new());

        // A manifest's paths stay inside the plugin: never `..`, absolute, or a symlink out.
        let outside = f.root.join("home/.credentials.json");
        write(
            &outside,
            r#"{"zqoauth":{"hooks":{"Stop":[{"hooks":[{}]}]}}}"#,
        );
        std::os::unix::fs::symlink(&outside, root.join("extra/link.json")).unwrap();
        for path in [
            outside.display().to_string(),
            "../home/.credentials.json".to_string(),
            "./extra/../../home/.credentials.json".to_string(),
            "./extra/link.json".to_string(),
        ] {
            write(
                &root.join(".claude-plugin/plugin.json"),
                &format!(r#"{{"hooks":{path:?},"mcpServers":{path:?}}}"#),
            );
            let got = plugin_contents(&root);
            assert_eq!(got.mcp_servers, Vec::<String>::new(), "{path}");
            assert_eq!(got.hooks, Vec::new(), "{path}");
        }
    }

    /// R12, R22: a home's own install that resolves (through a symlinked profile directory)
    /// to the source's user install is already the source's, and is listed once.
    #[test]
    fn an_own_install_resolving_to_the_sources_is_already_the_sources() {
        let f = fx();
        let (src, max) = (f.root.join("src"), f.root.join("max"));
        let q = src.join("plugins/cache/q");
        fs::create_dir_all(&q).unwrap();
        write(
            &src.join("plugins/installed_plugins.json"),
            &json!({"version": 2, "plugins": {"q@m": [{"scope": "user", "installPath": q}]}})
                .to_string(),
        );
        write(
            &src.join("settings.json"),
            &json!({"enabledPlugins": {"q@m": true}}).to_string(),
        );
        symlink(&src, f.root.join("profile")).unwrap();
        let through = f.root.join("profile/plugins/cache/q");
        write(
            &max.join("plugins/installed_plugins.json"),
            &json!({"version": 2, "plugins": {"q@m": [{"scope": "user", "installPath": through}]}})
                .to_string(),
        );
        write(
            &max.join("settings.json"),
            &json!({"enabledPlugins": {"q@m": true}}).to_string(),
        );
        let view = read(
            &named("max", &max),
            &sharing(&named("src", &src)),
            None,
            &f.env,
        );
        assert_eq!(view.plugins.len(), 1, "{:?}", view.plugins);
        assert_eq!(view.plugins[0].origin, Origin::AlreadySource);
        assert_eq!(view.plugins[0].path.as_ref(), Some(&through));
    }

    /// R18, R22: the source (its authentication listed as withheld), an opted-out account and
    /// a member of a missing source get nothing shared.
    #[test]
    fn roles() {
        let f = fx();
        let (src, max) = (f.root.join("src"), f.root.join("max"));
        write(&src.join("CLAUDE.md"), "x");
        write(
            &src.join("settings.json"),
            &json!({"apiKeyHelper": "/bin/key", "model": "opus"}).to_string(),
        );
        fs::create_dir_all(&max).unwrap();
        let (source, member) = (named("src", &src), named("max", &max));
        let alone = read(&member, &Sharing::default(), None, &f.env);
        assert_eq!(alone.role, Role::Alone);

        let view = read(&source, &sharing(&source), None, &f.env);
        assert_eq!(view.role, Role::Source);
        assert_eq!(view.withheld, ["apiKeyHelper"]);
        assert_eq!(shared_items(&view), Vec::<&str>::new());

        let opted = Sharing {
            opted_out: vec!["claude:max".into()],
            ..sharing(&source)
        };
        let view = read(&member, &opted, None, &f.env);
        assert_eq!(
            view.role,
            Role::OptedOut {
                source: "claude:src".into()
            }
        );
        assert_eq!(view.role.source(), Some("claude:src"));
        assert_eq!(shared_items(&view), Vec::<&str>::new());
        assert_eq!(view.withheld, Vec::<String>::new());

        let gone = sharing(&named("gone", &f.root.join("gone")));
        let view = read(&member, &gone, None, &f.env);
        assert_eq!(
            view.role,
            Role::SourceMissing {
                source: "claude:gone".into()
            }
        );
        assert_eq!(shared_items(&view), Vec::<&str>::new());
        assert_eq!(view.shared_settings, Summary::default());
    }

    /// R22: MCP servers by name only: the user's and those of the project's entry, keyed by
    /// the project root; the native login's `.claude.json` is `$HOME/.claude.json`.
    #[test]
    fn mcp_names_only() {
        let f = fx();
        let max = f.root.join("max");
        let repo = f.root.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("sub")).unwrap();
        write(
            &max.join(".claude.json"),
            &json!({
                "numStartups": 3,
                "tipsHistory": [1, "x", null, {"a": true}],
                "mcpServers": {"gh": {"env": {"TOKEN": "zqsecret"}}},
                "projects": {
                    repo.display().to_string(): {"mcpServers": {"db": {"args": ["zqsecret"]}},
                                                "history": [{"display": "zqsecret"}]},
                    "/other": {"mcpServers": {"elsewhere": {}}},
                    "/third": {"mcpServers": null},
                },
            })
            .to_string(),
        );
        let sub = repo.join("sub");
        let view = read(&named("max", &max), &Sharing::default(), Some(&sub), &f.env);
        assert_eq!(view.mcp.user, ["gh"]);
        assert_eq!(view.mcp.project, ["db"]);
        assert_eq!(view.problems, Vec::<String>::new());
        assert!(!format!("{view:?}").contains("zqsecret"));

        write(
            &f.root.join("home/.claude.json"),
            &json!({"mcpServers": {"native": {}}}).to_string(),
        );
        fs::create_dir_all(f.root.join("home/.claude")).unwrap();
        let view = read(
            &Account::default_for(CLAUDE),
            &Sharing::default(),
            None,
            &f.env,
        );
        assert_eq!(view.mcp.user, ["native"]);
        assert_eq!(view.mcp.project, Vec::<String>::new());

        fs::write(max.join(".claude.json"), "{").unwrap();
        let view = read(&named("max", &max), &Sharing::default(), None, &f.env);
        assert_eq!(
            view.problems,
            [".claude.json is not in a recognized format"]
        );
    }

    /// R18, R22: a settings file that would fail the launch is the first problem, and nothing
    /// is shown as shared.
    #[test]
    fn settings_problems_are_shown() {
        let f = fx();
        let (src, max) = (f.root.join("src"), f.root.join("max"));
        write(&src.join("CLAUDE.md"), "x");
        write(&src.join("settings.json"), "[]");
        fs::create_dir_all(&max).unwrap();
        let view = read(
            &named("max", &max),
            &sharing(&named("src", &src)),
            None,
            &f.env,
        );
        assert!(
            view.problems[0].contains("is not a JSON object"),
            "{:?}",
            view.problems
        );
        assert!(view.problems[0].starts_with("sessions of this account fail to start: "));
        assert_eq!(
            view.role,
            Role::Member {
                source: "claude:src".into()
            }
        );
        assert_eq!(shared_items(&view), Vec::<&str>::new());
        assert!(view.plugins.is_empty());

        // The home's own: said once.
        write(&src.join("settings.json"), "{}");
        write(&max.join("settings.json"), "1");
        let view = read(
            &named("max", &max),
            &sharing(&named("src", &src)),
            None,
            &f.env,
        );
        assert_eq!(view.problems.len(), 1, "{:?}", view.problems);
    }

    /// R18, R22: the pane shows what the launch injects: `--add-dir`, each `--plugin-dir`,
    /// the settings keys and the auto-memory directory of the `--settings` file.
    #[test]
    fn the_pane_agrees_with_inject() {
        let f = fx();
        let (src, max, work) = (f.root.join("src"), f.root.join("max"), f.root.join("work"));
        fs::create_dir_all(&work).unwrap();
        write(&src.join("CLAUDE.md"), "x");
        write(&src.join("skills/s/SKILL.md"), "");
        write(&src.join("agents/a.md"), "");
        fs::create_dir_all(&max).unwrap();
        symlink(src.join("skills"), max.join("skills")).unwrap();
        let cache = src.join("plugins/cache");
        for p in ["a", "b", "c"] {
            fs::create_dir_all(cache.join(p)).unwrap();
        }
        let install = |p: &str| json!([{"scope": "user", "installPath": cache.join(p)}]);
        write(
            &src.join("plugins/installed_plugins.json"),
            &json!({"version": 2, "plugins": {"a@m": install("a"), "b@m": install("b"),
                                             "c@m": install("c")}})
            .to_string(),
        );
        write(
            &src.join("settings.json"),
            &json!({
                "model": "opus",
                "theme": "dark",
                "env": {"DISABLE_TELEMETRY": "1", "ANTHROPIC_API_KEY": "k"},
                "enabledPlugins": {"a@m": true, "b@m": true, "c@m": true},
            })
            .to_string(),
        );
        write(
            &max.join("settings.json"),
            &json!({"enabledPlugins": {"c@m": false}}).to_string(),
        );
        let (source, member) = (named("src", &src), named("max", &max));
        let sharing = sharing(&source);
        let shared = share::inject(&sharing, &member, &[], Some(&work), &f.env, &f.config).unwrap();
        let view = read(&member, &sharing, Some(&work), &f.env);

        assert!(shared.args.iter().any(|a| a.starts_with("--add-dir=")));
        assert!(view.instructions.iter().any(|i| i.origin == Origin::Shared));
        let mut dirs: Vec<PathBuf> = shared
            .args
            .iter()
            .filter_map(|a| a.strip_prefix("--plugin-dir="))
            .map(PathBuf::from)
            .collect();
        dirs.sort();
        let mut shown: Vec<PathBuf> = view
            .plugins
            .iter()
            .filter(|p| p.origin == Origin::Shared)
            .filter_map(|p| p.path.clone())
            .collect();
        shown.sort();
        assert_eq!(dirs, [cache.join("a"), cache.join("b")]);
        assert_eq!(shown, dirs);
        assert!(
            view.plugins
                .iter()
                .any(|p| p.name == "c@m" && p.origin == Origin::NotShared(Skip::TurnedOff))
        );

        let file = shared
            .args
            .iter()
            .find_map(|a| a.strip_prefix("--settings="))
            .unwrap();
        let mut file = share::read_settings(Path::new(file)).unwrap();
        let memory = file.remove(MEMORY_KEY).unwrap();
        assert_eq!(view.memory.dir.as_deref(), memory.as_str());
        assert_eq!(view.memory.origin, Origin::Shared);
        let plan = share::plan(&sharing, &member, &[], Some(&work), &f.env).unwrap();
        let mut planned = plan.settings.clone();
        planned.remove(MEMORY_KEY);
        assert_eq!(
            file.keys().collect::<Vec<_>>(),
            planned.keys().collect::<Vec<_>>()
        );
        assert_eq!(view.shared_settings, Summary::of(&file));
        assert_eq!(view.shared_settings.env, ["DISABLE_TELEMETRY"]);
        assert_eq!(view.shared_settings.model.as_deref(), Some("opus"));
        assert_eq!(view.shared_settings.other, ["theme"]);
    }

    /// Every entry under `root` (not following symlinks): path, size, mtime.
    fn tree(root: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for e in fs::read_dir(&dir).unwrap().flatten() {
                let meta = fs::symlink_metadata(e.path()).unwrap();
                out.push((e.path(), meta.len(), meta.modified().unwrap()));
                if meta.is_dir() {
                    stack.push(e.path());
                }
            }
        }
        out.sort();
        out
    }

    /// R13, R22: reading writes nothing (not even the launch's link or settings file), and
    /// never opens credentials: a FIFO there, or in place of an agent, would block forever.
    #[test]
    fn reading_writes_nothing_and_never_opens_credentials() {
        let f = fx();
        let (src, max, work) = (f.root.join("src"), f.root.join("max"), f.root.join("work"));
        fs::create_dir_all(&work).unwrap();
        write(&src.join("CLAUDE.md"), "x");
        write(&src.join("settings.json"), r#"{"model": "opus"}"#);
        write(&max.join("agents/ok.md"), "");
        mkfifo(&max.join(".credentials.json"));
        mkfifo(&src.join(".credentials.json"));
        mkfifo(&max.join("agents/pipe.md"));
        mkfifo(&max.join("CLAUDE.md"));
        let before = tree(&f.root);
        let (member, sharing) = (named("max", &max), sharing(&named("src", &src)));
        let env = f.env.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(read(&member, &sharing, Some(&work), &env));
        });
        let view = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("read blocked");
        assert_eq!(names(entries(&view, "agents", Origin::Own)), ["ok"]);
        assert_eq!(shared_items(&view), ["CLAUDE.md"]);
        assert_eq!(tree(&f.root), before);
        assert!(!share::dir(&f.config).exists());
        assert!(!share::settings_dir(&f.config).exists());
    }
}
