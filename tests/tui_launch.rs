//! R6, R16: launches from the TUI, at the library level. The TUI decides a
//! [`remuda::launch::prepare`] plan like `run` does, logs it, then runs claude in the
//! foreground in the session's directory and waits (the terminal suspend/restore around it
//! is checked by hand).

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;

use common::Sandbox;
use remuda::launch::{self, EnvChange};
use remuda::provider::Provider;
use remuda::registry::{Account, Home};
use serde_json::json;

fn named(name: &str, home: &str) -> Account {
    Account {
        provider: Provider::Claude,
        name: name.into(),
        home: Home::Path(home.into()),
    }
}

fn plan(account: &Account, args: &[&str], cwd: &Path) -> launch::Launch {
    launch::prepare(
        account,
        args.iter().map(|a| a.to_string()).collect(),
        Some(cwd),
        "2026-09-24T12:00:00Z".into(),
        || "11111111-2222-4333-8444-555555555555".into(),
    )
}

#[test]
fn a_new_session_is_logged_then_run_in_its_directory() {
    let sb = Sandbox::new();
    let dir = sb.root().join("a project");
    fs::create_dir(&dir).unwrap();
    let account = named("max", "/p/max with space/");
    let plan = plan(&account, &["-n", "fix it"], &dir);
    let ran = launch::perform(
        &sb.bin().join("claude"),
        &plan,
        Some(&dir),
        &sb.launch_log(),
    );
    assert_eq!(ran.status.unwrap().code(), Some(0));
    assert_eq!(ran.log_error, None);

    let inv = sb.only_invocation();
    assert_eq!(inv.cwd, dir.canonicalize().unwrap());
    assert_eq!(inv.config_dir.as_deref(), Some("/p/max with space/"));
    assert_eq!(
        inv.args,
        [
            "-n",
            "fix it",
            "--session-id",
            "11111111-2222-4333-8444-555555555555"
        ]
    );
    let log = sb.launches();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0]["account"], json!("claude:max"));
    assert_eq!(log[0]["cwd"], json!(dir.to_str().unwrap()));
    assert_eq!(log[0]["args"], json!(["-n", "fix it"]));
    assert_eq!(
        log[0]["session_id"],
        json!("11111111-2222-4333-8444-555555555555")
    );
    assert_eq!(log[0]["injected"], json!(true));
}

#[test]
fn the_default_account_runs_without_config_dir() {
    let sb = Sandbox::new();
    let plan = plan(
        &Account::default_for(Provider::Claude),
        &["--resume", "abc"],
        &sb.work(),
    );
    assert_eq!(
        plan.env,
        EnvChange::Remove("CLAUDE_CONFIG_DIR".into()),
        "R2"
    );
    let ran = launch::perform(
        &sb.bin().join("claude"),
        &plan,
        Some(&sb.work()),
        &sb.launch_log(),
    );
    assert!(ran.status.unwrap().success());
    let inv = sb.only_invocation();
    assert_eq!(inv.config_dir, None);
    assert_eq!(inv.args, ["--resume", "abc"]);
    assert_eq!(sb.launches()[0]["session_id"], json!("abc"));
}

#[test]
fn a_log_failure_is_reported_and_claude_still_runs() {
    let sb = Sandbox::new();
    // `state` is a file: the log cannot be created below it.
    fs::create_dir_all(sb.remuda_home()).unwrap();
    fs::write(sb.remuda_home().join("state"), "").unwrap();
    let plan = plan(&named("max", "/p/max"), &[], &sb.work());
    let ran = launch::perform(
        &sb.bin().join("claude"),
        &plan,
        Some(&sb.work()),
        &sb.launch_log(),
    );
    assert!(ran.status.unwrap().success());
    assert!(ran.log_error.is_some());
    assert_eq!(sb.invocations().len(), 1);
}

#[test]
fn a_missing_directory_fails_before_anything_runs() {
    let sb = Sandbox::new();
    let gone = sb.root().join("gone");
    let plan = plan(&named("max", "/p/max"), &["--resume", "abc"], &gone);
    let ran = launch::perform(
        &sb.bin().join("claude"),
        &plan,
        Some(&gone),
        &sb.launch_log(),
    );
    assert!(ran.status.is_err());
    assert!(sb.invocations().is_empty());
}

/// remuda ignores Ctrl-C while claude owns the terminal, but claude itself must not: its
/// SIGINT is back to the default action.
#[test]
fn the_child_keeps_the_default_interrupt_action() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("self-interrupt");
    common::write_executable(&script, "#!/bin/sh\nkill -INT $$\nsleep 5\nexit 0\n");
    let keep = EnvChange::Set("REMUDA_TEST".into(), "1".into());
    let status = launch::run_foreground(&script, &[], &keep, Some(dir.path())).unwrap();
    assert_eq!(status.signal(), Some(2), "{status:?}");
}

#[test]
fn exit_codes_come_back() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("fail");
    common::write_executable(&script, "#!/bin/sh\nexit 3\n");
    let keep = EnvChange::Set("REMUDA_TEST".into(), "1".into());
    let status = launch::run_foreground(&script, &[], &keep, None).unwrap();
    assert_eq!(status.code(), Some(3));
}

// --- the TUI's launch path, one test per kind of launch ------------------------------------

use std::sync::{Arc, mpsc};

use remuda::tui::accounts::Listing;
use remuda::tui::app::{Event, Exit, LaunchRequest};
use remuda::tui::{self, Deps, Screen};

/// Records suspend/resume instead of touching a terminal; either can be made to fail.
#[derive(Default)]
struct FakeScreen {
    calls: Vec<&'static str>,
    fail_suspend: bool,
    fail_resume: bool,
}

impl Screen for FakeScreen {
    fn suspend(&mut self) -> std::io::Result<()> {
        self.calls.push("suspend");
        if self.fail_suspend {
            return Err(std::io::Error::other("no tty"));
        }
        Ok(())
    }
    fn resume(&mut self) -> std::io::Result<()> {
        self.calls.push("resume");
        if self.fail_resume {
            return Err(std::io::Error::other("no tty"));
        }
        Ok(())
    }
}

/// A TUI started now: its accounts are the registry's as it is (the implicit default alone
/// when there is none).
fn deps(sb: &Sandbox) -> Deps {
    let env: remuda::Env = [("HOME".to_string(), sb.home().display().to_string())].into();
    Deps {
        listing: Arc::new(Listing::open(sb.config_path(), env.clone()).unwrap()),
        env,
        claude: Some(sb.bin().join("claude")),
        codex: None,
        ps: None,
        tz: jiff::tz::TimeZone::UTC,
        clock: jiff::Timestamp::now,
        state_dir: sb.remuda_home().join("state"),
        cwd: Some(sb.work()),
        mode: remuda::tui::app::Mode::Browse,
        private: false,
    }
}

/// Where a launch tells that the registry changed, when no test listens.
fn quiet() -> mpsc::Sender<Event> {
    mpsc::channel().0
}

/// Writes a registry with `accounts`: the TUI launches only accounts it lists (R16).
fn register(sb: &Sandbox, accounts: &[&Account]) {
    let mut text = String::new();
    for a in accounts {
        text.push_str(&format!(
            "[[account]]\nprovider = \"{}\"\nname = \"{}\"\nhome = \"{}\"\n\n",
            a.provider, a.name, a.home
        ));
    }
    sb.write_config(&text);
}

