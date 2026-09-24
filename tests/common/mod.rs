//! Hermetic test sandbox (SPEC R15).
//!
//! Every integration test runs remuda with a cleared environment, a fresh `HOME` and
//! `REMUDA_HOME`, and a fake `claude` first on `PATH`. The real `claude`, `$HOME`,
//! `~/.claude*` and the Keychain are never touched.

// Each test file compiles its own copy of this module; not every file uses every helper.
#![allow(dead_code)]

pub mod rollouts;
pub mod transcripts;

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use tempfile::TempDir;

/// Fake `claude`. Every invocation appends one NUL-separated record to `$FAKE_CLAUDE_OUT`
/// (default, for library-level tests that cannot set it: the sandbox's `claude-out`, with the
/// sandbox's home for fixtures instead of the inherited `HOME`; written
/// to a temp file first and appended with a single `cat`, so parallel invocations never
/// interleave).
///
/// Record layout (every field terminated by NUL):
/// `@@invocation`, `cwd=<physical cwd>`, `ccd=<unset|set:VALUE>` (CLAUDE_CONFIG_DIR),
/// `css=<unset|set:VALUE>` (CLAUDE_SECURESTORAGE_CONFIG_DIR),
/// `acm=<unset|set:VALUE>` (CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD, R18), then
/// `arg=<argv[i]>` per argument.
///
/// Fixtures live in `$CLAUDE_CONFIG_DIR/<name>`, or `$HOME/.<name>` when it is unset:
/// - `fake-sleep`: if present, `exec sleep <its contents>` (simulates a hang);
/// - `auth status --json` prints `fake-auth.json`, `-p /usage ...` prints `fake-usage.txt`;
///   a missing fixture exits 1;
/// - `agents --json` prints `fake-agents.json`; without it, falls through to the default below
///   (no output), so `remuda run <account> agents --json` still passes through cleanly. With
///   `--all` (anywhere after `--json`), `fake-agents-all.json` is preferred when it exists
///   (real claude lists stopped background sessions only with `--all`);
/// - `logs <id>` prints `fake-logs.txt` (a missing fixture exits 1, like an unknown session);
/// - `stop <id>` / `rm <id>` / `attach <id>` succeed silently, unless `fake-<verb>-error.txt`
///   exists: then it goes to stderr and they exit 1;
/// - if `fake-auth-after.json` exists, only the first `auth status` call of that account gets
///   `fake-auth.json`; later calls get `fake-auth-after.json` (simulates an identity change).
///
/// Anything else exits `${FAKE_CLAUDE_EXIT:-0}`.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
# Library-level tests run claude with the test process's environment, which has no
# FAKE_CLAUDE_OUT and the real HOME: they record next to the sandbox's bin directory, and the
# sandbox's home stands in for fixtures.
if [ -z "${FAKE_CLAUDE_OUT+x}" ]; then
  FAKE_CLAUDE_OUT="$(dirname "$0")/../claude-out"
  HOME="$(dirname "$0")/../home"
fi
if [ -n "${FAKE_CLAUDE_OUT+x}" ]; then
  tmp="$FAKE_CLAUDE_OUT.$$.tmp"
  {
    printf '@@invocation\0'
    printf 'cwd=%s\0' "$(pwd -P)"
    if [ -n "${CLAUDE_CONFIG_DIR+x}" ]; then
      printf 'ccd=set:%s\0' "$CLAUDE_CONFIG_DIR"
    else
      printf 'ccd=unset\0'
    fi
    if [ -n "${CLAUDE_SECURESTORAGE_CONFIG_DIR+x}" ]; then
      printf 'css=set:%s\0' "$CLAUDE_SECURESTORAGE_CONFIG_DIR"
    else
      printf 'css=unset\0'
    fi
    if [ -n "${CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD+x}" ]; then
      printf 'acm=set:%s\0' "$CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD"
    else
      printf 'acm=unset\0'
    fi
    for a in "$@"; do
      printf 'arg=%s\0' "$a"
    done
  } > "$tmp"
  cat "$tmp" >> "$FAKE_CLAUDE_OUT"
  rm -f "$tmp"
