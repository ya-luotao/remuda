//! R20: token statistics — counting requests from claude transcripts and codex rollouts once,
//! across records, refreshes, copies and forks; attribution to accounts; periods; the cache.

mod common;

use std::fs;
use std::io::Write;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use common::rollouts as cx;
use common::transcripts as cl;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use remuda::Env;
use remuda::attribution::Attribution;
use remuda::index::RefreshStats;
use remuda::pricing::Prices;
use remuda::provider::Provider;
use remuda::registry::{Account, Home};
use remuda::stats::{
    self, Cache, Cost, ModelRow, Period, Report, SCHEMA_VERSION, Section, Source, SourceKind,
    Table, Tokens,
};
use serde_json::{Value, json};
use tempfile::TempDir;

const NOW: &str = "2026-09-24T02:00:00Z";
const S_A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const S_B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const R1: &str = "019c1e08-e4f6-7d70-a129-38ec744a3f31";
const R2: &str = "019c1e08-e4f6-7d70-a129-38ec744a3f32";
const R3: &str = "019c1e08-e4f6-7d70-a129-38ec744a3f33";

/// A sandbox with `$HOME/.claude/projects/-w-proj` for `claude:default`, and the accounts,
/// attribution and cache the statistics are computed with.
struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    env: Env,
    accounts: Vec<Account>,
    attribution: Attribution,
    cache: Cache,
    prices: Prices,
    now: Timestamp,
    tz: TimeZone,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("home/.claude/projects/-w-proj")).unwrap();
        let env = [("HOME".to_string(), root.join("home").display().to_string())]
            .into_iter()
            .collect();
        Fixture {
            _tmp: tmp,
            root,
            env,
            accounts: vec![Account::default_for(Provider::Claude)],
            attribution: Attribution::default(),
            cache: Cache::default(),
            prices: Prices::default(),
            now: NOW.parse().unwrap(),
            tz: TimeZone::UTC,
        }
    }

    /// The native store (realpath).
    fn projects(&self) -> PathBuf {
        self.root.join("home/.claude/projects")
    }

    /// Writes `<native store>/-w-proj/<rel>`, creating directories.
    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        write_file(&self.projects().join("-w-proj").join(rel), contents)
    }

    /// Registers the claude account `name` with home `<root>/p/<name>` (created, no store).
    fn claude(&mut self, name: &str) -> PathBuf {
        let home = self.root.join("p").join(name);
        fs::create_dir_all(&home).unwrap();
        self.accounts.push(Account {
            provider: Provider::Claude,
            name: name.into(),
            home: Home::Path(home.display().to_string()),
        });
        home
    }

    /// Registers the codex account `name` with home `<root>/c/<name>` and its `sessions`.
    fn codex(&mut self, name: &str) -> PathBuf {
        let home = self.root.join("c").join(name);
        fs::create_dir_all(home.join("sessions")).unwrap();
        self.accounts.push(Account {
            provider: Provider::Codex,
            name: name.into(),
            home: Home::Path(home.display().to_string()),
        });
        home
    }

    fn attribute(&mut self, session_id: &str, account: &str) {
        self.attribution.add(session_id, account);
    }

    fn sources(&self) -> Vec<Source> {
        stats::sources(&self.accounts, &self.env)
    }

    fn refresh(&mut self) -> RefreshStats {
        let sources = self.sources();
        stats::refresh(&mut self.cache, &sources, |_, _| {})
    }

    fn report(&self) -> Report {
        stats::report(
            &self.cache,
            &self.sources(),
            &self.attribution,
            &self.accounts,
            &self.prices,
            self.now,
            &self.tz,
        )
    }

    /// Refreshes, then returns the all-time table.
    fn all(&mut self) -> Table {
        self.refresh();
        self.report().table(Period::All).clone()
    }
}

fn write_file(path: &Path, contents: &str) -> PathBuf {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
    path.to_path_buf()
}

fn append(path: &Path, contents: &str) {
    let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(contents.as_bytes()).unwrap();
}

/// `record` with `edit` applied to its JSON.
fn edited(record: &str, edit: impl FnOnce(&mut Value)) -> String {
    let mut v: Value = serde_json::from_str(record).unwrap();
    edit(&mut v);
    cl::line(v)
}

/// Tokens with the cache write all 1-hour, as `cl::usage` records it.
fn toks(input: u64, cache_read: u64, cache_write: u64, output: u64, reasoning: u64) -> Tokens {
    toks6(input, cache_read, 0, cache_write, output, reasoning)
}

fn toks6(input: u64, read: u64, w5m: u64, w1h: u64, output: u64, reasoning: u64) -> Tokens {
    Tokens {
        input,
        cache_read: read,
        cache_write_5m: w5m,
        cache_write_1h: w1h,
        output,
        reasoning,
    }
}

/// The section of exactly `accounts`.
fn section<'a>(table: &'a Table, accounts: &[&str]) -> &'a Section {
    table
        .sections
        .iter()
        .find(|s| s.accounts == accounts)
        .unwrap_or_else(|| panic!("no section {accounts:?}: {:#?}", table.sections))
}

/// The tokens of `model` in `rows`; zero when absent.
fn of(rows: &[ModelRow], model: &str) -> Tokens {
    rows.iter()
        .find(|r| r.model == model)
        .map_or_else(Tokens::default, |r| r.tokens)
}

fn model_names(rows: &[ModelRow]) -> Vec<&str> {
    rows.iter().map(|r| r.model.as_str()).collect()
}

/// `msg_a1` of session `S_A` in 3 records, its output growing, then `msg_a2`.
fn msg_a1_records() -> [String; 3] {
    [(10, 1), (25, 2), (40, 3)].map(|(output, minute)| {
        cl::assistant_usage(
            S_A,
            "msg_a1",
            "claude-test",
            cl::usage(3, output, 100, 300),
            &cl::ts(minute),
        )
    })
}

fn msg_a2() -> String {
    cl::assistant_usage(
        S_A,
        "msg_a2",
        "claude-test",
        cl::usage(1, 5, 0, 200),
        &cl::ts(4),
    )
}

/// `msg_a1` + `msg_a2`, each counted once.
const A1_A2: Tokens = Tokens {
    input: 4,
    cache_read: 500,
    cache_write_5m: 0,
    cache_write_1h: 100,
    output: 45,
    reasoning: 0,
};

