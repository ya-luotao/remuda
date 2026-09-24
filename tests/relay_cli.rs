//! R19 (with R5, R16, R18): `remuda relay <session> <account>`.

mod common;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

use common::transcripts::*;
use common::{Sandbox, parse_table};
use predicates::prelude::*;
use serde_json::json;

const ID: &str = "766560c5-74e6-45f5-89fd-d92926b14898";

/// `max` holds session [`ID`] (last written in `proj`, the tail still being written) with
/// checkpoints; `team` has a store of its own.
struct Setup {
    sb: Sandbox,
    max: PathBuf,
    team: PathBuf,
    proj: PathBuf,
    transcript: PathBuf,
}

fn setup() -> Setup {
    let sb = Sandbox::new();
    let max = sb.make_claude_home("p/max");
    let team = sb.make_claude_home("p/team");
    fs::create_dir_all(team.join("projects")).unwrap();
    let proj = sb.root().canonicalize().unwrap().join("proj");
    fs::create_dir_all(&proj).unwrap();
    let dir = max.join("projects/-w-proj");
    fs::create_dir_all(&dir).unwrap();
    let transcript = dir.join(format!("{ID}.jsonl"));
    let cwd = proj.to_str().unwrap();
    fs::write(
        &transcript,
        format!(
            "{}{}{{\"type\":\"user\",\"cwd\":",
            user("fix the bug", "/old/place", &ts(1)),
            user("and more", cwd, &ts(2)),
        ),
    )
    .unwrap();
    let checkpoints = max.join("file-history").join(ID);
    fs::create_dir_all(&checkpoints).unwrap();
    fs::write(checkpoints.join("abc@v1"), "before").unwrap();
    sb.register(&[("max", &max), ("team", &team)]);
    Setup {
        sb,
        max,
        team,
        proj,
        transcript,
    }
}

impl Setup {
    fn copy(&self) -> PathBuf {
        self.team
            .join("projects/-w-proj")
            .join(format!("{ID}.jsonl"))
    }

    fn relay(&self, account: &str) -> assert_cmd::assert::Assert {
        self.sb.remuda().args(["relay", ID, account]).assert()
    }
}

/// R19: the transcript (up to its last complete line) and its checkpoints are copied into
/// the target's store, and claude forks the copy there, in `cwd_last`, under the target.
#[test]
fn relay_copies_then_forks_under_the_target() {
    let s = setup();
    s.relay("team")
        .success()
        .stderr(predicate::str::contains(format!(
            "copied session {ID} into the projects store of claude:team"
        )));
    let copy = fs::read_to_string(s.copy()).unwrap();
    assert!(copy.ends_with("}\n"), "no partial record: {copy:?}");
    assert_eq!(copy.lines().count(), 2);
    assert_eq!(
        fs::read_to_string(s.team.join("file-history").join(ID).join("abc@v1")).unwrap(),
        "before"
    );
    let original = fs::read_to_string(&s.transcript).unwrap();
    assert!(original.ends_with("\"cwd\":"), "the original is untouched");

    let inv = s.sb.only_invocation();
    assert_eq!(inv.config_dir.as_deref(), Some(s.team.to_str().unwrap()));
    assert_eq!(inv.cwd, s.proj);
    assert_eq!(
        inv.args[..4],
        ["--resume", ID, "--fork-session", "--session-id"]
    );
    let new = &inv.args[4];
    assert_ne!(new, ID);
    assert_eq!(inv.args.len(), 5);

    let log = s.sb.launches();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0]["account"], json!("claude:team"));
    assert_eq!(log[0]["args"], json!(["--resume", ID, "--fork-session"]));
    assert_eq!(log[0]["session_id"], json!(new));
    assert_eq!(log[0]["fork_of"], json!(ID));
    assert_eq!(log[0]["cwd"], json!(s.proj.to_str().unwrap()));
    let relay = &log[0]["relay"];
    let copy_real = s.copy().canonicalize().unwrap();
    assert_eq!(
        relay["source"],
        json!(s.transcript.canonicalize().unwrap().to_str().unwrap())
    );
    assert_eq!(relay["transcript"], json!(copy_real.to_str().unwrap()));
    assert_eq!(
        relay["checkpoints"],
        json!([s.team.join("file-history").join(ID).join("abc@v1")])
    );
    assert_eq!(relay["size"], json!(copy.len()));
    assert!(relay["mtime_ns"].as_i64().is_some() || relay["mtime_ns"].is_number());
}

