//! The life of one kind of background work: whether a round of it is out, whether it must run
//! once more when that round reports, and whether a result that arrives is that round's.
//!
//! [`super::app`] keeps a [`Slot`] per kind of work (and per account, for what is asked of each
//! account). `update` asks the slot before it starts anything: the slot says whether an
//! [`Effect`](super::app::Effect) goes out or the round already out is waited for. When the
//! [`Event`](super::app::Event) comes back, the slot says whether it is the answer still wanted.
//! Nothing here starts a thread; the slot only counts.

/// The number of a round of work: the rounds of one [`Slot`] are numbered from 1, and no number
/// is given twice.
pub type Round = u64;

/// What a [`Slot`] says of a result that has arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// Not the round that is out: there is none, it was cancelled, or this is the result of an
    /// earlier round or of another target. The slot is as it was.
    Stray,
    /// The round that was out, and nothing has asked for the work again since it began: the
    /// slot is idle.
    Done,
    /// The round that was out, but the work was asked for again meanwhile
    /// ([`Slot::restart`], [`Slot::invalidate`]): what it found is from before that. The next
    /// round, of this number and for the same target, has begun.
    Again(Round),
}

/// One kind of background work. `T` is what a round is for (the launch checked, the account
/// read, the transcript loaded); work that has no target is a `Slot<()>`.
///
/// At most one round is out for a target. A round for another target begins at once and makes
/// the one out a stray.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot<T = ()> {
    /// Rounds begun so far: the number of the last one.
    rounds: Round,
    out: Option<Out<T>>,
    stale: bool,
}

/// The round that has not reported.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Out<T> {
    what: T,
    /// Asked for again since it began.
    again: bool,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Slot {
            rounds: 0,
            out: None,
            stale: false,
        }
    }
}

impl<T> Slot<T> {
    /// What the round out is for.
    pub fn running(&self) -> Option<&T> {
        self.out.as_ref().map(|out| &out.what)
    }

    pub fn is_running(&self) -> bool {
        self.out.is_some()
    }

    /// The number of the last round begun; 0 before the first.
    pub fn round(&self) -> Round {
        self.rounds
    }

    /// What the last round found predates an [`invalidate`](Slot::invalidate): it holds until a
    /// round begun after that one is [`Claim::Done`].
    pub fn stale(&self) -> bool {
        self.stale
    }

    /// The round out is given up: its result will be a [`Claim::Stray`], and the work is not
    /// run again for it. Its target is returned. The numbering goes on.
    pub fn cancel(&mut self) -> Option<T> {
        self.out.take().map(|out| out.what)
    }

    /// What the last round found, and what a round out now will find, no longer counts: the
    /// slot is [`stale`](Slot::stale), and a round out runs once more when it reports. Nothing
    /// begins here.
    pub fn invalidate(&mut self) {
        self.stale = true;
        if let Some(out) = &mut self.out {
            out.again = true;
        }
    }

    /// The same slot with its target seen through `f`.
    pub fn map<U>(&self, f: impl FnOnce(&T) -> U) -> Slot<U> {
        Slot {
            rounds: self.rounds,
            out: self.out.as_ref().map(|out| Out {
                what: f(&out.what),
                again: out.again,
            }),
            stale: self.stale,
        }
    }

    fn begin(&mut self, what: T) -> Round {
        self.rounds += 1;
        self.out = Some(Out { what, again: false });
        self.rounds
    }
}

impl<T: PartialEq> Slot<T> {
    /// Asks for the work for `what`. A round for it is out: that one is waited for, and `None`
    /// says to start nothing. Otherwise a round begins, and its number is returned.
    pub fn start(&mut self, what: T) -> Option<Round> {
        if self.running() == Some(&what) {
            return None;
        }
        Some(self.begin(what))
    }

    /// Asks for the work for `what` because what it reads has changed. A round for it is out:
    /// it began before the change, so the work runs once more when it reports ([`Claim::Again`]),
    /// and `None` says to start nothing now. Otherwise as [`start`](Slot::start).
    pub fn restart(&mut self, what: T) -> Option<Round> {
        match &mut self.out {
            Some(out) if out.what == what => {
                out.again = true;
                None
            }
            _ => Some(self.begin(what)),
        }
    }

    /// A result that carries the number of its round has arrived for `what`. Only the last
    /// round begun, still out and for `what`, is not a [`Claim::Stray`].
    pub fn claim(&mut self, round: Round, what: &T) -> Claim {
        if round != self.rounds {
            return Claim::Stray;
        }
        self.settle(what)
    }

