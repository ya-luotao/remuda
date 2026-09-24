# Contributing to remuda

Thank you for your interest in improving Remuda. This document describes how changes are proposed,
checked and released. By participating, you agree to follow the
[Code of Conduct](CODE_OF_CONDUCT.md).

## Before you start

- **Bugs and small fixes:** open a pull request directly, or open an issue first if you are unsure
  whether the behavior is a bug.
- **New features and behavior changes:** open an issue describing the problem and the proposed
  behavior before writing code. Changes to the specification are discussed there.
- **Security issues:** do not open a public issue; follow [SECURITY.md](SECURITY.md).

## Spec-first workflow

[SPEC.md](SPEC.md) states the behavior that Remuda promises. Each entry has an anchor (`R1`, `R2`,
...), and the tests in `tests/` and in the unit test modules reference those anchors in their
names or doc comments.

- Behavior described in SPEC.md is a commitment. Changing it is a breaking change, and the change
  to SPEC.md and the corresponding change to `tests/` must land **in the same commit**.
- New behavior is specified first: add or amend the SPEC.md entry, then the tests that cite it,
  then the implementation.
- Entries marked **[unverified]** must be confirmed experimentally against the real agent before
  they are implemented. Record the agent version and what was observed in the entry.
- Behavior that is not in SPEC.md, such as the exact layout of TUI screens or the wording of
  messages, may change without a specification update.

## Development

Remuda is a single Rust crate (library and binary) targeting macOS and Linux. The minimum
supported Rust version is declared as `rust-version` in [Cargo.toml](Cargo.toml).

```sh
cargo build
cargo run -- --help
```

### Hermetic tests

Every test runs in a sealed sandbox (SPEC R15): a fresh `HOME` and `REMUDA_HOME` in a temporary
directory, a cleared environment, and a fake `claude` (and fake `codex`) first on `PATH` that
records its arguments and environment and returns fixtures. Tests never touch your real logins,
Keychain, homes or sessions, so the suite is safe to run on a machine where you use the agents
every day.

When adding tests:

- Use the helpers in `tests/common/` to build the sandbox; never read the real `HOME` or call the
  real agent.
- Fixtures for transcripts, rollouts, `sessions/*.json`, `history.jsonl` and `.claude.json` follow
  the real formats and must be scrubbed of personal data.
- Reference the SPEC anchor the test covers.
- Run the suite as a regular user, not as root: some tests rely on file permission bits that root
  bypasses.

### Required checks

Every pull request must pass, and CI runs, the following:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

CI also runs the tests on both macOS and Linux and checks that the crate builds with the minimum
supported Rust version.

## Commit messages

- Write the subject in the imperative mood, concise (about 72 characters at most), without a
  trailing period: `Refuse to resume a session running under another account`.
- Leave a blank line, then use the body to explain what changed and why, typically as a short
  bullet list. Cite the SPEC anchors involved, for example `SPEC R6: ...`.
- Keep each commit focused on one logical change, and keep the build and tests passing at every
  commit.

## Pull requests

- Keep pull requests focused; unrelated changes belong in separate pull requests.
- Describe the problem, the change, and how it was verified.
- If the change affects behavior in SPEC.md, update SPEC.md and `tests/` in the same commit.
- Add an entry under `## [Unreleased]` in [CHANGELOG.md](CHANGELOG.md) for every user-visible
  change, in the matching Keep a Changelog section (`Added`, `Changed`, `Deprecated`, `Removed`,
  `Fixed`, `Security`). Internal refactors and test-only changes do not need an entry.
- Do not change dependencies unless the change requires it; explain any new dependency in the
  description.
- Pull requests are merged once CI passes and a maintainer has approved them.

## Releases

Remuda follows [Semantic Versioning](https://semver.org/) and releases on a regular cadence when
there are user-visible changes.

- Before 1.0, a minor release (`0.x.0`) may contain breaking changes, and a patch release
  (`0.x.y`) contains only fixes and compatible additions. Breaking changes are called out in the
  changelog.
- To cut a release:
  1. Move the entries under `## [Unreleased]` in CHANGELOG.md into a new `## [X.Y.Z] - YYYY-MM-DD`
     section, and update the link references at the bottom of the file.
  2. Set `version` in Cargo.toml to `X.Y.Z` and refresh `Cargo.lock` (`cargo build`).
  3. Commit as `Release vX.Y.Z` and make sure CI passes on `main`.
  4. Tag the commit `vX.Y.Z` and push the tag, then publish a GitHub release whose notes are the
     changelog section.

## License

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed under the
[MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE) licenses, without any additional terms or
conditions.
