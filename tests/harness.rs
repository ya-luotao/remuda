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

/// R15, R23: the fake curl is always first on PATH; it records its arguments and standard input
/// exactly and answers from fixtures, never from the network.
#[test]
fn fake_curl_records_argv_and_stdin_and_answers_from_fixtures() {
    use std::io::Write;
    let sb = Sandbox::new();
    assert_eq!(
        sb.path_var().split(':').next(),
        Some(sb.bin().to_str().unwrap())
    );
    let run = |stdin: &str| {
        let mut child = std::process::Command::new(sb.bin().join("curl"))
            .env_clear()
            .env("HOME", sb.home())
            .env("FAKE_CURL_OUT", sb.curl_out())
            .args([
                "-q",
                "-w",
                "\n%{http_code}",
                "-K",
                "-",
                "https://example.invalid/",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("run fake curl");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    // No response fixture: like a curl that cannot connect.
    let out = run("data-binary = \"{\\\"a\\\":\\\"b\\\\\\\\c\\\"}\"\n");
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(
        sb.curl_invocations(),
        [[
            "-q",
            "-w",
            "\n%{http_code}",
            "-K",
            "-",
            "https://example.invalid/"
        ]]
    );
    assert_eq!(sb.jev_request_body(), "{\"a\":\"b\\\\c\"}");

    sb.set_jev_response("{\"ok\": true}");
    let out = run("x");
    assert_eq!(
        (out.status.code(), &out.stdout[..]),
        (Some(0), &b"{\"ok\": true}\n200"[..])
    );
    assert_eq!(sb.curl_stdin(), "x");
    sb.set_jev_status(401);
    assert_eq!(run("").stdout, b"{\"ok\": true}\n401");
    sb.set_curl_exit(28);
    assert_eq!(run("").status.code(), Some(28));
    assert_eq!(sb.curl_invocations().len(), 4);
}