/// R20: claude writes one record per content block, each repeating the message's usage with
/// its output growing; the message counts once, with the largest of each count.
#[test]
fn a_message_written_in_several_records_counts_once() {
    let mut f = Fixture::new();
    f.write(
        &format!("{S_A}.jsonl"),
        &[msg_a1_records().concat(), msg_a2()].concat(),
    );
    f.attribute(S_A, "claude:default");
    let table = f.all();
    let default = section(&table, &["claude:default"]);
    assert_eq!(default.models.len(), 1);
    assert_eq!(default.models[0].provider, Provider::Claude);
    assert_eq!(of(&default.models, "claude-test"), A1_A2);
    assert_eq!(default.total(), A1_A2);
    assert_eq!(table.overall, default.models);
}

/// R20: records of one message read in two refreshes merge by their key.
#[test]
fn a_message_split_across_two_refreshes_counts_once() {
    let mut f = Fixture::new();
    let [r1, r2, r3] = msg_a1_records();
    let path = f.write(&format!("{S_A}.jsonl"), &[r1, r2].concat());
    f.attribute(S_A, "claude:default");
    let table = f.all();
    assert_eq!(of(&table.overall, "claude-test"), toks(3, 300, 100, 25, 0));

    append(&path, &[r3, msg_a2()].concat());
    let s = f.refresh();
    assert_eq!((s.incremental, s.cold, s.files), (1, 0, 1));
    let table = f.report().table(Period::All).clone();
    assert_eq!(
        of(&table.overall, "claude-test"),
        A1_A2,
        "output 40, not 25 + 40"
    );
}

/// R20: only assistant records count: not a `<synthetic>` message, a tool result's usage, a
/// `progress` record repeating a subagent's message, a record without `message.id`, a
/// malformed line, or a line still being written (until it is complete).
#[test]
fn only_assistant_records_count() {
    let mut f = Fixture::new();
    let subagent = cl::sidechain(
        &cl::assistant_usage(
            S_A,
            "msg_s1",
            "claude-haiku-test",
            cl::usage(10, 2, 0, 0),
            &cl::ts(2),
        ),
        "s1",
    );
    let unfinished = cl::assistant_usage(
        S_A,
        "msg_a3",
        "claude-test",
        cl::usage(1, 1, 0, 0),
        &cl::ts(9),
    );
    let text = [
        cl::assistant_usage(
            S_A,
            "msg_syn",
            "<synthetic>",
            cl::usage(0, 0, 0, 0),
            &cl::ts(1),
        ),
        cl::assistant_usage(
            S_A,
            "msg_syn2",
            "<synthetic>",
            cl::usage(9, 9, 9, 9),
            &cl::ts(1),
        ),
        cl::task_result(cl::usage(900, 900, 900, 900), &cl::ts(2)),
        cl::agent_progress(&subagent, "s1", &cl::ts(2)),
        edited(
            &cl::assistant_usage(
                S_A,
                "msg_noid",
                "claude-test",
                cl::usage(7, 7, 7, 7),
                &cl::ts(3),
            ),
            |v| {
                v["message"].as_object_mut().unwrap().remove("id");
            },
        ),
        "{\"type\":\"assistant\",\"usage\" not json\n".to_string(),
        msg_a2(),
        unfinished.trim_end().to_string(),
    ]
    .concat();
    let path = f.write(&format!("{S_A}.jsonl"), &text);
    f.attribute(S_A, "claude:default");
    let table = f.all();
    assert_eq!(model_names(&table.overall), ["claude-test"]);
    assert_eq!(of(&table.overall, "claude-test"), toks(1, 200, 0, 5, 0));

    append(&path, "\n");
    let table = f.all();
    assert_eq!(of(&table.overall, "claude-test"), toks(2, 200, 0, 6, 0));
}

/// R20: transcripts are read in 8 MB pieces; a line longer than one, or cut by the end of one,
/// is carried over to the next.
#[test]
fn lines_across_reads_are_parsed() {
    const READ: usize = 8 << 20;
    let mut f = Fixture::new();
    let [a1, _, a1_last] = msg_a1_records();
    // No line ends in the first read.
    let head = [cl::filler(READ + 1000, "/w/proj", &cl::ts(1)), a1].concat();
    // `msg_a2` starts 50 bytes before the end of the second read.
    let overhead = cl::filler(0, "/w/proj", &cl::ts(1)).len();
    let pad = cl::filler(2 * READ - 50 - head.len() - overhead, "/w/proj", &cl::ts(1));
    let text = [head, pad, msg_a2(), a1_last].concat();
    assert!(text.len() > 2 * READ);
    f.write(&format!("{S_A}.jsonl"), &text);
    let s = f.refresh();
    assert_eq!(s.bytes_read, text.len() as u64);
    let table = f.report().table(Period::All).clone();
    assert_eq!(of(&table.overall, "claude-test"), A1_A2);
}

/// R20: an `advisor_message` in `usage.iterations` is a request to its own model, not included
/// in the top-level usage.
#[test]
fn advisor_calls_count_under_their_model() {
    let mut f = Fixture::new();
    let iteration = |kind: &str, input: u64, output: u64, cache_write: u64, cache_read: u64| {
        json!({"input_tokens": input, "output_tokens": output,
               "cache_read_input_tokens": cache_read, "cache_creation_input_tokens": cache_write,
               "cache_creation": {"ephemeral_5m_input_tokens": 0,
                                  "ephemeral_1h_input_tokens": cache_write},
               "type": kind})
    };
    let mut usage = cl::usage(4, 45, 60, 100);
    let mut advisor = iteration("advisor_message", 700, 70, 0, 0);
    advisor["model"] = json!("claude-advisor-test");
    usage["iterations"] = json!([
        iteration("message", 2, 20, 30, 50),
        advisor,
        iteration("message", 2, 25, 30, 50),
    ]);
    // Two content blocks: two records repeating the same usage.
    let record = cl::assistant_usage(S_A, "msg_a3", "claude-test", usage, &cl::ts(1));
    f.write(&format!("{S_A}.jsonl"), &[record.clone(), record].concat());
    f.attribute(S_A, "claude:default");
    let table = f.all();
    let default = section(&table, &["claude:default"]);
    assert_eq!(
        model_names(&default.models),
        ["claude-advisor-test", "claude-test"]
    );
    assert_eq!(of(&default.models, "claude-test"), toks(4, 100, 60, 45, 0));
    assert_eq!(
        of(&default.models, "claude-advisor-test"),
        toks(700, 0, 0, 70, 0)
    );
}