fi
if [ -n "${CLAUDE_CONFIG_DIR+x}" ]; then
  fixtures="$CLAUDE_CONFIG_DIR/"
else
  fixtures="$HOME/."
fi
if [ -f "${fixtures}fake-sleep" ]; then
  exec sleep "$(cat "${fixtures}fake-sleep")"
fi
serve() {
  if [ -f "$1" ]; then
    cat "$1"
    exit 0
  fi
  echo "fake claude: no fixture $1" >&2
  exit 1
}
if [ "$1" = auth ] && [ "$2" = status ] && [ "$3" = --json ]; then
  if [ -n "${FAKE_CLAUDE_OUT+x}" ] && [ -f "${fixtures}fake-auth-after.json" ]; then
    marker="$FAKE_CLAUDE_OUT.auth-served.$(printf '%s' "$fixtures" | cksum | cut -d ' ' -f 1)"
    if [ -f "$marker" ]; then
      serve "${fixtures}fake-auth-after.json"
    fi
    : > "$marker"
  fi
  serve "${fixtures}fake-auth.json"
fi
if [ "$1" = agents ] && [ "$2" = --json ]; then
  case " $* " in
    *" --all "*) [ -f "${fixtures}fake-agents-all.json" ] && serve "${fixtures}fake-agents-all.json" ;;
  esac
  [ -f "${fixtures}fake-agents.json" ] && serve "${fixtures}fake-agents.json"
fi
if [ "$1" = -p ] && [ "$2" = /usage ]; then
  serve "${fixtures}fake-usage.txt"
fi
if [ "$1" = logs ]; then
  serve "${fixtures}fake-logs.txt"
fi
case "$1" in
  stop|rm|attach)
    if [ -f "${fixtures}fake-$1-error.txt" ]; then
      cat "${fixtures}fake-$1-error.txt" >&2
      exit 1
    fi
    exit 0 ;;
esac
exit "${FAKE_CLAUDE_EXIT:-0}"
"#;

/// Fake `codex`, installed on `PATH` only by [`Sandbox::install_codex`] (its presence alone
/// lists `codex:default`, R17). Every invocation appends one NUL-separated record to
/// `$FAKE_CODEX_OUT` (default, for library-level tests: the sandbox's `codex-out`, with the
/// sandbox's home for fixtures): `@@invocation`, `cwd=<physical cwd>`,
/// `cxh=<unset|set:VALUE>` (CODEX_HOME), then `arg=<argv[i]>` per argument.
///
/// Fixtures live in `$CODEX_HOME/<name>`, or `$HOME/.codex/<name>` when it is unset:
/// - `fake-sleep`: if present, `exec sleep <its contents>`;
/// - `login status` prints `fake-login.txt` **to stderr** and exits 0, like codex 0.155.1
///   (`Logged in using ChatGPT`); without it, `Not logged in` to stderr and exit 1;
/// - `app-server` fails like a codex without it (`unrecognized subcommand`, exit 2) if
///   `fake-no-app-server` exists. Otherwise it speaks JSON-RPC over stdio like codex 0.155.1
///   (R4): every line it reads is appended to `$FAKE_CODEX_OUT.rpc` as `cxh<TAB>line` (`cxh` as
///   in the record); `initialize` is answered; `account/read` gets a notification, then
///   `fake-account-error.json` as its error if it exists, else `fake-account.json` as its
///   result (without either, a logged-out home's `{"account":null,"requiresOpenaiAuth":true}`);
///   `account/rateLimits/read` gets a
///   notification, then `fake-rate-limits.json` (without it, codex's -32600 authentication
///   error); any other request gets -32601. It exits 0 when stdin closes.
///
/// Anything else exits `${FAKE_CODEX_EXIT:-0}`.
const FAKE_CODEX: &str = r#"#!/bin/sh
if [ -z "${FAKE_CODEX_OUT+x}" ]; then
  FAKE_CODEX_OUT="$(dirname "$0")/../codex-out"
  HOME="$(dirname "$0")/../home"
