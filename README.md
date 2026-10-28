<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/brand/lockup-reverse.svg">
  <source media="(prefers-color-scheme: light)" srcset="assets/brand/lockup.svg">
  <img src="assets/brand/lockup.svg" alt="Remuda" width="280">
</picture>

**Use several Claude Code and Codex logins as one.**

Each Claude Code login lives in its own home directory, with its own settings, memory and
sessions. Remuda links those homes to one configuration, one memory and one session store, so
your accounts differ only in their login and its usage limits. It shows what each account has
left and when it resets, and starts, resumes or forks a session as whichever account you choose,
from one TUI or the command line. Codex accounts work too, with fewer features.

Remuda never reads, copies or moves a login, and makes no network requests of its own: every
session is the agent itself, with that account's home selected. It is not a gateway or a proxy.

The Accounts view, rendered at 100 columns with made-up accounts:

```text
 remuda  1 Accounts  2 Live  3 History  4 Stats                                   ?: help · q: quit
Accounts ───────────────────────────────────────────────────────────────────────────────────────────
ACCOUNT        EMAIL               ORG      PLAN 5H    WEEK  Fable AS OF
default        me@example.com      Personal max    34%   77%   41% cached 5m ago
work           me@example.org      Acme     team   12%   52%   18% cached 2m ago
side           me+side@example.com Personal pro    91%   96%     - cached 2h ago
codex:research logged in (ChatGPT) -        -        -   23%     - cached 30m ago
Resets · next 7 days ───────────────────────────────────────────────────────────────────────────────
               now      +1d       +2d      +3d      +4d      +5d       +6d    +7d next
default        ·S·······|·········f········W········|········|·········|········| S 2h00m W 3d00h
work           S········|·········|········|········|·······W|·········|········| S 1h00m W 4d21h
side           S······W·|·········|········|········|········|·········|········| S 40m W 18h00m
codex:research ·········|·········|········|········|········|·········|·W······| W 6d06h
               S 5h · W week (all models) · f week (Fable)
Checks ─────────────────────────────────────────────────────────────────────────────────────────────
! ANTHROPIC_API_KEY is set: it overrides every account's /login
· side: does not share rules with claude:default; to share an item: ln -s /Users/you/.claude/<item>
  /Users/you/.remuda/homes/claude/side/<item>

 n: new session · p: account · s: set up · D: remove · u: live usage · r: refresh
```

## Install

Requires macOS or Linux, Rust 1.88 or later, and `claude` on `PATH` (`codex` too, for Codex
accounts).

```sh
cargo install --locked --git https://github.com/ya-luotao/remuda
```

## Quick start

1. **Your login is already an account.** `default` is `~/.claude` (and `codex:default` is
   `~/.codex`); nothing needs registering.
