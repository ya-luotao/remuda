# Remuda architecture

How the code is organized and how data moves through it. The behavior itself is specified in
[SPEC.md](../SPEC.md); this document maps each part of that contract to the code that implements
it, so that a change can start from the right module. Anchors such as `R18` refer to SPEC entries.

## The big picture

Remuda sits between you and the agent CLIs. It keeps a registry of accounts (each one an agent
home directory), reads what the agents leave on disk, runs a few of their machine-readable
commands, and launches them with the right environment and options.

```text
                         you
                          │
            ┌─────────────┴──────────────┐
            │                            │
     remuda <command>               remuda  (TUI)
       src/cli.rs                  src/tui/
            │                            │
            └─────────────┬──────────────┘
                          │  library (src/lib.rs)
   ┌──────────────────────┼─────────────────────────────────────────┐
   │  registry   launch · share · relay   index · attribution       │
   │  paths      identity · usage · live  stats · pricing           │
   │  provider   checks · account_config  transcript · probe · text │
   └──────────────────────┬─────────────────────────────────────────┘
          reads │         │ runs            │ writes (R13)
                ▼         ▼                 ▼
   ┌────────────────┐ ┌───────────────┐ ┌──────────────────────────┐
   │ account homes  │ │ claude, codex │ │ $REMUDA_HOME             │
   │ ~/.claude      │ │ (auth status, │ │   config.toml            │
   │ ~/.claude-work │ │  agents,      │ │   state/  shared/        │
   │ ~/.codex …     │ │  -p /usage,   │ │   homes/<provider>/<name>│
   │ (transcripts,  │ │  app-server,  │ └──────────────────────────┘
   │  rollouts,     │ │  exec on      │   + one relay copy into a
   │  settings)     │ │  launch)      │     target home, on request
   └────────────────┘ └───────────────┘
```

Three rules shape the whole design and are worth knowing before reading any module:

- **Home strings are sacred** (R2). A home is stored and passed to the agent byte-for-byte.
  Canonical paths (realpath) are used only to compare directories: shared stores, duplicate
  registrations, components already shared with the source. They are never passed to an agent.
- **Writes are confined** (R13). Everything remuda writes is under `$REMUDA_HOME`, except the
  transcript and checkpoints an explicit relay copies.
- **The library never reads the process environment.** `main.rs` captures the environment, the
  current directory, the clock, the time zone and whether the standard streams are terminals into
  a `cli::Context` once, and everything below receives an `Env` snapshot. This is what makes the
  sealed test sandbox (R15) possible.

## Module map

Modules are layered: each layer uses the layers below it.

```text
 ┌─ entry ───────────────────────────────────────────────────────────────────┐
 │  main ──► cli                        tui ─ app · workers · render ·       │
 │                                            privacy · timeline · search    │
 ├─ features ────────────────────────────────────────────────────────────────┤
 │  launch · share · relay · setup       accounts: identity · usage · live · │
 │                                                 checks · account_config   │
 │  sessions: attribution                tokens:   stats · pricing           │
 ├─ reading agents' data ────────────────────────────────────────────────────┤
 │  index · transcript · provider::codex · provider::app_server · probe      │
 ├─ foundation ──────────────────────────────────────────────────────────────┤
 │  registry · provider · paths · text                                       │
 └───────────────────────────────────────────────────────────────────────────┘
```

The exceptions, all for a type or a small helper:

- `launch` holds the home variable of R2 (`env_change`, `apply_env`, `CONFIG_DIR_VAR`) and
  `find_on_path`, used by everything that runs an agent: `probe`, `provider`,
  `provider::app_server`, `identity`, `usage` and `live`.
- `registry` reads and validates the `[prices]` tables with `pricing::Prices::from_document` (R3,
  R20).
- `launch` names `relay::Relay` in the launch record; `pricing` prices `stats::Tokens`;
  `provider::codex` lists rollouts with `index::list_rollouts` and checks rate limits with
  `usage::codex_rows`.

