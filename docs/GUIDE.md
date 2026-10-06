# Remuda guide

Details behind the overview in the [README](../README.md). The normative behavior is specified in
[SPEC.md](../SPEC.md).

## TUI interaction

In a terminal at least 40 rows tall, the header is three rows and shows the remuda mark beside
the views; a shorter terminal keeps the one-line header, leaving the rows to the views.

In an expanded preview, the movement keys scroll the preview instead of the list. In forms, `Tab`
or `↓` moves to the next field, `Shift-Tab` or `↑` to the previous one, `Enter` submits and `Esc`
cancels. Confirmations accept `y`; any other key except `Ctrl-P` cancels. In the account picker
opened by `remuda run` without an account, `Enter` launches the selected account and `Esc` or `q`
exits without launching anything.

Resuming a Claude session that is still running elsewhere is refused, because two processes
writing the same session would overwrite each other. Codex has no source of running sessions, so
resuming a Codex session in place always asks for confirmation. A fork only reads the original
session, so it is allowed while it runs.

A Claude session can be resumed or forked only by an account whose `projects` directory holds
its transcript: any of the accounts whose `projects` are symlinks to the same directory, as in
the [shared layout](#shared-configuration), or else only the account that created it. Remuda
does not copy sessions between accounts; when an account cannot find a session, the message
says which `projects` to link.

The Stats view computes the statistics in the background the first time it opens, with the
reading progress in the status line, and again on each `r`; the first computation reads every
transcript whole. Above the table, a chart shows the period's cost (or tokens, when nothing is
priced) per hour, day, week or month, and each account's row shows its share.

## Private mode

`Ctrl-P` turns private mode on or off; the header then shows `PRIVATE`. It is off when the TUI
starts and is not saved. While it is on, the TUI shows:

- account names as aliases, `account-1`, `account-2`, … (`codex:account-1` for Codex), numbered
  when the TUI starts and stable while it runs; `default` is shown as is;
- emails as `•••@•••`, and organizations, session titles, first messages, session names, message
  previews, background session logs, search text and typed form values as `•••`;
- paths with every component masked, and `$HOME` as `~` (`~/•••/•••`);
- in the configuration pane, the names of agents, skills, commands, plugins, hooks, settings,
  `env` variables and MCP servers stay visible; descriptions are masked;
