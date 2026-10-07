//! The signals that end remuda (Ctrl-C, Ctrl-\, a hangup, SIGTERM): the one owner of what
//! remuda does with them (SPEC R4, R6).
//!
//! Two modules care. A foreground child ([`crate::launch`]) has the terminal, so remuda sits
//! Ctrl-C and Ctrl-\ out while it runs ([`hold`]). A captured command ([`crate::probe`]) leads
//! its own process group, so the terminal's signals no longer reach it: remuda passes them on,
//! and a SIGTERM it gets, before it ends by them ([`starting`]). Both go through one handler, installed once and never
//! replaced, so that neither can mistake what the other did for what remuda inherited.
//!
//! A command is remuda's to tell from the moment its start begins, not from the moment its
//! process group is known: commands start in parallel, and a signal that ended remuda between
//! one's `fork` and the note of its group would leave it running with nobody to end it. So a
//! signal that finds a start under way does not end remuda at once. It is remembered; nothing
//! new starts; each start under way tells its own group as soon as it has one; and the last of
//! them ends remuda by the signal, once the handler is done telling the groups it knew.
//!
//! The handler reads the groups from a table of fixed size, without a lock. No command runs
//! outside it: one that finds the table full waits for a place.

use std::sync::Once;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::time::Duration;

/// What a terminal sends its foreground process group, each ending a process by default:
/// Ctrl-C, Ctrl-\ and a hangup; and SIGTERM, which `kill` or a supervisor sends remuda alone.
/// The commands remuda runs in groups of their own get none of them by themselves.
const SIGNALS: [libc::c_int; 4] = [libc::SIGINT, libc::SIGQUIT, libc::SIGHUP, libc::SIGTERM];

/// How many commands remuda runs at a time, each in a watched process group. Nothing else
/// bounds them: there is one per account at once (`list`, `usage --live`, `pick`, the TUI's
/// refreshes), and in the fallback of R7 a `ps` per file of every `sessions/` directory, and
/// neither the registry nor a directory has a limit. One more waits for a place
/// ([`State::place`]), so that no command ever runs without being watched: a place is
/// a slot of a table the handler can read without a lock.
const PLACES: usize = 1024;

/// What a taken slot holds until its command runs: no process group (1 would name every
/// process, and is never signalled).
const TAKEN: libc::pid_t = 1;

/// How often a command that waits for a place looks for one.
const POLL: Duration = Duration::from_millis(10);

/// What the handler and the threads that start commands share, for at most `N` commands at a
/// time. Atomics only, each read and written in one total order (`SeqCst`): the handler may
/// run on any thread at any moment, and takes no lock.
struct State<const N: usize> {
    /// The process groups of the commands: 0 is a free slot, [`TAKEN`] one whose command is
    /// being started.
    groups: [AtomicI32; N],
    /// Live [`Held`] guards.
    held: AtomicUsize,
    /// Starts under way: begun, and their process group not yet watched.
    starting: AtomicUsize,
    /// The signal remuda is ending by, once one came that was not sat out; 0 before.
    ending: AtomicI32,
    /// Handlers that are telling the watched groups: remuda does not end under them.
    telling: AtomicUsize,
}

static STATE: State<PLACES> = State::new();
static INSTALL: Once = Once::new();

impl<const N: usize> State<N> {
    const fn new() -> Self {
        State {
            groups: [const { AtomicI32::new(0) }; N],
            held: AtomicUsize::new(0),
            starting: AtomicUsize::new(0),
            ending: AtomicI32::new(0),
            telling: AtomicUsize::new(0),
        }
    }

    /// Whether `signal` does nothing now: Ctrl-C or Ctrl-\ while a foreground child has the
    /// terminal, which gets them too. A hangup and SIGTERM are never sat out: the child does
    /// not get SIGTERM from the terminal, and remuda ends by it as it would without a handler.
    fn sat_out(&self, signal: libc::c_int) -> bool {
        matches!(signal, libc::SIGINT | libc::SIGQUIT) && self.held.load(Ordering::SeqCst) > 0
    }

