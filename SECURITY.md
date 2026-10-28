# Security Policy

## Supported versions

Security fixes are made in the latest release only. Before 1.0, there are no maintenance branches:
upgrade to the latest release to receive fixes.

| Version | Supported |
| --- | --- |
| Latest release | Yes |
| Older releases | No |

## Reporting a vulnerability

Please do not report security vulnerabilities through public issues, pull requests or
discussions.

Report them privately through GitHub's private vulnerability reporting:

1. Go to <https://github.com/ya-luotao/remuda/security/advisories/new>.
2. Describe the issue, the affected version or commit, and the steps to reproduce it. Include the
   platform and the versions of `claude` or `codex` involved, if relevant.

You should receive an acknowledgement within a few days. The maintainers will investigate, keep you
informed of progress, and coordinate the disclosure with you. Fixed vulnerabilities are announced
in a GitHub security advisory and in the changelog, with credit to the reporter unless you prefer
otherwise.

## Scope

Remuda manages paths to directories that hold coding-agent credentials, and launches the agents
with one of those directories selected. By design ([SPEC.md](SPEC.md), R2, R13 and R18), it:

- never reads credentials: not the Keychain, not Codex `auth.json`, and not session `*.key` files;
- writes only to `$REMUDA_HOME` (`config.toml`, `state/`, and the homes created by
  `remuda setup`), and never into any account home, with one exception: the symlinks
  `remuda setup` makes in the home it has just created, while that directory is still empty and
  before the login, when `[share.claude]` is set (R12, R18). They point at the source account's
  session store and configuration, never at credentials, and nothing is replaced or removed.
  `remuda setup` creates that home below real directories only: it refuses a `homes` or
  `homes/<provider>` that is a symlink;
- adds no option for sharing, and none of another account's settings, to a launch: shared
  configuration is what a home links (R18), and the one thing a launch sets for it is
  `CLAUDE_CODE_REMOTE_MEMORY_DIR`, the source's home, for a member whose `projects` is linked;
  and does not link a `settings.json` that sets authentication settings (credential helpers,
  provider, endpoint or organization settings, and secret-like `env` entries) at the moment
  `remuda setup` creates a home;
- never moves, renames or deletes a home directory;
- makes no network requests of its own. Network use by agent commands, including live usage
  queries, is the agent's own (R10, R13).

One thing these guarantees do not cover, by design: a linked `settings.json` is the source
account's file, shared whole. Authentication settings added to it after `remuda setup` are
read by every account that links it. Remuda warns about it in the Accounts view and at every
launch of such an account, naming the settings and never their values, but does not prevent
it. Login credentials are not settings (they are in the Keychain, or in a home's
`.credentials.json`, which is never linked); keep authentication settings out of a shared
`settings.json`.

Any behavior that breaks one of these guarantees is a security issue and should be reported
privately. Examples include remuda reading or exposing credential material, writing outside
`$REMUDA_HOME` (also through a symlink inside it), making or changing a link in a home other
than the one `remuda setup` has just created, adding another account's settings to a launch
or linking a `settings.json` that sets authentication settings when a home is created, a
warning above that shows a setting's value, launching an agent with a different account's
home than the one requested,
passing untrusted data (such as a session ID from a file) to an agent in a way that is interpreted
as an option or command, or making a network request of remuda's own.

Vulnerabilities in Claude Code or Codex themselves are out of scope; report them to their
respective maintainers.