fn tui_launch(sb: &Sandbox, account: &Account, args: &[&str], cwd: Option<&Path>) -> Event {
    let request = LaunchRequest {
        account: account.clone(),
        args: args.iter().map(|a| a.to_string()).collect(),
        cwd: cwd.map(Path::to_path_buf),
        what: "test".into(),
    };
    let mut screen = FakeScreen::default();
    let event = tui::launch_in_foreground(&mut screen, &deps(sb), request, &quiet()).unwrap();
    assert_eq!(screen.calls, ["suspend", "resume"], "the TUI steps aside");
    event
}

fn exit_of(event: &Event) -> &Result<Exit, String> {
    match event {
        Event::Launched { result, .. } => result,
        other => panic!("{other:?}"),
    }
}

fn project(sb: &Sandbox) -> std::path::PathBuf {
    let dir = sb.root().join("some project");
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn tui_new_session_injects_an_id_and_names_it() {
    let sb = Sandbox::new();
    let dir = project(&sb);
    let max = named("max", "/p/max/");
    register(&sb, &[&max]);
    let event = tui_launch(&sb, &max, &["-n", "fix it"], Some(&dir));
    assert_eq!(exit_of(&event), &Ok(Exit::Code(0)));
    let inv = sb.only_invocation();
    assert_eq!(inv.cwd, dir.canonicalize().unwrap());
    assert_eq!(inv.config_dir.as_deref(), Some("/p/max/"));
    assert_eq!(inv.args[..3], ["-n", "fix it", "--session-id"]);
    let log = sb.launches();
    assert_eq!(log[0]["session_id"], json!(inv.args[3]));
    assert_eq!(log[0]["injected"], json!(true));
    assert!(log[0].get("fork_of").is_none());
    assert_eq!(log[0]["cwd"], json!(dir.to_str().unwrap()));
}

#[test]
fn tui_resume_runs_in_the_sessions_directory() {
    let sb = Sandbox::new();
    let dir = project(&sb);
    let event = tui_launch(
        &sb,
        &Account::default_for(Provider::Claude),
        &["--resume", "s-1"],
        Some(&dir),
    );
    assert_eq!(exit_of(&event), &Ok(Exit::Code(0)));
    let inv = sb.only_invocation();
    assert_eq!(inv.cwd, dir.canonicalize().unwrap());
    assert_eq!(
        inv.config_dir, None,
        "default: CLAUDE_CONFIG_DIR removed (R2)"
    );
    assert_eq!(inv.args, ["--resume", "s-1"]);
    let log = sb.launches();
    assert_eq!(log[0]["session_id"], json!("s-1"));
    assert_eq!(log[0]["injected"], json!(false));
    assert!(log[0].get("fork_of").is_none());
}

#[test]
fn tui_fork_gets_a_new_id_and_records_the_original() {
    let sb = Sandbox::new();
    let dir = project(&sb);
    let team = named("team", "/p/team");
    register(&sb, &[&team]);
    tui_launch(
        &sb,
        &team,
        &["--resume", "s-1", "--fork-session"],
        Some(&dir),
    );
    let inv = sb.only_invocation();
    assert_eq!(inv.cwd, dir.canonicalize().unwrap());
    assert_eq!(inv.config_dir.as_deref(), Some("/p/team"));
    assert_eq!(
        inv.args[..4],
        ["--resume", "s-1", "--fork-session", "--session-id"]
    );
    let log = sb.launches();
    assert_eq!(log[0]["session_id"], json!(inv.args[4]));
    assert_eq!(log[0]["fork_of"], json!("s-1"));
    assert_eq!(log[0]["injected"], json!(true));
}

#[test]
fn tui_attach_runs_in_remudas_directory_and_injects_nothing() {
    let sb = Sandbox::new();
    let max = named("max", "/p/max");
    register(&sb, &[&max]);
    tui_launch(&sb, &max, &["attach", "766560c5"], None);
    let inv = sb.only_invocation();
    assert_eq!(inv.cwd, sb.work().canonicalize().unwrap());
    assert_eq!(inv.config_dir.as_deref(), Some("/p/max"));
    assert_eq!(inv.args, ["attach", "766560c5"]);
    let log = sb.launches();
    assert_eq!(log[0]["args"], json!(["attach", "766560c5"]));
    assert_eq!(log[0]["session_id"], serde_json::Value::Null);
    assert_eq!(log[0]["injected"], json!(false));
}

#[test]
fn tui_launch_without_claude_does_not_suspend() {
    let sb = Sandbox::new();
    let mut deps = deps(&sb);
    deps.claude = None;
    let request = LaunchRequest {
        account: named("max", "/p/max"),
        args: vec![],
        cwd: None,
        what: "new session as max".into(),
    };
    let mut screen = FakeScreen::default();
    let event = tui::launch_in_foreground(&mut screen, &deps, request, &quiet()).unwrap();
    assert!(screen.calls.is_empty());
    assert_eq!(
        exit_of(&event),
        &Err("`claude` not found on PATH".to_string())
    );
}

/// R16: when the terminal cannot be handed over, nothing is logged or run.
#[test]
fn tui_launch_that_cannot_suspend_runs_nothing() {
    let sb = Sandbox::new();
    let max = named("max", "/p/max");
    register(&sb, &[&max]);
    let request = LaunchRequest {
        account: max,
        args: vec![],
        cwd: None,
        what: "new session as max".into(),
    };
    let mut screen = FakeScreen {
        fail_suspend: true,
        ..FakeScreen::default()
    };
    let event = tui::launch_in_foreground(&mut screen, &deps(&sb), request, &quiet()).unwrap();
    let Err(e) = exit_of(&event) else {
        panic!("{event:?}")
    };
    assert!(e.contains("cannot hand the terminal over"), "{e}");
    assert_eq!(screen.calls, ["suspend", "resume"]);
    assert!(sb.invocations().is_empty());
    assert!(!sb.launch_log().exists());
}

/// R11, R16, R18: the authentication a member reads through a linked `settings.json` is
/// named for a launch from the TUI as for `run`, among the launch's warnings.
#[test]
fn tui_launches_name_authentication_read_through_a_link() {
    let sb = Sandbox::new();
    let source = sb.home().join(".claude");
    fs::create_dir_all(source.join("projects")).unwrap();
    fs::write(
        source.join("settings.json"),
        r#"{"forceLoginOrgUUID": "zz-org", "model": "opus"}"#,
    )
    .unwrap();
    let max = sb.root().join("max");
    fs::create_dir_all(&max).unwrap();
    for item in ["settings.json", "projects"] {
        std::os::unix::fs::symlink(source.join(item), max.join(item)).unwrap();
    }
    sb.write_config(&format!(
        "[[account]]\nprovider = \"claude\"\nname = \"max\"\nhome = \"{}\"\n\
         [share.claude]\nfrom = \"default\"\n",
        max.display()
    ));
    let account = named("max", max.to_str().unwrap());
    let event = tui_launch(&sb, &account, &["-p", "hi"], Some(&project(&sb)));
    let Event::Launched {
        warnings, result, ..
    } = &event
    else {
        panic!("{event:?}")
    };
    assert_eq!(result, &Ok(Exit::Code(0)));
    assert_eq!(
        warnings,
        &[
            "warning: claude:max reads the authentication settings of claude:default through its \
           settings.json link: forceLoginOrgUUID"
        ]
    );
}

/// R16, R18: the TUI launches through the same path as `run`: shared configuration from the
/// registry as it is at launch goes before the arguments, and its notices come back as
/// warnings.
#[test]
fn tui_launches_get_shared_configuration() {
    let sb = Sandbox::new();
    let source = sb.home().join(".claude");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("CLAUDE.md"), "be brief").unwrap();
    let max = sb.make_claude_home("max");
    sb.write_config(&format!(
        "[[account]]\nprovider = \"claude\"\nname = \"max\"\nhome = \"{}\"\n\
         [share.claude]\nfrom = \"default\"\n",
        max.display()
    ));
    let dir = project(&sb);
    let account = named("max", max.to_str().unwrap());
    let event = tui_launch(&sb, &account, &["--settings", "/x.json"], Some(&dir));
    let Event::Launched { warnings, .. } = &event else {
        panic!("{event:?}")
    };
    assert_eq!(
        warnings,
        &["--settings given: settings and auto-memory from claude:default are not injected"]
    );
    let inv = sb.only_invocation();
    let shared = sb.remuda_home().join("shared/claude");
    assert_eq!(
        inv.args[..3],
        [
            format!("--add-dir={}", shared.display()),
            "--settings".to_string(),
            "/x.json".to_string()
        ]
    );
    assert_eq!(inv.add_dir_claude_md.as_deref(), Some("1"));
    assert_eq!(
        fs::read_link(shared.join(".claude/CLAUDE.md")).unwrap(),
        source.join("CLAUDE.md"),
        "the shared directory holds one link per item the source has"
    );
    assert_eq!(sb.launches()[0]["shared"][0]["option"], json!("--add-dir"));

    // A registry that cannot be read now: the launch does not happen.
    let deps = deps(&sb);
    sb.write_config("[share.claude]\nfrom = \"nobody\"\n");
    let mut screen = FakeScreen::default();
    let request = LaunchRequest {
        account,
        args: vec![],
        cwd: Some(dir),
        what: "test".into(),
    };
    let event = tui::launch_in_foreground(&mut screen, &deps, request, &quiet()).unwrap();
    assert!(screen.calls.is_empty());
    let Err(e) = exit_of(&event) else {
        panic!("{event:?}")
    };
    assert!(e.contains("names no claude account"), "{e}");
    assert_eq!(sb.invocations().len(), 1);
}