    /// The handler's part: unless `signal` is sat out, remuda is ending by it from now on, and
    /// the watched groups get it. `true` when remuda ends now; `false` when it is sat out,
    /// when a start is under way, which then ends remuda ([`State::start_over`]), or when
    /// another handler is still telling the groups (two signals at once, on two threads, or
    /// one over the other on one thread), which then ends it when it is done.
    ///
    /// The order matters, here and in the functions below: `ending` is written before the
    /// groups and `starting` are read, and a start writes its group, or lowers `starting`,
    /// before it reads `ending`. So of a signal and a start, at least one sees the other.
    /// Likewise each handler lowers `telling` before it reads `telling` and `starting`, and a
    /// start lowers `starting` before it reads `telling`: whichever of them lowers its count
    /// last finds both at nothing and ends remuda, and none does while groups are still
    /// being told.
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
        self.telling.load(Ordering::SeqCst) == 0 && self.starting.load(Ordering::SeqCst) == 0
    }

    /// Takes a free slot, if there is one.
    fn take(&self) -> Option<usize> {
        self.groups.iter().position(|slot| {
            slot.compare_exchange(0, TAKEN, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        })
    }

    /// A place for one more command: a slot, waited for while every one is taken (each is
    /// given back when its command is over, which its timeout sees to). `None` when remuda
    /// is ending: nothing starts any more.
    fn place(&self) -> Option<usize> {
        loop {
            if self.ending.load(Ordering::SeqCst) != 0 {
                return None;
            }
            if let Some(slot) = self.take() {
                return Some(slot);
            }
            std::thread::sleep(POLL);
        }
    }

    /// A start begins. `false` when remuda is ending: the caller starts nothing. Either way
    /// the caller says when the start is over ([`State::start_over`]).
    fn start(&self) -> bool {
        self.starting.fetch_add(1, Ordering::SeqCst);
        self.ending.load(Ordering::SeqCst) == 0
    }

    /// The command of `slot` runs, as process `pid`, leading its group: the group is watched
    /// from now until [`State::give_back`] (a `pid` that names no group to signal is not). A
    /// signal remuda is already ending by goes to the group here: the handler may have read
    /// the slot before the group was written.
    fn watch(&self, slot: usize, pid: u32) {
        // 0 and 1 would name remuda's own group and every process.
        let Some(pgid) = libc::pid_t::try_from(pid).ok().filter(|pgid| *pgid > 1) else {
            return;
        };
        self.groups[slot].store(pgid, Ordering::SeqCst);
        signal_group(pgid, self.ending.load(Ordering::SeqCst));
    }

    /// The command of `slot` is over, or was never started: the slot is free again.
    fn give_back(&self, slot: usize) {
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

/// `signal` to process group `pgid`; no signal (0: remuda is not ending) and no group (a free
/// or a taken slot) are nothing to do.
fn signal_group(pgid: libc::pid_t, signal: libc::c_int) {
    if pgid > TAKEN && signal != 0 {
        // SAFETY: kill(2) takes no pointers; a negative pid names the process group.
        unsafe { libc::kill(-pgid, signal) };
    }
}

/// A command about to be started in a process group of its own, with its place among the
/// watched ones. Dropped without [`Starting::started`], nothing was started.
pub struct Starting {
    /// Given back on drop unless the command runs and [`Watched`] has it.
    slot: Option<usize>,
}

/// Before a command is started in a process group of its own: `None` when a signal is ending
/// remuda, and nothing may be started any more. The command is remuda's to tell of such a
/// signal from here on, though its group is known only once it runs ([`Starting::started`]).
/// When as many commands run as remuda watches at a time, this waits for one to be over.
pub fn starting() -> Option<Starting> {
    install();
    let slot = STATE.place()?;
    let began = STATE.start();
    // Dropped right away when remuda is ending: that start is over too.
    let starting = Starting { slot: Some(slot) };
    began.then_some(starting)
}

impl Starting {
    /// The command runs, as process `pid`, leading its group: the group is watched until the
    /// guard is dropped, and has been told already if a signal came while it was started.
    pub fn started(mut self, pid: u32) -> Watched {
        let slot = self
            .slot
            .take()
            .expect("a start has its slot until it is over");
        STATE.watch(slot, pid);
        Watched(slot)
    }
}

impl Drop for Starting {
    fn drop(&mut self) {
        if let Some(slot) = self.slot {
            STATE.give_back(slot);
        }
        if let Some(signal) = STATE.start_over() {
            end_by(signal);
        }
    }
}

/// A process group the terminal's signals are passed on to, until this is dropped.
pub struct Watched(usize);

impl Drop for Watched {
    fn drop(&mut self) {
        STATE.give_back(self.0);
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

/// The handler: unless the signal is sat out, the watched groups get it, and then remuda does:
/// at once, or when the last start under way is over, or when another handler that is telling
/// the groups is done.
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

    fn watched<const N: usize>(state: &State<N>, pid: u32) -> bool {
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
        // A taken slot holds no group until its command runs, nor for a pid that names none.
        let state = State::<2>::new();
        let slot = state.take().unwrap();
        for pid in [0, 1, u32::MAX] {
            state.watch(slot, pid);
            assert_eq!(state.groups[slot].load(Ordering::SeqCst), TAKEN, "{pid}");
        }
        state.give_back(slot);
        assert!(state.groups.iter().all(|g| g.load(Ordering::SeqCst) == 0));
    }

    /// R4 (GitHub review round 1): no command runs unwatched. With every place taken, one
    /// more waits until a command is over, and then has that place; it does not wait once
    /// remuda is ending, when nothing starts any more.
    #[test]
    fn a_command_waits_for_a_place() {
        let state = State::<2>::new();
        let (first, second) = (state.take().unwrap(), state.take().unwrap());
        assert_ne!(first, second);
        assert_eq!(state.take(), None, "a third command had a place");
        let waited = std::thread::scope(|scope| {
            let waiting = scope.spawn(|| {
                let start = Instant::now();
                (state.place(), start.elapsed())
            });
            std::thread::sleep(Duration::from_millis(200));
            assert!(!waiting.is_finished(), "started without a place");
            state.give_back(first);
            waiting.join().unwrap()
        });
        assert_eq!(waited.0, Some(first));
        assert!(waited.1 >= Duration::from_millis(200), "{:?}", waited.1);
        // Full again, and remuda is ending: no place, at once.
        state.ending.store(libc::SIGTERM, Ordering::SeqCst);
        assert_eq!(state.place(), None);
        state.give_back(second);
        assert_eq!(state.place(), None, "started while remuda was ending");
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
        let state = State::<4>::new();
        let slot = state.place().unwrap();
        assert!(state.start());
        state.watch(slot, child.id());
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
        let state = State::<4>::new();
        // Two commands are being started: forked, say, their groups not known yet.
        let (slot, other) = (state.place().unwrap(), state.place().unwrap());
        assert!(state.start());
        assert!(state.start());
        let mut first = sleeper();
        assert!(!state.signalled(libc::SIGTERM), "remuda ended over a start");
        assert_eq!(state.ending.load(Ordering::SeqCst), libc::SIGTERM);
        // Nothing new starts: there is no place any more, and a start that had its place
        // before the signal is refused, without being the last one under way.
        assert_eq!(state.place(), None);
        assert!(!state.start());
        assert_eq!(state.start_over(), None);
        // The first runs: it is told, though the handler found no group in its slot.
        state.watch(slot, first.id());
        assert_eq!(first.wait().unwrap().signal(), Some(libc::SIGTERM));
        assert_eq!(state.start_over(), None, "a start is still under way");
        // The second could not be started at all: its start is over, and it was the last.
        state.give_back(other);
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
        let state = State::<4>::new();
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

    /// R4 (GitHub review round 2): two signals may be handled at once, on two threads (Ctrl-C,
    /// and the terminal going right after it). The handler that is done first does not end
    /// remuda under the other, which has groups left to tell: a command that ignores the
    /// first signal would be left running. The last one ends remuda.
    #[test]
    fn a_handler_does_not_end_remuda_under_another() {
        let state = State::<4>::new();
        let slot = state.place().unwrap();
        assert!(state.start());
        let mut child = sleeper();
        state.watch(slot, child.id());
        assert_eq!(state.start_over(), None);
        // Another handler is among the groups, on another thread.
        state.telling.store(1, Ordering::SeqCst);
        assert!(
            !state.signalled(libc::SIGTERM),
            "ended under another handler"
        );
        assert_eq!(state.telling.load(Ordering::SeqCst), 1);
        // This one did its part: the group it knew was told.
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGTERM));
        // The other is done, and nobody else is telling: it ends remuda.
        state.telling.store(0, Ordering::SeqCst);
        assert!(state.signalled(libc::SIGHUP));
        assert_eq!(state.ending.load(Ordering::SeqCst), libc::SIGTERM);
    }

    /// R6: with a foreground child, Ctrl-C and Ctrl-\ are sat out, and remuda is not ending:
    /// commands still start. A hangup and SIGTERM never are sat out.
    #[test]
    fn interrupts_are_sat_out_while_held() {
        let state = State::<4>::new();
        state.held.store(1, Ordering::SeqCst);
        for signal in [libc::SIGINT, libc::SIGQUIT] {
            assert!(state.sat_out(signal));
            assert!(!state.signalled(signal));
        }
        assert_eq!(state.ending.load(Ordering::SeqCst), 0);
        assert!(state.start());
        assert_eq!(state.start_over(), None);
        assert!(!state.sat_out(libc::SIGHUP));
        assert!(!state.sat_out(libc::SIGTERM));
        assert!(state.signalled(libc::SIGHUP));
        assert_eq!(state.ending.load(Ordering::SeqCst), libc::SIGHUP);
        // The guard is what holds: for as long as it lives.
        let held = hold();
        assert!(STATE.sat_out(libc::SIGINT));
        drop(held);
    }
}
