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
with one of those directories selected. By design ([SPEC.md](SPEC.md), R2 and R13), it:

- never reads credentials: not the Keychain, not Codex `auth.json`, and not session `*.key` files;
- writes only to `$REMUDA_HOME` (`config.toml`, `state/`, and the empty homes created by
  `remuda setup`), and never into any account home;
- never moves, renames or deletes a home directory;
- makes no network requests of its own.

Any behavior that breaks one of these guarantees is a security issue and should be reported
privately. Examples include remuda reading or exposing credential material, writing outside
`$REMUDA_HOME`, launching an agent with a different account's home than the one requested, or
passing untrusted data (such as a session ID from a file) to an agent in a way that is interpreted
as an option or command.

Vulnerabilities in Claude Code or Codex themselves are out of scope; report them to their
respective maintainers.
