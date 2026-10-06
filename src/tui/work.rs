//! The life of one kind of background work: which rounds of it are out, whether it must run
//! once more when a round reports, and whether a result that arrives is the one still wanted.
//!
//! [`super::app`] keeps a [`Slot`] per kind of work (and per account, for what is asked of each
//! account). `update` asks the slot before it starts anything: the slot says whether an
//! [`Effect`](super::app::Effect) goes out or a round that is out is waited for. When the
//! [`Event`](super::app::Event) comes back, the slot says whether it is the answer still wanted.
//! Nothing here starts a thread; the slot only counts.

/// The number of a round of work: the rounds of one [`Slot`] are numbered from 1, and no number
/// is given twice.
pub type Round = u64;

/// What a [`Slot`] says of a result that has arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// Not the result waited for: no round is out for its target (or not the one of its
    /// number), or that round was given up (cancelled, or left for another target) and has
    /// not been asked for again. Nothing is to be done with it.
    Stray,
    /// The round waited for, and nothing has asked for the work again since it began: the
    /// slot waits for nothing.
    Done,
    /// The round waited for, but the work was asked for again after it began
    /// ([`Slot::restart`], or asked for after it had been given up): what it found is from
    /// before that, and may stand until the next round reports. That round, of this number and
    /// for the same target, has begun, and is the one waited for.
    Again(Round),
    /// The round waited for, but made void after it began ([`Slot::invalidate`]): what it
    /// found does not count. The next round has begun, as for [`Claim::Again`].
    Void(Round),
}

impl Claim {
    /// The round that has begun, for the caller to start.
    pub fn next(self) -> Option<Round> {
        match self {
            Claim::Again(next) | Claim::Void(next) => Some(next),
            Claim::Stray | Claim::Done => None,
        }
    }

    /// What the round found may be used: it was waited for, and not made void.
    pub fn counts(self) -> bool {
        matches!(self, Claim::Done | Claim::Again(_))
    }
}

/// One kind of background work. `T` is what a round is for (the launch checked, the account
/// read, the transcript loaded); work that has no target is a `Slot<()>`.
///
/// A target has one round out at most, whatever was asked in between: so a result is its
/// round's by its target alone, and a number on the wire is a second check, not what the
/// matching rests on. Rounds for different targets run side by side, and only one is waited
/// for: the one last asked for. The others were given up, and their results are strays unless
/// their target is asked for again before they report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot<T = ()> {
    /// Rounds begun so far: the number of the last one.
    rounds: Round,
    /// The rounds that have not reported: one for a target at most, and one waited for at
    /// most.
    out: Vec<Out<T>>,
    stale: bool,
}

/// A round that has not reported.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Out<T> {
    round: Round,
    what: T,
    /// Its result is waited for. `false`: given up.
    wanted: bool,
    /// Asked for again since it began.
    again: bool,
    /// Made void since it began: it belongs to this round, whichever other round reports
    /// meanwhile.
    void: bool,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Slot {
            rounds: 0,
            out: Vec::new(),
            stale: false,
        }
    }
}

impl<T> Slot<T> {
    /// What the round waited for is for.
    pub fn running(&self) -> Option<&T> {
        self.out.iter().find(|out| out.wanted).map(|out| &out.what)
    }

    /// A round is waited for.
    pub fn is_running(&self) -> bool {
        self.running().is_some()
    }

    /// The number of the last round begun; 0 before the first.
    pub fn round(&self) -> Round {
        self.rounds
    }

    /// What was found before the last [`invalidate`](Slot::invalidate) is all there is: no
    /// round begun after it is [`Claim::Done`] yet. For work with one target, whose last
    /// result is kept by the caller; the rounds themselves are judged one by one
    /// ([`Claim::Void`]).
    pub fn stale(&self) -> bool {
        self.stale
    }

    /// What has been found so far, and what every round that is out will find, no longer
    /// counts: those rounds are void ([`Claim::Void`]), each for itself and whatever reports
    /// in between, the one waited for runs once more when it reports, and the slot is
    /// [`stale`](Slot::stale). Nothing begins here.
    pub fn invalidate(&mut self) {
        self.stale = true;
        for out in &mut self.out {
            out.void = true;
        }
    }

    /// The same slot with its targets seen through `f`.
    pub fn map<U>(&self, mut f: impl FnMut(&T) -> U) -> Slot<U> {
        Slot {
            rounds: self.rounds,
            out: self
                .out
                .iter()
                .map(|out| Out {
                    round: out.round,
                    what: f(&out.what),
                    wanted: out.wanted,
                    again: out.again,
                    void: out.void,
                })
                .collect(),
            stale: self.stale,
        }
    }

    fn begin(&mut self, what: T) -> Round {
        self.rounds += 1;
        self.out.push(Out {
            round: self.rounds,
            what,
            wanted: true,
            again: false,
            void: false,
        });
        self.rounds
    }