/// R18: a directory typed into the TUI's new-session form is made real before it names the
/// project: a symlink with a trailing slash and a `..` give the same memory location.
#[test]
fn tui_memory_uses_the_real_start_directory() {
    let sb = Sandbox::new();
    fs::create_dir_all(sb.home().join(".claude")).unwrap();
    let max = sb.make_claude_home("max");
    sb.write_config(&format!(
        "[[account]]\nprovider = \"claude\"\nname = \"max\"\nhome = \"{}\"\n\
         [share.claude]\nfrom = \"default\"\n",
        max.display()
    ));
    let real = project(&sb).canonicalize().unwrap();
    fs::create_dir_all(real.join("sub")).unwrap();
    std::os::unix::fs::symlink(&real, sb.root().join("link")).unwrap();
    let account = named("max", max.to_str().unwrap());
    let mut deps = deps(&sb);
    deps.env.insert("PATH".into(), sb.path_var());
    let mut memories = Vec::new();
    for typed in [
        format!("{}/", sb.root().join("link").display()),
        format!("{}/sub/..", real.display()),
    ] {
        let _ = fs::remove_file(sb.claude_out());
        let request = LaunchRequest {
            account: account.clone(),
            args: vec![],
            cwd: Some(typed.clone().into()),
            what: "test".into(),
        };
        let mut screen = FakeScreen::default();
        tui::launch_in_foreground(&mut screen, &deps, request, &quiet()).unwrap();
        let inv = sb.only_invocation();
        let path = inv.args[0]
            .strip_prefix("--settings=")
            .expect("settings injected");
        let settings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        memories.push(
            settings["autoMemoryDirectory"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    let encoded: String = real
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let want = format!(
        "{}/projects/{encoded}/memory",
        sb.home().join(".claude").display()
    );
    assert_eq!(memories, [want.clone(), want]);
}

// --- codex from the TUI (R17) ---------------------------------------------------------------

const CODEX_ID: &str = "019c1e08-e4f6-7d70-a129-38ec744a3f3c";

fn codex_work() -> Account {
    Account {
        provider: Provider::Codex,
        name: "work".into(),
        home: Home::Path("/c/work with space/".into()),
    }
}

/// `codex resume <id> -C <dir>` (the TUI's own args) under the rollout's home, in that
/// directory; logged with the resumed id, nothing injected.
#[test]
fn tui_codex_resume_runs_codex_in_its_home_and_directory() {
    let sb = Sandbox::new();
    sb.install_codex();
    let dir = project(&sb);
    register(&sb, &[&codex_work()]);
    let deps = Deps {
        codex: Some(sb.bin().join("codex")),
        ..deps(&sb)
    };
    let args = Provider::Codex.resume_args(CODEX_ID, &dir, false);
    assert_eq!(args, ["resume", CODEX_ID, "-C", dir.to_str().unwrap()]);
    let request = LaunchRequest {
        account: codex_work(),
        args: args.clone(),
        cwd: Some(dir.clone()),
        what: "resume 019c1e08 as codex:work".into(),
    };
    let mut screen = FakeScreen::default();
    let event = tui::launch_in_foreground(&mut screen, &deps, request, &quiet()).unwrap();
    assert_eq!(exit_of(&event), &Ok(Exit::Code(0)));
    assert_eq!(screen.calls, ["suspend", "resume"]);
    let [inv] = sb.codex_invocations().try_into().unwrap();
    assert_eq!(inv.cwd, dir.canonicalize().unwrap());
    assert_eq!(inv.codex_home.as_deref(), Some("/c/work with space/"));
    assert_eq!(inv.args, args);
    assert!(sb.invocations().is_empty());
    let log = sb.launches();
    assert_eq!(log[0]["account"], json!("codex:work"));
    assert_eq!(log[0]["args"], json!(args));
    assert_eq!(log[0]["session_id"], json!(CODEX_ID));
    assert_eq!(log[0]["injected"], json!(false));
}

/// A codex fork from the TUI gets its id from codex: logged with a null session id, and the
/// forked id as `fork_of` (R17).
#[test]
fn tui_codex_fork_is_logged_with_fork_of_and_without_a_session_id() {
    let sb = Sandbox::new();
    sb.install_codex();
    let dir = project(&sb);
    register(&sb, &[&codex_work()]);
    let deps = Deps {
        codex: Some(sb.bin().join("codex")),
        ..deps(&sb)
    };
    let args = Provider::Codex.resume_args(CODEX_ID, &dir, true);
    let request = LaunchRequest {
        account: codex_work(),
        args: args.clone(),
        cwd: Some(dir.clone()),
        what: "fork 019c1e08 as codex:work".into(),
    };
    let mut screen = FakeScreen::default();
    let event = tui::launch_in_foreground(&mut screen, &deps, request, &quiet()).unwrap();
    assert_eq!(exit_of(&event), &Ok(Exit::Code(0)));
    let [inv] = sb.codex_invocations().try_into().unwrap();
    assert_eq!(inv.args, args);
    let log = sb.launches();
    assert_eq!(log[0]["args"], json!(args));
    assert_eq!(log[0]["session_id"], serde_json::Value::Null);
    assert_eq!(log[0]["fork_of"], json!(CODEX_ID));
    assert_eq!(log[0]["injected"], json!(false));
}

#[test]
fn tui_codex_launch_without_codex_does_not_suspend() {
    let sb = Sandbox::new();
    let request = LaunchRequest {
        account: codex_work(),
        args: vec!["-C".into(), "/w".into()],
        cwd: None,
        what: "new session as codex:work".into(),
    };
    let mut screen = FakeScreen::default();
    let event = tui::launch_in_foreground(&mut screen, &deps(&sb), request, &quiet()).unwrap();
    assert!(screen.calls.is_empty());
    assert_eq!(
        exit_of(&event),
        &Err("`codex` not found on PATH".to_string())
    );
}

#[test]
fn tui_codex_setup_runs_codex_login_in_the_new_home() {
    let sb = Sandbox::new();
    sb.install_codex();
    let deps = Deps {
        env: [
            ("HOME".to_string(), sb.home().display().to_string()),
            (
                "REMUDA_HOME".to_string(),
                sb.remuda_home().display().to_string(),
            ),
        ]
        .into(),
        codex: Some(sb.bin().join("codex")),
        ..deps(&sb)
    };
    let mut screen = FakeScreen::default();
    let event =
        tui::setup_in_foreground(&mut screen, &deps, Provider::Codex, "work", None, &quiet())
            .unwrap();
    assert_eq!(
        event,
        Event::SetupDone {
            provider: Provider::Codex,
            name: "work".into(),
            result: Ok(Exit::Code(0)),
            links: Vec::new(),
        }
    );
    let home = sb.remuda_home().join("homes/codex/work");
    let [inv] = sb.codex_invocations().try_into().unwrap();
    assert_eq!(inv.args, ["login"]);
    assert_eq!(inv.codex_home.as_deref(), Some(home.to_str().unwrap()));
    assert!(sb.invocations().is_empty());
}

// --- setup from the TUI: the same steps as `remuda setup` ----------------------------------

fn stored_names(sb: &Sandbox) -> Vec<String> {
    let Ok(text) = fs::read_to_string(sb.config_path()) else {
        return Vec::new();
    };
    remuda::registry::Registry::from_document(&text.parse().unwrap())
        .unwrap()
        .all(&remuda::Env::new())
        .into_iter()
        .map(|a| a.name)
        .collect()
}

#[test]
fn tui_setup_creates_registers_and_logs_in_in_the_foreground() {
    let sb = Sandbox::new();
    let deps = Deps {
        env: [
            ("HOME".to_string(), sb.home().display().to_string()),
            (
                "REMUDA_HOME".to_string(),
                sb.remuda_home().display().to_string(),
            ),
        ]
        .into(),
        ..deps(&sb)
    };
    let mut screen = FakeScreen::default();
    let event = tui::setup_in_foreground(
        &mut screen,
        &deps,
        Provider::Claude,
        "work",
        Some("me+work@example.com".into()),
        &quiet(),
    )
    .unwrap();
    assert_eq!(
        event,
        Event::SetupDone {
            provider: Provider::Claude,
            name: "work".into(),
            result: Ok(Exit::Code(0)),
            links: Vec::new(),
        }
    );
    assert_eq!(screen.calls, ["suspend", "resume"]);
    let home = sb.remuda_home().join("homes/claude/work");
    assert_eq!(
        fs::metadata(&home).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(stored_names(&sb), ["default", "work"]);
    let inv = sb.only_invocation();
    assert_eq!(
        inv.args,
        ["auth", "login", "--email", "me+work@example.com"]
    );
    assert_eq!(inv.config_dir.as_deref(), Some(home.to_str().unwrap()));
    assert!(!sb.launch_log().exists(), "setup is not a session launch");
}

/// R18: a setup from the TUI links a member's home like `remuda setup`, and its event carries
/// what to say about it, without paths (R21).
#[test]
fn tui_setup_links_a_members_home() {
    let sb = Sandbox::new();
    let source = sb.home().join(".claude");
    fs::create_dir_all(source.join("projects")).unwrap();
    fs::write(source.join("settings.json"), "{}").unwrap();
    fs::write(source.join(".claude.json"), "{}").unwrap();
    sb.write_config("[share.claude]\nfrom = \"default\"\n");
    for (name, private) in [("work", false), ("other", true)] {
        let deps = Deps {
            env: [
                ("HOME".to_string(), sb.home().display().to_string()),
                (
                    "REMUDA_HOME".to_string(),
                    sb.remuda_home().display().to_string(),
                ),
            ]
            .into(),
            private,
            ..deps(&sb)
        };
        let mut screen = FakeScreen::default();
        let event =
            tui::setup_in_foreground(&mut screen, &deps, Provider::Claude, name, None, &quiet())
                .unwrap();
        let note = |text: &str| remuda::setup::Note {
            warning: false,
            text: text.into(),
        };
        assert_eq!(
            event,
            Event::SetupDone {
                provider: Provider::Claude,
                name: name.into(),
                result: Ok(Exit::Code(0)),
                links: vec![
                    note("linked 2 items to claude:default: projects, settings.json"),
                    note(
                        "not linked (claude:default has none): file-history, CLAUDE.md, skills, \
                         commands, agents, hooks, plugins, rules, agent-memory, output-styles, \
                         keybindings.json"
                    ),
                ],
            }
        );
        let home = sb.remuda_home().join("homes/claude").join(name);
        for item in ["projects", "settings.json"] {
            assert_eq!(fs::read_link(home.join(item)).unwrap(), source.join(item));
        }
        assert_eq!(fs::read_dir(&home).unwrap().count(), 2);
        // Linked and registered before the login ran: what the login found.
        let login = sb.logins().pop().unwrap();
        assert_eq!(login.config_dir, home.to_str().unwrap());
        assert!(login.registered, "{name}: registered before the login");
        let found = |item: &str| {
            let target = source.join(item).to_str().unwrap().to_string();
            (item.to_string(), Some(target))
        };
        assert_eq!(login.entries, [found("projects"), found("settings.json")]);
    }
    assert_eq!(sb.logins().len(), 2);
    assert_eq!(stored_names(&sb), ["default", "work", "other"]);
    let logins: Vec<Vec<String>> = sb.invocations().into_iter().map(|i| i.args).collect();
    assert_eq!(logins, [["auth", "login"], ["auth", "login"]]);
}

#[test]
fn tui_setup_refusals_happen_before_anything_is_created() {
    let sb = Sandbox::new();
    let deps = Deps {
        env: [
            ("HOME".to_string(), sb.home().display().to_string()),
            (
                "REMUDA_HOME".to_string(),
                sb.remuda_home().display().to_string(),
            ),
        ]
        .into(),
        ..deps(&sb)
    };
    let existing = sb.remuda_home().join("homes/claude/work");
    fs::create_dir_all(&existing).unwrap();
    let mut screen = FakeScreen::default();
    let Event::SetupDone { result, .. } =
        tui::setup_in_foreground(&mut screen, &deps, Provider::Claude, "work", None, &quiet())
            .unwrap()
    else {
        panic!()
    };
    assert!(result.unwrap_err().contains("already exists"));
    assert!(screen.calls.is_empty(), "the terminal is not handed over");
    assert!(sb.invocations().is_empty());
    assert!(!sb.config_path().exists());

    let no_claude = Deps {
        claude: None,
        ..deps
    };
    let Event::SetupDone { result, .. } = tui::setup_in_foreground(
        &mut screen,
        &no_claude,
        Provider::Claude,
        "other",
        None,
        &quiet(),
    )
    .unwrap() else {
        panic!()
    };
    assert!(result.unwrap_err().contains("claude"));
    assert!(!sb.remuda_home().join("homes/claude/other").exists());
}

/// The terminal cannot be handed over: nothing is created, and the TUI takes it back.
#[test]
fn tui_setup_when_the_terminal_cannot_be_handed_over() {
    let sb = Sandbox::new();
    let deps = Deps {
        env: [
            ("HOME".to_string(), sb.home().display().to_string()),
            (
                "REMUDA_HOME".to_string(),
                sb.remuda_home().display().to_string(),
            ),
        ]
        .into(),
        ..deps(&sb)
    };
    let mut screen = FakeScreen {
        fail_suspend: true,
        ..FakeScreen::default()
    };
    let Event::SetupDone { result, .. } =
        tui::setup_in_foreground(&mut screen, &deps, Provider::Claude, "work", None, &quiet())
            .unwrap()
    else {
        panic!()
    };
    let error = result.unwrap_err();
    assert!(error.contains("cannot hand the terminal over"), "{error}");
    assert_eq!(screen.calls, ["suspend", "resume"], "like a launch");
    assert!(sb.invocations().is_empty());
    assert!(!sb.remuda_home().join("homes/claude/work").exists());
    assert!(!sb.config_path().exists());

    // Taking the terminal back fails: the TUI cannot go on (like a launch).
    let mut screen = FakeScreen {
        fail_resume: true,
        ..FakeScreen::default()
    };
    let err =
        tui::setup_in_foreground(&mut screen, &deps, Provider::Claude, "work", None, &quiet())
            .unwrap_err();
    assert!(
        format!("{err:#}").contains("cannot take the terminal back"),
        "{err:#}"
    );
    // The login ran, and the account stays registered.
    assert_eq!(sb.only_invocation().args, ["auth", "login"]);
    assert_eq!(stored_names(&sb), ["default", "work"]);
}

// --- the pre-launch check: fresh running sessions of every account (R16) ------------------

use remuda::tui::app::Effect;
use remuda::tui::workers;

const U: &str = "0badf00d-0000-4000-8000-000000000000";

/// `max` and `team` registered, and a TUI started with them (and `default`); every account
/// answers `agents --json` from its fixture; no ps.
fn two_accounts(sb: &Sandbox) -> (Deps, Account, Account) {
    let max = sb.make_claude_home("p/max");
    let team = sb.make_claude_home("p/team");
    sb.set_agents(None, "[]");
    sb.set_agents(Some(&max), "[]");
    sb.set_agents(Some(&team), "[]");
    sb.register(&[("max", &max), ("team", &team)]);
    let (max, team) = (
        named("max", max.to_str().unwrap()),
        named("team", team.to_str().unwrap()),
    );
    (deps(sb), max, team)
}

fn home_of(account: &Account) -> &Path {
    match &account.home {
        Home::Path(h) => Path::new(h),
        Home::Default => unreachable!(),
    }
}

fn check_launch(deps: &Deps, account: &Account, args: &[&str], cwd: &Path) -> Option<String> {
    let (events, error) = check_launch_events(deps, account, args, cwd);
    assert_eq!(events, [], "the registry did not change");
    error
}

/// The check's answer, and the events the worker sent before it.
fn check_launch_events(
    deps: &Deps,
    account: &Account,
    args: &[&str],
    cwd: &Path,
) -> (Vec<Event>, Option<String>) {
    let request = LaunchRequest {
        account: account.clone(),
        args: args.iter().map(|a| a.to_string()).collect(),
        cwd: Some(cwd.to_path_buf()),
        what: "test".into(),
    };
    let (tx, rx) = mpsc::channel();
    workers::spawn(
        Effect::CheckLaunch {
            check: 1,
            request: request.clone(),
        },
        &Arc::new(deps.clone()),
        &tx,
    );
    let mut before = Vec::new();
    loop {
        match rx.recv_timeout(std::time::Duration::from_secs(20)).unwrap() {
            Event::LaunchChecked {
                check: 1,
                request: r,
                error,
            } => {
                assert_eq!(r, request);
                return (before, error.map(|e| e.as_str().to_string()));
            }
            other => before.push(other),
        }
    }
}

#[test]
fn a_resume_is_checked_against_every_accounts_running_sessions() {
    let sb = Sandbox::new();
    let (deps, max, team) = two_accounts(&sb);
    let dir = project(&sb);
    // Running nowhere: fine.
    assert_eq!(check_launch(&deps, &max, &["--resume", U], &dir), None);
    // Running interactively under another account: refused, with where and which pid.
    sb.set_agents(
        Some(home_of(&team)),
        &format!(r#"[{{"pid": 21, "kind": "interactive", "sessionId": "{U}", "status": "busy"}}]"#),
    );
    let error = check_launch(&deps, &max, &["--resume", U], &dir).unwrap();
    assert_eq!(
        error,
        "session 0badf00d is running in team (pid 21): switch to its terminal \
         (two claudes writing one session overwrite each other)"
    );
    // A fork only reads it: allowed.
    assert_eq!(
        check_launch(&deps, &max, &["--resume", U, "--fork-session"], &dir),
        None
    );
    // A new session is not checked either.
    assert_eq!(check_launch(&deps, &max, &["-n", "x"], &dir), None);
    // A missing directory is reported first.
    let gone = sb.root().join("gone");
    assert_eq!(
        check_launch(&deps, &max, &["--resume", U], &gone),
        Some(format!("{} does not exist", gone.display()))
    );
    // Nothing but claude's own `agents` calls ran.
    assert!(
        sb.invocations()
            .iter()
            .all(|inv| inv.args[..2] == ["agents", "--json"]),
        "{:?}",
        sb.invocations()
    );
}

#[test]
fn a_running_background_session_is_not_resumed_either() {
    let sb = Sandbox::new();
    let (deps, max, _) = two_accounts(&sb);
    let Home::Path(home) = &max.home else {
        unreachable!()
    };
    sb.set_agents(
        Some(Path::new(home)),
        &format!(
            r#"[{{"id": "0badf00d", "kind": "background", "sessionId": "{U}", "state": "blocked"}}]"#
        ),
    );
    let error = check_launch(&deps, &max, &["--resume", U], &project(&sb)).unwrap();
    assert_eq!(
        error,
        "session 0badf00d is running as background session 0badf00d in max: \
         attach to it from Live (two claudes writing one session overwrite each other)"
    );
    // Once stopped, it is an ordinary transcript.
    sb.set_agents(
        Some(Path::new(home)),
        &format!(
            r#"[{{"id": "0badf00d", "kind": "background", "sessionId": "{U}", "state": "stopped"}}]"#
        ),
    );
    assert_eq!(
        check_launch(&deps, &max, &["--resume", U], &project(&sb)),
        None
    );
}

/// "Cannot tell" counts as "maybe running" for a resume (R16).
#[test]
fn a_resume_is_refused_when_an_account_cannot_be_checked() {
    let sb = Sandbox::new();
    let (deps, max, team) = two_accounts(&sb);
    let Home::Path(home) = &team.home else {
        unreachable!()
    };
    // team's `agents` prints nothing usable, and there is no ps for the fallback.
    fs::remove_file(Path::new(home).join("fake-agents.json")).unwrap();
    let error = check_launch(&deps, &max, &["--resume", U], &project(&sb)).unwrap();
    assert!(
        error.starts_with("cannot confirm that session 0badf00d is not running: team: "),
        "{error}"
    );
    // A fork does not need to know.
    assert_eq!(
        check_launch(
            &deps,
            &max,
            &["--resume", U, "--fork-session"],
            &project(&sb)
        ),
        None
    );
}

/// The check asks the accounts registered now, not only those the TUI started with, and
/// tells the TUI about the new ones.
#[test]
fn a_resume_is_checked_against_accounts_registered_after_the_tui_started() {
    let sb = Sandbox::new();
    let (deps, max, team) = two_accounts(&sb);
    // `remuda add work …` in another terminal while the TUI runs; `work` runs the session.
    let work = sb.make_claude_home("p/work");
    sb.set_agents(
        Some(&work),
        &format!(r#"[{{"pid": 31, "kind": "interactive", "sessionId": "{U}", "status": "busy"}}]"#),
    );
    sb.register(&[
        ("max", home_of(&max)),
        ("team", home_of(&team)),
        ("work", &work),
    ]);
    let (events, error) = check_launch_events(&deps, &max, &["--resume", U], &project(&sb));
    assert_eq!(
        error.as_deref(),
        Some(
            "session 0badf00d is running in work (pid 31): switch to its terminal \
             (two claudes writing one session overwrite each other)"
        )
    );
    let work = named("work", work.to_str().unwrap());
    assert_eq!(
        events,
        [Event::Accounts(vec![
            Account::default_for(Provider::Claude),
            max.clone(),
            team,
            work
        ])]
    );
    // A new session reads the registry too, and finds nothing new to tell.
    let (events, error) = check_launch_events(&deps, &max, &["-n", "x"], &project(&sb));
    assert_eq!((events, error), (vec![], None));
}

/// Without the registry the accounts to ask are unknown: a resume is refused.
#[test]
fn a_resume_is_refused_when_the_registry_cannot_be_read() {
    let sb = Sandbox::new();
    let (deps, max, _) = two_accounts(&sb);
    sb.write_config("[[account]]\nprovider = 7\n");
    let (events, error) = check_launch_events(&deps, &max, &["--resume", U], &project(&sb));
    assert_eq!(events, []);
    let error = error.unwrap();
    assert!(
        error.starts_with(
            "cannot confirm that session 0badf00d is not running: cannot read the registry: "
        ),
        "{error}"
    );
    assert_eq!(
        check_launch(
            &deps,
            &max,
            &["--resume", U, "--fork-session"],
            &project(&sb)
        ),
        None
    );
}

/// C2: an account removed from the registry while the TUI runs is still asked: the set of
/// accounts checked only grows during a session. The display follows the registry.
#[test]
fn a_resume_is_checked_against_accounts_removed_from_the_registry() {
    let sb = Sandbox::new();
    let (deps, max, team) = two_accounts(&sb);
    sb.set_agents(
        Some(home_of(&team)),
        &format!(r#"[{{"pid": 21, "kind": "interactive", "sessionId": "{U}", "status": "busy"}}]"#),
    );
    // `team` is unregistered in another terminal.
    sb.register(&[("max", home_of(&max))]);
    let (events, error) = check_launch_events(&deps, &max, &["--resume", U], &project(&sb));
    assert_eq!(
        error.as_deref(),
        Some(
            "session 0badf00d is running in team (pid 21): switch to its terminal \
             (two claudes writing one session overwrite each other)"
        )
    );
    assert_eq!(
        events,
        [Event::Accounts(vec![
            Account::default_for(Provider::Claude),
            max.clone()
        ])]
    );
    // Once the TUI shows the new registry, `team` is still remembered as seen (and there is
    // nothing more to tell).
    let error = check_launch(&deps, &max, &["--resume", U], &project(&sb));
    assert!(error.unwrap().contains("running in team"));
}

// --- the registry changes while the TUI runs (R16) -----------------------------------------

fn request(account: &Account, args: &[&str], cwd: Option<&Path>) -> LaunchRequest {
    LaunchRequest {
        account: account.clone(),
        args: args.iter().map(|a| a.to_string()).collect(),
        cwd: cwd.map(Path::to_path_buf),
        what: "test".into(),
    }
}

/// R16: an account that `remuda remove` took out of the registry in another terminal starts
/// no new session and no fork: the check before the launch reads the registry again, says
/// why, and tells the TUI once.
#[test]
fn an_account_removed_elsewhere_starts_no_new_session_and_no_fork() {
    let sb = Sandbox::new();
    let (deps, max, team) = two_accounts(&sb);
    let dir = project(&sb);
    sb.register(&[("max", home_of(&max))]);
    let listed = vec![Account::default_for(Provider::Claude), max.clone()];

    let (events, error) = check_launch_events(&deps, &team, &["-n", "x"], &dir);
    assert_eq!(error.as_deref(), Some("team is no longer registered"));
    assert_eq!(events, [Event::Accounts(listed)], "told once");
    let fork = ["--resume", U, "--fork-session"];
    let (events, error) = check_launch_events(&deps, &team, &fork, &dir);
    assert_eq!(error.as_deref(), Some("team is no longer registered"));
    assert_eq!(events, [], "the TUI already knows");
    // A resume in place is refused the same way, before any account is asked.
    assert_eq!(
        check_launch(&deps, &team, &["--resume", U], &dir).as_deref(),
        Some("team is no longer registered")
    );
    assert!(sb.invocations().is_empty(), "{:?}", sb.invocations());
    // A missing directory is still reported first.
    let gone = sb.root().join("gone");
    assert_eq!(
        check_launch(&deps, &team, &["-n", "x"], &gone),
        Some(format!("{} does not exist", gone.display()))
    );
    // The accounts still listed start as before.
    assert_eq!(check_launch(&deps, &max, &["-n", "x"], &dir), None);
    assert_eq!(check_launch(&deps, &max, &fork, &dir), None);
}

/// R16, R2: the same name registered again with another home is another account: a launch
/// that still holds the old home is refused.
#[test]
fn an_account_registered_again_with_another_home_is_not_launched_with_the_old_one() {
    let sb = Sandbox::new();
    let (deps, max, team) = two_accounts(&sb);
    let moved = sb.make_claude_home("q/team");
    sb.register(&[("max", home_of(&max)), ("team", &moved)]);
    assert_eq!(
        check_launch_events(&deps, &team, &["-n", "x"], &project(&sb)).1,
        Some("team is now registered with another home".to_string())
    );
    let mut screen = FakeScreen::default();
    let event =
        tui::launch_in_foreground(&mut screen, &deps, request(&team, &[], None), &quiet()).unwrap();
    assert_eq!(
        exit_of(&event),
        &Err("team is now registered with another home".to_string())
    );
    assert!(sb.invocations().is_empty(), "{:?}", sb.invocations());
}

/// R16: the launch itself reads the registry once more, whatever kind it is (an attach has
/// no check before it; an account can go while a check runs): nothing is handed over, run or
/// logged for an account no longer listed, and the TUI is told once.
#[test]
fn a_launch_reads_the_registry_again_and_runs_nothing_for_an_account_gone() {
    let sb = Sandbox::new();
    let (deps, max, team) = two_accounts(&sb);
    let dir = project(&sb);
    sb.register(&[("max", home_of(&max))]);
    let (tx, rx) = mpsc::channel();
    let kinds: [&[&str]; 4] = [
        &["-n", "x"],
        &["--resume", U, "--fork-session"],
        &["--resume", U],
        &["attach", "0badf00d"],
    ];
    for args in kinds {
        let mut screen = FakeScreen::default();
        let request = request(&team, args, Some(&dir));
        let event = tui::launch_in_foreground(&mut screen, &deps, request, &tx).unwrap();
        assert_eq!(
            exit_of(&event),
            &Err("team is no longer registered".to_string()),
            "{args:?}"
        );
        assert!(screen.calls.is_empty(), "{args:?}: the TUI stays");
    }
    assert!(sb.invocations().is_empty(), "{:?}", sb.invocations());
    assert!(!sb.launch_log().exists(), "nothing was logged");
    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        [Event::Accounts(vec![
            Account::default_for(Provider::Claude),
            max.clone()
        ])]
    );

    // An account still listed runs, in its home as the registry has it (R2).
    let mut screen = FakeScreen::default();
    let request = request(&max, &["-n", "x"], Some(&dir));
    let event = tui::launch_in_foreground(&mut screen, &deps, request, &tx).unwrap();
    assert_eq!(exit_of(&event), &Ok(Exit::Code(0)));
    assert_eq!(screen.calls, ["suspend", "resume"]);
    let inv = sb.only_invocation();
    assert_eq!(inv.config_dir.as_deref(), home_of(&max).to_str());
    assert_eq!(rx.try_iter().count(), 0, "nothing new to tell");
}

/// R16: a setup from the TUI reads the registry again: the new account is told before the
/// setup's own result is.
#[test]
fn a_setup_from_the_tui_tells_the_new_account() {
    let sb = Sandbox::new();
    let deps = Deps {
        env: [
            ("HOME".to_string(), sb.home().display().to_string()),
            (
                "REMUDA_HOME".to_string(),
                sb.remuda_home().display().to_string(),
            ),
        ]
        .into(),
        ..deps(&sb)
    };
    let (tx, rx) = mpsc::channel();
    let mut screen = FakeScreen::default();
    tui::setup_in_foreground(&mut screen, &deps, Provider::Claude, "work", None, &tx).unwrap();
    let home = sb.remuda_home().join("homes/claude/work");
    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        [Event::Accounts(vec![
            Account::default_for(Provider::Claude),
            named("work", home.to_str().unwrap())
        ])],
        "in the channel when the setup returns"
    );
    // A setup that registers nothing tells nothing.
    let mut screen = FakeScreen::default();
    let Event::SetupDone { result, .. } =
        tui::setup_in_foreground(&mut screen, &deps, Provider::Claude, "work", None, &tx).unwrap()
    else {
        panic!()
    };
    assert!(result.is_err());
    assert_eq!(rx.try_iter().count(), 0);
}

use remuda::tui::app::{self, App, Key, Mode, Overlay};

/// The event loop without a terminal: events are applied in order, background effects run on
/// their workers, and a launch runs on a [`FakeScreen`].
struct Headless {
    app: App,
    deps: Arc<Deps>,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    screen: FakeScreen,
    /// Every `Accounts` event applied.
    told: Vec<Vec<Account>>,
}

impl Headless {
    fn start(sb: &Sandbox, deps: Deps) -> Self {
        let (tx, rx) = mpsc::channel();
        let accounts = deps.listing.read(&tx).accounts;
        let mut app = App::new(accounts, deps.tz.clone(), None, jiff::Timestamp::now());
        app.cwd = Some(sb.work());
        Headless {
            app,
            deps: Arc::new(deps),
            tx,
            rx,
            screen: FakeScreen::default(),
            told: Vec::new(),
        }
    }

    /// Applies `event` and starts what it asks for, which is returned.
    fn apply(&mut self, event: Event) -> Vec<Effect> {
        if let Event::Accounts(accounts) = &event {
            self.told.push(accounts.clone());
        }
        let effects = app::update(&mut self.app, event);
        self.dispatch(effects.clone());
        effects
    }

    fn dispatch(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Launch(request) => {
                    let event =
                        tui::launch_in_foreground(&mut self.screen, &self.deps, request, &self.tx)
                            .unwrap();
                    self.tx.send(event).unwrap();
                }
                other => workers::spawn(other, &self.deps, &self.tx),
            }
        }
    }

    fn keys(&mut self, keys: &[Key]) {
        for key in keys {
            self.apply(Event::Key(*key));
        }
    }

    /// Applies what the workers send until `done`.
    fn run_until(&mut self, done: impl Fn(&App) -> bool) {
        while !done(&self.app) {
            let event = self
                .rx
                .recv_timeout(std::time::Duration::from_secs(20))
                .expect("the app gets there");
            self.apply(event);
        }
    }

    fn rows(&self) -> Vec<String> {
        self.app
            .accounts
            .iter()
            .map(|a| a.account.name.clone())
            .collect()
    }
}

/// R16, without a terminal: the registry loses an account while the TUI shows it; the new
/// session started for it reads the registry, one `Accounts` event takes its row away, and
/// the form says why nothing was launched. An account still listed launches as before.
#[test]
fn a_registry_change_reaches_the_app_and_the_account_gone_does_not_launch() {
    let sb = Sandbox::new();
    let (deps, max, team) = two_accounts(&sb);
    let mut tui = Headless::start(&sb, deps);
    assert_eq!(tui.rows(), ["default", "max", "team"]);
    // `n` on team opens its form; meanwhile `remuda remove team` runs in another terminal.
    tui.keys(&[Key::Down, Key::Down, Key::Char('n')]);
    sb.register(&[("max", home_of(&max))]);
    tui.keys(&[Key::Enter]);
    assert!(tui.app.pending.is_some(), "the launch waits for its check");
    tui.run_until(|app| app.pending.is_none());

    assert_eq!(
        tui.told,
        [vec![Account::default_for(Provider::Claude), max.clone()]],
        "one event"
    );
    assert_eq!(tui.rows(), ["default", "max"]);
    let Some(Overlay::Form(form)) = &tui.app.overlay else {
        panic!("{:?}", tui.app.overlay)
    };
    assert_eq!(
        form.error.as_ref().map(|e| e.as_str()),
        Some("team is no longer registered")
    );
    assert!(
        tui.screen.calls.is_empty(),
        "the terminal was not handed over"
    );
    assert!(!sb.launch_log().exists(), "nothing was launched");
    assert!(sb.invocations_with(Some(home_of(&team))).is_empty());

    // The row the selection is on now is max: its new session starts.
    tui.keys(&[Key::Esc, Key::Char('n'), Key::Enter]);
    tui.run_until(|_| sb.launch_log().exists());
    assert_eq!(tui.screen.calls, ["suspend", "resume"]);
    let log = sb.launches();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0]["account"], json!("claude:max"));
    let launched: Vec<_> = sb
        .invocations_with(Some(home_of(&max)))
        .into_iter()
        .filter(|inv| inv.args.first().map(String::as_str) == Some("--session-id"))
        .collect();
    assert_eq!(launched.len(), 1, "{:?}", sb.invocations());
    assert_eq!(tui.told.len(), 1, "nothing more to tell");
}

/// R16, R2, without a terminal: a new-session form is open for an account when another
/// `remuda` registers its name again with another home, and the running-sessions collection
/// the TUI repeats brings the new list to the screen. Enter then starts nothing: the form
/// holds the account it was opened for, which is not the one of that name any more.
#[test]
fn a_form_open_while_its_account_moves_to_another_home_starts_nothing() {
    let sb = Sandbox::new();
    let (deps, max, team) = two_accounts(&sb);
    let mut tui = Headless::start(&sb, deps);
    tui.keys(&[Key::Down, Key::Down, Key::Char('n')]);
    let moved = sb.make_claude_home("q/team");
    sb.set_agents(Some(&moved), "[]");
    sb.register(&[("max", home_of(&max)), ("team", &moved)]);
    workers::spawn(Effect::Live, &tui.deps, &tui.tx);
    tui.run_until(|app| app.live_loaded);
    let moved = named("team", moved.to_str().unwrap());
    assert_eq!(
        tui.told,
        [vec![
            Account::default_for(Provider::Claude),
            max.clone(),
            moved.clone()
        ]]
    );
    assert_eq!(tui.app.accounts[2].account, moved, "the row is the new one");

    assert_eq!(tui.apply(Event::Key(Key::Enter)), [], "nothing is started");
    assert_eq!(tui.app.pending, None);
    let Some(Overlay::Form(form)) = &tui.app.overlay else {
        panic!("{:?}", tui.app.overlay)
    };
    assert_eq!(
        form.error.as_ref().map(|e| e.as_str()),
        Some("team is now registered with another home")
    );
    assert!(
        tui.screen.calls.is_empty(),
        "the terminal was not handed over"
    );
    assert!(!sb.launch_log().exists(), "nothing was launched");
    for home in [home_of(&team), home_of(&moved)] {
        let launched = sb
            .invocations_with(Some(home))
            .into_iter()
            .any(|inv| inv.args.first().map(String::as_str) == Some("--session-id"));
        assert!(!launched, "{:?}", sb.invocations());
    }
}

/// R16, without a terminal: `r` reads the registry again although the identities, usage and
/// checks it would start are still counted as running: their answers are queued behind the
/// key, and nothing else repeats while an account is chosen for `remuda run`.
#[test]
fn a_refresh_reads_the_registry_while_earlier_answers_are_still_queued() {
    let sb = Sandbox::new();
    let (deps, max, _) = two_accounts(&sb);
    let mut tui = Headless::start(&sb, deps);
    tui.app.mode = Mode::PickForRun;
    let started = tui.app.start();
    assert_eq!(
        started,
        [Effect::Identities, Effect::CachedUsage, Effect::Checks]
    );
    tui.dispatch(started);
    // Every answer of the old list is in the queue, none applied: three identities, three
    // cached usages, the checks.
    let queued: Vec<Event> = (0..7)
        .map(|_| {
            tui.rx
                .recv_timeout(std::time::Duration::from_secs(20))
                .unwrap()
        })
        .collect();
    assert!(
        queued.iter().all(|e| !matches!(e, Event::Accounts(_))),
        "{queued:?}"
    );
    // `remuda remove team` in another terminal, then `r`: the key is applied first.
    sb.register(&[("max", home_of(&max))]);
    assert_eq!(
        tui.apply(Event::Key(Key::Char('r'))),
        [Effect::ReadAccounts]
    );
    for event in queued {
        assert_eq!(tui.apply(event), [], "an old answer starts nothing");
    }
    tui.run_until(|app| app.accounts.len() == 2);
    assert_eq!(tui.rows(), ["default", "max"]);
    assert_eq!(
        tui.told,
        [vec![Account::default_for(Provider::Claude), max]],
        "one event"
    );
}

/// R17: codex has no running-session source: a codex resume is not checked against claude's
/// (the TUI asks the user instead); only its directory is checked.
#[test]
fn a_codex_resume_only_has_its_directory_checked() {
    let sb = Sandbox::new();
    let (_, max, team) = two_accounts(&sb);
    register(&sb, &[&max, &team, &codex_work()]);
    let deps = deps(&sb);
    let dir = project(&sb);
    let args = ["resume", CODEX_ID, "-C", dir.to_str().unwrap()];
    assert_eq!(check_launch(&deps, &codex_work(), &args, &dir), None);
    let gone = sb.root().join("gone");
    assert_eq!(
        check_launch(&deps, &codex_work(), &args, &gone),
        Some(format!("{} does not exist", gone.display()))
    );
    assert!(sb.invocations().is_empty(), "{:?}", sb.invocations());
}

// --- The default account drops an inherited CLAUDE_CONFIG_DIR on the TUI path ------------

/// Set only in the child test process of [`the_default_account_drops_an_inherited_config_dir`].
const INHERITED: &str = "REMUDA_TEST_INHERITED_CONFIG_DIR";

/// Runs [`inherited_config_dir_is_removed_for_default`] in a child test process whose own
/// environment has `CLAUDE_CONFIG_DIR=/inherited` (the test process's environment is what a
/// launch inherits; setting it here would race the other tests).
#[test]
fn the_default_account_drops_an_inherited_config_dir() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "inherited_config_dir_is_removed_for_default",
            "--ignored",
            "--test-threads=1",
        ])
        .env("CLAUDE_CONFIG_DIR", "/inherited")
        .env(INHERITED, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("1 passed"), "the inner test ran: {stdout}");
}

#[test]
#[ignore = "run in a child process by the_default_account_drops_an_inherited_config_dir"]
fn inherited_config_dir_is_removed_for_default() {
    assert!(
        std::env::var_os(INHERITED).is_some(),
        "only meaningful in the child process started above"
    );
    assert_eq!(
        std::env::var("CLAUDE_CONFIG_DIR").as_deref(),
        Ok("/inherited"),
        "the precondition holds"
    );
    let sb = Sandbox::new();
    let dir = project(&sb);
    let max = named("max", "/p/max");
    register(&sb, &[&max]);
    tui_launch(
        &sb,
        &Account::default_for(Provider::Claude),
        &["--resume", U],
        Some(&dir),
    );
    tui_launch(&sb, &max, &["--resume", U], Some(&dir));
    let invocations = sb.invocations();
    assert_eq!(invocations[0].config_dir, None, "default: removed (R2)");
    assert_eq!(
        invocations[1].config_dir.as_deref(),
        Some("/p/max"),
        "named: overridden"
    );
}