/// R20: every `*.jsonl` below `<project>/<session id>/` is a subagent transcript of that
/// session.
#[test]
fn subagent_transcripts_count_for_their_session() {
    let mut f = Fixture::new();
    let sub = |id: &str, usage: Value, agent: &str| {
        cl::sidechain(
            &cl::assistant_usage(S_A, id, "claude-haiku-test", usage, &cl::ts(1)),
            agent,
        )
    };
    f.write(
        &format!("{S_A}/subagents/agent-s1.jsonl"),
        &sub("msg_s1", cl::usage(10, 2, 0, 0), "s1"),
    );
    f.write(
        &format!("{S_A}/subagents/workflows/wf_1/agent-s2.jsonl"),
        &sub("msg_s2", cl::usage(20, 3, 0, 0), "s2"),
    );
    // Not a transcript.
    f.write(&format!("{S_A}/subagents/agent-s1.meta.json"), "{}");
    f.attribute(S_A, "claude:default");
    let s = f.refresh();
    assert_eq!(s.files, 2);
    let table = f.report().table(Period::All).clone();
    let default = section(&table, &["claude:default"]);
    assert_eq!(model_names(&default.models), ["claude-haiku-test"]);
    assert_eq!(
        of(&default.models, "claude-haiku-test"),
        toks(30, 0, 0, 5, 0)
    );
    assert_eq!(table.sections.len(), 1, "nothing unattributed");
}

/// R20: a fork's copies of its parent's records (`forkedFrom`, same `message.id` and
/// timestamp) count for the parent's session, even when the fork's path sorts first.
#[test]
fn a_forked_copy_counts_for_the_original() {
    let mut f = Fixture::new();
    f.claude("max");
    f.write(&format!("{S_A}.jsonl"), &msg_a1_records().concat());
    // "0…" sorts before S_A: the copy would win a tie on the path.
    let fork = "0bbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let copies: String = msg_a1_records()
        .iter()
        .map(|r| cl::forked(r, S_A))
        .collect();
    let own = cl::assistant_usage(
        fork,
        "msg_b1",
        "claude-test",
        cl::usage(7, 7, 0, 0),
        &cl::ts(5),
    );
    f.write(&format!("{fork}.jsonl"), &[copies, own].concat());
    f.attribute(S_A, "claude:default");
    f.attribute(fork, "claude:max");
    let table = f.all();
    assert_eq!(
        of(&section(&table, &["claude:default"]).models, "claude-test"),
        toks(3, 300, 100, 40, 0)
    );
    assert_eq!(
        of(&section(&table, &["claude:max"]).models, "claude-test"),
        toks(7, 0, 0, 7, 0)
    );
    assert_eq!(of(&table.overall, "claude-test"), toks(10, 300, 100, 47, 0));
}

/// R20, R19: a relay's copy of a transcript in another store counts once, for the original.
/// Both copies are the same session, so which one counts only shows in the ranking: a
/// launch-log relay copy loses to any other copy, then the earliest timestamp wins, then the
/// path that sorts first.
#[test]
fn a_relay_copy_counts_for_the_original() {
    let mut f = Fixture::new();
    // `<root>/a-team` sorts before `<root>/home`: the copy would win a tie on the path.
    let team = f.root.join("a-team");
    fs::create_dir_all(team.join("projects")).unwrap();
    f.accounts.push(Account {
        provider: Provider::Claude,
        name: "team".into(),
        home: Home::Path(team.display().to_string()),
    });
    let text = [msg_a1_records().concat(), msg_a2()].concat();
    let original = f.write(&format!("{S_A}.jsonl"), &text);
    let copy = write_file(&team.join(format!("projects/-w-proj/{S_A}.jsonl")), &text);
    f.attribute(S_A, "claude:default");
    let table = f.all();
    assert_eq!(f.cache.files.len(), 2);
    assert_eq!(of(&table.overall, "claude-test"), A1_A2, "counted once");
    assert_eq!(
        of(&section(&table, &["claude:default"]).models, "claude-test"),
        A1_A2
    );
    assert!(section(&table, &["claude:team"]).models.is_empty());

    // Which copy counts, made visible: give each a session of its own, as if the copy were
    // another session holding the same messages.
    let rank = |f: &Fixture, relay: bool| {
        let mut cache = f.cache.clone();
        cache.files.get_mut(&copy).unwrap().session_id = "copy".into();
        let log = f.root.join("launches.jsonl");
        let line = json!({"ts": "2026-09-20T10:10:00Z", "account": "claude:team",
            "session_id": "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
            "relay": {"source": original, "transcript": copy, "checkpoints": [],
                      "size": 1, "mtime_ns": 1}});
        fs::write(
            &log,
            if relay {
                format!("{line}\n")
            } else {
                String::new()
            },
        )
        .unwrap();
        let mut attribution = Attribution::default();
        attribution.add_launch_log(&log);
        attribution.add(S_A, "claude:default");
        attribution.add("copy", "claude:team");
        assert_eq!(attribution.is_relay_copy(&copy), relay);
        let report = stats::report(
            &cache,
            &f.sources(),
            &attribution,
            &f.accounts,
            &f.prices,
            f.now,
            &f.tz,
        );
        let table = report.table(Period::All);
        (
            section(table, &["claude:default"]).total(),
            section(table, &["claude:team"]).total(),
        )
    };
    assert_eq!(rank(&f, true), (A1_A2, Tokens::default()));
    assert_eq!(
        rank(&f, false),
        (Tokens::default(), A1_A2),
        "a tie goes to the path that sorts first"
    );
}

/// R20, R8: a store several homes share is read once, for those accounts together.
#[test]
fn a_shared_store_is_read_once() {
    let mut f = Fixture::new();
    let max = f.claude("max");
    symlink(f.projects(), max.join("projects")).unwrap();
    f.write(&format!("{S_A}.jsonl"), &msg_a1_records().concat());
    let sources = f.sources();
    assert_eq!(
        sources,
        [Source {
            kind: SourceKind::Claude,
            path: f.projects(),
            accounts: vec!["claude:default".into(), "claude:max".into()],
        }]
    );
    f.attribute(S_A, "claude:max");
    let s = f.refresh();
    assert_eq!((s.files, s.cold), (1, 1));
    let table = f.report().table(Period::All).clone();
    assert_eq!(
        of(&table.overall, "claude-test"),
        toks(3, 300, 100, 40, 0),
        "not doubled"
    );
    assert_eq!(
        of(&section(&table, &["claude:max"]).models, "claude-test"),
        toks(3, 300, 100, 40, 0)
    );
}

/// Rollout R1: a repeated event, a compaction estimate, and a model switch; `ts(minute)` gives
/// the timestamps.
fn r1_records(ts: impl Fn(u32) -> String) -> String {
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
}

