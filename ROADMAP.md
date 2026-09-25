# remuda roadmap

**Status:** v0.1.0. Milestones M0–M3 are complete; M2.5 is implemented and awaits dogfooding
before release.

remuda is a multi-account and session manager for coding agents (Claude Code and Codex). Its
behavior contract is [SPEC.md](SPEC.md); this document records the design principles, decisions,
and planned work.

## Scope and principles

- **Driven by actual workflows.** Features exist because a recurring workflow needs them: check
  usage across accounts, pick an account, and start or resume a session in a directory.
- **No takeover of the bare `claude` command, and no shell wrapper.** Typing `claude` directly
  always uses the native login; named accounts are used only through remuda (the TUI or
  `remuda run`).
- **Accounts are registered home paths.** The Claude Code Keychain entry is bound to the exact
  home path string (SPEC R2), so an existing home can only be registered in place, never moved into
  a conventional location.
- **Prefer the agent CLI's machine-readable output over private files.** `claude agents --json`,
  `claude auth status --json`, `claude -p /usage`, `codex login status`, and `codex app-server` are
  CLI interfaces and are more stable than undocumented file formats.
- **New sessions get a pre-assigned `--session-id`.** The session ID is known before launch, which
  makes attribution exact and lets `remuda run` exec directly (SPEC R6, R9).

## Design decisions

| Date | Decision | Rationale |
| --- | --- | --- |
| 2026-09-23 | Rust + ratatui | Single binary; fast JSONL scanning; the Codex TUI is also built on ratatui |
| 2026-09-23 | Do not take over the bare `claude` command; no shell wrapper | Typing `claude` directly means the native login; named accounts go through remuda only |
| 2026-09-23 | An account is a registered home path, not a conventional directory | The Keychain entry is bound to the home path string (SPEC R2), so existing homes can only be registered in place |
| 2026-09-23 | Prefer the claude CLI's official output over parsing internal files | `agents --json`, `auth status --json`, and `-p /usage` are CLI interfaces, more stable than undocumented file formats |
| 2026-09-23 | Pre-assign new session IDs with `--session-id` | The ID is known before launch, so attribution is exact and `run` can exec directly (R6, R9) |
| 2026-09-24 | Sessions are not shared between accounts; configuration is | Exact attribution, no cross-account cleanup, work and personal sessions stay apart; switching accounts mid-session is covered by relay (R19) |
| 2026-09-24 | Share configuration by launch-time injection, not symlinks | No writes into homes (R13), no drift, new accounts share automatically; plugin packaging rejected because it namespaces agent and skill names (R18) |
| 2026-09-24 | Token statistics from transcripts, deduplicated by message id / cumulative total, own cache | The agents record exact usage per request; `message.id` and codex's cumulative total identify a request across repeated records, forks and relay copies. A row per request in `state/stats.json` keeps deduplication exact and needs no time zone; it stays out of `index.json`, which reads only head and tail windows (R20) |
| 2026-09-24 | Private mode as a redacted snapshot of the TUI state | The screen is drawn from a copy of the state with names aliased and personal fields masked; every field is destructured, so a new one cannot reach the screen before it is decided how it is shown (R21) |
| 2026-09-24 | Codex live usage and identity through `codex app-server`, only on explicit live queries; `list` keeps `codex login status` | `account/rateLimits/read` and `account/read` are machine-readable and codex reads its own credentials, so `auth.json` stays unread; but starting app-server is like launching Codex (it may refresh a token, uses the network, writes state into the home, takes about 1.5 s), so it runs only when live usage is asked for (R4, R10, R10a) |
| 2026-09-24 | Cost as an estimate at API list prices: a built-in table with config.toml overrides, exact in picodollars | Most accounts are subscriptions, so list prices are the only comparable figure; a built-in table keeps remuda free of network requests and overrides cover new models and price changes; integer picodollars keep each request's cost exact, so sections add up to overall (R20) |

## Technology

- `ratatui` + `crossterm`: TUI
- `clap`: subcommands
- `serde` / `serde_json`: line-by-line JSONL parsing, skipping malformed lines
- `toml_edit`: preserves comments when writing `config.toml` (R3)
- `nucleo`: fuzzy search
- The index and statistics caches are single files under `state/`; move to SQLite only if data
  volume requires it
- A single crate (lib + bin), organized into modules:
  `registry`, `provider/{claude,codex}`, `usage`, `live`, `index`, `launch`, `tui`, `cli`

## TUI

- **Accounts**: each account's identity, usage (the reset times of all accounts on one shared
  timeline), and checks (R11).
