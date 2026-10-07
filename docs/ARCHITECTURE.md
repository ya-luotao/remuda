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
   │  registry   launch · share           index · attribution       │
   │  paths      identity · usage · live  stats · pricing           │
   │  provider   checks · account_config  transcript · probe · text │
   │  privacy    pick · jev · home_items  owned (every write)       │
   │  interrupt  account_command                                    │
   └──────────────────────┬─────────────────────────────────────────┘
          reads │         │ runs            │ writes (R13)
                ▼         ▼                 ▼
   ┌────────────────┐ ┌───────────────┐ ┌──────────────────────────┐
   │ account homes  │ │ claude, codex │ │ $REMUDA_HOME             │
   │ ~/.claude      │ │ (auth status, │ │   config.toml            │
   │ ~/.claude-work │ │  agents,      │ │   state/  shared/        │
   │ ~/.codex …     │ │  -p /usage,   │ │   homes/<provider>/<name>│
   │ (transcripts,  │ │  app-server,  │ └──────────────────────────┘
   │  rollouts,     │ │  exec on      │
   │  settings)     │ │  launch)      │
   └────────────────┘ └───────────────┘
                      + curl → api.typesafe.ai:
                        `pick` only, with a key (R23)
```

Three rules shape the whole design and are worth knowing before reading any module:

- **Home strings are sacred** (R2). A home is stored and passed to the agent byte-for-byte.
  Canonical paths (realpath) are used only to compare directories: shared stores, duplicate
  registrations, components already shared with the source. They are never passed to an agent.
- **Writes are confined** (R13). Everything remuda writes is under `$REMUDA_HOME`, and all of
  it is written by one module, `owned`. The one write inside a home is the set of symlinks
  `setup` makes in the directory it has just created, before the login (R12, R18); nothing is
  written into a home after that, or into a home registered with `add`. `owned` reaches every
  directory it creates, replaces, or removes in without following a symlink below
  `$REMUDA_HOME`, and writes into it through its descriptor; the symlinks it writes through
  are the ones R3 names (`config.toml`, `state`, a file in `state/`).
- **The library never reads the process environment.** `main.rs` captures the command line, the
  environment, the current directory, the clock, the time zone and whether the standard streams
  are terminals into a `cli::Context` once, and everything below receives an `Env` snapshot.
  This is what makes the sealed test sandbox (R15) possible.

## Module map

Modules are layered: each layer uses the layers below it.

```text
 ┌─ entry ───────────────────────────────────────────────────────────────────┐
 │  main ──► cli                        tui ─ app · work · workers · render  │
 │                                            privacy · timeline · search ·  │
 │                                            accounts                       │
 ├─ features ────────────────────────────────────────────────────────────────┤
 │  launch · share · setup · home_items  accounts: identity · usage · live · │
 │                                                 checks · account_config   │
 │  sessions: attribution                tokens:   stats · pricing           │
 │  recommendation: pick · jev                                               │
 ├─ reading agents' data ────────────────────────────────────────────────────┤
 │  index · transcript · provider::codex · provider::app_server · probe      │
 │  tracking · account_command                                               │
 ├─ foundation ──────────────────────────────────────────────────────────────┤
 │  registry · provider · paths · privacy · text · owned · interrupt         │
 └───────────────────────────────────────────────────────────────────────────┘