- notices, errors, check messages and the problems of the configuration pane with the above
  replaced. A path remuda puts there itself is masked whole, and what the message says around
  it stays readable (the reason, what to do, a `/login` or `/rewind` it names). In text from
  elsewhere (an agent's output, a system error, a name read from a file) nothing says where a
  path ends, so a line is masked from its first word with a `/` to its end, and what follows
  the path on that line is hidden with it; a `/login` there is a path like any other.

It keeps visible the numbers (usage percentages, reset times, token counts, costs), model names, plans,
login methods, providers, session IDs, pids and times. It does not hide the output of an agent
after remuda hands it the terminal (the line remuda prints just before does follow private mode),
and the command-line commands have no private mode. In VS Code's integrated terminal on Linux and
Windows, `Ctrl-P` opens Quick Open; add `workbench.action.quickOpen` to
`terminal.integrated.commandsToSkipShell` with a leading `-` so the key reaches remuda.

## Shared configuration

Claude accounts can share one session store and one configuration, so that any account resumes
any session and nothing diverges between them; only the login stays per account. Name the
account whose home the others share, typically `default` (whose home is `~/.claude`):

```toml
[share.claude]
from = "default"

[[account]]
provider = "claude"
name = "personal"
home = "/Users/you/.remuda/homes/claude/personal"
share = false          # this account keeps only its own configuration
```

### The linked layout

The recommended layout is a home whose shared items are symlinks to the source's:

| Linked to the source | Never linked: one per account |
| --- | --- |
| `projects`, `file-history` (sessions, and the file backups `/rewind` needs) | `.claude.json` (the login's identity, MCP servers, project trust) |
| `settings.json`, `CLAUDE.md`, `skills`, `commands`, `agents`, `hooks`, `plugins` | `history.jsonl` (which account ran which session) |
| `rules`, `agent-memory`, `output-styles`, `keybindings.json` | `sessions` (running sessions), `remote-settings.json`, `policy-limits.json` (an organization's) |

`remuda setup <name>` makes these links for a new Claude account: in the home it has just
created, before the login, one link per item the source's home has, each pointing at
`<source home>/<item>`. It lists what it linked and what the source does not have. That is the
only time remuda writes into a home: it never adds, changes or removes a link afterwards, and
never in a home registered with `remuda add`. Codex homes, and Claude homes created without
`[share.claude]`, are created empty. The home is created under `$REMUDA_HOME/homes/<provider>`,
and `homes` and `homes/<provider>` must be real directories: if one is a symlink, `setup`
stops before creating anything.

`settings.json` needs care. A home that links it reads all of it, as its own user settings.
When `setup` runs and the source's `settings.json` sets authentication or provider settings
(such as `apiKeyHelper`, `env.ANTHROPIC_API_KEY` or `forceLoginOrgUUID`), or cannot be read,
`setup` leaves it out and names the settings that are the reason; that account then gets the
source's settings injected at launch, without them (see below). This is checked once, when
the home is created. A linked `settings.json` stays the source's file: a setting like these
that you add to it later is read by every account that links it. Remuda tells you, in the
Accounts view and at every launch of such an account:

```text
remuda: warning: claude:work reads the authentication settings of claude:default through its settings.json link: env.ANTHROPIC_API_KEY
```

but it does not stop it. Keep authentication out of the shared `settings.json`: settings that
belong to one account go in a home that does not link it (one that gets its settings by
injection, or one with `share = false`). The login itself is not in `settings.json`: it is in
the Keychain (or the home's `.credentials.json`), which is never shared.

A home that shares `projects` but has no `settings.json` of its own (the source's was left out,
or the source had none) should get a small one that sets `cleanupPeriodDays`: injected
settings reach sessions only, and the Accounts view warns about a shared `projects` whose
accounts do not all set it. `setup` says so.

In a home you registered with `remuda add`, or for an item the source got later, make the link
yourself, for example `ln -s ~/.claude/rules ~/.claude-work/rules`. If the home already has a
directory of that name, move its contents into the source's first: `ln -s` into an existing
directory creates the link inside it. Do this while no session of that account is running.
The Accounts view lists what a member's home is missing (see [Account checks](#account-checks)).

Two things to know about the layout: every linked account cleans up the shared `projects` by
its own `cleanupPeriodDays`, so set it in the shared `settings.json`; and a plugin installed
from a member account is recorded with a path through that home's `plugins` link, which must
then stay.

### Injection, the fallback

A home that does not link an item still gets it at launch. Every Claude account other than the
source starts its sessions (new ones, resumes, forks, `-p` runs; not subcommands, `--help` or
`--version`) with what its home does not link, injected as launch options before your own
arguments:

- **Instructions:** `CLAUDE.md`, skills, commands and agents, through
  `--add-dir=$REMUDA_HOME/shared/claude`, whose `.claude` directory holds one symlink per item
  the source home has (`CLAUDE.md`, `skills`, `commands`, `agents`), so a session reaches those
  four items of the source and not the rest of its home. remuda keeps the links current before
  each launch; a `.claude` from an earlier version, one link to the whole source home, is
  migrated in place. Restart a remuda TUI that was started before upgrading: until then it
  launches members without shared instructions, and says so.
- **Rules:** the source's `rules/**/*.md`, through the same `--add-dir`, as read-only copies
  under `.claude/rules/` there: claude does not load rules of an added directory through
  symlinks. remuda refreshes the copies before each launch, so change a rule in the source's
  home, not in the copy. A rule limited to files by `paths` in its frontmatter is not applied
  this way (claude ignores `paths` there); the Accounts view names such rules. They do apply in
  a home whose `rules` is a symlink to the source's, which makes sense for a home that links the
  other instruction items too: one that links some items and gets the rest injected loads the
  linked ones twice, and the Accounts view says so. A home that already links `CLAUDE.md`,
  skills, commands and agents to the source therefore needs a `rules` link as well, once the
  source has rules.
- **Settings:** the part of the source's `settings.json` that neither the account's own settings
  nor the project's `.claude/settings.json` and `.claude/settings.local.json` define, so the
  shared settings behave like user settings; identical hook entries are not added twice. They are
  passed as one `--settings` file, written with mode 0600 under `$REMUDA_HOME/state/settings/`.
  Authentication and provider settings (such as `apiKeyHelper`, `env.ANTHROPIC_API_KEY` or
  `env.CLAUDE_CODE_USE_BEDROCK`) are never injected. If you pass `--settings` or
  `--setting-sources` yourself, remuda injects no settings and says so.
- **Plugins:** `--plugin-dir` for each plugin the source enables and has installed, unless the
  account or the project turns it off, or the account has installed it itself.
- **Auto-memory:** the source's memory directory for the project (found the way claude finds it),
  so every account remembers the same things about it.
- **Agent memory:** subagents with `memory: user` keep their memory in the source's
  `agent-memory/`, through `CLAUDE_CODE_REMOTE_MEMORY_DIR` in the session's environment, set only
  together with the auto-memory location. The variable is not documented by claude, and it also
  moves the memory of `memory: local` subagents from the project's `.claude/agent-memory-local/`
  into the source's `projects/<project>/`.

Injection writes nothing into any account home, and a part a home links is not injected again;
a fully linked home gets nothing injected but the memory variable. Sessions cannot be injected:
an account whose `projects` is its own sees and resumes only its own sessions. A home that shares
`projects` with the source through a symlink gets `CLAUDE_CODE_REMOTE_MEMORY_DIR` set to the
source's home instead of a memory location: its memory is in the source's store already, but
claude asks for permission on every memory write that goes through the link, and not through
the source's own path. Its agent memory is the source's too. What an account has for itself is
not shared: the MCP
servers and project trust in its `.claude.json`. `share` applies to Claude
accounts only, and `from` must name a Claude account; anything else is a load error. The Accounts
view warns about problems such as a missing source home or a plugin whose install is gone.

## Account configuration

`p` in the Accounts view shows what the selected Claude account's sessions load, for a new
session in the directory remuda was started in: its `CLAUDE.md`, agents (with their model,
effort and tools), skills (with those turned off by `skillOverrides`, and overrides that name
no skill), commands, rules (those limited to paths marked), plugins with what each adds (agents,
skills, commands, hooks, MCP servers), a summary of its settings, its auto-memory directory, the
directory for the memory of user-scope subagents, and the names of its MCP servers. Each item is marked as the account's own, shared from the source of `[share.claude]`,
already the source's (a symlink), or not shared, with the reason. What is shared comes from the
same step that prepares a launch, so the pane and a launch always agree. Settings are shown by
key names and counts only; no values. Synced claude.ai skills are shown for the account's own
login only. Press `p` again to give the pane the whole screen, and `Esc` to step back.

## Recommendations

`remuda pick` answers "which account, model and effort should I launch now?" from each account's
usage (cached, or with `--live` queried first) and the `[pick]` table in `config.toml`:

```toml
[pick]
exclude = ["claude:team"]      # never recommended
prefer = ["claude:max"]        # breaks the last ties
min_headroom = 10              # percent left required on every window that applies
stale_after = 120              # minutes after which cached usage is marked stale
notes = """
Keep claude:max for long refactors. codex:work is the company's; weekdays only.
"""

[pick.claude]
models = ["claude-opus-5-5", "claude-fable-5-1"]   # in order of preference; the first is the default
efforts = ["medium", "high", "xhigh", "max"]       # from low to high
default_effort = "high"

[pick.codex]
models = ["gpt-6-astra"]
```

- **Rules first.** A pair of account and model is feasible when every window that applies to it
  has at least `min_headroom` percent left: the general windows, plus the per-model week of its
  family (`claude-fable-5-1` counts against `Week (Fable)`). Without `models`, remuda does not
  know the agent's default model: per-model windows are shown (`also`), never counted. Write
  accounts in `[pick]` as `provider:name`. A window whose reset has passed
  since the cache was written is of unknown usage: remuda names it (`reset since cached`) and
  neither counts it nor lets it block the pair, and a pair with no known window left ranks after
  every pair that has one; `--live` asks the agent instead. Old data never makes an exhausted
  window usable.
  Excluded and logged-out accounts are not feasible. The feasible pairs are ranked by model
  order, headroom (in 10-point bands), freshness, the sooner reset, `prefer`, and registry order.
- **Jev, when asked.** With `TYPESAFE_API_KEY` set and `notes` written, and something to choose
  (two options, or an effort to score), remuda sends one request to TypeSafe's Jev model through `curl` (the key on curl's standard
  input, never on its command line) and takes its choice when its confidence is at least 0.50,
  or its most probable account when that account's options add up to 0.70; otherwise, or on any
  error, the rules decide. With a single option only the effort is asked. The effort comes from
  Jev's score when it is confident and usable, else from `default_effort`.
- **What is sent.** Each feasible account under an alias (`claude:account-1`, as in private
  mode; `default` stays `default`) with its usage windows, the local weekday and time, your models
  and efforts, and your notes. In the notes, accounts written as `provider:name` are replaced by
  their aliases, registered or not, whatever is written around them (CJK text, punctuation, `-`,
  `_`); everything else is sent as written, so write accounts that way and keep secrets out of
  the notes. Credentials, emails, organizations, paths and session content are never sent.
  `remuda pick --print-request` prints the exact request without sending it; `--offline` never
  sends.
- **Output.** The account, model and effort, what decided (and the rules' choice when Jev chose
  otherwise), the binding window and its reset, how old the data is, the `remuda run` command,
  and every pair that is not feasible with the reason. `--json` prints the same, with every
  candidate. With nothing feasible, `pick` lists the reasons and exits 1.
- **Launching.** `remuda pick --run [-- <args>...]` launches the recommendation as `remuda run`
  would: claude gets `--model` and `--effort`, codex `-m` and `-c model_reasoning_effort=`,
  before your arguments; an option your arguments already set is left alone. Arguments that
  resume or fork a session are refused.

## Cost estimates

The statistics price each request at the provider's public API list price, built into remuda (as
of 2026-09-24). Claude's 5-minute and 1-hour cache writes are priced separately, as are fast mode
(twice the price on the models that offer it) and US-only inference (1.1 times the price on the
models from 4.6 on), as the transcripts record them. A Codex request with more than 272K input
tokens is priced at the long-context price on the models that have one.

To price a model remuda does not know, or to use another price, add a `[prices."<model>"]` table
to `config.toml`, in USD per million tokens:

```toml
[prices."claude-opus-4-6"]
input = 5
output = 25
cache_read = 0.50
cache_write_5m = 6.25
cache_write_1h = 10
```

`input` and `output` are required; `cache_read` (for Codex, the cached-input price),
`cache_write_5m` and `cache_write_1h` are optional, and a count whose price is left out is not
priced. The table applies to the model id as recorded, or to it without a trailing date
(`claude-haiku-4-5` also prices `claude-haiku-4-5-20251001`).

A cost followed by `+` (`$12.34+`) leaves out requests that could not be priced, and `-` means
nothing could be; the models concerned are named below the table. Not modelled: the long-context
premium of Claude Sonnet 4.5 and 4, batch and priority processing, and server tools such as web
search.

## Account checks

The Accounts view warns about conditions that silently break multi-account setups: an
`ANTHROPIC_API_KEY` that overrides every login, dangling symlinks, missing or logged-out homes, a
shared `projects` store without `cleanupPeriodDays`, and problems with the shared configuration:
a missing source home, a plugin whose install is gone, instruction items that would load twice,
and rules limited to `paths` that are not applied where the rules are injected.

For each member of `[share.claude]` it also checks the links of the home, and says which link
to make:

- `.claude.json`, `history.jsonl` or `sessions` that is a symlink to another account's: the
  logins get mixed up, sessions lose their attribution, or running sessions cannot be told
  apart. Each account needs its own.
- a home that does not share `projects` with the source: it does not see or resume the sessions
  there. This is a notice, not an error; its memory is still shared by injection.
- a home that shares `projects` but not `file-history` (`/rewind` does not find the file
  backups of another account's session), or not `agent-memory` where a settings file chooses
  `autoMemoryDirectory` (a launch then does not redirect memory, so the memory of user-scope
  subagents is not shared).
- a `plugins` link that installed plugins are recorded through: it must stay a symlink, or
  those plugins stop loading for every account.
- a `settings.json` linked to the source's while the source's sets authentication settings:
  the account reads them through the link (each of its launches says so too).

The first of these is checked even when the source's home is missing.

Accounts with `share = false` and the source itself are not checked.

## Data sources

- **Usage limits** come from what each agent records locally (Claude's usage cache in
  `.claude.json`, the rate limits in Codex rollouts), or, with `remuda usage --live` or `u` in the
  TUI, from asking the agent itself (`claude -p /usage`, `codex app-server`).
- **Identity** comes from `claude auth status --json` and `codex login status`. `remuda list`
  shows a Codex account's login method only; its email and plan appear after a live usage query.
- **Live sessions** come from `claude agents --json`. They are not available for Codex, because
  Codex exposes no machine-readable list of running sessions.
- **Token statistics** are counted from the agents' own transcripts. Each request counts once,
  even when a message is written in several records, a session is forked, or a store is shared
  by several accounts. Each request is also priced at the provider's public API list
  price (built in, as of 2026-09-24), which estimates what the usage would cost on the API; for
  subscription logins it is not a bill. No agent is run and nothing is fetched.
- **Recommendations** (`remuda pick`) read the same usage and `[pick]`; the only request remuda
  makes itself goes to TypeSafe, and only with a key and notes (see
  [Recommendations](#recommendations)).
- **Attribution:** new Claude sessions get a pre-assigned `--session-id`, and every launch is
  recorded in `state/launches.jsonl`, so each session can be attributed to the account that
  started it.

Where the agents' local formats are not public interfaces, parsing is best-effort: unrecognized
data degrades to missing fields rather than errors.
