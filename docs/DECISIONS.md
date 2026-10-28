# Design decisions

The decisions behind Remuda's design, with their reasons. Current decisions come first, then the
ones that were superseded, each with what replaced it. The behavior itself is specified in
[SPEC.md](../SPEC.md); [ROADMAP.md](../ROADMAP.md) has the status and what comes next.

## Current

- **2026-10-28 · The product is several logins used as one.** Remuda does three things: it
  shows what each account has left, it launches or resumes a session as any account, and it
  keeps one setup shared by every Claude account. Statistics come on top. Features that answered
  the same question a second way, or that daily use did not need, were removed: `remuda pick`
  with `usage --wait` and `[pick]` (a leftover `[pick]` is a load error, as for the Jev keys
  before it); the launch-time injection of shared configuration; the usage history with
  `usage --history`; and `stats --csv`. The pace of a window, the one part of the history
  report a decision needs, became a note of `remuda usage`. The five-hour window is labeled
  `5h`, since "session" names a coding session everywhere else in Remuda. Checks have two
  levels, so that a fact about a healthy setup is no longer shown as a warning. *Reason:* the
  maintainer's use showed `run` and the TUI in daily use and none of the removed features; each
  removal makes the README, the SPEC and the code shorter, and leaves one way to do each thing.
- **2026-10-04 · A linked `projects` gets `CLAUDE_CODE_REMOTE_MEMORY_DIR=<source home>` at
  launch.** claude grants its auto-memory directory by the literal path and then checks the
  resolved one, which through the link is under the source's home and outside the working
  directories: every memory write asked for permission, in every mode, past any allow rule
  (claude 2.1.288). The variable names the same directory by the source's path. It moves agent
  memory too, which a shared store wants anyway; the user's own `--settings`, variable or
  `autoMemoryDirectory` still win (R18). The R11 check for an `agent-memory` link next to it
  fires only where a settings file chooses `autoMemoryDirectory`.
- **2026-10-03 · Accounts share one session store and configuration through symlinks; only
  login state stays per account.** Any account can then resume any session, and configuration
  cannot diverge between accounts. "Only login state" cannot be one directory:
  `CLAUDE_SECURESTORAGE_CONFIG_DIR` separates the credentials but not `.claude.json`, so each
  account keeps its own `.claude.json`, `history.jsonl`, `sessions` and the files its
  organization sets; and no variable moves `projects`, so it can only be linked. `setup` makes
  the links in the home it has just created, once, before the login: the one exception to the
  write boundary. A home registered with `add` is never touched, only checked (R11, R12, R13,
  R18). Injection at launch, kept then as the fallback for homes without the links, was removed
  on 2026-10-28.
- **2026-10-03 · A linked `settings.json` is shared whole, authentication settings included;
  Remuda warns instead of enforcing.** The credentials of a login are in the Keychain or the
  home's `.credentials.json`, not in `settings.json`; authentication settings there are ones
  the user wrote. So `setup` does not link a `settings.json` that has them when it creates a
  home, and later ones are reported in the Accounts view and at every launch of an account that
  reads them through the link, by name (R11, R18).
- **2026-10-01 · No relay: a session is continued only by an account whose store holds it.** A
  relay copied a transcript and its checkpoints into another account's home, the one case in
  which Remuda wrote into a home; without it the write boundary has no exception (R13). With
  one shared store since 2026-10-03, there is nothing to copy. It was removed before any
  release contained it (R19).
- **2026-09-24 · Token statistics from the transcripts, deduplicated, in a cache of their
  own.** The agents record exact usage per request; `message.id` and codex's response IDs
  identify a request across repeated records, forks and copies in other stores. A row per
  request in `state/stats.json` keeps deduplication exact and needs no time zone; it stays out
  of `index.json`, which reads only head and tail windows (R20).