| Module | Responsibility | SPEC |
| --- | --- | --- |
| `main.rs` | Parse arguments, capture the process context, call `cli::run` | – |
| `cli` | Every subcommand; the `run` fast path and `exec`; plain-text output | R5, R6, R14, R14a, R19, R20 |
| `registry` | `config.toml`: load and validate strictly, resolve `name` / `provider:name`, add, remove, atomic comment-preserving writes; `[share.claude]` and `[prices]` | R1, R3, R14, R14a |
| `paths` | `$REMUDA_HOME`, `~` expansion, home string checks, the native login's directory | R2, R3 |
| `provider` | What differs between claude and codex: isolation variable, stores, launch arguments, login | R4 |
| `provider::codex` | Rollout parsing: head/tail windows, titles from `session_index.jsonl`, preview, cached rate limits | R10, R17 |
| `provider::app_server` | JSON-RPC client for `codex app-server` (`account/read`, `account/rateLimits/read`) | R4, R10 |
| `probe` | Run a short agent command with captured output and a timeout (killing the process group); run many in parallel | R4, R10 |
| `launch` | Classify arguments, inject `--session-id`, set or unset the home variable, the launch log, `exec` and foreground runs | R2, R6, R16, R17 |
| `share` | Shared configuration: `plan` (reads only) and `apply` (item links, settings file) | R18 |
| `relay` | Check, copy transcript and checkpoints, and prepare the fork launch; undo the copy on failure | R19 |
| `setup` | Create the new home and register it; the login command | R5, R13, R17 |
| `identity` | `claude auth status --json`, `.claude.json` fallback, `codex login status`, `account/read` | R10a |
| `usage` | Cached and live usage for both providers, window labels, severity, reset instants | R10 |
| `live` | Running claude sessions: `agents --json`, `sessions/*.json` fallback checked against `ps`; attach, logs, stop, rm | R7, R16 |
| `checks` | Warnings for the Accounts view | R11 |
| `index` | The session index over claude transcripts and codex rollouts; incremental cache | R8, R17 |
| `transcript` | Reading claude transcripts without loading them whole: windows, complete lines, preview | R8 |
| `attribution` | Which accounts a session belongs to: launch log, live sessions, `history.jsonl`; relay copies | R9, R19 |
| `stats` | Token counting, deduplication across copies, periods, sections, chart buckets, text table | R20 |
| `pricing` | Built-in prices and `[prices]` overrides; the cost of one request in picodollars | R20 |
| `account_config` | What an account's sessions load and where each item comes from | R22 |
| `text` | Terminal text measured in display columns | – |
| `tui` | Terminal ownership, the event loop, foreground launches | R16 |
| `tui::app` | All TUI state and the pure `update(app, event) -> effects` | R8, R16, R17, R19–R22 |
| `tui::workers` | Runs each background effect on a thread and sends back events | R7–R11, R20, R22 |
| `tui::render` | Draws the state; views, overlays, key reference | – |
| `tui::privacy` | Private mode: the redacted copy of the state that is drawn | R21 |
| `tui::timeline` | The shared seven-day reset timeline | R10 |
| `tui::search` | Fuzzy ranking of History rows | R8 |

## Launching an agent

`remuda run`, the TUI and relay all decide a launch through the same function, `launch::plan`,
so that the environment, the `--session-id` injection, the shared configuration and the launch log
cannot differ between them (R6, R16, R18, R19).

```text
 remuda run work -p "hi"
        │
        ▼
 Registry::load(config.toml) ── resolve "work" ──► Account { claude, work, home }
        │
        ▼
 launch::plan(account, args, cwd, ts, new_uuid, sharing, env, config)
        │
        ├─ env_change      CLAUDE_CONFIG_DIR=<home, byte-for-byte>  (or unset for default)
        ├─ classify(args)  NewSession │ Fork{of} │ Existing{id} │ NotASession
        ├─ share::inject   only for sessions of claude members of [share.claude]
        │     ├─ share::plan   reads settings, plugins, links (no writes)
        │     └─ share::apply  ensures shared/claude/.claude links, writes state/settings/<sha>.json
        └─ record          LaunchRecord { ts, account, cwd, args, session_id, fork_of, shared, relay }
        │
        ▼
 append_log(state/launches.jsonl)      the session ID is on disk before the agent starts
        │
        ▼
 exec(claude, [shared options…] + [user args with --session-id <uuid>])
```

The final argument vector puts the injected options first, in `--option=value` form, so that a
variadic option cannot swallow the user's arguments; `--session-id` goes before a `--` terminator,
if there is one:

```text
 claude --add-dir=$REMUDA_HOME/shared/claude  --settings=…/state/settings/3f2a….json
        --plugin-dir=<install> …               -p hi --session-id 1b4e…
        └──────────── shared (R18) ──────────┘ └──── user args + injected ID (R6) ──┘
```

`run` is a fast path: it reads only `config.toml` and the few files shared configuration needs,
starts no agent command and scans no sessions. From the TUI, `tui::launch_in_foreground` leaves
the alternate screen, runs the same plan as a child with `launch::perform`, waits, restores the
terminal and refreshes.

## Relay

A relay (R19) continues a claude session under an account whose store does not hold it. The
original is only read; the target gets a copy and a fork of that copy.

