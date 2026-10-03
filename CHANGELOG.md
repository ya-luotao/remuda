# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, a minor version
may contain breaking changes; they are listed under **Changed** or **Removed** with a note.

## [Unreleased]

### Fixed

- A member whose `projects` is a link to the source's (the layout `remuda setup` creates) had
  claude ask for permission on every auto-memory write: claude grants its memory directory by
  the literal path and then finds the resolved one under the source's `.claude/`, outside the
  working directories, which no allow rule or `additionalDirectories` entry gets past (2.1.288).
  Such a launch now sets `CLAUDE_CODE_REMOTE_MEMORY_DIR` to the source's home, so claude names
  the same directory by the source's path and writes without asking; the memory of user-scope
  subagents is the source's as well, linked or not, and the R11 check that asked for an
  `agent-memory` link next to a `projects` link is gone (SPEC R18, R11). Not set when the user
  passes `--settings`, sets the variable, or a settings file chooses `autoMemoryDirectory`.

## [0.2.0] - 2026-10-03

### Added

- Recommendations (SPEC R23): `remuda pick` recommends the account, model and effort to launch
  now. Rules keep only the pairs with at least `min_headroom` percent left on every usage window
  that applies (per-model windows included; a reset that has passed frees a window, stale data
  never does) and rank them; `[pick]` in config.toml sets exclusions, preferences, the models and
  effort levels to choose from, and notes. With `TYPESAFE_API_KEY` set and notes written, one
  request asks TypeSafe's Jev model to choose among the feasible options; its answer is taken
  only when confident, and the rules decide otherwise or on any error. The request carries
  aliased usage and the notes as written (accounts written as `provider:name` are aliased too,
  registered or not),
  never credentials, emails, organizations, paths or session content; `--print-request` shows it
  and `--offline` never sends. `--json` prints every candidate; `--run` launches the choice as
  `remuda run` does, with `--model`/`--effort` (claude) or `-m`/`-c model_reasoning_effort=`
  (codex) injected. This is remuda's first network request of its own, made only with a key.
  `remuda add` and `remuda setup` refuse an account that would leave `config.toml` invalid, such
  as one that makes a bare name in `[pick]` ambiguous; write accounts there as `provider:name`.

- A shared session store and configuration (SPEC R18): with `[share.claude] from =
  "<account>"`, `remuda setup` links a new Claude account's home to the source's before the
  login: `projects` and `file-history`, `settings.json`, `CLAUDE.md`, `skills`, `commands`,
  `agents`, `hooks`, `plugins`, `rules`, `agent-memory`, `output-styles` and `keybindings.json`,
  one symlink for each the source has. Any linked account can then resume any session, and the
  configuration cannot diverge. The login stays per account: `.claude.json`, `history.jsonl`,
  `sessions`, `remote-settings.json` and `policy-limits.json` are never linked. The links are
  made once, in the directory `setup` has just created and while it is empty; nothing is
  replaced or removed, a link that fails is reported without stopping the setup, and a home
  registered with `remuda add` is never touched. `s` in the TUI does the same. A source
  `settings.json` that sets authentication or provider settings (`apiKeyHelper`,
  `env.ANTHROPIC_API_KEY`, `forceLoginOrgUUID`, …), or that cannot be read, when the account is
  set up is not linked: that account gets the settings injected at launch, without them. A
  linked `settings.json` is shared whole, so such settings added to it later are read by every
  account that links it; remuda warns about it but does not prevent it. `setup` creates the
  home without following a symlink below `$REMUDA_HOME`, and refuses a `homes` or
  `homes/<provider>` that is one.
- Checks for the links of a member's home in the Accounts view (SPEC R11): `.claude.json`,
  `history.jsonl` or `sessions` that is a symlink to another account's; a home that does not
  share `projects` with the source (it does not see or resume the sessions there); a home that
  shares `projects` but not `file-history`; a `plugins` link that installed plugins are
  recorded through and that must therefore stay; a `settings.json` linked to the source's
  while the source's sets authentication settings, which that account then reads. The
  messages say which link to make or remove.
- A warning at every session launch of an account whose `settings.json` is linked to the
  source's while the source's sets authentication settings (SPEC R18), from `remuda run` and
  the TUI alike, naming the settings and never their values.
- Shared configuration by injection (SPEC R18), the fallback for a home that does not link an
  item: with `[share.claude] from = "<account>"`, every other Claude
  account launches with the source account's instructions (`CLAUDE.md`, skills, commands,
  agents, rules), settings, enabled plugins, and memory locations (auto-memory and the memory of
  user-scope subagents), injected as launch options. Sessions cannot be injected. Injection
  writes nothing into any home; an account's own settings keep precedence; what a home links
  is detected so nothing loads twice. Opt an account out with `share = false`.
  Authentication and provider settings are never injected, and shared settings travel to claude as
  a private (0600) file rather than on the command line. Rules are shared as read-only copies
  under `$REMUDA_HOME/shared`, refreshed at each launch; a rule limited to files by `paths` is
  not applied that way, and the Accounts view names such rules. The memory of user-scope
  subagents is redirected through `CLAUDE_CODE_REMOTE_MEMORY_DIR`, which claude does not
  document, and only together with the auto-memory location.