fi
tmp="$FAKE_CODEX_OUT.$$.tmp"
{
  printf '@@invocation\0'
  printf 'cwd=%s\0' "$(pwd -P)"
  if [ -n "${CODEX_HOME+x}" ]; then
    printf 'cxh=set:%s\0' "$CODEX_HOME"
  else
    printf 'cxh=unset\0'
  fi
  for a in "$@"; do
    printf 'arg=%s\0' "$a"
  done
} > "$tmp"
cat "$tmp" >> "$FAKE_CODEX_OUT"
rm -f "$tmp"
if [ -n "${CODEX_HOME+x}" ]; then
  fixtures="$CODEX_HOME/"
else
  fixtures="$HOME/.codex/"
fi
if [ -f "${fixtures}fake-sleep" ]; then
  exec sleep "$(cat "${fixtures}fake-sleep")"
fi
if [ "$1" = login ] && [ "$2" = status ]; then
  if [ -f "${fixtures}fake-login.txt" ]; then
    cat "${fixtures}fake-login.txt" >&2
    exit 0
  fi
  echo "Not logged in" >&2
  exit 1
fi
if [ "$1" = app-server ]; then
  if [ -f "${fixtures}fake-no-app-server" ]; then
    echo "error: unrecognized subcommand 'app-server'" >&2; exit 2
  fi
  if [ -n "${CODEX_HOME+x}" ]; then cxh="set:$CODEX_HOME"; else cxh=unset; fi
  echo "fake codex app-server starting" >&2
  while IFS= read -r line; do
    printf '%s\t%s\n' "$cxh" "$line" >> "$FAKE_CODEX_OUT.rpc"
    id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
    case "$line" in
      *'"method":"initialize"'*) printf '{"id":%s,"result":{"userAgent":"fake"}}\n' "$id" ;;
      *'"method":"account/read"'*)
        printf '{"method":"remoteControl/status/changed","params":{"status":"disabled"}}\n'
        if [ -f "${fixtures}fake-account-error.json" ]; then
          printf '{"id":%s,"error":%s}\n' "$id" "$(tr -d '\n' < "${fixtures}fake-account-error.json")"
        elif [ -f "${fixtures}fake-account.json" ]; then
          printf '{"id":%s,"result":%s}\n' "$id" "$(tr -d '\n' < "${fixtures}fake-account.json")"
        else
          printf '{"id":%s,"result":{"account":null,"requiresOpenaiAuth":true}}\n' "$id"
        fi ;;
      *'"method":"account/rateLimits/read"'*)
        printf '{"method":"account/rateLimits/updated","params":{}}\n'
        if [ -f "${fixtures}fake-rate-limits.json" ]; then
          printf '{"id":%s,"result":%s}\n' "$id" "$(tr -d '\n' < "${fixtures}fake-rate-limits.json")"
        else
          printf '{"error":{"code":-32600,"message":"codex account authentication required to read rate limits"},"id":%s}\n' "$id"
        fi ;;
      *'"id":'*) printf '{"error":{"code":-32601,"message":"method not found"},"id":%s}\n' "$id" ;;
    esac
  done
  exit 0
fi
exit "${FAKE_CODEX_EXIT:-0}"
"#;

/// One JSON-RPC line the fake `codex app-server` read.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexRpc {
    /// `None` when `CODEX_HOME` was unset in codex's environment.
    pub codex_home: Option<String>,
    pub message: serde_json::Value,
}

/// One recorded run of the fake `codex`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexInvocation {
    pub cwd: PathBuf,
    /// `None` when `CODEX_HOME` was unset in codex's environment.
    pub codex_home: Option<String>,
    pub args: Vec<String>,
}

