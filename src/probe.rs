//! Running short, non-interactive agent commands for an account: output captured, stdin
//! closed (or, for JSON-RPC over stdio, held open until the answers arrived), bounded by a
//! timeout (R4, R7, R10, R10a).

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::launch::{self, EnvChange};

const POLL: Duration = Duration::from_millis(10);
/// How long to wait for output after the process exited, at minimum.
const GRACE: Duration = Duration::from_millis(100);
/// How long a JSON-RPC server gets to exit after its stdin closed, before it is killed.
const EXIT_GRACE: Duration = Duration::from_secs(2);
/// How long a JSON-RPC server's process group gets to go after SIGTERM, before SIGKILL.
const TERM_GRACE: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// `code` is `None` when the process was killed by a signal.
    Exited {
        code: Option<i32>,
        stdout: String,
        stderr: String,
    },
    TimedOut,
    SpawnFailed(String),
}

impl Outcome {
    /// Stdout of a run that exited with status 0.
    pub fn success_stdout(&self) -> Option<&str> {
        match self {
            Outcome::Exited {
                code: Some(0),
                stdout,
                ..
            } => Some(stdout),
            _ => None,
        }
    }

    /// Why the run did not succeed, e.g. `timed out after 15s`; for a successful run, `succeeded`.
    pub fn describe(&self, timeout: Duration) -> String {
        match self {
            Outcome::TimedOut => format!("timed out after {timeout:?}"),
            Outcome::SpawnFailed(e) => format!("could not be started: {e}"),
            Outcome::Exited { code: None, .. } => "was killed by a signal".to_string(),
            Outcome::Exited { code: Some(0), .. } => "succeeded".to_string(),
            Outcome::Exited {
                code: Some(code),
                stderr,
                ..
            } => match stderr.lines().map(str::trim).find(|l| !l.is_empty()) {
                Some(line) => format!("exited with status {code}: {line}"),
                None => format!("exited with status {code}"),
            },
        }
    }
}

/// Runs `program args...` with the inherited environment plus `change`, stdin closed and
/// stdout/stderr captured. A run exceeding `timeout` is killed.
pub fn run_captured(
    program: &Path,
    args: &[&str],
    change: &EnvChange,
    timeout: Duration,
) -> Outcome {
    run_captured_with(program, args, std::slice::from_ref(change), timeout)
}

/// [`run_captured`] with several environment changes, applied in order.
pub fn run_captured_with(
    program: &Path,
    args: &[&str],
    changes: &[EnvChange],
    timeout: Duration,
) -> Outcome {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for change in changes {
        launch::apply_env(&mut cmd, change);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return Outcome::SpawnFailed(e.to_string()),
    };
    let stdout = read_in_background(child.stdout.take());
    let stderr = read_in_background(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL),
            other => {
                // Timed out, or waiting failed. The reader threads are left behind rather than
                // joined: a grandchild may still hold the pipes open.
                let _ = child.kill();
                let _ = child.wait();
                return match other {
                    Err(e) => Outcome::SpawnFailed(e.to_string()),
                    _ => Outcome::TimedOut,
                };
            }
        }
    };
    let remaining = || {
        deadline
            .saturating_duration_since(Instant::now())
            .max(GRACE)
    };
    match (
        stdout.recv_timeout(remaining()),
        stderr.recv_timeout(remaining()),
    ) {
        (Ok(out), Ok(err)) => Outcome::Exited {
            code: status.code(),
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
        },
        _ => Outcome::TimedOut,
    }
}