```text
  source (store A, read only)                  target account's home (store B)
  ───────────────────────────                  ───────────────────────────────
  file-history/<id>/*        ── (a) copy ────► file-history/<id>/*
  (union over every home on store A)
  projects/<dir>/<id>.jsonl  ── (b) copy, up ─► projects/<dir>/<id>.jsonl
                                to the last      (the relay copy, hidden from History)
                                complete line
                                                (c) launch log: fork_of, relay { paths, size, mtime }
                                                (d) claude --resume <id> --fork-session
                                                          --session-id <new>
                                                   in cwd_last, with shared configuration
                                                          │
                                                          ▼
                                               projects/<dir>/<new>.jsonl
                                                 (the fork: the target's own session)
```

`relay::check` refuses before anything is written; `relay::copy` does (a) and (b); if (c) fails,
or the agent cannot be started, the transcript copy is removed again (`relay::discard`). The launch log's `relay` object is how the
index later recognizes the copy.

## Reading: index, attribution, statistics

The session index and the token statistics read the same files with the same technique, but keep
separate caches, because they need different parts of each file.

```text
  account homes                         $REMUDA_HOME/state/
  ─────────────                         ───────────────────
  claude projects/*/*.jsonl ──┐
  codex  sessions/**/rollout ─┤ index::stores   (dedup by realpath of the store)
                              ▼
                        index::refresh ──────────────────► index.json   (head/tail windows,
                              │                                          offsets; schema 3)
                              ▼
  launches.jsonl ─────► attribution::collect ◄── live sessions (agents --json)
  history.jsonl  ─────►       │
                              ▼
                     History rows, `remuda sessions`, resume and relay targets


  claude projects/**/*.jsonl ─┐
  codex rollouts (+ archived) ┤ stats::sources
                              ▼
                        stats::refresh ──────────────────► stats.json   (one row per request,
                              │                                          whole files; schema 2)
                              ▼
                        stats::report(period, prices)  ◄── pricing (built-in + [prices])
                              │
                              ├─► stats::format        `remuda stats`
                              └─► stats::chart_series  Stats view chart
```

Both refreshes follow the append-only rule of R8: an unchanged file is skipped, a grown file is
read from the last complete line, and a file that shrank, was replaced or moved back in time is
read again whole. Only complete lines are parsed. The first index scan reads only a head and a
tail window per file; the statistics read every file whole the first time.

Deduplication is the heart of the statistics: a claude message counts once by `message.id`
across records, forks, relay copies and shared stores; a codex request counts once by its
cumulative total. A session attributed to several accounts is counted once, for all of them
together, so the sections add up to the overall total.

## The TUI

The TUI is an update/effect loop. All state lives in `tui::app::App`; `update` is pure (no I/O),
and anything slow is described as an `Effect` that a worker thread carries out and answers with
an `Event`.

```text
          keys (crossterm)          ticks (100 ms)
                 │                        │
                 ▼                        ▼
        ┌──────────────────────────────────────────┐
        │ event loop (tui/mod.rs)                  │
        │   batch = keys + queued events + tick    │
        │   for event in batch:                    │
        │     effects = app::update(&mut app, ev) ─┼──► Launch / Relay / Setup:
        │     spawn(effect)  ─────────┐            │      suspend TUI, run in foreground,
        │   draw(render(app))         │            │      then queue the result
        └─────────────────────────────┼────────────┘
                 ▲                    ▼
                 │           tui::workers::spawn
                 │   RefreshIndex · Identities · CachedUsage · LiveUsage · Live
                 │   Attribution · Checks · Stats · Preview · Config · CheckLaunch
                 │   Logs · Control · RemoveAccount · RolloutWritten
                 │                    │
                 └──── mpsc::Sender<Event> ◄──── a thread per effect (per account for
                                                 identities and live usage)
```

- Keys are read on the loop's own thread. While a launched agent has the terminal, that thread is
  waiting for it, so nothing else reads the agent's input.
- Results that can arrive late are matched by what they carry, never by position: the account
  they belong to (identities, usage), or the number of the request they answer (the
  Configuration pane, pre-launch checks), so an answer to an outdated request is ignored.
- A launch that resumes a session in place is preceded by `CheckLaunch`, which queries every
  account's running sessions again right before starting (R16).
- In private mode, `render` does not draw `App` itself but `privacy::redacted(app)`, a copy in
  which names are aliased and personal fields masked. `privacy::Snapshot` keeps that copy until
  the app changes, since making it for every frame is too slow for a large index. Every
  field of the state is destructured there, so a new field does not compile until it is decided
  how private mode shows it (R21).

## Shared configuration

`share::plan` decides, for one account, one directory and one argument list, what the source's
configuration adds; `share::apply` makes it real. The Configuration pane (R22) calls `plan` only,
which is why the pane cannot disagree with a launch.