pub struct Sandbox {
    root: TempDir,
}

/// One recorded run of the fake `claude`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub cwd: PathBuf,
    /// `None` when `CLAUDE_CONFIG_DIR` was unset in claude's environment.
    pub config_dir: Option<String>,
    /// `None` when `CLAUDE_SECURESTORAGE_CONFIG_DIR` was unset.
    pub securestorage_dir: Option<String>,
    /// `CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD` (R18); `None` when unset.
    pub add_dir_claude_md: Option<String>,
    pub args: Vec<String>,
}

/// Writes an executable file through a `sh` child, so this process never holds a write
/// descriptor on it: on Linux, a child forked by another test thread inherits such a
/// descriptor until it execs, and running the file meanwhile fails with ETXTBSY.
pub fn write_executable(path: &Path, contents: &str) {
    use std::io::Write;
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("spawn sh to write an executable");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(contents.as_bytes())
        .expect("pipe the executable's contents");
    assert!(child.wait().unwrap().success(), "write {}", path.display());
}

impl Sandbox {
    pub fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("remuda-test-")
            .tempdir()
            .expect("create sandbox root");
        let sb = Sandbox { root };
        for dir in [sb.home(), sb.bin(), sb.work()] {
            fs::create_dir_all(&dir).expect("create sandbox dir");
        }
        // REMUDA_HOME is deliberately not created: remuda must cope with it missing.
        let claude = sb.bin().join("claude");
        write_executable(&claude, FAKE_CLAUDE);
        sb
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub fn home(&self) -> PathBuf {
        self.root().join("home")
    }

    pub fn remuda_home(&self) -> PathBuf {
        self.root().join("remuda")
    }

    pub fn bin(&self) -> PathBuf {
        self.root().join("bin")
    }

    /// Default working directory for remuda (and hence for claude).
    pub fn work(&self) -> PathBuf {
        self.root().join("work")
    }

    pub fn claude_out(&self) -> PathBuf {
        self.root().join("claude-out")
    }

    pub fn codex_out(&self) -> PathBuf {
        self.root().join("codex-out")
    }

    /// Puts the fake `codex` on `PATH` (next to the fake claude): `codex:default` is listed
    /// from now on (R17).
    pub fn install_codex(&self) {
        let codex = self.bin().join("codex");
        write_executable(&codex, FAKE_CODEX);
    }

    /// Creates a directory that looks like a Codex home (has a `config.toml`).
    pub fn make_codex_home(&self, rel: &str) -> PathBuf {
        let dir = self.root().join(rel);
        fs::create_dir_all(&dir).expect("create codex home");
        fs::write(dir.join("config.toml"), "").expect("write config.toml");
        dir
    }

    /// Where the fake codex looks for fixtures: the home, or `$HOME/.codex` for `None`
    /// (created).
    pub fn codex_fixture_dir(&self, home: Option<&Path>) -> PathBuf {
        let dir = match home {
            Some(dir) => dir.to_path_buf(),
            None => self.home().join(".codex"),
        };
        fs::create_dir_all(&dir).expect("create codex home");
        dir
    }

    /// `codex login status` output (stderr) for a codex home (`None`: `$HOME/.codex`).
    pub fn set_codex_login(&self, home: Option<&Path>, text: &str) {
        let dir = self.codex_fixture_dir(home);
        fs::write(dir.join("fake-login.txt"), text).expect("write login fixture");
    }

    /// The result of `codex app-server`'s `account/read` for a codex home.
    pub fn set_codex_account(&self, home: Option<&Path>, json: &str) {
        let dir = self.codex_fixture_dir(home);
        fs::write(dir.join("fake-account.json"), json).expect("write account fixture");
    }