```

The exceptions, all for a type or a small helper:

- `launch` holds the home variable of R2 (`env_change`, `apply_env`, `CONFIG_DIR_VAR`) and
  `find_on_path`, used by everything that runs an agent: `probe`, `provider`,
  `provider::app_server` and `account_command`, through which `identity`, `usage`, `live` and
  `pick` run an agent's commands for an account.
- `registry` reads and validates the `[prices]` tables with `pricing::Prices::from_document` (R3,
  R20), and `[pick]` with `pick::Config::from_document` (R3, R23).
- `pricing` prices `stats::Tokens`; `provider::codex` lists rollouts with `index::list_rollouts` and checks rate limits with
  `usage::codex_rows`.
- `home_items` and `share` use each other: `share::plan` starts from the relations
  `home_items` gives it, and `home_items` reads the source's `settings.json` and counts its
  rule files with `share`'s readers (`read_settings`, `withheld`, `rule_files`).

| Module | Responsibility | SPEC |
| --- | --- | --- |
| `main.rs` | Parse arguments, capture the process context, call `cli::run` | – |
| `cli` | Every subcommand; the `run` fast path and `exec`; plain-text output | R5, R6, R14, R14a, R20, R23 |
| `registry` | `config.toml`: load and validate strictly, resolve `name` / `provider:name`, add, remove, comment-preserving edits (written by `owned`, under its lock); `[share.claude]` and `[prices]` | R1, R3, R14, R14a |
| `owned` | Every write below `$REMUDA_HOME`: the way down without following a symlink (`Dir`), private modes and tightening, temporary file + rename and the cleanup of leftovers, exclusive locks (`Locks`), the registry's locked update, the caches, the launch log's rules; where things are (`state_dir`, `launch_log`, …) | R3, R13, R18 |
| `paths` | `$REMUDA_HOME`, `~` expansion, home string checks, the native login's directory | R2, R3 |
| `provider` | What differs between claude and codex: isolation variable, stores, launch arguments, login | R4 |
| `provider::codex` | Rollout parsing: head/tail windows, titles from `session_index.jsonl`, preview, cached rate limits | R10, R17 |
| `provider::app_server` | JSON-RPC client for `codex app-server` (`account/read`, `account/rateLimits/read`) | R4, R10 |
| `probe` | Run a short command in its own process group with captured output and a timeout, taking its output once it exited and terminating the group when it times out; JSON-RPC over stdio on the same core, its group terminated on every way out; run many in parallel; run `curl` with its configuration on stdin | R4, R10, R23 |
| `account_command` | Run an agent's command for an account: pick the provider's program, set or remove the home variable (and remove the variables a caller names), run it, and word the failure (`Runner`, `OnPath`); the shape of a parse that may be partial (`Parsed`) | R2, R4, R10, R10a |
| `interrupt` | The terminal's signals and SIGTERM, owned in one place: Ctrl-C and Ctrl-\ sat out while a foreground child has the terminal, passed on to the process groups of running commands and of commands being started, before remuda ends by them; a place for each of at most 1024 commands at a time, which one more waits for | R4, R6 |
| `launch` | Classify arguments, inject `--session-id`, set or unset the home variable, the launch record, `exec` and foreground runs | R2, R6, R16, R17 |
| `share` | Shared configuration injected at launch, the fallback for what a home does not link: `plan` (reads only; a `Plan` carries the relation of every shared item) and `apply` (item links, rule copies, settings file: what they are; `owned` writes them) | R18 |
| `home_items` | The items of a claude home, once: the catalog (each item's name, whether `setup` links it and on what condition, whether the launch's `--add-dir` carries it, when the source's counts as one to share, what breaks when it is another account's) and how a member's home relates to the source's right now (`Source::relate`, `linked_elsewhere`, `membership`) | R11, R12, R18 |
| `setup` | Create the new home (through `owned`), link a member's to the source's session store and configuration (`share_links`, over the catalog of `home_items`), and register it; the login command | R5, R12, R13, R17, R18 |
| `identity` | Parses `claude auth status --json`, `codex login status` and `account/read`; the `.claude.json` fallback | R10a |
| `usage` | Cached and live usage for both providers as rows (what the agent said; a partly read answer is not used), window labels, the text of `remuda usage` | R10 |
| `usage::snapshot` | Rows read at an instant: each window's reset (ahead, passed since, unknown), its percentage and severity (unknown once it has reset since), the snapshot's age and staleness. The one place that compares a reset with now; `usage`, `pick`, `jev` and the TUI take it from here | R10, R23 |
| `live` | Running claude sessions: parses `agents --json`, `sessions/*.json` fallback checked against `ps`; attach, logs, stop, rm | R7, R16 |
| `checks` | Warnings for the Accounts view, among them what a member's home links and does not (the relations of `home_items`, put into words) | R11 |
| `index` | The session index over claude transcripts and codex rollouts: its stores, how they are listed, the head and tail windows of one file, its cache | R8, R17 |
| `tracking` | Keeping a cache up to date with the files below a set of directories: which are reused, read on or read whole, the worker threads, what vanished, progress, and a directory that cannot be listed. Private; `index` and `stats` each give it an adapter | R8, R20 |
| `transcript` | Reading claude transcripts without loading them whole: windows, complete lines, preview; a session's last time, directory and model for `pick` (`session_tail`) | R8, R23 |
| `attribution` | Which accounts a session belongs to: launch log, live sessions, `history.jsonl`; the latest launch that ran a session (`last_launch`, R23) | R9, R23 |
| `stats` | Token counting, deduplication across copies, periods, sections, chart buckets, text table; its sources, how they are listed, its cache | R20 |
| `pricing` | Built-in prices and `[prices]` overrides; the cost of one request in picodollars | R20 |
| `account_config` | What an account's sessions load and where each item comes from, on top of `share::plan` | R22 |
| `pick` | `[pick]`; candidates, which windows apply to a model, and feasibility; the rules' ranking; combining Jev's answer; the report; `--run` options; the session the arguments resume or fork and what the launch log and the session index say of it (`session_args`, `read_session`); when `--wait` tries again (`next_attempt`) | R3, R23 |
| `wait` | `--wait`: when to try again from the windows that block and their resets (`schedule`, its named constants), the loop of attempts with an injected clock and sleep (`until`), the status line | R10, R23 |
| `jev` | The request to Jev (aliased state, Choice and Score questions), `curl` transport, response parsing | R23 |
| `privacy` | `Marked`, a message in the pieces it was put together from (remuda's words, a path, text from elsewhere); account-name aliases, whole-word aliasing of names in free text, and aliasing of every `provider:name` in the `pick` notes | R21, R23 |
| `text` | Terminal text measured in display columns | – |
| `tui` | Terminal ownership, the event loop, foreground launches | R16 |
| `tui::app` | All TUI state and the pure `update(app, event) -> effects` | R8, R16, R17, R20–R22 |
| `tui::work` | The slot of one kind of background work: whether a round is out, whether it runs once more, whether a result is the one still wanted | R7, R16, R22 |
| `tui::workers` | Runs each background effect on a thread and sends back events | R7–R11, R20, R22 |
| `tui::accounts` | The account listing: reads the registry again for whatever goes over the accounts, tells a change once, refuses a launch as an account no longer listed | R3, R16 |
| `tui::render` | Draws the state; views, overlays, key reference | – |
| `tui::privacy` | Private mode: the redacted copy of the state that is drawn; a `Marked` message masked piece by piece; aliases from `privacy` | R21 |
| `tui::timeline` | The shared seven-day reset timeline | R10 |
| `tui::search` | Fuzzy ranking of History rows | R8 |

## Launching an agent

`remuda run` and the TUI decide a launch through the same function, `launch::plan`, so that the
environment, the `--session-id` injection, the shared configuration and the launch log cannot
differ between them (R6, R16, R18).

```text
 remuda run work -p "hi"
        │
        ▼
 cli::run_arguments(argv)     the account, then every token after it as typed (R5): clap
        │                     would drop a `--` right after the account
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
        │     └─ share::apply  ensures shared/claude/.claude links and rule copies,
        │                      writes state/settings/<sha>.json
        └─ record          LaunchRecord { ts, account, cwd, args, session_id, fork_of, shared }
        │
        ▼
 append_log(state/launches.jsonl)      the session ID is on disk before the agent starts;
        │                              state/ is 0700 and the log 0600 (R3, owned::append_log)
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
                     History rows, `remuda sessions`, resume targets


  claude projects/**/*.jsonl ─┐
  codex rollouts (+ archived) ┤ stats::sources
                              ▼
                        stats::refresh ──────────────────► stats.json   (one row per request,
                              │                                          whole files; schema 3)
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