- **Live**: running sessions and the accounts they belong to.
- **History**: all sessions in chronological order, fuzzy-searchable by title / cwd / account, with
  attribution.
- **Preview**: the last few messages of the selected session.
- **Stats**: tokens per account and model for a period, with an estimated cost and a chart,
  computed in the background the first time the view opens (R20).
- **Private mode**: `Ctrl-P` hides names, emails, paths and session content for screenshots (R21).
- Actions: start a new session in a directory with the selected account; resume the selected
  session (optionally under a different account); set up a new account; remove an account from the
  registry.

## Milestones

**M0 · CLI core** — **Done**
Registry, `add`, `setup` (`claude auth login`), `run` (pre-assigned `--session-id`, launch log,
exec), `list` (`auth status --json`), `usage [--live]`, and the sealed test harness. `remove`
(SPEC R14a; `D` in the TUI's Accounts view) was added later.

**M1 · Read-only TUI** — **Done**
Accounts, Live (`claude agents --json`), History (deduplication, titles, incremental cache,
attribution), and Preview.

**M2 · TUI actions** — **Done**
Start or resume sessions from the TUI (suspend the TUI, run in the foreground, refresh on return;
optional naming with `-n`); the account picker for `run` without an account; setup; attach / logs /
stop for background sessions (via `claude attach|logs|stop`).

**M3 · Codex provider** — **Done**
Codex accounts (`CODEX_HOME`), identity (`codex login status`; email and plan from a live query),
usage (cached from the rate limits in rollouts, live through `codex app-server`), the rollout
session index, and launch / resume / fork (SPEC R4, R10, R10a, R17).

**M2.5 · Shared configuration and relay** — **Implemented; dogfooding pending**
Sessions stay with the account that created them; configuration is shared by injection at launch
(SPEC R18): instructions (`CLAUDE.md`, skills, commands, agents) through `--add-dir`, settings and
the auto-memory location through one `--settings`, and enabled plugins through `--plugin-dir`.
Nothing is written into any home, and existing symlink layouts are detected so nothing loads
twice. A relay (SPEC R19) continues a session under another account by copying its transcript and
checkpoints into the target store and forking it there.
It is released as 0.2.0 after it has run on a real multi-account setup. Homes that symlink every
component, `projects` included, into the source home get nothing injected and refuse every relay,
so such a setup first moves `projects` to per-account stores.

## Later, as needed

- Launch background sessions from the TUI (`claude --bg`) and manage them alongside foreground
  sessions.
- Rename sessions: `claude -p --resume <id> "/rename <new-name>"` (needs verification).
- Cleanup: only via a `claude project purge --dry-run` preview followed by confirmed execution;
  remuda never deletes files itself. Note that with a shared `projects` directory, cleanup affects
  every account.
- Interact with running sessions through `messagingSocketPath`.
- Relocate homes using `CLAUDE_SECURESTORAGE_CONFIG_DIR` (R2).
- `--private` for command-line output (R21 covers the TUI only).

## Known issues

- A launch request whose running check is already in progress still launches after the check
  passes, even if its account was removed in the meantime by another process (`remuda remove`
  elsewhere; `D` in the TUI cancels a pending launch first). `pending` holds an `Account`. This
  affects only a very narrow timing window.
- The Configuration pane (R22) confines only the paths a plugin manifest names to the plugin
  directory. The plugin's own `hooks/hooks.json`, `.mcp.json`, and the files under `agents/`,
  `skills/`, and `commands/` are still opened when they are symlinks pointing outside it, which
  can reveal at most top-level key names or frontmatter. Low risk, since a plugin can already run
  hooks as the user; a follow-up is to confine them the same way.
- A member that shares some but not all of `CLAUDE.md`, `skills`, `commands`, `agents` with the
  source through symlinks gets all four through `--add-dir`, so the ones it already shares load
  twice (R11 warns). A follow-up is to link, per member, only the items it does not share (one
  link set per combination under `$REMUDA_HOME/shared/claude/`).

## Open questions

1. Whether session variables inherited when a child claude is launched from inside a claude session
   need to be stripped (R6).
2. Whether `claude --resume <id>` looks up sessions only under the current project directory (R6).
3. Stability of the experimental `codex app-server` methods remuda uses (`account/rateLimits/read`,
   `account/read`; R4, R10).

Resolved:

- Stale cached usage is addressed by on-demand live queries via
  `claude -p /usage --no-session-persistence` (R10).
- Codex `default` home semantics and keyring isolation: `CODEX_HOME=~/.codex` equals leaving it
  unset, and the keyring key hashes the normalized path (R4, verified on 0.155.1).
