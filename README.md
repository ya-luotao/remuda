<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/brand/lockup-reverse.svg">
  <source media="(prefers-color-scheme: light)" srcset="assets/brand/lockup.svg">
  <img src="assets/brand/lockup.svg" alt="Remuda" width="280">
</picture>

**One terminal for all your coding-agent accounts.**

Remuda is a multi-account and session manager for
[Claude Code](https://github.com/anthropics/claude-code) and [Codex](https://github.com/openai/codex).
If you have several logins, each with its own usage limits and its own sessions, remuda shows
them side by side: see which account has usage left, launch as it, and start, resume, fork or
hand off sessions in any directory, from one TUI or CLI.

Remuda never touches credentials and makes no network requests of its own.

## Highlights

- **Every account's limits on one screen.** Five-hour, weekly and per-model limits for each
  Claude and Codex account, with all reset times on one shared seven-day timeline.
- **Launch as any account.** `remuda run <account>` starts the agent with that account's home.
  Each launch is logged, so every session can be attributed to the account that started it.
- **All sessions in one place.** Search the history of every account, preview messages, and
  resume or fork any session. Running Claude sessions are listed too, with attach, logs and stop
  for background ones.
- **Relay when an account runs out.** Continue a Claude session under another account: it is
  copied into that account and forked there, and the original is never modified.
- **One configuration for all accounts.** Your `CLAUDE.md`, skills, commands, agents, settings,
  plugins and auto-memory from one account are passed to every other Claude account at launch,
  with nothing copied into their homes.
- **Checks for silent breakage.** Warnings for an `ANTHROPIC_API_KEY` that overrides every
  login, dangling symlinks, missing or logged-out homes, and similar multi-account pitfalls.
- **Token statistics and cost.** Input, cache, output and reasoning tokens per account and model,
  counted from the agents' own transcripts, with an estimated cost at API list prices and a chart
  over time.
- **Built for screenshots.** `Ctrl-P` switches the TUI into a private mode that hides names,
  emails, paths and session titles.
- **Out of your way.** Remuda does not take over your shell: typing `claude` still uses your
  native login, and named accounts are used only through `remuda`. Accounts are registered where
  they already are, and their homes are never moved or rewritten.

Codex accounts can be registered, set up, launched, indexed, resumed and forked, with their
usage limits and token statistics. Live sessions, relay and shared configuration are Claude-only.

## Installation

Requires macOS or Linux with `ps` on `PATH`, Rust 1.88 or later, and the `claude` CLI on `PATH`
(plus `codex` for Codex accounts).

```sh
cargo install --git https://github.com/ya-luotao/remuda
```

Or from a local checkout: `git clone https://github.com/ya-luotao/remuda && cd remuda && cargo install --path .`

## Quick start

```sh
remuda add work ~/.claude-work        # register an existing Claude home, as is
remuda setup personal                 # or create a new account and log in to it

remuda usage                          # usage left on every account (add --live to ask each agent now)
remuda                                # open the TUI: accounts, live sessions, history, stats

remuda run work                       # launch claude as `work`; extra args go to claude unchanged
remuda relay <session-id> personal    # continue a session under another account
```

Codex accounts work the same way: `remuda add --provider codex research ~/.codex-research`, then
`remuda run codex:research`.

## Commands

Accounts are referenced as `name` or `provider:name`. A bare name that exists under more than one
provider is an error that lists the candidates. `default` is the native login of each provider:
the bare name means `claude:default`, and the Codex one is `codex:default`.

| Command | Description |
| --- | --- |
| `remuda` | Open the TUI. Requires a terminal on standard input and output. |
| `remuda run [<account>] [args...]` | Launch the account's agent, replacing the `remuda` process. `args` are passed to the agent verbatim. Without an account, the TUI account picker opens first; that form takes no other arguments. |
| `remuda usage [<account>] [--live] [--timeout <SECONDS>]` | Print usage limits for every account, or for one. Without `--live`, reads the agent's local cache. With `--live`, asks each account's agent in parallel (`claude -p /usage`, `codex app-server`) and exits 1 if any query fails. `--timeout` applies to each live query (default 90). |
| `remuda list [--timeout <SECONDS>]` | Print every account with its login identity (email, organization and plan; for Codex, the login method only) and home. `--timeout` applies to each identity query (default 15). |
| `remuda sessions [--limit <N>]` | Print the newest sessions: time, attributed accounts, title and working directory (default 30). |
| `remuda stats [<account>] [--period today\|7d\|30d\|all]` | Print tokens per account and model for a period (default `all`); with an account, only the sections that include it. The first run reads every transcript whole, which can take tens of seconds on a large history; later runs read only what changed. Shows each model's estimated cost (≈ API list price, prices built in as of 2026-09-24; an estimate, not a bill). |
| `remuda add [--provider <claude\|codex>] <name> <path>` | Register an existing home directory as an account. The provider defaults to `claude`. |
| `remuda setup [--provider <claude\|codex>] <name> [--email <EMAIL>]` | Create a new home under `$REMUDA_HOME/homes/<provider>/<name>`, register it, and run the agent's login (`claude auth login` or `codex login`). `--email` prefills the Claude login. |
| `remuda remove <account>` | Unregister an account. Its home and everything in it are left in place, and its path is printed so `remuda add` can register it again. `default` and the source of `[share.claude]` cannot be removed. |
| `remuda relay <session> <account>` | Continue a Claude session under another Claude account, in the session's last directory, replacing the `remuda` process. `<session>` is a full session ID from the index. See [Relay](docs/GUIDE.md#relay). |
| `remuda help [<command>]` | Show help for remuda or a command. |

Account names match `[A-Za-z0-9_-]+`. Because `run` forwards `-h` and `--help` to the agent, use
`remuda help run` for the help of `run` itself.

## TUI

Four views, Accounts, Live, History and Stats, with a preview pane for the selected session in
Live and History and a configuration pane for the selected account in Accounts. Press `?` for the
key reference.

| Key | Action |
| --- | --- |
| `1` `2` `3` `4`, `Tab`, `Shift-Tab` | Switch view: Accounts, Live, History, Stats |
| `j` `k`, `↑` `↓` | Move the selection (Stats: scroll) |
| `g` `G`, `Home` `End`, `PgUp` `PgDn` | First / last row, page up / down |
| `Enter` | History: resume the selected session (Codex asks for confirmation first). Live: attach to a background session |
| `f` | Fork the selected session into a new session; the original is left unchanged |
| `c` | Continue the selected Claude session under another account (relay) |
| `p`, `Space` | Live and History: expand or collapse the preview. Accounts: show the selected account's configuration (instructions, plugins, settings, auto-memory, MCP servers, and where each comes from); press again to expand it, again to close it. `PgUp` `PgDn` scroll it |
| `n` | Accounts: start a new session with the selected account |
| `s` | Accounts: set up a new Claude or Codex account, as `remuda setup` does |
| `l` | Live: show a background session's logs in the preview |
| `x` | Live: stop a background session (asks for confirmation) |
| `D` | Accounts: remove the selected account from the registry; its home is kept (asks for confirmation). Live: remove a stopped background session (asks for confirmation) |
| `/` | History: fuzzy search over title, working directory and accounts |
| `a` | History: also show teammate, SDK and Codex subagent sessions. Live: also show stopped background sessions |
| `t` | Stats: next period (all time, today, last 7 days, last 30 days) |
| `u` | Query live usage for every account |
| `r` | Refresh the index, identities, live sessions and checks, and the statistics once the Stats view has been opened |
| `Esc` | Go back: collapse the preview or the configuration pane, close logs, clear the search, or cancel a form or pending launch check |
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
├── shared/claude/.claude          links to the source's CLAUDE.md, agents, skills and commands
└── state/                         caches (index, stats, launch log, shared settings); safe to delete
```

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
```

Homes must be absolute paths. The file is validated strictly on load: invalid or duplicate names,
a registered `default`, or a relative home are reported as errors naming the file. Everything under
`state/` is rebuilt on the next run.

Costs in the statistics use prices built into remuda (as of 2026-09-24); `[prices."<model>"]`
overrides a model's price or prices one remuda does not know (for Codex, `cache_read` is the
cached-input price).