That rule is written once, in `tracking`. `index::refresh` and `stats::refresh` each hand
`tracking::refresh` their cache's map and an adapter, a `tracking::Files`:

```text
  tracking::refresh(cache, directories, adapter, progress) -> RefreshStats
    │  per listed file: reused · incremental · cold (size, mtime, inode)
    │  opens it and stats it again: a decision is only downgraded
    │  8 worker threads · what vanished drops out · progress
    │  a directory that cannot be read: what the cache has of that store below
    │  it stays, and it is reported (RefreshStats::unreadable)
    │  a store that cannot be resolved: what the cache has of the real path it
    │  last resolved to stays, and it is reported likewise
    │
    ├─ Files::list   index: projects/*/*.jsonl (top level), sessions/**/rollout-*.jsonl
    │                stats: also below <project>/<session>/, and archived_sessions
    └─ Files::read   index: head and tail windows of the open file → Entry
                     stats: every complete line, in chunks → FileStats
```

The two adapters list differently on purpose (R8 takes the top level of a project, R20 also the
subagent transcripts below it) and read differently; everything else is `tracking`'s. An adapter
reads no directory and examines no entry itself: it lists through `tracking::Listing`, which is
where a directory that does not exist (it has no files: what the cache had there drops out) is
told from one that exists but cannot be read, because it cannot be listed or because what it
lists cannot be examined (nothing is known: what the cache has of that store below it stays,
and `remuda sessions`, `remuda stats` and the TUI say the result is incomplete).

