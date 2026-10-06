//! Claude homes for tests: a builder for what a home holds (SPEC R18), and a temporary root to
//! keep homes in. There is one copy of this file: integration tests reach it through
//! `tests/common`, the library's unit tests through `src/lib.rs`, which is why it uses nothing
//! of the crate.

// Not every test that includes this file uses every helper.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// A claude home being filled: every method puts one thing into it and panics when it cannot.
pub struct ClaudeHome {
    dir: PathBuf,
}

impl ClaudeHome {
    /// The home at `dir`, created with the directories above it.
    pub fn at(dir: impl Into<PathBuf>) -> ClaudeHome {
        let dir = dir.into();
        fs::create_dir_all(&dir).unwrap();
        ClaudeHome { dir }
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    pub fn into_path(self) -> PathBuf {
        self.dir
    }

    /// The directory `rel`, with the ones above it.
    pub fn dir(self, rel: &str) -> ClaudeHome {
        fs::create_dir_all(self.dir.join(rel)).unwrap();
        self
    }

    /// The file `rel` holding `content`, in a directory made as needed.
    pub fn file(self, rel: &str, content: &str) -> ClaudeHome {
        let path = self.dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
        self
    }

    pub fn claude_md(self, text: &str) -> ClaudeHome {
        self.file("CLAUDE.md", text)
    }

    /// `settings.json` with `json` as its text, valid or not.
    pub fn settings(self, json: &str) -> ClaudeHome {
        self.file("settings.json", json)
    }

    /// The directory of the skill `name`, without a `SKILL.md`.
    pub fn skill(self, name: &str) -> ClaudeHome {
        self.dir(&format!("skills/{name}"))
    }

    /// A `user` install of the plugin `name` (`name@marketplace`) in the directory `install`
    /// of the home, which is created, recorded in `plugins/installed_plugins.json` (format
    /// version 2) after the installs already there, with `version` when given.
    pub fn plugin(self, name: &str, install: &str, version: Option<&str>) -> ClaudeHome {
        let path = self.dir.join(install);
        fs::create_dir_all(&path).unwrap();
        let list = self.dir.join("plugins/installed_plugins.json");
        let mut file: Value = match fs::read(&list) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap(),
            Err(_) => json!({"version": 2, "plugins": {}}),
        };
        let mut record = json!({"scope": "user", "installPath": path});
        if let Some(version) = version {
            record["version"] = json!(version);
        }
        let installs = &mut file["plugins"][name];
        if installs.is_null() {
            *installs = json!([]);
        }
        installs.as_array_mut().unwrap().push(record);
        self.file("plugins/installed_plugins.json", &file.to_string())
    }

    /// `rel` as a symlink whose target does not exist.
    pub fn dangling(self, rel: &str) -> ClaudeHome {
        symlink(self.dir.join("nowhere"), self.dir.join(rel)).unwrap();
        self
    }

    /// Each of `items` as a symlink to that item of the home `source`, written from `source`
    /// as given: the links of a member's home (R18).
    pub fn linked(self, source: &Path, items: &[&str]) -> ClaudeHome {
        for item in items {
            symlink(source.join(item), self.dir.join(item)).unwrap();
        }
        self
    }
}

/// A temporary directory to keep homes in, for tests without the sandbox: its real path, an
/// environment whose `HOME` is `<root>/home` (holding `.claude`, the native login's home), and
/// where `config.toml` would be (`<root>/remuda/config.toml`, not created).
pub struct Root {
    _dir: tempfile::TempDir,
    pub root: PathBuf,
    pub env: BTreeMap<String, String>,
    pub config: PathBuf,
}

impl Root {
    pub fn new() -> Root {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        fs::create_dir_all(home.join(".claude")).unwrap();
        let env = [("HOME".to_string(), home.display().to_string())].into();
        let config = root.join("remuda/config.toml");
        Root {
            _dir: dir,
            root,
            env,
            config,
        }
    }

    /// The native login's home: `$HOME/.claude`.
    pub fn native(&self) -> PathBuf {
        self.root.join("home/.claude")
    }

    /// A home `<root>/<name>` to fill, created.
    pub fn home(&self, name: &str) -> ClaudeHome {
        ClaudeHome::at(self.root.join(name))
    }
}
