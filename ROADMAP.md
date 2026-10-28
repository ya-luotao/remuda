# remuda roadmap

**Status:** v0.4.0 is released: a product reset that removed what daily use did not need and
rewrote the documentation around what is left (M4).

Remuda lets several Claude Code and Codex logins be used as one: it shows what each account has
left, launches or resumes a session as any account, and keeps one setup shared by every Claude
account; token statistics come on top. Its behavior contract is [SPEC.md](SPEC.md), and the
decisions behind its design, current and superseded, are in [docs/DECISIONS.md](docs/DECISIONS.md).

## Scope and principles

- **Driven by actual workflows.** A feature exists because a recurring workflow needs it: see
  what each account has left, choose one, start or resume a session in a directory. A feature
  that daily use does not reach is removed, not kept "in case".
- **One way to do each thing.** One place answers "which account now" (the Accounts view, and
  `remuda usage` for the shell); one mechanism shares configuration (links in the homes).
- **Remuda installs nothing into the shell.** Typing `claude` means the native login unless the
  user aliases it; named accounts go through `remuda run` or the TUI.
- **Accounts are registered home paths.** The Claude Code Keychain entry is bound to the exact
  home path string (SPEC R2), so an existing home can only be registered in place.
- **Prefer the agents' machine-readable output over their files.** `claude agents --json`,
  `claude auth status --json`, `claude -p /usage`, `codex login status` and `codex app-server`
  are CLI interfaces, more stable than undocumented file formats.
- **Never touch a credential, and make no network requests of its own.** Identity and usage come
  from the agents; Remuda writes only below `$REMUDA_HOME`, apart from the links `setup` makes
  in a home it has just created (SPEC R13).

## Technology

- `ratatui` + `crossterm`: TUI
- `clap`: subcommands
- `serde` / `serde_json`: line-by-line JSONL parsing, skipping malformed lines
- `toml_edit`: preserves comments when writing `config.toml` (R3)
- `nucleo-matcher`: fuzzy search
- `jiff`: timestamps, the system time zone, statistics periods
- `unicode-normalization`: NFC for home paths and project names (R2, R22); `unicode-width`:
  display columns; `uuid`: pre-assigned session IDs (R6); `libc`: terminal modes and process
  groups
- No HTTP transport and no API key: live usage goes through the agents
- The index and statistics caches are single files under `state/`; move to SQLite only if data
  volume requires it
- A single crate (lib + bin); the modules and how data flows between them are described in
  [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)

## TUI

- **Accounts**: each account's login, usage (the reset times of all accounts on one timeline)
  and checks, warnings apart from notes (R10, R11); `p` opens the account card, with what its
  sessions load (R22).
- **Live**: running Claude sessions and the accounts they belong to; attach, logs and stop for
  background ones (R7).
- **History**: every account's sessions, newest first, fuzzy-searchable by title, directory and
  account; resume or fork as an account that can; `p` opens the session card (R8, R9, R16).
- **Stats**: tokens and estimated cost per account or project and model, with a chart (R20).
- **Private mode**: `Ctrl-P` hides names, emails, paths and session content for screenshots
  (R21).

## Milestones

**M0 · CLI core** — **Done**
Registry, `add`, `setup` (`claude auth login`), `run` (pre-assigned `--session-id`, launch log,
exec), `list`, `usage [--live]`, and the sealed test harness. `remove` (SPEC R14a) was added
later.

**M1 · Read-only TUI** — **Done**
Accounts, Live (`claude agents --json`), History (deduplication, titles, incremental cache,
attribution), and the preview.

**M2 · TUI actions** — **Done**
Start or resume sessions from the TUI (suspend the TUI, run in the foreground, refresh on return);
the account picker of `run` without an account; setup; attach, logs and stop for background
sessions.

**M3 · Codex provider** — **Done**
Codex accounts (`CODEX_HOME`), identity, usage (cached from the rollouts, live through
`codex app-server`), the rollout session index, and launch, resume and fork (SPEC R4, R10, R10a,
R17).

**M2.5 · Shared session store and configuration** — **Done** (0.2.0)
Accounts share one session store and their configuration through symlinks in each member's home,
pointing at the source's (SPEC R18): `remuda setup` makes them in the home it creates, and the
Accounts view reports what a home registered with `add` does not link (R11). Only login state
stays per account.

**M4 · Product reset** — **Done** (0.4.0)
Removed: `remuda pick` with `usage --wait` and `[pick]`, the launch-time injection of shared
configuration, the usage history with `usage --history`, and `stats --csv`. The pace of a window
as a note of `remuda usage`; the five-hour window labeled `5h`, the `SOURCE` column renamed
`AS OF`, and checks in two levels. The README and the guide rewritten around the product's three
jobs, and the design decisions moved to docs/DECISIONS.md.

## Later, as needed

- Launch background sessions from the TUI (`claude --bg`) and manage them alongside foreground
  sessions.
- Rename sessions: `claude -p --resume <id> "/rename <new-name>"` (needs verification).
- Cleanup: only through a `claude project purge --dry-run` preview followed by confirmed
  execution; Remuda never deletes files itself. With a shared `projects` directory, cleanup
  affects every account.
- Interact with running sessions through `messagingSocketPath`.
- Relocate homes using `CLAUDE_SECURESTORAGE_CONFIG_DIR` (R2).
- `--private` for command-line output (R21 covers the TUI only).
- Share `plugins` through `CLAUDE_CODE_PLUGIN_CACHE_DIR` instead of a link, so that an install
  from a member's home is not recorded through that home's `plugins` link (R11). Not done until
  plugin loading and the `plugins/synced` directories are verified with it.

## Known issues

- User-scope MCP servers and project trust are not shared: they live in each account's
  `.claude.json`, which is never linked (R18).
- The account card (R22) confines only the paths a plugin manifest names to the plugin
  directory. The plugin's own `hooks/hooks.json`, `.mcp.json`, and the files under `agents/`,
  `skills/` and `commands/` are still opened when they are symlinks pointing outside it, which
  can reveal at most top-level key names or frontmatter. Low risk, since a plugin can already
  run hooks as the user; a follow-up is to confine them the same way.
- A linked `settings.json` is shared whole: authentication settings added to the source's after
  `setup` reach every account that links it. Remuda warns (R11, and at each launch) but does not
  prevent it, by decision (2026-10-03).
- `remuda setup` links only what the source has at that moment. An item the source gets later
  (`rules`, `agent-memory`, `file-history`) is not linked into existing homes: R11 notes it, and
  the user makes the link.

## Open questions

1. Stability of the experimental `codex app-server` methods Remuda uses
   (`account/rateLimits/read`, `account/read`; R4, R10).

Resolved:

- Session variables inherited when Remuda is started from inside a claude session: only
  `CLAUDE_CODE_CHILD_SESSION` changes what the child does (an interactive session is not
  saved). Remuda passes it on and warns, before the launch and in the Accounts view (R6, R11).
- Whether `claude --resume <id>` finds a session only under the current project directory no
  longer matters: Remuda resumes a session in its last directory (`cwd_last`, R16).
- Stale cached usage is addressed by on-demand live queries via
  `claude -p /usage --no-session-persistence --setting-sources "" --strict-mcp-config` (R10),
  which run none of the account's hooks or MCP servers.
- Codex `default` home semantics and keyring isolation: `CODEX_HOME=~/.codex` equals leaving it
  unset, and the keyring key hashes the normalized path (R4, verified on 0.155.1).