/// JSON-RPC over stdio (R4): runs `program args...` with `change`, writes `lines` (each + "\n",
/// one write, then flush) and KEEPS STDIN OPEN until a response for every id in `ids` arrived
/// (codex app-server exits on stdin EOF without answering pending requests). Stdout is read line
/// by line on a thread; lines that are not JSON objects with a numeric `id` in `ids` and a
/// `result` or `error` are skipped (notifications, logs). Then stdin is closed, the process gets
/// up to EXIT_GRACE to exit, and is killed after that. Returns each id's whole response object.
pub fn run_json_rpc(
    program: &Path,
    args: &[&str],
    change: &EnvChange,
    lines: &[String],
    ids: &[u64],
    timeout: Duration,
) -> Result<HashMap<u64, Value>, String> {
    let deadline = Instant::now() + timeout;
    let mut cmd = Command::new(program);
    // Its own process group, so that its children go with it: codex installed through npm or
    // bun is a node shim whose child is the real server.
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    launch::apply_env(&mut cmd, change);
    let mut child = Reaper(
        cmd.spawn()
            .map_err(|e| format!("could not be started: {e}"))?,
    );
    let stdout = lines_in_background(child.0.stdout.take());
    let stderr = read_in_background(child.0.stderr.take());

    let mut stdin = child.0.stdin.take();
    if let Some(pipe) = &mut stdin {
        // A few short requests fit in the pipe's buffer: the write does not block. Errors are
        // ignored (SIGPIPE is ignored, so a process that already exited gives EPIPE): reading
        // says why it went away.
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let _ = pipe.write_all(text.as_bytes());
        let _ = pipe.flush();
    }

    let wanted: HashSet<u64> = ids.iter().copied().collect();
    let mut answers = HashMap::new();
    while answers.len() < wanted.len() {
        let left = deadline.saturating_duration_since(Instant::now());
        match stdout.recv_timeout(left) {
            Ok(line) => {
                if let Some((id, response)) = response(&line, &wanted) {
                    answers.insert(id, response);
                }
            }
            Err(RecvTimeoutError::Timeout) => return Err(format!("timed out after {timeout:?}")),
            Err(RecvTimeoutError::Disconnected) => {
                drop(stdin);
                let left = deadline.saturating_duration_since(Instant::now());
                let status = wait_until(&mut child.0, Instant::now() + left.min(EXIT_GRACE));
                let line = stderr.recv_timeout(GRACE).ok().and_then(|err| {
                    String::from_utf8_lossy(&err)
                        .lines()
                        .map(str::trim)
                        .find(|l| !l.is_empty())
                        .map(str::to_string)
                });
                return Err(match (status.and_then(|s| s.code()), line) {
                    (Some(0), _) => "exited before answering".to_string(),
                    (Some(code), Some(line)) => format!("exited with status {code}: {line}"),
                    (Some(code), None) => format!("exited with status {code}"),
                    (None, _) => "was killed by a signal".to_string(),
                });
            }
        }
    }
    drop(stdin);
    wait_until(&mut child.0, Instant::now() + EXIT_GRACE);
    Ok(answers)
}

/// A line of stdout that answers one of `ids`: `(id, the whole response)`.
fn response(line: &str, ids: &HashSet<u64>) -> Option<(u64, Value)> {
    let value: Value = serde_json::from_str(line).ok()?;
    let id = value.get("id")?.as_u64()?;
    let answered = value.get("result").is_some() || value.get("error").is_some();
    (ids.contains(&id) && answered).then_some((id, value))
}

/// A child leading its own process group that is terminated with the group and reaped when
/// dropped, so that no path out of [`run_json_rpc`] (a timeout, an early exit, an error, or a
/// server that does not exit after its stdin closed) leaves it, or a process it started,
/// running. Killing and waiting for a child that was already reaped does nothing.
struct Reaper(Child);

impl Drop for Reaper {
    fn drop(&mut self) {
        terminate_group(&mut self.0);
    }
}

/// SIGTERM to `child`'s process group (its id: it leads the group), up to [`TERM_GRACE`] for
/// every member to go, then SIGKILL to what is left; the direct child is killed and reaped
/// either way. An empty group is not signalled again.
fn terminate_group(child: &mut Child) {
    if let Ok(pgid) = libc::pid_t::try_from(child.id())
        && signal_group(pgid, libc::SIGTERM)
    {
        let until = Instant::now() + TERM_GRACE;
        let mut alive = true;
        while alive && Instant::now() < until {
            thread::sleep(POLL);
            // Reaps the direct child once it exited: a zombie still counts as a member.
            let _ = child.try_wait();
            alive = signal_group(pgid, 0);
        }
        if alive {
            signal_group(pgid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// `kill(-pgid, signal)`: whether the group had a member to signal.
fn signal_group(pgid: libc::pid_t, signal: libc::c_int) -> bool {
    // SAFETY: kill(2) takes no pointers; a negative pid names the process group.
    unsafe { libc::kill(-pgid, signal) == 0 }
}

/// Waits for `child` to exit until `until`; `None` if it is still running (or cannot be waited
/// for).
fn wait_until(child: &mut Child, until: Instant) -> Option<ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < until => thread::sleep(POLL),
            _ => return None,
        }
    }
}

/// Each line of `pipe` as it arrives; the channel disconnects at EOF. The thread is left behind
/// when nobody waits for it, as in [`run_captured`]: a grandchild may hold the pipe open.
fn lines_in_background<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let Some(pipe) = pipe else {
            return;
        };
        for line in BufReader::new(pipe).lines() {
            let Ok(line) = line else {
                return;
            };
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    rx
}

fn read_in_background<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    rx
}

/// Maps `f` over `items` on one thread per item; results keep the order of `items`.
pub fn parallel<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    thread::scope(|scope| {
        let handles: Vec<_> = items.iter().map(|item| scope.spawn(|| f(item))).collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("probe thread panicked"))
            .collect()
    })
}