    /// The error `codex app-server` answers `account/read` with for a codex home.
    pub fn set_codex_account_error(&self, home: Option<&Path>, json: &str) {
        let dir = self.codex_fixture_dir(home);
        fs::write(dir.join("fake-account-error.json"), json).expect("write account fixture");
    }

    /// The result of `codex app-server`'s `account/rateLimits/read` for a codex home.
    pub fn set_codex_rate_limits(&self, home: Option<&Path>, json: &str) {
        let dir = self.codex_fixture_dir(home);
        fs::write(dir.join("fake-rate-limits.json"), json).expect("write rate limits fixture");
    }

    /// Makes `codex app-server` fail for a codex home like a codex without it.
    pub fn set_codex_without_app_server(&self, home: Option<&Path>) {
        let dir = self.codex_fixture_dir(home);
        fs::write(dir.join("fake-no-app-server"), "").expect("write app-server fixture");
    }

    /// Makes every codex invocation for this home hang for `secs` seconds.
    pub fn set_codex_hang(&self, home: Option<&Path>, secs: u32) {
        let dir = self.codex_fixture_dir(home);
        fs::write(dir.join("fake-sleep"), secs.to_string()).expect("write sleep fixture");
    }

    /// Every JSON-RPC line the fake `codex app-server` read so far, in order (empty if it
    /// never ran).
    pub fn codex_rpc(&self) -> Vec<CodexRpc> {
        let path = self.root().join("codex-out.rpc");
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => panic!("read fake codex rpc record: {e}"),
        };
        text.lines()
            .map(|line| {
                let (cxh, message) = line.split_once('\t').expect("cxh<TAB>message");
                CodexRpc {
                    codex_home: parse_var(Some(cxh)),
                    message: serde_json::from_str(message).expect("rpc line is JSON"),
                }
            })
            .collect()
    }

    /// All invocations of the fake codex so far (empty if it never ran).
    pub fn codex_invocations(&self) -> Vec<CodexInvocation> {
        let bytes = match fs::read(self.codex_out()) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => panic!("read fake codex record: {e}"),
        };
        parse_records(&bytes, "cxh=")
            .into_iter()
            .map(|(cwd, codex_home, args)| CodexInvocation {
                cwd,
                codex_home,
                args,
            })
            .collect()
    }

    pub fn config_path(&self) -> PathBuf {
        self.remuda_home().join("config.toml")
    }

    pub fn launch_log(&self) -> PathBuf {
        self.remuda_home().join("state").join("launches.jsonl")
    }

    /// PATH with the fake claude first and only system dirs after it.
    pub fn path_var(&self) -> String {
        format!("{}:/usr/bin:/bin", self.bin().display())
    }

    /// A remuda command with a cleared environment: only `HOME`, `REMUDA_HOME`, `PATH`,
    /// `FAKE_CLAUDE_OUT`, `FAKE_CODEX_OUT` and `TZ=UTC` (deterministic times) are set. The cwd is the sandbox
    /// work dir.
    pub fn remuda(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_remuda"));
        cmd.env_clear()
            .env("HOME", self.home())
            .env("REMUDA_HOME", self.remuda_home())
            .env("PATH", self.path_var())
            .env("FAKE_CLAUDE_OUT", self.claude_out())
            .env("FAKE_CODEX_OUT", self.codex_out())
            .env("TZ", "UTC")
            .current_dir(self.work());
        cmd
    }

    /// Where the fake claude looks for fixtures: `<home>/<name>` for a named account,
    /// `$HOME/.<name>` for the default account (`home == None`).
    pub fn fixture_path(&self, home: Option<&Path>, name: &str) -> PathBuf {
        match home {
            Some(dir) => dir.join(name),
            None => self.home().join(format!(".{name}")),
        }
    }

    /// `claude auth status --json` output for an account.
    pub fn set_auth(&self, home: Option<&Path>, json: &str) {
        fs::write(self.fixture_path(home, "fake-auth.json"), json).expect("write auth fixture");
    }

    /// `claude -p /usage` output for an account.
    pub fn set_live_usage(&self, home: Option<&Path>, text: &str) {
        fs::write(self.fixture_path(home, "fake-usage.txt"), text).expect("write usage fixture");
    }

    /// `claude agents --json` output for an account.
    pub fn set_agents(&self, home: Option<&Path>, json: &str) {
        fs::write(self.fixture_path(home, "fake-agents.json"), json).expect("write agents fixture");
    }

    /// `claude agents --json --all` output for an account (preferred over `fake-agents.json`
    /// when `--all` is passed).
    pub fn set_agents_all(&self, home: Option<&Path>, json: &str) {
        fs::write(self.fixture_path(home, "fake-agents-all.json"), json)
            .expect("write agents --all fixture");
    }

    /// Makes every claude invocation for this account hang for `secs` seconds.
    pub fn set_hang(&self, home: Option<&Path>, secs: u32) {
        fs::write(self.fixture_path(home, "fake-sleep"), secs.to_string())
            .expect("write sleep fixture");
    }

    /// `.claude.json` of an account (`None`: the native `$HOME/.claude.json`).
    pub fn write_claude_json(&self, home: Option<&Path>, json: &str) {
        let path = match home {
            Some(dir) => dir.join(".claude.json"),
            None => self.home().join(".claude.json"),
        };
        fs::write(path, json).expect("write .claude.json");
    }

    /// Registers accounts by writing config.toml directly: `(name, home)` pairs.
    pub fn register(&self, accounts: &[(&str, &Path)]) {
        let mut text = String::new();
        for (name, home) in accounts {
            text.push_str(&format!(
                "[[account]]\nprovider = \"claude\"\nname = \"{name}\"\nhome = \"{}\"\n\n",
                home.display()
            ));
        }
        self.write_config(&text);
    }

    /// Creates a directory that looks like a Claude home (has a synthetic `.claude.json`).
    pub fn make_claude_home(&self, rel: &str) -> PathBuf {
        let dir = self.root().join(rel);
        fs::create_dir_all(&dir).expect("create claude home");
        fs::write(dir.join(".claude.json"), "{}\n").expect("write .claude.json");
        dir
    }

    pub fn write_config(&self, contents: &str) {
        fs::create_dir_all(self.remuda_home()).expect("create REMUDA_HOME");
        fs::write(self.config_path(), contents).expect("write config.toml");
    }

    pub fn read_config(&self) -> String {
        fs::read_to_string(self.config_path()).expect("read config.toml")
    }

    /// All invocations of the fake claude so far (empty if it never ran).
    pub fn invocations(&self) -> Vec<Invocation> {
        match fs::read(self.claude_out()) {
            Ok(bytes) => parse_invocations(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => panic!("read fake claude record: {e}"),
        }
    }

    /// The single invocation of the fake claude; panics if there were zero or several.
    pub fn only_invocation(&self) -> Invocation {
        let mut all = self.invocations();
        assert_eq!(
            all.len(),
            1,
            "expected exactly one claude invocation: {all:?}"
        );
        all.remove(0)
    }

    /// The invocations whose `CLAUDE_CONFIG_DIR` was `config_dir` (`None`: unset).
    pub fn invocations_with(&self, config_dir: Option<&Path>) -> Vec<Invocation> {
        let want = config_dir.map(|p| p.to_str().unwrap().to_string());
        self.invocations()
            .into_iter()
            .filter(|i| i.config_dir == want)
            .collect()
    }

    /// Parsed lines of `$REMUDA_HOME/state/launches.jsonl`.
    pub fn launches(&self) -> Vec<serde_json::Value> {
        let text = fs::read_to_string(self.launch_log()).expect("read launch log");
        text.lines()
            .map(|l| serde_json::from_str(l).expect("launch log line is JSON"))
            .collect()
    }
}