A store whose real path cannot be found (its home cannot be searched, say) fails before there
is anything to list. `index::stores` and `stats::sources` leave it out, as they always did, so
that `checks`, `attribution` and the TUI see the same lists; `index::resolve` and
`stats::resolve` return those lists together with what each home gives as its directory
(`index::Given`, one for each directory a home gives that is or may be there, with its real
path or the error; for the statistics also a codex home's `archived_sessions`), told from
missing ones by `tracking::real_dir`. `remuda sessions`, `remuda stats` and the TUI workers pass
both to `index::refresh_with` / `stats::refresh_with`. A directory that cannot be resolved has
no real path for its cached files to be below, so each cache remembers the real path every
directory last resolved to, by the whole path its home gives it (`Index::stores`,
`Cache::sources`, kept up by `tracking::remember`), and `tracking` keeps the files of that
real path alone: a store that is gone or that left the registry drops out as usual, whatever
cannot be resolved beside it, and an account's name carries nothing from one home to another.

Deduplication is the heart of the statistics: a claude message counts once by `message.id`
across records, forks and shared stores; a codex request counts once by its
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
        │     effects = app::update(&mut app, ev) ─┼──► Launch / Setup:
        │     spawn(effect)  ─────────┐            │      suspend TUI, run in foreground,
        │   draw(render(app))         │            │      then queue the result
        └─────────────────────────────┼────────────┘
                 ▲                    ▼
                 │           tui::workers::spawn
                 │   RefreshIndex · Identities · CachedUsage · LiveUsage · Live
                 │   Attribution · Checks · Stats · Preview · Config · CheckLaunch
                 │   Logs · Control · RemoveAccount · RolloutWritten · ReadAccounts
                 │                    │
                 └──── mpsc::Sender<Event> ◄──── a thread per effect (per account for
                                                 identities and live usage)
```

- Keys are read on the loop's own thread. While a launched agent has the terminal, that thread is
  waiting for it, so nothing else reads the agent's input.
- Every kind of background work has a `work::Slot` in the state (one per account for
  identities and usage). `update` asks the slot before it starts anything, and the slot says
  whether an effect goes out: work that is already out for the same target is not started
  twice (a held key starts one thread), and work asked for again because what it reads may have
  changed (a launch ended, the account list changed, the user asks for the configuration or
  the logs again) runs once more when the round that is out reports. A target has one round
  out at most, whatever was asked in between: a round given up (a cancelled check, a session
  the selection left) stays out until it reports, and its target asked for again waits for it.
- Results that can arrive late are matched by what they carry, never by position: the account
  they belong to (identities, usage), or the target they are for (the transcript of a preview,
  the account's session of the logs, the account of the Configuration pane, the launch of a
  pre-launch check). The slot then says whether that target's round is the one waited for, so
  an answer to a request that was given up is ignored. Since a target has one round out, the
  target is enough to find the round; the Configuration pane and pre-launch checks carry the
  round's number as well, a second check that an answer is the one asked for (R16).
- A launch that resumes a session in place is preceded by `CheckLaunch`, which queries every
  account's running sessions again right before starting (R16).
- Neither the app nor `Deps` is where the accounts come from: `tui::accounts::Listing` is, and
  `Listing::read` is the only way to them. A worker that goes over the accounts reads the registry
  when it starts (what is asked of each account goes to the accounts its effect names that are
  still listed); so do the check before a launch and the launch itself, and `r` asks for a read of
  its own (`ReadAccounts`), which nothing still running holds back. A read that finds another list
  than the last one sends one `Event::Accounts` before it returns, so the app rebuilds its rows
  ahead of any result for the new list, and results for an account that is gone find no row.
  Results that are not per account but for the whole list: the checks run once more through their
  slot when the list changed while they ran, and those for the old list are not shown; the
  statistics go out through `Listing::answer`, which holds a report back when the list changed
  while it was computed, and the worker then computes it again (their slot keeps a second
  computation from starting beside it). What a `config.toml` that cannot be read means (the last
  accounts, nothing shared, built-in prices, no launch) is decided there once, in `Reading`.
- In private mode, `render` does not draw `App` itself but `privacy::redacted(app)`, a copy in
  which names are aliased and personal fields masked. `privacy::Snapshot` keeps that copy until
  the app changes, since making it for every frame is too slow for a large index. Every
  field of the state is destructured there, so a new field does not compile until it is decided
  how private mode shows it (R21).
- A message remuda puts together for the screen is a `privacy::Marked`, made where the message
  is made: a notice or a form's error (`tui::app`), a check (`checks`), a problem of the
  Configuration pane (`account_config`), the error of a cache (`tui::workers`), the line of
  an incomplete refresh (`tracking`), why the registry cannot be read or a launch is refused
  (`tui::accounts`: `Reading::unreadable`, `Reading::refusal`). It reads as one
  string and keeps its pieces: remuda's own words (`.words()`), each path (`.path()`), each
  text from elsewhere (`.text()`: an error of the system, an agent's output, a name read from
  a file). `tui::privacy::Scrubber` masks a path whole, looks for no path in remuda's words,
  and only in text from elsewhere falls back to hiding a line from its first path on, which
  never reaches the next piece. A plain string is text from elsewhere (`From<&str>`): that
  words are remuda's own is always said, so an entry not yet made of pieces is masked the
  careful way. A message is also an error (`Err(message.into())`, or `message.because(cause)`
  where a context would go): it prints as the same string, so the command line does not change,
  and `Marked::from_error` gives the TUI the pieces of every message in the chain, the causes
  nobody marked staying one text. `share::read_settings` tells its errors this way, which is
  how a settings problem of the Configuration pane keeps its reason in private mode.

## Shared configuration

Accounts share one session store and their configuration through symlinks in each member's home
(R18). `setup::create_and_register` makes them once, for the home it has just created:

```text
 remuda setup work                      [share.claude] from = "default"
        │
        ▼
 setup::plan              every check, and the source's home, before any side effect
        │
        ▼
 setup::create_and_register
        ├─ create_home    $REMUDA_HOME/homes/claude/work, mode 0700, must not exist; each
        │                 level is opened from the one above without following a symlink
        │                 (a symlinked homes or homes/claude is refused), and the new home
        │                 stays open
        ├─ share_links    through that descriptor, only if the directory is empty: for each
        │                 item the catalog says setup links (home_items) that the source
        │                 has, work/<item> -> <source home>/<item> (as registered);
        │                 settings.json only if it has no authentication at that moment
        │                 (checked once);
        │                 never .claude.json, history.jsonl, sessions, remote-settings.json,
        │                 policy-limits.json; nothing replaced, nothing removed
        └─ register       config.toml
        │
        ▼
 claude auth login        in the new home, in the foreground
```

A home registered with `add` is never linked by remuda; `checks::sharing` reports what it does
not link (R11).

What a home holds, and what a member's home has of the source's, is answered in one place,
`home_items`, and read everywhere else:

```text
 home_items
   the catalog      in R18's order: 13 items a member shares with the source (session
                    store, then configuration), 5 that stay per account
   Source           the source's home: its settings.json read once, its authentication
                    settings (withheld), what setup links (setup_links)
   Source::relate   member's home × source ─► for each shared item, one of:
                    linked (same realpath) │ unlinked (the source has it and the home
                    does not reach it) │ the source has none
   linked_elsewhere a per-account item that is a link to another account's
   membership       Alone │ Source │ OptedOut │ SourceMissing │ Member
        │
        ├─► share::plan       what is linked is not injected; Plan.items keeps the table
        │        └─► account_config::read   origins in the Configuration pane (R22)
        ├─► checks::sharing   what is not linked, in words (R11)
        └─► setup             share_links and its notes (R18)
