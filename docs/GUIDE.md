# Remuda guide

The [README](../README.md) is the overview; this guide is the manual behind it: every command, key
and check, and what to do when something looks wrong. [SPEC.md](../SPEC.md) is the behavior
contract, and the entries named here (R10, R18, …) hold the exact rules.

## Accounts and homes

| Term | Meaning |
| --- | --- |
| provider | The agent an account belongs to: `claude` (Claude Code) or `codex` (Codex). Codex accounts show only their login method until a live usage query gives the email and plan, list no running sessions (Codex has no list to read), and share no configuration. |
| home | The directory where one login keeps its credentials, sessions and settings: `~/.claude` or the one `CLAUDE_CONFIG_DIR` names; `~/.codex` or `CODEX_HOME`. |
| account | A name for a home, unique within its provider and matching `[A-Za-z0-9_-]+`. Remuda launches an account by setting its provider's variable to the home. |
| `default` | Each provider's native login: the home used when the variable is unset. It needs no registering, and launching it removes the variable from the agent's environment. `codex:default` is listed only when `~/.codex` exists or `codex` is on `PATH`. |
| `provider:name` | How to name an account: `claude:work`, `codex:research`. A bare name will do while only one provider has it; when two do, Remuda lists both instead of guessing. Bare `default` means `claude:default`. |

- `remuda add [--provider claude|codex] <name> <path>` registers an existing home as it is,
  touching nothing in it. It warns when the directory does not look like a home, and refuses a
  taken name, a directory already registered under any spelling, and `~/.claude` (`default`).
