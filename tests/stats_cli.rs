//! R5, R20: `remuda stats [<account>] [--period today|7d|30d|all]`.

mod common;

use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use common::Sandbox;
use common::rollouts as cx;
use common::transcripts as cl;
use serde_json::{Value, json};

const S_A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const S_B: &str = "bbbbbbbb-0000-4000-8000-000000000002";
const S_M: &str = "cccccccc-0000-4000-8000-000000000003";
const S_T: &str = "dddddddd-0000-4000-8000-000000000004";
const S_U: &str = "eeeeeeee-0000-4000-8000-000000000005";
const R1: &str = "019c1e08-e4f6-7d70-a129-38ec744a3f31";
const R2: &str = "019c1e08-e4f6-7d70-a129-38ec744a3f32";
const R3: &str = "019c1e08-e4f6-7d70-a129-38ec744a3f33";
const R4: &str = "019c1e08-e4f6-7d70-a129-38ec744a3f34";
const R5: &str = "019c1e08-e4f6-7d70-a129-38ec744a3f35";

struct Setup {
    sb: Sandbox,
    max: PathBuf,
    team: PathBuf,
    native: PathBuf,
}

/// default + max share `$HOME/.claude/projects` (max via a symlink); team has its own store.
fn setup() -> Setup {
    let sb = Sandbox::new();
    let max = sb.make_claude_home("p/max");
    let team = sb.make_claude_home("p/team");
    sb.register(&[("max", &max), ("team", &team)]);
    let native = sb.home().join(".claude/projects");
    fs::create_dir_all(&native).unwrap();
    symlink(&native, max.join("projects")).unwrap();
    fs::create_dir_all(team.join("projects")).unwrap();
    Setup {
        sb,
        max,
        team,
        native,
    }
}

/// Writes `<projects>/-w-proj/<rel>`, creating directories; returns it.
fn write(projects: &Path, rel: &str, text: &str) -> PathBuf {
    let path = projects.join("-w-proj").join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, text).unwrap();
    path
}