pub fn parse_invocations(bytes: &[u8]) -> Vec<Invocation> {
    let text = std::str::from_utf8(bytes).expect("fake claude record is UTF-8");
    let mut fields: Vec<&str> = text.split('\0').collect();
    assert_eq!(fields.pop(), Some(""), "record must end with NUL");

    let mut out = Vec::new();
    let mut iter = fields.into_iter().peekable();
    while let Some(marker) = iter.next() {
        assert_eq!(marker, "@@invocation", "unexpected record field");
        let cwd = iter
            .next()
            .and_then(|f| f.strip_prefix("cwd="))
            .expect("cwd field");
        let config_dir = parse_var(iter.next().and_then(|f| f.strip_prefix("ccd=")));
        let securestorage_dir = parse_var(iter.next().and_then(|f| f.strip_prefix("css=")));
        let add_dir_claude_md = parse_var(iter.next().and_then(|f| f.strip_prefix("acm=")));
        let mut args = Vec::new();
        while let Some(f) = iter.peek() {
            if *f == "@@invocation" {
                break;
            }
            let arg = f.strip_prefix("arg=").expect("arg field");
            args.push(arg.to_string());
            iter.next();
        }
        out.push(Invocation {
            cwd: PathBuf::from(cwd),
            config_dir,
            securestorage_dir,
            add_dir_claude_md,
            args,
        });
    }
    out
}

