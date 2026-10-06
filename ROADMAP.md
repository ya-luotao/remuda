# remuda roadmap

**Status:** v0.2.0. Milestones M0–M3 and M2.5 are complete; M2.5 went out with 0.2.0.

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
| 2026-09-24 | Sessions are not shared between accounts; configuration is | Exact attribution, no cross-account cleanup, work and personal sessions stay apart; switching accounts mid-session was to be covered by relay (R19; removed 2026-10-01, see below) |
| 2026-09-24 | Share configuration by launch-time injection, not symlinks | No writes into homes (R13), no drift, new accounts share automatically; plugin packaging rejected because it namespaces agent and skill names (R18) |
| 2026-09-24 | Token statistics from transcripts, deduplicated by message id / cumulative total, own cache | The agents record exact usage per request; `message.id` and codex's cumulative total identify a request across repeated records, forks and copies in other stores. A row per request in `state/stats.json` keeps deduplication exact and needs no time zone; it stays out of `index.json`, which reads only head and tail windows (R20) |
| 2026-09-24 | Private mode as a redacted snapshot of the TUI state | The screen is drawn from a copy of the state with names aliased and personal fields masked; every field is destructured, so a new one cannot reach the screen before it is decided how it is shown (R21) |
| 2026-09-24 | Codex live usage and identity through `codex app-server`, only on explicit live queries; `list` keeps `codex login status` | `account/rateLimits/read` and `account/read` are machine-readable and codex reads its own credentials, so `auth.json` stays unread; but starting app-server is like launching Codex (it may refresh a token, uses the network, writes state into the home, takes about 1.5 s), so it runs only when live usage is asked for (R4, R10, R10a) |
| 2026-09-24 | Cost as an estimate at API list prices: a built-in table with config.toml overrides, exact in picodollars | Most accounts are subscriptions, so list prices are the only comparable figure; a built-in table keeps cost estimates free of network requests and overrides cover new models and price changes; integer picodollars keep each request's cost exact, so sections add up to overall (R20) |
| 2026-09-27 | `remuda pick`: rules decide what is feasible, Jev chooses among it, taken only when confident | Hard limits are facts remuda can check (headroom on every window that applies, exclusions, logins), so they are never left to a model; what a model adds is weighing the user's free-text notes against headroom, resets and staleness. Jev answers a typed Choice with calibrated probabilities, so its pair is taken from 0.50 confidence, its account from 0.70, and the rules decide otherwise or on any error (R23) |
| 2026-09-27 | The first network request of remuda's own: only `pick`, only with `TYPESAFE_API_KEY` and notes; through `curl`, the key on its stdin | Without a key or notes nothing is sent and the rules decide; the state is aliased usage and the notes as written, never credentials, emails, organizations, paths or session content, and `--print-request` shows it. `curl` keeps an HTTP and TLS stack out of the binary; the key goes in the configuration curl reads on stdin, so it is never in the process list (R13, R23) |
| 2026-10-01 | No relay: a session is continued only by an account whose store holds it | A relay copied a transcript and its checkpoints into another account's home, the one case in which remuda wrote into a home; without it the write boundary has no exception (R13). The cost is accepted: a session cannot be moved to another account. This replaces the relay named in the 2026-09-24 decision on sessions; it was removed before any release contained it (R19) |
| 2026-10-01 | Memory is what accounts share: rules as copies, agent memory through `CLAUDE_CODE_REMOTE_MEMORY_DIR` | With sessions kept per account, what must not diverge is what claude remembers and is told. claude loads rules of an added directory only from regular files, so they are copied under `$REMUDA_HOME/shared` at launch; rules limited to `paths` do not apply that way and are reported. User-scope agent memory has no setting, only an undocumented variable, set only together with the injected auto-memory location because it moves that too. Both stay inside the write boundary (R13, R18) |
| 2026-10-03 | Accounts share one session store and configuration through symlinks; only login state stays per account | Any account can then resume any session, and configuration cannot diverge between accounts. This replaces the 2026-09-24 decision that sessions are not shared, and the premise "with sessions kept per account" of the 2026-10-01 decision on memory; injection (2026-09-24) stays as the fallback for a home without the links and is not extended. Relay stays removed: with one store there is nothing to copy. "Only login state" cannot be one directory: `CLAUDE_SECURESTORAGE_CONFIG_DIR` separates the credentials but not `.claude.json`, so each account keeps its own `.claude.json`, `history.jsonl`, `sessions`, and the files its organization sets; and no variable moves `projects`, so it can only be linked. `setup` makes the links in the home it has just created, once, before the login, the one exception to the write boundary; a home registered with `add` is never touched, only checked (R11, R12, R13, R18) |
| 2026-10-03 | A linked `settings.json` is shared whole, authentication settings included; remuda warns instead of enforcing | The credentials of a login are in the Keychain (or the home's `.credentials.json`), not in `settings.json`; authentication settings there are ones the user wrote. So `setup` does not link a `settings.json` that has them when it creates a home, and later ones are reported in the Accounts view and at every launch of an account that reads them through the link, by name. The alternatives raised in review, no raw link to the source's `settings.json` but filtered injection or a sanitized copy, were not taken: the linked layout stays as it is. remuda no longer says that authentication is never shared, only that it is never injected (R11, R18) |
| 2026-10-04 | A linked `projects` gets `CLAUDE_CODE_REMOTE_MEMORY_DIR=<source home>` at launch; the R11 check for an `agent-memory` link next to it fires only where a settings file chooses `autoMemoryDirectory` | claude grants its auto-memory directory by the literal path and then checks the resolved one, which through the link is under the source's `.claude/` and outside the working directories: every memory write asked for permission, in every mode, past any allow rule or `additionalDirectories` (2.1.288). The variable names the same directory by the source's path. It moves agent memory too, which a shared store wants anyway; the user's own `--settings`, variable or `autoMemoryDirectory` still win (R18) |

## Technology

- `ratatui` + `crossterm`: TUI
- `clap`: subcommands
- `serde` / `serde_json`: line-by-line JSONL parsing, skipping malformed lines
- `toml_edit`: preserves comments when writing `config.toml` (R3)
- `nucleo-matcher`: fuzzy search
- `jiff`: timestamps, the system time zone, statistics periods
- `unicode-normalization`: NFC for home paths and project names (R2, R18); `unicode-width`:
  display columns; `sha2`: settings file names (R18); `uuid`: pre-assigned session IDs (R6);
  `libc`: terminal modes and process groups
- `curl` (the system's, found on PATH): the one request of `remuda pick` (R23); no HTTP client
  is linked in
- The index and statistics caches are single files under `state/`; move to SQLite only if data
  volume requires it
- A single crate (lib + bin); the modules and how data flows between them are described in
  [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)

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

**M2.5 · Shared session store and configuration** — **Done**
Accounts share one session store and their configuration through symlinks in each member's
home, pointing at the source's (SPEC R18): `remuda setup` makes them in the home it creates,
and the Accounts view reports what a home registered with `add` does not link, or links that it
must not (R11). Only login state stays per account.
Injection at launch is the fallback for a home without the links: instructions (`CLAUDE.md`,
skills, commands, agents, rules) through `--add-dir`, settings and the auto-memory location
through one `--settings`, enabled plugins through `--plugin-dir`, and the memory of user-scope
subagents through an environment variable. It writes nothing into any home, skips each
component a home already links, and cannot share sessions.
The linked layout is the one in daily use. Injection has run only in the tests, not on a real
multi-account setup: the author's homes link everything, so nothing is injected there. Released
in 0.2.0.

## Later, as needed

- Launch background sessions from the TUI (`claude --bg`) and manage them alongside foreground
  sessions.
- Rename sessions: `claude -p --resume <id> "/rename <new-name>"` (needs verification).
- Cleanup: only via a `claude project purge --dry-run` preview followed by confirmed execution;
  remuda never deletes files itself. With the shared `projects` directory, cleanup affects every
  account.
- Interact with running sessions through `messagingSocketPath`.
- Relocate homes using `CLAUDE_SECURESTORAGE_CONFIG_DIR` (R2).
- `--private` for command-line output (R21 covers the TUI only).
- Task text in `remuda pick` (the state has a `task` slot for it), so the recommendation can
  weigh what the session is for.
- A TUI key for `pick`: the recommendation in the Accounts view, and a launch from it.
- Share user-scope MCP servers (`mcpServers` in the source's `.claude.json`) through
  `--mcp-config`, with R18's withholding rules applied to their `env` and `headers`. Not done
  until there is a server to verify it against. Project trust in `.claude.json` cannot be shared:
  `.claude.json` stays per account (R18).
- Share `plugins` through `CLAUDE_CODE_PLUGIN_CACHE_DIR` instead of a link, so that an install
  from a member's home is not recorded through that home's `plugins` link (R11). Not done until
  plugin loading and the `plugins/synced` directories are verified with it.

## Known issues

- The Configuration pane (R22) confines only the paths a plugin manifest names to the plugin
  directory. The plugin's own `hooks/hooks.json`, `.mcp.json`, and the files under `agents/`,
  `skills/`, and `commands/` are still opened when they are symlinks pointing outside it, which
  can reveal at most top-level key names or frontmatter. Low risk, since a plugin can already run
  hooks as the user; a follow-up is to confine them the same way.
- A member that shares some but not all of `CLAUDE.md`, `skills`, `commands`, `agents` with the
  source through symlinks gets all four through `--add-dir`, so the ones it already shares load
  twice (R11 warns, and says to link the others too). Injection is not extended to cover this.
- A linked `settings.json` is shared whole: authentication settings added to the source's
  after `setup` reach every account that links it. remuda warns (R11, and at each launch) but
  does not prevent it, by decision (2026-10-03).
- `remuda setup` links only what the source has at that moment. An item the source gets later
  (`rules`, `agent-memory`, `file-history`) is not linked into existing homes: R11 reports it,
  and the user makes the link.

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
