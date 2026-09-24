//! Running short, non-interactive claude commands for an account: output captured, stdin
//! closed, bounded by a timeout (R7, R10, R10a).

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::launch::{self, EnvChange};

const POLL: Duration = Duration::from_millis(10);
/// How long to wait for output after the process exited, at minimum.
const GRACE: Duration = Duration::from_millis(100);

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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use super::*;

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

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

    #[test]
    fn parallel_keeps_input_order() {
        let out = parallel(&[30u64, 0, 15], |ms| {
            thread::sleep(Duration::from_millis(*ms));
            *ms
        });
        assert_eq!(out, [30, 0, 15]);
    }
}
