//! Running short, non-interactive commands: agent commands for an account, with output
//! captured, stdin closed (or, for JSON-RPC over stdio, held open until the answers arrived),
//! bounded by a timeout (R4, R7, R10, R10a); and `curl` for `remuda pick`'s one request, fed its
//! configuration on stdin (R23). Each runs in its own process group, which is terminated when
//! the command times out; what a command that exited left running is left alone, except by a
//! JSON-RPC server's run, which leaves nothing behind (R4).

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::interrupt::{self, Watched};
use crate::launch::{self, EnvChange};

const POLL: Duration = Duration::from_millis(10);
/// How long the output of a process that exited is waited for at least, when what it wrote
/// has not all been read yet: for a command, until its deadline if that is later.
const GRACE: Duration = Duration::from_millis(100);
/// How long a JSON-RPC server gets to exit after its stdin closed, before it is killed.
const EXIT_GRACE: Duration = Duration::from_secs(2);
/// How long a process group gets to go after SIGTERM, before SIGKILL.
const TERM_GRACE: Duration = Duration::from_millis(500);
/// How much of a pipe is read at a time.
const CHUNK: usize = 64 << 10;

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
            Outcome::TimedOut => timeout_text(timeout),
            Outcome::SpawnFailed(e) => spawn_text(e),
            Outcome::Exited { code: Some(0), .. } => "succeeded".to_string(),
            Outcome::Exited { code, stderr, .. } => exit_text(*code, stderr),
        }
    }

    /// [`Outcome::describe`], with the first line of stdout when a failing run wrote nothing to
    /// stderr: for a command that reports its errors there.
    pub fn describe_or_stdout(&self, timeout: Duration) -> String {
        match self {
            Outcome::Exited {
                code: Some(code),
                stdout,
                stderr,
            } if *code != 0 && first_line(stderr).is_none() => exit_text(Some(*code), stdout),
            _ => self.describe(timeout),
        }
    }
}

/// How a process that exited without succeeding ended, with the first line of what it said:
/// `exited with status 2: <line>`; for `code` `None`, `was killed by a signal`.
fn exit_text(code: Option<i32>, said: &str) -> String {
    match (code, first_line(said)) {
        (None, _) => "was killed by a signal".to_string(),
        (Some(code), Some(line)) => format!("exited with status {code}: {line}"),
        (Some(code), None) => format!("exited with status {code}"),
    }
}

fn timeout_text(timeout: Duration) -> String {
    format!("timed out after {timeout:?}")
}

fn spawn_text(error: &str) -> String {
    format!("could not be started: {error}")
}

/// The first line of `text` that is not blank, trimmed.
fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|l| !l.is_empty())
}

/// Runs `program args...` with the inherited environment plus `change`, stdin closed and
/// stdout/stderr captured. A run exceeding `timeout` is killed, with every process it started
/// (its process group). One that exited is not kept waiting by a process it left behind, and
/// that process is left alone: a helper an agent started is the agent's business.
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
    run(program, args, changes, None, timeout)
}

/// Runs `program args...` with the inherited environment, `stdin` written to its standard input
/// (on a thread, then closed) and stdout/stderr captured, like [`run_captured`]. For `curl -K -`,
/// which reads its configuration, and so the request's secrets, from there (R23).
pub fn run_with_stdin(program: &Path, args: &[&str], stdin: &[u8], timeout: Duration) -> Outcome {
    run(program, args, &[], Some(stdin), timeout)
}

