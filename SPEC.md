# remuda behavior specification

remuda is a multi-account and session manager for coding agents: a TUI, plus a few subcommands for
direct use from the shell. The main workflow is: **check each account's usage → pick an account →
start or resume a session in a directory.**

remuda does not take over the shell: it injects no shell functions and does not change what typing
`claude` directly does (that is the native login). Named accounts are used only through remuda (the
TUI or `remuda run`).

This document states remuda's commitments. Changing any behavior described here is a breaking
change: this file and `tests/` must be updated in the same commit. Tests reference entries by
anchor (e.g. `R2`). Entries marked **[unverified]** must be confirmed experimentally before they are
implemented.

Entries are numbered in the order they were added, and the numbers never change, since tests cite
them. By area:

| Area | Entries |
| --- | --- |
| Foundations | [R1](#r1-model) model · [R2](#r2-home-path-invariant) home path invariant · [R3](#r3-registry) registry · [R4](#r4-provider-contract) provider contract · [R12](#r12-symlinks-in-homes) symlinks in homes · [R13](#r13-write-boundary) write boundary · [R15](#r15-testing) testing |
| Commands and launching | [R5](#r5-commands) commands · [R6](#r6-launch-run-and-launches-from-the-tui) launch · [R14](#r14-add-name-path) `add` · [R14a](#r14a-remove-account) `remove` · [R16](#r16-tui-actions) TUI actions · [R17](#r17-codex) Codex |
| Sessions | [R7](#r7-running-sessions) running sessions · [R8](#r8-session-index) session index · [R9](#r9-session-attribution) attribution |
| Accounts view | [R10](#r10-usage) usage · [R10a](#r10a-identity) identity · [R11](#r11-checks-in-the-accounts-view) checks · [R22](#r22-account-configuration-tui) account configuration |
| Across accounts | [R18](#r18-shared-configuration) shared configuration |
| Statistics | [R20](#r20-token-statistics) token statistics and cost |
| Privacy | [R21](#r21-private-mode-tui) private mode |
| Recommendation | [R23](#r23-recommendation-pick) `pick` |

How the entries map onto the code is described in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## R1. Model

- **Provider**: an agent CLI. `claude` is fully supported. `codex` supports accounts, identity,
  usage, the session index, token statistics, launch, resume, and fork; running sessions (R7),
  shared configuration (R18), and the configuration pane (R22) are claude-only (R4, R17).
- **Account**: `(provider, name, home)`. `home` is the provider's isolation directory, or the
  special value `default`.
- `name` matches `[A-Za-z0-9_-]+` and is unique within a provider. On the command line an account
  is referenced as `name` or `provider:name`; if a bare `name` exists under more than one provider,
  remuda reports an error listing the candidates rather than guessing.
- The bare name `default` always means `claude:default` (existing behavior since M0; adding codex
  must not make `remuda run default` ambiguous). The codex native login is written `codex:default`.
  Any other bare name that exists under more than one provider is still an error.
- The name `default` is reserved within each provider and denotes the agent's native login, used
  when no isolation variable is set (`home = "default"`). No other name may use `home = "default"`.
  Each provider's `default` exists implicitly and need not be registered.

## R2. Home path invariant

Basis (Claude Code 2.1.280 source, verified against the macOS Keychain: the hash of the original
path string matches an existing entry, and the same path with a trailing `/` appended does not): the
macOS Keychain entry is named `Claude Code-credentials-<sha256(NFC(CLAUDE_CONFIG_DIR))[0:8]>`, with
no suffix when `CLAUDE_CONFIG_DIR` is unset. Any change to the path **string** is equivalent to
switching to a different, logged-out account.

- remuda stores the original home string and passes it to the agent byte-for-byte at launch: no
  rewriting, no `realpath`, no adding or removing a trailing `/`. At registration the path must be
  absolute and in NFC; `~` is expanded exactly once, at the moment of registration.
- remuda **never moves, renames, or deletes** any home directory.
- Claude's `default` is represented by **not setting** `CLAUDE_CONFIG_DIR`: when the variable is
  set, `.claude.json` is read from inside that directory, whereas the native login's is at
  `~/.claude.json`. When launching `default`, the variable must be removed from the child process
  environment, even if remuda itself runs in an environment where it is set (for example, when the
  TUI is opened inside a claude session).
- `CLAUDE_SECURESTORAGE_CONFIG_DIR` decouples the Keychain key from the directory (undocumented).
  remuda does not use it; it is recorded here only as a future escape hatch for relocating homes. If it
  is already set in the environment at launch, remuda warns and leaves it unchanged.

## R3. Registry

- `config.toml` under `$REMUDA_HOME` (default `~/.remuda`) is the single source of truth:
  ```toml
  [[account]]
  provider = "claude"
  name = "work"
  home = "/Users/you/.claude-work"

  [[account]]
  provider = "claude"
  name = "personal"
  home = "/Users/you/.claude-personal"
  share = false             # optional: opt this account out of shared configuration (R18)

  [share.claude]
  from = "default"          # optional: the account whose configuration is shared (R18)

  [prices."claude-opus-4-6"]  # optional: USD per million tokens, instead of the built-in price (R20)
  input = 5
  output = 25
  cache_read = 0.50
  cache_write_5m = 6.25
  cache_write_1h = 10

  [pick]                    # optional: what `remuda pick` may recommend (R23)
  exclude = ["claude:work"] # never recommended; write accounts as provider:name
  prefer = ["claude:personal"]  # the rules' last tie-break
  min_headroom = 10         # percent left required on every window that applies
  stale_after = 120         # minutes after which cached usage is stale
  notes = "Keep claude:personal for long refactors."   # sent to Jev (R23)

  [pick.claude]             # also [pick.codex]
  models = ["claude-opus-5-5", "claude-fable-5-1"]     # in order of preference
  efforts = ["medium", "high", "xhigh", "max"]         # from low to high
  default_effort = "high"
  ```
- Homes created by `setup` live at `$REMUDA_HOME/homes/<provider>/<name>`; homes registered with
  `add` stay where they are. `homes` and `homes/<provider>` are real directories: `setup`
  creates them as needed and refuses one that is a symlink or not a directory (R13).
- Writes are atomic (temporary file + rename) and preserve the user's comments and unknown keys
  (the comments of an account that `remove` deletes go with it, R14a). If `config.toml` is a
  symlink, writes go through the symlink.
- Loading validates strictly: an invalid name, a duplicate name, a claimed `default`, a named
  account whose `home` is not an absolute path, `share` on a codex account, a `[share.claude] from`
  that names no claude account, a `[prices."<model>"]` that is not a table, has a key other than
  `input`, `output`, `cache_read`, `cache_write_5m`, and `cache_write_1h`, lacks `input` or
  `output`, or has a price that is not a number from 0 to 1,000,000, a `[pick]` with a key other
  than those above, an `exclude` or `prefer` entry that does not resolve as in R1, a
  `min_headroom` that is not an integer from 0 to 100, a `stale_after` that is not a positive
  integer, `notes` longer than 4000 characters, a model not matching `[A-Za-z0-9._:-]+` or
  starting with `-`, an effort not matching `[a-z]+`, a duplicate model or effort, a claude model
  or effort that is a claude subcommand, or a `default_effort` not among `efforts`, and similar
  problems are all reported as errors naming the file; remuda neither guesses nor skips. A bare
  name in `[pick]` resolves as in R1, so it becomes an error once another provider has an account
  of that name: `add` and `setup` refuse such an account (R14), and `provider:name` avoids it.
- Runtime state (index cache, statistics cache, launch log) lives in `$REMUDA_HOME/state/` and may
  be deleted and rebuilt at any time.
- Runtime state is the user's alone: the launch log holds the arguments of every launch, prompts
  among them (R6), and the caches hold titles and directories. `state/` is created with mode
  0700 and its files with mode 0600. A `state/` or a launch log from before that the group or
  others could access is tightened by the next write there, and a cache is always replaced by a
  file of mode 0600; nothing is ever loosened. Two modes are left as the user has them: that of
  the directory a `state` symlink points at, and that of the file a symlink in `state/` points
  at; both are written through. The regular files remuda owns by name in a directory reached
  through a `state` symlink (the caches, the launch log) are its own all the same: created
  with mode 0600, and the log tightened to it. `config.toml` keeps the mode it has; a new
  one gets the default mode (the umask's).
- Nothing is appended to a launch log that the group or others can still access (any
  permission bit of theirs) once remuda has tried to tighten it: a log remuda cannot change
  the mode of (it belongs to another user), or a log that is a symlink to such a file, whose
  mode remuda does not change. Nor is anything appended to a launch log that is not a regular
  file (a FIFO, a socket, or a device would hand the line to whoever reads it); the log is
  opened without blocking, so a FIFO in its place does not hold up the launch. The type and
  the mode are read from the open file right before the line would be written. This is a
  launch log that cannot be written: a warning naming the file
  and its mode, the launch goes on (R6), and the session is attributed as R9 does without a
  record. Tightening `state/` itself is best effort and stops no write: a directory's mode
  does not give away the contents of a file in it, and those files are 0600, or, for the log,
  not written.

## R4. Provider contract

Each provider declares the following capabilities. A missing capability is shown as unavailable in
the UI, not reported as an error:

| Capability | claude | codex |
| --- | --- | --- |
| Isolation variable / `default` semantics | `CLAUDE_CONFIG_DIR` / must be unset | `CODEX_HOME` / unset (explicitly setting it to `~/.codex` is equivalent to leaving it unset; verified) |
| Identity | `claude auth status --json` (R10a) | `codex login status`: login method only (ChatGPT / API key / not logged in); the email and plan come from the live query (`account/read`, R10, R10a); `auth.json` is not read (it holds credentials) |
| Usage | cached `cachedUsageUtilization`; live `claude -p /usage` (R10) | cached: the rate limits codex records in its rollouts; live: `codex app-server` `account/rateLimits/read` (R10) |
| Session index | `projects/*/*.jsonl` (R8) | `sessions/YYYY/MM/DD/rollout-*-<id>.jsonl` (R17) |
| Running processes | `claude agents --json` (R7) | no machine-readable source (`codex agents` is interactive only): not supported |
| Attribution | pre-assigned `--session-id` + `history.jsonl` (R9) | sessions are stored per `CODEX_HOME`: the home containing the rollout is the owner, exactly |
| Launch / resume / fork | `claude` / `--resume <id>` / `--fork-session` | `codex` / `codex resume <id>` / `codex fork <id>`, working directory via `-C <dir>` |
| Login | `claude auth login` | `codex login` |

- Confirmed for codex (0.155.1, source and experiment): `auth.json` and `sessions/` both live under
  `CODEX_HOME`; an empty `CODEX_HOME` means logged out. Credentials are stored in
  `CODEX_HOME/auth.json` by default; when the keyring is used instead, the key is the first 16 hex
  digits of the sha256 of the **normalized** path, so different spellings of the same directory make
  no difference to codex (remuda still applies R2's byte-for-byte rule to codex; it is simply no
  longer a necessary condition).
- `codex app-server` (marked experimental; verified on 0.155.1, field names from
  `codex app-server generate-json-schema`): JSON-RPC over stdio, one JSON message per line. The
  client sends `initialize` (`{"clientInfo": {"name", "version"}}`) and the `initialized`
  notification, then its requests; responses carry the request's `id`, may arrive in any order,
  and are interleaved with notifications. It honors `CODEX_HOME`.
  - Once its stdin closes, pending requests go unanswered, and it exits only after its startup
    completes. So remuda keeps stdin open until every answer has arrived, then closes it.
  - On timeout, its whole process group is terminated: an npm or bun install is a node shim whose
    child is the real server.
  - Its startup does what launching Codex does: it may refresh a stale login token at the
    provider, contacts the provider's and plugin endpoints, and writes Codex's own state into the
    home (SQLite databases, `installation_id`, `skills/.system`, `.tmp/plugins-clone-*`, `tmp/`);
    for a logged-in home it takes about 1.5 s, against 0.05 s and no network for
    `codex login status`. That is why remuda runs it only for explicit live queries (R10), never
    for `list` or the identities of the accounts view.
  - remuda calls only `initialize`, `account/rateLimits/read`, and `account/read`, never a method
    that changes anything (such as `account/rateLimitResetCredit/consume` or `account/logout`).
- Codex's `$CODEX_HOME/<name>.config.toml` is a configuration layer under the same login, **not**
  account isolation; remuda does not treat it as an account.

## R5. Commands

```
remuda                                             open the TUI
remuda run [<account>] [args]                      launch the agent under an account (R6); without
                                                   an account, open the TUI picker (R16)
remuda usage [<account>] [--live] [--timeout S]    per-account usage as plain text (R10)
remuda list [--timeout S]                          accounts, login identity, home (R10a)
remuda sessions [--limit N]                        recent sessions: time, account attribution,
                                                   title, cwd (R8, R9)
remuda stats [<account>] [--period P]              tokens and estimated cost per account and
                                                   model (R20)
remuda add [--provider P] <name> <path>            register an existing home directory (R14, R17)
remuda setup [--provider P] <name> [--email E]     create a new home, link it to the source of
                                                   shared configuration if there is one (R18),
                                                   and run the agent's login (`claude auth login`,
                                                   `codex login`; R17)
remuda remove <account>                            unregister an account; its home is left in
                                                   place (R14a)
remuda pick [--provider P] [--live] [--timeout S]  recommend the account, model and effort to
       [--offline] [--json | --print-request]      launch now; with --run, launch it (R23)
       [--run [-- <args>]]
remuda help [<command>]                            help for remuda or a command
```

- There is no shell integration, global routing, or per-directory binding.
- `run` passes `args` through to the agent unchanged: every token after the account, in order,
  a `--` included, also one right after the account (`remuda run work -- -x` gives the agent
  `-- -x`). Only a `--` before the account is remuda's own (`remuda run -- -x`, for a
  registered account whose name starts with `-`). Because `-h` and `--help` after `run` go to
  the agent too, the help of `run` itself is `remuda help run`.
- `--provider` is `claude` (the default) or `codex`; for `pick`, a filter without default.
  `--timeout` is in seconds, per query: `usage --live` and `pick --live` default to 90, `list` to
  15.

## R6. Launch (`run` and launches from the TUI)

- Child process environment = current environment + the account's isolation variable (or, per R2,
  with that variable removed).
- **remuda pre-assigns the ID of a new session**: it generates a UUID, writes the launch record to
  the launch log first, and then starts claude with `--session-id <uuid>`. Basis (verified on
  2.1.280): in `-p` mode the returned `session_id` equals the UUID passed in, and the transcript
  file is named after that UUID (`<uuid>.jsonl`).
- `--session-id` is injected only when the arguments mean "start a brand-new session". Every call
  that is not a new session is passed through unchanged, without injection, for example:
  - the arguments contain a claude subcommand name (`agents`, `auth`, `attach`, `logs`, `stop`,
    `rm`, `respawn`, `doctor`, `mcp`, `plugin`, `project`, `update`, etc.; the list tracks claude
    versions). This is not limited to the first positional argument: telling a prompt apart from an
    option value would require claude's full option table, so if any argument equals a subcommand
    name, nothing is injected. The cost is that prompts such as `-p update` lose attribution;
  - arguments with resume, continue, or attach semantics: `--resume` / `-r`, `--continue` / `-c`,
    `--session-id`, `--teleport`, `--from-pr`, `--cloud`;
  - `--help` / `-h`, `--version` / `-v`.
  When in doubt, nothing is injected. A `--` in the arguments changes none of this: what follows
  it is classified like what precedes it (`-- --resume <id>` is passed through without
  injection). The injected `--session-id <uuid>` goes after the user's arguments, or right
  before their first `--` when there is one, so that it stays an option.

  ```text
  claude arguments
    ├─ any argument is a subcommand name, -h/--help or -v/--version
    │    ──► not a session: passed through; no ID, no shared configuration (R18)
    ├─ no resume, continue or attach option, and no --session-id
    │    ──► new session: --session-id <uuid> injected
    ├─ --resume <id> and --fork-session, with no --session-id and no other resume option
    │    ──► fork: --session-id <uuid> injected, fork_of = <id>
    └─ anything else (resume, continue, attach, other fork forms)
         ──► existing session: passed through; the ID is logged when the arguments name it
  ```
- A resume with `--fork-session` produces a new session ID. For the form `--resume <id>
  --fork-session` (an explicit ID, with no user-supplied `--session-id`), remuda injects a
  pre-assigned `--session-id <uuid>`; `-c --fork-session`, `--resume --fork-session` without an ID,
  and similar forms get no injection, and their `session_id` is recorded as unknown. When injecting,
  the launch log records the new ID, and records the forked ID as `fork_of`. Basis (verified on
  2.1.281): `--resume X --fork-session --session-id Y` produces a session whose ID is Y, in the file
  `Y.jsonl`.
  For calls that are passed through, a session ID that can be read from the arguments
  (`--resume <id>`, `--session-id <id>`) is still recorded in the launch log; otherwise it is
  recorded as unknown. Rationale: users may alias `claude` to `remuda run <account>`, in which case
  calls such as `claude agents --json` also pass through `run`.
- Resuming a session: `claude --resume <id>`; the ID is known and is logged directly. The child's
  cwd is set to the `cwd` of the **last** record in the transcript; if that directory no longer
  exists, remuda reports an error rather than guessing. Basis (verified on 2.1.280): `--resume <id>`
  finds the session from any directory and appends new records to the original transcript, but
  those records carry the new `cwd`, and tools run in the new directory. A wrong cwd produces no
  error; the session simply continues in the wrong directory, so remuda must set it correctly.
- Optionally, `-n <name>` gives the session a display name (shown in claude's prompt box, the
  `/resume` list, and the terminal title).
- `remuda run` replaces itself via exec: the ID is already in the log, so there is no need to stay
  resident.
- `run` is a fast path: it reads only `config.toml`, does not scan sessions, invoke claude, or
  initialize the TUI; its overhead is measured in milliseconds.
- Launching from the TUI: leave the alternate screen and restore the terminal, spawn the child
  process and wait for it in the foreground, then return to the TUI and refresh.
- claude writes no transcript for a session (new or forked) that exits before its first message is
  sent, so the launch log may contain IDs with no corresponding index row; this is expected. Also,
  a `--resume` that sends no message still appends several bookkeeping records to the original
  transcript (verified on 2.1.281), so "resuming just to look" is not a read-only operation.
- Shared configuration (R18) is injected into session invocations only, using the classification
  above: any subcommand name, `--help` / `-h`, or `--version` / `-v` means "not a session". New
  sessions, resumes, continues, forks, and `-p` runs are sessions.
- Launch log `state/launches.jsonl`: one line per launch, containing time, account, home, cwd,
  args, and session ID (or unknown). `args` are the user's arguments as the agent got them,
  without what remuda injected: a prompt given on the command line is in the log, which is why
  the log is readable by the user alone (R3).
- **[unverified]**: whether variables inherited when launching from within a claude session, such
  as `CLAUDECODE`, `CLAUDE_CODE_SESSION_ID`, and `CLAUDE_CODE_MESSAGING_*`, affect the child claude;
  if they do, define a list of variables to strip.

## R7. Running sessions

- Primary source: run `claude agents --json` for each account (its help text says it is intended
  for scripts and does not need a TTY; measured at about 0.14 seconds). It lists interactive and
  background sessions: `pid, cwd, kind, startedAt, sessionId, name, status`.
  Verified: the output is scoped per `CLAUDE_CONFIG_DIR`; for each account, the number of entries
  matched the number of `sessions/*.json` files in that account's home.
- Fallback source (when the command fails or the version is too old): `<home>/sessions/<pid>.json`.
  remuda then determines liveness itself: the pid exists **and** the process start time matches
  `procStart` (`procStart` is UTC while local `ps` prints the local time zone; normalize before
  comparing). The `*.key` files in the same directory are **never read**.
- `agents --json` has two kinds of entries (verified on 2.1.281):
  - interactive: `pid, cwd, kind: "interactive", startedAt, sessionId, name, status`;
  - background: `id` (8-character short ID), `cwd, kind: "background", startedAt, sessionId, state`
    (e.g. `blocked`, `stopped`), with **no `pid`** and no `status`. `--all` also lists stopped
    background sessions.
  Both kinds must be accepted; a missing `pid` is not a reason to discard an entry. `stopped` and
  `done` are treated as inactive (hidden by default; cannot be stopped; can be removed); any other
  state (`blocked`, etc.) is treated as running.
- Background sessions are operated on by short ID: `claude attach <id>`, `claude logs <id>`,
  `claude stop <id>`, `claude rm <id>`. remuda only invokes these commands and does not reimplement
  them. `logs` emits raw terminal sequences (ANSI), which must be stripped or converted before
  display. The conversion is bounded, so that no output can exhaust memory: only the last 4 MB of
  the output are read (the text then says first how much was left out), and they are drawn on a
  screen of at most 1000 columns, 100,000 rows, and 2,000,000 cells. A cursor position past an
  edge is at the edge, a character past the last column is dropped, and beyond the last row or
  the cells the earliest rows are dropped: a long log keeps its end.
- Unknown fields are ignored; an unknown `status` is displayed as-is.

## R8. Session index

- Claude transcripts: `<home>/projects/*/*.jsonl`, top level only (subdirectories contain subagent
  transcripts). Session ID = file stem.
- The `projects` directories of several homes may be symlinked to the same place: deduplicate by
  the realpath of `projects`, scan a shared store only once, and record which accounts share it.
  (This realpath is used only for deduplication and does not conflict with R2.)
- For each transcript, record: session ID, title, first user text, `cwd_first` / `cwd_last`, first
  and last `timestamp`, `entrypoint`, size, mtime, and the offset scanned so far.
  - Title: the last `{"type":"ai-title","aiTitle":…}` record; if there is none, the first user text.
    The following do not count as user text: user records with `isMeta` (such as the
    `<local-command-caveat>` boilerplate), local-command records (text starting with
    `<command-name>`, `<command-message>`, `<local-command-stdout>`, or `<local-command-stderr>`,
    e.g. a `/clear` at the start of a session), and user records that carry only a tool_result.
  - cwd: taken from the records' `cwd` field; the directory name is not decoded back into a path
    (the encoding is lossy). After a cross-directory resume the first and last cwd differ (R6); both
    list display and resume use `cwd_last`.
- Scale (a measured corpus, 2026-09-24): 6067 files, 6.4 GB, p95 4 MB, largest 37 MB. Therefore:
  - **The first scan reads only the head and tail**: the first 64 KB yields `cwd_first`, the first
    timestamp, the first user text, and `entrypoint`; the last 64 KB yields the title, `cwd_last`,
    and the last timestamp. Basis: of the 60 most recent files in that corpus, 31 had an
    `ai-title`, and in every one the last `ai-title` fell within the final 64 KB; there were no
    `summary` records.
  - **Subsequent scans are incremental**: transcripts are append-only (verified: the hash of the
    first 1 MB is unchanged before and after a file grows). The offset is cached; when a file has
    grown and its mtime is not earlier than the cached one, only the new bytes are parsed. The whole
    file is rescanned when it shrinks, when its size is unchanged but its mtime changed, when its
    mtime moves backward, or when its inode changed (the file was replaced).
  - When the tail window contains no complete record (the file ends with one very long line), the
    window grows by ×4, up to 4 MB.
  - Known limitation: when a large file's only `ai-title` is in the middle and the tail window does
    contain complete records, the title is not found and the first user text is used instead.
  - Only complete lines are parsed: a final line still being written is left for the next scan.
    Lines cut by a window boundary are discarded.
- Cache: `$REMUDA_HOME/state/index.json`, with a schema version; on a version mismatch it is
  rebuilt. Written atomically; may be deleted at any time (R3).
- The index is built in the background: the UI does not wait for it and shows progress and the rows
  obtained so far while it builds.
- The list hides "noise" sessions by default: those whose first user text starts with
  `<teammate-message` (teammates in agent teams) or whose `entrypoint` starts with `sdk`. In the
  TUI, `a` toggles showing everything, and the status bar states `showing N of M`.
- Search matches only the displayed columns: title (the ai-title or the fallback user text),
  `cwd_last`, and account.
- The preview is read only when a row is selected: the most recent N user / assistant texts are
  read from the tail; `tool_use` is shown as `[tool: <name>]`, and thinking and tool-result bodies
  are skipped. Consecutive assistant records with the same `message.id` (claude writes one per
  content block) are merged into a single message.
- Parsing is best-effort: malformed lines are skipped, unknown formats degrade to missing fields,
  and it never crashes.

## R9. Session attribution

Transcripts carry no account information, and shared `projects` directories mean the path cannot
distinguish accounts either. Attribution merges the following sources:

1. The remuda launch log (R6): sessions launched through remuda have pre-assigned IDs, so their
   attribution is exact.
2. Running sessions (R7): the account under which the session is running or has run (including
   stopped and finished background sessions).
3. `sessionId` values appearing in each home's `history.jsonl`: these fill in history from before
   remuda.

When the `history.jsonl` files of two accounts resolve to the same realpath, the file cannot
distinguish accounts and is not used for attribution. When `ps` is unavailable (so the process
start time cannot be obtained), the fallback source for running sessions claims nothing.

A session may belong to more than one account (started under A, resumed under B). **Unattributed is
a normal state**: in a measured corpus (2026-09-24), about 45% of all top-level transcripts in a
shared store could be attributed via `history.jsonl` (2735 / 6016), and one sampled SDK session
appeared in no `history.jsonl`.

## R10. Usage

- For each account, show its usage windows with utilization and reset time: claude's 5-hour
  window, weekly window, and per-model weekly limits; codex's windows under the same labels (below).
- The reset times of all accounts are drawn on a single timeline, so that the account with the most
  headroom right now is easy to pick.
- Two data sources:
  1. **Cached** (instant, default): what the agent itself recorded locally. Claude: the
     `cachedUsageUtilization` in `.claude.json`, which has `fetchedAtMs` and ISO-format
     `resets_at`. Codex: the rate limits in its rollouts (below). The cache time is always shown.
  2. **Live** (refreshed on demand), run in the account's environment, so that the agent itself
     queries with that account's credentials. Accounts are queried in parallel; the default
     timeout is 90 seconds.
     - Claude: `claude -p /usage --no-session-persistence`. Basis (verified on 2.1.280): `/usage`
       is a local command that supports non-interactive use and does not call the model (0
       tokens, no cost); with `--no-session-persistence` it leaves no transcript. It takes
       anywhere from 2 to 20 seconds (it scans the local session history).
     - Codex: one `codex app-server` (R4) per account, which is sent both
       `account/rateLimits/read` with `{"excludeResetCreditDetails": true}` and `account/read`
       with `{"refreshToken": false}`, and answers both (about a second or two). The rate limits
       are the usage; `account/read` also gives the account's email and plan (R10a).
- Claude's live source produces only human-readable text (`--output-format json` merely places the
  same text in `result`). remuda parses only the `Current session` / `Current week (…)` lines; if
  it cannot parse them it displays the text as-is and never crashes. This format is not a public
  interface and may change between versions.
- **Codex, cached** (verified on 0.155.1 against 1456 real rollouts): each model turn appends
  `{"timestamp", "type": "event_msg", "payload": {"type": "token_count", "rate_limits": {…}}}`.
  `rate_limits` (may be null) holds `limit_id`, `limit_name`, and the windows `primary` and
  `secondary`, each `{"used_percent", "window_minutes", "resets_at"}` (Unix seconds, may be null)
  or null. `limit_id` is `codex` for the account's general limit, another ID for a per-model limit
  (e.g. `codex_bengalfox`, named `GPT-5.3-Codex-Spark`), and null in 2025 rollouts (windows of 299
  and 10079 minutes). The cached value is the general limit (`limit_id` `codex` or null, with a
  usable window) of the newest such record by `timestamp`, in `<home>/sessions/**/rollout-*.jsonl`
  and `<home>/archived_sessions/rollout-*.jsonl`; the cache time is that record's `timestamp`.
  Per-model limits are shown live only. The scan is bounded: rollouts newest first by mtime, at
  most 16, stopping at the first whose mtime is older than the best record found (none of its
  records can be newer); each is read from its end, 64 KB growing ×4 up to 1 MB.
- **Codex, live**: the general limit is `rateLimitsByLimitId.codex`, else `rateLimits`; every
  other entry of `rateLimitsByLimitId` is a per-model limit, named by its `limitName`, else its
  key. Windows are `{"usedPercent", "windowDurationMins", "resetsAt"}`. An error response to
  `account/rateLimits/read` (a logged-out home answers -32600 "codex account authentication
  required to read rate limits") is that account's failure; a response without a usable window is
  shown as is. `account/read` is extra: if it fails, is not recognized, or names no logged-in
  account, the usage is still shown, without an email or plan.
- **Codex windows** are labeled by duration, not position (a Pro plan's `primary` window is weekly,
  with no `secondary`): minutes rounded to whole hours; 5 hours is `Session`, 168 hours is
  `Week (all models)`, any other duration `<N>h window` (`<N>d window` for whole days). A
  per-model limit's name replaces `all models` in `Week (<name>)` and is appended to the others
  (`Session (<name>)`). A window without a numeric percentage and a positive duration is dropped.
  Rows: the general limit first, then per-model limits, each by window length.
- Codex credits, reset credits, spend control, and upsell data are not shown (R4).
- A live query does **not** refresh the local cache (verified for claude: `fetchedAtMs` is
  unchanged after consecutive runs); the two sources are displayed separately.
- remuda makes no network requests of its own, except the one request of `remuda pick` (R23), and
  never reads credentials to call the agent's usage endpoint directly.
- The cache formats are undocumented and parsed on a best-effort basis: unrecognized formats
  degrade to missing rows, and parsing never crashes.
- `remuda usage [--live]` prints the same information as plain text for direct use from the shell;
  a codex account's live header also shows its email and plan.
- The live sources provide no severity: 75% is marked as a warning and 90% as critical. Claude's
  live reset times are localized text; the timeline makes a best effort to parse them into instants
  and otherwise uses the cached reset time of the same limit. Codex reports instants.

## R10a. Identity

- Source: `claude auth status --json`, run in the account's environment. Verified output fields:
  `loggedIn, authMethod, email, orgName, subscriptionType, configDirectory, projectsDirectory`.
- If the command fails, fall back to reading `oauthAccount` from `.claude.json` (for `default`,
  `~/.claude.json`).
- `configDirectory` can be used to confirm that the path remuda passed actually took effect.
- Codex (verified on 0.155.1): `codex login status`, run in the account's environment. It prints
  to stderr, and exits 1 when not logged in: `Logged in using <method>` (an API key's masked key
  is not kept) or `Not logged in`. That is the login method only, no email; `auth.json` is not
  read (it holds credentials).
- A codex account's email and plan appear after a live query (R10): its `account/read` gives
  `{"type": "chatgpt", "email", "planType"}` (`unknown` counts as no plan; codex has no
  organization). A later identity refresh shows the login method again.

## R11. Checks in the accounts view

- `ANTHROPIC_API_KEY` is set: warning (it overrides `/login` for every account).
- The home contains symlinks whose targets do not exist.
- `projects` is shared by several accounts and one of the participating accounts does not set
  `cleanupPeriodDays`: warning (the default 30-day cleanup deletes everyone's sessions in the
  shared store).
- A registered home does not exist, or appears not to be logged in.
- Shared configuration (R18): the source account does not exist or its home is missing; a home
  shares some but not all of `CLAUDE.md`, `skills`, `commands`, `agents`, `rules` with the source
  through symlinks (those items may load twice); a home that shares `projects` with the source
  through a symlink but not `agent-memory`, when the source has one and a settings file of the
  source or the home chooses `autoMemoryDirectory` (a launch then does not redirect memory, so
  the memory of user-scope subagents is not shared); rules of the source whose frontmatter has
  `paths`, when some member gets
  the rules at launch (they are not applied there); an enabled plugin whose install path does not
  exist; an `installed_plugins.json` whose format is not recognized; authentication keys in the
  source's settings that are withheld from the settings injected at launch (said of the
  accounts that get their settings that way when some member links `settings.json` instead,
  and not said when every member does).
- The links of a member's home (R18), for members only (an account with `share = false` and the
  source are not checked):
  - `.claude.json`, `history.jsonl`, or `sessions` is a symlink that resolves to the same item
    of another registered account, the source included: warning, with what breaks. This is
    checked for every member whose home exists, also when the source's home is missing (the
    checks below need the source's home). A shared
    `.claude.json` mixes up the two logins (claude does not fetch the account's profile again
    within 24 hours); a shared `history.jsonl` takes the attribution from the sessions of a
    shared store (R9); a shared `sessions` makes running sessions impossible to tell apart by
    account (R7).
  - The home shares `projects` with the source but not `file-history`, when the source has one:
    after resuming another account's session, `/rewind` does not find its file backups. The
    message says how to link it.
  - The home does not share `projects` with the source, when the source has one: the account
    does not see the sessions of the source's store and cannot resume them (R16). This is a
    notice that the account is not sharing sessions, not an error: the injected memory
    locations (R18) still apply. The message says to link `projects`.
  - The source's `installed_plugins.json` records an `installPath` under `<home>/plugins/` (the
    home's path as registered) while the home's `plugins` resolves to the source's: that link
    cannot be removed without breaking those plugins for every account. The message gives
    their number. Nothing is reported when the file is not recognized (reported above).
  - The home's `settings.json` resolves to the source's, and the source's settings have keys
    that R18 never injects (authentication): warning that this account reads them through the
    link, naming the keys, never their values (each session launch says it too, R18). The
    message says to remove the link, so that the rest is injected, or to move those settings
    out of the source's `settings.json`.
  The messages about items shared in part (`CLAUDE.md` and the other instruction items,
  `file-history`, `agent-memory`) suggest linking the rest.

## R12. Symlinks in homes

- remuda detects symlinks in a home that point elsewhere and displays them as `-> <target>`.
- remuda does not create, delete, or modify any symlink in a home, with one exception: the links
  `setup` makes in the home it has just created, before the first login (R18). (The item links
  remuda keeps under `$REMUDA_HOME/shared/`, R18, are its own.) Any future write operation that
  encounters a symlink must either write through it or refuse; it must never replace the symlink
  with a private copy.
- Accounts share one session store and their configuration through symlinks in their homes:
  `setup` makes them in a home it creates, and the user makes them in any other home (R18). What
  a member's home does not link is injected at launch instead, as a fallback (R18); what it
  links is detected so that nothing is injected twice.

## R13. Write boundary

The complete set of remuda's write operations:

- `$REMUDA_HOME/config.toml`, `$REMUDA_HOME/state/**`, `$REMUDA_HOME/shared/**` (R18).
  Where the user put a symlink: a `config.toml`, a `state`, or a file in `state/` that is a
  symlink is written through (R3); where it points, remuda touches only its own files (the
  registry, the caches, the launch log, the settings files of R18). Modes there (R3): the
  directory a `state` symlink points at keeps its mode, and so does the file that a
  `config.toml` or a file in `state/` that is itself a symlink points at; a regular file
  remuda owns by name in a directory reached through a `state` symlink is still created with
  mode 0600, and the launch log there tightened to it. A launch log that is a symlink to a
  file others can access, or that is not a regular file, is not appended to (R3).
  Below `shared`, remuda also replaces and removes links and rule copies, so it writes there
  only below real directories: a `shared` or `shared/claude` that is a symlink, or that exists
  and is not a directory, is refused before anything is created, replaced, or removed, and the
  launch goes on without shared instructions (R18). What remains is the instant between that
  check and the write (R18).
- `$REMUDA_HOME/homes/<provider>/<name>/` created by `setup`: the directory itself and, for a
  claude account that is a member of `[share.claude]`, the symlinks of R18 in it, made once,
  while the directory is still empty, before the account is registered and logged in (login is
  performed by `claude auth login` itself, optionally with `--email` prefilled). `setup` never
  writes through a symlink below `$REMUDA_HOME`: `$REMUDA_HOME` is opened as given (the path
  is the user's, a symlink or not), and `homes`, `homes/<provider>`, and the new home are each
  opened relative to the directory above, without following a symlink. A `homes` or
  `homes/<provider>` that exists and is a symlink or not a directory is an error, found before
  anything is created and again when it is opened: `setup` then creates nothing, registers
  nothing, and runs no login. A missing level is created. The new home's mode is set, and its
  links are made, through the descriptor of the directory `setup` created, not through its
  path.

remuda never writes credentials, `.claude.json`, the Keychain, transcripts, `history.jsonl`, or
`*.key` files, and never writes into any home directory: not into a home registered with `add`,
not into the native login's, and not into a home `setup` created, apart from those links at
that one moment. It never
makes network requests of its own (live usage is queried by the agent itself; see R10), except the
one request of `remuda pick` to TypeSafe, made only with `TYPESAFE_API_KEY` set and `[pick]
notes` written (R23). Network use
and writes into a home by an
agent are the agent's own, and happen only in live queries (`claude -p /usage`, `codex app-server`;
R4, R10) or in the other agent commands remuda runs in an account's environment (`claude auth
status`, `codex login status`, which creates `tmp/`, a launch, a login).

## R14. `add <name> <path>`

- Registers an existing directory as an account: the path is normalized per R2 and then stored
  verbatim; the directory itself is not modified.
- The directory must exist. If it does not look like a home for the provider (Claude: neither
  `.claude.json` nor `projects/` is present), remuda warns but registers it anyway.
- It is an error if the name is already taken, if the same path is already registered under another
  name, if another spelling of the same directory (trailing `/`, a symlink, etc.) is already
  registered, or if `config.toml` would no longer load with the account (R3: a bare name in
  `[pick]` that it would make ambiguous). `setup` refuses the same, before it creates the home.
- Registering the native login's directory (equal to `$HOME/.claude` after normalization) is
  refused: that directory is `default`, and giving it another name would create a separate Keychain
  entry and read `.claude.json` from inside the directory, making it appear logged out.
- Registration only records a name; nothing is moved, copied, or created.

## R14a. `remove <account>`

- Unregisters an account: its `[[account]]` table is deleted from `config.toml`, atomically (R3).
  The home directory and everything in it are left untouched (R2); remuda prints the home's path
  and how to register it again (`remuda add`).
- The account is named as in R1 (`name` or `provider:name`; a bare name that exists under more than
  one provider is an error listing the candidates).
- `default` (`claude:default`, `codex:default`) is implicit and cannot be removed: error.
- Refused while `[share.claude] from` or `[pick]` (`exclude`, `prefer`) names the account: the
  registry would no longer load (R3).
  `from` must be changed or removed first; remuda does not edit it.
- The removed table's comments go with it: those inside it and the comment lines directly above
  its header. A comment block separated from the header by a blank line is kept (moved before
  the next table, or to the end of the file). Every other line is kept.
- State is not touched: the launch log keeps the account's lines (R3). Sessions in a store that no
  remaining account has drop out of the index at its next refresh; in a store shared with a
  remaining account they stay, and their launch-log attribution may still name the removed
  account, which can no longer be chosen to resume them.

## R15. Testing

Every test runs in a sealed sandbox: a fresh `HOME` and `REMUDA_HOME`, and a fake `claude` on PATH
that prints its own environment and arguments and returns fixtures for `agents --json`,
`auth status --json`, and `-p /usage`, and, when a test installs it, a fake `codex` that records
the same, answers `login status`, and acts as `app-server`: it reads JSON-RPC lines from stdin,
records them, answers `initialize`, `account/read`, and `account/rateLimits/read` from fixtures
with a notification before each answer, and exits when stdin closes; tests never touch real logins
or sessions. A fake `curl` is always first on PATH (`/usr/bin/curl` exists on macOS and Linux):
it records its arguments and standard input and answers from fixtures (a response body and
status, or a forced exit such as 28 for a timeout), so tests never reach the network, and
`TYPESAFE_API_KEY` is set only by tests that use a sentinel key and check it never shows in output
or in curl's arguments. Fixtures for transcripts, rollouts, `sessions/*.json`, `history.jsonl`, and
`.claude.json` are derived from real structures and anonymized. In library-level tests where
`FAKE_CLAUDE_OUT` / `HOME` are not set, the fake claude defaults to paths inside the sandbox.

## R16. TUI actions

- Every launch reuses the `run` path (R6): the same environment changes, `--session-id` injection,
  and launch log. The TUI first leaves the alternate screen and restores the terminal, spawns the
  child in the foreground and waits for it, and afterwards restores the TUI and refreshes the index,
  running sessions, and attribution.
- **Resume** (`Enter` in History / Live):
  - Account: if the session is attributed to exactly one account, that account is used; otherwise
    an account picker is shown (attributed accounts listed first).
  - Resuming an interactive session that is currently running is refused, stating the account and
    pid under which it runs (two processes writing the same session overwrite each other). This
    check must use data fresh **as of the moment before launch**: immediately before launching,
    `agents --json` (R7) is rerun for every account. If the query fails for any account and the
    fallback source cannot confirm either, every non-fork resume is refused ("cannot confirm that
    session <id> is not running") rather than treated as not running. Resume is also refused while the live data in
    the UI has not finished loading.
  - The object resumed is **the selected row** (its transcript path), not a fresh lookup by session
    ID: the same ID may appear in two stores.
  - Refused when the selected account's `projects` store (realpath) differs from the store holding
    the transcript: that account cannot find the session, and remuda does not copy sessions
    between stores (R13; a relay that did was removed, R19). Accounts that link `projects` to
    one store can each resume its sessions (R18); the message says to link the account's
    `projects`.
  - The cwd is `cwd_last` (R6); an error is reported if the directory does not exist.
  - `f`: fork (`--resume <id> --fork-session`, injecting a new ID per R6). A fork only reads the
    original session and writes under a new ID, so it is allowed even for a running session.
  - A background session that is no longer running is resumed from History as an ordinary
    transcript; in Live, `Enter` attaches (claude states that stopped sessions can be `attach`ed
    again).
- **New session** (`n` in Accounts): launches with the selected account in a directory. The
  directory defaults to the cwd at the time the TUI was opened, is editable (`~` is expanded), and
  must exist. An optional session name is passed to claude via `-n`. A session name that starts
  with `-` (including `--`) or is exactly a subcommand name is refused: the former would be parsed
  by claude as an option, and the latter would make R6's check skip ID injection.
- attach writes a launch log entry just like `run` (with `session_id` null); `logs`, `stop`, `rm`,
  and the `auth login` in setup do not.
- **New account** (`s` in Accounts): equivalent to `remuda setup` (R5).
- **Remove account** (`D` in Accounts): equivalent to `remuda remove` (R14a) after confirmation
  (`y` confirms; `Ctrl-P` toggles private mode, R21; any other key cancels); the prompt names the
  home that is kept. Refused on a `default` row. The account list is read again afterwards.
- **Background sessions** (in Live): `Enter` attaches (`claude attach <id>`, likewise suspending the
  TUI); `l` shows `claude logs <id>` in the preview pane; `x` stops and `D` removes a stopped
  session, both after confirmation (`y` confirms; `Ctrl-P` toggles private mode, R21; any other key
  cancels). Interactive sessions in Live are view-only.
- Session IDs and background short IDs passed to claude are validated first (session IDs must be
  UUIDs; short IDs must be 8 lowercase hex digits) and rejected if malformed, so that a file name
  or JSON field starting with `-` cannot be interpreted as an option.
- A child that exits abnormally (e.g. killed by SIGKILL) may leave the terminal in raw / no-echo
  mode: the TUI saves the termios state once on entry and restores that snapshot before handing
  over the terminal and on final exit.
- **`remuda run` without an account** (in which case no other arguments are allowed either): opens
  the TUI account picker; once an account is chosen, the TUI exits and execs claude as
  `remuda run <account>` would.
- **Configuration** (`p` or Space in Accounts): the selected account's configuration (R22).
- **Preview** (`p` or Space in History / Live): expands or collapses the preview (R8); `Enter`
  resumes or attaches, as above.

## R17. Codex

- Accounts: `remuda add --provider codex <name> <path>`, `remuda setup --provider codex <name>` (the
  new home is at `$REMUDA_HOME/homes/codex/<name>`, it is created empty (the links of R18 are
  claude's), and login uses `codex login`). The implicit
  `codex:default` is listed only when `~/.codex` exists or `codex` is on PATH.
- `run codex:<name> [args]`: sets or removes `CODEX_HOME` (same rules as R6), execs `codex`, passes
  arguments through unchanged, and **injects no** session ID (codex has no such option). The launch
  is still logged: new sessions and forks have a null `session_id` (codex chooses the ID), a fork
  records the forked ID as `fork_of`, and a resume records the resumed ID (only the forms
  `resume <id>` / `fork <id>` in the first position are recognized; when unsure, null is recorded).
- Titles in a shared store: within a single `session_index.jsonl`, the last line for each id wins;
  across several homes, the entry with the latest `updated_at` wins (lines without a time count as
  oldest; on a tie, the home later in the registry wins).
- When the `sessions` directories of several codex accounts resolve to the same realpath, a rollout
  does not uniquely determine its account: resume and fork must show a picker containing only those
  accounts (resume still requires confirmation) and must not default to the first one.
- The account picker and the new-session form remember the account itself (`provider:name`), not a
  list index; if the account list changes while they are open, the account is re-resolved by name,
  and the action is refused if it cannot be resolved. The picker lists only accounts of the same
  provider as the session.
- Session index: `<home>/sessions/**/rollout-*.jsonl` (excluding `archived_sessions`). Only the
  **first** `session_meta` is honored (a forked rollout also contains a second one from the parent
  session, which must be ignored); it supplies `id`, `cwd`, `originator`, and `source`. The title is
  the `thread_name` from the last line for that id in `<home>/session_index.jsonl`; if there is
  none, the first genuine user text is used (skipping injected blocks that start with a lowercase
  tag, such as `<environment_context>`, `<user_instructions>`, `<skill>`, as well as
  `# AGENTS.md instructions`; for VS Code's `# Context from my IDE setup:`, the text after
  `## My request for Codex:` is used).
  Rollouts are likewise append-only and use the same incremental scan (R8), except that the head
  window grows up to 1 MB: real rollouts carry 20–46 KB of `base_instructions` plus AGENTS.md
  before the user's first message, and the first genuine user text sits at a median offset of 94 KB
  (measured across 1441 rollouts).
- The 2025 legacy format (first line `{"id","timestamp","instructions"}`, with no `source` and no
  cwd) is still indexed; without a cwd it cannot be resumed.
- Noise: sessions whose `source` is `exec`, `{"subagent": …}`, and the like (that is, present and
  neither `cli` nor `vscode`) are hidden by default (toggled with `a`, as in R8); legacy-format
  sessions without a `source` do not count as noise.
- Resume / fork: `codex resume <id>` / `codex fork <id>` with `-C <cwd>`. The account is the one
  whose home contains the rollout: with a single home there is no picker, and the account cannot be
  switched (no other home contains the session); for shared stores, see above.
- **Running check**: codex has no source for running sessions, so remuda cannot confirm that a
  session is not running elsewhere. An in-place codex resume therefore **requires confirmation
  first** ("remuda cannot confirm that this session is not running elsewhere"; `y` resumes anyway,
  `Ctrl-P` toggles private mode (R21), any other key cancels); if the rollout file was written
  within the last 10 minutes, the prompt adds that it is still being written. Forks need no
  confirmation.
- Running sessions: not supported for codex; shown as unavailable in the UI. Identity and usage:
  R4, R10, R10a.

## R18. Shared configuration

Accounts share one session store and their configuration through symlinks in the member's home,
each pointing at that item of the source's home: `setup` makes them in a home it creates (R5,
below), and the user makes them in any other home. The login stays per account: the
credentials, `.claude.json`, `history.jsonl`, and `sessions` are never linked. A linked
`settings.json` is the source's file, shared whole, authentication settings in it included
(**Never injected: authentication**, below). Injection at
launch fills in what a member's home does not link, and is skipped for each component the home
already shares; it writes nothing into any home (R13). It is the fallback for a home without
the links, and it cannot share sessions: an account whose `projects` is not the source's does
not see the sessions in the source's store (R11, R16).

- **Source.** `[share.claude] from = "<account>"` in `config.toml` names the account whose home is
  the source of shared configuration (typically `default`, whose home is `~/.claude`). Without this
  table nothing is shared. The source must be a claude account; loading fails otherwise (R3).
- **Members.** Every other claude account, unless it sets `share = false`. The source account itself
  gets no injection. Codex accounts are not affected; `share` on a codex account is a load error
  (R3).
- **Links made by `setup`.** For a new claude account that will be a member (`[share.claude]`
  is set: a new account has no `share = false` and cannot be the source), `setup` makes
  symlinks in the home it has just created, after creating the directory and before registering
  the account and running the login:
  - One link for each item of this list that the source's home has, in this order: `projects`,
    `file-history`, `settings.json`, `CLAUDE.md`, `skills`, `commands`, `agents`, `hooks`,
    `plugins`, `rules`, `agent-memory`, `output-styles`, `keybindings.json`. An item the source
    does not have (missing, or a dangling link) gets no link and is listed on stderr; R11
    reports the ones that matter once the source has them.
  - `settings.json` is linked only when it sets no authentication at that moment. A home
    that links it reads all of it, so `setup` reads the source's `settings.json` first, and
    when it has any key that injection withholds (**Never injected: authentication**, below:
    the same rules), or cannot be read as a JSON object, it is not linked. `setup` says so,
    naming the keys, never their values. The other items are linked as usual, and the account
    gets the source's settings by injection, without the authentication. This is a default
    for the new home, checked once, and not a boundary: a linked `settings.json` stays the
    source's file, so authentication settings added to it later are read by every home that
    links it. remuda says so then (below) but does not prevent it.
  - A home that shares `projects` and has no `settings.json` of its own (the source's was not
    linked, or the source had none) has no `cleanupPeriodDays` where R11 looks for one:
    `setup` says so.
  - Never linked, whatever the source has: `.claude.json` (the login's identity and the
    account's caches), `history.jsonl` (attribution, R9), `sessions` (running sessions, R7),
    `remote-settings.json` and `policy-limits.json` (what an organization sets for its
    accounts), and everything else that is not in the list.
  - A link is `<home>/<item>` and points at `<source home>/<item>`: an absolute path, written
    from the source home's path as registered (`$HOME/.claude` for `default`), not
    canonicalized, as for the item links under `$REMUDA_HOME/shared/` below.
  - Only in the directory this `setup` created (R13): the home is made with `mkdirat` below
    `homes/<provider>`, which fails where the name exists, and opened without following a
    symlink; the links are made with `symlinkat` through that descriptor, and before the first
    one remuda reads the directory through the same descriptor and requires it to be empty.
    Otherwise nothing is linked, the account is not registered, no login runs, and the
    directory is left as it is (R2). `symlinkat` fails where a name exists: nothing is ever
    replaced or removed.
  - A link that cannot be made is reported on stderr. The links already made stay, the
    remaining ones are still made, and the account is registered and logged in as usual:
    nothing is rolled back.
  - Once: remuda never adds, changes, or removes a link in that home afterwards, and never
    makes one in a home registered with `add` or in the native login's. What such a home does
    not link is reported (R11) and injected at launch (below).
  - When the source's home is not a directory, nothing is linked and `setup` says so. Without
    `[share.claude]`, and for codex, the home is created empty.
  - `setup` reports what it linked on stderr; from the TUI, the notice after the login gives
    the number of links and the item names, without paths (R21); the line printed before the
    login in private mode gives the number only.
  - Residual: none by path below `$REMUDA_HOME`. A symlink at `homes` or `homes/<provider>` is
    refused, and the links go into the directory remuda created even if a process of the same
    user moves or replaces it meanwhile: a directory swapped in at the home's path gets
    nothing. What remains is `$REMUDA_HOME` itself, opened as the path the user gave; a
    directory another process puts at the home's name in the instant between its creation and
    its opening, which is linked only if it is empty; and the source's items, whose existence
    and settings are read by path a moment before each link is made.

  Basis (read from the 2.1.286 bundle, not verified by experiment unless stated): every path of
  a home derives from `CLAUDE_CONFIG_DIR`; no variable moves `projects` or `history.jsonl`
  alone, so a session store can be shared only through a symlink.
  `CLAUDE_SECURESTORAGE_CONFIG_DIR` (R2) moves the credentials only, not `.claude.json`, whose
  `oauthAccount` claude does not fetch again within 24 hours: two accounts on one
  `.claude.json` would send one account's organization with the other's token. claude writes
  the user's `settings.json` and `.claude.json` through a symlink, without replacing it.
  `file-history` holds the file backups of `/rewind` by session ID, read from the home of the
  account that resumes. The layout with `projects`, `settings.json`, `CLAUDE.md`, `skills`,
  `commands`, `agents`, `hooks`, and `plugins` linked is the one observed in daily use
  (2.1.286); the other five links are not. A plugin installed from a member's home is recorded
  in the shared `installed_plugins.json` with an `installPath` through that home's `plugins`
  link (observed), which then cannot be removed (R11).
- **When.** Injection happens only for session invocations (R6), from `run` and from the TUI
  alike.
- **What is injected**, component by component. A component is skipped for a home that already
  shares it with the source, detected by comparing realpaths (existing symlink layouts, R12):

  | Component | Skipped when | Injection |
  | --- | --- | --- |
  | Instructions: `CLAUDE.md`, `skills/`, `commands/`, `agents/`, `rules/` | every one the source has resolves to the source's | `--add-dir=$REMUDA_HOME/shared/claude` (whose `.claude/` holds one link per item of the source's first four, and copies of its rules) and `CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD=1` in the child environment |
  | Settings | `settings.json` resolves to the source's | the part of the source's `settings.json` that neither the home nor the project defines, in the single `--settings` |
  | Plugins | `plugins/` resolves to the source's | `--plugin-dir=<install path>` for each plugin enabled in the source's settings |
  | Auto-memory | `projects/` resolves to the source's: `CLAUDE_CODE_REMOTE_MEMORY_DIR=<source home>` is set instead (below) | `autoMemoryDirectory`, in the single `--settings` |
  | Agent memory | `agent-memory/` resolves to the source's, neither `autoMemoryDirectory` is injected nor `projects/` resolves to the source's, or the user set the variable | `CLAUDE_CODE_REMOTE_MEMORY_DIR=<source home>` in the child environment |

- **Instructions.** `$REMUDA_HOME/shared/claude/.claude` is a directory in which remuda keeps
  copies of the source's rules (below) and
  symlinks named `CLAUDE.md`, `skills`, `commands`, and `agents`, each pointing at that item of
  the source home (`<source home>/<item>`, written from the home's path as registered, not
  canonicalized; an existing link is compared with it as a path); an item the
  source does not have (missing, or a dangling link) gets no entry. remuda brings the directory to
  that state before a launch that needs it, and writes nothing when it already is: a missing link
  is created, one with another target is replaced atomically (a temporary link in the same
  directory, renamed into place), and one for an item the source no longer has is removed. An
  entry named like an item that is not a symlink, or a `.claude` that is neither a directory nor
  a symlink, is never replaced: nothing is changed, and the launch goes on without shared
  instructions and says so. `shared` and `shared/claude` are directories of remuda's own, made
  when missing: one that is a symlink, or that exists and is not a directory, is treated the same
  way, checked before anything else: nothing is created, replaced, or removed below it (so
  nothing where a link points), and the launch goes on without shared instructions and says so.
  Nothing else in `shared/claude/` or in `.claude/` is touched, apart
  from `.claude/rules/` (below).
  A `.claude` that is a symlink (the earlier layout: one link to the whole source home) is
  migrated in place: the directory is built under a temporary name in `shared/claude/`, the link
  (only a link, never a directory or a file) is removed, and the directory is renamed into place.
  A directory cannot be renamed over a symlink, so for an instant there is no `.claude`: a
  remuda launch never misses it (it prepares `.claude` itself before starting claude), but a
  session already running may not find the shared items if it reads them in that instant. When
  two remudas migrate at the same time, the one whose rename finds a directory already in place
  removes its own temporary directory and checks the items of the one in place; a temporary
  directory is removed on every failure remuda sees. A process killed between removing the link
  and the rename leaves no `.claude` (and an inert temporary directory, which is not touched)
  until the next member launch creates it. A remuda from before this layout, still running,
  refuses the directory ("is not a symlink") and launches without shared instructions, saying
  so, until it is restarted. Residual: a `shared`, `shared/claude`, or `.claude` that is a
  symlink when remuda looks is refused (or, for `.claude`, migrated), but the checks and the
  writes are by path, so a process of the same user that replaced one of them with a
  symlink in the instant between remuda's check and its write could redirect that write; remuda
  never creates such a symlink, and such a process can already write there itself. The same
  holds for the rule copies and their directories, where the redirected operation could also
  replace or remove a `*.md` file.
  Basis (verified on 2.1.282): with the environment variable set, `--add-dir=<dir>` loads
  `<dir>/.claude/CLAUDE.md` and the skills, commands, and agents under `<dir>/.claude/` with their
  plain names, through per-item symlinks exactly as through a whole-home `.claude` symlink: a
  skill that is itself a relative symlink inside the source's `skills/` loads, and a nested
  command `commands/probe/nested.md` loads as `probe:nested`; it does not load
  `<dir>/.claude/settings.json` (2.1.281). Plugins were rejected for this component because plugin
  items are namespaced (`name:item`), which would rename every agent and skill. Why per-item links
  (verified on 2.1.282, `-p`, default permission mode): through a whole-home link,
  `<dir>/.claude/…` reached every file of the source home (transcripts, a leftover
  `.credentials.json`), and Read, Grep, and Glob of such paths were refused pending permission
  ("resolves through a symlink to <source>, which is outside the allowed working directories"),
  the same as reading the source home directly; through per-item links,
  `<dir>/.claude/.credentials.json`, `<dir>/.claude/projects/…`, and even
  `<dir>/.claude/agents/../.credentials.json` do not exist (paths are normalized lexically), and
  Glob and Grep of `<dir>/.claude` find nothing. claude grants silent access in neither layout;
  the per-item layout bounds what one approved prompt can expose to the four items instead of the
  whole source home, and does not depend on that check of claude's. Side effects, documented
  rather than prevented: tools may access the four items like any `--add-dir`, and the variable
  also loads `CLAUDE.md` from other `--add-dir` directories the user passes.
- **Rules.** The source's rules are its `rules/**/*.md` regular files, down to 16 directory
  levels, symlinks followed and a directory reached twice read once; at most the first 1000 in
  name order, directory by directory; a `rules` without such a file is not an item. They are
  shared as copies, not links: `.claude/rules/` under `$REMUDA_HOME/shared/claude/` is a
  directory holding a regular file for each rule, at the rule's path below `rules/`. remuda
  brings it to that state together with the item links, before a launch that needs it, and
  writes nothing when it already is:
  - a copy that is missing or whose content differs is written read-only (mode 0400) under a
    temporary name in its directory (`.<32 hex digits>.tmp`, not a rule's name) and renamed
    into place; directories are made one level at a time, never through a link;
  - a regular `*.md` file there that the source no longer has is removed, and so is a temporary
    file a write cut short left behind, and then the directories that leaves empty, `rules`
    included. A file reached under another spelling of a rule's name (a file system that
    ignores case) is that rule's copy and stays;
  - a rule that became a directory of rules, or the reverse, replaces the copy of the other
    kind: the removals come first, and a directory in a copy's place goes when they leave it
    without files;
  - `rules`, or a directory on the way to a copy, that is not a directory, and a copy's place
    taken by a link, or by a directory holding anything that is not removed above, are never
    replaced: as for the item links, nothing is changed, and the launch goes on without shared
    instructions and says so. Other files and links in that directory are left alone;
  - a rule that cannot be read is not shared;
  - one remuda at a time changes `.claude` (an exclusive lock on the directory itself, where
    the file system has locks), so launches at the same time agree on the copies even while
    the source's rules change.

  A copy is a snapshot taken at launch, and is the wrong place to change a rule: the source's
  file is, and the copy is read-only so that an edit to it fails rather than being overwritten
  at the next launch.
  Basis (verified on 2.1.286, `-p`): from an added directory, `.claude/rules/**/*.md` loads
  when it is made of regular files, a hard link included, and a nested directory too; nothing
  loads through a `rules` that is a symlink, nor through a rule file or a subdirectory that is
  one. Read from the 2.1.286 bundle: a link out of an added directory's `rules` is followed only
  with the project's `hasClaudeMdExternalIncludesApproved` in the account's `.claude.json`, a
  per-project, per-account approval remuda cannot rely on. Known limitation (verified on
  2.1.286): a rule whose frontmatter has `paths` is not applied from an added directory,
  whatever file the session reads, in the project or in the added directory; R11 names such
  rules. A home whose `rules` is a symlink to the source's loads them as its own user rules,
  `paths` included; like any other item, it gets none injected only when every other item it
  has not linked is absent from the source (R11's warning about items loading twice applies to
  `rules` as to the others).
- **Settings.** claude merges settings sources in the order user, project, local, flag, policy
  (lowest to highest; read from the 2.1.281 bundle), and `--settings` is the flag source: injected
  keys win over the home's own `settings.json` and over the project's `.claude/settings.json` and
  `.claude/settings.local.json`, and hook lists from all of them run (verified on 2.1.281). The
  shared settings must behave as if they were user settings, and nothing may run twice, so remuda
  injects the source's settings **minus what the home, the project, and the local project settings
  already define**, computed recursively against each of them in turn:
  - a key the other side does not define: the source's value is kept;
  - a key where both values are objects: recurse;
  - a key where both values are arrays: the source's elements that are not equal (as JSON) to any
    element of the other array are kept, so identical hook entries are not duplicated;
  - any other key the other side defines: dropped (the other side wins).
  claude's merge replaces rather than combines a few keys (read from the 2.1.281 bundle: every
  array is concatenated and deduplicated except `fallbackModel`, which is replaced; `modelPicker` is
  replaced; `extraKnownMarketplaces` and `managedMcpServers` are merged one level deep). These are
  compared as whole values: `fallbackModel` and `modelPicker` are dropped if the other side defines
  them at all, and each entry of `extraKnownMarketplaces` (and its alias `additionalMarketplaces`)
  and `managedMcpServers` is dropped if the other side defines that entry.
  Project settings are read as claude reads them (from the 2.1.281 bundle): `.claude/settings.json`
  from the start directory; `.claude/settings.local.json` from the start directory and, when the
  project root differs from it, is not the real home directory, and it, its `.git`, and its
  `.claude` belong to the current user, also from the project root. Unreadable or invalid project
  files are treated as absent. If the user's arguments contain `--setting-sources`, remuda injects
  no settings and no auto-memory and says so on stderr, as for `--settings`.
- **Never injected: authentication.** Settings that choose credentials, provider, endpoint, or
  organization are removed from the injected settings, whatever the home defines: remuda never
  injects them, so a member that gets its settings at launch does not authenticate or bill as
  the source through them, and none of them reaches that member's tools. This is a property of
  injection only. A member whose `settings.json` is a symlink to the source's reads the file
  whole, these settings included, as its own user settings. `setup` does not make that link
  when the source's settings have any of these at that moment (above). When the source's
  settings have them and a member's `settings.json` resolves to the source's, remuda says so
  and does not prevent it: R11 warns in the accounts view, and each session launch of that
  member (R6, from `run` and from the TUI alike) gives a warning on stderr, or among the TUI
  launch's warnings, naming the settings and never their values: `warning: <account> reads
  the authentication settings of <source> through its settings.json link: <keys>`. For this
  the source's `settings.json` is read even when the home shares every component; a file that
  cannot be read is not reported there (R11 reports it) and does not fail the launch.
  The credentials of a login are not settings: they are in the Keychain, or in the home's
  `.credentials.json`, which is never linked. Settings that should apply to one account only,
  authentication among them, belong outside the source's `settings.json`: in a home that does
  not link it (an account that gets its settings by injection, or one with `share = false`).
  The settings concerned:
  - the keys `apiKeyHelper`, `proxyAuthHelper`, `otelHeadersHelper`, `awsAuthRefresh`,
    `awsCredentialExport`, `gcpAuthRefresh`, `forceLoginMethod`, `forceLoginOrgUUID`;
  - in `env`, names are matched case-insensitively and withheld if any rule matches:
    - prefix `ANTHROPIC_`, `AWS_`, `AZURE_`, `GOOGLE_`, `GCLOUD_`, `GCE_`, `CLOUDSDK_`,
      `CLOUD_ML_`, `VERTEX_`, `METADATA_`, `IDENTITY_`, `IMDS_`, `MSI_`, `CLAUDE_CODE_USE_`,
      `CLAUDE_CODE_SKIP_`, `CLAUDE_CODE_HOST_`, `CLAUDE_CODE_PROVIDER_`,
      `CLAUDE_CODE_FEDERATION_`, `CLAUDE_CODE_CERT`, `CLAUDE_CODE_CLIENT_CERT`, or `_CLAUDE_CODE_`;
    - substring `TOKEN`, `KEY`, `SECRET`, `PASSWORD`, `CREDENTIAL`, `CREDS`, `OAUTH`, `UUID`,
      `BASE_URL`, or `HEADERS`;
    - an underscore-separated part equal to `AUTH` (so `…_HOST_AUTH_REFRESH` matches and
      `GIT_AUTHOR_NAME` does not);
    - exactly `CLAUDE_CONFIG_DIR`, `CLAUDE_SECURESTORAGE_CONFIG_DIR`, `HTTP_PROXY`, `HTTPS_PROXY`,
      or `ALL_PROXY` (a proxy URL can carry credentials).
    Exceptions, shared unless a prefix rule or another substring rule matches: the model-name
    variables `ANTHROPIC_MODEL`, `ANTHROPIC_DEFAULT_MODEL`, `ANTHROPIC_SMALL_FAST_MODEL`,
    `ANTHROPIC_DEFAULT_*_MODEL*`, and `ANTHROPIC_CUSTOM_MODEL_OPTION*`, and names ending in
    `_TOKENS` (counts such as `MAX_THINKING_TOKENS`, not secrets).
  The rules cover claude's own groupings of provider, credential, endpoint, and host-auth variables
  in the 2.1.281 bundle, widened to prefixes so that new variables are withheld by default. The accounts
  view (R11) lists the settings withheld this way.
- **Passing settings.** claude uses only the last `--settings` option (verified on 2.1.281), so
  remuda passes exactly one, as a file path, never inline: the injected JSON is written with mode
  0600 to `$REMUDA_HOME/state/settings/<sha256 of the content>.json` (written atomically, reused
  when the content is unchanged; files not used for 30 days are removed when a new one is written,
  under an exclusive lock on `state/settings/.lock` (a regular file) that reuse also takes, so a
  file is never removed between being chosen and being passed; on a filesystem that does not
  support locking, remuda proceeds without the lock). claude reads the file once at startup and keeps its
  content (2.1.281 bundle).
  This keeps settings values out of the process list and away from per-argument size limits.
  `autoMemoryDirectory` is added to the same JSON when auto-memory is injected. If the user's
  arguments already contain `--settings`, remuda injects no settings and no auto-memory and says so
  on stderr. A settings file of the source or home that is not a JSON object, or not a regular
  file (a FIFO or a device would block), is an error for the launch.
  Note: claude treats a settings file passed with `--settings` like repository settings in one
  check: a cloud or teleport git-bundle upload refuses if such a file sets `env.PATH`, `env.HOME`,
  or similar variables (2.1.281 bundle). remuda shares them anyway; the case is rare.
- **Plugins.** Enabled plugins are the keys whose value in the source settings' `enabledPlugins` is
  `true` or an array (claude treats both as enabled), except those set to `false` in the
  `enabledPlugins` of the home or of the project settings above, and those the home has installed
  itself in a way claude loads here, which would otherwise load twice. claude loads an install
  (2.1.281 bundle) when its scope is `user` or `managed`, or when its `projectPath` equals the start
  directory, or when both the `projectPath` and the start directory are inside git repositories
  with the same project root (auto-memory rule below). If the home's own `installed_plugins.json`
  exists but is not recognized, remuda injects no plugins (R11 warns).
  Their install paths come from the source's `plugins/installed_plugins.json` (format version 2:
  `plugins.<name@marketplace>` is a list of installs; the first `user`-scoped install with an
  existing `installPath` is used). Basis (verified on 2.1.281): `enabledPlugins` alone does nothing
  in a home that has not installed the plugin, while `--plugin-dir=<install path>` loads it under
  the same `name:item` names as an installed plugin, including the plugin's MCP servers. An
  unrecognized file means no plugin injection and an R11 warning, never a failed launch.
- **Auto-memory.** Memory belongs with configuration, not with sessions: its default location is
  `<home>/projects/<project>/memory/`. remuda points it at the source's copy:
  `<source home>/projects/<project>/memory`, and must compute `<project>` exactly as claude does
  (read from the 2.1.281 bundle and checked against the path claude reports):
  - The start directory is the launch cwd (for a resume, `cwd_last`), made absolute,
    canonicalized (realpath), and NFC-normalized, as claude does with its own cwd.
  - The project root is found as claude finds it, without running git (read from the 2.1.281
    bundle): walk up from the start directory to the first directory containing a `.git` entry
    (file or directory). If that `.git` is a file of the form `gitdir: <path>` whose git dir has a
    `commondir`, and the git dir's `gitdir` file points back (by realpath) at that `.git` file, the
    root is the parent of the common dir when the common dir is named `.git`, and the common dir
    itself otherwise (a linked worktree, including a worktree of a bare repository); on any other
    outcome the root is the directory containing `.git` (a plain repository, a submodule, a
    separate git dir, a moved worktree). With no `.git` entry up to `/`, the root is the start
    directory. Not replicated: claude refuses to follow a `.git` symlink, `gitdir`, or `commondir`
    into network locations (on macOS, paths under `/net`, `/Network`, `/home/<user>`, `/.vol`,
    `/.file`, and `//` UNC paths) and keeps walking up; in those rare layouts remuda may choose a
    different project name.
  - The root is NFC-normalized. `<project>` replaces every UTF-16 code unit that is not an ASCII
    letter or digit with `-` (so a character outside the Basic Multilingual Plane becomes `--`).
  - If the root has more than 200 UTF-16 code units, claude truncates and hashes the name
    (**[unverified]** details), so remuda does not inject auto-memory.
  - remuda does not inject `autoMemoryDirectory` if the source, the home, or the project's local
    settings already set it.
- **Agent memory.** A subagent with `memory: user` keeps its memory in
  `<home>/agent-memory/<agent>/`. No setting moves it; with `CLAUDE_CODE_REMOTE_MEMORY_DIR=<dir>`
  in its environment claude keeps it in `<dir>/agent-memory/<agent>/` instead. remuda sets the
  variable to the source's home, as registered, exactly when it injects `autoMemoryDirectory`
  and the settings file carrying it was written, unless the home's `agent-memory` resolves to
  the source's; or when the home's `projects` resolves to the source's (next bullet). The
  condition is not optional: the variable also moves the default auto-memory location, to
  `<dir>/projects/<project>/memory`, so remuda sets it only where it has itself decided where
  auto-memory goes, or where that is already the place. remuda creates nothing: claude makes
  the directory when a subagent first writes.
  - **A linked `projects`.** A home whose `projects` resolves to the source's keeps its
    auto-memory in the source's store already, but by a path through the link. claude grants
    its memory directory write access by that literal path and then checks the resolved one,
    which is under the source's `.claude/` and outside the working directories: every memory
    write asks for permission, in every mode, and no `Edit(//…)` allow rule or
    `additionalDirectories` entry avoids it (observed on 2.1.288). With the variable set to the
    source's home claude names the directory by the source's path, the same directory without
    the link, and writes freely; `autoMemoryDirectory` is not injected (it would be the same
    place). Not set when the user passes `--settings` or `--setting-sources`, sets the variable
    themselves, or a settings file (the source's, the home's, the project's) chooses
    `autoMemoryDirectory`: remuda does not know which of the two claude would follow. In this
    case a malformed settings file of the source means no `autoMemoryDirectory` seen, not a
    failed launch, as for any launch that injects nothing.
  - **A variable already in remuda's environment.** With the source's home as its value, it was
    set by an outer remuda launch (a member's session that starts remuda again): it is decided
    again for this launch, and removed from the child's environment when this launch does not
    set it, so the source, an opted-out account, or a member that gets no auto-memory injected
    never inherits it. With any other value it is the user's: remuda neither sets nor removes
    it. Not covered: a `claude` started inside a member's session without remuda inherits the
    variable but not the settings, and keeps its memory under the source's home.
  - Basis (verified on 2.1.286, `-p`, with an agent given by `--agents`, and again through
    `remuda run`): the agent's `MEMORY.md` and notes were written under
    `<dir>/agent-memory/<agent>/`, the home got no `agent-memory`, and auto-memory stayed at the
    injected `autoMemoryDirectory`. The variable is not documented and was made for claude's
    remote sessions (2.1.286 bundle), so it is the least stable part of R18.
  - Side effects, documented rather than prevented (2.1.286 bundle): a subagent with
    `memory: local` keeps its memory in `<dir>/projects/<project>/agent-memory-local/<agent>/`
    instead of the project's `.claude/agent-memory-local/`, so for such an agent the members
    and the source, which has no variable, no longer read the same directory; and where claude
    tidies memory files under `projects/` (`tiny_memory`, `memory/proposals`) it looks under
    `<dir>/projects/`, the source's store. `memory: project` is not affected.
  - A member that shares `projects` with the source through a symlink gets the variable, so
    its agent memory is the source's whether or not `agent-memory` is linked too; where a
    settings file chooses `autoMemoryDirectory` the variable is not set, and only a link
    shares it (R11 warns).
- **Order.** Injected options come before the user's arguments, each in the `--option=value` form,
  so that a variadic option (such as `--add-dir`) cannot consume the user's arguments.

  ```text
  claude --add-dir=$REMUDA_HOME/shared/claude --settings=<file> --plugin-dir=<install> …  <user args>
         └─ instructions ──────────────────┘ └─ settings, ───┘ └─ one per plugin ──┘  └─ with R6's
                                               auto-memory                               --session-id
  ```
 The launch
  log (R6) records the injected option names and the byte size of each value, not the values.
- `run` stays a fast path (R6): injection reads a handful of settings files and
  `installed_plugins.json`, runs no subprocess, and scans no sessions.

## R19. Relay (removed)

Removed on 2026-10-01, before any release contained it (ROADMAP, design decisions). A relay
continued a session under another account by copying its transcript and checkpoints into that
account's home and forking it there; it was the one case in which remuda wrote into a home (R13).
A session is now continued only by an account whose `projects` store holds it (R16). The number
stays reserved so that the entries after it keep theirs.

## R20. Token statistics

Token counts per account and model, read from the agents' own transcripts, for a period, and their
estimated cost: what the requests would cost at the providers' public API list prices (prices as
of 2026-09-24). Most accounts are subscription logins, so the cost is an estimate for comparison
(≈ API list price), not a bill. Computing them runs no agent command and makes no network request:
the prices are built into remuda and can be overridden in `config.toml` (R3).

- **Counts.** Input, cache read, cache write, output, and reasoning. The total is input + cache
  read + cache write + output (reasoning is part of output). A count a provider does not record is
  shown as `-`: claude records no reasoning apart from output, codex no cache write. Claude's
  cache write is counted by cache lifetime, 5 minutes and 1 hour, which are priced differently,
  and shown as one count.
- **Claude** (verified on 2.1.71–2.1.281 against 1,064,472 records in 19,431 transcripts): an
  assistant record (`"type": "assistant"`) carries `message.id`, `message.model`, and
  `message.usage` with `input_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens`
  (cache write), and `output_tokens`, and `cache_creation` with `ephemeral_5m_input_tokens` and
  `ephemeral_1h_input_tokens`, the cache write by lifetime. Claude writes one record per content
  block, each repeating the message's usage, with `output_tokens` growing while the message
  streams. So a message is counted once, by its `message.id` (in the corpus no id was shared by
  two requests), with the largest value of each count among its records and the timestamp of its
  first. Not counted: other record types (a `progress` record repeats a subagent's message, which
  is counted from the subagent's transcript; a tool result's `usage` sums a subagent's), records
  without `message.id`, and the model `<synthetic>` (claude's placeholder messages, whose usage is
  zero).
- **Cache lifetime** (verified on 2.1.211–2.1.281 against 210,026 messages): `usage.cache_creation`
  matches the cache write of a message with at most one `message` entry in `usage.iterations`;
  with several (2,511 messages, each with an advisor call) it is the first entry's, and the sum
  over the `message` entries matches. So the 1-hour cache write is the sum of
  `ephemeral_1h_input_tokens` over the `message` entries that record `cache_creation`, else the
  top-level one, at most the cache write; the rest of the cache write is 5-minute, including a
  cache write recorded without lifetimes.
- **Fast mode and US-only inference** (**[unverified]**: neither was ever observed in
  transcripts) are read from `usage.speed` (`"fast"`) and `usage.inference_geo` (`"us"`), the
  values the API documents; in the sample above only
  `"standard"`, `"not_available"`, and `"global"` occurred. They apply to the message, not to its
  advisor calls, whose entries record neither.
- **Advisor calls.** `usage.iterations` lists the requests behind a message, and the top-level
  usage is the sum of its `message` entries (12,735 of 12,741 records). An `advisor_message`
  entry is a separate request to its own `model`, not included in the top-level usage; each is
  counted under that model. Its cache write is split by lifetime from its own `cache_creation`.
- **Claude transcripts**: in each `projects` store (a store shared by several homes is read once,
  R8), every top-level `<project>/*.jsonl`, and every `*.jsonl` at any depth below a directory
  `<project>/<session id>/` (subagent transcripts, such as `subagents/agent-*.jsonl` and
  `subagents/workflows/<id>/agent-*.jsonl`), which belongs to that session. Sessions hidden from
  History (R8) count too.
- **Codex** (verified on 0.155.1 against 106,795 `token_count` events in 1,464 rollouts): an
  `event_msg` of type `token_count` with an `info` carries `total_token_usage`, the session's
  cumulative usage, and `last_token_usage`, the latest request's; each has `input_tokens` (cached
  included), `cached_input_tokens`, `output_tokens`, `reasoning_output_tokens` (part of output),
  and `total_tokens`. An event whose `total_token_usage` equals the previous one's in the same
  rollout is skipped: codex repeats events (16,750 in the corpus), and after compacting it records
  the new context size with an unchanged total (673). Every other event counts its
  `last_token_usage`: input without cached, cached as cache read, output, reasoning. An event
  belongs to the model of the last `turn_context` before it (`payload.model`); events before the
  first `turn_context` take the model of the first one after them, else `unknown`. Rollouts:
  `sessions/**/rollout-*.jsonl` and `archived_sessions/rollout-*.jsonl` of every codex home,
  including those hidden from History (R17).
- **Copies count once.** A fork holds its parent's history: claude copies the parent's records
  with their `message.id` and timestamp and marks them `forkedFrom`; a codex fork may replay the
  parent's `token_count` events with the fork's timestamps and the parent's cumulative totals; a
  transcript may also have been copied whole into another store by hand. So each claude message
  is counted once across all stores by its `message.id`, and each codex request once across all rollouts by its
  `total_token_usage`. In the corpus, 2,360 of the 2,362 totals found in more than one rollout
  came from forks; the other two were the first requests of unrelated `codex exec` runs with
  identical usage, which are counted once (a known limitation). The copy that counts is, in
  order: one not known to be a copy (a `forkedFrom` record), the one with the earliest timestamp,
  the one whose path sorts first.
- **Accounts.** A message counts for the session of the transcript whose copy counts. A claude
  session's accounts are those of R9 without running sessions (the launch log and
  `history.jsonl`); a codex rollout's are the accounts of its home (R17). A session attributed to
  several accounts is counted once, for those accounts together (`max + team`); a claude session
  attributed to none is counted as unattributed. The sections therefore add up to the overall
  total.
- **Periods**: today, the last 7 days, the last 30 days, all. A period starts at local midnight
  (the system time zone) of today, of 6 days before, or of 29 days before; a message is in it when
  its timestamp is not earlier than the start. All also includes messages without a timestamp.
- **Cost.** Each request is priced by its model and counts, and the costs are summed. Built-in
  prices, USD per million tokens:

  | Model | Input | 5-minute cache write | 1-hour cache write | Cache read | Output |
  | --- | ---: | ---: | ---: | ---: | ---: |
  | `claude-fable-5-1`, `claude-mythos-5-1` | 10 | 12.50 | 20 | 0.25 | 50 |
  | `claude-fable-5`, `claude-mythos-5` | 10 | 12.50 | 20 | 1 | 50 |
  | `claude-opus-5-5` | 4 | 5 | 8 | 0.20 | 20 |
  | `claude-opus-5`, `claude-opus-4-8`, `claude-opus-4-7`, `claude-opus-4-6`, `claude-opus-4-5` | 5 | 6.25 | 10 | 0.50 | 25 |
  | `claude-opus-4-1`, `claude-opus-4` | 15 | 18.75 | 30 | 1.50 | 75 |
  | `claude-sonnet-5` | 2 | 2.50 | 4 | 0.20 | 10 |
  | `claude-sonnet-4-6`, `claude-sonnet-4-5`, `claude-sonnet-4` | 3 | 3.75 | 6 | 0.30 | 15 |
  | `claude-haiku-4-5` | 1 | 1.25 | 2 | 0.10 | 5 |
  | `claude-3-5-haiku` | 0.80 | 1 | 1.60 | 0.08 | 4 |

  Codex, USD per million tokens:

  | Model | Input | Cached input | Output | Long context |
  | --- | ---: | ---: | ---: | :---: |
  | `gpt-6-astra` | 10 | 1 | 50 | yes |
  | `gpt-6-sol` | 2 | 0.20 | 10 | yes |
  | `gpt-5.6-sol` | 4 | 0.40 | 20 | yes |
  | `gpt-5.6-terra` | 2 | 0.20 | 12 | yes |
  | `gpt-5.5` | 5 | 0.50 | 30 | yes |
  | `gpt-5.4` | 2.50 | 0.25 | 15 | yes |
  | `gpt-5.3-codex`, `gpt-5.2-codex`, `gpt-5.2` | 1.75 | 0.175 | 14 | no |
  | `gpt-5.1-codex-max`, `gpt-5.1-codex`, `gpt-5-codex`, `gpt-5` | 1.25 | 0.125 | 10 | no |
  | `gpt-5.1-codex-mini` | 0.25 | 0.025 | 2 | no |
  | `o4-mini` | 1.10 | 0.275 | 4.40 | no |

  `gpt-5.6-sol`'s price is a promotional one, through 2026-11-21. Other codex models have no
  public API price (`codex-auto-review`, codex's own routing id, and `gpt-5.3-codex-spark`, for
  example) and are priced only by `[prices]`.
  - **[unverified]** (as the API documents; not observed): fast mode on `claude-opus-5-5`,
    `claude-opus-5`, and `claude-opus-4-8` doubles every price
    (input 8, 10, and 10; output 40, 50, and 50; the cache prices keep their ratio to input); on
    other models it is priced as standard.
  - **[unverified]** (as the API documents; not observed): US-only inference multiplies every
    price by 1.1 on the models from 4.6 on: `claude-fable-5-1`,
    `claude-fable-5`, `claude-mythos-5-1`, `claude-mythos-5`, `claude-opus-5-5`, `claude-opus-5`,
    `claude-opus-4-8`, `claude-opus-4-7`, `claude-opus-4-6`, `claude-sonnet-5`, and
    `claude-sonnet-4-6`; with fast mode, both apply.
  - Codex: input without cached at the input price, cached input at the cached price, and output
    (reasoning included, not priced again) at the output price.
  - Codex long context, on the models marked above: a request with more than 272,000 input
    tokens, cached included, is priced whole at twice the input and cached prices and 1.5 times
    the output price.
  - A model is found by its id without a trailing `-YYYYMMDD` (`claude-haiku-4-5-20251001` is
    `claude-haiku-4-5`), exactly: `claude-fable-5` and `claude-fable-5-1` differ, and no prefix
    matches. Claude's prices apply to claude's requests, codex's to codex's.
  - `[prices."<model>"]` in `config.toml` (R3) gives the price of a model, found by the id as
    recorded or else without its date: it replaces the built-in price or prices a model that has
    none. Fast mode, US-only inference, and codex long context apply to it as to the built-in
    model of the same id. A count whose price it leaves out is not priced. For codex,
    `cache_read` is the cached-input price.
  - A request is not priced when its model has no price (`unknown` included) or a nonzero count
    of it has none; its tokens are still counted. A cost that leaves such requests out is shown
    followed by `+` (`$12.34+`), one with nothing priced as `-`, and the models with such
    requests are named below the table.
  - Not modelled (known limitations): the long-context premium of `claude-sonnet-4-5` and
    `claude-sonnet-4` (input beyond 200K tokens; the 4.6 and later models have none), batch and
    priority processing, codex's fast and priority processing (rollouts do not record it),
    server tools such as web search, and data residency other than US-only.
  - The cost is exact (integer picodollars; override prices are rounded to 10⁻⁶ USD per million
    tokens) and shown rounded to the cent: below $1,000 with two decimals (`$0.42`, `$12.34`), a
    cost that is not zero but below half a cent as `<$0.01`, and from $1,000 on like counts
    (`$1.2K`, `$45.6K`, `$118K`, `$1.2M`).
- **Shown**, for a period: every account in registry order (including those with nothing in the
  period), then each group of accounts, then unattributed. Each lists the tokens and cost per
  model (the model id as recorded), most tokens first, and their total. Then the tokens and cost
  per model over everything. Counts below 1,000 are shown whole, others in K, M, B, or T, with
  one decimal below 100 (`1.2M`, `93.3B`, `118K`).
- **Only transcripts that exist count**: tokens of transcripts deleted since (claude deletes those
  older than `cleanupPeriodDays`) are no longer counted.
- **Cache**: `$REMUDA_HOME/state/stats.json`, with a schema version, rebuilt on a mismatch,
  written atomically, deletable at any time (R3). For each transcript it holds what was counted
  from it (a 64-bit FNV-1a hash of each request's key, its timestamp, model, counts with the
  cache write by lifetime, and whether it used fast mode or US-only inference), how far the
  transcript was read, and, for codex, the last total and model. Transcripts are read like the
  index (R8): an unchanged file is not read again, a grown one only from its last complete line,
  any other one whole; only complete lines are parsed. Records of one message read in two
  refreshes merge by their key. The first computation reads every transcript whole (measured:
  20,895 files, 17.2 GB), and so does the first one after the schema version changes (version 2
  added the cache lifetimes and pricing flags).
- **Command**: `remuda stats [<account>] [--period today|7d|30d|all]` prints one period
  (default `all`) with a COST column, then a line saying the cost is ≈ API list price and the
  prices' date, and a line naming the models not priced, if any. With an account, it prints only
  the sections that include that account, and no overall section. Reading progress goes to
  stderr, as for `sessions`.
- **TUI**: view `4`, Stats. The statistics are computed in the background the first time the
  view opens, and again on each `r` after that, with reading progress shown; `r` also reads the
  prices in `config.toml` again (when they cannot be read, the built-in prices are used and the
  status line says so). `t` in the view cycles the period (all, today, 7 days, 30 days).
  The title says the cost is ≈ API list price, and the status line gives the prices' date unless it shows an error. Above
  the table, a chart shows the period over time: a bar per hour (today), per day (7 and 30 days),
  or, for all, per day from the first request with a timestamp (at most 3,660 days back), else
  per week (from Monday), else per month, whichever is the finest that fits the width; buckets
  that still do not fit are dropped from the oldest. The bars are the cost, or the tokens when
  nothing in the period is priced. Requests without a timestamp, or with one after today, are
  not charted. Each section's row has a bar showing its share of the period's cost (or tokens).
  The column header stays in view once the chart is scrolled past.

## R21. Private mode (TUI)

For screenshots, `Ctrl-P` toggles private mode anywhere in the TUI: in every view, in forms and
the search prompt (nothing is typed), in the account picker, and in the help box and
confirmation prompts, which stay open (`Ctrl-P` is not a key that cancels, R16, R17). Private mode
is off when the TUI starts and is not saved. The command line has no private mode.

While it is on, the header shows `PRIVATE`, and nothing on screen (views, overlays, forms, the
status and hint lines, notices, the help box) shows:

- **Account names** other than `default`, including names that are not registered (a launch log
  entry, `[share.claude] from`, a name typed in a setup): each is replaced by an alias
  `account-<n>`, numbered per provider in registry order when the TUI starts, then in the order the
  names appear (names that appear together, by name). Codex aliases keep their prefix
  (`codex:account-1`). An alias does not change while the TUI runs, even when accounts are added
  or removed.
- **Emails and organization names**: shown as `•••@•••` and `•••`.
- **Paths** (homes, working directories, stores, the directory remuda started in, form values):
  each component is shown as `•••`, and a leading `$HOME` as `~` (`~/•••/•••`).
- **Session content**: titles, first messages, session names, preview messages, background
  session logs, search text, and the descriptions in the configuration pane (R22) are shown as
  `•••`.
- **Free text** (notices, errors, check messages, the description of a pending launch): account
  names are replaced by their aliases, as whole words (ASCII letters, digits, `_`, and `-` make a
  word, so a name right next to CJK text or full-width punctuation is replaced too); emails,
  organization names, live session names, the search text, and the values typed in the open form
  (as typed, without the blanks around them, and as an error quotes them) are masked; text in
  `“…”` is masked; and each word with an `@` is masked. Paths in it are masked in two ways:
  - A path remuda itself puts in a notice or in a form's error (a launch directory, a store, a
    home) is masked whole, as a path above, whatever characters it holds; what the message says
    after it stays.
  - In any other text (an agent's output, an error of the system, a check message), where a path
    ends cannot be told: a path may hold blanks, `: `, `, `, quotes, and brackets. So each line is
    masked from its first path to its end, as one path. A path starts in the first word that
    holds a `/`: at the `/` or `~/` itself when it begins the word or follows a quote, a bracket,
    `=`, or `:` in it; otherwise after the last quote, bracket, or `=` in the word, or at its
    start, and the rest is a relative path (`./a/b`, `a/b`, `key=a/b`). A `/` and a single name
    is a path like any other (`/tmp`; the `/My` of `/My Disk/x`). This may hide what follows a
    path on its line (the reason of an error, say), and never shows part of one.
  - Only Claude's slash commands that remuda's own messages name, `/login` and `/rewind`, are
    not paths, and only as whole words: nothing but punctuation follows the command in its word
    (`/loginx`, `/login-x`, `/login.x`, and `/login/x` are paths).
  - A path the TUI knows (`$HOME`, a home, a store, the directory remuda started in, a directory
    typed in the open form) starts a path wherever it occurs as whole components, even within a
    word. One that is named like one of those commands, or lies under such a name, makes that
    name a path again: with a home `/login` or `/login/me`, `/login` is masked.
  - A notice made of several messages (the result of a launch and each of its warnings) is
    masked message by message: what one hides does not reach the next.

Numbers (usage percentages, reset times, token counts, costs, and the Stats chart), model names,
plans, login methods, providers, session IDs, pids, and times stay visible, and so do the names
of configuration items (R22): agents, skills, commands, plugins, hook events, settings keys,
`env` variables, MCP servers, and an agent's model, effort, and tools. The line remuda prints
before handing the terminal to a child (R16) shows no path and follows the same rules.

## R22. Account configuration (TUI)

For the selected claude account, the accounts view shows what its sessions load and where each
part comes from. It only reads: nothing is written (R13), no agent command runs, and credentials
(`.credentials.json`, the Keychain, `*.key`) are never opened.

- **Keys.** `p` or Space in Accounts opens the Configuration pane (beside the view from 120
  columns on, below the account table otherwise); pressed again, the pane takes the whole view;
  again, it closes. `Esc` steps back one of these. While the pane is beside or below, the
  selection moves as usual and the pane follows it, and `PgUp` / `PgDn` scroll the pane; while it
  takes the whole view, the movement keys scroll it. The account picker of `remuda run` (R5) has
  no pane.
- **When it is read.** In the background, never while drawing: when the pane opens, when the
  selected account changes while it is open, on `r`, and when the account list is read again
  (after a setup or a removal). For a codex account the pane says that configuration listing is
  Claude-only.
- **For a directory.** What a session loads depends on where it starts (project settings,
  plugins installed for a project, the auto-memory project, project MCP servers). The pane
  describes a new session started in the directory remuda was started in (the default of R16's
  new session), and its title names both: `Configuration · <account> · for <directory>`.
- **The same plan as a launch.** What shared configuration adds (R18) is decided by the very
  step that decides a launch's injection for a new session in that directory, without the
  launch's writes (the `.claude` item links and the settings file), so the pane cannot disagree
  with a launch. A settings file that would fail the launch (not a JSON object) is shown as a
  problem.
- **Origins.** Each item is tagged *own* (in the account's home), *shared from <source>*
  (injected at launch, R18), *already the source's* (the home's item, or a plugin's install,
  resolves by realpath to the source's, so nothing is injected for it, R12), or *not shared*
  with the reason (authentication where settings are injected, turned off by the home or the
  project, installed by the home itself, no user install, an unrecognized
  `installed_plugins.json`). An account that is the
  source, has `share = false`, or whose source home is missing says so once instead of listing
  the source's items. A symlink shows as `-> <target>`; one whose target does not exist, as
  broken.
- **Instructions.** `CLAUDE.md` (size and lines); `agents/*.md` (top level), by the `name` of their
  frontmatter or else the file name, with its `description`, `model`, `effort`, and `tools`;
  `skills/<dir>/SKILL.md`, by `name` or the directory name, with its `description`;
  `commands/**/*.md` (at most 4 levels), a subdirectory shown as `dir:name`; `rules/**/*.md` (R18),
  by path without the extension, one whose frontmatter has `paths` marked as limited to paths.
  Symlinks are followed; only regular files are opened; frontmatter is read from a file's first 8
  KB; at most 500 entries are listed per directory.
- **Synced skills.** `skills/synced/<organization>_<account>/` holds the claude.ai skills of one
  login, and a `skills` directory shared by several homes holds every login's buckets. The pane
  lists only the bucket named by `oauthAccount.organizationUuid` and `accountUuid` in the
  account's own `.claude.json`, as synced skills, with the number of other buckets; without those
  ids it says that the bucket cannot be matched. `synced` is never listed as a skill, and bucket
  names are never shown.
- **Skill overrides.** A skill set to `"off"` in `skillOverrides` of the settings a session gets
  (the home's, the project's, the injected) is shown as off; an override naming no skill the pane
  lists (own, shared, synced, or a plugin's, as `<skill>` or `<plugin>:<skill>`) is listed as
  stale.
- **Plugins.** The home's enabled plugins (`enabledPlugins` `true` or an array), each with the
  install claude loads for that directory (R18's load rule; when several do, one made for the
  directory, `local` then `project`, before `user` then `managed`, else the first in the file),
  its version and scope, and the number of install records; or marked not installed. Plugins set
  to `false` are counted, not listed. Then the source's enabled plugins, injected or not with the
  reason. For each installed plugin: its agents, skills, and commands (as above, in its install
  directory), its hook events with counts (`hooks/hooks.json`, or `hooks` in
  `.claude-plugin/plugin.json`), and the names of its MCP servers (`.mcp.json`, or `mcpServers`
  in `.claude-plugin/plugin.json`).
- **Settings.** The home's `settings.json` and the injected part of the source's, each
  summarized as: model; permission rule counts (allow, ask, deny); hook events with the number of
  hooks of each; `env` names; whether a status line is set; the names of other keys. The
  source's authentication settings (R18) are listed by name: for a member that gets settings
  at launch as not shared; for a member whose `settings.json` resolves to the source's as
  read through the link, since nothing is withheld there (R11 warns); for the source as
  withheld from injected settings.
- **Auto-memory.** The directory a session there uses, `autoMemoryDirectory` as injected, else
  as set by the project's settings, else by the home's, else `<home>/projects/<project>/memory`
  (R18's project name), and how many `*.md` files it holds. And the directory for the memory of
  user-scope subagents: the source's `agent-memory` where a launch redirects it (R18), else the
  home's own, marked as already the source's when it resolves there, with the number of agents
  that have one.
- **MCP servers.** The names in `mcpServers` of the account's `.claude.json` (user scope) and of
  its `projects` entry for the project, whose key is the project root of R18 (read from the
  2.1.281 bundle). They belong to the account: shared configuration does not include
  `.claude.json`.
- **No values.** Settings are summarized by key names and counts: no `env` value, hook command,
  permission rule, or MCP server definition is shown. `.claude.json` is parsed for those names
  and the two ids only; its other values are not kept.
- **Private mode (R21).** Names stay visible (agents, skills, commands, plugins, hook events,
  settings keys, `env` variables, MCP servers, and an agent's model, effort, and tools);
  descriptions are masked as `•••`; paths (link targets, install and memory directories, the
  directory in the title) are masked as in R21; the source account is shown by its alias;
  problems are free text.

## R23. Recommendation (`pick`)

`remuda pick` recommends which account and model to launch now, and at what effort. Rules decide
what is feasible and rank it; with a key, TypeSafe's Jev model chooses among the feasible options.
It reads the usage of R10 and `[pick]` (R3), runs only `codex login status` (R4) or, with
`--live`, R10's live queries, and writes nothing except, with `--run`, the launch log (R6).

- **Candidates.** Each account R1 lists (`default` included) with each model of its provider's
  `models`, or with the agent's default (nothing injected) when there are none. Not feasible, each
  with its reason: excluded; not of `--provider`; claude with neither `oauthAccount` in its
  `.claude.json` nor a usage cache; codex without `codex` on PATH, or whose `codex login status`
  says it is not logged in (a status that cannot be read is noted, not blocking); or a window
  that applies with less than `min_headroom` percent left (default 10).
- **Windows.** A usage row (R10) without a parenthesized name, or with `(all models)`, applies to
  every model. `<window> (<name>)` applies to the claude models of that family
  (`claude-<family>-…`; a bare alias is its own family) and to the codex model with that id,
  ignoring case; one that matches no configured model is ignored. Without `models`, the agent's
  default model is unknown: the `default` pair shows its account's per-model windows (in the
  output, in `--json` as `default_model_windows`, and in the request, marked as applying only if
  that model is of their family) but is never made infeasible by them. A window whose reset
  instant has passed counts as 0% used (reset since cached). Headroom is the least percent left over the
  windows that apply; without usage data a pair is feasible, of unknown headroom. Cached codex
  usage has no per-model limits (R10): they are shown as unknown.
- **Staleness.** Cached usage older than `stale_after` minutes (default 120), or of unknown age,
  is stale; it is marked, and breaks ties (below). Staleness never makes a window feasible: a used
  percentage only grows until its reset. `--live` queries every candidate account first (R10;
  `--timeout` per query, default 90); a failed query falls back to the cache, with a note.
- **Rules.** Feasible pairs rank by: known headroom before unknown; the model's position in
  `models`; headroom in 10-point bands, higher first (90% left and more is one band); fresh before
  stale; the binding window's reset, sooner first; `prefer` order; registry order. The rules'
  effort is `default_effort`, or none.
- **When Jev is asked.** Only with `TYPESAFE_API_KEY` in remuda's environment, without
  `--offline`, with `notes`, and with a choice to make (two feasible pairs, or a provider with two
  or more `efforts` and a feasible pair). Otherwise the rules decide, and the reason is `offline`,
  `no_key`, `no_notes`, or `single_option`.
- **The request.** One POST to `https://api.typesafe.ai/v1/systemone` with model `jev-latest`,
  through `curl` from PATH: `curl -q -sS --proto =https --max-time 10 -X POST -H "Content-Type:
  application/json" -o - -w "\n%{http_code}" -K - <endpoint>`. The authorization header and the
  body go in the configuration curl reads on its standard input, never in its arguments; a key
  holding a quote, a backslash, or a control character is not sent.
  - Questions: `launch`, a Choice over the feasible pairs (the rules' best 255 when there are
    more), named `<alias> / <model>` (`default` for the agent's default) and described by the
    binding window and the data's age, asked only when there are two or more; and
    `effort_<provider>`, a Score over `efforts`, for each provider with at least two and a
    feasible pair. The criteria are a JSON object, so their order carries no meaning.
  - State: plain text with the local weekday and time (no time zone), the rules already applied,
    each account with a feasible pair under its alias (`<provider>:account-<n>`, numbered per
    provider in registry order as in R21; `default` stays `default`), its usage (source, age,
    staleness, and each window that applies with its percentage and time to reset), the models,
    the notes, and an empty task. In the notes, each qualified name is replaced by its alias: a
    provider, `:`, and the longest run of name characters (`[A-Za-z0-9_-]`) after it, wherever
    the character before it is not an ASCII letter or digit (so also right after CJK text, `-`,
    or `_`, and before full-width punctuation). A name that is not registered gets the next
    alias of its provider, in the order the notes name them; `claude:max2` and `claude:max_` are
    such names, not `claude:max`. The rest, bare names included, is sent as written. No email,
    organization, plan, path, working directory, or session content is sent.
- **Combination.** Jev's pair when its confidence is at least 0.50 (`jev`); else, when the
  probabilities of one account's pairs add up to at least 0.70, that account with its most
  probable model (`jev_account`); else the rules' first (`low_confidence`). With one option,
  `launch` is not asked: the rules keep it (`single_option`) and Jev decides only the effort. The
  effort is the chosen provider's Score rounded to a level when its confidence is at least 0.50,
  else `default_effort`; an effort answer that is missing or not a score within its levels costs
  only the effort (`default_effort`, the problem in `jev.effort_error` and the output). No `curl`,
  a curl failure or its timeout (10 s), an HTTP status other than 2xx, a response that is not
  JSON, or a `launch` answer that is missing or names an option not offered: the rules decide
  (`jev_error`), and the message holds at most 200 characters of the response and never the key.
- **Output.** The account, model, and effort (and whether the effort is Jev's); what decided and
  why, with Jev's confidence; the rules' choice when Jev's differs; the binding window and its
  reset; the data's age; the exclusions; the `remuda run <account> <options>` command; and every pair that is not feasible,
  with its reason. `--json` prints `account`, `provider`, `model`, `effort`, `decided_by` (`jev`,
  `jev_account`, `rules`), `reason`, `effort_by` (`jev`, `rules`; null without an effort), `jev`
  (`model`, `confidence`, `effort_confidence`, `effort_error`, `error`; null when not asked),
  `command`, and `candidates` (`account`, `model`, `feasible`, `why_not`, `headroom`, `binding`,
  `resets_at`, `default_model_windows`, `source`, `fetched_at`, `age_seconds`, `stale`,
  `rules_rank`, `jev_probability`). `--print-request` prints the body a send would use and sends
  nothing (no key needed). With nothing feasible: the reasons, exit 1.
- **`--run`.** Launches the recommendation exactly as `remuda run <account> <options> <args>`
  (R6, R17, R18), `<args>` being those after `--`: claude gets `--model <m> --effort <e>`, codex
  `-m <m> -c model_reasoning_effort=<e>` (verified: codex 0.156.1 accepts both before a
  subcommand), before the user's arguments; the launch log records them among its `args`. An
  option the user's arguments already set (claude `--model`, `--effort`; codex `-m`, `--model`,
  `-c`/`--config model_reasoning_effort=…`) is not injected, and remuda says so on stderr.
  Arguments that do not start a new session (R6's classification for claude; `resume` or `fork`
  first for codex) are refused before anything is sent. `--json` and `--print-request` do not
  combine with `--run`.
