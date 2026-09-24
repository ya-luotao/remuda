//! Shared configuration, injected at launch (SPEC R18): a claude account gets the source
//! account's instructions, settings, enabled plugins and auto-memory location as launch
//! options. Nothing is written into any home (R13); the only file remuda keeps for it is the
//! `.claude` symlink in `$REMUDA_HOME/shared/claude/`.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::Env;
use crate::launch;
use crate::registry::{Account, Sharing};

/// With it set, `--add-dir` also loads `CLAUDE.md` from the added directory (R18).
pub const CLAUDE_MD_VAR: &str = "CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD";
/// The instruction items of a home that `--add-dir=$REMUDA_HOME/shared/claude` shares.
pub const INSTRUCTIONS: [&str; 4] = ["CLAUDE.md", "skills", "commands", "agents"];
/// The settings key that moves auto-memory.
pub const MEMORY_KEY: &str = "autoMemoryDirectory";
/// Longer project encodings are **[unverified]** in R18: no auto-memory is injected for them.
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
/// skipped when the home already shares it with the source (same realpath, R12).
///
/// Only a settings file that is not a JSON object fails: everything else that cannot be read
/// or made degrades to injecting less, with a notice where the user would miss it.
pub fn inject(
    sharing: &Sharing,
    account: &Account,
    user_args: &[String],
    cwd: Option<&Path>,
    env: &Env,
    shared_dir: &Path,
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
        match ensure_link(shared_dir, &from) {
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

    let source_settings = if settings_part || memory_part || plugins_part {
        read_settings(&from.join("settings.json"))?
    } else {
        Map::new()
    };
    // The home's own settings: read only when it is a file that is not the source's (a
    // directory or a dangling link defines nothing).
    let own = if !(settings_part || memory_part) {
        Map::new()
    } else if settings_shared {
        source_settings.clone()
    } else if fs::metadata(&own_path).is_ok_and(|m| m.is_file()) {
        read_settings(&own_path)?
    } else {
        Map::new()
    };
    let mut injected = if settings_part {
        missing_from(&source_settings, &own)
    } else {
        Map::new()
    };
    // A location either side chose is kept: the source's comes with its settings, and the
    // home's own wins like any key it defines.
    if memory_part && !source_settings.contains_key(MEMORY_KEY) && !own.contains_key(MEMORY_KEY) {
        let path_var = env.get("PATH").map(String::as_str);
        let git = launch::find_on_path("git", path_var).ok();
        if let Some(memory) = cwd.and_then(|cwd| memory_dir(&from, cwd, git.as_deref())) {
            injected.insert(MEMORY_KEY.to_string(), Value::String(memory));
        }
    }
    if !injected.is_empty() {
        let json = serde_json::to_string(&Value::Object(injected))?;
        shared.args.push(format!("--settings={json}"));
    }

    if plugins_part {
        let installs = installed_plugins(&from);
        for plugin in enabled_plugins(&source_settings) {
            if let Some(path) = installs.install_path(&plugin) {
                shared.args.push(format!("--plugin-dir={}", path.display()));
            }
        }
    }
    Ok(shared)
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

/// `<source>/projects/<project>/memory` for a session in `cwd`, where `<project>` is claude's
/// encoding of the project root (R18); `None` when the encoding is too long or not UTF-8.
pub fn memory_dir(source: &Path, cwd: &Path, git: Option<&Path>) -> Option<String> {
    let root = git
        .and_then(|git| main_worktree(git, cwd))
        .unwrap_or_else(|| cwd.to_path_buf());
    let project = encode_project(root.to_str()?)?;
    Some(
        source
            .join("projects")
            .join(project)
            .join("memory")
            .display()
            .to_string(),
    )
}

/// Every character that is not an ASCII letter or digit becomes `-`; `None` beyond
/// [`MAX_PROJECT_NAME`] characters (R18).
pub fn encode_project(root: &str) -> Option<String> {
    let encoded: String = root
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    (encoded.chars().count() <= MAX_PROJECT_NAME).then_some(encoded)
}

/// The main repository's root for a directory inside a git repository or one of its
/// worktrees: the parent of the common git directory, from one `git` invocation. `None`
/// outside a repository or when git fails.
pub fn main_worktree(git: &Path, cwd: &Path) -> Option<PathBuf> {
    let out = Command::new(git)
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let common = Path::new(text.trim_end_matches(['\n', '\r']));
    if !common.is_absolute() {
        return None;
    }
    common.parent().map(Path::to_path_buf)
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

    /// R18: every character that is not an ASCII letter or digit becomes `-`; names over 200
    /// characters are not encoded (unverified).
    #[test]
    fn project_names_are_encoded_like_claude() {
        assert_eq!(
            encode_project("/Users/you/my repo.v2_x").as_deref(),
            Some("-Users-you-my-repo-v2-x")
        );
        assert_eq!(encode_project("/tmp/café").as_deref(), Some("-tmp-caf-"));
        let at_limit = format!("/{}", "a".repeat(MAX_PROJECT_NAME - 1));
        assert_eq!(encode_project(&at_limit).map(|e| e.len()), Some(200));
        let over = format!("/{}", "a".repeat(MAX_PROJECT_NAME));
        assert_eq!(encode_project(&over), None);
        // Counted in characters, not bytes.
        let wide = format!("/{}", "é".repeat(MAX_PROJECT_NAME - 1));
        assert_eq!(encode_project(&wide).map(|e| e.chars().count()), Some(200));
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
            .args(args)
            .env("HOME", dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// R18: the project root is the main repository's root, also from a subdirectory or a
    /// worktree; outside a repository (or without git) it is the directory itself.
    #[test]
    fn memory_follows_the_main_repository() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let repo = root.join("my repo");
        fs::create_dir_all(repo.join("sub/dir")).unwrap();
        run_git(&repo, &["init", "-q"]);
        run_git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let tree = root.join("tree");
        run_git(
            &repo,
            &["worktree", "add", "-q", tree.to_str().unwrap(), "-b", "t"],
        );
        let plain = root.join("plain");
        fs::create_dir_all(&plain).unwrap();

        let git = git();
        assert_eq!(main_worktree(&git, &repo), Some(repo.clone()));
        assert_eq!(
            main_worktree(&git, &repo.join("sub/dir")),
            Some(repo.clone())
        );
        assert_eq!(main_worktree(&git, &tree), Some(repo.clone()));
        assert_eq!(main_worktree(&git, &plain), None);

        let source = Path::new("/src/home");
        let want = |root: &Path| {
            format!(
                "/src/home/projects/{}/memory",
                encode_project(root.to_str().unwrap()).unwrap()
            )
        };
        assert_eq!(
            memory_dir(source, &repo.join("sub/dir"), Some(&git)),
            Some(want(&repo))
        );
        assert_eq!(memory_dir(source, &tree, Some(&git)), Some(want(&repo)));
        assert_eq!(memory_dir(source, &plain, Some(&git)), Some(want(&plain)));
        assert_eq!(memory_dir(source, &tree, None), Some(want(&tree)));
        let long = root.join("x".repeat(MAX_PROJECT_NAME));
        fs::create_dir_all(&long).unwrap();
        assert_eq!(memory_dir(source, &long, Some(&git)), None);
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
        let shared_dir = root.join("remuda/shared/claude");
        let env = Env::new();
        let inject = |account: &Account| {
            inject(&sharing, account, &[], Some(&root), &env, &shared_dir).unwrap()
        };
        let got = inject(&named("max", &max));
        assert_eq!(
            got.args,
            [
                format!("--add-dir={}", shared_dir.display()),
                format!(
                    "--settings={}",
                    json!({
                        "autoMemoryDirectory": format!(
                            "{}/projects/{}/memory",
                            source_home.display(),
                            encode_project(root.to_str().unwrap()).unwrap()
                        ),
                        "model": "opus",
                    })
                ),
            ]
        );
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
            super::inject(&gone, &named("max", &max), &[], None, &env, &shared_dir).unwrap(),
            Shared::default()
        );
    }
}