    /// A result that carries no number has arrived for `what`: the round out is the one, if it
    /// is for `what`.
    pub fn settle(&mut self, what: &T) -> Claim {
        match self.out.take() {
            Some(out) if out.what == *what => {
                if out.again {
                    Claim::Again(self.begin(out.what))
                } else {
                    self.stale = false;
                    Claim::Done
                }
            }
            other => {
                self.out = other;
                Claim::Stray
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asked twice, started once: the round out is waited for. Another target does not wait.
    #[test]
    fn work_that_is_out_is_not_started_again() {
        let mut slot: Slot = Slot::default();
        assert_eq!((slot.round(), slot.is_running()), (0, false));
        assert_eq!(slot.start(()), Some(1));
        assert_eq!(slot.start(()), None);
        assert_eq!(slot.start(()), None);
        assert_eq!((slot.round(), slot.is_running()), (1, true));
        assert_eq!(slot.settle(&()), Claim::Done);
        assert!(!slot.is_running());
        assert_eq!(slot.start(()), Some(2));

        let mut slot = Slot::default();
        assert_eq!(slot.start("a"), Some(1));
        assert_eq!(slot.start("a"), None);
        assert_eq!(slot.start("b"), Some(2));
        assert_eq!(slot.running(), Some(&"b"));
        assert_eq!(slot.start("b"), None);
    }

    /// Asked again while out: nothing starts, and the round that reports begins the next one,
    /// once, however often it was asked.
    #[test]
    fn work_asked_again_while_out_runs_once_more() {
        let mut slot: Slot = Slot::default();
        assert_eq!(slot.restart(()), Some(1));
        assert_eq!(slot.restart(()), None);
        assert_eq!(slot.restart(()), None);
        assert_eq!(slot.settle(&()), Claim::Again(2));
        assert!(slot.is_running());
        assert_eq!(slot.settle(&()), Claim::Done);
        assert_eq!(slot.settle(&()), Claim::Stray);
        // Idle: it starts at once.
        assert_eq!(slot.restart(()), Some(3));

        // The round asked again keeps its target; for another target it is simply replaced.
        let mut slot = Slot::default();
        slot.start("a");
        assert_eq!(slot.restart("a"), None);
        assert_eq!(slot.claim(1, &"a"), Claim::Again(2));
        assert_eq!(slot.running(), Some(&"a"));
        assert_eq!(slot.restart("b"), Some(3));
        assert_eq!(slot.claim(3, &"b"), Claim::Done);
    }

    /// Only the last round begun, still out and for the same target, is claimed: an earlier
    /// number, another target, a cancelled round and a second answer are strays, and leave the
    /// slot as it was. A number is never given twice.
    #[test]
    fn a_late_result_is_a_stray() {
        let mut slot = Slot::default();
        assert_eq!(slot.start("a"), Some(1));
        assert_eq!(slot.start("b"), Some(2));
        let before = slot.clone();
        assert_eq!(slot.claim(1, &"a"), Claim::Stray);
        assert_eq!(slot.claim(1, &"b"), Claim::Stray);
        assert_eq!(slot.claim(2, &"a"), Claim::Stray);
        assert_eq!(slot.claim(3, &"b"), Claim::Stray);
        assert_eq!(slot.settle(&"a"), Claim::Stray);
        assert_eq!(slot, before);
        assert_eq!(slot.claim(2, &"b"), Claim::Done);
        assert_eq!(slot.claim(2, &"b"), Claim::Stray);

        // Cancelled: its answer does not count, even for the same target asked again.
        assert_eq!(slot.start("b"), Some(3));
        assert_eq!(slot.cancel(), Some("b"));
        assert_eq!(slot.cancel(), None);
        assert_eq!(slot.claim(3, &"b"), Claim::Stray);
        assert_eq!(slot.start("b"), Some(4));
        assert_eq!(slot.claim(3, &"b"), Claim::Stray);
        assert_eq!(slot.claim(4, &"b"), Claim::Done);
        // A round asked again and then cancelled is not run again.
        slot.start("b");
        slot.restart("b");
        slot.cancel();
        assert_eq!(slot.settle(&"b"), Claim::Stray);
        assert!(!slot.is_running());
    }

    /// Invalidated: stale until a round begun afterwards is done. Nothing starts by itself.
    #[test]
    fn invalidated_work_is_stale_until_a_later_round_is_done() {
        let mut slot: Slot = Slot::default();
        assert!(!slot.stale());
        // Idle: nothing begins; the next round clears it.
        slot.invalidate();
        assert!(slot.stale() && !slot.is_running());
        assert_eq!(slot.start(()), Some(1));
        assert_eq!(slot.settle(&()), Claim::Done);
        assert!(!slot.stale());

        // Out: that round began before, so it runs once more and only the next one clears it.
        slot.start(());
        slot.invalidate();
        assert_eq!(slot.start(()), None);
        assert_eq!(slot.settle(&()), Claim::Again(3));
        assert!(slot.stale());
        // Asked again for another reason meanwhile: one more round, and still stale until the
        // last is done.
        assert_eq!(slot.restart(()), None);
        assert_eq!(slot.settle(&()), Claim::Again(4));
        assert!(slot.stale());
        assert_eq!(slot.settle(&()), Claim::Done);
        assert!(!slot.stale());
        // Asking again alone does not make it stale.
        slot.start(());
        slot.restart(());
        assert!(!slot.stale());
    }

    #[test]
    fn a_mapped_slot_keeps_its_round() {
        let mut slot = Slot::default();
        slot.start(2);
        slot.restart(2);
        let mut mapped = slot.map(|n| n.to_string());
        assert_eq!(mapped.running(), Some(&"2".to_string()));
        assert_eq!(mapped.claim(1, &"2".to_string()), Claim::Again(2));
    }
}