```

None of these spells an item's name or decides by itself whether a home has the source's, so
they cannot disagree about what is linked. Paths are always the home as registered with the
item's name appended (R2); realpaths are compared inside `home_items` and never leave it.
The catalog does not say how a launch injects the settings, the plugins or the memory
locations: `share::plan` decides that, item by item, from the relations. Of injection the
catalog knows only which items the one `--add-dir` carries, as links or as copies, because
`share` builds its directory from that list.

Injection is the fallback for what a member's home does not link. `share::plan` decides, for
one account, one directory and one argument list, what the source's configuration adds;
`share::apply` makes it real. The Configuration pane (R22) calls `plan` only, which is why the
pane cannot disagree with a launch.

```text
 [share.claude] from = "default"                 member account "work"
 source home ~/.claude                           home ~/.claude-work
 ───────────────────────                         ───────────────────────────────
 CLAUDE.md skills/ commands/ agents/  ──links──► --add-dir=$REMUDA_HOME/shared/claude
                                                   (.claude/{CLAUDE.md,skills,commands,agents})
                                                   + CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD=1
 rules/**/*.md  ──copies──────────────────────►   (.claude/rules/**, the same --add-dir)
 settings.json  − home − project − local  ─────► --settings=state/settings/<sha256>.json (0600)
                − authentication keys              { …source-only keys…, autoMemoryDirectory }
 enabledPlugins + installed_plugins.json  ─────► --plugin-dir=<install path>  (one per plugin)
 projects/<project>/memory  ───────────────────► autoMemoryDirectory (inside the same --settings)
 agent-memory/  ───────────────────────────────► CLAUDE_CODE_REMOTE_MEMORY_DIR=<source home>
                                                   (with autoMemoryDirectory, or when the
                                                   member's projects/ is a link to the source's:
                                                   claude then writes memory by the source's
                                                   path, not through the link, without asking)
```

Each row is skipped when the member's home already resolves to the source's item by realpath
(linked, in the relations above: R12), so nothing loads twice; a fully linked home gets
nothing injected. Sessions have no row: they are shared only through the `projects` link, and
so are `file-history`, `hooks`, `output-styles` and `keybindings.json`, which no launch option
carries.

## Files remuda owns

```text
 $REMUDA_HOME/                    (default ~/.remuda)
 ├── config.toml                  registry; written by add / setup / remove      registry
 ├── homes/<provider>/<name>/     homes created by setup: empty, or holding      setup
 │                                the links to the source's home (R18)
 ├── shared/claude/.claude/       one symlink per shared instruction item,       share
 │                                copies of the source's rules; never written
 │                                through a shared or shared/claude symlink
 └── state/                       caches and logs (0700, files 0600; R3); safe
     │                            to delete
     ├── index.json               session index cache                            index
     ├── stats.json               token statistics cache                         stats
     ├── launches.jsonl           one line per launch (append-only)              launch
     └── settings/                injected settings                              share
         ├── <sha256>.json        one per content (0600), pruned after 30 days
         └── .lock                taken while choosing or pruning a file
```

`launches.jsonl` is the only file in `state/` whose loss costs information: attribution of
sessions started through remuda falls back to `history.jsonl`.

The modules in the right column decide what a file holds; `owned` is the one that writes it.
What every write there has in common lives in that module and is tested there once:

```text
 owned::Dir            an open directory of remuda's own: each level opened from the one
                       above with O_NOFOLLOW, every operation an *at call on one entry name
   state / settings    private (0700, tightened, never loosened); `state` may be a symlink
   shared / homes      real directories or refused (Blocked: Symlink, NotADirectory)
   write, replace_link `.remuda-<pid>-<32 hex>.tmp` in the same directory, then rename
   sweep               removes the temporary files of processes that are gone
   (a directory that can be searched and written but not read is opened for that alone:
    written in by name, not listed, so not swept and not locked)
   lock, lock_file     flock through the `Locks` seam; Lockless::Refuse (the registry)
                       or Lockless::Proceed (settings, shared instructions)
 owned::update_registry  lock the directory of the file that is replaced ($REMUDA_HOME, or
                         where a symlinked config points), run the edit, write it there
 owned::save_cache       0600 whatever was there; through a symlinked cache file
 owned::append_log       regular file only, the user's alone, opened without blocking
```

The symlinks remuda writes through are the three R3 names; everything else is opened without
following one. Reads do not go through `owned`, so a command that only reads creates nothing.

## Tests

Integration tests live in `tests/`, one file per area, each naming the SPEC entries it covers in
its first line. `tests/common/` builds the sealed sandbox (R15): a fresh `HOME` and
`REMUDA_HOME`, a cleared environment, and fake `claude` and `codex` scripts first on `PATH` that
record their arguments and environment and answer from fixtures. `tests/common/transcripts.rs`
and `tests/common/rollouts.rs` build synthetic records with the real shapes, and
`tests/common/homes.rs` builds claude homes (a source's items, a member's links); the unit
tests include that same file (`src/lib.rs`), so there is one builder.

| Test file | Covers |
| --- | --- |
| `harness.rs` | The sandbox itself (R15) |
| `registry_cli.rs`, `remove_cli.rs`, `setup_cli.rs` | `add`, `remove`, `setup` and its links (R1–R3, R12–R14, R14a, R18) |
| `run_cli.rs`, `launch_log.rs`, `tui_launch.rs` | Launch, `--session-id`, launch log, TUI launches (R2, R5, R6, R16) |
| `share_cli.rs` | Shared configuration (R18) |
| `index.rs`, `codex_index.rs`, `preview.rs`, `sessions_cli.rs` | Session index and preview (R8, R17) |
| `attribution.rs` | Attribution (R9) |
| `live.rs` | Running sessions (R7) |
| `usage_cli.rs`, `list_identity.rs`, `codex_cli.rs` | Usage and identity (R4, R10, R10a, R17) |
| `stats.rs`, `stats_cli.rs` | Token statistics and cost (R20) |
| `tui_cli.rs` | Bare `remuda` needs a terminal (R5) |

Unit tests sit next to the code (`mod tests`); the TUI's are in `src/tui/tests.rs` and drive
`app::update` and `render` against a test backend, without a terminal. The modules that parse
an agent's answers (`identity`, `usage`, `live`, `pick`) are tested with
`account_command::Scripted`, a runner that answers from a script without starting a process;
how a process is run, timed out, cleaned up and interrupted is tested in `probe` and
`interrupt` with real `sh` scripts.

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
| Change what `setup` links in a new home | SPEC R12 / R13 / R18, the catalog in `home_items` (`ITEMS`, read through `Source::setup_links`) and `setup::share_links`, `tests/setup_cli.rs`; it is the write boundary |
| Add an item of a home, or change how one is shared or kept per account | SPEC R11 / R18, its entry in `home_items::ITEMS`; then `share::plan` if a launch injects it, `checks::sharing` if the Accounts view warns about it |
| Write a new file below `$REMUDA_HOME`, or change a mode, a lock, or how a file is replaced | SPEC R3 / R13, `owned` (and its tests, which cover what all writes share); never `std::fs` writes elsewhere |
| Support another agent CLI | SPEC R4, `provider` (every `match Provider`), `index`, `usage`, `identity` |
| Run another agent command for an account | `account_command` runs it (`runner.run_ok(account, ARGS, timeout)`); the caller only parses, and tests its parsing with `account_command::Scripted` |
| Change how commands are run, killed or interrupted | SPEC R4, `probe` (`Bounded`) and `interrupt`; `probe`'s script tests |
| Read a new field from transcripts | `transcript` (index) or `stats` (counts); bump the cache's `SCHEMA_VERSION` |
| Change when a file is read again, or what an unreadable directory means | SPEC R8, `tracking` (`decide`, `Listing`); its unit tests drive it with a fake adapter |
| Keep another set of append-only files up to date | A `tracking::Files` adapter (how to list, how to read one file) and a map for `tracking::refresh` |
| Add a TUI action | `tui::app` (`Key` → `Effect`), `tui::workers` (the effect), `tui::render`, `tui::privacy` |
| Use the accounts, shared configuration or prices in the TUI | `deps.listing.read(&tx)` in the worker (`tui::accounts`); never `Registry::load` |
| Add background work to the TUI | a `work::Slot` in the `tui::app` state (asked before the `Effect` goes out, told when the `Event` comes back), the effect in `tui::workers`, the slot's case in `tui::privacy::redacted` |
| Add something shown on screen | `tui::app` state, `tui::render`, and its case in `tui::privacy::redacted` |
| Add or change a message with a path in it (a check, a notice, a problem) | Build it as `privacy::Marked` where it is made: `.words()` for what remuda says, `.path()` for each path, `.text()` for anything read from a file or a child; the tests of `checks` and `account_config` fail on a path formatted into words. An error the TUI shows: return the message as the error and read it with `Marked::from_error`; `tests/private_messages.rs` draws the result |
| Add a model price | SPEC R20 table and `pricing` |
| Change what a usage window means once its reset has passed, or how old usage counts | SPEC R10 / R23, `usage::snapshot` (`Snapshot::at`); every surface reads it from there |
| Change what `remuda pick` sends to Jev | SPEC R23, `jev::request` and `jev::state_text`; the privacy test in `tests/pick_cli.rs` |