fn append(path: &Path, text: &str) {
    let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

fn history(path: &Path, sids: &[&str]) {
    let text: String = sids
        .iter()
        .map(|s| {
            format!(
                "{{\"display\":\"p\",\"pastedContents\":{{}},\"timestamp\":1,\"project\":\"/w\",\"sessionId\":\"{s}\"}}\n"
            )
        })
        .collect();
    fs::write(path, text).unwrap();
}

fn message(session: &str, id: &str, usage: Value) -> String {
    cl::assistant_usage(session, id, "claude-test", usage, &cl::ts(1))
}

/// `remuda stats <args>`: stdout of a successful run.
fn stats(sb: &Sandbox, args: &[&str]) -> String {
    let out = sb
        .remuda()
        .arg("stats")
        .args(args)
        .assert()
        .success()
        .get_output()
        .clone();
    String::from_utf8(out.stdout).unwrap()
}

/// The lines of the section labeled `label` (up to the next blank line), each with its cells
/// separated by one space.
fn block(stdout: &str, label: &str) -> Vec<String> {
    let mut lines = stdout.lines().skip_while(|l| *l != label);
    assert_eq!(
        lines.next(),
        Some(label),
        "no section {label:?} in:\n{stdout}"
    );
    lines
        .take_while(|l| !l.is_empty())
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect()
}

/// The section labels: lines that are neither blank nor indented, after the title and header
/// and before the cost note.
fn labels(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .skip(3)
        .take_while(|l| !l.starts_with("Cost ≈"))
        .filter(|l| !l.is_empty() && !l.starts_with(' '))
        .collect()
}

/// The claude fixtures of `tests/stats.rs`: in S_A (default) a message in several records,
/// records that do not count, an advisor call and two subagent transcripts; in S_B (max) a
/// fork of S_A's first message plus its own; S_M claimed by both default and max; S_T in
/// team's own store; S_U attributed to nobody.
fn claude_fixtures(s: &Setup) {
    let a1 = |output: u64, minute: u32| {
        cl::assistant_usage(
            S_A,
            "msg_a1",
            "claude-test",
            cl::usage(3, output, 100, 300),
            &cl::ts(minute),
        )
    };
    let a1 = [a1(10, 1), a1(25, 2), a1(40, 3)];
    let mut advised = cl::usage(4, 45, 60, 100);
    advised["iterations"] = json!([
        {"type": "message", "input_tokens": 2, "output_tokens": 20,
         "cache_creation_input_tokens": 30, "cache_read_input_tokens": 50},
        {"type": "advisor_message", "model": "claude-advisor-test", "input_tokens": 700,
         "output_tokens": 70, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0},
        {"type": "message", "input_tokens": 2, "output_tokens": 25,
         "cache_creation_input_tokens": 30, "cache_read_input_tokens": 50},
    ]);
    let subagent = cl::sidechain(
        &cl::assistant_usage(
            S_A,
            "msg_s1",
            "claude-haiku-test",
            cl::usage(10, 2, 0, 0),
            &cl::ts(4),
        ),
        "s1",
    );
    let text = [
        a1.concat(),
        message(S_A, "msg_a2", cl::usage(1, 5, 0, 200)),
        cl::assistant_usage(
            S_A,
            "msg_syn",
            "<synthetic>",
            cl::usage(0, 0, 0, 0),
            &cl::ts(4),
        ),
        cl::task_result(cl::usage(900, 900, 900, 900), &cl::ts(4)),
        cl::agent_progress(&subagent, "s1", &cl::ts(4)),
        "{\"type\":\"assistant\",\"usage\" not json\n".to_string(),
        message(S_A, "msg_a3", advised),
        // Still being written: not counted.
        message(S_A, "msg_a4", cl::usage(1000, 0, 0, 0))
            .trim_end()
            .to_string(),
    ]
    .concat();
    write(&s.native, &format!("{S_A}.jsonl"), &text);
    write(
        &s.native,
        &format!("{S_A}/subagents/agent-s1.jsonl"),
        &subagent,
    );
    write(
        &s.native,
        &format!("{S_A}/subagents/workflows/wf_1/agent-s2.jsonl"),
        &cl::sidechain(
            &cl::assistant_usage(
                S_A,
                "msg_s2",
                "claude-haiku-test",
                cl::usage(20, 3, 0, 0),
                &cl::ts(5),
            ),
            "s2",
        ),
    );
    let fork: String = a1.iter().map(|r| cl::forked(r, S_A)).collect();
    write(
        &s.native,
        &format!("{S_B}.jsonl"),
        &[fork, message(S_B, "msg_b1", cl::usage(7, 7, 0, 0))].concat(),
    );
    write(
        &s.native,
        &format!("{S_M}.jsonl"),
        &message(S_M, "msg_m1", cl::usage(5, 5, 0, 0)),
    );
    write(
        &s.native,
        &format!("{S_U}.jsonl"),
        &message(S_U, "msg_u1", cl::usage(9, 9, 0, 0)),
    );
    write(
        &s.team.join("projects"),
        &format!("{S_T}.jsonl"),
        &message(S_T, "msg_t1", cl::usage(11, 11, 0, 0)),
    );
    history(&s.sb.home().join(".claude/history.jsonl"), &[S_A, S_M]);
    history(&s.max.join("history.jsonl"), &[S_B, S_M]);
    history(&s.team.join("history.jsonl"), &[S_T]);
}

/// R5, R20: every account, then groups, then unattributed, then overall; each message counted
/// once, advisor calls under their model, subagents for their session; cache write is claude's,
/// reasoning is codex's (`-` here).
#[test]
fn stats_by_account_and_model() {
    let s = setup();
    claude_fixtures(&s);
    let out = stats(&s.sb, &[]);
    let mut lines = out.lines();
    assert_eq!(lines.next(), Some("Tokens · all time"));
    assert_eq!(lines.next(), Some(""));
    assert_eq!(
        lines.next().unwrap().split_whitespace().collect::<Vec<_>>(),
        [
            "MODEL",
            "INPUT",
            "CACHE",
            "READ",
            "CACHE",
            "WRITE",
            "OUTPUT",
            "REASONING",
            "TOTAL",
            "COST"
        ]
    );
    assert_eq!(
        labels(&out),
        [
            "claude:default",
            "claude:max",
            "claude:team",
            "claude:default + claude:max",
            "unattributed",
            "overall",
        ]
    );
    assert_eq!(
        block(&out, "claude:default"),
        [
            "claude-test 8 600 160 90 - 858 -",
            "claude-advisor-test 700 0 0 70 - 770 -",
            "claude-haiku-test 30 0 0 5 - 35 -",
            "total 738 600 160 165 - 1.7K -",
        ]
    );
    assert_eq!(
        block(&out, "claude:max"),
        ["claude-test 7 0 0 7 - 14 -", "total 7 0 0 7 - 14 -"]
    );
    assert_eq!(
        block(&out, "claude:team"),
        ["claude-test 11 0 0 11 - 22 -", "total 11 0 0 11 - 22 -"]
    );
    assert_eq!(
        block(&out, "claude:default + claude:max"),
        ["claude-test 5 0 0 5 - 10 -", "total 5 0 0 5 - 10 -"]
    );
    assert_eq!(
        block(&out, "unattributed"),
        ["claude-test 9 0 0 9 - 18 -", "total 9 0 0 9 - 18 -"]
    );
    assert_eq!(
        block(&out, "overall"),
        [
            "claude-test 40 600 160 122 - 922 -",
            "claude-advisor-test 700 0 0 70 - 770 -",
            "claude-haiku-test 30 0 0 5 - 35 -",
            "total 770 600 160 197 - 1.7K -",
        ]
    );
    // Columns align over the whole output.
    let ends: Vec<usize> = out
        .lines()
        .filter(|l| l.starts_with("  ") || l.starts_with("MODEL"))
        .map(str::len)
        .collect();
    assert!(ends.windows(2).all(|w| w[0] == w[1]), "{out}");
    // Then what the cost is, and the models without a price.
    let last: Vec<&str> = out.lines().rev().take(3).collect();
    assert_eq!(
        last,
        [
            "Not priced: claude-advisor-test, claude-haiku-test, claude-test \
             (add [prices.\"<model>\"] to config.toml)",
            "Cost ≈ API list price (prices as of 2026-10-07): an estimate, not a bill.",
            "",
        ]
    );
}

/// R3, R20: `[prices."<model>"]` in config.toml prices a model remuda has no price for; a
/// cost below half a cent shows as `<$0.01`.
#[test]
fn stats_prices_from_config() {
    let s = setup();
    claude_fixtures(&s);
    let config = s.sb.read_config();
    s.sb.write_config(&format!(
        "{config}[prices.\"claude-test\"]\ninput = 1\noutput = 1\ncache_read = 1\n\
         cache_write_5m = 1\ncache_write_1h = 1\n"
    ));
    let out = stats(&s.sb, &[]);
    // 14 tokens at $1 per million: $0.000014.
    assert_eq!(
        block(&out, "claude:max"),
        [
            "claude-test 7 0 0 7 - 14 <$0.01",
            "total 7 0 0 7 - 14 <$0.01"
        ]
    );
    let overall = block(&out, "overall");
    assert_eq!(overall[0], "claude-test 40 600 160 122 - 922 <$0.01");
    assert_eq!(overall[3], "total 770 600 160 197 - 1.7K <$0.01+");
    assert_eq!(
        out.lines().last(),
        Some(
            "Not priced: claude-advisor-test, claude-haiku-test \
             (add [prices.\"<model>\"] to config.toml)"
        )
    );
}

/// R3: an invalid `[prices]` table fails every command, naming the file.
#[test]
fn stats_invalid_prices_fail_naming_the_config() {
    let s = setup();
    let config = s.sb.read_config();
    s.sb.write_config(&format!("{config}[prices.\"x\"]\ninput = -1\noutput = 1\n"));
    s.sb.remuda()
        .arg("stats")
        .assert()
        .code(1)
        .stderr(predicates::str::contains(
            s.sb.config_path().display().to_string(),
        ))
        .stderr(predicates::str::contains(
            "[prices.\"x\"]: `input` must be from 0 to 1000000",
        ));
}

/// R20: codex rollouts (fixtures of `tests/stats.rs`) in `$HOME/.codex`: repeats, compaction
/// estimates, fork replays and inherited totals skipped, archived rollouts counted, events
/// before the first turn under its model; codex records cache write, here 0.
#[test]
fn stats_codex() {
    let sb = Sandbox::new();
    let codex = sb.home().join(".codex");
    let r1 = |ts: &dyn Fn(u32) -> String| {
        [
            cx::meta(R1, "/w/proj", json!("cli"), 0, &ts(0)),
            cx::model_turn("gpt-test-a", &ts(1)),
            cx::tokens([100, 50, 10, 4], [100, 50, 10, 4], &ts(2)),
            cx::tokens([100, 50, 10, 4], [100, 50, 10, 4], &ts(2)),
            cx::tokens([250, 150, 30, 10], [150, 100, 20, 6], &ts(3)),
            cx::compacted(&ts(4)),
            cx::tokens([250, 150, 30, 10], [60, 0, 0, 0], &ts(4)),
            cx::model_turn("gpt-test-b", &ts(5)),
            cx::tokens([400, 250, 45, 12], [150, 100, 15, 2], &ts(6)),
        ]
        .concat()
    };
    cx::write_rollout(&codex, R1, &r1(&cx::ts));
    let replay: String = r1(&|m| cx::ts(m + 10))
        .lines()
        .skip(1)
        .map(|l| format!("{l}\n"))
        .collect();
    cx::write_rollout(
        &codex,
        R2,
        &[
            cx::fork_meta(R2, R1, "/w/proj", &cx::ts(20)),
            replay,
            cx::model_turn("gpt-test-b", &cx::ts(21)),
            cx::tokens([520, 330, 55, 13], [120, 80, 10, 1], &cx::ts(22)),
        ]
        .concat(),
    );
    cx::write_rollout(
        &codex,
        R3,
        &[
            cx::meta(R3, "/w/proj", json!({"subagent": "review"}), 0, &cx::ts(0)),
            cx::model_turn("gpt-test-a", &cx::ts(1)),
            cx::tokens([1000, 800, 50, 10], [100, 80, 5, 1], &cx::ts(2)),
        ]
        .concat(),
    );
    cx::write_archived_rollout(
        &codex,
        R4,
        &[
            cx::meta(R4, "/w/proj", json!("cli"), 0, &cx::ts(0)),
            cx::model_turn("gpt-test-d", &cx::ts(1)),
            cx::tokens([200, 100, 20, 8], [200, 100, 20, 8], &cx::ts(2)),
        ]
        .concat(),
    );
    cx::write_rollout(
        &codex,
        R5,
        &[
            cx::meta(R5, "/w/proj", json!("cli"), 0, &cx::ts(0)),
            cx::compacted(&cx::ts(1)),
            cx::tokens([60, 20, 6, 0], [60, 20, 6, 0], &cx::ts(2)),
            cx::model_turn("gpt-test-c", &cx::ts(3)),
            cx::tokens([90, 40, 9, 0], [30, 20, 3, 0], &cx::ts(4)),
        ]
        .concat(),
    );
    let out = stats(&sb, &[]);
    assert_eq!(labels(&out), ["claude:default", "codex:default", "overall"]);
    assert_eq!(block(&out, "claude:default"), ["no tokens"]);
    let codex_rows = [
        "gpt-test-a 120 230 0 35 11 385 -",
        "gpt-test-b 90 180 0 25 3 295 -",
        "gpt-test-d 100 100 0 20 8 220 -",
        "gpt-test-c 50 40 0 9 0 99 -",
        "total 360 550 0 89 22 999 -",
    ];
    assert_eq!(block(&out, "codex:default"), codex_rows);
    assert_eq!(block(&out, "overall"), codex_rows);
}

/// R20: codex's `cache_write_input_tokens`, part of `input_tokens`, is shown as cache write and
/// priced at the cache-write price; the total is unchanged. The request of a real codex 0.160.0
/// event, with 2,000 tokens written to the cache (and so 2,000 more input tokens).
#[test]
fn stats_codex_cache_write() {
    let sb = Sandbox::new();
    let codex = sb.home().join(".codex");
    cx::write_rollout(
        &codex,
        R1,
        &[
            cx::meta(R1, "/w/proj", json!("cli"), 0, &cx::ts(0)),
            cx::model_turn("gpt-5.6-sol", &cx::ts(1)),
            cx::usage_event(
                cx::usage_with_write([5_730_399, 5_586_560, 2_000, 19_533, 4_590]),
                cx::usage_with_write([139_139, 136_704, 2_000, 529, 90]),
                &cx::ts(2),
            ),
        ]
        .concat(),
    );
    let out = stats(&sb, &[]);
    // $0.0770016: 435 × 4 + 136,704 × 0.40 + 2,000 × 5 + 529 × 20, per million.
    assert_eq!(
        block(&out, "codex:default"),
        [
            "gpt-5.6-sol 435 137K 2K 529 90 140K $0.08",
            "total 435 137K 2K 529 90 140K $0.08",
        ]
    );
}

/// R20: with an account, only the sections that include it, and no overall section.
#[test]
fn stats_filter_by_account() {
    let s = setup();
    claude_fixtures(&s);
    for account in ["max", "claude:max"] {
        let out = stats(&s.sb, &[account]);
        assert_eq!(
            labels(&out),
            ["claude:max", "claude:default + claude:max"],
            "{account}"
        );
        assert_eq!(
            block(&out, "claude:max"),
            ["claude-test 7 0 0 7 - 14 -", "total 7 0 0 7 - 14 -"]
        );
    }
}

/// R5, R20: `--period` picks one period, starting at local midnight; anything else is a usage
/// error.
#[test]
fn stats_period_today() {
    let sb = Sandbox::new();
    let native = sb.home().join(".claude/projects");
    let now = jiff::Timestamp::now().to_string();
    let today = jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .strftime("%Y-%m-%d 00:00")
        .to_string();
    write(
        &native,
        &format!("{S_U}.jsonl"),
        &[
            cl::assistant_usage(S_U, "msg_now", "claude-test", cl::usage(1, 1, 0, 0), &now),
            cl::assistant_usage(
                S_U,
                "msg_old",
                "claude-test",
                cl::usage(100, 100, 0, 0),
                "2025-01-01T10:00:00.000Z",
            ),
        ]
        .concat(),
    );
    let out = stats(&sb, &["--period", "today"]);
    assert_eq!(
        out.lines().next().unwrap(),
        format!("Tokens · today (since {today})")
    );
    assert_eq!(
        block(&out, "unattributed"),
        ["claude-test 1 0 0 1 - 2 -", "total 1 0 0 1 - 2 -"]
    );
    assert_eq!(block(&out, "claude:default"), ["no tokens"]);
    let out = stats(&sb, &["--period", "all"]);
    assert_eq!(
        block(&out, "unattributed"),
        [
            "claude-test 101 0 0 101 - 202 -",
            "total 101 0 0 101 - 202 -"
        ]
    );
    let out = stats(&sb, &["--period", "7d"]);
    assert!(out.starts_with("Tokens · last 7 days (since "), "{out}");
    sb.remuda()
        .args(["stats", "--period", "2w"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("expected today, 7d, 30d or all"));
}

/// R20: no agent is run; the counts are cached in `state/stats.json` (not rewritten when
/// nothing changed), and a grown transcript raises them.
#[test]
fn stats_runs_no_agent_and_caches() {
    let s = setup();
    claude_fixtures(&s);
    let first = stats(&s.sb, &[]);
    assert!(s.sb.invocations().is_empty());
    let cache = s.sb.remuda_home().join("state/stats.json");
    let v: Value = serde_json::from_slice(&fs::read(&cache).unwrap()).unwrap();
    assert_eq!(v["schema_version"], remuda::stats::SCHEMA_VERSION);
    let written = fs::metadata(&cache).unwrap().modified().unwrap();
    assert_eq!(stats(&s.sb, &[]), first);
    assert_eq!(
        fs::metadata(&cache).unwrap().modified().unwrap(),
        written,
        "an unchanged cache is not rewritten"
    );

    append(
        &s.native.join(format!("-w-proj/{S_U}.jsonl")),
        &message(S_U, "msg_u2", cl::usage(1, 1, 0, 0)),
    );
    let out = stats(&s.sb, &[]);
    assert_eq!(
        block(&out, "unattributed"),
        ["claude-test 10 0 0 10 - 20 -", "total 10 0 0 10 - 20 -"]
    );
    assert!(s.sb.invocations().is_empty());
}

/// R3: the statistics cache is written readable by the user alone, in a `state/` that is, also
/// over a cache and a directory from before that others could read.
#[test]
fn the_statistics_cache_is_private() {
    let mode = |path: &Path| fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777;
    let s = setup();
    claude_fixtures(&s);
    let state = s.sb.remuda_home().join("state");
    let cache = state.join("stats.json");
    let first = stats(&s.sb, &[]);
    assert_eq!(mode(&state), 0o700);
    assert_eq!(mode(&cache), 0o600);

    // Written again once a transcript grew.
    fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o644)).unwrap();
    append(
        &s.native.join(format!("-w-proj/{S_U}.jsonl")),
        &message(S_U, "msg_u2", cl::usage(1, 1, 0, 0)),
    );
    assert_ne!(stats(&s.sb, &[]), first);
    assert_eq!(mode(&state), 0o700);
    assert_eq!(mode(&cache), 0o600);
}