/// `(cwd, var, args)` per record of a fake whose one variable field starts with `var_prefix`.
fn parse_records(bytes: &[u8], var_prefix: &str) -> Vec<(PathBuf, Option<String>, Vec<String>)> {
    let text = std::str::from_utf8(bytes).expect("fake record is UTF-8");
    let mut fields: Vec<&str> = text.split('\0').collect();
    assert_eq!(fields.pop(), Some(""), "record must end with NUL");
    let mut out = Vec::new();
    let mut iter = fields.into_iter().peekable();
    while let Some(marker) = iter.next() {
        assert_eq!(marker, "@@invocation", "unexpected record field");
        let cwd = iter
            .next()
            .and_then(|f| f.strip_prefix("cwd="))
            .expect("cwd field");
        let var = parse_var(iter.next().and_then(|f| f.strip_prefix(var_prefix)));
        let mut args = Vec::new();
        while let Some(f) = iter.peek() {
            if *f == "@@invocation" {
                break;
            }
            args.push(f.strip_prefix("arg=").expect("arg field").to_string());
            iter.next();
        }
        out.push((PathBuf::from(cwd), var, args));
    }
    out
}

fn parse_var(field: Option<&str>) -> Option<String> {
    match field.expect("env var field") {
        "unset" => None,
        f => Some(f.strip_prefix("set:").expect("set:VALUE").to_string()),
    }
}

/// Parses column output whose header words mark where each column starts. Returns one map
/// per data row, keyed by header word; values are trimmed.
pub fn parse_table(text: &str) -> Vec<std::collections::BTreeMap<String, String>> {
    let mut lines = text.lines();
    let header: Vec<char> = lines.next().expect("table header").chars().collect();
    let mut cols: Vec<(String, usize)> = Vec::new();
    let mut i = 0;
    while i < header.len() {
        if header[i] != ' ' && (i == 0 || header[i - 1] == ' ') {
            let end = (i..header.len())
                .find(|&j| header[j] == ' ')
                .unwrap_or(header.len());
            cols.push((header[i..end].iter().collect(), i));
            i = end;
        } else {
            i += 1;
        }
    }
    lines
        .map(|line| {
            let chars: Vec<char> = line.chars().collect();
            cols.iter()
                .enumerate()
                .map(|(k, (name, start))| {
                    let end = cols
                        .get(k + 1)
                        .map_or(chars.len(), |c| c.1)
                        .min(chars.len());
                    let start = (*start).min(chars.len());
                    let value: String = chars[start..end].iter().collect();
                    (name.clone(), value.trim().to_string())
                })
                .collect()
        })
        .collect()
}
