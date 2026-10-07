# Lessons

A rolling list of rules that review rounds keep teaching. One entry per rule: the rule, why it
holds, the evidence (which review found what, with the date), and on how many distinct days it
has come up. It is reread after every merged batch of work; an entry seen on three or more days
is promoted into [SPEC.md](SPEC.md) or [CONTRIBUTING.md](CONTRIBUTING.md) and marked so here.
Review records live under `docs/` in the maintainer's checkout (`docs/review-2026-10-02.md`,
`docs/arch-2026-10-06/`, `docs/study-2026-10-07/`), git-excluded; the dates below point at them.

## Data and time

**An unknown is not a zero.** A window past its reset, a reset whose wording could not be read, a
usage answer that tells no usage: each is *unknown*, and must stay unknown through every layer
instead of collapsing into a known value (0% used, 100% headroom, a future reset time, "not
recognized").
*Why:* every collapse so far ranked an exhausted account first or hid why nothing was shown.
*Evidence:* 2026-10-02 review item 8 (stale usage past its reset counted as 100% headroom);
2026-10-06 lane 7 (`Reset::Unknown` lost its reason, the TUI swapped a past reset for another
future one); 2026-10-07 lane 3 (`LiveUsage::Untold` separated from `Unrecognized`).
*Seen:* 3 days → promoted: SPEC R10 "Usage is read at an instant", R23 "Windows".

**Read time-dependent data at the time of the data, not at the time the command started.** A live
query may take as long as its timeout; a reset that falls in between has happened.
*Evidence:* 2026-10-06 lane 7, two majors (feasibility and `resets_at` computed from the start
time; wrong across a reset); 2026-10-08 lane C (`remuda usage` evaluated cached readings at the
command's start, then `usage --wait` took the clock before reading the cache: a reset in between
was recorded as current). *Seen:* 2 days (promoted with the entry above: R23 "The instant"; R10
now says the time is taken after the read).

**Merge duplicates whole, never field by field.** Two records of one request are two views of one
allocation; taking each column's maximum builds a usage nobody ever had.
*Evidence:* 2026-10-07 lane 2 (codex `input` / `cache_write` split merged by per-field maxima:
110 tokens became 170; a 201K request crossed the 272K long-context price step, $0.68 → $2.94).
*Seen:* 1 day.

**A newly accepted input shape gets its range guards in the same change.** Accepting a wording is
not the same as having validated its parts.
*Evidence:* 2026-10-07 lane 3 (new reset formats panicked on minute `60`: exit 101 where `main`
merely failed to parse). *Seen:* 1 day.

**Verify a premise against its primary source before writing it into a spec or a lane brief.**
*Evidence:* 2026-10-07 lane 2 (the brief said OpenAI does not charge cache writes; the pricing
page says 1.25× input from GPT-5.6 on); the same lane's `PRICES_AS_OF` had to match the day the
prices were actually checked, not the day the table was first written. *Seen:* 1 day.

## Identity and freshness

**A name is not an identity; key on the whole thing.** An account is its home path string (R2), a
session is its id *and* the account that ran it, a request is its full id.
*Evidence:* 2026-10-06 lane 3 (Logs slot deduplicated by short id swallowed another account's
request); lane 4 (a same-named account registered with another home was launched silently).
*Seen:* 1 day.

**Re-read before acting; "fresh as of the moment before launch" means every launch.** A registry,
an account list, a running-sessions answer that was true when the view opened may not be when
the key is pressed.
*Evidence:* 2026-10-06 lane 4 (reread tied to an ordinary worker dispatch did not cover every
refresh); SPEC R16 resume check. *Seen:* 1 day.

**Text from elsewhere stays marked as such through every layer.** Agent output, paths, settings
errors: never flatten them to plain text at the source, or private mode and the error messages
downstream lose what they need.
*Evidence:* 2026-10-02 review item 2 (path tails shown in private mode); 2026-10-06 lane 5
(settings validation errors flattened where they arose). *Seen:* 2 days.

**Output from elsewhere is untrusted bytes.** One oversized CSI parameter in `claude logs` output
aborted remuda and left the terminal in raw mode.
*Evidence:* 2026-10-02 review item 7. *Seen:* 1 day.

## Files, locks, processes

**A lock guards the resource, not the path it was reached by; resolve the path once and never go
through the name again.** Two valid entries to one registry (a symlinked `config.toml`) must take
the same lock; after the lock is taken, every read and write goes through the resolved target (or
the locked directory's descriptor), or a link retargeted meanwhile redirects the write.
*Evidence:* 2026-10-06 lane 1 (locking the entry directory lost an account under two entries);
2026-10-08 lane C round 1 (two `$REMUDA_HOME`s whose history files linked to one file took two
locks and lost a point) and round 2 (the link retargeted between validation and the write
redirected the compaction). *Seen:* 2 days.

**Check the permission the operation needs, not a neighbouring one.** Readable is not writable;
readable is not searchable.
*Evidence:* 2026-10-06 lane 1 (directory readability made a precondition for a write into a
private `state/`); lane 6 (a directory readable but not searchable silently emptied the cache);
2026-10-08 lane C (compaction wrote through a 0644 target the append path had refused; a target
deleted mid-write was recreated with the registry's default mode). *Seen:* 2 days.

**A child started between a signal and its registration belongs to nobody.** Register the start
before the fork, or make the last start under way finish the signal's work.
*Evidence:* 2026-10-06 lane 8 (`probe.rs` / `interrupt.rs`: Ctrl-C during parallel starts left an
agent running). *Seen:* 1 day.

**A fix narrows a lookup: keep the selection rule inside the narrowed set, and re-run the
finding's neighbours.** Two fixes in a row each introduced the next round's finding.
*Evidence:* 2026-10-08 lane A (restricting transcripts to the account's store replaced "the latest
copy" with "the first path"); lane C (refusing a retargeted link introduced recreating a deleted
target with the wrong mode). *Seen:* 1 day.

**Validate before every early return, and fix the sibling path too.** An argument check placed
after a "nothing feasible" return is skipped exactly when it matters; an ordering bug in one path
usually exists in its twin.
*Evidence:* 2026-10-08 lane A (GitHub review: `pick -- --resume` with every account exhausted said
"nothing to recommend"); lane C (the clock-before-read order fixed in `usage` was still in
`usage --wait`, found by the critic one round later). *Seen:* 1 day.

## The agents' commands

**Each flag changes what the command means; verify it on the exact command before adopting it.**
*Evidence:* 2026-10-07: `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` makes `claude -p /usage`
print no new reading; `--bare` makes it print only the `Total cost:` summary (the same text an
account that is not logged in gets); a plain `-p /usage` runs the user's `SessionStart` and
`SessionEnd` hooks and starts the plugins' MCP servers (claude 2.1.292). *Seen:* 1 day.

**Call the agent binary by path in experiments.** A wrapper or alias in the shell (here `claude`
→ `remuda run max`) routes the run to another account and writes the launch log.
*Evidence:* 2026-10-07 (a lane's `--version` probe and an orchestrator's `/usage` series, both
logged as `claude:max`; the probe measured the wrong account). *Seen:* 1 day.

## Process

**Gate every follow-up fix and every semantic merge with the critic before pushing.** The test
suite passing is not the gate.
*Evidence:* 2026-10-06 fleet (four regressions caught by the local critic after the whole suite
had passed); 2026-10-07 lane 2 (critic round 3 on the price-date follow-up); 2026-10-08 (the
critic's merge reviews passed, its follow-up reviews found the sibling-path bug above).
*Seen:* 3 days → promote: CONTRIBUTING "Pull requests" should name the review gate.

**Word a review request as a code review, not as an attack.** The codex critic produced nothing,
twice, for prompts that said "bypass the privacy check" and "retargeting"; the same request in
ordinary terms ("file-mode check", "concurrent-writer reproducer") went through.
*Evidence:* 2026-10-08 lane C rounds 2 and 3. *Seen:* 1 day.

**Archive lane records without their build output.** Critic evidence directories carry Cargo
target directories of several GB each; copy them with `target`, `build`, `mutation-build`,
`baseline-target` and `mutation-target` excluded.
*Evidence:* 2026-10-08 (an 8.1 GB copy of one lane's `.lane/`, 123 MB after pruning). *Seen:* 1 day.

**Every fix carries a test that is red with the fix reverted.** Compile failures do not count.
*Evidence:* the lane briefs of 2026-10-06 and 2026-10-07; the revert evidence in each lane's
report. *Seen:* 2 days → in CONTRIBUTING's spirit ("SPEC and tests in the same change"); promote
the revert rule explicitly when seen once more.

**Review loops need a convergence rule.** From round 3 on, re-check prior fixes and regressions
only; new findings block only within a named blocker set (R2 / R13 / R21 violations, data loss,
a SPEC contradiction, a red suite).
*Evidence:* 2026-10-06 (four rounds on two lanes before the rule); 2026-10-07 (three rounds,
converged); 2026-10-08 (lane C took eight rounds, each later round one finding in the blocker
set, so the rule held). *Seen:* 3 days → promote into CONTRIBUTING with the entry above.
