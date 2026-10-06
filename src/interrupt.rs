//! The terminal's signals (Ctrl-C, Ctrl-\, a hangup): the one owner of what remuda does with
//! them (SPEC R4, R6).
//!
//! Two modules care. A foreground child ([`crate::launch`]) has the terminal, so remuda sits
//! Ctrl-C and Ctrl-\ out while it runs ([`hold`]). A captured command ([`crate::probe`]) leads
//! its own process group, so the terminal's signals no longer reach it: remuda passes them on
//! before it ends by them ([`starting`]). Both go through one handler, installed once and never
//! replaced, so that neither can mistake what the other did for what remuda inherited.
//!
//! A command is remuda's to tell from the moment its start begins, not from the moment its
//! process group is known: commands start in parallel, and a signal that ended remuda between
//! one's `fork` and the note of its group would leave it running with nobody to end it. So a
//! signal that finds a start under way does not end remuda at once. It is remembered; nothing
//! new starts; each start under way tells its own group as soon as it has one; and the last of
//! them ends remuda by the signal, once the handler is done telling the groups it knew.

use std::sync::Once;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

/// What a terminal sends its foreground process group, each ending a process by default:
/// Ctrl-C, Ctrl-\ and a hangup.
const SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGQUIT, libc::SIGHUP];

/// How many process groups can be watched at once: a query per account, and a `ps` per file of
/// a `sessions/` directory, all in parallel. One more is told of a signal that is ending
/// remuda when it starts, and not of a later one (it is still terminated when its run times
/// out).
const SLOTS: usize = 1024;

/// What the handler and the threads that start commands share. Atomics only, each read and
/// written in one total order (`SeqCst`): the handler may run on any thread at any moment, and
/// takes no lock.
struct State {
    /// The watched process groups; 0 is a free slot.
    groups: [AtomicI32; SLOTS],
    /// Live [`Held`] guards.
    held: AtomicUsize,
    /// Starts under way: begun, and their process group not yet watched.
    starting: AtomicUsize,
    /// The signal remuda is ending by, once one came that was not sat out; 0 before.
    ending: AtomicI32,
    /// Handlers that are telling the watched groups: remuda does not end under them.
    telling: AtomicUsize,
}

static STATE: State = State::new();
static INSTALL: Once = Once::new();

impl State {
    const fn new() -> State {
        State {
            groups: [const { AtomicI32::new(0) }; SLOTS],
            held: AtomicUsize::new(0),
            starting: AtomicUsize::new(0),
            ending: AtomicI32::new(0),
            telling: AtomicUsize::new(0),
        }
    }

    /// Whether `signal` does nothing now: Ctrl-C or Ctrl-\ while a foreground child has the
    /// terminal. A hangup is never sat out.
    fn sat_out(&self, signal: libc::c_int) -> bool {
        signal != libc::SIGHUP && self.held.load(Ordering::SeqCst) > 0
    }

