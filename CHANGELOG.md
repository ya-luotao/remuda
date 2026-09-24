# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, a minor version
may contain breaking changes; they are listed under **Changed** or **Removed** with a note.

## [Unreleased]

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