2. **Share one setup.** Create `~/.remuda/config.toml` with:

   ```toml
   [share.claude]
   from = "default"
   ```

   Every account you set up from now on shares what `default`'s home has of its sessions,
   settings, `CLAUDE.md`, skills, commands, agents, hooks, plugins, rules and memory; only its
   login is its own ([what is linked](docs/GUIDE.md#one-setup-for-every-account)).
3. **Log in a second time.** `remuda setup work` creates a home under `~/.remuda/homes/`, links
   it to `default`'s and runs `claude auth login`. A home you already have is registered as it
   is with `remuda add work ~/.claude-work`; the Accounts view then lists the links it is
   missing.
4. **See what is left.** `remuda` opens the TUI. Accounts shows every account's limits and
   resets; `u` asks each agent for a live reading. `remuda usage` prints the same.
5. **Launch as an account.** `remuda run work` starts `claude` as `work`; anything after the name
   goes to `claude` unchanged (`remuda run work --model opus`). To make every launch go through
   Remuda, alias your main account: `alias claude='remuda run work'`.
6. **Continue any session.** History (`3`) lists the sessions of every account: `Enter` resumes
   one, `f` forks it, as an account whose store holds it.

## What you get

### Every account's limits on one screen

The five-hour, weekly and per-model limits of every Claude and Codex account, with all reset
times on one seven-day timeline. Readings come from the agents' own caches, instantly, or live
from the agents when you press `u` (`remuda usage --live`). `remuda usage` adds a note when a
window resets within the hour with a quarter or more left, and when it is on pace to run out
before its reset. [Usage limits](docs/GUIDE.md#usage-limits) in the guide has the details.

### Launch, resume and fork as any account

`remuda run <account>` replaces itself with the agent, started in the current directory with that
account's home selected. New Claude sessions get their session ID from Remuda, and every launch is
logged, so each session is shown under the account that ran it. In the TUI, `n` starts a session
as the selected account; History resumes or forks any session, and its card (`p`) lets you choose
the account with `R` and `F`; Live lists running Claude sessions, with attach, logs and stop for
background ones. The TUI refuses to resume a Claude session that is still running elsewhere.
[Launching and sessions](docs/GUIDE.md#launching-and-sessions) has the rest.

### One setup for every account

With `[share.claude]`, `remuda setup` links a new account's home to the source's: `projects`
(the sessions), `file-history`, `settings.json`, `CLAUDE.md`, `skills`, `commands`, `agents`,
`hooks`, `plugins`, `rules`, `agent-memory`, `output-styles` and `keybindings.json`. What
identifies a login stays per account: credentials, `.claude.json`, `history.jsonl` and
`sessions`. `p` on an account opens its card: login, usage, sessions, and everything a new
session in Remuda's directory would load, each item marked as its own or the source's.

Remuda also checks for what silently breaks such a setup: an `ANTHROPIC_API_KEY` that overrides
every login, a shared `settings.json` that sets authentication, a dangling symlink, a home that
is logged out. Problems are shown with `!`; a note, such as the items a member does not link, is
shown dimmed. [One setup for every account](docs/GUIDE.md#one-setup-for-every-account) explains
the layout and how to link a home by hand.

### Where the tokens went

`remuda stats` and the Stats view (`4`) count input, cache, output and reasoning tokens per
account (or per project) and model, from the agents' own transcripts, with an estimated cost at
API list prices; the Stats view adds a chart over time. For a subscription the cost is a measure,
not a bill.
[Statistics and cost](docs/GUIDE.md#statistics-and-cost) lists what is priced and how.

## Commands

| Command | What it does |
| --- | --- |
| `remuda` | Open the TUI |
| `remuda run [<account>] [args...]` | Launch the agent as an account; `args` go to it unchanged |
| `remuda usage [<account>] [--live]` | Usage limits and resets |
| `remuda sessions` | The newest sessions, with their accounts |
| `remuda stats [<account>] [--period ...] [--by project]` | Tokens and estimated cost |
| `remuda list` | Accounts, with their login and home |
| `remuda add <name> <path>` | Register an existing home |
| `remuda setup <name>` | Create a home, link it, and log in |
| `remuda remove <account>` | Unregister an account; its home stays |

Accounts are `name` or `provider:name`; `add` and `setup` take `--provider codex`. Options are
in `remuda help <command>`, and the guide's [Accounts and homes](docs/GUIDE.md#accounts-and-homes)
covers `config.toml`.

## TUI keys

| Key | Action |
| --- | --- |
| `1` `2` `3` `4` | Accounts, Live, History, Stats |
| `Enter` · `f` | Resume (History) or attach (Live) · fork |
| `n` | New session as the selected account |
| `p` | The card of the selected account or session |
| `u` · `r` | Live usage · refresh |
| `/` | Search History |
| `Ctrl-P` | Private mode: names, emails, paths and titles hidden, for screenshots |
| `?` · `q` | Every key · quit |

The full list is in the guide's [TUI reference](docs/GUIDE.md#tui-reference).

## Safety

- **Credentials are never read.** Identity and usage come from the agents' own commands and
  non-secret local metadata; Remuda does not read the Keychain or any token file.
- **Homes are never moved or rewritten.** Their paths are stored and passed byte for byte, since
  the agent may key its Keychain entry to that exact string.
- **Writes stay under `$REMUDA_HOME`** (`~/.remuda` unless set): the registry, `state/` and the
  homes `setup` creates. No account home is written into, except the links `remuda setup` makes
  in the home it has just created, before the login.
- **No network requests of its own.** A live reading is the agent asking, and only when you ask.
- **Your shell is left alone.** `claude` stays your native login unless you alias it.

The exact promises are in [SPEC.md](SPEC.md) (R2, R13) and [SECURITY.md](SECURITY.md).

## More

- [docs/GUIDE.md](docs/GUIDE.md): the full guide, from accounts and `config.toml` to checks,
  troubleshooting and private mode
- [CHANGELOG.md](CHANGELOG.md): what changed in each release
- [SPEC.md](SPEC.md): the behavior contract the tests enforce
- [CONTRIBUTING.md](CONTRIBUTING.md), [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) and
  [docs/DECISIONS.md](docs/DECISIONS.md): working on Remuda, and why it is built the way it is

Remuda is at version 0.x. macOS is the primary platform, Linux is supported and tested in CI,
and Windows is not supported. Until 1.0, a minor release may contain breaking changes; the
changelog lists them, with how to migrate.

## Acknowledgements

[magpie](https://github.com/yetone/magpie) by [yetone](https://github.com/yetone) is a local
gateway and model picker for coding agents. A study of it in October 2026 shaped Remuda's
statistics by project, the pace note of `remuda usage`, and the checks for environment variables
that quietly redirect every account. The two differ on purpose: Remuda runs no gateway and never
handles a credential.

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