fn run(
    program: &Path,
    args: &[&str],
    changes: &[EnvChange],
    stdin: Option<&[u8]>,
    timeout: Duration,
) -> Outcome {
    let feed = if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    let mut run = match Bounded::start(program, args, changes, feed, timeout) {
        Ok(run) => run,
        Err(e) => return Outcome::SpawnFailed(e.to_string()),
    };
    if let (Some(mut pipe), Some(input)) = (run.child.stdin.take(), stdin) {
        // A thread, so that a child that does not read everything cannot block this one; an
        // error (EPIPE from a child that exited early) shows in its exit status. Dropping the
        // pipe closes it.
        let input = input.to_vec();
        thread::spawn(move || {
            let _ = pipe.write_all(&input);
        });
    }
    let stdout = Capture::of(run.child.stdout.take().map(OwnedFd::from));
    match run.exit_by(run.deadline) {
        Ok(Some(status)) => {
            // What the command wrote is what was written by now, to either pipe: that
            // much is taken, as soon as it was read. A process the command left behind is
            // not waited for, whether it holds the pipes open in silence or goes on writing.
            // (The deadline, one for both pipes, bounds only how far behind the reading
            // may be.)
            let (out, err) = (stdout.mark(), run.stderr.mark());
            let until = run.deadline.max(Instant::now() + GRACE);
            let outcome = exited(
                status,
                stdout.text_to(out, until),
                run.stderr.text_to(err, until),
            );
            if outcome != Outcome::TimedOut {
                run.leave();
            }
            outcome
        }
        // Dropping the run terminates its process group.
        Ok(None) => Outcome::TimedOut,
        Err(e) => Outcome::SpawnFailed(e.to_string()),
    }
}

/// What a command that exited with `status` comes to, given what could be read of each of its
/// streams in time. Output of which a part is missing is not the command's answer, and is not
/// handed on as one: the run then counts as timed out, as it would have before it exited.
fn exited(status: ExitStatus, stdout: Option<String>, stderr: Option<String>) -> Outcome {
    match (stdout, stderr) {
        (Some(stdout), Some(stderr)) => Outcome::Exited {
            code: status.code(),
            stdout,
            stderr,
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
    let changes = std::slice::from_ref(change);
    let mut run = Bounded::start(program, args, changes, Stdio::piped(), timeout)
        .map_err(|e| spawn_text(&e.to_string()))?;
    let deadline = run.deadline;
    let stdout = lines_in_background(run.child.stdout.take());

    let mut stdin = run.child.stdin.take();
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
            Err(RecvTimeoutError::Timeout) => return Err(timeout_text(timeout)),
            Err(RecvTimeoutError::Disconnected) => {
                drop(stdin);
                let until = deadline.min(Instant::now() + EXIT_GRACE);
                let status = run.exit_by(until).ok().flatten();
                let stderr = run.stderr.text_by(Instant::now() + GRACE);
                return Err(match status.and_then(|s| s.code()) {
                    Some(0) => "exited before answering".to_string(),
                    code => exit_text(code, &stderr),
                });
            }
        }
    }
    drop(stdin);
    let _ = run.exit_by(Instant::now() + EXIT_GRACE);
    Ok(answers)
}

/// A line of stdout that answers one of `ids`: `(id, the whole response)`.
fn response(line: &str, ids: &HashSet<u64>) -> Option<(u64, Value)> {
    let value: Value = serde_json::from_str(line).ok()?;
    let id = value.get("id")?.as_u64()?;
    let answered = value.get("result").is_some() || value.get("error").is_some();
    (ids.contains(&id) && answered).then_some((id, value))
}

/// A command with a deadline, leading its own process group: what [`run_captured`],
/// [`run_with_stdin`] and [`run_json_rpc`] all run on. Its own group, so that what it starts
/// goes with it: codex installed through npm or bun is a node shim whose child is the real
/// server. The group is terminated and the child reaped when this is dropped, so that no way out
/// of a run (an answer, a timeout, an early exit, an error, or a server that does not exit after
/// its stdin closed) leaves it, or a process it started, running; only [`Bounded::leave`], for
/// a command that ran to its end, lets what is left of the group be. Killing and waiting for a
/// child that was already reaped does nothing.
///
/// The group is not the terminal's foreground group. The terminal's signals no longer reach
/// it: it is watched for as long, so that they are passed on ([`interrupt::starting`]). And a
/// process that reads the terminal or changes its modes from there is stopped (SIGTTIN,
/// SIGTTOU), which would look like a hang until the timeout: the command ignores both, so the
/// read fails at once and the change goes through.
struct Bounded {
    child: Child,
    deadline: Instant,
    /// Everything the group writes to stderr.
    stderr: Capture,
    _watched: Watched,
    /// The command exited and what it left running stays ([`Bounded::leave`]).
    left: bool,
}

