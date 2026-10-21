//! R15: the sandbox itself, and the first command through it.

mod common;

use common::Sandbox;

#[test]
fn version_runs_through_sandbox() {
    let sb = Sandbox::new();
    sb.remuda()
        .arg("--version")
        .assert()
        .success()
        .stdout(format!("remuda {}\n", env!("CARGO_PKG_VERSION")));
}

#[test]
fn fake_claude_records_argv_env_and_cwd_exactly() {
    let sb = Sandbox::new();
    let status = std::process::Command::new(sb.bin().join("claude"))
        .env_clear()
        .env("PATH", sb.path_var())
        .env("FAKE_CLAUDE_OUT", sb.claude_out())
        .env("FAKE_CLAUDE_EXIT", "7")
        .env("CLAUDE_CONFIG_DIR", "/x/with space/")
        .current_dir(sb.work())
        .args(["-p", "hi there", "two\nlines", "", "100%s", "arg=x"])
        .status()
        .expect("run fake claude");
    assert_eq!(status.code(), Some(7));

    let inv = sb.only_invocation();
    assert_eq!(
        inv.args,
        ["-p", "hi there", "two\nlines", "", "100%s", "arg=x"]
    );
    assert_eq!(inv.config_dir.as_deref(), Some("/x/with space/"));
    assert_eq!(inv.securestorage_dir, None);
    assert_eq!(inv.add_dir_claude_md, None);
    assert_eq!(inv.cwd, sb.work().canonicalize().unwrap());
}

#[test]
fn fake_claude_distinguishes_empty_from_unset() {
    let sb = Sandbox::new();
    for _ in 0..2 {
        std::process::Command::new(sb.bin().join("claude"))
            .env_clear()
            .env("FAKE_CLAUDE_OUT", sb.claude_out())
            .env("CLAUDE_SECURESTORAGE_CONFIG_DIR", "")
            .env("CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD", "1")
            .current_dir(sb.work())
            .status()
            .expect("run fake claude");
    }
    let all = sb.invocations();
    assert_eq!(all.len(), 2);
    for inv in all {
        assert_eq!(inv.config_dir, None);
        assert_eq!(inv.securestorage_dir.as_deref(), Some(""));
        assert_eq!(inv.add_dir_claude_md.as_deref(), Some("1"));
        assert!(inv.args.is_empty());
    }
}

#[test]
fn fake_claude_serves_fixtures_per_account() {
    let sb = Sandbox::new();
    let max = sb.root().join("max");
    std::fs::create_dir(&max).unwrap();
    sb.set_auth(Some(&max), "{\"email\":\"max\"}");
    sb.set_auth(None, "{\"email\":\"native\"}");
    let run = |config_dir: Option<&std::path::Path>, args: &[&str]| {
        let mut cmd = std::process::Command::new(sb.bin().join("claude"));
        cmd.env_clear().env("HOME", sb.home()).args(args);
        if let Some(dir) = config_dir {
            cmd.env("CLAUDE_CONFIG_DIR", dir);
        }
        cmd.output().expect("run fake claude")
    };
    let out = run(Some(&max), &["auth", "status", "--json"]);
    assert_eq!(
        (out.status.code(), &out.stdout[..]),
        (Some(0), &b"{\"email\":\"max\"}"[..])
    );
    let out = run(None, &["auth", "status", "--json"]);
    assert_eq!(out.stdout, b"{\"email\":\"native\"}");
    // No usage fixture: exit 1.
    assert_eq!(run(Some(&max), &["-p", "/usage"]).status.code(), Some(1));
}

#[test]
fn parallel_fake_invocations_do_not_interleave() {
    let sb = Sandbox::new();
    let long = "x".repeat(2000);
    let children: Vec<_> = (0..12)
        .map(|i| {
            std::process::Command::new(sb.bin().join("claude"))
                .env_clear()
                .env("FAKE_CLAUDE_OUT", sb.claude_out())
                .env("CLAUDE_CONFIG_DIR", format!("/cfg/{i}"))
                .args([format!("{i}"), long.clone(), format!("end{i}")])
                .spawn()
                .expect("spawn fake claude")
        })
        .collect();
    for mut c in children {
        c.wait().unwrap();
    }
    let all = sb.invocations();
    assert_eq!(all.len(), 12);
    for inv in all {
        let i = inv.args[0].clone();
        assert_eq!(inv.config_dir, Some(format!("/cfg/{i}")));
        assert_eq!(inv.args, [i.clone(), long.clone(), format!("end{i}")]);
    }
}