```text
 [share.claude] from = "default"                 member account "work"
 source home ~/.claude                           home ~/.claude-work
 ───────────────────────                         ───────────────────────────────
 CLAUDE.md skills/ commands/ agents/  ──links──► --add-dir=$REMUDA_HOME/shared/claude
                                                   (.claude/{CLAUDE.md,skills,commands,agents})
                                                   + CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD=1
 settings.json  − home − project − local  ─────► --settings=state/settings/<sha256>.json (0600)
                − authentication keys              { …source-only keys…, autoMemoryDirectory }
 enabledPlugins + installed_plugins.json  ─────► --plugin-dir=<install path>  (one per plugin)
 projects/<project>/memory  ───────────────────► autoMemoryDirectory (inside the same --settings)
```

Each row is skipped when the member's home already resolves to the source's item by realpath
(an existing symlink layout, R12), so nothing loads twice.

## Files remuda owns

```text
 $REMUDA_HOME/                    (default ~/.remuda)
 ├── config.toml                  registry; written by add / setup / remove      registry
 ├── homes/<provider>/<name>/     empty homes created by setup                   setup
 ├── shared/claude/.claude/       one symlink per shared instruction item        share
 └── state/                       caches and logs; safe to delete
     ├── index.json               session index cache                            index
     ├── stats.json               token statistics cache                         stats
     ├── launches.jsonl           one line per launch (append-only)              launch
     └── settings/                injected settings                              share
         ├── <sha256>.json        one per content (0600), pruned after 30 days
         └── .lock                taken while choosing or pruning a file
```

`launches.jsonl` is the only file in `state/` whose loss costs information: attribution of
sessions started through remuda falls back to `history.jsonl`, and relay copies would reappear
in History (R19).

## Tests

Integration tests live in `tests/`, one file per area, each naming the SPEC entries it covers in
its first line. `tests/common/` builds the sealed sandbox (R15): a fresh `HOME` and
`REMUDA_HOME`, a cleared environment, and fake `claude` and `codex` scripts first on `PATH` that
record their arguments and environment and answer from fixtures. `tests/common/transcripts.rs`
and `tests/common/rollouts.rs` build synthetic records with the real shapes.

| Test file | Covers |
| --- | --- |
| `harness.rs` | The sandbox itself (R15) |
| `registry_cli.rs`, `remove_cli.rs`, `setup_cli.rs` | `add`, `remove`, `setup` (R1–R3, R13, R14, R14a) |
| `run_cli.rs`, `launch_log.rs`, `tui_launch.rs` | Launch, `--session-id`, launch log, TUI launches (R2, R5, R6, R16) |
| `share_cli.rs` | Shared configuration (R18) |
| `relay_cli.rs` | Relay (R19) |
| `index.rs`, `codex_index.rs`, `preview.rs`, `sessions_cli.rs` | Session index and preview (R8, R17) |
| `attribution.rs` | Attribution (R9) |
| `live.rs` | Running sessions (R7) |
| `usage_cli.rs`, `list_identity.rs`, `codex_cli.rs` | Usage and identity (R4, R10, R10a, R17) |
| `stats.rs`, `stats_cli.rs` | Token statistics and cost (R20) |
| `tui_cli.rs` | Bare `remuda` needs a terminal (R5) |

Unit tests sit next to the code (`mod tests`); the TUI's are in `src/tui/tests.rs` and drive
`app::update` and `render` against a test backend, without a terminal.

`examples/corpus_timing.rs` and `examples/codex_timing.rs` time the index on a real claude or
codex home. They read it only, and put the cache in a temporary directory:

```sh
cargo run --release --example corpus_timing -- [<projects dir> [<history.jsonl>]]
cargo run --release --example codex_timing -- [<codex home>]
```

## Where to make a change

| To … | Start in |
| --- | --- |
| Add or change a subcommand | SPEC R5, `cli`, a `tests/*_cli.rs` file |
| Change what a launch passes to the agent | SPEC R6 / R18, `launch::prepare_with` or `share::plan` |
| Support another agent CLI | SPEC R4, `provider` (every `match Provider`), `index`, `usage`, `identity` |
| Read a new field from transcripts | `transcript` (index) or `stats` (counts); bump the cache's `SCHEMA_VERSION` |
| Add a TUI action | `tui::app` (`Key` → `Effect`), `tui::workers` (the effect), `tui::render`, `tui::privacy` |
| Add something shown on screen | `tui::app` state, `tui::render`, and its case in `tui::privacy::redacted` |
| Add a model price | SPEC R20 table and `pricing` |