impl Bounded {
    /// Starts `program args...` with the inherited environment plus `changes` and piped
    /// stdout and stderr; `timeout` from now is its deadline.
    fn start(
        program: &Path,
        args: &[&str],
        changes: &[EnvChange],
        stdin: Stdio,
        timeout: Duration,
    ) -> io::Result<Bounded> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        for change in changes {
            launch::apply_env(&mut cmd, change);
        }
        // SAFETY: the closure runs in the forked child before exec and only calls `signal`,
        // which is async-signal-safe. An ignored signal stays ignored across exec.
        unsafe {
            cmd.pre_exec(|| {
                libc::signal(libc::SIGTTIN, libc::SIG_IGN);
                libc::signal(libc::SIGTTOU, libc::SIG_IGN);
                Ok(())
            });
        }
        // The command is remuda's to tell of a signal from here on, before it has a group to
        // be told through: a signal that comes meanwhile reaches it once it runs. This waits
        // while as many commands run as remuda watches at a time, which is not the command's
        // time: its deadline counts from its start.
        let Some(starting) = interrupt::starting() else {
            let why = "remuda is ending by a signal";
            return Err(io::Error::new(io::ErrorKind::Interrupted, why));
        };
        let deadline = Instant::now() + timeout;
        let mut child = cmd.spawn()?;
        let watched = starting.started(child.id());
        let stderr = Capture::of(child.stderr.take().map(OwnedFd::from));
        Ok(Bounded {
            child,
            deadline,
            stderr,
            _watched: watched,
            left: false,
        })
    }

    /// Ends the run of a command that exited, without terminating what it left running in
    /// its group: a helper an agent started on the side (an update, say) is the agent's
    /// business, and killing it costs more than letting it finish. The threads reading a pipe
    /// such a process holds open are left behind with it.
    fn leave(mut self) {
        self.left = true;
    }

    /// The child's exit status, waiting for it until `until`; `None` if it is still running
    /// then.
    fn exit_by(&mut self, until: Instant) -> io::Result<Option<ExitStatus>> {
        loop {
            match self.child.try_wait()? {
                Some(status) => return Ok(Some(status)),
                None if Instant::now() < until => thread::sleep(POLL),
                None => return Ok(None),
            }
        }
    }
}