/// R3, R20: a cache that cannot be written is a warning; the statistics are still printed.
#[test]
fn stats_warns_when_the_cache_cannot_be_written() {
    let s = setup();
    claude_fixtures(&s);
    fs::write(s.sb.remuda_home().join("state"), "not a directory").unwrap();
    let out =
        s.sb.remuda()
            .arg("stats")
            .assert()
            .success()
            .stderr(predicates::str::contains(
                "remuda: warning: cannot write statistics cache",
            ))
            .get_output()
            .clone();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        block(&stdout, "claude:max")[0],
        "claude-test 7 0 0 7 - 14 -"
    );
}

/// R20, R8 (review #10): a store that exists but cannot be read is not one whose transcripts
/// were deleted. The report keeps their counts as last read and says on stdout that it is
/// incomplete, naming the store; the cache is left as it was.
#[test]
fn stats_says_when_a_store_cannot_be_read_and_keeps_its_counts() {
    let s = setup();
    claude_fixtures(&s);
    let complete = stats(&s.sb, &[]);
    assert!(!complete.contains("Incomplete"), "{complete}");
    let cache = s.sb.remuda_home().join("state/stats.json");
    let saved = fs::read(&cache).unwrap();
    let written = fs::metadata(&cache).unwrap().modified().unwrap();
    let files = |path: &Path| -> usize {
        let v: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        let below = |p: &&String| p.starts_with(s.native.canonicalize().unwrap().to_str().unwrap());
        v["files"].as_object().unwrap().keys().filter(below).count()
    };
    let cached = files(&cache);
    assert!(cached > 1);

    let store = s.native.canonicalize().unwrap();
    fs::set_permissions(&store, fs::Permissions::from_mode(0o000)).unwrap();
    let out =
        s.sb.remuda()
            .arg("stats")
            .assert()
            .success()
            .stderr("")
            .get_output()
            .clone();
    let filtered = stats(&s.sb, &["max"]);
    fs::set_permissions(&store, fs::Permissions::from_mode(0o755)).unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let line = stdout
        .strip_prefix(complete.as_str())
        .unwrap_or_else(|| panic!("the same report, then the line:\n{stdout}"));
    let said = format!("Incomplete: cannot read {}: ", store.display());
    assert!(line.starts_with(&said), "{line}");
    assert!(
        line.ends_with(&format!(
            "; {cached} transcripts below it are counted as last read\n"
        )),
        "{line}"
    );
    assert_eq!(line.lines().count(), 1, "{line}");
    assert!(
        filtered.ends_with(line),
        "also for one account:\n{filtered}"
    );
    assert_eq!(fs::read(&cache).unwrap(), saved);
    assert_eq!(
        fs::metadata(&cache).unwrap().modified().unwrap(),
        written,
        "the cache is not rewritten"
    );

    // Readable again: the report is complete, and nothing was read again.
    assert_eq!(stats(&s.sb, &[]), complete);
    assert_eq!(fs::metadata(&cache).unwrap().modified().unwrap(), written);
}