/// R19, R18: the fork gets the shared configuration like any launch, before its arguments.
#[test]
fn relay_injects_shared_configuration() {
    let s = setup();
    let source = s.sb.home().join(".claude");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("CLAUDE.md"), "be brief").unwrap();
    let config = s.sb.read_config();
    s.sb.write_config(&format!("{config}[share.claude]\nfrom = \"default\"\n"));
    s.relay("team").success();
    let inv = s.sb.only_invocation();
    assert!(inv.args[0].starts_with("--add-dir="), "{:?}", inv.args);
    assert!(inv.args[1].starts_with("--settings="), "{:?}", inv.args);
    assert_eq!(inv.args[2..5], ["--resume", ID, "--fork-session"]);
    assert_eq!(inv.add_dir_claude_md.as_deref(), Some("1"));
    assert_eq!(
        s.sb.launches()[0]["shared"][0]["option"],
        json!("--add-dir")
    );
}

/// R19: an unchanged earlier copy is replaced by a new relay; one that changed, or a file
/// remuda did not copy, is never overwritten.
#[test]
fn relay_replaces_only_its_own_unchanged_copy() {
    let s = setup();
    s.relay("team").success();
    // The session went on meanwhile (its last record is complete now).
    let mut original = fs::read_to_string(&s.transcript).unwrap();
    original.push_str(&format!("{:?}}}\n", s.proj.to_str().unwrap()));
    fs::write(&s.transcript, &original).unwrap();
    s.relay("team").success();
    assert_eq!(fs::read_to_string(s.copy()).unwrap(), original);
    assert_eq!(s.sb.invocations().len(), 2);

    // Changed since: refused, nothing run or logged.
    let mut changed = original.clone();
    changed.push_str("{}\n");
    fs::write(s.copy(), &changed).unwrap();
    s.relay("team").failure().stderr(predicate::str::contains(
        "is not an unchanged earlier relay copy",
    ));
    assert_eq!(fs::read_to_string(s.copy()).unwrap(), changed);
    assert_eq!(s.sb.invocations().len(), 2);
    assert_eq!(s.sb.launches().len(), 2);

    // Someone else's transcript at the destination: the target's store has the session, so
    // there is nothing to relay (and nothing is overwritten).
    let other = setup();
    fs::create_dir_all(other.copy().parent().unwrap()).unwrap();
    fs::write(other.copy(), "theirs\n").unwrap();
    other
        .relay("team")
        .failure()
        .stderr(predicate::str::contains("already holds session"));
    assert_eq!(fs::read_to_string(other.copy()).unwrap(), "theirs\n");
    assert!(other.sb.invocations().is_empty());
}

/// R19: checkpoints come from every home whose `projects` resolves to the transcript's store.
#[test]
fn relay_takes_checkpoints_from_every_home_of_the_store() {
    let s = setup();
    let alt = s.sb.make_claude_home("p/alt");
    symlink(s.max.join("projects"), alt.join("projects")).unwrap();
    let dir = alt.join("file-history").join(ID);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("def@v2"), "later").unwrap();
    s.sb.register(&[("max", &s.max), ("team", &s.team), ("alt", &alt)]);
    s.relay("team").success();
    let copied = s.team.join("file-history").join(ID);
    assert_eq!(fs::read_to_string(copied.join("abc@v1")).unwrap(), "before");
    assert_eq!(fs::read_to_string(copied.join("def@v2")).unwrap(), "later");
}