    /// The round at `i` has reported.
    fn reported(&mut self, i: usize) -> Claim {
        let out = self.out.remove(i);
        if !out.wanted {
            Claim::Stray
        } else if out.void {
            Claim::Void(self.begin(out.what))
        } else if out.again {
            Claim::Again(self.begin(out.what))
        } else {
            // Not void: it began after the last invalidation.
            self.stale = false;
            Claim::Done
        }
    }
}

impl<T: Clone> Slot<T> {
    /// The round waited for is given up: its target is returned, and its result will be a
    /// [`Claim::Stray`]. It is still out: asked for again before it reports, its target does
    /// not get a second round beside it, and what it found does not become [`Claim::Done`]
    /// either: the work runs once more when it reports ([`Claim::Again`]).
    pub fn cancel(&mut self) -> Option<T> {
        let out = self.out.iter_mut().find(|out| out.wanted)?;
        out.wanted = false;
        Some(out.what.clone())
    }
}

impl<T: PartialEq> Slot<T> {
    /// Asks for the work for `what`, which becomes the target waited for. A round for it is
    /// out: that one is waited for, and `None` says to start nothing. Otherwise a round
    /// begins, and its number is returned. A round waited for another target is given up.
    pub fn start(&mut self, what: T) -> Option<Round> {
        self.ask(what, false)
    }

    /// Asks for the work for `what` because what it reads has changed. A round for it is out:
    /// it began before the change, so the work runs once more when it reports
    /// ([`Claim::Again`]), and `None` says to start nothing now. Otherwise as
    /// [`start`](Slot::start).
    pub fn restart(&mut self, what: T) -> Option<Round> {
        self.ask(what, true)
    }

    fn ask(&mut self, what: T, changed: bool) -> Option<Round> {
        let found = self.out.iter().position(|out| out.what == what);
        for (i, out) in self.out.iter_mut().enumerate() {
            if Some(i) == found {
                // A round that had been given up began before this was asked.
                out.again |= changed || !out.wanted;
                out.wanted = true;
            } else {
                out.wanted = false;
                out.again = false;
            }
        }
        match found {
            Some(_) => None,
            None => Some(self.begin(what)),
        }
    }

    /// A result that carries the number of its round has arrived for `what`. A number that is
    /// not that of the round out for `what` is a [`Claim::Stray`], and leaves the slot as it
    /// was.
    pub fn claim(&mut self, round: Round, what: &T) -> Claim {
        let found = self
            .out
            .iter()
            .position(|out| out.round == round && out.what == *what);
        found.map_or(Claim::Stray, |i| self.reported(i))
    }

    /// A result that carries no number has arrived for `what`: the round out for `what` is the
    /// one, since there is no other.
    pub fn settle(&mut self, what: &T) -> Claim {
        let found = self.out.iter().position(|out| out.what == *what);
        found.map_or(Claim::Stray, |i| self.reported(i))
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

        // The round asked again keeps its target; another target gets a round of its own.
        let mut slot = Slot::default();
        slot.start("a");
        assert_eq!(slot.restart("a"), None);
        assert_eq!(slot.claim(1, &"a"), Claim::Again(2));
        assert_eq!(slot.running(), Some(&"a"));
        assert_eq!(slot.restart("b"), Some(3));
        assert_eq!(slot.claim(3, &"b"), Claim::Done);
    }

    /// Only the round waited for is claimed. Another number, a target with no round out and a
    /// second answer are strays that leave the slot as it was; the result of a round given up
    /// (for another target, or cancelled) is a stray too. A number is never given twice.
    #[test]
    fn a_late_result_is_a_stray() {
        let mut slot = Slot::default();
        assert_eq!(slot.start("a"), Some(1));
        assert_eq!(slot.start("b"), Some(2));
        let before = slot.clone();
        assert_eq!(slot.claim(2, &"a"), Claim::Stray);
        assert_eq!(slot.claim(1, &"b"), Claim::Stray);
        assert_eq!(slot.claim(3, &"b"), Claim::Stray);
        assert_eq!(slot.settle(&"c"), Claim::Stray);
        assert_eq!(slot, before);
        // `a` was left for `b`: what its round found is for no one, and nothing follows.
        assert_eq!(slot.claim(1, &"a"), Claim::Stray);
        assert_eq!(slot.running(), Some(&"b"));
        assert_eq!(slot.claim(1, &"a"), Claim::Stray);
        assert_eq!(slot.claim(2, &"b"), Claim::Done);
        assert_eq!(slot.claim(2, &"b"), Claim::Stray);

        // Cancelled: its answer does not count, and it is not run again.
        assert_eq!(slot.start("b"), Some(3));
        slot.restart("b");
        assert_eq!(slot.cancel(), Some("b"));
        assert_eq!(slot.cancel(), None);
        assert!(!slot.is_running());
        assert_eq!(slot.claim(3, &"b"), Claim::Stray);
        assert_eq!(slot.start("b"), Some(4));
        assert_eq!(slot.claim(3, &"b"), Claim::Stray);
        assert_eq!(slot.claim(4, &"b"), Claim::Done);
    }