- **2026-09-24 · Cost is an estimate at API list prices.** Most accounts are subscriptions, so
  list prices are the only comparable figure. A built-in table keeps estimates free of network
  requests, `config.toml` overrides cover new models and price changes, and integer picodollars
  keep each request's cost exact, so sections add up to the total (R20).
- **2026-09-24 · Private mode is a redacted snapshot of the TUI state.** The screen is drawn
  from a copy of the state with names aliased and personal fields masked; every field is
  destructured, so a new one cannot reach the screen before it is decided how it is shown
  (R21).
- **2026-09-24 · Codex live usage and identity come from `codex app-server`, and only on an
  explicit live query.** Its answers are machine-readable and codex reads its own credentials,
  so `auth.json` stays unread. Starting app-server is like launching Codex (it may refresh a
  token, uses the network, writes state into the home, takes about 1.5 s), so it runs only
  when live usage is asked for; `remuda list` keeps `codex login status` (R4, R10, R10a).
- **2026-09-23 · Rust and ratatui.** A single binary and fast JSONL scanning; the Codex TUI is
  built on ratatui too.
- **2026-09-23 · Remuda does not take over the bare `claude` command and installs no shell
  wrapper.** Typing `claude` means the native login unless the user chooses otherwise (an alias
  to `remuda run <account>` is the user's own); named accounts go through Remuda.
- **2026-09-23 · An account is a registered home path, not a conventional directory.** The
  Keychain entry is bound to the exact home path string (R2), so an existing home can only be
  registered where it is, never moved into a conventional location.
- **2026-09-23 · Prefer the agents' machine-readable output over their internal files.**
  `claude agents --json`, `claude auth status --json`, `claude -p /usage`, `codex login status`
  and `codex app-server` are CLI interfaces, more stable than undocumented file formats.
- **2026-09-23 · New sessions get a pre-assigned `--session-id`.** The ID is known before the
  launch, so attribution is exact and `remuda run` can exec directly (R6, R9).

## Superseded

- **2026-10-21 · `pick` selects locally, without Jev.** It took the first ranked feasible
  candidate and its provider's `default_effort`, and kept headroom and pace ranking, session
  affinity, `--live`, `--wait` and `--run`. *Superseded on 2026-10-28:* `pick` was removed.
- **2026-10-01 · Memory is what accounts share: rules as copies, agent memory through
  `CLAUDE_CODE_REMOTE_MEMORY_DIR`.** With sessions kept per account, what must not diverge was
  what claude remembers and is told; rules of an added directory load only from regular files,
  so they were copied under `$REMUDA_HOME/shared` at launch. *Superseded:* sessions are shared
  since 2026-10-03, and the rule copies went with the injection on 2026-10-28. The memory
  variable remains, for a linked `projects` (2026-10-04).
- **2026-09-27 · `remuda pick`: rules decide what is feasible, Jev chooses among it, taken
  only when confident.** Hard limits were facts Remuda could check; what a model added was
  weighing the user's notes against headroom, resets and staleness. *Superseded on 2026-10-21*
  (Jev removed) and on 2026-10-28 (`pick` removed).
- **2026-09-27 · Remuda's first network request: only `pick`, only with `TYPESAFE_API_KEY` and
  notes, through `curl`.** *Superseded on 2026-10-21:* the Jev path, the key and the transport
  were removed, and Remuda again makes no network requests of its own.
- **2026-09-24 · Share configuration by launch-time injection, not symlinks.** No writes into
  homes, no drift, new accounts share automatically; plugin packaging was rejected because it
  namespaces agent and skill names. *Superseded on 2026-10-03* by links, with injection kept as
  the fallback for homes without them; *removed on 2026-10-28*, having almost never run on a
  real setup.
- **2026-09-24 · Sessions are not shared between accounts; configuration is.** Exact
  attribution, no cross-account cleanup, work and personal sessions kept apart, and relay for
  switching accounts mid-session. *Superseded on 2026-10-03* by one shared session store; relay
  was removed on 2026-10-01.