/// R20, R8 (review #10): a project directory that can be listed but not searched is not one
/// whose transcripts were deleted either: the same report, one `Incomplete:` line naming it,
/// and the cache left as it was, so that nothing is read again afterwards.
#[test]
fn stats_says_when_a_project_cannot_be_searched_and_keeps_its_counts() {
    let s = setup();
    claude_fixtures(&s);
    let complete = stats(&s.sb, &[]);
    let cache = s.sb.remuda_home().join("state/stats.json");
    let saved = fs::read(&cache).unwrap();
    let written = fs::metadata(&cache).unwrap().modified().unwrap();
    let project = s.native.canonicalize().unwrap().join("-w-proj");
    let v: Value = serde_json::from_slice(&saved).unwrap();
    let below = |p: &&String| Path::new(p.as_str()).starts_with(&project);
    let cached = v["files"].as_object().unwrap().keys().filter(below).count();
    assert!(cached > 2, "top-level and subagent transcripts: {cached}");

    fs::set_permissions(&project, fs::Permissions::from_mode(0o444)).unwrap();
    let out = stats(&s.sb, &[]);
    fs::set_permissions(&project, fs::Permissions::from_mode(0o755)).unwrap();
    let line = out
        .strip_prefix(complete.as_str())
        .unwrap_or_else(|| panic!("the same report, then the line:\n{out}"));
    assert_eq!(line.lines().count(), 1, "{line}");
    assert!(
        line.starts_with(&format!("Incomplete: cannot read {}: ", project.display())),
        "{line}"
    );
    assert!(
        line.ends_with(&format!(
            "; {cached} transcripts below it are counted as last read\n"
        )),
        "{line}"
    );
    assert_eq!(fs::read(&cache).unwrap(), saved);

    assert_eq!(stats(&s.sb, &[]), complete);
    assert_eq!(
        fs::metadata(&cache).unwrap().modified().unwrap(),
        written,
        "nothing was read again: the cache is not rewritten"
    );
}

