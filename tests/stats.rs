//! R20: token statistics — counting requests from claude transcripts and codex rollouts once,
//! across records, refreshes, copies and forks; attribution to accounts; periods; the cache.

mod common;

use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::Duration;

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

/// R20: the same transcript in two stores counts once. Both copies are the same session, so
/// which one counts only shows in the ranking: the earliest timestamp wins, then the path
/// that sorts first.
#[test]
fn a_transcript_in_two_stores_counts_once() {
    let mut f = Fixture::new();
    // `<root>/a-team` sorts before `<root>/home`: the copy wins a tie on the path.
    let team = f.root.join("a-team");
    fs::create_dir_all(team.join("projects")).unwrap();
    f.accounts.push(Account {
        provider: Provider::Claude,
        name: "team".into(),
        home: Home::Path(team.display().to_string()),
    });
    let text = [msg_a1_records().concat(), msg_a2()].concat();
    f.write(&format!("{S_A}.jsonl"), &text);
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
    let mut cache = f.cache.clone();
    cache.files.get_mut(&copy).unwrap().session_id = "copy".into();
    let mut attribution = Attribution::default();
    attribution.add(S_A, "claude:default");
    attribution.add("copy", "claude:team");
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
    assert_eq!(
        (
            section(table, &["claude:default"]).total(),
            section(table, &["claude:team"]).total(),
        ),
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

/// The project sections of `f`'s cache for all time: (directory, total tokens), as listed.
fn project_totals(f: &Fixture, account: Option<&str>) -> Vec<(Option<String>, u64)> {
    let sources = f.sources();
    let counted = stats::requests(&f.cache, &sources, &f.attribution, &f.accounts, &f.prices);
    let table = stats::projects(&counted, Period::All, f.now, &f.tz, account);
    table
        .sections
        .iter()
        .map(|p| (p.dir.clone(), p.total().total()))
        .collect()
}

/// R20: a request counts for the project of the copy that counts, as it counts for its
/// accounts: a claude fork in another store, started in another directory and on a path that
/// sorts first, holds copies of its parent's messages, which count in the parent's directory.
#[test]
fn a_request_counts_for_the_project_of_the_copy_that_counts() {
    let mut f = Fixture::new();
    // `<root>/a-team` sorts before `<root>/home`: the fork is visited first.
    let team = f.root.join("a-team");
    fs::create_dir_all(team.join("projects")).unwrap();
    f.accounts.push(Account {
        provider: Provider::Claude,
        name: "team".into(),
        home: Home::Path(team.display().to_string()),
    });
    let in_dir = |record: &str, cwd: &str| edited(record, |v| v["cwd"] = json!(cwd));
    f.write(
        &format!("{S_A}.jsonl"),
        &msg_a1_records().map(|r| in_dir(&r, "/w/parent")).concat(),
    );
    let fork = [
        msg_a1_records()
            .map(|r| in_dir(&cl::forked(&r, S_A), "/w/fork"))
            .concat(),
        in_dir(
            &cl::assistant_usage(
                S_B,
                "msg_b1",
                "claude-test",
                cl::usage(7, 7, 0, 0),
                &cl::ts(5),
            ),
            "/w/fork",
        ),
    ]
    .concat();
    write_file(&team.join(format!("projects/-w-fork/{S_B}.jsonl")), &fork);
    f.attribute(S_A, "claude:default");
    f.attribute(S_B, "claude:team");
    f.refresh();
    let msg_a1 = toks(3, 300, 100, 40, 0).total();
    assert_eq!(
        project_totals(&f, None),
        [
            (Some("/w/parent".into()), msg_a1),
            (Some("/w/fork".into()), 14)
        ]
    );
    assert_eq!(
        project_totals(&f, Some("claude:team")),
        [(Some("/w/fork".into()), 14)]
    );
}

/// R20: the same for codex: a fork's rollout replays its parent's requests with later
/// timestamps; the parent's copies count, in the parent's directory, though the fork's
/// rollout is visited first.
#[test]
fn a_codex_request_counts_for_the_project_of_the_copy_that_counts() {
    let mut f = Fixture::new();
    let work = f.codex("work");
    let other = f.root.join("a-other");
    fs::create_dir_all(other.join("sessions")).unwrap();
    f.accounts.push(Account {
        provider: Provider::Codex,
        name: "other".into(),
        home: Home::Path(other.display().to_string()),
    });
    cx::write_rollout(&work, R1, &r1_records(cx::ts));
    let replay: String = r1_records(|m| cx::ts(m + 10))
        .lines()
        .skip(1)
        .map(|l| format!("{l}\n"))
        .collect();
    cx::write_rollout(
        &other,
        R2,
        &[
            cx::fork_meta(R2, R1, "/w/fork", &cx::ts(20)),
            replay,
            cx::tokens([520, 330, 55, 13], [120, 80, 10, 1], &cx::ts(22)),
        ]
        .concat(),
    );
    f.refresh();
    // R1: 100 + 10, 150 + 20, 150 + 15 (the compaction estimate is not counted).
    assert_eq!(
        project_totals(&f, None),
        [(Some("/w/proj".into()), 445), (Some("/w/fork".into()), 130)]
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

/// A real `token_count` event of codex 0.160.0 (gpt-6-astra, 2026-10-07): `[input, cached,
/// cache write, output, reasoning]` of the session's total and of the latest request.
const REAL_TOTAL: [u64; 5] = [5_728_399, 5_586_560, 0, 19_533, 4_590];
const REAL_LAST: [u64; 5] = [137_139, 136_704, 0, 529, 90];

/// `usage` with `write` more tokens written to the cache: they are part of the prompt, so
/// `input_tokens` (and `total_tokens`) grow by as many.
fn written(usage: [u64; 5], write: u64) -> [u64; 5] {
    let [input, cached, w, output, reasoning] = usage;
    [input + write, cached, w + write, output, reasoning]
}

/// R20: codex's `cache_write_input_tokens` is part of `input_tokens`, like
/// `cached_input_tokens`: it is counted as cache write and taken out of the input, so the total
/// stays `input_tokens` + `output_tokens`; it is priced at the model's cache-write price. A
/// rollout without it (before codex 0.160) counts as before.
#[test]
fn codex_cache_write_is_part_of_input() {
    let rollout = |id: &str, model: &str, total: Value, last: Value| {
        [
            cx::meta(id, "/w/proj", json!("cli"), 0, &cx::ts(0)),
            cx::model_turn(model, &cx::ts(1)),
            cx::usage_event(total, last, &cx::ts(2)),
        ]
        .concat()
    };
    let mut f = Fixture::new();
    let work = f.codex("work");
    // The real event, and the same request with 2,000 tokens written to the cache.
    cx::write_rollout(
        &work,
        R1,
        &rollout(
            R1,
            "gpt-6-astra",
            cx::usage_with_write(REAL_TOTAL),
            cx::usage_with_write(REAL_LAST),
        ),
    );
    cx::write_rollout(
        &work,
        R2,
        &rollout(
            R2,
            "gpt-5.6-sol",
            cx::usage_with_write(written(REAL_TOTAL, 2_000)),
            cx::usage_with_write(written(REAL_LAST, 2_000)),
        ),
    );
    let table = f.all();
    let row = |model: &str| table.overall.iter().find(|r| r.model == model).unwrap();
    let real = row("gpt-6-astra");
    assert_eq!(real.tokens, toks6(435, 136_704, 0, 0, 529, 90));
    assert_eq!(real.tokens.total(), 137_139 + 529);
    // USD per million tokens: input 10, cached 1, output 50.
    assert_eq!(
        real.cost,
        Cost {
            pico_usd: 435 * 10_000_000 + 136_704 * 1_000_000 + 529 * 50_000_000,
            unpriced_tokens: 0,
        }
    );
    let write = row("gpt-5.6-sol");
    assert_eq!(write.tokens, toks6(435, 136_704, 2_000, 0, 529, 90));
    assert_eq!(write.tokens.total(), 139_139 + 529);
    // Input 4, cached 0.40, cache write 5 (1.25 times input), output 20.
    assert_eq!(
        write.cost,
        Cost {
            pico_usd: 435 * 4_000_000 + 136_704 * 400_000 + 2_000 * 5_000_000 + 529 * 20_000_000,
            unpriced_tokens: 0,
        }
    );

    // Without the field, as codex wrote it before.
    let unrecorded = |u: [u64; 5]| {
        let mut v = cx::usage_with_write(u);
        v.as_object_mut()
            .unwrap()
            .remove("cache_write_input_tokens");
        v
    };
    let mut f = Fixture::new();
    let work = f.codex("work");
    cx::write_rollout(
        &work,
        R1,
        &rollout(R1, "gpt-5.4", unrecorded(REAL_TOTAL), unrecorded(REAL_LAST)),
    );
    let table = f.all();
    assert_eq!(
        table.overall,
        [ModelRow {
            provider: Provider::Codex,
            model: "gpt-5.4".into(),
            tokens: toks6(435, 136_704, 0, 0, 529, 90),
            cost: Cost {
                pico_usd: 435 * 2_500_000 + 136_704 * 250_000 + 529 * 15_000_000,
                unpriced_tokens: 0,
            },
        }]
    );
}

/// R20: requests with the same five cumulative counts but a different cache write (the first
/// requests of two unrelated rollouts, counted once: a known limitation) count as the usage of
/// the copy that counts, whole: input from one and cache write from the other would be more
/// tokens than either request had, and could cross the long-context threshold neither did.
/// Within a rollout, the first usage of a total stays.
#[test]
fn codex_requests_with_one_total_keep_one_usage() {
    // `[input, cached, cache write, output, reasoning]` in the first request of a rollout in
    // store `a` at minute 2 and in store `b` at minute 5.
    let collide = |first: [u64; 5], second: [u64; 5]| {
        let mut f = Fixture::new();
        for (store, id, usage, minute) in [("a", R1, first, 2), ("b", R2, second, 5)] {
            let home = f.codex(store);
            let text = [
                cx::meta(id, "/w/proj", json!("cli"), 0, &cx::ts(0)),
                cx::model_turn("gpt-5.6-sol", &cx::ts(1)),
                cx::usage_event(
                    cx::usage_with_write(usage),
                    cx::usage_with_write(usage),
                    &cx::ts(minute),
                ),
            ]
            .concat();
            cx::write_rollout(&home, id, &text);
        }
        let table = f.all();
        assert_eq!(model_names(&table.overall), ["gpt-5.6-sol"]);
        let row = table.overall[0].clone();
        (row.tokens, row.cost)
    };
    let priced = |pico_usd| Cost {
        pico_usd,
        unpriced_tokens: 0,
    };

    // The earlier copy counts, with its own split.
    let (tokens, _) = collide([100, 40, 60, 10, 0], [100, 40, 0, 10, 0]);
    assert_eq!(tokens, toks6(0, 40, 60, 0, 10, 0));
    assert_eq!(tokens.total(), 110);
    let (tokens, _) = collide([100, 40, 0, 10, 0], [100, 40, 60, 10, 0]);
    assert_eq!(tokens, toks6(60, 40, 0, 0, 10, 0));
    assert_eq!(tokens.total(), 110);

    // 200K input tokens each: neither is a long-context request (input 4, cached 0.40, cache
    // write 5, output 20 USD per million tokens).
    let (tokens, cost) = collide(
        [200_000, 40_000, 0, 1_000, 0],
        [200_000, 40_000, 160_000, 1_000, 0],
    );
    assert_eq!(tokens, toks6(160_000, 40_000, 0, 0, 1_000, 0));
    assert_eq!(
        cost,
        priced(160_000 * 4_000_000 + 40_000 * 400_000 + 1_000 * 20_000_000)
    );
    let (tokens, cost) = collide(
        [200_000, 40_000, 160_000, 1_000, 0],
        [200_000, 40_000, 0, 1_000, 0],
    );
    assert_eq!(tokens, toks6(0, 40_000, 160_000, 0, 1_000, 0));
    assert_eq!(
        cost,
        priced(40_000 * 400_000 + 160_000 * 5_000_000 + 1_000 * 20_000_000)
    );

    // In one rollout, a total seen again (not right after itself) keeps its first usage.
    let mut f = Fixture::new();
    let work = f.codex("work");
    let event = |total: [u64; 5], last: [u64; 5], minute| {
        cx::usage_event(
            cx::usage_with_write(total),
            cx::usage_with_write(last),
            &cx::ts(minute),
        )
    };
    let text = [
        cx::meta(R1, "/w/proj", json!("cli"), 0, &cx::ts(0)),
        cx::model_turn("gpt-5.6-sol", &cx::ts(1)),
        event([100, 40, 60, 10, 0], [100, 40, 60, 10, 0], 2),
        event([150, 80, 60, 15, 0], [50, 40, 0, 5, 0], 3),
        event([100, 40, 0, 10, 0], [100, 40, 0, 10, 0], 4),
    ]
    .concat();
    cx::write_rollout(&work, R1, &text);
    let table = f.all();
    assert_eq!(
        of(&table.overall, "gpt-5.6-sol"),
        toks6(10, 80, 60, 0, 15, 0)
    );
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
    assert_eq!(claude["cwd"], "/w/proj");
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
    // A cache of schema 1 (a single cache write), 2 (codex's cache write not read: its rows
    // would keep it in the input), 3 (no project directory: a file read on from its offset
    // would never get one) or 4 (never released: codex's project by an earlier rule) is
    // rebuilt.
    let old = f.root.join("old-stats.json");
    for version in [1, 2, 3, 4] {
        let mut older = v.clone();
        older["schema_version"] = json!(version);
        fs::write(&old, older.to_string()).unwrap();
        let loaded = Cache::load(&old);
        assert!(
            loaded.files.is_empty(),
            "a cache of schema {version} is not rebuilt"
        );
        assert_eq!(loaded.schema_version, SCHEMA_VERSION);
    }
    assert_eq!(SCHEMA_VERSION, 5);

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

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// R20, R8 (review #10): a store that exists but cannot be listed is not a store whose
/// transcripts were deleted. Their counts stay in the cache and in the report as last read,
/// the refresh names the store, the cache is not rewritten, and nothing is read again once the
/// store can be listed.
#[test]
fn a_store_that_cannot_be_read_keeps_its_counts_and_is_reported() {
    let mut f = Fixture::new();
    f.write(&format!("{S_A}.jsonl"), &msg_a1_records().concat());
    f.write(
        &format!("{S_A}/subagents/agent-1.jsonl"),
        &cl::assistant_usage(
            S_A,
            "msg_sub",
            "claude-test",
            cl::usage(7, 7, 0, 0),
            &cl::ts(5),
        ),
    );
    let counted = toks(10, 300, 100, 47, 0);
    assert_eq!(of(&f.all().overall, "claude-test"), counted);
    let saved = f.root.join("remuda/state/stats.json");
    f.cache.save(&saved).unwrap();
    let before = f.cache.clone();
    let store = f.projects();

    chmod(&store, 0o000);
    let s = f.refresh();
    chmod(&store, 0o755);
    assert_eq!(f.cache, before, "nothing is known about the store");
    assert_eq!(
        (s.files, s.removed, s.reused, s.cold, s.kept()),
        (2, 0, 0, 0, 2)
    );
    assert_eq!(s.unreadable.len(), 1, "{:?}", s.unreadable);
    assert_eq!(
        (s.unreadable[0].path.as_path(), s.unreadable[0].kept),
        (store.as_path(), 2)
    );
    assert!(s.incomplete().is_some_and(|said| {
        said.starts_with(&format!("incomplete: cannot read {}: ", store.display()))
    }));
    let report = f.report();
    assert_eq!(
        of(&report.table(Period::All).overall, "claude-test"),
        counted
    );
    assert_eq!(report.files, 2);
    assert!(
        !f.cache.save_if_changed(&saved, &s).unwrap(),
        "what was kept is no change"
    );

    let s = f.refresh();
    assert_eq!(
        (s.reused, s.cold, s.incremental, s.bytes_read),
        (2, 0, 0, 0),
        "readable again: nothing is read again"
    );
    assert!(s.unreadable.is_empty());
    assert_eq!(f.cache, before);
}

/// R20, R8 (review #10): only what is below the directory that cannot be listed is kept: a
/// transcript deleted beside it stops counting, and so do those of a directory that is gone.
#[test]
fn a_session_directory_that_cannot_be_read_keeps_its_counts_alone() {
    let mut f = Fixture::new();
    let top = f.write(&format!("{S_A}.jsonl"), &msg_a1_records().concat());
    let sub = f.write(
        &format!("{S_A}/subagents/agent-1.jsonl"),
        &cl::assistant_usage(
            S_A,
            "msg_sub",
            "claude-test",
            cl::usage(7, 7, 0, 0),
            &cl::ts(5),
        ),
    );
    f.all();
    let locked = f.projects().join("-w-proj").join(S_A);

    chmod(&locked, 0o000);
    fs::remove_file(&top).unwrap();
    let s = f.refresh();
    chmod(&locked, 0o755);
    assert_eq!((s.files, s.removed, s.kept()), (1, 1, 1));
    assert_eq!(s.unreadable.len(), 1, "{:?}", s.unreadable);
    assert_eq!(s.unreadable[0].path, locked);
    assert!(f.cache.files.contains_key(&sub) && !f.cache.files.contains_key(&top));
    let table = f.report().table(Period::All).clone();
    assert_eq!(of(&table.overall, "claude-test"), toks(7, 0, 0, 7, 0));

    // The directory is gone: its transcripts no longer count.
    fs::remove_dir_all(&locked).unwrap();
    let s = f.refresh();
    assert_eq!((s.files, s.removed), (0, 1));
    assert!(s.unreadable.is_empty());
    assert_eq!(f.report().files, 0);
}

/// R20, R8 (review #10): directories that can be listed but not searched, a claude project and
/// a codex day directory, give names and nothing else. Their transcripts are not taken for
/// deleted: the counts stay, each directory is reported, and nothing is read again once they
/// can be searched.
#[test]
fn directories_that_cannot_be_searched_keep_their_counts_and_are_reported() {
    let mut f = Fixture::new();
    let work = f.codex("work");
    let top = f.write(&format!("{S_A}.jsonl"), &msg_a1_records().concat());
    f.write(
        &format!("{S_A}/subagents/agent-1.jsonl"),
        &cl::assistant_usage(
            S_A,
            "msg_sub",
            "claude-test",
            cl::usage(7, 7, 0, 0),
            &cl::ts(5),
        ),
    );
    let rollout = cx::write_rollout(&work, R1, &r1_records(cx::ts));
    f.all();
    let before = f.cache.clone();
    let (project, day) = (top.parent().unwrap(), rollout.parent().unwrap());

    chmod(project, 0o444);
    chmod(day, 0o444);
    let s = f.refresh();
    chmod(project, 0o755);
    chmod(day, 0o755);
    assert_eq!(f.cache, before);
    assert_eq!((s.files, s.removed, s.cold, s.kept()), (3, 0, 0, 3));
    // The project once: not again for the session directory below it, which fails too.
    let unreadable: Vec<(&Path, usize)> = s
        .unreadable
        .iter()
        .map(|u| (u.path.as_path(), u.kept))
        .collect();
    assert_eq!(unreadable, [(project, 2), (day, 1)]);

    let s = f.refresh();
    assert_eq!(
        (s.reused, s.cold, s.incremental, s.bytes_read),
        (3, 0, 0, 0)
    );
    assert!(s.unreadable.is_empty());
}

/// R20, R8: a file rewritten in place between the listing and the read (the same inode,
/// larger) with an mtime earlier than the cached one is read whole: its counts are replaced,
/// not added to.
#[test]
fn a_file_rewritten_with_an_earlier_mtime_before_it_is_opened_is_read_whole() {
    let set_mtime = |path: &Path, to| {
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(to).unwrap();
    };
    let mut f = Fixture::new();
    let path = f.write(&format!("{S_A}.jsonl"), &msg_a2());
    assert_eq!(of(&f.all().overall, "claude-test"), toks(1, 200, 0, 5, 0));
    let before = fs::metadata(&path).unwrap().modified().unwrap();
    append(&path, "\n");
    set_mtime(&path, before + Duration::from_secs(60));

    let sources = f.sources();
    // The first report comes after the listing and before anything is read.
    let s = stats::refresh(&mut f.cache, &sources, |done, _| {
        if done == 0 {
            fs::write(&path, msg_a1_records().concat()).unwrap();
            set_mtime(&path, before - Duration::from_secs(60));
        }
    });
    assert_eq!((s.incremental, s.cold), (1, 0), "listed as grown");
    let table = f.report().table(Period::All).clone();
    assert_eq!(of(&table.overall, "claude-test"), toks(3, 300, 100, 40, 0));
    let s = f.refresh();
    assert_eq!((s.reused, s.bytes_read), (1, 0), "cached as it was opened");
}

/// R20: a transcript the cache holds that is listed as changed and can no longer be read is
/// dropped, and that is a change to save: the saved cache does not keep it, whether or not the
/// refresh also kept what is below a directory it could not read (R8).
#[test]
fn a_cached_file_that_can_no_longer_be_read_leaves_the_saved_cache() {
    for with_an_unreadable_directory in [false, true] {
        let mut f = Fixture::new();
        f.write(&format!("{S_A}.jsonl"), &msg_a1_records().concat());
        f.write(
            &format!("{S_A}/subagents/agent-1.jsonl"),
            &cl::assistant_usage(
                S_A,
                "msg_sub",
                "claude-test",
                cl::usage(7, 7, 0, 0),
                &cl::ts(5),
            ),
        );
        let b = f.write(
            &format!("{S_B}.jsonl"),
            &cl::assistant_usage(
                S_B,
                "msg_b1",
                "claude-test",
                cl::usage(7, 7, 0, 0),
                &cl::ts(5),
            ),
        );
        f.all();
        let saved = f.root.join("remuda/state/stats.json");
        f.cache.save(&saved).unwrap();
        assert_eq!(Cache::load(&saved).files.len(), 3);

        // Grown, then no longer readable: listed, and not opened.
        append(
            &b,
            &cl::assistant_usage(
                S_B,
                "msg_b2",
                "claude-test",
                cl::usage(1, 1, 0, 0),
                &cl::ts(6),
            ),
        );
        let session = f.projects().join("-w-proj").join(S_A);
        chmod(&b, 0o000);
        if with_an_unreadable_directory {
            chmod(&session, 0o000);
        }
        let s = f.refresh();
        chmod(&b, 0o644);
        chmod(&session, 0o755);
        let kept = usize::from(with_an_unreadable_directory);
        assert_eq!(
            (s.files, s.incremental, s.removed, s.kept(), s.bytes_read),
            (2, 1, 1, kept, 0),
            "{s:?}"
        );
        assert_eq!(s.reused, 2 - kept);
        assert!(!f.cache.files.contains_key(&b));
        assert!(
            f.cache.save_if_changed(&saved, &s).unwrap(),
            "dropping a cached file is a change"
        );
        assert_eq!(Cache::load(&saved), f.cache);
    }
}

/// R20, R8: a transcript that is a symlink to somewhere out of reach is a single transcript
/// that cannot be read. It stops counting, like one that cannot be opened; it is no directory
/// whose counts stay, and the refresh is not incomplete for it.
#[test]
fn a_symlinked_transcript_out_of_reach_stops_counting() {
    let mut f = Fixture::new();
    f.write(&format!("{S_A}.jsonl"), &msg_a1_records().concat());
    let away = f.root.join("away");
    let target = write_file(
        &away.join("b.jsonl"),
        &cl::assistant_usage(
            S_B,
            "msg_b1",
            "claude-test",
            cl::usage(7, 7, 0, 0),
            &cl::ts(5),
        ),
    );
    let linked = f.projects().join(format!("-w-proj/{S_B}.jsonl"));
    symlink(&target, &linked).unwrap();
    let both = toks(10, 300, 100, 47, 0);
    assert_eq!(of(&f.all().overall, "claude-test"), both);
    assert!(f.cache.files.contains_key(&linked));

    chmod(&away, 0o000);
    let s = f.refresh();
    chmod(&away, 0o755);
    assert_eq!(
        (s.files, s.removed, s.reused, s.kept()),
        (1, 1, 1, 0),
        "{:?}",
        s.unreadable
    );
    assert!(s.unreadable.is_empty(), "{:?}", s.unreadable);
    assert_eq!(s.incomplete(), None);
    assert!(!f.cache.files.contains_key(&linked));
    let table = f.report().table(Period::All).clone();
    assert_eq!(of(&table.overall, "claude-test"), toks(3, 300, 100, 40, 0));

    // In reach again: it counts again.
    assert_eq!(of(&f.all().overall, "claude-test"), both);
}

/// R20, R8 (review #10): a codex home that cannot be searched: neither its `sessions` nor its
/// `archived_sessions` can be resolved, which is not the same as their being gone. Their
/// rollouts stay in the cache and in the counts as last read, both directories are reported
/// as the home gives them, and nothing is read again once they resolve. A home that is gone
/// has no sources.
#[test]
fn sources_that_cannot_be_resolved_keep_their_counts_and_are_reported() {
    let mut f = Fixture::new();
    let work = f.codex("work");
    f.write(&format!("{S_A}.jsonl"), &msg_a1_records().concat());
    cx::write_rollout(&work, R1, &r1_records(cx::ts));
    cx::write_archived_rollout(
        &work,
        R2,
        &[
            cx::meta(R2, "/w/proj", json!("cli"), 0, &cx::ts(0)),
            cx::model_turn("gpt-test-a", &cx::ts(1)),
            cx::tokens([7, 0, 3, 0], [7, 0, 3, 0], &cx::ts(2)),
        ]
        .concat(),
    );
    let refresh = |f: &mut Fixture| {
        let (sources, given) = stats::resolve(&f.accounts, &f.env);
        let s = stats::refresh_with(&mut f.cache, &sources, &given, |_, _| {});
        let unresolved: Vec<_> = given.into_iter().filter(|g| g.real.is_err()).collect();
        (sources, unresolved, s)
    };
    let (sources, unresolved, s) = refresh(&mut f);
    assert_eq!((sources.len(), unresolved.len(), s.files), (3, 0, 3));
    let before = f.cache.clone();
    let overall = f.report().table(Period::All).overall.clone();
    let saved = f.root.join("remuda/state/stats.json");
    f.cache.save(&saved).unwrap();

    chmod(&work, 0o000);
    let (sources, unresolved, s) = refresh(&mut f);
    let listed = f.sources();
    let report = f.report();
    chmod(&work, 0o755);
    assert_eq!(
        sources, listed,
        "the list of sources is the same either way"
    );
    assert_eq!(sources.len(), 1);
    let given: Vec<(Provider, &Path, &str)> = unresolved
        .iter()
        .map(|u| (u.provider, u.path.as_path(), u.account.as_str()))
        .collect();
    assert_eq!(
        given,
        [
            (
                Provider::Codex,
                work.join("sessions").as_path(),
                "codex:work"
            ),
            (
                Provider::Codex,
                work.join("archived_sessions").as_path(),
                "codex:work"
            ),
        ]
    );
    assert_eq!(f.cache, before);
    assert_eq!(
        (s.files, s.removed, s.reused, s.cold, s.kept()),
        (3, 0, 1, 0, 2)
    );
    // Each with the rollouts last read below it.
    let kept: Vec<(&Path, usize, bool)> = s
        .unreadable
        .iter()
        .map(|u| (u.path.as_path(), u.kept, u.unresolved))
        .collect();
    assert_eq!(
        kept,
        [
            (work.join("sessions").as_path(), 1, true),
            (work.join("archived_sessions").as_path(), 1, true),
        ]
    );
    assert_eq!(report.table(Period::All).overall, overall);
    assert_eq!(report.files, 3);
    // Whose home the rollouts are in is not known meanwhile: counted, as unattributed.
    let table = report.table(Period::All);
    assert!(section(table, &["codex:work"]).models.is_empty());
    assert!(!of(&section(table, &[]).models, "gpt-test-a").is_zero());
    assert!(
        !f.cache.save_if_changed(&saved, &s).unwrap(),
        "what was kept is no change"
    );

    let (_, unresolved, s) = refresh(&mut f);
    assert!(unresolved.is_empty() && s.unreadable.is_empty());
    assert_eq!(
        (s.reused, s.cold, s.incremental, s.bytes_read),
        (3, 0, 0, 0),
        "resolved again: nothing is read again"
    );

    // Only `archived_sessions` cannot be resolved (a link through a directory that cannot be
    // searched): the rollouts of `sessions`, which is listed, are told from the archived one.
    let away = f.root.join("away");
    fs::create_dir_all(&away).unwrap();
    fs::rename(work.join("archived_sessions"), away.join("archived")).unwrap();
    symlink(away.join("archived"), work.join("archived_sessions")).unwrap();
    let (sources, _, s) = refresh(&mut f);
    assert_eq!((sources.len(), s.files, s.removed, s.cold), (3, 3, 1, 1));
    chmod(&away, 0o000);
    fs::remove_dir_all(work.join("sessions/2026")).unwrap();
    let (sources, unresolved, s) = refresh(&mut f);
    chmod(&away, 0o755);
    assert_eq!((sources.len(), unresolved.len()), (2, 1));
    assert_eq!((s.files, s.removed, s.reused, s.kept()), (2, 1, 1, 1));
    assert_eq!(s.unreadable[0].path, work.join("archived_sessions"));

    // The home is gone: so are its sources and what was counted from them.
    fs::remove_dir_all(&work).unwrap();
    let (sources, unresolved, s) = refresh(&mut f);
    assert_eq!((sources.len(), unresolved.len()), (1, 0));
    assert_eq!((s.files, s.removed), (1, 1));
    assert!(s.unreadable.is_empty());
}

/// R20, R8 (review #10): a source that cannot be resolved keeps what was counted from the
/// source it last resolved to, and from that alone. Here a codex home's `archived_sessions`
/// cannot be resolved while its `sessions` is gone: the rollouts of `sessions` stop counting,
/// the archived one stays, and the cache that is saved says the same. Likewise for a claude
/// store whose account left the registry.
#[test]
fn a_source_that_cannot_be_resolved_does_not_keep_the_counts_of_another() {
    let mut f = Fixture::new();
    let work = f.codex("work");
    let other = f.claude("other");
    f.write(&format!("{S_A}.jsonl"), &msg_a1_records().concat());
    write_file(
        &other.join(format!("projects/-w/{S_B}.jsonl")),
        &cl::assistant_usage(
            S_B,
            "msg_b1",
            "claude-test",
            cl::usage(7, 7, 0, 0),
            &cl::ts(5),
        ),
    );
    cx::write_rollout(&work, R1, &r1_records(cx::ts));
    let away = f.root.join("away");
    let archived = cx::write_archived_rollout(
        &away,
        R2,
        &[
            cx::meta(R2, "/w/proj", json!("cli"), 0, &cx::ts(0)),
            cx::model_turn("gpt-test-a", &cx::ts(1)),
            cx::tokens([7, 0, 3, 0], [7, 0, 3, 0], &cx::ts(2)),
        ]
        .concat(),
    );
    symlink(
        away.join("archived_sessions"),
        work.join("archived_sessions"),
    )
    .unwrap();
    let refresh = |f: &mut Fixture| {
        let (sources, given) = stats::resolve(&f.accounts, &f.env);
        stats::refresh_with(&mut f.cache, &sources, &given, |_, _| {})
    };
    let s = refresh(&mut f);
    assert_eq!((s.files, s.cold), (4, 4));
    assert!(s.remembered, "where each source is was not known before");
    assert_eq!(f.cache.sources.len(), 4, "{:?}", f.cache.sources);
    let saved = f.root.join("remuda/state/stats.json");
    f.cache.save(&saved).unwrap();
    let s = refresh(&mut f);
    assert!(!s.remembered && !f.cache.save_if_changed(&saved, &s).unwrap());

    // `sessions` is gone, `archived_sessions` cannot be resolved, and `other` left the
    // registry with its store still there.
    fs::remove_dir_all(work.join("sessions")).unwrap();
    f.accounts.retain(|a| a.name != "other");
    chmod(&away, 0o000);
    let s = refresh(&mut f);
    chmod(&away, 0o755);
    assert_eq!(
        (s.files, s.removed, s.reused, s.kept()),
        (2, 2, 1, 1),
        "{:?}",
        s.unreadable
    );
    let left: Vec<&PathBuf> = f.cache.files.keys().collect();
    assert_eq!(
        left,
        [
            &archived,
            &f.projects().join(format!("-w-proj/{S_A}.jsonl"))
        ]
    );
    assert_eq!(s.unreadable.len(), 1);
    assert_eq!(
        (s.unreadable[0].path.as_path(), s.unreadable[0].kept),
        (work.join("archived_sessions").as_path(), 1)
    );
    assert!(s.remembered, "two sources are no longer tracked");
    assert!(f.cache.save_if_changed(&saved, &s).unwrap());
    assert_eq!(Cache::load(&saved), f.cache);

    let s = refresh(&mut f);
    assert_eq!((s.files, s.reused, s.removed, s.bytes_read), (2, 2, 0, 0));
    assert!(!s.remembered);
}

/// R20, R8: the cache remembers where each source was last found, without another schema: a
/// cache written before that was kept is read as it was, and remembers nothing.
#[test]
fn the_cache_remembers_its_sources_and_reads_one_written_before() {
    let mut f = Fixture::new();
    f.write(&format!("{S_A}.jsonl"), &msg_a1_records().concat());
    let (sources, given) = stats::resolve(&f.accounts, &f.env);
    let s = stats::refresh_with(&mut f.cache, &sources, &given, |_, _| {});
    assert!(s.remembered);
    // By the directory as the home gives it: `$HOME/.claude/projects`, here its real path too.
    let remembered: Vec<(&Path, &Path)> = f
        .cache
        .sources
        .iter()
        .map(|(given, real)| (given.as_path(), real.as_path()))
        .collect();
    assert_eq!(
        remembered,
        [(f.projects().as_path(), f.projects().as_path())]
    );
    let saved = f.root.join("remuda/state/stats.json");
    f.cache.save(&saved).unwrap();
    assert_eq!(Cache::load(&saved), f.cache);

    let mut v: Value = serde_json::from_slice(&fs::read(&saved).unwrap()).unwrap();
    assert_eq!(v["schema_version"], SCHEMA_VERSION);
    assert_eq!(v["sources"].as_object().map(|m| m.len()), Some(1));
    v.as_object_mut().unwrap().remove("sources");
    fs::write(&saved, v.to_string()).unwrap();
    let mut before = Cache::load(&saved);
    assert_eq!(before.files, f.cache.files);
    assert!(before.sources.is_empty());
    // Refreshed, it remembers, and that alone is a change to save.
    let s = stats::refresh_with(&mut before, &sources, &given, |_, _| {});
    assert_eq!((s.reused, s.files, s.bytes_read), (1, 1, 0));
    assert!(s.remembered && before.save_if_changed(&saved, &s).unwrap());
    assert_eq!(Cache::load(&saved), f.cache);
}

/// R20, R8 (review #10): what the cache remembers of a source goes by the directory its home
/// gives, not by the account's name. A codex account registered again under the same name
/// with another home that cannot be searched does not take over what was counted from the old
/// home, neither from its `sessions` nor from its `archived_sessions`; registered again under
/// another name with the same home, it keeps it. Either way the cache saved and read back
/// says the same on the next refresh.
#[test]
fn what_is_kept_for_a_source_goes_by_its_home_not_by_the_accounts_name() {
    let f = Fixture::new();
    let (old, new) = (f.root.join("c/old"), f.root.join("c/new"));
    cx::write_rollout(&old, R1, &r1_records(cx::ts));
    cx::write_archived_rollout(
        &old,
        R2,
        &[
            cx::meta(R2, "/w/proj", json!("cli"), 0, &cx::ts(0)),
            cx::model_turn("gpt-test-a", &cx::ts(1)),
            cx::tokens([7, 0, 3, 0], [7, 0, 3, 0], &cx::ts(2)),
        ]
        .concat(),
    );
    cx::write_rollout(&new, R3, &r1_records(cx::ts));
    let codex = |name: &str, home: &Path| Account {
        provider: Provider::Codex,
        name: name.into(),
        home: Home::Path(home.display().to_string()),
    };
    let saved = f.root.join("remuda/state/stats.json");
    // One process each: the cache is loaded, refreshed for `accounts`, and saved if changed.
    let run = |accounts: &[Account]| {
        let mut cache = Cache::load(&saved);
        let (sources, given) = stats::resolve(accounts, &f.env);
        let s = stats::refresh_with(&mut cache, &sources, &given, |_, _| {});
        cache.save_if_changed(&saved, &s).unwrap();
        assert_eq!(
            Cache::load(&saved),
            cache,
            "what is saved is what was refreshed"
        );
        (s.files, s.removed, s.kept(), s.unreadable.len())
    };
    assert_eq!(run(&[codex("work", &old)]), (2, 0, 0, 0));
    let before = Cache::load(&saved);
    assert_eq!(before.sources.len(), 2, "{:?}", before.sources);

    // The same home under another name, out of reach before the next refresh: the same two
    // sources, kept.
    chmod(&old, 0o000);
    let renamed = [
        run(&[codex("renamed", &old)]),
        run(&[codex("renamed", &old)]),
    ];
    chmod(&old, 0o755);
    assert_eq!(renamed, [(2, 0, 2, 2); 2]);
    assert_eq!(Cache::load(&saved), before);

    // The same name with another home, out of reach before it was ever read: nothing of the
    // old home is its to keep. Both directories of the new home are reported: whether it has
    // an `archived_sessions` cannot be told either.
    chmod(&new, 0o000);
    let moved = [run(&[codex("work", &new)]), run(&[codex("work", &new)])];
    chmod(&new, 0o755);
    assert_eq!(moved, [(0, 2, 0, 2), (0, 0, 0, 2)]);
    let after = Cache::load(&saved);
    assert!(
        after.files.is_empty() && after.sources.is_empty(),
        "{:?}",
        after.sources
    );
    assert_eq!(run(&[codex("work", &new)]), (1, 0, 0, 0));
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
