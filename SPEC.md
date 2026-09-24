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

## R1. Model

- **Provider**: an agent CLI (v1: `claude` is fully supported; `codex` supports accounts, usage, the session index, launch, resume, and fork, but not live sessions; see R4 and R17).
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
  v1 does not use it; it is recorded here only as a future escape hatch for relocating homes. If it
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
  ```
- Homes created by `setup` live at `$REMUDA_HOME/homes/<provider>/<name>`; homes registered with
  `add` stay where they are.
- Writes are atomic (temporary file + rename) and preserve the user's comments and unknown keys
  (the comments of an account that `remove` deletes go with it, R14a). If `config.toml` is a
  symlink, writes go through the symlink.
- Loading validates strictly: an invalid name, a duplicate name, a claimed `default`, a named
  account whose `home` is not an absolute path, `share` on a codex account, a `[share.claude] from`
  that names no claude account, and similar problems are all reported as errors
  naming the file; remuda neither guesses nor skips.
- Runtime state (index cache, statistics cache, launch log) lives in `$REMUDA_HOME/state/` and may
  be deleted and rebuilt at any time.

## R4. Provider contract

Each provider declares the following capabilities. A missing capability is shown as unavailable in
the UI, not reported as an error:

| Capability | claude | codex (M3) |
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
remuda                          open the TUI
remuda run [<account>] [args]   launch the agent under an account; without an account, open the TUI picker
remuda usage [<account>] [--live]  print per-account usage as plain text (R10)
remuda list                     accounts, login identity, home
remuda sessions [--limit N]     print recent sessions as plain text: time, account attribution, title, cwd (R8, R9)
remuda add <name> <path>        register an existing home directory (R14)
remuda setup <name>             create a new home and run `claude auth login`
remuda remove <account>         unregister an account; its home is left in place (R14a)
remuda relay <session> <account>  continue a session under another account (R19)
```

- There is no shell integration, global routing, or per-directory binding.
- `run` passes `args` through to the agent unchanged.

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
  When in doubt, nothing is injected.
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
- Launch log `state/launches.jsonl`: one line per launch, containing time, account, cwd, args, and
  session ID (or unknown).
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
  display.
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
- remuda makes no network requests of its own and never reads credentials to call the agent's usage
  endpoint directly.
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
  shares some but not all of `CLAUDE.md`, `skills`, `commands`, `agents` with the source through
  symlinks (those items may load twice); an enabled plugin whose install path does not exist; an
  `installed_plugins.json` whose format is not recognized; authentication keys in the source's
  settings that are withheld from members.

## R12. Symlinks in homes

- v1 detects symlinks in a home that point elsewhere and displays them as `-> <target>`.
- v1 does not create, delete, or modify any symlink. Any future write operation that encounters a
  symlink must either write through it or refuse; it must never replace the symlink with a private
  copy.
- Configuration is shared without symlinks, by injection at launch (R18). Existing symlink layouts
  keep working and are detected so that nothing is injected twice.

## R13. Write boundary

The complete set of v1 write operations:

- `$REMUDA_HOME/config.toml`, `$REMUDA_HOME/state/**`, `$REMUDA_HOME/shared/**` (R18)
- On an explicit relay (R19), and only then: a copy of one transcript into
  `<target home>/projects/<dir>/` and of its checkpoint files into
  `<target home>/file-history/<id>/`, creating those directories if needed. A relay never
  overwrites or deletes anything that remuda did not create in an earlier relay.
- `$REMUDA_HOME/homes/<provider>/<name>/` created by `setup` (an empty directory; login is performed
  by `claude auth login` itself, optionally with `--email` prefilled)

