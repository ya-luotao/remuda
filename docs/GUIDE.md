# Remuda guide

Details behind the overview in the [README](../README.md). The normative behavior is specified in
[SPEC.md](../SPEC.md).

## TUI interaction

In an expanded preview, the movement keys scroll the preview instead of the list. In forms, `Tab`
or `↓` moves to the next field, `Shift-Tab` or `↑` to the previous one, `Enter` submits and `Esc`
cancels. Confirmations accept `y`; any other key except `Ctrl-P` cancels. In the account picker
opened by `remuda run` without an account, `Enter` launches the selected account and `Esc` or `q`
exits without launching anything.

Resuming a Claude session that is still running elsewhere is refused, because two processes
writing the same session would overwrite each other. Codex has no source of running sessions, so
resuming a Codex session in place always asks for confirmation. Forks and relays only read the
original session, so they are allowed while it runs.

The Stats view computes the statistics in the background the first time it opens, with the
reading progress in the status line, and again on each `r`; the first computation reads every
transcript whole.

## Private mode

`Ctrl-P` turns private mode on or off; the header then shows `PRIVATE`. It is off when the TUI
starts and is not saved. While it is on, the TUI shows:

- account names as aliases, `account-1`, `account-2`, … (`codex:account-1` for Codex), numbered
  when the TUI starts and stable while it runs; `default` is shown as is;
- emails as `•••@•••`, and organizations, session titles, first messages, session names, message
  previews, background session logs, search text and typed form values as `•••`;
- paths with every component masked, and `$HOME` as `~` (`~/•••/•••`);
- notices, errors and check messages with the above replaced, as a best effort: a path is
  recognized from a `/` or `~/` that begins a word, or from a directory remuda knows.

It keeps visible the numbers (usage percentages, reset times, token counts), model names, plans,
login methods, providers, session IDs, pids and times. It does not hide the output of an agent
after remuda hands it the terminal (the line remuda prints just before does follow private mode),
and the command-line commands have no private mode. In VS Code's integrated terminal on Linux and
Windows, `Ctrl-P` opens Quick Open; add `workbench.action.quickOpen` to
`terminal.integrated.commandsToSkipShell` with a leading `-` so the key reaches remuda.

## Shared configuration

Sessions stay with the account that created them, but configuration can be shared. Name the
account whose configuration the others get, typically `default` (whose home is `~/.claude`):

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
  `--add-dir=$REMUDA_HOME/shared/claude`, whose `.claude` entry is a symlink to the source home.
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

Nothing is written into any account home. Homes that already share part of their configuration
through symlinks are detected, and that part is not injected again. `share` applies to Claude
accounts only, and `from` must name a Claude account; anything else is a load error. The Accounts
view warns about problems such as a missing source home or a plugin whose install is gone.

## Relay

`remuda relay <session> <account>`, or `c` in the TUI, continues a Claude session under an account
whose session store does not have it, for example when the session's own account has run out of
usage. Remuda copies the transcript (up to its last complete record) and its checkpoints into the
target account's store, then runs `claude --resume <id> --fork-session` there, in the session's
last directory and with the shared configuration. The fork is a new session that belongs to the
target account; the original is never modified, and the copy is hidden from the session history.
A relay never overwrites anything except its own earlier copy of the same session, and only if
that copy is unchanged.

## Account checks

The Accounts view warns about conditions that silently break multi-account setups: an
`ANTHROPIC_API_KEY` that overrides every login, dangling symlinks, missing or logged-out homes, a
shared `projects` store without `cleanupPeriodDays`, and problems with the shared configuration.

## Data sources

- **Usage limits** come from what each agent records locally (Claude's usage cache in
  `.claude.json`, the rate limits in Codex rollouts), or, with `remuda usage --live` or `u` in the
  TUI, from asking the agent itself (`claude -p /usage`, `codex app-server`).
- **Identity** comes from `claude auth status --json` and `codex login status`. `remuda list`
  shows a Codex account's login method only; its email and plan appear after a live usage query.
- **Live sessions** come from `claude agents --json`. They are not available for Codex, because
  Codex exposes no machine-readable list of running sessions.
- **Token statistics** are counted from the agents' own transcripts. Each request counts once,
  even when a message is written in several records, a session is forked or relayed, or a store
  is shared by several accounts. No cost is estimated, and no agent is run.
- **Attribution:** new Claude sessions get a pre-assigned `--session-id`, and every launch is
  recorded in `state/launches.jsonl`, so each session can be attributed to the account that
  started it.

Where the agents' local formats are not public interfaces, parsing is best-effort: unrecognized
data degrades to missing fields rather than errors.
