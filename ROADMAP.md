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
  `claude auth status --json`, and `claude -p /usage` are CLI interfaces and are more stable than
  undocumented file formats.
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

## Technology

- `ratatui` + `crossterm`: TUI
- `clap`: subcommands
- `serde` / `serde_json`: line-by-line JSONL parsing, skipping malformed lines
- `toml_edit`: preserves comments when writing `config.toml` (R3)
- `nucleo`: fuzzy search
- The index cache is a single file under `state/`; move to SQLite only if data volume requires it
- A single crate (lib + bin), organized into modules:
  `registry`, `provider/{claude,codex}`, `usage`, `live`, `index`, `launch`, `tui`, `cli`

## TUI

- **Accounts**: each account's identity, usage (the reset times of all accounts on one shared
  timeline), and checks (R11).
- **Live**: running sessions and the accounts they belong to.
- **History**: all sessions in chronological order, fuzzy-searchable by title / cwd / account, with
  attribution.
- **Preview**: the last few messages of the selected session.
- Actions: start a new session in a directory with the selected account; resume the selected
  session (optionally under a different account); set up a new account.

## Milestones

**M0 · CLI core** — **Done**
Registry, `add`, `setup` (`claude auth login`), `run` (pre-assigned `--session-id`, launch log,
exec), `list` (`auth status --json`), `usage [--live]`, and the sealed test harness.

**M1 · Read-only TUI** — **Done**
Accounts, Live (`claude agents --json`), History (deduplication, titles, incremental cache,
attribution), and Preview.

**M2 · TUI actions** — **Done**
Start or resume sessions from the TUI (suspend the TUI, run in the foreground, refresh on return;
optional naming with `-n`); the account picker for `run` without an account; setup; attach / logs /
stop for background sessions (via `claude attach|logs|stop`).

**M3 · Codex provider** — **Done**
Codex accounts (`CODEX_HOME`), the rollout session index, and launch / resume / fork (SPEC R4, R17).

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
- Show plugins in the accounts view: `claude plugin list --json`.
- Interact with running sessions through `messagingSocketPath`.
- Relocate homes using `CLAUDE_SECURESTORAGE_CONFIG_DIR` (R2).

## Known issues

Both affect only display or very narrow timing windows:

- `Identity` / `CachedUsage` / `LiveUsage` events are delivered by account index, so if the account
  registry changes while the TUI is running, data can be shown on the wrong row.
- A launch request whose running check is already in progress still launches after the check
  passes, even if its account was removed in the meantime (`pending` holds an `Account`).

## Open questions

1. Whether session variables inherited when a child claude is launched from inside a claude session
   need to be stripped (R6).
2. Whether `claude --resume <id>` looks up sessions only under the current project directory (R6).
3. Codex `default` home semantics and keyring isolation (R4).

Resolved: stale cached usage is addressed by on-demand live queries via
`claude -p /usage --no-session-persistence` (R10).