/// R20, R8 (review #10): a home that cannot be searched does not make its store an absent
/// one. The store cannot be resolved: its transcripts stay counted as last read, the report
/// says so naming the store as the home gives it, and the cache is left as it was.
#[test]
fn stats_says_when_a_store_cannot_be_resolved_and_keeps_its_counts() {
    let s = setup();
    claude_fixtures(&s);
    let complete = stats(&s.sb, &[]);
    let cache = s.sb.remuda_home().join("state/stats.json");
    let saved = fs::read(&cache).unwrap();
    let written = fs::metadata(&cache).unwrap().modified().unwrap();

    fs::set_permissions(&s.team, fs::Permissions::from_mode(0o000)).unwrap();
    let out = stats(&s.sb, &[]);
    fs::set_permissions(&s.team, fs::Permissions::from_mode(0o755)).unwrap();
    // The tokens are all there; whose they are is another matter (the home's `history.jsonl`
    // cannot be read either).
    assert_eq!(block(&out, "overall"), block(&complete, "overall"));
    let line = out.lines().last().unwrap();
    assert!(
        line.starts_with(&format!(
            "Incomplete: cannot read {}: ",
            s.team.join("projects").display()
        )),
        "{out}"
    );
    assert!(
        line.ends_with("; 1 transcript below it is counted as last read"),
        "{line}"
    );
    assert_eq!(out.matches("Incomplete:").count(), 1, "{out}");
    assert_eq!(fs::read(&cache).unwrap(), saved);

    assert_eq!(stats(&s.sb, &[]), complete);
    assert_eq!(
        fs::metadata(&cache).unwrap().modified().unwrap(),
        written,
        "nothing was read again: the cache is not rewritten"
    );
}