    /// A target has one round out, whatever was asked in between. Asked for again while the
    /// round that was given up for it is still out, no second round begins beside it; that
    /// round began before the asking, so its result is never `Done`: the work runs once more.
    #[test]
    fn a_target_given_up_and_asked_again_waits_for_its_round_and_runs_once_more() {
        // Left for another target, and back.
        let mut slot = Slot::default();
        assert_eq!(slot.start("a"), Some(1));
        assert_eq!(slot.start("b"), Some(2));
        assert_eq!(slot.start("a"), None);
        assert_eq!(slot.running(), Some(&"a"));
        assert_eq!(slot.start("a"), None);
        // `b` was left in turn.
        assert_eq!(slot.settle(&"b"), Claim::Stray);
        // The only round out for `a` is the first: no other answer can be taken for it.
        assert_eq!(slot.settle(&"a"), Claim::Again(3));
        assert_eq!(slot.running(), Some(&"a"));
        assert_eq!(slot.settle(&"a"), Claim::Done);
        assert_eq!(slot.settle(&"a"), Claim::Stray);

        // Cancelled, and asked for again: by number as well.
        let mut slot = Slot::default();
        assert_eq!(slot.start("a"), Some(1));
        assert_eq!(slot.cancel(), Some("a"));
        assert_eq!(slot.start("a"), None);
        assert_eq!(slot.running(), Some(&"a"));
        assert_eq!(slot.claim(1, &"a"), Claim::Again(2));
        assert_eq!(slot.claim(1, &"a"), Claim::Stray);
        assert_eq!(slot.claim(2, &"a"), Claim::Done);

        // Given up again before it reports: nothing follows.
        let mut slot = Slot::default();
        slot.start("a");
        slot.start("b");
        slot.restart("a");
        slot.cancel();
        assert_eq!(slot.settle(&"a"), Claim::Stray);
        assert_eq!(slot.settle(&"b"), Claim::Stray);
        assert_eq!(slot.start("a"), Some(3));
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

        // Out: that round began before, so it is void, runs once more, and only the next one
        // clears it.
        slot.start(());
        slot.invalidate();
        assert_eq!(slot.start(()), None);
        assert_eq!(slot.settle(&()), Claim::Void(3));
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

        // The result of a round given up clears nothing.
        let mut slot = Slot::default();
        slot.start("a");
        slot.invalidate();
        slot.start("b");
        assert_eq!(slot.settle(&"a"), Claim::Stray);
        assert!(slot.stale());
        assert_eq!(slot.settle(&"b"), Claim::Done);
        assert!(!slot.stale());
    }

    /// Void is each round's own: a round that was out at the invalidation stays void whatever
    /// reports in between, also when it was given up and is asked for again. Rounds begun
    /// afterwards are not void.
    #[test]
    fn a_round_made_void_stays_void_whatever_reports_in_between() {
        let mut slot = Slot::default();
        assert_eq!(slot.start("a"), Some(1));
        slot.invalidate();
        // Another target, begun after: its result counts, and it is done.
        assert_eq!(slot.start("b"), Some(2));
        assert_eq!(slot.claim(2, &"b"), Claim::Done);
        assert!(Claim::Done.counts() && !slot.stale());
        // Back to `a`: its round is the one from before.
        assert_eq!(slot.start("a"), None);
        let claim = slot.claim(1, &"a");
        assert_eq!(claim, Claim::Void(3));
        assert!(!claim.counts());
        assert_eq!(claim.next(), Some(3));
        // The round that followed began after: asked again, its result counts.
        assert_eq!(slot.restart("a"), None);
        let claim = slot.claim(3, &"a");
        assert_eq!(claim, Claim::Again(4));
        assert!(claim.counts());
        assert_eq!(slot.claim(4, &"a"), Claim::Done);

        // Given up before the invalidation, void all the same.
        let mut slot = Slot::default();
        slot.start("a");
        slot.start("b");
        slot.invalidate();
        assert_eq!(slot.start("a"), None);
        assert_eq!(slot.settle(&"a"), Claim::Void(3));
        assert_eq!(slot.settle(&"b"), Claim::Stray);
        assert!(!Claim::Stray.counts());
        assert_eq!(Claim::Stray.next(), None);
    }

    #[test]
    fn a_mapped_slot_keeps_its_rounds() {
        let mut slot = Slot::default();
        slot.start(1);
        slot.start(2);
        slot.restart(2);
        let mut mapped = slot.map(|n| n.to_string());
        assert_eq!(mapped.running(), Some(&"2".to_string()));
        assert_eq!(mapped.claim(1, &"1".to_string()), Claim::Stray);
        assert_eq!(mapped.start("1".to_string()), Some(3));
        assert_eq!(mapped.claim(2, &"2".to_string()), Claim::Stray);

        let mut slot = Slot::default();
        slot.start(1);
        slot.invalidate();
        let mut mapped = slot.map(|n| n.to_string());
        assert!(mapped.stale());
        assert_eq!(mapped.settle(&"1".to_string()), Claim::Void(2));
    }
}