/// R20: a repeated `token_count` event, and the one recording the context size after
/// compacting (same total), are skipped; each other one counts its latest request under the
/// model of the last `turn_context`.
#[test]
fn codex_repeats_and_compaction_estimates_are_skipped() {
    let mut f = Fixture::new();
    let work = f.codex("work");
    cx::write_rollout(&work, R1, &r1_records(cx::ts));
    let table = f.all();
    let section = section(&table, &["codex:work"]);
    assert_eq!(model_names(&section.models), ["gpt-test-a", "gpt-test-b"]);
    assert!(section.models.iter().all(|m| m.provider == Provider::Codex));
    assert_eq!(of(&section.models, "gpt-test-a"), toks(100, 150, 0, 30, 10));
    assert_eq!(of(&section.models, "gpt-test-b"), toks(50, 100, 0, 15, 2));
    assert_eq!(section.total().total(), 445, "the last total_tokens");
}

/// R20: a fork that replays its parent's `token_count` events (with its own timestamps and the
/// parent's totals) counts only its own requests; the replayed ones count at the parent's time.
#[test]
fn codex_fork_replays_count_once_at_the_original_time() {
    let fork = |ts: &dyn Fn(u32) -> String, replay: &str| {
        [
            cx::fork_meta(R2, R1, "/w/proj", &ts(0)),
            // The parent's records after its session_meta, replayed.
            replay
                .lines()
                .skip(1)
                .map(|l| format!("{l}\n"))
                .collect::<String>(),
            cx::model_turn("gpt-test-b", &ts(1)),
            cx::tokens([520, 330, 55, 13], [120, 80, 10, 1], &ts(2)),
        ]
        .concat()
    };
    let mut f = Fixture::new();
    let work = f.codex("work");
    cx::write_rollout(&work, R1, &r1_records(cx::ts));
    cx::write_rollout(
        &work,
        R2,
        &fork(&|m| cx::ts(m + 20), &r1_records(|m| cx::ts(m + 10))),
    );
    let table = f.all();
    assert_eq!(of(&table.overall, "gpt-test-a"), toks(100, 150, 0, 30, 10));
    assert_eq!(
        of(&table.overall, "gpt-test-b"),
        toks(50 + 40, 100 + 80, 0, 15 + 10, 2 + 1)
    );

    // The parent 40 days ago, the fork (and its replay) today.
    let mut f = Fixture::new();
    let work = f.codex("work");
    let old = |m: u32| format!("2026-08-15T10:{m:02}:00.000Z");
    let today = |m: u32| format!("2026-09-24T01:{m:02}:00.000Z");
    cx::write_rollout(&work, R1, &r1_records(old));
    cx::write_rollout(
        &work,
        R2,
        &fork(&|m| today(m + 30), &r1_records(|m| today(m + 10))),
    );
    f.refresh();
    let report = f.report();
    assert_eq!(
        report.table(Period::Today).overall,
        [ModelRow {
            provider: Provider::Codex,
            model: "gpt-test-b".into(),
            tokens: toks(40, 80, 0, 10, 1),
            // gpt-test-b has no price.
            cost: Cost {
                pico_usd: 0,
                unpriced_tokens: 130,
            },
        }]
    );
    assert_eq!(
        of(&report.table(Period::All).overall, "gpt-test-a"),
        toks(100, 150, 0, 30, 10)
    );
}

/// R20: a subagent rollout's first total includes what it inherited; only its latest request
/// counts.
#[test]
fn codex_inherited_totals_are_not_counted() {
    let mut f = Fixture::new();
    let work = f.codex("work");
    let text = [
        cx::meta(
            R3,
            "/w/proj",
            json!({"subagent": {"thread_spawn": {"parent_thread_id": R1, "depth": 1}}}),
            0,
            &cx::ts(0),
        ),
        cx::parent_meta(R1, "/w/proj", &cx::ts(0)),
        cx::model_turn("gpt-test-a", &cx::ts(1)),
        cx::tokens([1000, 800, 50, 10], [100, 80, 5, 1], &cx::ts(2)),
    ]
    .concat();
    cx::write_rollout(&work, R3, &text);
    let table = f.all();
    assert_eq!(table.overall.len(), 1);
    assert_eq!(of(&table.overall, "gpt-test-a"), toks(20, 80, 0, 5, 1));
}

/// R20: rollouts in `archived_sessions` count, for the accounts of that home.
#[test]
fn codex_archived_rollouts_count() {
    let mut f = Fixture::new();
    let work = f.codex("work");
    let text = [
        cx::meta(R1, "/w/proj", json!("cli"), 0, &cx::ts(0)),
        cx::model_turn("gpt-test-a", &cx::ts(1)),
        cx::tokens([100, 50, 10, 4], [100, 50, 10, 4], &cx::ts(2)),
    ]
    .concat();
    cx::write_archived_rollout(&work, R1, &text);
    assert_eq!(
        f.sources()
            .iter()
            .map(|s| (s.kind, s.accounts.clone()))
            .collect::<Vec<_>>(),
        [
            (SourceKind::Claude, vec!["claude:default".to_string()]),
            (SourceKind::CodexSessions, vec!["codex:work".to_string()]),
            (SourceKind::CodexArchived, vec!["codex:work".to_string()]),
        ]
    );
    let table = f.all();
    assert_eq!(
        of(&section(&table, &["codex:work"]).models, "gpt-test-a"),
        toks(50, 50, 0, 10, 4)
    );
}

/// R20: events before the first `turn_context` (a rollout that starts compacted) take its
/// model, also when it arrives in a later refresh; without any, the model is `unknown`.
#[test]
fn codex_events_before_the_first_turn_take_its_model() {
    let mut f = Fixture::new();
    let work = f.codex("work");
    let before = [
        cx::meta(R1, "/w/proj", json!("cli"), 0, &cx::ts(0)),
        cx::compacted(&cx::ts(1)),
        cx::tokens([60, 20, 6, 0], [60, 20, 6, 0], &cx::ts(2)),
    ]
    .concat();
    let after = [
        cx::model_turn("gpt-test-c", &cx::ts(3)),
        cx::tokens([90, 40, 9, 0], [30, 20, 3, 0], &cx::ts(4)),
    ]
    .concat();
    cx::write_rollout(&work, R1, &[before.as_str(), &after].concat());
    let table = f.all();
    assert_eq!(model_names(&table.overall), ["gpt-test-c"]);
    assert_eq!(of(&table.overall, "gpt-test-c"), toks(50, 40, 0, 9, 0));

    // The turn_context arrives in a later refresh.
    let mut f = Fixture::new();
    let work = f.codex("work");
    let path = cx::write_rollout(&work, R1, &before);
    let table = f.all();
    assert_eq!(model_names(&table.overall), ["unknown"]);
    assert_eq!(of(&table.overall, "unknown"), toks(40, 20, 0, 6, 0));
    append(&path, &after);
    let table = f.all();
    assert_eq!(model_names(&table.overall), ["gpt-test-c"]);
    assert_eq!(of(&table.overall, "gpt-test-c"), toks(50, 40, 0, 9, 0));
}