/// R20, R8 (review #10): while one store cannot be resolved, a store that is gone is still
/// gone. Its tokens are no longer counted and leave the cache; only those of the store that
/// cannot be resolved stay, with the `Incomplete:` line.
#[test]
fn a_store_that_is_gone_stops_counting_while_another_cannot_be_resolved() {
    let s = setup();
    claude_fixtures(&s);
    stats(&s.sb, &[]);
    let cache = s.sb.remuda_home().join("state/stats.json");
    let saved = |cache: &Path| -> Vec<String> {
        let v: Value = serde_json::from_slice(&fs::read(cache).unwrap()).unwrap();
        v["files"].as_object().unwrap().keys().cloned().collect()
    };
    assert!(saved(&cache).len() > 2);

    fs::remove_dir_all(&s.native).unwrap();
    fs::set_permissions(&s.team, fs::Permissions::from_mode(0o000)).unwrap();
    let out = stats(&s.sb, &[]);
    fs::set_permissions(&s.team, fs::Permissions::from_mode(0o755)).unwrap();
    // What is left is the one message of team's store.
    assert_eq!(
        block(&out, "overall"),
        ["claude-test 11 0 0 11 - 22 -", "total 11 0 0 11 - 22 -"],
        "{out}"
    );
    assert!(
        out.lines()
            .last()
            .is_some_and(|l| l.starts_with("Incomplete: cannot read ")
                && l.ends_with("; 1 transcript below it is counted as last read")),
        "{out}"
    );
    let left = saved(&cache);
    assert_eq!(left.len(), 1, "{left:?}");
    assert!(left[0].contains(S_T), "{left:?}");
}

