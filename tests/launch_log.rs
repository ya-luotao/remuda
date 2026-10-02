//! R6, R9: `--session-id` pre-allocation and the launch log.

mod common;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use common::Sandbox;
use predicates::prelude::*;
use serde_json::{Value, json};

const MAX_HOME: &str = "/nonexistent/profiles/max/";

fn sandbox_with_max() -> Sandbox {
    let sb = Sandbox::new();
    sb.write_config(&format!(
        "[[account]]\nprovider = \"claude\"\nname = \"max\"\nhome = \"{MAX_HOME}\"\n"
    ));
    sb
}

fn only_launch(sb: &Sandbox) -> Value {
    let mut all = sb.launches();
    assert_eq!(all.len(), 1, "expected exactly one launch record: {all:?}");
    all.remove(0)
}

fn work_cwd(sb: &Sandbox) -> String {
    sb.work()
        .canonicalize()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string()
}

#[test]
fn new_session_gets_preallocated_id_logged_before_exec() {
    let sb = sandbox_with_max();
    sb.remuda().args(["run", "max"]).assert().success();

    let inv = sb.only_invocation();
    assert_eq!(inv.args.len(), 2, "{:?}", inv.args);
    assert_eq!(inv.args[0], "--session-id");
    let id = &inv.args[1];
    let parsed = uuid::Uuid::parse_str(id).expect("injected id is a UUID");
    assert_eq!(parsed.get_version_num(), 4);
    assert_eq!(parsed.hyphenated().to_string(), *id);

    let rec = only_launch(&sb);
    assert_eq!(rec["session_id"], json!(id));
    assert_eq!(rec["injected"], json!(true));
    assert_eq!(rec["account"], json!("claude:max"));
    assert_eq!(rec["home"], json!(MAX_HOME));
    assert_eq!(rec["args"], json!([]));
    assert_eq!(rec["cwd"], json!(work_cwd(&sb)));
    let ts = rec["ts"].as_str().expect("ts is a string");
    assert!(ts.ends_with('Z'), "UTC: {ts}");
    ts.parse::<jiff::Timestamp>().expect("ts is RFC 3339");
}

#[test]
fn new_session_with_prompt_keeps_user_args_and_appends_id() {
    let sb = sandbox_with_max();
    sb.remuda()
        .args(["run", "max", "-p", "hi there"])
        .assert()
        .success();
    let inv = sb.only_invocation();
    assert_eq!(inv.args[..3], ["-p", "hi there", "--session-id"]);
    let rec = only_launch(&sb);
    assert_eq!(rec["args"], json!(["-p", "hi there"]));
    assert_eq!(rec["session_id"], json!(inv.args[3]));
}

/// R5, R6: the logged `args` are the user's as the agent got them, a `--` right after the
/// account included; the injected ID goes before it.
#[test]
fn a_double_dash_after_the_account_is_logged_as_the_agent_got_it() {
    let sb = sandbox_with_max();
    sb.remuda()
        .args(["run", "max", "--", "-1 is not valid"])
        .assert()
        .success();
    let inv = sb.only_invocation();
    assert_eq!(inv.args[0], "--session-id");
    assert_eq!(inv.args[2..], ["--", "-1 is not valid"]);
    let rec = only_launch(&sb);
    assert_eq!(rec["args"], json!(["--", "-1 is not valid"]));
    assert_eq!(rec["session_id"], json!(inv.args[1]));
    assert_eq!(rec["injected"], json!(true));

    // A resume option after the terminator is still read as one (R6: when in doubt, nothing
    // is injected); what is logged is what the agent got.
    let sb = sandbox_with_max();
    sb.remuda()
        .args(["run", "max", "--", "--resume", "abc"])
        .assert()
        .success();
    assert_eq!(sb.only_invocation().args, ["--", "--resume", "abc"]);
    let rec = only_launch(&sb);
    assert_eq!(rec["args"], json!(["--", "--resume", "abc"]));
    assert_eq!(rec["injected"], json!(false));
}

#[test]
fn each_new_session_gets_a_fresh_id() {
    let sb = sandbox_with_max();
    sb.remuda().args(["run", "max"]).assert().success();
    sb.remuda().args(["run", "max"]).assert().success();
    let ids: Vec<String> = sb
        .invocations()
        .into_iter()
        .map(|i| i.args[1].clone())
        .collect();
    assert_ne!(ids[0], ids[1]);
    let logged: Vec<Value> = sb
        .launches()
        .into_iter()
        .map(|r| r["session_id"].clone())
        .collect();
    assert_eq!(logged, [json!(ids[0]), json!(ids[1])]);
}

#[test]
fn subcommand_is_passed_through_and_logged_without_id() {
    let sb = sandbox_with_max();
    sb.remuda()
        .args(["run", "max", "agents", "--json"])
        .assert()
        .success();
    assert_eq!(sb.only_invocation().args, ["agents", "--json"]);
    let rec = only_launch(&sb);
    assert_eq!(rec["session_id"], Value::Null);
    assert_eq!(rec["injected"], json!(false));
    assert_eq!(rec["args"], json!(["agents", "--json"]));
}

