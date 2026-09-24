//! R5, R20: `remuda stats [<account>] [--period today|7d|30d|all]`.

mod common;

use std::fs;
use std::io::Write;
use std::os::unix::fs::symlink;
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

/// The section labels: lines that are neither blank nor indented, after the title and header.
fn labels(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .skip(3)
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
            "TOTAL"
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
            "claude-test 8 600 160 90 - 858",
            "claude-advisor-test 700 0 0 70 - 770",
            "claude-haiku-test 30 0 0 5 - 35",
            "total 738 600 160 165 - 1.7K",
        ]
    );
    assert_eq!(
        block(&out, "claude:max"),
        ["claude-test 7 0 0 7 - 14", "total 7 0 0 7 - 14"]
    );
    assert_eq!(
        block(&out, "claude:team"),
        ["claude-test 11 0 0 11 - 22", "total 11 0 0 11 - 22"]
    );
    assert_eq!(
        block(&out, "claude:default + claude:max"),
        ["claude-test 5 0 0 5 - 10", "total 5 0 0 5 - 10"]
    );
    assert_eq!(
        block(&out, "unattributed"),
        ["claude-test 9 0 0 9 - 18", "total 9 0 0 9 - 18"]
    );
    assert_eq!(
        block(&out, "overall"),
        [
            "claude-test 40 600 160 122 - 922",
            "claude-advisor-test 700 0 0 70 - 770",
            "claude-haiku-test 30 0 0 5 - 35",
            "total 770 600 160 197 - 1.7K",
        ]
    );
    // Columns align over the whole output.
    let ends: Vec<usize> = out
        .lines()
        .filter(|l| l.starts_with("  ") || l.starts_with("MODEL"))
        .map(str::len)
        .collect();
    assert!(ends.windows(2).all(|w| w[0] == w[1]), "{out}");
}

/// R20: codex rollouts (fixtures of `tests/stats.rs`) in `$HOME/.codex`: repeats, compaction
/// estimates, fork replays and inherited totals skipped, archived rollouts counted, events
/// before the first turn under its model; cache write is `-`.
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
        "gpt-test-a 120 230 - 35 11 385",
        "gpt-test-b 90 180 - 25 3 295",
        "gpt-test-d 100 100 - 20 8 220",
        "gpt-test-c 50 40 - 9 0 99",
        "total 360 550 - 89 22 999",
    ];
    assert_eq!(block(&out, "codex:default"), codex_rows);
    assert_eq!(block(&out, "overall"), codex_rows);
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
            ["claude-test 7 0 0 7 - 14", "total 7 0 0 7 - 14"]
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
        ["claude-test 1 0 0 1 - 2", "total 1 0 0 1 - 2"]
    );
    assert_eq!(block(&out, "claude:default"), ["no tokens"]);
    let out = stats(&sb, &["--period", "all"]);
    assert_eq!(
        block(&out, "unattributed"),
        ["claude-test 101 0 0 101 - 202", "total 101 0 0 101 - 202"]
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
        ["claude-test 10 0 0 10 - 20", "total 10 0 0 10 - 20"]
    );
    assert!(s.sb.invocations().is_empty());
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
    assert_eq!(block(&stdout, "claude:max")[0], "claude-test 7 0 0 7 - 14");
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