With `[share.claude]`, every other Claude account launches with the source's instructions,
settings (without authentication or provider settings), enabled plugins and auto-memory, injected
as launch options; each account's own settings still take precedence, and `share = false` opts an
account out. See [Shared configuration](docs/GUIDE.md#shared-configuration) for exactly what is
passed and how.

## Safety guarantees

Remuda manages paths to directories that hold agent credentials, so its write boundary is part of
the specification ([SPEC.md](SPEC.md), R2 and R13):

- **Home paths are stored and passed byte-for-byte.** The agent may key its credentials to the
  exact path string (Claude Code on macOS names its Keychain entry after a hash of it), so remuda
  never rewrites, canonicalizes or adds or removes a trailing slash from a registered home.
- **Homes are never moved, renamed or deleted.** `remuda add` only records a name and
  `remuda remove` only forgets it; neither moves, copies, creates or deletes anything.
- **Writes are confined to `$REMUDA_HOME`:** `config.toml`, `state/`, `shared/`, and the empty
  directories created by `remuda setup`. The one exception is an explicit relay, which copies one
  transcript and its checkpoints into the target account's `projects/` and `file-history/`.
  Otherwise remuda never writes into any account home, and it never writes credentials,
  `.claude.json`, the Keychain, existing transcripts or `history.jsonl`.
- **Credentials are never read.** Identity and usage come from the agents' own commands
  (`claude auth status --json`, `codex login status`, `claude -p /usage`, `codex app-server`) and
  non-secret local metadata (the usage cache in `.claude.json`, the rate limits in Codex
  rollouts). Remuda does not read the Keychain, Codex `auth.json` or session `*.key` files.
- **No network requests of its own.** Live usage is queried by the agent itself, with the
  account's own login, and only when you ask for it (`remuda usage --live`, `u` in the TUI). A
  live query starts the agent: `codex app-server` behaves like launching Codex, so it may refresh
  the account's login token and writes Codex's own state into the home. Cost estimates use
  prices built into remuda; nothing is fetched.

## Status

Remuda is at version 0.1.0. Its behavior is specified in [SPEC.md](SPEC.md) and covered by
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

- [docs/GUIDE.md](docs/GUIDE.md): TUI details, private mode, shared configuration, relay, account
  checks and data sources
- [SPEC.md](SPEC.md): behavior specification; the contract that the tests enforce
- [ROADMAP.md](ROADMAP.md): milestones, planned work and open questions
- [CHANGELOG.md](CHANGELOG.md): release history
- [CONTRIBUTING.md](CONTRIBUTING.md): development workflow and release process
- [SECURITY.md](SECURITY.md): how to report a vulnerability
- [docs/VI.md](docs/VI.md): visual identity and logo assets

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