#[test]
fn resume_is_not_injected_and_logs_the_id() {
    let sb = sandbox_with_max();
    sb.remuda()
        .args(["run", "max", "--resume", "abc"])
        .assert()
        .success();
    assert_eq!(sb.only_invocation().args, ["--resume", "abc"]);
    let rec = only_launch(&sb);
    assert_eq!(rec["session_id"], json!("abc"));
    assert_eq!(rec["injected"], json!(false));
}

#[test]
fn fork_session_gets_a_preallocated_id_and_logs_what_it_forked() {
    let sb = sandbox_with_max();
    sb.remuda()
        .args(["run", "max", "--resume", "abc", "--fork-session"])
        .assert()
        .success();
    let inv = sb.only_invocation();
    assert_eq!(
        inv.args[..4],
        ["--resume", "abc", "--fork-session", "--session-id"]
    );
    assert_eq!(inv.args.len(), 5, "{:?}", inv.args);
    let id = &inv.args[4];
    uuid::Uuid::parse_str(id).expect("injected id is a UUID");
    let rec = only_launch(&sb);
    assert_eq!(rec["session_id"], json!(id));
    assert_eq!(rec["fork_of"], json!("abc"));
    assert_eq!(rec["injected"], json!(true));
    assert_eq!(rec["args"], json!(["--resume", "abc", "--fork-session"]));
}

#[test]
fn fork_with_the_users_session_id_is_not_injected() {
    let sb = sandbox_with_max();
    let args = ["--resume", "abc", "--fork-session", "--session-id", "Y"];
    sb.remuda()
        .args(["run", "max"])
        .args(args)
        .assert()
        .success();
    assert_eq!(sb.only_invocation().args, args);
    let rec = only_launch(&sb);
    assert_eq!(rec["session_id"], json!("Y"));
    assert_eq!(rec["fork_of"], json!("abc"));
    assert_eq!(rec["injected"], json!(false));
}

#[test]
fn records_without_a_fork_have_no_fork_of() {
    let sb = sandbox_with_max();
    sb.remuda()
        .args(["run", "max", "--resume", "abc"])
        .assert()
        .success();
    assert!(only_launch(&sb).get("fork_of").is_none());
}

#[test]
fn continue_is_logged_with_unknown_id() {
    let sb = sandbox_with_max();
    sb.remuda().args(["run", "max", "-c"]).assert().success();
    assert_eq!(sb.only_invocation().args, ["-c"]);
    assert_eq!(only_launch(&sb)["session_id"], Value::Null);
}

#[test]
fn default_account_logs_home_default() {
    let sb = Sandbox::new();
    sb.remuda()
        .env("CLAUDE_CONFIG_DIR", "/inherited")
        .args(["run", "default"])
        .assert()
        .success();
    let inv = sb.only_invocation();
    assert_eq!(inv.config_dir, None);
    let rec = only_launch(&sb);
    assert_eq!(rec["account"], json!("claude:default"));
    assert_eq!(rec["home"], json!("default"));
    assert_eq!(rec["session_id"], json!(inv.args[1]));
}