/// A test script at `dir/name`: `body` under `#!/bin/sh`. Written through a `sh` child, so
/// this process never holds a write descriptor on the script: on Linux, a child forked by
/// another test thread inherits such a descriptor until it execs, and running the script
/// meanwhile fails with ETXTBSY.
#[cfg(test)]
pub(crate) fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    let mut child = Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(&path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let text = format!("#!/bin/sh\n{body}\n");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keep() -> EnvChange {
        EnvChange::Set("REMUDA_PROBE_TEST".into(), "1".into())
    }

    #[test]
    fn captures_output_and_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(
            dir.path(),
            "s",
            "echo out; echo \"$@\"; echo err >&2; exit 4",
        );
        let outcome = run_captured(&p, &["a b", "c"], &keep(), Duration::from_secs(10));
        assert_eq!(
            outcome,
            Outcome::Exited {
                code: Some(4),
                stdout: "out\na b c\n".into(),
                stderr: "err\n".into()
            }
        );
        assert_eq!(outcome.success_stdout(), None);
        assert_eq!(
            outcome.describe(Duration::from_secs(1)),
            "exited with status 4: err"
        );
    }

    #[test]
    fn applies_env_change() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(
            dir.path(),
            "s",
            "printf '%s' \"${CLAUDE_CONFIG_DIR-unset}\"",
        );
        let t = Duration::from_secs(10);
        let set = EnvChange::Set("CLAUDE_CONFIG_DIR".into(), "/h/x/".into());
        assert_eq!(
            run_captured(&p, &[], &set, t).success_stdout(),
            Some("/h/x/")
        );
        let remove = EnvChange::Remove("CLAUDE_CONFIG_DIR".into());
        assert_eq!(
            run_captured(&p, &[], &remove, t).success_stdout(),
            Some("unset")
        );
    }

    #[test]
    fn stdin_is_closed() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(dir.path(), "s", "cat; echo done");
        let outcome = run_captured(&p, &[], &keep(), Duration::from_secs(10));
        assert_eq!(outcome.success_stdout(), Some("done\n"));
    }

    #[test]
    fn kills_on_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(dir.path(), "s", "exec sleep 30");
        let start = Instant::now();
        let outcome = run_captured(&p, &[], &keep(), Duration::from_millis(200));
        assert_eq!(outcome, Outcome::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_eq!(
            outcome.describe(Duration::from_millis(200)),
            "timed out after 200ms"
        );
    }

    #[test]
    fn missing_program_is_a_spawn_failure() {
        let outcome = run_captured(
            Path::new("/nonexistent/claude"),
            &[],
            &keep(),
            Duration::from_secs(1),
        );
        assert!(matches!(outcome, Outcome::SpawnFailed(_)), "{outcome:?}");
    }

    fn rpc(
        body: &str,
        lines: &[&str],
        ids: &[u64],
        timeout: Duration,
    ) -> Result<HashMap<u64, Value>, String> {
        let dir = tempfile::tempdir().unwrap();
        let p = script(dir.path(), "s", body);
        let lines: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        run_json_rpc(&p, &[], &keep(), &lines, ids, timeout)
    }

    /// R4: responses are matched by id, in any order, past notifications and noise.
    #[test]
    fn json_rpc_matches_responses_by_id() {
        let start = Instant::now();
        let answers = rpc(
            "read a; read b; read c; echo noise; echo '{\"method\":\"n\",\"params\":{}}'; \
             echo '{\"id\":3,\"result\":{\"n\":3}}'; \
             echo '{\"id\":2,\"error\":{\"code\":-1,\"message\":\"no\"}}'; cat >/dev/null",
            &["{\"id\":1}", "{}", "{\"id\":2}"],
            &[2, 3],
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(
            answers,
            HashMap::from([
                (3, serde_json::json!({"id": 3, "result": {"n": 3}})),
                (
                    2,
                    serde_json::json!({"id": 2, "error": {"code": -1, "message": "no"}})
                ),
            ])
        );
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    /// R4: like codex app-server, the script dies as soon as its stdin closes, without
    /// answering: stdin must stay open until the answer arrived. (A background job's stdin is
    /// `/dev/null` in `sh`, even for `3<&0` on the job; it gets the real one through fd 3.)
    #[test]
    fn json_rpc_keeps_stdin_open_until_answered() {
        let answers = rpc(
            "exec 3<&0; read a; (cat <&3 >/dev/null; kill $$) & sleep 0.3; \
             echo '{\"id\":1,\"result\":1}'; wait",
            &["{\"id\":1}"],
            &[1],
            Duration::from_secs(10),
        );
        assert_eq!(
            answers,
            Ok(HashMap::from([(
                1,
                serde_json::json!({"id": 1, "result": 1})
            )]))
        );
    }

    #[test]
    fn json_rpc_times_out_and_kills() {
        let start = Instant::now();
        let result = rpc("exec sleep 30", &["{}"], &[1], Duration::from_millis(200));
        assert_eq!(result, Err("timed out after 200ms".to_string()));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn json_rpc_reports_an_early_exit() {
        let t = Duration::from_secs(10);
        assert_eq!(
            rpc(
                "echo 'unrecognized subcommand' >&2; exit 2",
                &["{}"],
                &[1],
                t
            ),
            Err("exited with status 2: unrecognized subcommand".to_string())
        );
        assert_eq!(
            rpc("exit 3", &["{}"], &[1], t),
            Err("exited with status 3".to_string())
        );
        assert_eq!(
            rpc("exit 0", &["{}"], &[1], t),
            Err("exited before answering".to_string())
        );
        assert_eq!(
            rpc("kill -9 $$", &["{}"], &[1], t),
            Err("was killed by a signal".to_string())
        );
        // One of two answered: still an early exit.
        assert_eq!(
            rpc(
                "echo '{\"id\":1,\"result\":1}'; exit 0",
                &["{}"],
                &[1, 2],
                t
            ),
            Err("exited before answering".to_string())
        );
    }

    /// Whether process `pid` is gone, waiting up to 5 s for it to be (a killed orphan is
    /// reaped by init).
    fn gone(pid: libc::pid_t) -> bool {
        let until = Instant::now() + Duration::from_secs(5);
        // SAFETY: kill(2) with signal 0 only checks that the process exists.
        while unsafe { libc::kill(pid, 0) } == 0 {
            if Instant::now() >= until {
                return false;
            }
            thread::sleep(POLL);
        }
        true
    }

    /// The pid a script wrote to `path`.
    fn pid_in(path: &Path) -> libc::pid_t {
        std::fs::read_to_string(path)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    /// The server's process group goes with it: a process it started is terminated on a
    /// timeout, and one that ignores SIGTERM is killed after the grace. (The timeout leaves the
    /// script time to write both pids, even on a loaded machine running the suite in parallel.)
    #[test]
    fn json_rpc_kills_the_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(
            dir.path(),
            "s",
            "sleep 30 & echo $! > \"$1/plain\"; (trap '' TERM; exec sleep 30) & \
             echo $! > \"$1/stubborn\"; exec sleep 30",
        );
        let d = dir.path().to_str().unwrap();
        let start = Instant::now();
        let result = run_json_rpc(&p, &[d], &keep(), &[], &[1], Duration::from_secs(5));
        assert_eq!(result, Err("timed out after 5s".to_string()));
        assert!(start.elapsed() < Duration::from_secs(10));
        for name in ["plain", "stubborn"] {
            assert!(gone(pid_in(&dir.path().join(name))), "{name} survived");
        }
    }

    /// After an answer, a process the server left behind is terminated too.
    #[test]
    fn json_rpc_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(
            dir.path(),
            "s",
            "read a; sleep 30 & echo $! > \"$1/left\"; echo '{\"id\":1,\"result\":1}'; \
             cat >/dev/null",
        );
        let d = dir.path().to_str().unwrap();
        let lines = ["{}".to_string()];
        let result = run_json_rpc(&p, &[d], &keep(), &lines, &[1], Duration::from_secs(10));
        assert!(result.is_ok(), "{result:?}");
        assert!(gone(pid_in(&dir.path().join("left"))), "left survived");
    }

    #[test]
    fn json_rpc_spawn_failure() {
        let result = run_json_rpc(
            Path::new("/nonexistent/codex"),
            &[],
            &keep(),
            &[],
            &[1],
            Duration::from_secs(1),
        );
        let e = result.unwrap_err();
        assert!(e.starts_with("could not be started"), "{e}");
    }

    #[test]
    fn json_rpc_applies_env() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(
            dir.path(),
            "s",
            "read a; printf '{\"id\":1,\"result\":\"%s\"}\\n' \"${CODEX_HOME-unset}\"; cat >/dev/null",
        );
        let t = Duration::from_secs(10);
        let lines = ["{}".to_string()];
        let result = |change: &EnvChange| {
            run_json_rpc(&p, &[], change, &lines, &[1], t).unwrap()[&1]["result"].clone()
        };
        let set = EnvChange::Set("CODEX_HOME".into(), "/c/x/".into());
        assert_eq!(result(&set), "/c/x/");
        let remove = EnvChange::Remove("CODEX_HOME".into());
        assert_eq!(result(&remove), "unset");
    }

    #[test]
    fn parallel_keeps_input_order() {
        let out = parallel(&[30u64, 0, 15], |ms| {
            thread::sleep(Duration::from_millis(*ms));
            *ms
        });
        assert_eq!(out, [30, 0, 15]);
    }
}