- `remuda setup [--provider claude|codex] <name> [--email <EMAIL>]` creates a home at
  `$REMUDA_HOME/homes/<provider>/<name>`, [links it](#what-setup-links-and-when) if it is a Claude
  member of `[share.claude]`, registers it, and runs `claude auth login` (the email prefilled) or
  `codex login`. If the login fails, the account stays registered and Remuda prints the command to
  retry, such as `remuda run work auth login`.
- `remuda remove <account>` unregisters an account and prints the `remuda add` that registers its
  home again; the home is left as it is. `default` and the source of `[share.claude]` cannot be
  removed.
- `remuda list [--timeout <SECONDS>]` prints each account's login and home, from
  `claude auth status --json` (else `.claude.json`) and `codex login status`, 15 seconds at most.

Homes are registered where they are, and their paths kept byte for byte: Claude Code on macOS
names a login's Keychain entry after a hash of the home path string, so the same directory spelled
another way, even with a trailing `/`, is another, logged-out account. Remuda stores the path as
you registered it (absolute, `~` expanded once), passes it unchanged, and never moves, renames or
deletes a home ([R2](../SPEC.md#r2-home-path-invariant)).

### config.toml

`$REMUDA_HOME/config.toml` is the registry. `add`, `setup` and `remove` edit it and keep your
comments, and you may edit it by hand:

```toml
[share.claude]
from = "default"             # optional: the account the other Claude accounts share

[[account]]
provider = "claude"
name = "work"
home = "/Users/you/.claude-work"
share = false                # optional: this account shares nothing

[prices."claude-opus-4-6"]   # optional: USD per million tokens (see Prices)
input = 5
output = 25
```

Loading is strict: an invalid or duplicate name, a registered `default`, a relative home, `share`
on a Codex account, a `from` that names no Claude account, or a malformed `[prices]` table stops
every command with an error naming the file. Keys Remuda does not know are kept and ignored, except
a top-level `pick`: the `[pick]` tables of the removed `remuda pick` are an error until you delete
them by hand, and until then `add`, `setup` and `remove` change nothing. Remuda never rewrites the
file for you.

### $REMUDA_HOME

```text
$REMUDA_HOME/                   ~/.remuda unless set
├── config.toml                 the registry
├── homes/<provider>/<name>/    homes made by `remuda setup`, below real directories only
└── state/                      mode 0700, its files 0600
    ├── index.json              the session index
    ├── stats.json              the token statistics
    └── launches.jsonl          one line per launch
```

`launches.jsonl` keeps each launch's time, account, home, directory, session ID, and arguments as
typed, so a prompt given on the command line is in it; the caches keep titles and directories. So
`state/` is yours alone: one an earlier version left open is tightened at the next write, and a
launch log that cannot be made private is not written to (Remuda warns and launches). Anything in
`state/` may be deleted and is rebuilt, but sessions only `launches.jsonl` attributed lose their
account.

## Usage limits

Each window has a percentage used and a reset. Claude has a five-hour window, `5h`, a week of all
models, `Week (all models)`, and on some plans a week per model, such as `Week (Fable)`. Codex
names its windows by length (`5h`, `Week (all models)`, `<N>h window`, `<N>d window`), since a
plan's only window may be weekly, and reads its per-model limits live only. The Accounts view puts
every account's windows in one table and all their resets on one seven-day timeline, a letter per
window (named in the legend). `remuda usage [<account>]` prints the same as text.

### Cached and live readings

The `AS OF` column says which reading you see and how old it is: `cached 5m ago`, `live 0s ago`,
`live…` while a query runs, `live failed`, or `no cache`. A cached reading, the default, asks no
one: it is Claude's usage cache in `.claude.json`, or the rate limits Codex records in its
rollouts. A live reading asks every agent now, in parallel, each with its account's own login: `u`
in the TUI, or `remuda usage --live [--timeout <SECONDS>]` (90 seconds per account; exit 1 if a
query fails). Claude takes 2 to 20 seconds, Codex a second or two; neither updates the cache.

Claude's live query is `claude -p /usage --no-session-persistence --setting-sources ""
--strict-mcp-config`, a local command: no model call, no cost, no transcript. It loads none of the
account's settings files and starts none of its MCP servers, so your hooks do not run and the
settings' `env` does not apply; and Remuda removes `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` for
it, since with it `/usage` only repeats a reading from the last hour. When Claude gives no limits,
one line says why: `no usage limits told for now` (it could not get them just then), or `not
logged in to a Claude subscription, or using an API key`. Codex's live query is `codex app-server`,
asked for the rate limits and the account (email and plan), never for a change. It starts as Codex
does, so it may refresh the login token and writes Codex's own state into the home: that is why it
runs only when you ask.

When a window's reset has passed since its reading, what it holds now is unknown, so it shows no
percentage until a new reading: `reset` in the table, `-` and `reset since cached (<time>)` in
`remuda usage` (`reset since asked` for a live answer), and no marker on the timeline. Readings
without a severity of their own (live ones, and Codex's) are marked at 75% as a warning and 90% as
critical: in color in the TUI, as `!` and `!!` in `remuda usage`. A live reset time Remuda cannot
read is replaced on the timeline by the cached one, while that is ahead.

### The notes of `remuda usage`

`remuda usage`, cached or live, may add two notes under a window; the TUI shows neither.

- `note: resets in 42 min with 71% left`: the window resets within 60 minutes with at least 25%
  left, which then goes unused. The minutes are rounded up; a reset unknown or passed gets none.
- `note: at this pace 100% by Oct 30 10:55, 2h05m before the reset`: at the pace of the reading,
  the window is used up before its reset. A window of length `L` that resets at `R`, read at `t`
  with `u`% used, reaches 100% at `T = (R − L) + (t − (R − L)) × 100 / u`, and the margin is
  rounded down. The note needs all of these:
  - a known length: Codex's minutes, else the label (`5h` is five hours, `Week …` seven days);
  - a reset known and ahead, and a reading of known time (not a cache without one) taken after
    the window started;
  - at least 25% used, and less than 100%;
  - more of the window used than of its time passed;
  - `T` still ahead, and at least a minute before the reset.

## Launching and sessions

### remuda run

`remuda run <account> [args...]` replaces itself with the account's agent, `claude` or `codex`, in
the current directory with the account's home selected. Everything after the account goes to the
agent unchanged, a `--` included: `remuda run work --resume <id>`, `remuda run work -p "..."`,
`remuda run codex:research resume <id>`. `-h` and `--help` go to the agent too, so the help of
`run` itself is `remuda help run`. The agent gets your environment with its home variable set, or
removed for `default`; a member of `[share.claude]` may also get the
[memory variable](#memory-through-a-linked-projects).

Without an account, `remuda run` opens the TUI's account picker and takes no other arguments:
`Enter` launches the selected account, `Esc` or `q` exits, and `u` and `r` work as in the Accounts
view. An account removed while the picker was open is not launched.

Remuda puts nothing in your shell, and typing `claude` still runs your native login. To make an
account your default, alias it yourself: `alias claude='remuda run work'`. Only a launch through
Remuda is attributed in the launch log, gets the memory variable that lets a member with a linked
`projects` write memory without asking, and is warned before a session Claude would not save or
that reads another account's authentication settings. Subcommands such as `claude agents --json`
pass through untouched, and `command claude` skips the alias.

### Attribution

Transcripts do not say which account wrote them, so Remuda attributes a session from three sources:

1. The launch log. For a new Claude session, Remuda generates the ID, logs it, then starts claude
   with `--session-id <id>`; a fork (`--resume <id> --fork-session`) gets one the same way, and a
   resume is logged with the ID it names. No ID is given when an argument is a claude subcommand
   name (`agents`, `mcp`, `update`, …, even as a prompt: `-p update`), or a resume or continue
   option, `--help` or `--version`.
2. Running sessions: the account a session runs or ran under.
3. Each home's `history.jsonl`, for sessions from before Remuda, unless two accounts share it.

A session may belong to several accounts, or to none, which is normal for old and SDK sessions.
Codex chooses its own IDs: a Codex session belongs to the account whose home holds its rollout.

### Sessions Claude does not save

A terminal started from inside a Claude session (by its Bash tool, or a `screen` or tmux started
there) may carry `CLAUDE_CODE_CHILD_SESSION=1`. Claude then saves no transcript or prompt history
for an interactive session started in it: the session never reaches History and cannot be resumed.
Remuda cannot tell the marker from real nesting, so it passes it on and warns: `run` on stderr, a
TUI launch before it hands over the terminal (quit Claude to start again) and once Claude exits,
and the Accounts view. If the terminal is your own, set `CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1`
in it. Both are read as Claude reads a switch: `1`, `true`, `yes` and `on`, in any case, are on,
anything else off. `-p` runs, subcommands and Codex are not affected.

### History

View `3` lists every account's sessions, newest first: time, accounts, title (Claude's AI title or
Codex's thread name, else the first message) and directory. Teammate, SDK, and Codex `exec` and
subagent sessions stay hidden until you press `a`; `/` filters by a fuzzy search over title,
directory and accounts; the preview shows the latest messages, a tool call as `[tool: <name>]`.
`remuda sessions [--limit <N>]` prints the newest sessions, 30 by default. A directory the index
cannot read keeps its sessions as last indexed, and the status line says so.

`Enter` resumes the selected session and `f` forks it into a new one, leaving the original as it
is; both start in the session's last directory, which must exist.

- The account is the one the session is attributed to; with several or none, a picker opens,
  attributed accounts first. `R` and `F` in the session card let you choose.
- A Claude session can be continued only by an account whose `projects` store holds it: every
  account that links that store, or else the one whose home has it. Remuda never copies a session
  between accounts; the refusal says which `projects` to link.
- Resuming a Claude session that runs elsewhere is refused, naming its account and pid. Remuda
  asks every account just before the launch (`Esc` cancels) and refuses when it cannot confirm.
  A fork only reads the original, so it may run meanwhile.
- A Codex session is resumed by the account whose home holds it, and resuming it in place asks
  first, warning when the rollout was written in the last 10 minutes.

### Session card and Live

`p` or Space in History or Live opens the session card: the id, provider and entry point; the
accounts and whether it runs (pid or background id, and state); when it started and was last
active; its directory, transcript and store; then the preview. Its keys are in the
[TUI reference](#tui-reference).

View `2` lists the Claude sessions running now, for every account. Interactive ones are listed
only; background ones can be attached (`Enter`), shown with their logs (`l`), stopped (`x`; the
conversation is kept) and, once stopped, removed (`D`, which deletes the session and its worktree);
`a` shows the stopped ones. Codex sessions never appear here.

## One setup for every account

Claude accounts can share one session store and one configuration, so that any account resumes
any session and all of them load the same instructions, settings, plugins and memory; only the
login stays per account. Sharing is done with symlinks: in each member's home, a shared item links
to the same item in the source's home. `[share.claude] from` in `config.toml` names the source,
usually `default` (`~/.claude`). Every other Claude account is a member, unless its `[[account]]`
table sets `share = false`; Codex accounts take no part.

| Linked to the source's | Never linked: one per account |
| --- | --- |
| `projects`, `file-history`: the sessions with their auto-memory, and the backups `/rewind` needs | `.claude.json`: the login's identity, MCP servers and project trust |
| `settings.json`, read whole ([below](#settingsjson-and-authentication)) | `history.jsonl`: which account ran which session |
| `CLAUDE.md`, `skills`, `commands`, `agents`, `hooks`, `plugins`, `rules`, `output-styles`, `keybindings.json` | `sessions`: the running sessions, by account |
| `agent-memory`: the memory of subagents with `memory: user` | `remote-settings.json`, `policy-limits.json`, and the credentials |

### What setup links, and when

`remuda setup` links a new member's home once: in the directory it has just created, still empty,
before registering the account and before the login. It makes one link per item of the left column
that the source has, `<home>/<item>` pointing at `<source home>/<item>`, then lists what it linked
and what the source lacks. A link that fails is reported, and the rest are still made; when the
source's home is missing, nothing is linked. Remuda never touches those links again, and never
links a home registered with `remuda add`. Codex homes, and Claude homes made without
`[share.claude]`, start empty.

### settings.json and authentication

A home that links `settings.json` reads all of it, so `setup` checks the source's first. If it sets
an authentication or provider setting (`apiKeyHelper`, `forceLoginOrgUUID`, `env.ANTHROPIC_API_KEY`
and the rest of [R18's list](../SPEC.md#r18-shared-configuration)), or is not a JSON object,
`setup` leaves it out, names the keys but never their values, and asks you to give the new home a
`settings.json` of its own; until then, the account has no shared settings. That check happens
once: such a setting added to a linked file later reaches every account that links it. Remuda then
warns in the Accounts view and at each session launch of those accounts (`claude:work reads the
authentication settings of claude:default through its settings.json link: env.ANTHROPIC_API_KEY`),
but does not stop it. Keep one account's settings in its own `settings.json`, or in an account with
`share = false`. The login itself is no setting: it stays in the Keychain or the home's
`.credentials.json`.

Every account that shares `projects` cleans it up by its own `cleanupPeriodDays`, and Claude's
30-day default deletes everyone's old sessions there. Set it in the shared `settings.json`; a home
that shares `projects` without a `settings.json` needs one of its own that sets it.

### Linking a home by hand

A home registered with `remuda add`, or an item the source gained after `setup`, you link yourself,
while no session of that account runs: `ln -s ~/.claude/rules ~/.claude-work/rules`. If the home
already has a directory of that name, move its contents into the source's and remove it first:
`ln -s` onto a directory puts the link inside it. The Accounts view lists what each member does not
link, with the command; what is not linked is not shared. Once a plugin is installed from a member
account, its install is recorded through that member's `plugins` link, which must then stay:
without it, the plugin stops loading for every account. The member's account card says so.

### Memory through a linked `projects`

A member that links `projects` keeps its auto-memory in the source's store already, by a path
through the link. Claude grants write access to the memory directory by that path but checks the
resolved one, so every memory write would ask for permission. Each session launch of such a member
through Remuda therefore sets `CLAUDE_CODE_REMOTE_MEMORY_DIR` to the source's home: Claude names
the memory directory by the source's path and writes freely, and keeps the memory of user-scope
subagents in the source's `agent-memory/`, linked or not.

The variable is not set when you pass `--settings` or `--setting-sources` (Remuda says so), when
you set it yourself, or when a settings file chooses `autoMemoryDirectory`, where only an
`agent-memory` link shares subagent memory; nor for the source, an opted-out account, a member whose
`projects` is its own, or a command that is not a session. Claude does not document it, so it is
the least stable part of sharing; with it, `memory: local` subagents keep their memory under the
source's `projects/<project>/`.

### Account card

`p` or Space in the Accounts view opens the account card: the home, the login, the usage (each
window and the time to its reset, with the reading's age), the sessions in History and running, the
checks, and the account's part in sharing (alone, source, member, opted out, or source missing).
Below, for a Claude account, is what a new session in Remuda's own directory loads (`CLAUDE.md`,
agents, skills, commands, rules, synced claude.ai skills, plugins and what each adds, the settings
by key names and counts, the memory directories, the MCP servers), each item marked as the
account's own, already the source's (a link), set by the project, bundled with Claude, or set at
launch; `d` adds the details. The card comes from the same step that prepares a launch, so the two
agree: open it on each account to see that they all load the same thing. A Codex account's card
has the facts only.

## Statistics and cost

Remuda counts input, cache read, cache write, output and reasoning tokens (reasoning is part of
output; Claude does not record it apart) from the agents' own transcripts: Claude's `projects`
with subagent transcripts, and Codex's rollouts, archived ones too. Each request counts once, even
when Claude writes it in several records, a fork copies it, or several accounts share the store.
Transcripts Claude has cleaned up no longer count; sessions History hides do.

`remuda stats [<account>] [--period today|7d|30d|all] [--by account|project]` prints one period,
`all` by default; periods start at local midnight. By account, each account comes in registry
order, then sessions attributed to several accounts, counted once for them together
(`work + personal`), then unattributed ones, then the overall total they add up to. By project, each
directory a session started in gets a section, as recorded: two spellings are two projects, a
resumed session stays where it started, and `(no directory)` holds those that record none. With an
account, only its sessions count, without the overall section. A directory that could not be read
is named in an `Incomplete:` line after the table.

View `4` shows the same under a chart of the period's cost (or tokens, when nothing is priced) by
hour, day, week or month, with Overall and the models not priced first and a share bar per section:
`t` changes the period, `b` switches between accounts and projects, `r` computes again. The first
computation reads every transcript whole and takes tens of seconds on a large history (the TUI runs
it in the background, with progress); later ones read only what changed. It happens once more after
an upgrade that changes the statistics cache.

### Prices

Each request is priced at the provider's public API list price, built in as of 2026-10-21 (tables
in [R20](../SPEC.md#r20-token-statistics)): an estimate of what the usage would cost on the API,
not a bill. Claude's 5-minute and 1-hour cache writes are priced apart; fast mode doubles the price
on the models that offer it, and US-only inference adds 10% on the models from 4.6 on. Claude Haiku
5.5 costs five times its base prices for a request with more than 100K input tokens, cache
included. Codex's cache writes are priced at the 30-minute rate (1.25 times the input price from
GPT-5.6 on, the input price before), and a request with more than 272K input tokens at the
long-context price where the model has one. These thresholds apply per request.

A `[prices."<model>"]` table in `config.toml` prices a model Remuda does not know, or replaces a
built-in price, in USD per million tokens. `input` and `output` are required; `cache_read`,
`cache_write_5m` and `cache_write_1h` are optional (for Codex, `cache_read` is the cached-input
price and `cache_write_5m` the cache-write price), and a count without a price is not priced.
Prices run from 0 to 1,000,000, and no other key is allowed. A table applies to the model id as
recorded or without its trailing date (`claude-haiku-4-5` prices `claude-haiku-4-5-20251001`), and
sets base prices: the multipliers above still apply.

A cost followed by `+` (`$12.34+`) leaves out requests that could not be priced, and `-` means none
could; the models concerned are named under the table, or after Overall in the TUI. Not modelled:
the long-context premium of Claude Sonnet 4.5 and 4, batch and priority processing, Codex's fast
and priority processing and 12-hour cache writes, server tools such as web search, and data
residency other than US-only.

## Checks and troubleshooting

The Accounts view ends with checks for what silently breaks a setup of several accounts, run when
the TUI starts, on `r`, and when the account list changes. A warning, `!`, means something is
broken or overrides your setup; a note, `·` and dim, is a fact worth knowing, not a fault, and
follows the warnings, even after `✓ no problems found`. Checks name variables and settings, never
their values; the account card shows one account's checks the same way.

| Warning | Why it matters |
| --- | --- |
| `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN` or `CLAUDE_CODE_OAUTH_TOKEN` is set | it overrides every account's `/login` |
| `ANTHROPIC_BASE_URL` is set | every account's requests go to that endpoint instead of Anthropic's |
| `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` is set | `/usage` in a session repeats old readings; Remuda's live query removes it |
| `CLAUDE_CODE_CHILD_SESSION` is on | interactive sessions started here are [not saved](#sessions-claude-does-not-save) |
| a home is missing or not logged in | the account cannot be used |
| a live usage query failed | the agent's error says why |
| a dangling symlink in a home | its target is gone |
| a shared `projects` with an account that does not set `cleanupPeriodDays` | the 30-day default deletes everyone's sessions |
| the source of `[share.claude]` does not exist, or its home is missing | nothing is shared |
| a member's `.claude.json`, `history.jsonl` or `sessions` links to another account's | the logins mix, attribution is lost, or running sessions cannot be told apart |
| a member shares `projects` but not `file-history` | `/rewind` misses the backups of sessions from other accounts |
| a member shares `projects` but not `agent-memory`, and a settings file chooses `autoMemoryDirectory` | the memory of user-scope subagents is not shared |
| a member's `settings.json` links the source's, which sets authentication settings | the account reads them; give it its own file, or move them out |

Only members are checked for sharing, not the source or accounts with `share = false`. The one
note, per member, lists the source's items its home does not link, with
`ln -s <source home>/<item> <home>/<item>`. Without `projects`, it adds that the account does not
see or resume the source's sessions; when the source's `settings.json` sets authentication settings
or is not a JSON object, it says the file is not shared and why, instead of offering it.

### Troubleshooting

- **The first stats run is slow.** It reads every transcript whole, once; later runs read only what
  changed.
- **Every account shows the same login, or `/login` seems ignored.** An `ANTHROPIC_API_KEY`,
  `ANTHROPIC_AUTH_TOKEN` or `CLAUDE_CODE_OAUTH_TOKEN` in your environment overrides them all.
- **A live reading repeats an old one.** `/usage` in a session does while
  `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` is set; `u` and `--live` remove it. If they say
  `no usage limits told for now`, Claude could not get the limits just then: try again.
- **A session is missing from History.** It was started inside a Claude session
  ([not saved](#sessions-claude-does-not-save)), ended before its first message, or is hidden until
  `a`; an index that could not read a directory says so in the status line.
- **Memory writes ask for permission.** The member's session was not launched through Remuda, or
  the [memory variable](#memory-through-a-linked-projects) was not set for it.
- **A Codex live query changed the home.** `codex app-server` starts like Codex: it may refresh the
  login token and writes Codex's own state. Only `u` and `--live` run it.

## TUI reference

`remuda` opens the TUI, which needs a terminal, on four views: `1` Accounts, with the usage table,
the reset timeline and the checks; `2` Live; `3` History; `4` Stats. In a terminal at least 40 rows
tall, the header takes three rows, the mark's two tracks framing the tabs, with the wordmark and the
`PRIVATE` badge in the upper rail; a shorter terminal gets one line. Below the view, the status line
shows progress, notices and errors, and the hint line the keys at hand.

| Key | Where | What it does |
| --- | --- | --- |
| `1` `2` `3` `4`, `Tab`, `Shift-Tab` | views | switch view |
| `j` `k`, `↑` `↓`, `g` `G`, `Home` `End`, `PgUp` `PgDn` | views, cards | move or scroll: by one, to the first or last, by a page |
| `Enter` · `f` | History, Live | resume the session (Codex asks first), or attach a background one in Live · fork it |
| `p`, Space | History, Live · Accounts | open or close the session card · the account card |
| `n` · `s` | Accounts | start a new session as the selected account · set up a new account |
| `D` | Accounts · Live | remove the account, keeping its home · remove a stopped background session; both ask first |
| `l` · `x` | Live | show a background session's logs in the preview · stop it (asks first) |
| `a` | History · Live | show or hide teammate, SDK, and Codex `exec` and subagent sessions · stopped ones |
| `/` | History | search: type to filter, `Enter` keeps the filter, `Esc` clears it |
| `t` · `b` | Stats | next period: all, today, 7 days, 30 days · sections by account or by project |
| `u` · `r` | views, cards | query live usage for every account · refresh the index, logins, running sessions and checks, and the statistics once computed |
| `Esc` | anywhere | go back: close a card or the logs, clear the search, cancel a form or a launch check |
| `Ctrl-P` | anywhere | turn private mode on or off |
| `?` · `q` | views | show the keys (any key but `Ctrl-P` closes them) · quit; `Ctrl-C` quits anywhere |

In the session card, `Enter` and `f` work as in the list, and in Live so do `l`, `x`, `D` and `a`:

| Key | What it does |
| --- | --- |
| `R`, `F` | resume or fork, choosing the account: every Claude account, the session's own first; for Codex, the homes that hold it |
| `n` | start a new session in the session's directory, as its account |
| `y`, `Y` | copy the session id, or the transcript path, through the terminal (OSC 52; tmux needs `set-clipboard`) |
| `m` | read more of the transcript: the last 100, 500, then all messages, then 20 again |
| `[`, `]` | move to the previous or next session |
| `p`, Space, `Esc` | close the card |

In the account card, `n`, `u`, `D` and `r` work as in the view; `d` shows or hides the details
(descriptions, an agent's model, effort and tools, the entries of linked items and plugins); `y`
copies the home directory; `[` and `]` move to the previous or next account; `p`, Space, `Esc` or
another view closes it. The movement keys scroll either card. The picker of `remuda run` has none.

### Forms and confirmations

In a form, `Tab` or `↓` goes to the next field, `Shift-Tab` or `↑` back, `Enter` submits and `Esc`
cancels. The new-session form (`n`) asks for a directory (by default where Remuda was started; `~`
is expanded, and it must exist) and, for Claude, an optional session name passed as `-n`, refused
if it starts with `-` or is a claude subcommand. The setup form (`s`) asks for the name, an optional
email for the Claude login, and the provider. A confirmation takes `y`; any other key but `Ctrl-P`
cancels. A launch leaves the TUI's screen, prints what runs and hands the terminal over; when the
agent exits, the TUI lists the launch's warnings and refreshes. It reads `config.toml` again before
each launch, so an account another `remuda` removed meanwhile is not launched.

### Private mode

`Ctrl-P` turns private mode on or off anywhere, also in forms, the search, the picker, the key list
and confirmations, which stay open; the header then shows `PRIVATE`. It is off when the TUI starts,
it is not saved, and the command-line commands have none. While it is on:

- account names other than `default` show as `account-1`, `account-2`, … (`codex:account-1`),
  numbered when the TUI starts and stable while it runs, unregistered names included; emails show
  as `•••@•••`, and organizations as `•••`;
- paths show every component masked and `$HOME` as `~` (`~/•••/•••`), form values included; the
  session card hides the transcript and store entirely;
- titles, first messages, session names, previews, background logs, the search, typed values and
  the account card's descriptions show as `•••`;
- messages (notices, errors, checks, the account card's problems) mask all of the above. In one
  Remuda composes, each path is masked whole and the rest stays readable: the reason, what to do,
  a `/login` or `/rewind` it names. In text from elsewhere, such as an agent's output or a system
  error, Remuda cannot tell where a path ends, so a line is masked from its first word with a `/`
  to its end; a `/login` there counts as a path.

Numbers (percentages, reset times, token counts, costs, the chart), model names, plans, login
methods, providers, session IDs, pids, times, and the names of configuration items in the account
card stay visible. The agent's own screen, once Remuda hands it the terminal, is not masked; the
line Remuda prints just before is. In VS Code's terminal on Linux and Windows, `Ctrl-P` opens Quick
Open: add `-workbench.action.quickOpen` to `terminal.integrated.commandsToSkipShell`.

## Data sources and privacy

| What | Where it comes from |
| --- | --- |
| Usage | cached: Claude's usage cache in `.claude.json`, the rate limits in Codex's rollouts; live, when you ask: `claude -p /usage`, `codex app-server` |
| Identity | `claude auth status --json`, else the account recorded in `.claude.json`; `codex login status` |
| Sessions, statistics | Claude's `projects`; Codex's `sessions`, `archived_sessions` and `session_index.jsonl` |
| Running sessions | `claude agents --json`, else `sessions/<pid>.json` checked with `ps` |
| Attribution | the launch log, running sessions, and each home's `history.jsonl` |
| The account card | the home's and the project's files; from `.claude.json`, MCP server names and account ids only |

Remuda never reads credentials: not the Keychain, a home's `.credentials.json`, Codex's
`auth.json`, or the `*.key` files of running sessions. It makes no network requests of its own and
needs no API key: a live query is the agent asking with its own login, when you ask, and prices are
built in. It writes only `config.toml`, `state/`, and the homes `setup` creates with their links
([R13](../SPEC.md#r13-write-boundary)). The agent commands it runs write what the agent writes: a
launch, a login, `codex app-server`, and `codex login status`, which creates `tmp/` in the home.

The agents' local formats are not public interfaces, so Remuda reads them best-effort: what it does
not recognize becomes a missing field, never an error.