- Account configuration in the TUI (SPEC R22): `p` or Space in the Accounts view shows the selected
  Claude account's instructions (`CLAUDE.md`, agents with model, effort and tools, skills, commands,
  rules), plugins and what each adds, a settings summary (key names and counts, never values), the
  auto-memory and subagent memory directories and MCP server names, each marked as the account's
  own, shared from the shared-configuration source, already the source's, or not shared, for a
  session in remuda's directory. It reads only, and uses the same plan as a launch's shared
  configuration.
- `remuda remove <account>` unregisters an account (SPEC R14a). The home directory is left in
  place, and its path is printed so it can be registered again; `default` and the source of
  shared configuration cannot be removed. `D` in the TUI's Accounts view does the same after
  confirmation.
- Token statistics (SPEC R20): `remuda stats [<account>] [--period today|7d|30d|all]` and the
  TUI's Stats view (`4`; `t` cycles the period) show input, cache read, cache write, output and
  reasoning tokens per account and model, counted from Claude transcripts (subagents and advisor
  calls included) and Codex rollouts. Each request counts once across repeated records, forks
  and shared stores; a session attributed to several accounts is counted once, for
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
  Numbers, model names and session IDs stay visible. In a message that comes from elsewhere (an
  agent's output, a system error, a check message), the line is hidden from its first path to
  its end.
- Codex usage limits (SPEC R10): `remuda usage` reads the newest rate limits Codex recorded in the
  account's rollouts; `--live` and `u` in the TUI query them through `codex app-server`. Five-hour
  and weekly windows show as Session and Week, per-model limits by name, on the same timeline as
  Claude's. The live query also shows a Codex account's email and plan.

- The TUI header shows the remuda mark, drawn in box-drawing characters, in terminals at least
  40 rows tall; shorter terminals keep the one-line header.

### Changed

- Direction: accounts share one session store and their configuration through symlinks, and
  only the login state stays per account (ROADMAP, 2026-10-03). Launch-time injection, until
  now the way configuration was shared, is the fallback for a home without the links and is
  not extended. **Write boundary:** `remuda setup` used to create an empty home; with
  `[share.claude]` set it now also makes the links above in that new, empty directory, the one
  case in which remuda writes inside a home (SPEC R12, R13). Without `[share.claude]`, and for
  Codex, the home is created empty as before. The injection has been exercised by the test
  suite only: on the author's own accounts every component is linked, so nothing is injected
  there.
- When an account cannot find a session to resume, the message says which `projects` to link
  (SPEC R16).
- Shared instructions (SPEC R18) are exposed through per-item links:
  `$REMUDA_HOME/shared/claude/.claude` is now a directory holding one symlink each to the source
  home's `CLAUDE.md`, `skills`, `commands` and `agents` (for the items the source has), instead
  of one symlink to the whole source home, so a member session's `--add-dir` reaches those four
  items and not the source's transcripts or credentials. An existing whole-home link is migrated
  in place at the next member launch; a remuda started before the upgrade and still running
  launches members without shared instructions (and says so) until it is restarted.

### Fixed

- TUI: identity and usage results no longer land on another account's row when the account list
  changes while a query runs.
- `remuda run <account> -- ...` passes the `--` right after the account to the agent, like every
  other argument (SPEC R5). It used to be dropped, so `remuda run work -- "-1 is not valid"`
  gave the agent the prompt as an option, and `remuda run work -- --resume abc` became
  `--resume abc`. A `--` before the account (`remuda run -- -x`) is still remuda's.
- TUI: the logs of a background session (`l` in Live) can no longer take remuda down. A cursor
  position with a huge parameter in the output of `claude logs` (20 bytes are enough) made
  remuda ask for terabytes of memory and abort, leaving the terminal in raw mode. The output is
  now drawn on a bounded screen (1000 columns, 100,000 rows, 2,000,000 cells; the earliest rows
  are dropped beyond), and only its last 4 MB are read (SPEC R7).

### Security

- The launch log and the caches are readable by the user alone (SPEC R3). `state/launches.jsonl`
  records the arguments of every launch, prompts given on the command line among them, and was
  created with the default mode (usually 0644, in a 0755 `state/`), like `index.json` and
  `stats.json`. `state/` is now created with mode 0700 and its files with mode 0600; a `state/`,
  a log, or a cache from before is tightened the next time remuda writes there. A `state` or
  a file in it that is a symlink is written through and its target keeps its mode. A launch
  log that the group or others can still access after that (a symlink to such a file, or a
  file whose mode remuda cannot change), or that is not a regular file (a FIFO would hand the
  line to its reader), is not appended to: remuda warns and launches anyway.
- A `$REMUDA_HOME/shared` or `shared/claude` that is a symlink is no longer written through
  (SPEC R13, R18). A member's launch used to create the item links and rule copies where the
  link pointed, and remove the `*.md` files under `.claude/rules/` there that the source does
  not have. It now leaves that directory alone and launches without shared instructions, saying
  so.

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

[Unreleased]: https://github.com/ya-luotao/remuda/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/ya-luotao/remuda/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ya-luotao/remuda/releases/tag/v0.1.0
