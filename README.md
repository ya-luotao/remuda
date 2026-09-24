<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/brand/lockup-reverse.svg">
  <source media="(prefers-color-scheme: light)" srcset="assets/brand/lockup.svg">
  <img src="assets/brand/lockup.svg" alt="Remuda" width="280">
</picture>

Remuda is a multi-account and session manager for coding agents. It keeps a registry of agent
accounts, each an isolated home directory for [Claude Code](https://github.com/anthropics/claude-code)
or [Codex](https://github.com/openai/codex), and gives you one terminal interface to compare their
usage limits, pick an account, and start, resume or fork sessions in any directory. Remuda does not
take over your shell: typing `claude` directly still uses the native login, and named accounts are
used only through `remuda`. It stores home paths exactly as registered, never touches credentials,
and makes no network requests of its own.

## Features

- **Account registry.** Register existing agent homes in place with `remuda add`, or create new
  ones with `remuda setup`, which runs the agent's own login. Each provider's native login is
  always available as the implicit `default` account.
- **Usage at a glance.** Five-hour, weekly and per-model weekly limits for every Claude account,
  read from the agent's local cache or queried live through `claude -p /usage`. The TUI draws
  every account's reset times on one shared seven-day timeline.
- **Launching with attribution.** `remuda run <account>` executes the agent with the account's
  home selected. New Claude sessions get a pre-assigned `--session-id`, and every launch is
  recorded in a launch log, so each session can be attributed to the account that started it.
- **Session history.** An incremental index over every account's transcripts, with deduplication
  of shared session stores, titles, working directories, fuzzy search and a message preview.
- **Live sessions.** Running interactive and background Claude sessions per account (via
  `claude agents --json`), with attach, logs, stop and remove for background sessions.
- **Shared configuration.** Name one Claude account as the source, and every other Claude account
  launches with its instructions (`CLAUDE.md`, skills, commands, agents), settings, enabled
  plugins and auto-memory, injected as launch options. Nothing is copied into the other homes,
  and each account's own settings still take precedence.
- **Relay.** Continue a Claude session under another account, for example when one account runs
  out of usage: the session is copied into that account's session store and forked there. The
  original is never modified.
- **Account checks.** Warnings for conditions that silently break multi-account setups: an
  `ANTHROPIC_API_KEY` that overrides every login, dangling symlinks, missing or logged-out homes,
  and a shared `projects` store without `cleanupPeriodDays`.
- **Codex support.** Codex accounts (`CODEX_HOME`) can be registered, set up, launched, indexed,
  resumed and forked. Usage limits and live sessions are not available for Codex, because Codex
  exposes no machine-readable source for them.

## Status

Remuda is at version 0.1.0. Its behavior is specified in [SPEC.md](SPEC.md) and covered by
tests, but the project is young: until 1.0, minor releases may contain breaking changes, which are
always listed in [CHANGELOG.md](CHANGELOG.md).

- **macOS** is the primary development platform.
- **Linux** is supported: the full test suite runs on Linux in CI. The account model is
  platform-neutral, since remuda only sets or unsets the agent's home variable
  (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`); how the agent stores credentials on each platform is left
  to the agent.
- **Windows** is not supported. Remuda relies on Unix process, terminal and file APIs.

Remuda drives the `claude` and `codex` command-line interfaces and reads some of their local
files. Where those formats are not public interfaces, parsing is best-effort: unrecognized data
degrades to missing fields rather than errors.

## Requirements

- A Unix-like system (macOS or Linux) with `ps` on `PATH`.
- Rust 1.88 or later to build from source.
- The [Claude Code](https://github.com/anthropics/claude-code) CLI, `claude`, on `PATH`.
- Optionally, the [Codex](https://github.com/openai/codex) CLI, `codex`, on `PATH` for Codex
  accounts.

## Installation

From the Git repository:

```sh
cargo install --git https://github.com/ya-luotao/remuda
```

From a local checkout:

```sh
git clone https://github.com/ya-luotao/remuda
cd remuda
cargo install --path .
```

## Quick start

Register an existing Claude home directory as the account `work`. The directory is recorded
exactly as given and is not modified:

```sh
remuda add work ~/.claude-work
```

Create a new account `personal` under `$REMUDA_HOME/homes/claude/personal` and log in to it:

```sh
remuda setup personal --email you@example.com
```

Check who each account is logged in as, and how much of each usage limit is left:

```sh
remuda list
remuda usage          # from each account's local cache, instantly
remuda usage --live   # asks claude now; takes a few seconds per account
```

Open the TUI to browse accounts, live sessions and history, and to start or resume sessions:

```sh
remuda
```

Launch Claude as an account directly. Everything after the account name is passed to `claude`
unchanged:

```sh
remuda run work
remuda run personal --model opus
remuda run          # choose the account in the TUI picker first
```

List recent sessions across all accounts:

```sh
remuda sessions --limit 10
```

Continue a session under another account (`c` in the TUI does the same for the selected
session):

```sh
remuda relay 766560c5-74e6-45f5-89fd-d92926b14898 personal
```

Codex accounts work the same way with `--provider codex`:

```sh
remuda add --provider codex research ~/.codex-research
remuda run codex:research
```

## Commands

Accounts are referenced as `name` or `provider:name`. A bare name that exists under more than one
provider is an error that lists the candidates. The bare name `default` always means
`claude:default`, the native Claude login; the native Codex login is `codex:default`.

| Command | Description |
| --- | --- |
| `remuda` | Open the TUI. Requires a terminal on standard input and output. |
| `remuda run [<account>] [args...]` | Launch the account's agent, replacing the `remuda` process. `args` are passed to the agent verbatim. Without an account, the TUI account picker opens first; that form takes no other arguments. |
| `remuda usage [<account>] [--live] [--timeout <SECONDS>]` | Print usage limits for every account, or for one. Without `--live`, reads the agent's local cache. With `--live`, runs `claude -p /usage` per account in parallel and exits 1 if any query fails. `--timeout` applies to each live query (default 90). |
| `remuda list [--timeout <SECONDS>]` | Print every account with its login identity (email, organization and plan; for Codex, the login method only) and home. `--timeout` applies to each identity query (default 15). |
| `remuda sessions [--limit <N>]` | Print the newest sessions: time, attributed accounts, title and working directory (default 30). |
| `remuda add [--provider <claude\|codex>] <name> <path>` | Register an existing home directory as an account. The provider defaults to `claude`. |
| `remuda setup [--provider <claude\|codex>] <name> [--email <EMAIL>]` | Create a new home under `$REMUDA_HOME/homes/<provider>/<name>`, register it, and run the agent's login (`claude auth login` or `codex login`). `--email` prefills the Claude login. |
| `remuda relay <session> <account>` | Continue a Claude session under another Claude account: copy its transcript and checkpoints into that account's session store and fork it there, in the session's last directory, replacing the `remuda` process. `<session>` is a full session ID from the index. |
| `remuda help [<command>]` | Show help for remuda or a command. |

Account names match `[A-Za-z0-9_-]+`. Because `run` forwards `-h` and `--help` to the agent, use
`remuda help run` for the help of `run` itself.

## TUI

The TUI has three views, Accounts, Live and History, with a preview pane for the selected session.
Press `?` in the TUI for the key reference.

| Key | Action |
| --- | --- |
| `1` `2` `3`, `Tab`, `Shift-Tab` | Switch view: Accounts, Live, History |
| `j` `k`, `↑` `↓` | Move the selection |
| `g` `G`, `Home` `End` | First / last row |
| `PgUp` `PgDn` | Page up / down |
| `Enter` | History: resume the selected session (Codex asks for confirmation first). Live: attach to a background session |
| `f` | Fork the selected session into a new session; the original is left unchanged |
| `c` | Continue the selected Claude session under another account: choose the account, and the session is copied into its store and forked there (relay) |
| `p`, `Space` | Expand or collapse the preview (Live and History) |
| `n` | Accounts: start a new session with the selected account |
| `s` | Accounts: set up a new Claude or Codex account, as `remuda setup` does |
| `l` | Live: show a background session's logs in the preview |
| `x` | Live: stop a background session (asks for confirmation) |
| `D` | Live: remove a stopped background session (asks for confirmation) |
| `/` | History: fuzzy search over title, working directory and accounts |
| `a` | History: also show teammate, SDK and Codex subagent sessions. Live: also show stopped background sessions |
| `u` | Query live usage for every Claude account |
| `r` | Refresh the index, identities, live sessions and checks |
| `Esc` | Go back: collapse the preview, close logs, clear the search, or cancel a form or pending launch check |
| `?` | Show the key reference; any key closes it |
| `q`, `Ctrl-C` | Quit |

In an expanded preview, the movement keys scroll the preview instead of the list. In forms, `Tab`
or `↓` moves to the next field, `Shift-Tab` or `↑` to the previous one, `Enter` submits and `Esc`
cancels. Confirmations accept `y`; any other key cancels. In the account picker opened by
`remuda run` without an account, `Enter` launches the selected account and `Esc` or `q` exits
without launching anything.

Resuming a Claude session that is still running elsewhere is refused, because two processes
writing the same session would overwrite each other. Codex has no source of running sessions, so
resuming a Codex session in place always asks for confirmation. Forks and relays only read the
original session, so they are allowed while it runs.

## Configuration

Remuda keeps its files under `$REMUDA_HOME`, which defaults to `~/.remuda`:

```text
$REMUDA_HOME/
├── config.toml                    account registry; the single source of truth
├── homes/<provider>/<name>/       homes created by `remuda setup`
├── shared/claude/.claude          symlink to the shared configuration's source home
└── state/
    ├── index.json                 session index cache
    └── launches.jsonl             launch log
```

`config.toml` lists the registered accounts. `remuda add` and `remuda setup` edit it for you,
preserving comments and unknown keys, and it can also be edited by hand:

```toml
[[account]]
provider = "claude"
name = "work"
home = "/Users/you/.claude-work"

[[account]]
provider = "claude"
name = "personal"
home = "/Users/you/.remuda/homes/claude/personal"

[[account]]
provider = "codex"
name = "research"
home = "/Users/you/.codex-research"
```

Homes must be absolute paths. The file is validated strictly on load: invalid or duplicate names,
a registered `default`, or a relative home are reported as errors naming the file. Everything under
`state/` is a cache and may be deleted at any time; it is rebuilt on the next run.

### Shared configuration

Sessions stay with the account that created them, but configuration can be shared. Name the
account whose configuration the others get, typically `default` (whose home is `~/.claude`):

```toml
[share.claude]
from = "default"

[[account]]
provider = "claude"
name = "personal"
home = "/Users/you/.remuda/homes/claude/personal"
share = false          # this account keeps only its own configuration
```

Every other Claude account then starts its sessions (new ones, resumes, forks, `-p` runs; not
subcommands, `--help` or `--version`) with the source's configuration, injected as launch options
before your own arguments:

- **Instructions:** `CLAUDE.md`, skills, commands and agents, through
  `--add-dir=$REMUDA_HOME/shared/claude`, whose `.claude` entry is a symlink to the source home.
- **Settings:** the part of the source's `settings.json` that the account's own settings do not
  define, in one `--settings` option. The account's own values win; identical hook entries are not
  added twice. If you pass `--settings` yourself, remuda injects no settings and says so.
- **Plugins:** `--plugin-dir` for each plugin the source enables and has installed.
- **Auto-memory:** the source's memory directory for the project, so every account remembers the
  same things about it.

Nothing is written into any account home. Homes that already share part of their configuration
through symlinks are detected, and that part is not injected again. `share` applies to Claude
accounts only, and `from` must name a Claude account; anything else is a load error. The Accounts
view warns about problems such as a missing source home or a plugin whose install is gone.

### Relay

`remuda relay <session> <account>`, or `c` in the TUI, continues a Claude session under an account
whose session store does not have it, for example when the session's own account has run out of
usage. Remuda copies the transcript (up to its last complete record) and its checkpoints into the
target account's store, then runs `claude --resume <id> --fork-session` there, in the session's
last directory and with the shared configuration. The fork is a new session that belongs to the
target account; the original is never modified, and the copy is hidden from the session history.
A relay never overwrites anything except its own earlier copy of the same session, and only if
that copy is unchanged.

## Safety guarantees

Remuda manages paths to directories that hold agent credentials, so its write boundary is part of
the specification ([SPEC.md](SPEC.md), R2 and R13):

- **Home paths are stored and passed byte-for-byte.** The agent may key its credentials to the
  exact path string (Claude Code on macOS names its Keychain entry after a hash of it), so remuda
  never rewrites, canonicalizes or adds or removes a trailing slash from a registered home.
- **Homes are never moved, renamed or deleted.** `remuda add` only records a name; it does not
  move, copy or create anything.
- **Writes are confined to `$REMUDA_HOME`:** `config.toml`, `state/`, `shared/`, and the empty
  directories created by `remuda setup`. The one exception is an explicit relay, which copies one
  transcript and its checkpoints into the target account's `projects/` and `file-history/`.
  Otherwise remuda never writes into any account home, and it never writes credentials,
  `.claude.json`, the Keychain, existing transcripts or `history.jsonl`.
- **Credentials are never read.** Identity and usage come from the agents' own commands
  (`claude auth status --json`, `codex login status`, `claude -p /usage`) and non-secret local
  metadata. Remuda does not read the Keychain, Codex `auth.json` or session `*.key` files.
- **No network requests of its own.** Live usage is queried by the agent itself, with the
  account's own login.

## Documentation

- [SPEC.md](SPEC.md): behavior specification; the contract that the tests enforce
- [ROADMAP.md](ROADMAP.md): milestones, planned work and open questions
- [CHANGELOG.md](CHANGELOG.md): release history
- [CONTRIBUTING.md](CONTRIBUTING.md): development workflow and release process
- [SECURITY.md](SECURITY.md): how to report a vulnerability
- [docs/VI.md](docs/VI.md): visual identity and logo assets

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.

### Brand assets

The logo and other files in [`assets/brand/`](assets/brand/) are described in
[docs/VI.md](docs/VI.md). The wordmark is set in Manrope, which is licensed under the SIL Open Font
License 1.1; its copyright and license notice is retained in
[`assets/brand/OFL-Manrope.txt`](assets/brand/OFL-Manrope.txt).
