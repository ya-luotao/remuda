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
its transcript: usually the account that created it, or any of the accounts whose `projects`
are symlinks to the same directory. Remuda does not copy sessions between accounts.

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
- notices, errors and check messages with the above replaced, as a best effort: a path is
  recognized from a `/` or `~/` that begins a word, or from a directory remuda knows.

It keeps visible the numbers (usage percentages, reset times, token counts, costs), model names, plans,
login methods, providers, session IDs, pids and times. It does not hide the output of an agent
after remuda hands it the terminal (the line remuda prints just before does follow private mode),
and the command-line commands have no private mode. In VS Code's integrated terminal on Linux and
Windows, `Ctrl-P` opens Quick Open; add `workbench.action.quickOpen` to
`terminal.integrated.commandsToSkipShell` with a leading `-` so the key reaches remuda.

## Shared configuration

Sessions stay with the account that created them, but configuration and memory can be shared.
Name the account whose configuration the others get, typically `default` (whose home is
`~/.claude`):

```toml
[share.claude]
from = "default"

[[account]]
provider = "claude"
name = "personal"
home = "/Users/you/.remuda/homes/claude/personal"
share = false          # this account keeps only its own configuration
```

Every other Claude account then starts its sessions (new ones, resumes, forks, `-p` runs; not
subcommands, `--help` or `--version`) with the source's configuration, injected as launch options
before your own arguments:

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
  `env.CLAUDE_CODE_USE_BEDROCK`) are never shared. If you pass `--settings` or
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

Nothing is written into any account home. Homes that already share part of their configuration
through symlinks are detected, and that part is not injected again. A home that shares `projects`
with the source through a symlink gets no memory location injected, so its agent memory is shared
only if `agent-memory` is a symlink too. What an account has for itself is not shared: the MCP
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
  since the cache was written counts as empty; old data never makes an exhausted window usable.
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
  their aliases; everything else is sent as written, so write accounts that way and keep secrets
  out of the notes. Credentials, emails, organizations, paths and session content are never sent.
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
rules limited to `paths` that are not applied where the rules are injected, and a home that
shares `projects` with the source but not `agent-memory`.

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