impl Drop for Bounded {
    fn drop(&mut self) {
        if !self.left {
            terminate_group(&mut self.child);
        }
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

/// Each line of `pipe` as it arrives; the channel disconnects at EOF. The thread ends with the
/// pipe, at the latest when the group holding it is terminated ([`run_json_rpc`] always does).
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

/// A pipe read on a thread, into a buffer that can be taken while the pipe is still open: a
/// process the command started may hold it open after the command itself exited, and may go
/// on writing to it. The thread waits for bytes without the buffer's lock and moves them under
/// it, so whoever holds the lock sees every byte written so far, in the buffer or still in the
/// pipe, however far behind the thread is: that much, and no more, is what [`Capture::mark`]
/// counts and [`Capture::text_to`] waits for.
struct Capture {
    /// Kept open for as long as it is asked about; `None` for a stream that was not piped.
    pipe: Option<Arc<OwnedFd>>,
    read: Arc<Mutex<Vec<u8>>>,
    ended: mpsc::Receiver<()>,
}

impl Capture {
    fn of(pipe: Option<OwnedFd>) -> Capture {
        let pipe = pipe.map(Arc::new);
        let read = Arc::new(Mutex::new(Vec::new()));
        let (tx, ended) = mpsc::channel();
        let (from, into) = (pipe.clone(), Arc::clone(&read));
        thread::spawn(move || {
            if let Some(pipe) = from {
                let mut chunk = vec![0u8; CHUNK];
                while readable(&pipe) {
                    let mut read = into.lock().unwrap_or_else(|e| e.into_inner());
                    // SAFETY: read(2) into `chunk`, which is `chunk.len()` bytes long. It does
                    // not block: the pipe has bytes or has ended, and nobody else reads it.
                    let n = unsafe {
                        libc::read(pipe.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len())
                    };
                    match usize::try_from(n) {
                        Ok(0) => break,
                        // Nobody is left to take it: what a process the command left behind
                        // goes on writing is read, so that it is not held up, and dropped.
                        Ok(_) if Arc::strong_count(&into) == 1 => read.clear(),
                        Ok(n) => read.extend_from_slice(&chunk[..n]),
                        Err(_) if retry(&io::Error::last_os_error()) => {}
                        Err(_) => break,
                    }
                }
            }
            let _ = tx.send(());
        });
        Capture { pipe, read, ended }
    }

    /// How many bytes were written to the pipe and not taken yet, now: the ones in the buffer
    /// and the ones still in the pipe. What a command wrote before it exited is among them,
    /// whatever a process it left behind writes afterwards.
    fn mark(&self) -> usize {
        let read = self.read.lock().unwrap_or_else(|e| e.into_inner());
        read.len() + self.pending()
    }

    /// The first `mark` bytes that were not taken yet, as text, once the thread has read that
    /// far: no wait when it has, and none for anything written after the mark. `None` when it
    /// has not read that far by `until`, or cannot: nothing is taken then, since a part of
    /// what was written is not what was written.
    fn text_to(&self, mark: usize, until: Instant) -> Option<String> {
        loop {
            let ended = !matches!(self.ended.try_recv(), Err(mpsc::TryRecvError::Empty));
            let mut read = self.read.lock().unwrap_or_else(|e| e.into_inner());
            if read.len() >= mark {
                let taken: Vec<u8> = read.drain(..mark).collect();
                return Some(String::from_utf8_lossy(&taken).into_owned());
            }
            if ended || Instant::now() >= until {
                return None;
            }
            drop(read);
            thread::sleep(POLL);
        }
    }

    /// Everything written to the pipe so far, as text, or as much of it as was read by
    /// `until`: for a line to say why a process went away, where a part is better than
    /// nothing.
    fn text_by(&self, until: Instant) -> String {
        self.text_to(self.mark(), until).unwrap_or_else(|| {
            let mut read = self.read.lock().unwrap_or_else(|e| e.into_inner());
            String::from_utf8_lossy(&std::mem::take(&mut *read)).into_owned()
        })
    }

    /// How many bytes wait in the pipe; 0 when that cannot be told.
    fn pending(&self) -> usize {
        let Some(pipe) = &self.pipe else {
            return 0;
        };
        let mut waiting: libc::c_int = 0;
        // SAFETY: FIONREAD writes one int, the bytes that can be read, into `waiting`.
        match unsafe { libc::ioctl(pipe.as_raw_fd(), libc::FIONREAD as _, &mut waiting) } {
            0 => usize::try_from(waiting).unwrap_or(0),
            _ => 0,
        }
    }
}

/// Waits until `pipe` has bytes to read or has ended; `false` when it cannot be waited for.
fn readable(pipe: &OwnedFd) -> bool {
    let mut poll = libc::pollfd {
        fd: pipe.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: poll(2) on the one `pollfd` above, without a timeout.
        match unsafe { libc::poll(&mut poll, 1, -1) } {
            1.. => return true,
            _ if retry(&io::Error::last_os_error()) => {}
            _ => return false,
        }
    }
}

/// Whether a call that failed with `error` is simply made again.
fn retry(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
    )
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

    /// R23: `run_with_stdin` hands the child its input and closes it, larger than a pipe's
    /// buffer included, and still times out.
    #[test]
    fn writes_stdin_then_closes_it() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(dir.path(), "s", "wc -c | tr -d ' '; echo \"$1\"");
        let input = vec![b'x'; 300_000];
        let outcome = run_with_stdin(&p, &["arg"], &input, Duration::from_secs(10));
        assert_eq!(outcome.success_stdout(), Some("300000\narg\n"));
        let slow = script(dir.path(), "slow", "exec sleep 30");
        let outcome = run_with_stdin(&slow, &[], b"x", Duration::from_millis(200));
        assert_eq!(outcome, Outcome::TimedOut);
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

    /// Whether process `pid` is still there.
    fn alive(pid: libc::pid_t) -> bool {
        // SAFETY: kill(2) with signal 0 only checks that the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// Ends a process a test's command left running on purpose.
    fn end(pid: libc::pid_t) {
        // SAFETY: kill(2) takes no pointers; the pid is the test's own `sleep`.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }

    /// R4 (review #13): the command exited, and a process it started still holds its output
    /// open (both pipes, or either one): what the command wrote is its output, taken without
    /// waiting for that process. The process is not remuda's to end: it is left running.
    #[test]
    fn output_is_taken_when_a_left_behind_process_holds_the_pipes() {
        for holds in ["", "2>/dev/null", ">/dev/null"] {
            let dir = tempfile::tempdir().unwrap();
            let p = script(
                dir.path(),
                "s",
                &format!(
                    "sleep 30 {holds} & echo $! > \"$1/left\"; echo out; echo err >&2; exit 3"
                ),
            );
            let d = dir.path().to_str().unwrap();
            let start = Instant::now();
            let outcome = run_captured(&p, &[d], &keep(), Duration::from_secs(20));
            let left = pid_in(&dir.path().join("left"));
            let survived = alive(left);
            end(left);
            assert_eq!(
                outcome,
                Outcome::Exited {
                    code: Some(3),
                    stdout: "out\n".into(),
                    stderr: "err\n".into()
                },
                "{holds:?}"
            );
            assert!(start.elapsed() < Duration::from_secs(10), "waited for it");
            assert!(survived, "what the command left running was ended");
        }
    }

    /// R4 (review #13): on a timeout the whole process group goes, a process that ignores
    /// SIGTERM included, not only the command itself. (As for JSON-RPC below, the timeout
    /// leaves the script time to write both pids.)
    #[test]
    fn a_timeout_kills_the_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(
            dir.path(),
            "s",
            "sleep 30 & echo $! > \"$1/plain\"; (trap '' TERM; exec sleep 30) & \
             echo $! > \"$1/stubborn\"; echo partial; exec sleep 30",
        );
        let d = dir.path().to_str().unwrap();
        let start = Instant::now();
        let outcome = run_captured(&p, &[d], &keep(), Duration::from_secs(5));
        assert_eq!(outcome, Outcome::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(10));
        for name in ["plain", "stubborn"] {
            assert!(gone(pid_in(&dir.path().join(name))), "{name} survived");
        }
    }

    /// R4 (review #13, review round 3): the command exited and left processes that keep
    /// writing to its output, both streams, without a pause. What it printed is its output,
    /// taken at once: the writers are not waited for, not for a moment without output of
    /// theirs and least of all until the command's timeout, and they are left running.
    /// (Several rounds: whether a writer is caught between two lines is a matter of timing.)
    #[test]
    fn output_is_taken_when_left_behind_processes_keep_writing() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(
            dir.path(),
            "s",
            "echo $$ > \"$1/group\"; echo out; echo err >&2; \
             for n in 1 2 3 4 5 6 7 8; do (while :; do echo noise; echo noise >&2; done) & done; \
             echo $! > \"$1/writer\"; exit 3",
        );
        let d = dir.path().to_str().unwrap();
        let timeout = Duration::from_secs(4);
        for round in 0..5 {
            let start = Instant::now();
            let outcome = run_captured(&p, &[d], &keep(), timeout);
            let took = start.elapsed();
            let survived = alive(pid_in(&dir.path().join("writer")));
            // SAFETY: kill(2) takes no pointers; the group is the script's, writers and all.
            unsafe { libc::kill(-pid_in(&dir.path().join("group")), libc::SIGKILL) };
            let Outcome::Exited {
                code: Some(3),
                stdout,
                stderr,
            } = outcome
            else {
                panic!("round {round}: {}", outcome.describe(timeout));
            };
            assert!(stdout.starts_with("out\n"), "round {round}: no `out`");
            assert!(stderr.starts_with("err\n"), "round {round}: no `err`");
            assert!(
                took < timeout / 2,
                "round {round}: took {took:?}, waiting for the writers"
            );
            assert!(
                survived,
                "round {round}: what the command left running was ended"
            );
        }
    }

    /// R4 (review round 3): what is taken is what was written when the command was seen to
    /// have exited, a fixed number of bytes. A writer that never stops afterwards is neither
    /// waited for (the pipe is never empty again, and the deadline is a minute away) nor part
    /// of the output.
    #[test]
    fn output_ends_where_it_was_marked() {
        let (reader, mut writer) = io::pipe().unwrap();
        let capture = Capture::of(Some(OwnedFd::from(reader)));
        writer.write_all(b"answer\n").unwrap();
        let mark = capture.mark();
        assert_eq!(mark, 7);
        let (tx, stop) = mpsc::channel::<()>();
        let noise = thread::spawn(move || {
            while stop.try_recv() == Err(mpsc::TryRecvError::Empty) {
                if writer.write_all(b"noise\n").is_err() {
                    break;
                }
            }
        });
        let start = Instant::now();
        let text = capture.text_to(mark, start + Duration::from_secs(60));
        let took = start.elapsed();
        // The rest is there for whoever asks later, from where the first take ended.
        let later = capture.text_to(6, start + Duration::from_secs(60));
        drop(tx);
        // The writer is not left blocked on a full pipe: what nobody takes is still read.
        drop(capture);
        noise.join().unwrap();
        assert_eq!(text.as_deref(), Some("answer\n"));
        assert_eq!(later.as_deref(), Some("noise\n"));
        assert!(
            took < Duration::from_secs(10),
            "took {took:?}: waited for the writer"
        );
    }

    /// R4 (GitHub review round 3): what a command wrote and was not all read by the deadline
    /// is not handed on as its output, cut short: there is none, nothing is taken, and the
    /// run counts as timed out. (One byte more than was written stands for the bytes a
    /// reading thread that is behind has not come to: it never reads that far.)
    #[test]
    fn output_that_was_not_all_read_in_time_is_no_output() {
        let (reader, mut writer) = io::pipe().unwrap();
        let capture = Capture::of(Some(OwnedFd::from(reader)));
        writer.write_all(b"half an ans").unwrap();
        let mark = capture.mark() + 1;
        assert_eq!(capture.text_to(mark, Instant::now() + GRACE), None);
        // The same when the pipe ends short of the mark: it cannot be read that far.
        let (short, ends) = io::pipe().unwrap();
        let ended = Capture::of(Some(OwnedFd::from(short)));
        drop(ends);
        assert_eq!(
            ended.text_to(1, Instant::now() + Duration::from_secs(60)),
            None
        );
        // Nothing was taken: when the rest has been read, all of it is there.
        writer.write_all(b"w").unwrap();
        assert_eq!(
            capture
                .text_to(mark, Instant::now() + Duration::from_secs(60))
                .as_deref(),
            Some("half an answ")
        );
        // A run of which either stream is cut short has timed out, whatever its exit status;
        // one that has both is the command's exit.
        use std::os::unix::process::ExitStatusExt;
        let status = ExitStatus::from_raw(0);
        let whole = || Some("whole".to_string());
        assert_eq!(exited(status, None, whole()), Outcome::TimedOut);
        assert_eq!(exited(status, whole(), None), Outcome::TimedOut);
        assert_eq!(
            exited(status, whole(), Some(String::new())),
            Outcome::Exited {
                code: Some(0),
                stdout: "whole".into(),
                stderr: String::new()
            }
        );
    }

    /// R4: a command's output is taken when the bytes written so far were read, not when the
    /// pipe ends: a process holding the pipe open and saying nothing is not waited for,
    /// however far away the deadline is.
    #[test]
    fn output_is_taken_when_the_pipe_is_empty() {
        let (reader, mut writer) = io::pipe().unwrap();
        let capture = Capture::of(Some(OwnedFd::from(reader)));
        writer.write_all(b"said\n").unwrap();
        let start = Instant::now();
        let text = capture.text_by(start + Duration::from_secs(60));
        assert_eq!(text, "said\n");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "waited for the end"
        );
        // Nothing more: at once again. The pipe ends: the rest, at once as well.
        assert_eq!(capture.text_by(start + Duration::from_secs(60)), "");
        writer.write_all(b"last").unwrap();
        drop(writer);
        assert_eq!(capture.text_by(start + Duration::from_secs(60)), "last");
        // A stream that was not piped has nothing.
        assert_eq!(
            Capture::of(None).text_by(start + Duration::from_secs(60)),
            ""
        );
    }

