<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/brand/lockup-reverse.svg">
  <source media="(prefers-color-scheme: light)" srcset="assets/brand/lockup.svg">
  <img src="assets/brand/lockup.svg" alt="Remuda" width="280">
</picture>

**One terminal for all your coding-agent accounts.**

Remuda is a multi-account and session manager for
[Claude Code](https://github.com/anthropics/claude-code) and [Codex](https://github.com/openai/codex).
If you have several logins, each with its own usage limits and its own sessions, remuda shows
them side by side: see which account has usage left, launch as it, and start, resume or fork
sessions in any directory, from one TUI or CLI. The accounts can share one configuration and one
memory, while each keeps its own sessions.

Remuda never touches credentials. Its only network request of its own is optional: `remuda pick`
asks TypeSafe's Jev model for a recommendation when `TYPESAFE_API_KEY` is set and you have written
notes for it.

## Is remuda for you?

- You are logged in to Claude Code or Codex **more than once** (work and personal, two plans, a
  team account) and keep switching `CLAUDE_CONFIG_DIR` or `CODEX_HOME` by hand.
- You want to know **which account has usage left** before you start, and when each limit resets.
- Your sessions are **spread over several homes** and you want one list to search, resume and
  fork from, whatever account started them.

Remuda is not a gateway or a proxy: it does not switch models or providers, does not route one
agent to another vendor's model, and never copies, moves or reads a login. If you want one agent
on another provider's model, or automatic failover between accounts inside the agent, look at
[magpie](https://github.com/yetone/magpie) instead (see [Acknowledgements](#acknowledgements)).

**Contents:** [First launch](#first-launch-in-five-minutes) · [Concepts](#concepts) ·
[How it works](#how-it-works) · [Highlights](#highlights) · [Commands](#commands) · [TUI](#tui) ·
[Configuration](#configuration) · [Safety guarantees](#safety-guarantees) ·
[Troubleshooting](#troubleshooting) · [Developing](#developing) · [Status](#status) ·
[Documentation](#documentation) · [Acknowledgements](#acknowledgements) · [License](#license)

## How it works

An account is a name for an agent home directory: the directory where that login keeps its
credentials, sessions and settings (Claude accounts can share the last two through symlinks to
one home). Remuda keeps the names in one registry, reads what each home holds, and launches the
agent with the home selected.

```text
     remuda run work         remuda  (TUI)          remuda usage · sessions · stats
            │                      │                            │
            └──────────────────────┼────────────────────────────┘
                                   ▼
                  ~/.remuda/config.toml   (accounts = named homes)
                                   │
       ┌───────────────────────────┼────────────────────────────┐
       ▼                           ▼                            ▼
  claude:default              claude:work                  codex:research
  CLAUDE_CONFIG_DIR unset     CLAUDE_CONFIG_DIR=           CODEX_HOME=
  (~/.claude, native login)   ~/.claude-work               ~/.codex-research
       │                           │                            │
       └───────── each home keeps its own login and usage limits ─────────┘
             remuda reads them and launches the agent in them;
             it never moves, rewrites or logs in to them itself
```

## Highlights

- **Every account's limits on one screen.** Five-hour, weekly and per-model limits for each
  Claude and Codex account, with all reset times on one shared seven-day timeline.
- **Launch as any account.** `remuda run <account>` starts the agent with that account's home.
  Each launch is logged, so every session can be attributed to the account that started it.
- **All sessions in one place.** Search the history of every account, preview messages, and
  resume or fork a session under an account that holds it. Running Claude sessions are listed
  too, with attach, logs and stop for background ones.
- **One session store, one configuration and one memory for all accounts.** A new Claude
  account's home is linked to one account's sessions, `CLAUDE.md`, rules, skills, commands,
  agents, settings and plugins, so any account can resume any session and nothing diverges;
  only the login stays per account. A home that is not linked gets the configuration and the
  memory locations passed at launch instead.
- **Checks for silent breakage.** Warnings for an `ANTHROPIC_API_KEY` that overrides every
  login, dangling symlinks, missing or logged-out homes, and similar multi-account pitfalls.
- **Token statistics and cost.** Input, cache, output and reasoning tokens per account and model,
  counted from the agents' own transcripts, with an estimated cost at API list prices and a chart
  over time.
- **Which account now.** `remuda pick` recommends the account, model and effort to launch, from
  every account's limits and your `[pick]` rules; with a TypeSafe key and notes, Jev chooses among
  what the rules allow. `--run` launches it.
- **Built for screenshots.** `Ctrl-P` switches the TUI into a private mode that hides names,
  emails, paths and session titles.
- **Out of your way.** Remuda does not take over your shell: typing `claude` still uses your
  native login, and named accounts are used only through `remuda`. Accounts are registered where
  they already are, and their homes are never moved or rewritten.

What each provider supports:

| Feature | Claude | Codex |
| --- | :---: | :---: |
| Register, set up and remove accounts | ✓ | ✓ |
| Launch, resume and fork sessions | ✓ | ✓ ¹ |
| Login identity | ✓ | ✓ ² |
| Usage limits, cached and live | ✓ | ✓ |
| Session history, search and preview | ✓ | ✓ |
| Token statistics and estimated cost | ✓ | ✓ |
| Recommendation of account, model and effort (`pick`) | ✓ | ✓ |
| Live sessions (attach, logs, stop) | ✓ | – |
| Shared session store, configuration and memory; the configuration pane | ✓ | – |

¹ Codex has no list of running sessions, so resuming a Codex session in place asks for
confirmation first. ² The login method only; the email and plan appear after a live usage query.

## Installation

Requires macOS or Linux with `ps` on `PATH`, Rust 1.88 or later, and the `claude` CLI on `PATH`
(plus `codex` for Codex accounts). `remuda pick` also needs `curl` on `PATH` to ask Jev when a key
is set; without it, the rules decide.

```sh
cargo install --git https://github.com/ya-luotao/remuda
```

Or from a local checkout: `git clone https://github.com/ya-luotao/remuda && cd remuda && cargo install --path .`

## First launch in five minutes

Your native login is already an account: `default` is `~/.claude` (and `codex:default` is
`~/.codex`). Nothing has to be registered for it.

1. **Register a second account.** Either an existing home, as it is, or a new one that remuda
   creates and logs in:

   ```sh
   remuda add work ~/.claude-work        # an existing Claude home; nothing in it is touched
   remuda setup personal                 # or: create ~/.remuda/homes/claude/personal and run `claude auth login`
   ```

   `remuda list` then shows each account with its home and its login identity: email,
   organization and plan for a Claude account, the login method for a Codex one.

2. **See what is left.** `remuda usage` prints every account's limits from the agents' local
   caches, in a second, without asking anyone; `remuda usage --live` asks each agent now.
   `remuda usage --history` shows how each window's usage went and its pace to the reset.

3. **Open the TUI** with `remuda`: `1` Accounts (limits, resets, checks), `2` Live sessions,
   `3` History of every account, `4` Stats. `?` lists the keys, `q` quits.

4. **Launch as an account.** `remuda run work` starts `claude` with that account's home; anything
   after the account name goes to `claude` unchanged (`remuda run work --resume <id>`,
   `remuda run work -p "..."`). In the TUI, `n` on an account does the same, and `Enter` on a
   session in History resumes it under an account that holds it.

5. **Let remuda choose.** `remuda pick` names the account, model and effort that your `[pick]`
   rules rank first among the options with room left on every window (model preference first,
   then headroom in bands, then freshness and reset time); `remuda pick --run` launches it. When
   nothing has room, `--wait` waits for a limit to reset and tries again.

Codex accounts work the same way: `remuda add --provider codex research ~/.codex-research`, then
`remuda run codex:research`. Where to go next: [Configuration](#configuration) to share one
session store and one configuration between your Claude accounts, the
[guide](docs/GUIDE.md) for the TUI, private mode and recommendations in detail, and
[Troubleshooting](#troubleshooting) when something looks wrong.

## Concepts

- **Account.** A name for an agent home directory: the directory where one login keeps its
  credentials, sessions and settings (`~/.claude`, or any `CLAUDE_CONFIG_DIR`; `~/.codex`, or any
  `CODEX_HOME`). Accounts are written `name` or `provider:name`; `default` is the native login
  of each provider. The registry is `~/.remuda/config.toml` ([SPEC R1–R3](SPEC.md)).
- **Home.** The directory behind an account. Remuda records its path byte for byte and never
  moves, rewrites or logs in to it; the agent may key its Keychain entry to that exact string
  ([SPEC R2](SPEC.md)).
- **Usage window.** A limit with a reset: Claude's five-hour session window and weekly windows
  (all models, and per model), and Codex's windows under the same labels by their length. Remuda
  reads them from the agents' caches, asks live on request, and records what it read for
  `usage --history` ([data sources](docs/GUIDE.md#data-sources)).
- **Launch and attribution.** Every launch is logged with its account. A new Claude session gets a
  session ID from remuda before it starts, so it can later be shown under the account that ran it
  and resumed as that account; a Claude session the log cannot name is attributed from the
  home's `history.jsonl`. Codex chooses its own IDs, so a Codex session is attributed to the
  accounts of the home it lives in ([SPEC R6, R9, R17](SPEC.md)).
- **Source and members.** With `[share.claude]`, one Claude account's home is the source whose
  session store and configuration the other accounts link or receive at launch; the login stays
  per account ([shared configuration](docs/GUIDE.md#shared-configuration)).
- **Pick.** The recommendation of account, model and effort, from rules over every window's
  headroom (or its pace to the reset) and, with a key, Jev's choice among what the rules allow
  ([recommendations](docs/GUIDE.md#recommendations)).
- **Private mode.** `Ctrl-P` in the TUI: names, emails, paths and session content masked, numbers
  kept, for screenshots ([private mode](docs/GUIDE.md#private-mode)).

## Commands

Accounts are referenced as `name` or `provider:name`. A bare name that exists under more than one
provider is an error that lists the candidates. `default` is the native login of each provider:
the bare name means `claude:default`, and the Codex one is `codex:default`.

| Command | Description |
| --- | --- |
| `remuda` | Open the TUI. Requires a terminal on standard input and output. |
| `remuda run [<account>] [args...]` | Launch the account's agent, replacing the `remuda` process. `args` are passed to the agent verbatim. Without an account, the TUI account picker opens first; that form takes no other arguments. |
| `remuda usage [<account>] [--live] [--timeout <SECONDS>] [--wait [--max-wait <SECONDS>]]` | Print usage limits for every account, or for one. Without `--live`, reads the agent's local cache. With `--live`, asks each account's agent in parallel (`claude -p /usage`, `codex app-server`) and exits 1 if any query fails. `--timeout` applies to each live query (default 90). With `--wait` and an account, waits until none of its windows has less than `[pick] min_headroom` percent left, then prints it; `--max-wait` gives up (exit 1) when the next check would come later. A window that resets within the hour with at least 25% left gets a note. What is read is recorded in `state/usage-history.jsonl` (45 days). |
| `remuda usage --history [<account>] [--days <N>]` | Print the usage recorded over the last `N` days (default 7): each current window point by point with its pace (ahead of or behind an even pace, and where that pace ends up at the reset), earlier windows a line each with their peak. Records nothing itself. |
| `remuda list [--timeout <SECONDS>]` | Print every account with its login identity (email, organization and plan; for Codex, the login method only) and home. `--timeout` applies to each identity query (default 15). |
| `remuda sessions [--limit <N>]` | Print the newest sessions: time, attributed accounts, title and working directory (default 30). |
| `remuda stats [<account>] [--period today\|7d\|30d\|all] [--by account\|project \| --csv]` | Print tokens per account and model for a period (default `all`); with an account, only the sections that include it. `--by project` gives a section per directory the sessions started in; `--csv` prints every request instead, with its exact cost, for reconciling with a bill (exit 1 if the report is incomplete). The first run reads every transcript whole, which can take tens of seconds on a large history; later runs read only what changed. Shows each model's estimated cost (≈ API list price, prices built in as of 2026-10-07; an estimate, not a bill). |
| `remuda add [--provider <claude\|codex>] <name> <path>` | Register an existing home directory as an account. The provider defaults to `claude`. |
| `remuda setup [--provider <claude\|codex>] <name> [--email <EMAIL>]` | Create a new home under `$REMUDA_HOME/homes/<provider>/<name>`, register it, and run the agent's login (`claude auth login` or `codex login`). With `[share.claude]`, a new Claude home is first linked to the source's session store and configuration. `--email` prefills the Claude login. |
| `remuda remove <account>` | Unregister an account. Its home and everything in it are left in place, and its path is printed so `remuda add` can register it again. `default` and the source of `[share.claude]` cannot be removed. |
| `remuda pick [--provider <P>] [--live] [--timeout <SECONDS>] [--offline] [--json\|--print-request] [--wait [--max-wait <SECONDS>]] [--run] [-- <args>...]` | Recommend the account, model and effort to launch now; with `-- --resume <id>` (or codex `-- resume <id>`), the account to resume that session as, preferring the one that ran it lately (its prompt cache is warm). Rules keep only what has at least `min_headroom` percent left on every window that applies (default 10) and rank it; with `TYPESAFE_API_KEY` set and `[pick] notes`, Jev chooses among those options. `--print-request` shows what would be sent, `--offline` never sends, `--run` launches the choice as `remuda run` does. `--timeout` applies to each `--live` query (default 90). Exits 1 when nothing is feasible; with `--wait`, waits until something is (it never queries live by itself), and `--max-wait` gives up when the next check would come later. See [Recommendations](docs/GUIDE.md#recommendations). |
| `remuda help [<command>]` | Show help for remuda or a command. |

Account names match `[A-Za-z0-9_-]+`. Because `run` forwards `-h` and `--help` to the agent, use
`remuda help run` for the help of `run` itself.

## TUI

Four views, Accounts, Live, History and Stats, with a preview pane for the selected session in
Live and History, and `p` for a card with everything Remuda knows of the selected session, or of
the selected account, and what to do with it. Press `?` for the key reference.

The Accounts view, with every account's limits and their resets on one timeline:

```text
 remuda  1 Accounts  2 Live  3 History  4 Stats                                   ?: help · q: quit
Accounts ───────────────────────────────────────────────────────────────────────────────────────────
ACCOUNT EMAIL           ORG PLAN SESSION WEEK  Fable SOURCE
default me@example.com  Org max      34%   77%  100% cached 5m ago
max     max@example.com Org max      12%   91%     - live 0s ago
team    not logged in   -   -          -     -     - no cache
Resets · next 7 days ───────────────────────────────────────────────────────────────────────────────
        now       +1d        +2d       +3d       +4d       +5d        +6d     +7d next
default ·S········|··········f·········W·········|·········|··········|·········| S 2h00m W 3d00h
max     S·········|··········|·········|·········|·········|··········W·········| S 1h00m W 5d23h
team    ··········|··········|·········|·········|·········|··········|·········|
        S session · W week (all models) · f week (Fable)
Checks ─────────────────────────────────────────────────────────────────────────────────────────────
! ANTHROPIC_API_KEY is set: it overrides every account's /login
! team: not logged in

 n: new session · p: config · s: set up · D: remove · u: live usage · r: refresh
```

| Key | Action |
| --- | --- |
| `1` `2` `3` `4`, `Tab`, `Shift-Tab` | Switch view: Accounts, Live, History, Stats |
| `j` `k`, `↑` `↓` | Move the selection (Stats: scroll) |
| `g` `G`, `Home` `End`, `PgUp` `PgDn` | First / last row, page up / down |
| `Enter` | History: resume the selected session (Codex asks for confirmation first). Live: attach to a background session |
| `f` | Fork the selected session into a new session; the original is left unchanged |
| `p`, `Space` | Live and History: open or close the session card: the session's id, provider and entry point, accounts and whether it runs, when it started and was last active, directory, transcript and store, above the preview, which `j` `k` `PgUp` `PgDn` scroll. In it, `Enter` and `f` resume and fork as below; `R` `F` resume or fork choosing the account; `n` starts a new session in the session's directory as its account; `y` `Y` copy the session id or the transcript path to the clipboard (OSC 52, through the terminal); `m` reads more of the transcript (100, 500, all messages); `[` `]` move to the previous or next session. Accounts: open or close the account card: the account's home, login, usage (each window with its reset), sessions (in History, running), checks and part in sharing, above its configuration (instructions, plugins, settings, memory, MCP servers, and where each comes from), which the movement keys scroll. In it, `n` `u` `D` `r` work as in the view; `d` shows the details (descriptions, an agent's model, effort and tools, the items a member gets through a link, a plugin's entries); `y` copies the home directory; `[` `]` move to the previous or next account |
| `n` | Accounts: start a new session with the selected account. Session card: one in the session's directory as its account |
| `s` | Accounts: set up a new Claude or Codex account, as `remuda setup` does |
| `l` | Live: show a background session's logs in the preview |
| `x` | Live: stop a background session (asks for confirmation) |
| `D` | Accounts: remove the selected account from the registry; its home is kept (asks for confirmation). Live: remove a stopped background session (asks for confirmation) |
| `/` | History: fuzzy search over title, working directory and accounts |
| `a` | History: also show teammate, SDK and Codex subagent sessions. Live: also show stopped background sessions |
| `t` | Stats: next period (all time, today, last 7 days, last 30 days) |
| `b` | Stats: sections by account or by project (the directory each session started in) |
| `u` | Query live usage for every account |
| `r` | Refresh the index, identities, live sessions and checks, and the statistics once the Stats view has been opened |
| `Esc` | Go back: close the session card or the account card, close logs, clear the search, or cancel a form or pending launch check |
| `Ctrl-P` | Turn private mode on or off, anywhere |
| `?` · `q`, `Ctrl-C` | Show the key reference (any key but `Ctrl-P` closes it) · Quit |

Forms, confirmations, resume rules and the exact scope of private mode are described in the
[guide](docs/GUIDE.md#tui-interaction).

## Configuration

Remuda keeps its files under `$REMUDA_HOME`, which defaults to `~/.remuda`:

```text
$REMUDA_HOME/
├── config.toml                    account registry; the single source of truth
├── homes/<provider>/<name>/       homes created by `remuda setup`
├── shared/claude/.claude/         links to the source's CLAUDE.md, agents, skills and commands;
│                                  copies of its rules
└── state/                         caches and logs (mode 0700); safe to delete, rebuilt on the
    │                              next run
    ├── index.json                 session index (mode 0600)
    ├── stats.json                 token statistics (mode 0600)
    ├── launches.jsonl             one line per launch: time, account, home, directory,
    │                              arguments, session ID (mode 0600)
    └── settings/                  shared settings passed to members (mode 0600)
```

`launches.jsonl` records the arguments of each launch as you typed them, so a prompt given on
the command line (`remuda run work -p "..."`) is in it. That is why `state/` and its files are
readable by you alone; a `state/` or a log that an earlier version left readable by others is
tightened the next time remuda writes there, except through a symlink, whose target keeps its
mode. remuda does not append to a log that others can still access after that (a symlink to
such a file, or a file whose mode it cannot change): it warns and launches anyway. Deleting
`launches.jsonl` loses the attribution of sessions started through remuda that no
`history.jsonl` records.

`config.toml` lists the registered accounts. `remuda add`, `remuda setup` and `remuda remove`
edit it for you, preserving comments and unknown keys, and it can also be edited by hand:

```toml
[share.claude]
from = "default"       # optional: share this account's configuration with the other Claude accounts

[[account]]
provider = "claude"
name = "work"
home = "/Users/you/.claude-work"

[[account]]
provider = "codex"
name = "research"
home = "/Users/you/.codex-research"

[prices."claude-opus-4-6"]   # optional: USD per million tokens, instead of the built-in price
input = 5
output = 25
cache_read = 0.50
cache_write_5m = 6.25
cache_write_1h = 10

[pick]                       # optional: what `remuda pick` may recommend
exclude = ["codex:research"]
affinity_minutes = 60        # resuming a session: how long its last account stays preferred
strategy = "headroom"        # or "pace": rank by what a reset would waste first
notes = "Keep claude:work for long refactors."   # sent to Jev, with account names aliased

[pick.claude]
models = ["claude-opus-5-5", "claude-sonnet-5"]  # in order of preference
efforts = ["medium", "high", "max"]
default_effort = "high"
```

Homes must be absolute paths. The file is validated strictly on load: invalid or duplicate names,
a registered `default`, or a relative home are reported as errors naming the file. Everything under
`state/` is rebuilt on the next run.

Costs in the statistics use prices built into remuda (as of 2026-10-07); `[prices."<model>"]`
overrides a model's price or prices one remuda does not know (for Codex, `cache_read` is the
cached-input price).

With `[share.claude]`, the other Claude accounts share the source's session store and
configuration: through symlinks in their homes (`projects`, `settings.json`, `CLAUDE.md`, skills,
commands, agents, hooks, plugins, rules, agent memory and more; `remuda setup` makes them, and
the Accounts view says which are missing in a home registered with `remuda add`), or, in a home
without the links, as options injected at launch (instructions, settings without authentication
or provider settings, enabled plugins and memory locations; sessions cannot be injected). The
login stays per account: `.claude.json`, `history.jsonl` and `sessions` are never linked, and
settings that choose credentials are neither linked at setup nor injected. `share = false` opts
an account out. [Shared configuration](docs/GUIDE.md#shared-configuration) in the guide has the
layout, what is linked and what is injected, and the warnings that go with a shared
`settings.json`.

## Safety guarantees

Remuda manages paths to directories that hold agent credentials, so its write boundary is part of
the specification ([SPEC.md](SPEC.md), R2 and R13):

- **Home paths are stored and passed byte-for-byte.** The agent may key its credentials to the
  exact path string (Claude Code on macOS names its Keychain entry after a hash of it), so remuda
  never rewrites, canonicalizes or adds or removes a trailing slash from a registered home.
- **Homes are never moved, renamed or deleted.** `remuda add` only records a name and
  `remuda remove` only forgets it; neither moves, copies, creates or deletes anything.
- **Writes are confined to `$REMUDA_HOME`:** `config.toml`, `state/`, `shared/`, and the
  directories created by `remuda setup`. The one thing remuda writes inside a home is the set
  of symlinks `remuda setup` makes in the directory it has just created, while it is still
  empty and before the login, when `[share.claude]` is set; after that, and in every home
  registered with `remuda add`, it writes nothing. `remuda setup` does not create a home
  through a symlink: `homes` and `homes/<provider>` under `$REMUDA_HOME` must be real
  directories, or it stops before creating anything. It never writes credentials,
  `.claude.json`, the Keychain, transcripts or `history.jsonl`.
- **Authentication settings are never injected.** Settings that choose credentials, a
  provider, an endpoint or an organization are removed from what remuda injects at launch, and
  `remuda setup` does not link a `settings.json` that sets them when it creates a home. A
  linked `settings.json` is the source's file, shared whole: such settings added to it later
  reach every account that links it. Remuda warns about that, naming the settings but never
  their values; it does not prevent it. Login credentials are not settings (they are in the
  Keychain, or in the home's `.credentials.json`) and are never linked.
- **Credentials are never read.** Identity and usage come from the agents' own commands
  (`claude auth status --json`, `codex login status`, `claude -p /usage`, `codex app-server`) and
  non-secret local metadata (the usage cache in `.claude.json`, the rate limits in Codex
  rollouts). Remuda does not read the Keychain, Codex `auth.json` or session `*.key` files.
- **Network requests of its own: only `remuda pick`, only with a key.** Live usage is queried by
  the agent itself, with the account's own login, and only when you ask for it (`remuda usage
  --live`, `u` in the TUI). A live query starts the agent: `codex app-server` behaves like
  launching Codex, so it may refresh the account's login token and writes Codex's own state into
  the home. Cost estimates use prices built into remuda; nothing is fetched. `remuda pick` sends
  one request to TypeSafe (`api.typesafe.ai`) when `TYPESAFE_API_KEY` is set and `[pick] notes`
  are written: each account's usage under an alias (`claude:account-1`), the models, and your
  notes as written (write accounts in them as `provider:name` so they are aliased too). It never
  sends credentials, emails, organizations, paths or session content. `--print-request` shows the
  request without sending it, `--offline` never sends, and without a key the local rules
  decide.

## Troubleshooting

- **The first `remuda stats` (or the first Stats view) takes long.** It reads every transcript and
  rollout whole, once; later runs read only what changed. The same happens once after an upgrade
  that changes the statistics cache ([cost estimates](docs/GUIDE.md#cost-estimates)).
- **Every account shows the same login, or `/login` seems ignored.** `ANTHROPIC_API_KEY`,
  `ANTHROPIC_AUTH_TOKEN` or `CLAUDE_CODE_OAUTH_TOKEN` is set in your environment and overrides
  every account's login; the Accounts view says so ([account checks](docs/GUIDE.md#account-checks)).
- **Live usage repeats an old reading.** `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` is set:
  claude's `/usage` then answers from its cache. Remuda removes it for its own live query, but
  claude launched as any account keeps it.
- **A session started through remuda is missing from History.** The terminal was started from
  inside a Claude session and carries `CLAUDE_CODE_CHILD_SESSION`, so claude saved no transcript
  for it. Remuda warns before such a launch and in the Accounts view; set
  `CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1` in that terminal if it is your own. `-p` runs are
  saved either way ([account checks](docs/GUIDE.md#account-checks)).
- **A Codex live query changed the account's state.** `codex app-server` behaves like launching
  Codex: it may refresh the login token and writes Codex's own state into the home. Only
  `--live` and `u` trigger it.
- **Something in the Accounts view is marked `!`.** Each check names what is wrong and, where there
  is one, the fix: a dangling symlink, a missing or logged-out home, a shared `settings.json` that
  sets authentication ([account checks](docs/GUIDE.md#account-checks)).

## Developing

Remuda is one Rust crate (a library and a binary), edition 2024, minimum Rust 1.88, for macOS
and Linux. The sixty-second version of [CONTRIBUTING.md](CONTRIBUTING.md):

```sh
git clone https://github.com/ya-luotao/remuda && cd remuda
cargo build && cargo run -- --help
cargo test                                      # safe on the machine you use the agents on
cargo fmt --check && cargo clippy --all-targets -- -D warnings   # with the tests, the three CI checks
```

- **The test suite is sealed.** Every test runs with a fresh `HOME` and `REMUDA_HOME` in a
  temporary directory, a cleared environment, and fake `claude`, `codex` and `curl` scripts
  first on `PATH`, so it never reaches your logins, Keychain, homes, sessions or the network
  ([hermetic tests](CONTRIBUTING.md#hermetic-tests)). It takes a few minutes: two tests wait a
  real minute to check `--wait`. Run it as a regular user, not root.
- **The specification comes first.** [SPEC.md](SPEC.md) is the contract, one anchor per promise
  (`R6`, `R20`, ...); the tests cite the anchors they cover, and a change to a promise lands with
  its tests in the same commit ([spec-first workflow](CONTRIBUTING.md#spec-first-workflow)).
  Behavior that is not in SPEC.md, such as screen layout and wording, may change freely.
- **A change, end to end:** amend or add the SPEC entry → write the test that cites it and is red
  without the fix → implement → `cargo fmt`, `clippy`, `test` → an entry under `## [Unreleased]`
  in [CHANGELOG.md](CHANGELOG.md) for anything user-visible → a pull request that says which test
  goes red with the fix reverted → a review of the diff before merge
  ([review gate](CONTRIBUTING.md#review-gate)).
- **Where to start reading:** [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) maps SPEC entries to
  modules and ends with a table, [Where to make a change](docs/ARCHITECTURE.md#where-to-make-a-change),
  that names the module and test file for the common kinds of change. [LESSONS.md](LESSONS.md)
  lists the rules that review rounds keep teaching.

```text
src/              the library: one module per concern (registry, launch, usage, index, stats, pick, ...)
src/cli.rs        the subcommands        src/tui/   the TUI (app state, workers, render, privacy)
src/owned.rs      every write below $REMUDA_HOME goes through here (SPEC R13)
tests/            integration tests, one file per area, each naming its SPEC anchors
tests/common/     the sealed sandbox: fake agents, fixture builders for transcripts and rollouts
examples/         read-only timing of the session index against your real claude or codex home
SPEC.md           the behavior contract      docs/ARCHITECTURE.md   the code map
docs/GUIDE.md     the user guide             CHANGELOG.md · ROADMAP.md · LESSONS.md
```

## Status

Remuda is at version 0.3.0. Its behavior is specified in [SPEC.md](SPEC.md) and covered by
tests, but the project is young: until 1.0, minor releases may contain breaking changes, which are
always listed in [CHANGELOG.md](CHANGELOG.md).

- **macOS** is the primary development platform.
- **Linux** is supported: the full test suite runs on Linux in CI. Remuda only sets or unsets the
  agent's home variable (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`); how the agent stores credentials on
  each platform is left to the agent.
- **Windows** is not supported. Remuda relies on Unix process, terminal and file APIs.

Remuda drives the `claude` and `codex` command-line interfaces and reads some of their local
files. Where those formats are not public interfaces, parsing is best-effort: unrecognized data
degrades to missing fields rather than errors.

## Documentation

- [docs/GUIDE.md](docs/GUIDE.md): TUI details, private mode, shared configuration,
  recommendations, account checks and data sources
- [SPEC.md](SPEC.md): behavior specification; the contract that the tests enforce
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): how the code is organized, with diagrams of the
  launch, indexing and TUI flows
- [ROADMAP.md](ROADMAP.md): milestones, planned work and open questions
- [CHANGELOG.md](CHANGELOG.md): release history
- [CONTRIBUTING.md](CONTRIBUTING.md): development workflow, review gate and release process
- [LESSONS.md](LESSONS.md): the rules that review rounds keep teaching, and which were promoted
- [SECURITY.md](SECURITY.md): how to report a vulnerability
- [docs/VI.md](docs/VI.md): visual identity and logo assets

## Acknowledgements

[magpie](https://github.com/yetone/magpie) by [yetone](https://github.com/yetone) (Go, MIT),
"Every agent's model. One place.", is a local gateway and model picker for coding agents. A study
of it on 2026-10-07 shaped several of remuda's features, as design rather than code: usage
rolled up by project and exported request by request for reconciling with a bill
(`stats --by project`, `stats --csv`), waiting for a limit to come back instead of failing
(`pick --wait`, `usage --wait`), usage recorded over time with a pace to the reset
(`usage --history`, the `pace` strategy of `pick`), and the checks for environment variables
that quietly redirect every account. The two tools then part ways on purpose: remuda runs no
gateway, switches no models or providers, never handles a credential and never fails over on
its own (SPEC R13, R23); magpie does those things, and does them well.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.

### Brand assets

The logo and other files in [`assets/brand/`](assets/brand/) are described in
[docs/VI.md](docs/VI.md). The wordmark is set in Manrope, which is licensed under the SIL Open Font
License 1.1; its copyright and license notice is retained in
[`assets/brand/OFL-Manrope.txt`](assets/brand/OFL-Manrope.txt).