    /// The handler's part: unless `signal` is sat out, remuda is ending by it from now on, and
    /// the watched groups get it. `true` when remuda ends now; `false` when it is sat out, or
    /// when a start is under way, which then ends remuda ([`State::start_over`]).
    ///
    /// The order matters, here and in the functions below: `ending` is written before the
    /// groups and `starting` are read, and a start writes its group, or lowers `starting`,
    /// before it reads `ending`. So of a signal and a start, at least one sees the other.
    /// Likewise `telling` is lowered before `starting` is read, and a start lowers `starting`
    /// before it reads `telling`: remuda is ended by one of the two, and by neither while the
    /// groups are still being told.
    fn signalled(&self, signal: libc::c_int) -> bool {
        if self.sat_out(signal) {
            return false;
        }
        self.telling.fetch_add(1, Ordering::SeqCst);
        let _ = self
            .ending
            .compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst);
        for slot in &self.groups {
            signal_group(slot.load(Ordering::SeqCst), signal);
        }
        self.telling.fetch_sub(1, Ordering::SeqCst);
        self.starting.load(Ordering::SeqCst) == 0
    }

    /// A start begins. `false` when remuda is ending: the caller starts nothing. Either way
    /// the caller says when the start is over ([`State::start_over`]).
    fn start(&self) -> bool {
        self.starting.fetch_add(1, Ordering::SeqCst);
        self.ending.load(Ordering::SeqCst) == 0
    }

    /// Watches the process group led by process `pid`, from now until [`State::unwatch`]:
    /// the slot it took (`None` when there is none left, or `pid` is no group to signal). A
    /// signal remuda is already ending by goes to the group here: the handler may have read
    /// the slots before this one was written.
    fn watch(&self, pid: u32) -> Option<usize> {
        // 0 and 1 would name remuda's own group and every process.
        let pgid = libc::pid_t::try_from(pid).ok().filter(|pgid| *pgid > 1)?;
        let slot = self.groups.iter().position(|slot| {
            slot.compare_exchange(0, pgid, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        });
        let ending = self.ending.load(Ordering::SeqCst);
        if ending != 0 {
            signal_group(pgid, ending);
        }
        slot
    }

    fn unwatch(&self, slot: usize) {
        self.groups[slot].store(0, Ordering::SeqCst);
    }

    /// A start is over: its group is watched, or nothing was started. The signal remuda ends
    /// by, when it is ending, this was the last start under way, and no handler is still
    /// telling the watched groups (it then ends remuda itself): the caller ends it.
    fn start_over(&self) -> Option<libc::c_int> {
        self.starting.fetch_sub(1, Ordering::SeqCst);
        let ending = self.ending.load(Ordering::SeqCst);
        let last = self.starting.load(Ordering::SeqCst) == 0;
        let told = self.telling.load(Ordering::SeqCst) == 0;
        (ending != 0 && last && told).then_some(ending)
    }
}

/// `signal` to process group `pgid`; a free slot (0) is nobody.
fn signal_group(pgid: libc::pid_t, signal: libc::c_int) {
    if pgid > 1 {
        // SAFETY: kill(2) takes no pointers; a negative pid names the process group.
        unsafe { libc::kill(-pgid, signal) };
    }
}

/// A command about to be started in a process group of its own. Dropped without
/// [`Starting::started`], nothing was started.
pub struct Starting(());

/// Before a command is started in a process group of its own: `None` when a signal is ending
/// remuda, and nothing may be started any more. The command is remuda's to tell of such a
/// signal from here on, though its group is known only once it runs ([`Starting::started`]).
pub fn starting() -> Option<Starting> {
    install();
    // Dropped right away when remuda is ending: that start is over too.
    let starting = Starting(());
    STATE.start().then_some(starting)
}

impl Starting {
    /// The command runs, as process `pid`, leading its group: the group is watched until the
    /// guard is dropped, and has been told already if a signal came while it was started.
    pub fn started(self, pid: u32) -> Watched {
        Watched(STATE.watch(pid))
    }
}

impl Drop for Starting {
    fn drop(&mut self) {
        if let Some(signal) = STATE.start_over() {
            end_by(signal);
        }
    }
}

/// A process group the terminal's signals are passed on to, until this is dropped.
pub struct Watched(Option<usize>);

impl Drop for Watched {
    fn drop(&mut self) {
        if let Some(slot) = self.0 {
            STATE.unwatch(slot);
        }
    }
}

/// Ctrl-C and Ctrl-\ do nothing to remuda for as long as any of these lives (guards on several
/// threads nest): a child in the foreground has the terminal, as in `system(3)`.
pub struct Held(());

/// See [`Held`].
pub fn hold() -> Held {
    install();
    STATE.held.fetch_add(1, Ordering::SeqCst);
    Held(())
}