/// R20, R17: a rollout of a store several codex homes share counts for those accounts
/// together.
#[test]
fn a_shared_codex_store_counts_for_its_accounts_together() {
    let mut f = Fixture::new();
    let a = f.codex("a");
    let b = f.root.join("c/b");
    fs::create_dir_all(&b).unwrap();
    symlink(a.join("sessions"), b.join("sessions")).unwrap();
    f.accounts.push(Account {
        provider: Provider::Codex,
        name: "b".into(),
        home: Home::Path(b.display().to_string()),
    });
    let text = [
        cx::model_turn("gpt-test-a", &cx::ts(1)),
        cx::tokens([100, 50, 10, 4], [100, 50, 10, 4], &cx::ts(2)),
    ]
    .concat();
    cx::write_rollout(&a, R1, &text);
    let table = f.all();
    let accounts: Vec<String> = table
        .sections
        .iter()
        .map(|s| s.accounts.join(" + "))
        .collect();
    assert_eq!(
        accounts,
        ["claude:default", "codex:a", "codex:b", "codex:a + codex:b"]
    );
    assert!(section(&table, &["codex:a"]).models.is_empty());
    assert_eq!(
        of(
            &section(&table, &["codex:a", "codex:b"]).models,
            "gpt-test-a"
        ),
        toks(50, 50, 0, 10, 4)
    );
}

/// R20: a period starts at local midnight of today, 6 days before or 29 days before; all also
/// has messages without a timestamp.
#[test]
fn periods_start_at_local_midnight() {
    let mut f = Fixture::new();
    f.tz = TimeZone::fixed(jiff::tz::offset(8));
    let at = [
        Some("2026-09-23T16:00:00Z"),
        Some("2026-09-23T15:59:59Z"),
        Some("2026-09-17T16:00:00Z"),
        Some("2026-09-17T15:59:59Z"),
        Some("2026-08-25T16:00:00Z"),
        Some("2026-08-25T15:59:59Z"),
        None,
    ];
    // Output 1, 2, 4, …: a sum tells which messages a period holds.
    let text: String = at
        .iter()
        .enumerate()
        .map(|(i, ts)| {
            let record = cl::assistant_usage(
                S_A,
                &format!("msg_{i}"),
                "claude-test",
                cl::usage(0, 1 << i, 0, 0),
                ts.unwrap_or("x"),
            );
            match ts {
                Some(_) => record,
                None => edited(&record, |v| {
                    v.as_object_mut().unwrap().remove("timestamp");
                }),
            }
        })
        .collect();
    f.write(&format!("{S_A}.jsonl"), &text);
    f.refresh();
    let report = f.report();
    let output = |p: Period| of(&report.table(p).overall, "claude-test").output;
    assert_eq!(output(Period::Today), 0b1);
    assert_eq!(output(Period::Week), 0b111);
    assert_eq!(output(Period::Month), 0b11111);
    assert_eq!(output(Period::All), 0b1111111);
    let since = |p: Period| report.table(p).since.map(|t| t.to_string());
    assert_eq!(
        since(Period::Today).as_deref(),
        Some("2026-09-23T16:00:00Z")
    );
    assert_eq!(since(Period::Week).as_deref(), Some("2026-09-17T16:00:00Z"));
    assert_eq!(
        since(Period::Month).as_deref(),
        Some("2026-08-25T16:00:00Z")
    );
    assert_eq!(since(Period::All), None);
    assert_eq!(
        report.tables.iter().map(|t| t.period).collect::<Vec<_>>(),
        Period::ALL
    );
}

/// R20: every registered account (also with nothing), then each other group of accounts, by
/// its first account's registry position, size and names (an account no longer registered
/// comes last), then unattributed; models most tokens first, then by provider and model.
#[test]
fn sections_list_every_account_then_groups_then_unattributed() {
    let mut f = Fixture::new();
    f.claude("max");
    f.claude("team");
    let work = f.codex("work");
    let session = |n: u32| format!("{n:08}-0000-4000-8000-000000000000");
    let message = |n: u32, model: &str, input: u64| {
        cl::assistant_usage(
            &session(n),
            &format!("msg_{n}_{model}"),
            model,
            cl::usage(input, 0, 0, 0),
            &cl::ts(n),
        )
    };
    f.write(
        &format!("{}.jsonl", session(1)),
        &[
            message(1, "claude-b", 100),
            message(1, "claude-c", 500),
            message(1, "claude-a", 100),
        ]
        .concat(),
    );
    for n in 2..=7 {
        f.write(&format!("{}.jsonl", session(n)), &message(n, "claude-a", 1));
    }
    f.attribute(&session(1), "claude:default");
    f.attribute(&session(2), "claude:max");
    f.attribute(&session(2), "claude:default");
    for account in ["claude:team", "claude:default", "claude:max"] {
        f.attribute(&session(3), account);
    }
    f.attribute(&session(4), "claude:team");
    f.attribute(&session(4), "claude:max");
    // A removed account, named by the launch log.
    let log = f.root.join("launches.jsonl");
    fs::write(
        &log,
        [(5, "claude:gone"), (6, "claude:gone")]
            .iter()
            .map(|(n, account)| {
                format!(
                    "{}\n",
                    json!({"ts": "2026-09-20T10:00:00Z", "account": account,
                           "session_id": session(*n)})
                )
            })
            .collect::<String>(),
    )
    .unwrap();
    f.attribution.add_launch_log(&log);
    f.attribute(&session(6), "claude:default");
    // Session 7 is unattributed.
    cx::write_rollout(
        &work,
        R1,
        &[
            cx::model_turn("gpt-a", &cx::ts(1)),
            cx::tokens([100, 0, 0, 0], [100, 0, 0, 0], &cx::ts(2)),
        ]
        .concat(),
    );

    let table = f.all();
    let accounts: Vec<String> = table
        .sections
        .iter()
        .map(|s| s.accounts.join(" + "))
        .collect();
    assert_eq!(
        accounts,
        [
            "claude:default",
            "claude:max",
            "claude:team",
            "codex:work",
            "claude:default + claude:gone",
            "claude:default + claude:max",
            "claude:default + claude:max + claude:team",
            "claude:max + claude:team",
            "claude:gone",
            "",
        ]
    );
    let default = section(&table, &["claude:default"]);
    assert_eq!(
        model_names(&default.models),
        ["claude-c", "claude-a", "claude-b"]
    );
    assert!(section(&table, &["claude:max"]).models.is_empty());
    assert!(section(&table, &["claude:team"]).models.is_empty());
    assert_eq!(
        of(&section(&table, &[]).models, "claude-a"),
        toks(1, 0, 0, 0, 0)
    );
    // claude-a: 100 + 6 × 1; claude-b and gpt-a tie at 100: claude first.
    assert_eq!(
        table
            .overall
            .iter()
            .map(|m| (m.provider, m.model.as_str(), m.tokens.total()))
            .collect::<Vec<_>>(),
        [
            (Provider::Claude, "claude-c", 500),
            (Provider::Claude, "claude-a", 106),
            (Provider::Claude, "claude-b", 100),
            (Provider::Codex, "gpt-a", 100),
        ]
    );
    // The sections add up to the overall total.
    let mut sum = Tokens::default();
    for s in &table.sections {
        sum.add(&s.total());
    }
    let mut overall = Tokens::default();
    for m in &table.overall {
        overall.add(&m.tokens);
    }
    assert_eq!(sum, overall);
}