remuda never writes credentials, `.claude.json`, the Keychain, transcripts, `history.jsonl`, or
`*.key` files, never writes into any home directory except for the relay copies above, and never makes network requests of its own
(live usage is queried by the agent itself; see R10). Network use and writes into a home by an
agent are the agent's own, and happen only in live queries (`claude -p /usage`, `codex app-server`;
R4, R10) or in the other agent commands remuda runs in an account's environment (`claude auth
status`, `codex login status`, which creates `tmp/`, a launch, a login).

## R14. `add <name> <path>`

- Registers an existing directory as an account: the path is normalized per R2 and then stored
  verbatim; the directory itself is not modified.
- The directory must exist. If it does not look like a home for the provider (Claude: neither
  `.claude.json` nor `projects/` is present), remuda warns but registers it anyway.
- It is an error if the name is already taken, if the same path is already registered under another
  name, or if another spelling of the same directory (trailing `/`, a symlink, etc.) is already
  registered.
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
- Refused while `[share.claude] from` names the account: the registry would no longer load (R3).
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
or sessions. Fixtures for transcripts, rollouts, `sessions/*.json`, `history.jsonl`, and
`.claude.json` are derived from real structures and anonymized. In library-level tests where
`FAKE_CLAUDE_OUT` / `HOME` are not set, the fake claude defaults to paths inside the sandbox.

## R16. TUI actions (M2)

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
    the transcript: that account cannot find the session.
  - The cwd is `cwd_last` (R6); an error is reported if the directory does not exist.
  - `c`: continue under another account (relay, R19): a picker of the other claude accounts, then
    the relay and a foreground launch.
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
  (`y` confirms; any other key cancels); the prompt names the home that is kept. Refused on a
  `default` row. The account list is read again afterwards.
- **Background sessions** (in Live): `Enter` attaches (`claude attach <id>`, likewise suspending the
  TUI); `l` shows `claude logs <id>` in the preview pane; `x` stops and `D` removes a stopped
  session, both after confirmation (`y` confirms; any other key cancels). Interactive sessions in
  Live are view-only.
- Session IDs and background short IDs passed to claude are validated first (session IDs must be
  UUIDs; short IDs must be 8 lowercase hex digits) and rejected if malformed, so that a file name
  or JSON field starting with `-` cannot be interpreted as an option.
- A child that exits abnormally (e.g. killed by SIGKILL) may leave the terminal in raw / no-echo
  mode: the TUI saves the termios state once on entry and restores that snapshot before handing
  over the terminal and on final exit.
- **`remuda run` without an account** (in which case no other arguments are allowed either): opens
  the TUI account picker; once an account is chosen, the TUI exits and execs claude as
  `remuda run <account>` would.
- Preview expansion moved from `Enter` to `p` (or Space).

## R17. Codex (M3)

- Accounts: `remuda add --provider codex <name> <path>`, `remuda setup --provider codex <name>` (the
  new home is at `$REMUDA_HOME/homes/codex/<name>`, and login uses `codex login`). The implicit
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
  any other key cancels); if the rollout file was written within the last 10 minutes, the prompt
  adds that it is still being written. Forks need no confirmation.
- Running sessions: not supported for codex; shown as unavailable in the UI. Identity and usage:
  R4, R10, R10a.

## R18. Shared configuration (M2.5)

Sessions stay with the account that created them; only configuration is shared. remuda shares it
by injecting launch options, so nothing is written into any home (R13), and homes created by
`setup` get the shared configuration without any setup of their own.

- **Source.** `[share.claude] from = "<account>"` in `config.toml` names the account whose home is
  the source of shared configuration (typically `default`, whose home is `~/.claude`). Without this
  table nothing is shared. The source must be a claude account; loading fails otherwise (R3).
- **Members.** Every other claude account, unless it sets `share = false`. The source account itself
  gets no injection. Codex accounts are not affected; `share` on a codex account is a load error
  (R3).
- **When.** Only for session invocations (R6), from `run` and from the TUI alike.
- **What is injected**, component by component. A component is skipped for a home that already
  shares it with the source, detected by comparing realpaths (existing symlink layouts, R12):

  | Component | Skipped when | Injection |
  | --- | --- | --- |
  | Instructions: `CLAUDE.md`, `skills/`, `commands/`, `agents/` | all four resolve to the source's | `--add-dir=$REMUDA_HOME/shared/claude` and `CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD=1` in the child environment |
  | Settings | `settings.json` resolves to the source's | the part of the source's `settings.json` that neither the home nor the project defines, in the single `--settings` |
  | Plugins | `plugins/` resolves to the source's | `--plugin-dir=<install path>` for each plugin enabled in the source's settings |
  | Auto-memory | `projects/` resolves to the source's | `autoMemoryDirectory`, in the single `--settings` |

- **Instructions.** `$REMUDA_HOME/shared/claude/` contains exactly one entry, a symlink `.claude`
  pointing at the source home; remuda creates or corrects it before a launch that needs it. Basis
  (verified on 2.1.281): with the environment variable set, `--add-dir=<dir>` loads
  `<dir>/.claude/CLAUDE.md` and the skills, commands, and agents under `<dir>/.claude/` with their
  plain names, also through a `.claude` symlink; it does not load `<dir>/.claude/settings.json`.
  Plugins were rejected for this component because plugin items are namespaced (`name:item`), which
  would rename every agent and skill. Side effects, documented rather than prevented: tools may
  access that directory like any `--add-dir`, and the variable also loads `CLAUDE.md` from other
  `--add-dir` directories the user passes.
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
  organization are removed from the injected settings, whatever the home defines, so a member never
  authenticates or bills as the source, and no secret of the source reaches a member's tools:
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
  on stderr. A settings file of the source or home that is not a JSON object is an error for the
  launch.
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
  - The start directory is the launch cwd (for a resume or relay, `cwd_last`), made absolute,
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
- **Order.** Injected options come before the user's arguments, each in the `--option=value` form,
  so that a variadic option (such as `--add-dir`) cannot consume the user's arguments. The launch
  log (R6) records the injected option names and the byte size of each value, not the values.
- `run` stays a fast path (R6): injection reads a handful of settings files and
  `installed_plugins.json`, runs no subprocess, and scans no sessions.

## R19. Relay: continuing a session under another account (M2.5)

A relay continues a session under an account whose `projects` store does not contain it (verified
on 2.1.281: `--resume <id>` in another home fails with "No conversation found"). The original
session is never modified: the relay forks it, and the fork belongs to the target account, so
every session still belongs to exactly one account.

- **Entry points.** `remuda relay <session> <account>` (`<session>` is a full session ID from the
  index) and `c` in History / Live (R16). Claude sessions only.
- **Refused** when the target is not a claude account, when the target's `projects` store (realpath)
  already holds the transcript (use a fork instead), when the transcript's `cwd_last` does not
  exist (R6), or when the session or target account cannot be resolved.
- **Copy.**
  1. The selected transcript (R16: the row, not a lookup by ID) is copied to
     `<target home>/projects/<dir>/<id>.jsonl`, where `<dir>` is the name of the directory holding
     the transcript in its store. The copy ends at the last complete line, so a transcript being
     written (a running session) never yields a partial record; a transcript with no complete line
     is refused.
  2. The checkpoints in `file-history/<id>/` are copied from every home whose `projects` resolves
     to the transcript's store (a session may have run under several homes sharing a store), as
     a union: checkpoint files are content-addressed and immutable, so a name found in two homes is
     the same file. Missing checkpoints are not an error.
  Basis (verified on 2.1.281): a fork copies the checkpoints of `file-history/<id>/` to the new ID;
  without them the fork works but `/rewind` cannot reach points before the fork.
- **Order and failure.** Checkpoints are copied first and the transcript is placed last. If anything
  after placing the transcript fails before claude starts (writing the launch log, handing over
  the terminal), remuda removes the transcript copy it just placed (its own file) and reports the
  error; the launch does not proceed without its log record.
- **Overwrite rules.** Copies are written to a temporary name and renamed into place. An existing
  destination transcript is replaced only if the launch log records it as an earlier relay copy
  and its size and mtime still equal the recorded ones; otherwise the relay is refused.
  Checkpoint files are immutable (`<hash>@v<n>`): existing ones are kept, missing ones are copied.
- **Launch.** Under the target account, in `cwd_last`, with R18 injection:
  `claude --resume <id> --fork-session --session-id <new uuid>`. The launch log records `fork_of`
  and a `relay` object: the source transcript path, the copied paths, and the copy's size and
  mtime.
- **Index.** A transcript recorded as a relay copy is hidden from History and is not attributed to
  the target account; the original row keeps its attribution. If `state/` is deleted (R3), copies
  reappear as the same ID in two stores, which R16 already handles.
- A relay is allowed while the session is running, like a fork (R16).

## R20. Token statistics

Token counts per account and model, read from the agents' own transcripts, for a period. No cost is
estimated, and computing them runs no agent command.

- **Counts.** Input, cache read, cache write, output, and reasoning. The total is input + cache
  read + cache write + output (reasoning is part of output). A count a provider does not record is
  shown as `-`: claude records no reasoning apart from output, codex no cache write.
- **Claude** (verified on 2.1.71–2.1.281 against 1,064,472 records in 19,431 transcripts): an
  assistant record (`"type": "assistant"`) carries `message.id`, `message.model`, and
  `message.usage` with `input_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens`
  (cache write), and `output_tokens`. Claude writes one record per content block, each repeating
  the message's usage, with `output_tokens` growing while the message streams. So a message is
  counted once, by its `message.id` (in the corpus no id was shared by two requests), with the
  largest value of each count among its records and the timestamp of its first. Not counted:
  other record types (a `progress` record repeats a subagent's message, which is counted from the
  subagent's transcript; a tool result's `usage` sums a subagent's), records without
  `message.id`, and the model `<synthetic>` (claude's placeholder messages, whose usage is zero).
- **Advisor calls.** `usage.iterations` lists the requests behind a message, and the top-level
  usage is the sum of its `message` entries (12,735 of 12,741 records). An `advisor_message`
  entry is a separate request to its own `model`, not included in the top-level usage; each is
  counted under that model.
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
  relay (R19) copies a transcript whole. So each claude message is counted once across all stores
  by its `message.id`, and each codex request once across all rollouts by its
  `total_token_usage`. In the corpus, 2,360 of the 2,362 totals found in more than one rollout
  came from forks; the other two were the first requests of unrelated `codex exec` runs with
  identical usage, which are counted once (a known limitation). The copy that counts is, in
  order: one not known to be a copy (a `forkedFrom` record, or a transcript the launch log records
  as a relay copy, R19), the one with the earliest timestamp, the one whose path sorts first.
- **Accounts.** A message counts for the session of the transcript whose copy counts. A claude
  session's accounts are those of R9 without running sessions (the launch log and
  `history.jsonl`); a codex rollout's are the accounts of its home (R17). A session attributed to
  several accounts is counted once, for those accounts together (`max + team`); a claude session
  attributed to none is counted as unattributed. The sections therefore add up to the overall
  total.
- **Periods**: today, the last 7 days, the last 30 days, all. A period starts at local midnight
  (the system time zone) of today, of 6 days before, or of 29 days before; a message is in it when
  its timestamp is not earlier than the start. All also includes messages without a timestamp.
- **Shown**, for a period: every account in registry order (including those with nothing in the
  period), then each group of accounts, then unattributed. Each lists the tokens per model (the
  model id as recorded), most first, and their total. Then the tokens per model over everything.
  Counts below 1,000 are shown whole, others in K, M, B, or T, with one decimal below 100
  (`1.2M`, `93.3B`, `118K`).
- **Only transcripts that exist count**: tokens of transcripts deleted since (claude deletes those
  older than `cleanupPeriodDays`) are no longer counted.
- **Cache**: `$REMUDA_HOME/state/stats.json`, with a schema version, rebuilt on a mismatch,
  written atomically, deletable at any time (R3). For each transcript it holds what was counted
  from it (a 64-bit FNV-1a hash of each request's key, its timestamp, model, and counts), how far
  the transcript was read, and, for codex, the last total and model. Transcripts are read like the
  index (R8): an unchanged file is not read again, a grown one only from its last complete line,
  any other one whole; only complete lines are parsed. Records of one message read in two
  refreshes merge by their key. The first computation reads every transcript whole (measured:
  20,895 files, 17.2 GB).