impl Drop for Held {
    fn drop(&mut self) {
        STATE.held.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Installs [`on_signal`] for each of [`SIGNALS`], once, before the first command is started or
/// the first guard held. A signal remuda inherited as ignored (`nohup`, a background job of a
/// shell without job control) stays ignored. Nothing in remuda sets one to ignored afterwards,
/// so what is found here is what was inherited.
fn install() {
    INSTALL.call_once(|| {
        for signal in SIGNALS {
            // SAFETY: sigaction(2) with pointers to zeroed locals; the handler only calls
            // async-signal-safe functions.
            unsafe {
                let mut old: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(signal, std::ptr::null(), &mut old) != 0
                    || old.sa_sigaction == libc::SIG_IGN
                {
                    continue;
                }
                let mut new: libc::sigaction = std::mem::zeroed();
                new.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
                // Calls a signal interrupts are restarted: the handler may return, with a
                // guard held or a start under way.
                new.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut new.sa_mask);
                libc::sigaction(signal, &new, std::ptr::null_mut());
            }
        }
    });
}

/// The handler: unless the signal is sat out, the watched groups get it, and then remuda does,
/// at once or when the last start under way is over.
extern "C" fn on_signal(signal: libc::c_int) {
    if STATE.signalled(signal) {
        end_by(signal);
    }
}

/// Ends remuda by `signal`, with its default action back, so that it ends as it would have
/// without a handler.
fn end_by(signal: libc::c_int) {
    // SAFETY: signal(2) and raise(3) are async-signal-safe. In the handler the signal is
    // blocked, and delivered to this thread as the handler returns; elsewhere, at once.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    use super::*;

    /// A process leading a group of its own, as a command remuda runs does.
    fn sleeper() -> Child {
        Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap()
    }

    fn watched(state: &State, pid: u32) -> bool {
        let pgid = libc::pid_t::try_from(pid).unwrap();
        state
            .groups
            .iter()
            .any(|g| g.load(Ordering::SeqCst) == pgid)
    }

    /// R4: a group is watched from the end of its start until the guard is dropped; pids that
    /// would name remuda's own group or every process are never stored.
    #[test]
    fn a_group_is_watched_until_dropped() {
        let mut child = sleeper();
        let guard = starting().expect("not ending").started(child.id());
        assert!(watched(&STATE, child.id()));
        drop(guard);
        assert!(!watched(&STATE, child.id()));
        child.kill().unwrap();
        child.wait().unwrap();
        let state = State::new();
        for pid in [0, 1, u32::MAX] {
            assert_eq!(state.watch(pid), None, "{pid}");
        }
        assert!(state.groups.iter().all(|g| g.load(Ordering::SeqCst) == 0));
    }

    /// R4: the signal goes to every member of a watched group, and with no start under way
    /// remuda ends at once. (A state of its own, here and below: the process's is shared with
    /// every test that runs a command.)
    #[test]
    fn the_signal_reaches_the_whole_group() {
        let dir = tempfile::tempdir().unwrap();
        let script =
            crate::probe::script(dir.path(), "s", "\"$0.inner\" & echo $! > \"$0.pid\"; wait");
        crate::probe::script(dir.path(), "s.inner", "exec sleep 30");
        let mut child = Command::new(&script).process_group(0).spawn().unwrap();
        let pid_file = dir.path().join("s.pid");
        let until = Instant::now() + Duration::from_secs(10);
        let inner: libc::pid_t = loop {
            match std::fs::read_to_string(&pid_file).map(|t| t.trim().parse()) {
                Ok(Ok(pid)) => break pid,
                _ if Instant::now() < until => std::thread::sleep(Duration::from_millis(10)),
                other => panic!("no pid: {other:?}"),
            }
        };
        let state = State::new();
        assert!(state.start());
        assert!(state.watch(child.id()).is_some());
        assert_eq!(state.start_over(), None);
        assert!(state.signalled(libc::SIGTERM), "nothing is starting");
        assert!(!child.wait().unwrap().success());
        // SAFETY: kill(2) with signal 0 only checks that the process exists.
        while unsafe { libc::kill(inner, 0) } == 0 {
            assert!(Instant::now() < until, "the inner process survived");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// R4 (review round 1): a signal that comes while commands are being started does not end
    /// remuda before they are told. It is remembered; nothing new starts; a command whose
    /// start was under way gets the signal as soon as its group is known; and the last start
    /// to be over ends remuda by that signal.
    #[test]
    fn a_signal_during_a_start_reaches_the_command_once_it_runs() {
        let state = State::new();
        // Two commands are being started: forked, say, their groups not known yet.
        assert!(state.start());
        assert!(state.start());
        let mut first = sleeper();
        assert!(!state.signalled(libc::SIGTERM), "remuda ended over a start");
        assert_eq!(state.ending.load(Ordering::SeqCst), libc::SIGTERM);
        // Nothing new starts, and that start is not the last one under way.
        assert!(!state.start());
        assert_eq!(state.start_over(), None);
        // The first runs: it is told, though the handler never saw its group.
        assert!(state.watch(first.id()).is_some());
        assert_eq!(first.wait().unwrap().signal(), Some(libc::SIGTERM));
        assert_eq!(state.start_over(), None, "a start is still under way");
        // The second could not be started at all: its start is over, and it was the last.
        assert_eq!(state.start_over(), Some(libc::SIGTERM));
        // A later signal does not change what remuda ends by.
        assert!(state.signalled(libc::SIGHUP));
        assert_eq!(state.ending.load(Ordering::SeqCst), libc::SIGTERM);
    }

    /// R4 (review round 1): the last start to be over does not end remuda while the handler
    /// is still telling the groups it knew: the ones it had not come to would be left
    /// running. The handler ends remuda itself when it is done.
    #[test]
    fn remuda_does_not_end_while_the_groups_are_being_told() {
        let state = State::new();
        assert!(state.start());
        // A handler is among the groups, on another thread: remuda is ending by its signal.
        state.telling.store(1, Ordering::SeqCst);
        state.ending.store(libc::SIGTERM, Ordering::SeqCst);
        assert_eq!(state.start_over(), None, "ended under the handler");
        // It finds no start under way when it is done.
        state.telling.store(0, Ordering::SeqCst);
        assert!(state.signalled(libc::SIGTERM));
        assert_eq!(state.telling.load(Ordering::SeqCst), 0);
    }

    /// R4: a group there was no slot left for is still told of the signal remuda is ending by.
    #[test]
    fn a_group_without_a_slot_is_told_when_it_starts() {
        let state = State::new();
        for slot in &state.groups {
            // A pid no process has: its group takes the slot and gets no signal.
            slot.store(libc::pid_t::MAX, Ordering::SeqCst);
        }
        assert!(state.start());
        let mut child = sleeper();
        assert!(!state.signalled(libc::SIGTERM));
        assert_eq!(state.watch(child.id()), None);
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGTERM));
        assert_eq!(state.start_over(), Some(libc::SIGTERM));
    }

    /// R6: with a foreground child, Ctrl-C and Ctrl-\ are sat out, and remuda is not ending:
    /// commands still start. A hangup never is sat out.
    #[test]
    fn interrupts_are_sat_out_while_held() {
        let state = State::new();
        state.held.store(1, Ordering::SeqCst);
        for signal in [libc::SIGINT, libc::SIGQUIT] {
            assert!(state.sat_out(signal));
            assert!(!state.signalled(signal));
        }
        assert_eq!(state.ending.load(Ordering::SeqCst), 0);
        assert!(state.start());
        assert_eq!(state.start_over(), None);
        assert!(!state.sat_out(libc::SIGHUP));
        assert!(state.signalled(libc::SIGHUP));
        assert_eq!(state.ending.load(Ordering::SeqCst), libc::SIGHUP);
        // The guard is what holds: for as long as it lives.
        let held = hold();
        assert!(STATE.sat_out(libc::SIGINT));
        drop(held);
    }
}