/// R20, R3: the cache round-trips, an unchanged file is not read again, and a cache of another
/// schema (or garbage) is rebuilt.
#[test]
fn cache_round_trips_and_a_schema_mismatch_rebuilds() {
    let mut f = Fixture::new();
    let work = f.codex("work");
    f.write(&format!("{S_A}.jsonl"), &msg_a1_records().concat());
    cx::write_rollout(&work, R1, &r1_records(cx::ts));
    let path = f.root.join("remuda/state/stats.json");
    assert!(Cache::load(&path).files.is_empty());
    let mut calls = Vec::new();
    let sources = f.sources();
    let s = stats::refresh(&mut f.cache, &sources, |done, total| {
        calls.push((done, total))
    });
    assert_eq!((s.files, s.cold), (2, 2));
    assert_eq!(calls.first(), Some(&(0, 2)));
    assert_eq!(calls.last(), Some(&(2, 2)));
    f.cache.save(&path).unwrap();

    let mut loaded = Cache::load(&path);
    assert_eq!(loaded, f.cache);
    let s = stats::refresh(&mut loaded, &sources, |_, _| {});
    assert_eq!(
        (s.reused, s.cold, s.incremental, s.bytes_read),
        (2, 0, 0, 0)
    );
    let v: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(v["schema_version"], SCHEMA_VERSION);
    let claude = &v["files"][f
        .projects()
        .join(format!("-w-proj/{S_A}.jsonl"))
        .to_str()
        .unwrap()];
    assert_eq!(claude["session_id"], S_A);
    assert_eq!(claude["models"], json!(["claude-test"]));
    // One row per request: [key, ts, model, flags, input, cache read, cache write 5m, cache
    // write 1h, output, reasoning].
    let row = claude["rows"][0].as_array().unwrap();
    assert_eq!(row.len(), 10);
    assert_eq!(
        row[1..],
        json!([1_789_898_460, 0, 0, 3, 300, 0, 100, 40, 0])
            .as_array()
            .unwrap()[..]
    );
    // A cache of schema 1 (a single cache write) is rebuilt.
    assert_eq!(SCHEMA_VERSION, 2);
    let old = f.root.join("old-stats.json");
    fs::write(&old, "{\"schema_version\":1,\"files\":{}}").unwrap();
    let loaded = Cache::load(&old);
    assert!(loaded.files.is_empty());
    assert_eq!(loaded.schema_version, 2);

    let mut other = v.clone();
    other["schema_version"] = json!(SCHEMA_VERSION + 1);
    fs::write(&path, other.to_string()).unwrap();
    let mut rebuilt = Cache::load(&path);
    assert!(rebuilt.files.is_empty());
    assert_eq!(rebuilt.schema_version, SCHEMA_VERSION);
    assert_eq!(stats::refresh(&mut rebuilt, &sources, |_, _| {}).cold, 2);
    for garbage in ["", "{", "[]", "{\"schema_version\": 1, \"files\": 7}"] {
        fs::write(&path, garbage).unwrap();
        assert!(Cache::load(&path).files.is_empty(), "{garbage:?}");
    }
}

/// R20, R8: a file that shrank or was replaced is read again whole: its counts are replaced,
/// not added to.
#[test]
fn a_shrunk_or_replaced_file_is_read_again_whole() {
    let mut f = Fixture::new();
    let path = f.write(
        &format!("{S_A}.jsonl"),
        &[msg_a1_records().concat(), msg_a2()].concat(),
    );
    assert_eq!(of(&f.all().overall, "claude-test"), A1_A2);

    // Shrunk.
    fs::write(&path, msg_a2()).unwrap();
    let s = f.refresh();
    assert_eq!((s.cold, s.incremental), (1, 0));
    let table = f.report().table(Period::All).clone();
    assert_eq!(of(&table.overall, "claude-test"), toks(1, 200, 0, 5, 0));

    // Replaced by a larger file (another inode): it looks grown, but is read whole.
    let other = f.projects().join("replacement");
    fs::write(&other, [msg_a2(), msg_a1_records()[0].clone()].concat()).unwrap();
    fs::rename(&other, &path).unwrap();
    let s = f.refresh();
    assert_eq!((s.cold, s.incremental), (1, 0));
    let table = f.report().table(Period::All).clone();
    assert_eq!(of(&table.overall, "claude-test"), toks(4, 500, 100, 15, 0));
}

/// R20: only transcripts that exist count.
#[test]
fn vanished_files_stop_counting() {
    let mut f = Fixture::new();
    let a = f.write(&format!("{S_A}.jsonl"), &msg_a1_records().concat());
    f.write(
        &format!("{S_B}.jsonl"),
        &cl::assistant_usage(
            S_B,
            "msg_b1",
            "claude-test",
            cl::usage(7, 7, 0, 0),
            &cl::ts(5),
        ),
    );
    assert_eq!(
        of(&f.all().overall, "claude-test"),
        toks(10, 300, 100, 47, 0)
    );
    fs::remove_file(&a).unwrap();
    let s = f.refresh();
    assert_eq!((s.removed, s.files), (1, 1));
    let table = f.report().table(Period::All).clone();
    assert_eq!(of(&table.overall, "claude-test"), toks(7, 0, 0, 7, 0));
    assert_eq!(f.report().files, 1);
}

