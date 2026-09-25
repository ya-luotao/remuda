# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, a minor version
may contain breaking changes; they are listed under **Changed** or **Removed** with a note.

## [Unreleased]

### Added

- Shared configuration (SPEC R18): with `[share.claude] from = "<account>"`, every other Claude
  account launches with the source account's instructions (`CLAUDE.md`, skills, commands,
  agents), settings, enabled plugins, and auto-memory location, injected as launch options.
  Nothing is written into any home; an account's own settings keep precedence; existing symlink
  layouts are detected so nothing loads twice. Opt an account out with `share = false`.
  Authentication and provider settings are never shared, and shared settings travel to claude as
  a private (0600) file rather than on the command line.
- Account configuration in the TUI (SPEC R22): `p` or Space in the Accounts view shows the
  selected Claude account's instructions (`CLAUDE.md`, agents with model, effort and tools,
  skills, commands), plugins and what each adds, a settings summary (key names and counts, never
  values), auto-memory directory and MCP server names, each marked as the account's own, shared
  from the shared-configuration source, already the source's, or not shared, for a session in
  remuda's directory. It reads only, and uses the same plan as a launch's shared configuration.
- Relay (SPEC R19): `remuda relay <session> <account>` and `c` in the TUI continue a session under
  another account by copying its transcript and checkpoints into the target store and forking it
  there. The original session is never modified.
- `remuda remove <account>` unregisters an account (SPEC R14a). The home directory is left in
  place, and its path is printed so it can be registered again; `default` and the source of
  shared configuration cannot be removed. `D` in the TUI's Accounts view does the same after
  confirmation.
- Token statistics (SPEC R20): `remuda stats [<account>] [--period today|7d|30d|all]` and the
  TUI's Stats view (`4`; `t` cycles the period) show input, cache read, cache write, output and
  reasoning tokens per account and model, counted from Claude transcripts (subagents and advisor
  calls included) and Codex rollouts. Each request counts once across repeated records, forks,
  relay copies and shared stores; a session attributed to several accounts is counted once, for
  those accounts together. The counts are cached in `state/stats.json`; the first run reads every
  transcript whole.
- Estimated cost in the token statistics (SPEC R20): a COST column in `remuda stats` and the Stats
  view at public API list prices, built in as of 2026-09-24 (an estimate, not a bill), pricing
  5-minute and 1-hour cache writes, fast mode and US-only inference separately, and Codex's
  long-context requests at their own price; `[prices."<model>"]` in `config.toml` overrides or
  adds prices. The Stats view charts the period's cost over time and shows each account's share.
  The statistics cache moves to schema 2, so the first run afterwards reads every transcript
  again.
- Private mode in the TUI (SPEC R21): `Ctrl-P`, anywhere, hides account names (shown as aliases),
  emails, organizations, paths, session titles, previews, logs and typed text, for screenshots.
  Numbers, model names and session IDs stay visible.
- Codex usage limits (SPEC R10): `remuda usage` reads the newest rate limits Codex recorded in the
  account's rollouts; `--live` and `u` in the TUI query them through `codex app-server`. Five-hour
  and weekly windows show as Session and Week, per-model limits by name, on the same timeline as
  Claude's. The live query also shows a Codex account's email and plan.

### Fixed

- TUI: identity and usage results no longer land on another account's row when the account list
  changes while a query runs.

## [0.1.0] - 2026-09-24

Initial release.

### Added

- Account registry in `$REMUDA_HOME/config.toml` (default `~/.remuda`), with strict validation,
  atomic writes that preserve comments and unknown keys, and home paths stored and passed to the
  agent byte-for-byte.
- `remuda add` registers an existing home directory in place, refusing duplicate names, other
  spellings of an already registered directory, and the native login's own directory.
- `remuda setup` creates a new home under `$REMUDA_HOME/homes/<provider>/<name>`, registers it and
  runs the agent's login (`claude auth login`, optionally with `--email`, or `codex login`).
- `remuda run` executes the agent as an account, passing all further arguments through unchanged.
  New Claude sessions, and forks of a named session, get a pre-assigned `--session-id`, and every
  launch is recorded in `state/launches.jsonl` before the agent starts. Without an account,
  `remuda run` opens the TUI account picker.
- `remuda list` shows each account's login identity from `claude auth status --json` or
  `codex login status`, falling back to the identity cached in `.claude.json`.
- `remuda usage` shows five-hour, weekly and per-model limits from each account's local usage
  cache; `--live` queries them through `claude -p /usage` in parallel.
- `remuda sessions` lists recent sessions with time, attributed accounts, title and working
  directory, from an incremental session index cached in `state/index.json`.
- Session attribution from the launch log, live sessions and each home's `history.jsonl`.
- TUI with four parts:
  - **Accounts**: identity, usage with every account's reset times on a shared timeline, and
    checks for an overriding `ANTHROPIC_API_KEY`, dangling symlinks, missing or logged-out homes,
    and shared session stores without `cleanupPeriodDays`.
  - **Live**: running interactive and background Claude sessions per account.
  - **History**: all indexed sessions, newest first, with fuzzy search over title, working
    directory and accounts, and attribution.
  - **Preview**: the latest messages of the selected session, read on demand.
- TUI actions: start a new session with an account in a chosen directory, resume or fork a
  session (refusing to resume a Claude session that is still running), set up a new account, and
  attach to, show logs of, stop and remove background sessions.
- Codex provider: Codex accounts via `CODEX_HOME` for `add`, `setup`, `run`, `list` and
  `sessions`; a session index over Codex rollouts; resume and fork from the TUI, with confirmation
  before resuming in place.

[Unreleased]: https://github.com/ya-luotao/remuda/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/ya-luotao/remuda/releases/tag/v0.1.0