#[test]
fn log_failure_warns_but_still_launches() {
    let sb = sandbox_with_max();
    // `state` is a file, so `state/launches.jsonl` cannot be created.
    fs::write(sb.remuda_home().join("state"), "").unwrap();
    sb.remuda()
        .args(["run", "max", "--resume", "abc"])
        .assert()
        .success()
        .stderr(
            predicate::str::starts_with("remuda: warning: ")
                .and(predicate::str::contains("launch log")),
        );
    assert_eq!(sb.only_invocation().args, ["--resume", "abc"]);
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// R3: the log holds the arguments, prompts among them: it is created readable by the user
/// alone, in a `state/` that is.
#[test]
fn the_launch_log_is_created_private() {
    let sb = sandbox_with_max();
    sb.remuda()
        .args(["run", "max", "-p", "a prompt"])
        .assert()
        .success()
        .stderr("");
    assert_eq!(mode(&sb.remuda_home().join("state")), 0o700);
    assert_eq!(mode(&sb.launch_log()), 0o600);
    assert_eq!(only_launch(&sb)["args"], json!(["-p", "a prompt"]));
}

/// R3: a log and a `state/` from before, readable by others, are tightened by the next launch;
/// the lines already there stay.
#[test]
fn an_existing_launch_log_is_tightened() {
    let sb = sandbox_with_max();
    let state = sb.remuda_home().join("state");
    fs::create_dir(&state).unwrap();
    chmod(&state, 0o755);
    fs::write(sb.launch_log(), "{\"session_id\":\"old\"}\n").unwrap();
    chmod(&sb.launch_log(), 0o644);
    sb.remuda()
        .args(["run", "max", "--resume", "abc"])
        .assert()
        .success()
        .stderr("");
    assert_eq!(mode(&state), 0o700);
    assert_eq!(mode(&sb.launch_log()), 0o600);
    let ids: Vec<Value> = sb
        .launches()
        .into_iter()
        .map(|r| r["session_id"].clone())
        .collect();
    assert_eq!(ids, [json!("old"), json!("abc")]);
}

/// R3: no mode is changed through a symlink. A `state` that is one keeps its target's mode
/// (the log in it is remuda's file all the same, and private); a log that is one keeps its
/// target's. The line is written through either.
#[test]
fn a_linked_state_directory_and_a_linked_log_keep_their_modes() {
    // `state` is a link.
    let sb = sandbox_with_max();
    let outside = sb.root().join("outside");
    fs::create_dir(&outside).unwrap();
    chmod(&outside, 0o755);
    fs::write(outside.join("launches.jsonl"), "{\"session_id\":\"old\"}\n").unwrap();
    chmod(&outside.join("launches.jsonl"), 0o644);
    let state = sb.remuda_home().join("state");
    symlink(&outside, &state).unwrap();
    sb.remuda()
        .args(["run", "max", "--resume", "abc"])
        .assert()
        .success()
        .stderr("");
    assert_eq!(fs::read_link(&state).unwrap(), outside);
    assert_eq!(mode(&outside), 0o755);
    assert_eq!(mode(&outside.join("launches.jsonl")), 0o600);
    assert_eq!(sb.launches().len(), 2);

    // So is a log created through that link.
    fs::remove_file(outside.join("launches.jsonl")).unwrap();
    sb.remuda()
        .args(["run", "max", "--resume", "abc"])
        .assert()
        .success();
    assert_eq!(mode(&outside), 0o755);
    assert_eq!(mode(&outside.join("launches.jsonl")), 0o600);

    // The log is a link, to a file that is the user's alone.
    let sb = sandbox_with_max();
    let target = sb.root().join("elsewhere.jsonl");
    fs::write(&target, "").unwrap();
    chmod(&target, 0o600);
    let state = sb.remuda_home().join("state");
    fs::create_dir(&state).unwrap();
    chmod(&state, 0o755);
    symlink(&target, sb.launch_log()).unwrap();
    sb.remuda()
        .args(["run", "max", "--resume", "abc"])
        .assert()
        .success()
        .stderr("");
    assert_eq!(fs::read_link(sb.launch_log()).unwrap(), target);
    assert_eq!(mode(&target), 0o600);
    assert_eq!(mode(&state), 0o700);
    assert_eq!(sb.launches().len(), 1);
}

/// R3: nothing is appended to a log the group or others can still access. A log that is a
/// symlink keeps its target's mode, so one whose target others can read gets no line: the
/// prompt stays out of it, the launch goes on, and the warning says why.
#[test]
fn a_log_others_can_access_gets_no_line() {
    for wide in [0o644, 0o640, 0o604, 0o620] {
        let sb = sandbox_with_max();
        let target = sb.root().join("elsewhere.jsonl");
        fs::write(&target, "{\"session_id\":\"old\"}\n").unwrap();
        chmod(&target, wide);
        fs::create_dir(sb.remuda_home().join("state")).unwrap();
        symlink(&target, sb.launch_log()).unwrap();
        sb.remuda()
            .args(["run", "max", "-p", "a secret prompt"])
            .assert()
            .success()
            .stderr(
                predicate::str::starts_with("remuda: warning: cannot write launch log ").and(
                    predicate::str::contains(format!(
                        "{} can be accessed by the group or others (mode {wide:04o}) and was \
                         not made private; nothing was appended",
                        sb.launch_log().display()
                    )),
                ),
            );
        assert_eq!(sb.only_invocation().args[..2], ["-p", "a secret prompt"]);
        assert_eq!(fs::read_link(sb.launch_log()).unwrap(), target);
        assert_eq!(mode(&target), wide, "{wide:o}");
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "{\"session_id\":\"old\"}\n",
            "{wide:o}"
        );
    }
}

#[test]
fn no_log_when_claude_cannot_be_found() {
    let sb = sandbox_with_max();
    sb.remuda()
        .env("PATH", "/usr/bin:/bin")
        .args(["run", "max"])
        .assert()
        .code(1);
    assert!(!sb.launch_log().exists());
}

#[test]
fn run_writes_nothing_else_under_remuda_home() {
    let sb = sandbox_with_max();
    let before = sb.read_config();
    sb.remuda().args(["run", "max"]).assert().success();
    assert_eq!(sb.read_config(), before);
    let mut entries: Vec<String> = fs::read_dir(sb.remuda_home())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    entries.sort();
    assert_eq!(entries, ["config.toml", "state"]);
    let state: Vec<String> = fs::read_dir(sb.remuda_home().join("state"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(state, ["launches.jsonl"]);
}