/// The row of `model` in the cache entry of `path`, as the cache stores it.
fn cached_row(f: &Fixture, path: &Path, model: &str) -> Vec<Value> {
    let file = serde_json::to_value(&f.cache.files[path]).unwrap();
    let index = file["models"]
        .as_array()
        .unwrap()
        .iter()
        .position(|m| m == model)
        .unwrap_or_else(|| panic!("no {model} in {file}"));
    file["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r[2] == index)
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

/// R20: the cache write is split by lifetime: `cache_creation` of the message, or, with
/// several `message` entries in `usage.iterations`, their sum; an advisor call by its own; a
/// cache write recorded without lifetimes is 5-minute.
#[test]
fn cache_write_is_split_by_lifetime() {
    let mut f = Fixture::new();
    let record = |id: &str, model: &str, usage: Value| {
        cl::assistant_usage(S_A, id, model, usage, &cl::ts(1))
    };
    let unsplit = edited(&record("msg_c2", "claude-b", cl::usage(1, 1, 50, 0)), |v| {
        v["message"]["usage"]
            .as_object_mut()
            .unwrap()
            .remove("cache_creation");
    });
    // As claude records a message with an advisor call: the top-level split is the first
    // `message` entry's.
    let entry = |kind: &str, write: u64, m5: u64, h1: u64| {
        json!({"type": kind, "input_tokens": 1, "output_tokens": 1,
               "cache_read_input_tokens": 0, "cache_creation_input_tokens": write,
               "cache_creation": {"ephemeral_5m_input_tokens": m5,
                                  "ephemeral_1h_input_tokens": h1}})
    };
    let mut advised = cl::usage(2, 2, 3430, 0);
    advised["cache_creation"] = json!({"ephemeral_5m_input_tokens": 0,
                                       "ephemeral_1h_input_tokens": 896});
    let mut advisor = entry("advisor_message", 40, 40, 0);
    advisor["model"] = json!("claude-advisor-test");
    advised["iterations"] = json!([
        entry("message", 896, 0, 896),
        advisor,
        entry("message", 2534, 2534, 0)
    ]);
    f.write(
        &format!("{S_A}.jsonl"),
        &[
            record("msg_c1", "claude-a", cl::usage_split(1, 1, 30, 70, 0)),
            unsplit,
            record("msg_c3", "claude-c", advised),
        ]
        .concat(),
    );
    let table = f.all();
    let split = |model: &str| {
        let t = of(&table.overall, model);
        (t.cache_write_5m, t.cache_write_1h)
    };
    assert_eq!(split("claude-a"), (30, 70));
    assert_eq!(split("claude-b"), (50, 0));
    assert_eq!(split("claude-c"), (2534, 896));
    assert_eq!(of(&table.overall, "claude-c").cache_write(), 3430);
    assert_eq!(split("claude-advisor-test"), (40, 0));
}

/// R20: fast mode (`usage.speed`) and US-only inference (`usage.inference_geo`) are kept with
/// the request, from any of its records or copies, and priced; an advisor call has neither.
#[test]
fn fast_and_us_flags_are_recorded_and_priced() {
    let mut f = Fixture::new();
    let opus = "claude-opus-5-5";
    let mut flagged = cl::with_flags(cl::usage_split(1_000_000, 0, 0, 0, 0), "fast", "us");
    flagged["iterations"] = json!([
        {"type": "message", "input_tokens": 1_000_000, "output_tokens": 0,
         "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
        {"type": "advisor_message", "model": "claude-advisor-test", "input_tokens": 10,
         "output_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
    ]);
    let plain = cl::usage_split(1_000_000, 0, 0, 0, 0);
    let at = cl::ts(1);
    // msg_f1: a record without the flags, then two with them.
    let f1 = |usage: &Value| cl::assistant_usage(S_A, "msg_f1", opus, usage.clone(), &at);
    // msg_f2: without them here, with them in a fork's copy.
    let f2 = |usage: &Value| cl::assistant_usage(S_A, "msg_f2", opus, usage.clone(), &at);
    let a = f.write(
        &format!("{S_A}.jsonl"),
        &[f1(&plain), f1(&flagged), f1(&flagged), f2(&plain)].concat(),
    );
    f.write(&format!("{S_B}.jsonl"), &cl::forked(&f2(&flagged), S_A));
    let table = f.all();
    let row = cached_row(&f, &a, opus);
    assert_eq!(row[3], 6, "fast and US-only: {row:?}");
    assert_eq!(cached_row(&f, &a, "claude-advisor-test")[3], 0);
    // $4 per million input tokens, doubled, and a tenth more: $8.80 per request.
    let cost = table.overall.iter().find(|m| m.model == opus).unwrap().cost;
    assert_eq!(
        (cost.pico_usd, cost.unpriced_tokens),
        (2 * 8_800_000_000_000, 0)
    );
}

/// R20: each request is priced once (a fork's copy too), the sections' costs add up to the
/// overall cost, and a request without a price is counted as not priced, until `[prices]`
/// gives one. Codex requests are priced at codex's prices, a long-context one at its own.
#[test]
fn costs_add_up_and_unpriced_requests_are_counted() {
    let mut f = Fixture::new();
    f.claude("max");
    let work = f.codex("work");
    let msg = |session: &str, id: &str, model: &str, usage: Value| {
        cl::assistant_usage(session, id, model, usage, &cl::ts(1))
    };
    let m1 = msg(S_A, "msg_1", "claude-opus-4-6", cl::usage(1000, 100, 0, 0));
    f.write(
        &format!("{S_A}.jsonl"),
        &[
            m1.clone(),
            msg(
                S_A,
                "msg_2",
                "claude-haiku-4-5-20251001",
                cl::usage_split(500, 50, 200, 300, 1000),
            ),
            msg(S_A, "msg_3", "claude-test", cl::usage(10, 10, 0, 0)),
        ]
        .concat(),
    );
    f.write(
        &format!("{S_B}.jsonl"),
        &[
            cl::forked(&m1, S_A),
            msg(S_B, "msg_4", "claude-opus-4-6", cl::usage(2000, 0, 0, 0)),
        ]
        .concat(),
    );
    f.attribute(S_A, "claude:default");
    f.attribute(S_B, "claude:max");
    let first = [100_000, 20_000, 1_000, 500];
    cx::write_rollout(
        &work,
        R1,
        &[
            cx::model_turn("gpt-5.4", &cx::ts(1)),
            cx::tokens(first, first, &cx::ts(2)),
            // Input 300K with cached: more than 272K, the long-context price.
            cx::tokens(
                [400_000, 220_000, 2_000, 500],
                [300_000, 200_000, 1_000, 0],
                &cx::ts(3),
            ),
        ]
        .concat(),
    );
    let table = f.all();
    let cost = |model: &str| {
        table
            .overall
            .iter()
            .find(|m| m.model == model)
            .unwrap()
            .cost
    };
    // Input 3,000 at $5 and output 100 at $25 per million; the fork's copy once.
    assert_eq!(
        cost("claude-opus-4-6").pico_usd,
        3_000 * 5_000_000 + 100 * 25_000_000
    );
    // Input, output, 5-minute and 1-hour cache write, cache read.
    assert_eq!(
        cost("claude-haiku-4-5-20251001").pico_usd,
        500 * 1_000_000 + 50 * 5_000_000 + 200 * 1_250_000 + 300 * 2_000_000 + 1000 * 100_000
    );
    assert_eq!(
        cost("gpt-5.4").pico_usd,
        80_000 * 2_500_000
            + 20_000 * 250_000
            + 1_000 * 15_000_000
            + 100_000 * 5_000_000
            + 200_000 * 500_000
            + 1_000 * 22_500_000
    );
    let test = cost("claude-test");
    assert_eq!((test.pico_usd, test.unpriced_tokens), (0, 20));
    let sections: u128 = table.sections.iter().map(|s| s.cost().pico_usd).sum();
    let overall: u128 = table.overall.iter().map(|m| m.cost.pico_usd).sum();
    assert_eq!(sections, overall);
    assert_eq!(stats::unpriced_models(&table.overall), ["claude-test"]);

    f.prices = Prices::from_document(
        &"[prices.\"claude-test\"]\ninput = 1\noutput = 1\ncache_read = 1\n\
          cache_write_5m = 1\ncache_write_1h = 1\n"
            .parse()
            .unwrap(),
    )
    .unwrap();
    let table = f.report().table(Period::All).clone();
    let test = table
        .overall
        .iter()
        .find(|m| m.model == "claude-test")
        .unwrap();
    assert_eq!(
        (test.cost.pico_usd, test.cost.unpriced_tokens),
        (20_000_000, 0)
    );
    assert!(stats::unpriced_models(&table.overall).is_empty());
}

/// R20: the chart's buckets: today by local hour, 7 and 30 days by local day from the period's
/// start, all by local day from the first request; a request without a timestamp is counted but
/// not charted.
#[test]
fn series_buckets_by_local_hour_and_day() {
    let mut f = Fixture::new();
    f.tz = TimeZone::fixed(jiff::tz::offset(8));
    let at = [
        Some("2026-09-23T17:30:00Z"),
        Some("2026-09-23T15:30:00Z"),
        Some("2026-09-10T00:00:00Z"),
        Some("2026-07-01T00:00:00Z"),
        None,
    ];
    // Output 1, 2, 4, …: a sum tells which messages a bucket holds.
    let text: String = at
        .iter()
        .enumerate()
        .map(|(i, ts)| {
            let record = cl::assistant_usage(
                S_A,
                &format!("msg_{i}"),
                "claude-test",
                cl::usage(0, 1 << i, 0, 0),
                ts.unwrap_or("x"),
            );
            match ts {
                Some(_) => record,
                None => edited(&record, |v| {
                    v.as_object_mut().unwrap().remove("timestamp");
                }),
            }
        })
        .collect();
    f.write(&format!("{S_A}.jsonl"), &text);
    f.refresh();
    let report = f.report();
    let series = |p: Period| &report.table(p).series;
    let starts =
        |p: Period| -> Vec<String> { series(p).iter().map(|b| b.start.to_string()).collect() };
    let charted = |p: Period| -> u64 { series(p).iter().map(|b| b.tokens.output).sum() };
    let today = starts(Period::Today);
    assert_eq!(today.len(), 24);
    assert_eq!(today[0], "2026-09-23T16:00:00Z");
    assert_eq!(today[23], "2026-09-24T15:00:00Z");
    assert_eq!(series(Period::Today)[1].tokens.output, 0b1);
    let week = starts(Period::Week);
    assert_eq!(week.len(), 7);
    assert_eq!(week[0], "2026-09-17T16:00:00Z");
    assert_eq!(week[6], "2026-09-23T16:00:00Z");
    assert_eq!(series(Period::Week)[5].tokens.output, 0b10);
    assert_eq!(series(Period::Week)[6].tokens.output, 0b1);
    assert_eq!(starts(Period::Month).len(), 30);
    let all = starts(Period::All);
    // 2026-07-01 08:00 local to 2026-09-24: 31 + 31 + 24 days.
    assert_eq!(all.len(), 86);
    assert_eq!(all[0], "2026-06-30T16:00:00Z");
    for p in Period::ALL {
        let total = of(&report.table(p).overall, "claude-test").output;
        let undated = if p == Period::All { 0b10000 } else { 0 };
        assert_eq!(charted(p), total - undated, "{p:?}");
    }
    assert_eq!(charted(Period::All), 0b1111);

    // Nothing with a timestamp: no chart for all.
    let mut f = Fixture::new();
    f.write(
        &format!("{S_A}.jsonl"),
        &edited(&msg_a2(), |v| {
            v.as_object_mut().unwrap().remove("timestamp");
        }),
    );
    f.refresh();
    let report = f.report();
    assert!(report.table(Period::All).series.is_empty());
    assert_eq!(report.table(Period::Week).series.len(), 7);
}

/// R20: today's buckets are the day's real hours: 23 on the day DST starts, 25 on the day it
/// ends.
#[test]
fn today_follows_dst() {
    let mut f = Fixture::new();
    f.tz = TimeZone::posix("EST5EDT,M3.2.0,M11.1.0").unwrap();
    for (now, hours) in [
        ("2026-03-08T18:00:00Z", 23),
        ("2026-11-01T18:00:00Z", 25),
        ("2026-09-24T18:00:00Z", 24),
    ] {
        f.now = now.parse().unwrap();
        let report = f.report();
        assert_eq!(report.table(Period::Today).series.len(), hours, "{now}");
        assert_eq!(report.table(Period::Week).series.len(), 7, "{now}");
    }
}