    /// R4 (GitHub review round 1 follow-up): what a command wrote is not lost to a reader
    /// that is behind, as on a machine running a thousand commands at once. Here the thread
    /// that reads the pipe is kept from its buffer for longer than the grace: the bytes still
    /// in the pipe are waited for.
    #[test]
    fn output_is_not_lost_to_a_reader_that_is_behind() {
        let (reader, mut writer) = io::pipe().unwrap();
        let capture = Capture::of(Some(OwnedFd::from(reader)));
        // Another thread has the buffer, and keeps it for a while.
        let buffer = Arc::clone(&capture.read);
        let (tx, kept) = mpsc::channel();
        let keeper = thread::spawn(move || {
            let behind = buffer.lock().unwrap();
            tx.send(()).unwrap();
            thread::sleep(GRACE * 3);
            drop(behind);
        });
        kept.recv().unwrap();
        writer.write_all(b"late\n").unwrap();
        let text = capture.text_by(Instant::now() + Duration::from_secs(60));
        keeper.join().unwrap();
        assert_eq!(text, "late\n");
    }

    /// R4: what a command that ran to its end left running is left alone, whatever its exit
    /// status and however it was fed: a helper an agent started on the side is the agent's.
    /// (Long after the grace a group gets to go: nothing ended it meanwhile.)
    #[test]
    fn what_a_command_that_exited_left_running_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_str().unwrap();
        let t = Duration::from_secs(20);
        let quiet = "sleep 30 >/dev/null 2>&1 & echo $! > \"$1/$2\"";
        let ok = script(dir.path(), "ok", &format!("{quiet}; echo done"));
        let failing = script(dir.path(), "failing", &format!("{quiet}; exit 4"));
        let fed = script(dir.path(), "fed", &format!("{quiet}; cat; echo done"));
        let outcome = run_captured(&ok, &[d, "ok.pid"], &keep(), t);
        assert_eq!(outcome.success_stdout(), Some("done\n"));
        let outcome = run_captured(&failing, &[d, "failing.pid"], &keep(), t);
        assert_eq!(outcome.describe(t), "exited with status 4");
        let outcome = run_with_stdin(&fed, &[d, "fed.pid"], b"fed\n", t);
        assert_eq!(outcome.success_stdout(), Some("fed\ndone\n"));
        thread::sleep(TERM_GRACE + GRACE);
        let left = ["ok.pid", "failing.pid", "fed.pid"].map(|name| pid_in(&dir.path().join(name)));
        let survived = left.map(alive);
        left.into_iter().for_each(end);
        assert_eq!(survived, [true; 3]);
    }

    /// R4, R23: a command fed on stdin that times out takes its process group with it too.
    #[test]
    fn a_timeout_kills_the_process_group_of_a_command_fed_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(
            dir.path(),
            "s",
            "sleep 30 & echo $! > \"$1/left\"; cat >/dev/null; exec sleep 30",
        );
        let d = dir.path().to_str().unwrap();
        let outcome = run_with_stdin(&p, &[d], b"fed\n", Duration::from_secs(5));
        assert_eq!(outcome, Outcome::TimedOut);
        assert!(gone(pid_in(&dir.path().join("left"))), "left survived");
    }

    /// R4: outside the terminal's foreground process group, reading the terminal or changing
    /// its modes stops a process (SIGTTIN, SIGTTOU) until someone continues it, which nobody
    /// would before the timeout. The command ignores both. (The signals stand in for the
    /// terminal here: their default action is to stop. `tests/usage_cli.rs` has a terminal.)
    #[test]
    fn the_terminals_stop_signals_are_ignored_in_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(dir.path(), "s", "kill -s TTIN $$; kill -s TTOU $$; echo on");
        let start = Instant::now();
        let outcome = run_captured(&p, &[], &keep(), Duration::from_secs(10));
        assert_eq!(outcome.success_stdout(), Some("on\n"));
        let answers = rpc(
            "kill -s TTIN $$; kill -s TTOU $$; read a; echo '{\"id\":1,\"result\":1}'; \
             cat >/dev/null",
            &["{}"],
            &[1],
            Duration::from_secs(10),
        );
        assert!(answers.is_ok(), "{answers:?}");
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "stopped until the timeout"
        );
    }

    /// The command runs in a process group of its own, which it leads.
    #[test]
    fn the_command_leads_its_own_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let p = script(dir.path(), "s", "ps -o pgid= -p $$; echo $$");
        let outcome = run_captured(&p, &[], &keep(), Duration::from_secs(10));
        let out = outcome.success_stdout().unwrap();
        let ids: Vec<&str> = out.split_whitespace().collect();
        assert_eq!(ids.len(), 2, "{out:?}");
        assert_eq!(ids[0], ids[1], "{out:?}");
        // SAFETY: getpgrp(2) takes no arguments.
        let own = unsafe { libc::getpgrp() };
        assert_ne!(ids[0], own.to_string());
    }

    /// One wording for a run that exited, with the stream a caller says its errors are on.
    #[test]
    fn a_failure_is_described_from_stderr_or_on_request_stdout() {
        let t = Duration::from_secs(1);
        let exited = |code, stdout: &str, stderr: &str| Outcome::Exited {
            code,
            stdout: stdout.into(),
            stderr: stderr.into(),
        };
        let silent = exited(Some(1), "\n Refusing \nmore\n", " \n");
        assert_eq!(silent.describe(t), "exited with status 1");
        assert_eq!(
            silent.describe_or_stdout(t),
            "exited with status 1: Refusing"
        );
        let both = exited(Some(1), "out\n", "err\n");
        assert_eq!(both.describe_or_stdout(t), "exited with status 1: err");
        let killed = exited(None, "out\n", "");
        assert_eq!(killed.describe_or_stdout(t), "was killed by a signal");
        assert_eq!(
            exited(Some(0), "out\n", "").describe_or_stdout(t),
            "succeeded"
        );
        assert_eq!(
            Outcome::TimedOut.describe_or_stdout(t),
            "timed out after 1s"
        );
        assert_eq!(
            Outcome::SpawnFailed("no".into()).describe(t),
            "could not be started: no"
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