/// R19: the refusals, before anything is copied.
#[test]
fn relay_refusals() {
    // The target already sees the transcript: a fork does it.
    let s = setup();
    let alt = s.sb.make_claude_home("p/alt");
    symlink(s.max.join("projects"), alt.join("projects")).unwrap();
    s.sb.register(&[("max", &s.max), ("team", &s.team), ("alt", &alt)]);
    for account in ["alt", "max"] {
        s.relay(account)
            .failure()
            .stderr(predicate::str::contains("fork it there instead"));
    }
    // A codex target.
    s.sb.install_codex();
    let cx = s.sb.make_codex_home("p/cx");
    let config = s.sb.read_config();
    s.sb.write_config(&format!(
        "{config}[[account]]\nprovider = \"codex\"\nname = \"cx\"\nhome = \"{}\"\n",
        cx.display()
    ));
    s.relay("codex:cx")
        .failure()
        .stderr(predicate::str::contains("is not a claude account"));
    // Unknown session and account; an id that is not a UUID.
    s.sb.remuda()
        .args(["relay", "11111111-2222-4333-8444-555555555555", "team"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("is not in the index"));
    s.relay("nobody")
        .failure()
        .stderr(predicate::str::contains("unknown account"));
    s.sb.remuda()
        .args(["relay", "--", "-x", "team"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not a full session ID"));
    // The session's directory is gone.
    fs::remove_dir(&s.proj).unwrap();
    s.relay("team")
        .failure()
        .stderr(predicate::str::contains("does not exist"));
    assert!(!s.copy().exists());
    assert!(!s.team.join("file-history").exists());
    assert!(s.sb.invocations().is_empty());
}

/// R19: the copy is hidden from the session list, and the original keeps its attribution.
#[test]
fn relay_copies_are_hidden_from_sessions() {
    let s = setup();
    fs::write(
        s.max.join("history.jsonl"),
        format!(
            "{{\"display\":\"p\",\"timestamp\":1,\"project\":\"/w\",\"sessionId\":\"{ID}\"}}\n"
        ),
    )
    .unwrap();
    s.relay("team").success();
    let out = s.sb.remuda().args(["sessions"]).assert().success();
    let rows = parse_table(&String::from_utf8(out.get_output().stdout.clone()).unwrap());
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["ACCOUNTS"], "claude:max");

    // Without the launch log the copy is just the same id in another store (R16).
    fs::remove_dir_all(s.sb.remuda_home().join("state")).unwrap();
    let out = s.sb.remuda().args(["sessions"]).assert().success();
    let rows = parse_table(&String::from_utf8(out.get_output().stdout.clone()).unwrap());
    assert_eq!(rows.len(), 2, "{rows:?}");
}

/// R19: a relay copy is never left without its launch record: when the log cannot be
/// written, the copy is removed and claude does not run.
#[test]
fn relay_removes_its_copy_when_the_launch_cannot_be_logged() {
    let s = setup();
    fs::create_dir_all(s.sb.launch_log()).unwrap();
    s.relay("team")
        .failure()
        .stderr(predicate::str::contains("cannot write launch log"))
        .stderr(predicate::str::contains("the relay copy was removed"));
    assert!(!s.copy().exists());
    assert!(s.sb.invocations().is_empty());
    // Checkpoints stay: immutable, and copied again (or kept) by the next relay.
    assert!(
        s.team
            .join("file-history")
            .join(ID)
            .join("abc@v1")
            .is_file()
    );
}

/// R19: when claude cannot be started, the copy just placed is removed too.
#[test]
fn relay_removes_its_copy_when_claude_cannot_start() {
    let s = setup();
    common::write_executable(&s.sb.bin().join("claude"), "#!/nonexistent/interpreter\n");
    s.relay("team")
        .failure()
        .stderr(predicate::str::contains("the relay copy was removed"));
    assert!(!s.copy().exists());
}

/// R19: a checkpoint that cannot be copied stops the relay before any transcript is placed.
#[test]
fn relay_stops_before_the_transcript_when_a_checkpoint_fails() {
    use std::os::unix::fs::PermissionsExt;
    let s = setup();
    let locked = s.max.join("file-history").join(ID).join("locked@v1");
    fs::write(&locked, "x").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    s.relay("team")
        .failure()
        .stderr(predicate::str::contains("locked@v1"));
    assert!(!s.copy().exists());
    assert!(s.sb.invocations().is_empty());
    assert!(!s.sb.launch_log().exists());
}

/// R19: a transcript with no complete line is refused before anything is written.
#[test]
fn relay_refuses_a_transcript_without_a_complete_line() {
    let s = setup();
    fs::write(&s.transcript, "{\"type\":\"user\",").unwrap();
    s.relay("team")
        .failure()
        .stderr(predicate::str::contains("has no complete record"));
    assert!(!s.copy().exists());
    assert!(!s.team.join("file-history").exists());
}