/// R20, R8 (review #10): an account removed and registered again under the same name with
/// another home, whose store cannot be resolved before it was read, does not keep what was
/// counted from the old home: the tokens leave the report and the saved cache, on this run
/// and the next, and the new store is named with nothing kept below it.
#[test]
fn an_account_registered_again_with_another_home_does_not_keep_the_old_counts() {
    let sb = Sandbox::new();
    let (old, new) = (sb.make_claude_home("p/old"), sb.make_claude_home("p/new"));
    write(
        &old.join("projects"),
        &format!("{S_A}.jsonl"),
        &message(S_A, "msg_old", cl::usage(10, 10, 0, 0)),
    );
    write(
        &new.join("projects"),
        &format!("{S_B}.jsonl"),
        &message(S_B, "msg_new", cl::usage(20, 20, 0, 0)),
    );
    sb.register(&[("work", &old)]);
    let first = stats(&sb, &[]);
    assert_eq!(block(&first, "overall")[0], "claude-test 10 0 0 10 - 20 -");
    let cache = sb.remuda_home().join("state/stats.json");
    let saved = |cache: &Path| -> usize {
        let v: Value = serde_json::from_slice(&fs::read(cache).unwrap()).unwrap();
        v["files"].as_object().unwrap().len()
    };
    assert_eq!(saved(&cache), 1);

    sb.register(&[("work", &new)]);
    fs::set_permissions(&new, fs::Permissions::from_mode(0o000)).unwrap();
    let runs = [stats(&sb, &[]), stats(&sb, &[])];
    let left = saved(&cache);
    fs::set_permissions(&new, fs::Permissions::from_mode(0o755)).unwrap();
    for out in &runs {
        assert_eq!(block(out, "overall"), ["no tokens"], "{out}");
        let line = out.lines().last().unwrap();
        assert!(
            line.starts_with(&format!(
                "Incomplete: cannot read {}: ",
                new.join("projects").display()
            )),
            "{out}"
        );
        assert!(!line.contains("below it"), "nothing is kept: {line}");
    }
    assert_eq!(left, 0, "the saved cache does not keep the old home's");

    let out = stats(&sb, &[]);
    assert_eq!(block(&out, "overall")[0], "claude-test 20 0 0 20 - 40 -");
    assert!(!out.contains("Incomplete"), "{out}");
}

/// R5: an account that does not resolve is an error.
#[test]
fn stats_unknown_account_fails() {
    let s = setup();
    s.sb.remuda()
        .args(["stats", "nobody"])
        .assert()
        .code(1)
        .stderr(predicates::str::starts_with("remuda: "));
}
