//! The TUI's account listing (SPEC R3, R16): which accounts `config.toml` lists now, and what
//! else it says that the TUI goes by (shared configuration, R18; prices, R20).
//!
//! [`Listing::read`] is the only way to the accounts: work that goes over them reads the
//! registry when it starts, so none starts from a list older than the file, and a list that
//! changed is told to the app with one [`Event::Accounts`]. What a registry that cannot be read
//! means is answered here, once ([`Reading`]).

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, PoisonError};

use anyhow::Result;

use crate::Env;
use crate::pricing::Prices;
use crate::registry::{Account, Registry, Sharing};

use super::app::{self, Event};

/// The accounts of `config.toml` as last read, shared by the event loop and every worker
/// (one per TUI: clones of [`super::Deps`] hold the same one).
pub struct Listing {
    config: PathBuf,
    env: Env,
    state: Mutex<State>,
}

struct State {
    accounts: Vec<Account>,
    /// Every account listed since [`Listing::open`], in first-seen order (grow-only).
    seen: Vec<Account>,
}

/// What one [`Listing::read`] found. When the registry cannot be read, the accounts are the
/// last ones read, nothing is shared, the prices are the built-in ones, and `unreadable` says
/// why.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// The accounts to list now, in the registry's order ([`Registry::all`]); their homes are
    /// the registry's strings, byte for byte (R2).
    pub accounts: Vec<Account>,
    /// `accounts`, then every other account listed since the TUI started: one unregistered
    /// meanwhile may still run a session, so a resume in place asks these (R16).
    pub seen: Vec<Account>,
    pub sharing: Sharing,
    pub prices: Prices,
    /// Why `config.toml` could not be read or is not valid; the reason names the file.
    pub unreadable: Option<String>,
}

impl Listing {
    /// Reads the registry at `config` for the first time; `env` decides which implicit
    /// defaults are listed. A registry that cannot be read is an error: there is no list to
    /// start from.
    pub fn open(config: PathBuf, env: Env) -> Result<Self> {
        let accounts = Registry::load(&config)?.all(&env);
        let state = Mutex::new(State {
            seen: accounts.clone(),
            accounts,
        });
        Ok(Listing { config, env, state })
    }

    /// `$REMUDA_HOME/config.toml`, for what writes the registry (a setup, a removal) or names
    /// it; whoever changed it reads it again.
    pub fn config(&self) -> &Path {
        &self.config
    }

    /// Reads the registry again. A list that differs from the last one read replaces it and is
    /// told once, with an [`Event::Accounts`] that is in `tx` before this returns: ahead of
    /// whatever the caller sends there afterwards. Readers at the same time take turns, so
    /// the last list told is the one held.
    pub fn read(&self, tx: &Sender<Event>) -> Reading {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let registry = self.load(&mut state, tx);
        let accounts = state.accounts.clone();
        let seen = union(&[&accounts, &state.seen]);
        match registry {
            Ok(registry) => Reading {
                accounts,
                seen,
                sharing: registry.sharing,
                prices: registry.prices,
                unreadable: None,
            },
            Err(why) => Reading {
                accounts,
                seen,
                sharing: Sharing::default(),
                prices: Prices::default(),
                unreadable: Some(why),
            },
        }
    }

    /// Answers long work done over `accounts` (those of the [`Reading`] it started from):
    /// reads the registry once more and sends `result`, unless it lists other accounts by
    /// now. Then nothing is sent but the change, and the caller does the work again: the app
    /// is not given a result for accounts it no longer shows. The check and the send are one
    /// step, so a change found later is told after `result`, and the app starts the work
    /// again itself. Whether `result` was sent.
    pub fn answer(&self, tx: &Sender<Event>, accounts: &[Account], result: Event) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        // A registry that cannot be read keeps its accounts: the result is theirs.
        let _ = self.load(&mut state, tx);
        if state.accounts != accounts {
            return false;
        }
        let _ = tx.send(result);
        true
    }

    /// The registry as it is now, or why it cannot be read. A list that differs from the one
    /// held replaces it and is told.
    fn load(&self, state: &mut State, tx: &Sender<Event>) -> Result<Registry, String> {
        let registry = Registry::load(&self.config).map_err(|e| format!("{e:#}"))?;
        let accounts = registry.all(&self.env);
        if accounts != state.accounts {
            state.seen = union(&[&state.seen, &accounts]);
            state.accounts = accounts.clone();
            // A send error means the TUI has quit.
            let _ = tx.send(Event::Accounts(accounts));
        }
        Ok(registry)
    }
}

impl Reading {
    /// Why nothing is launched as `account`, if nothing is (R16): the registry cannot be read,
    /// or it no longer lists the account (one of that name with another home is another
    /// account, R2).
    pub fn refusal(&self, account: &Account) -> Option<String> {
        if let Some(why) = &self.unreadable {
            return Some(why.clone());
        }
        unlisted(&self.accounts, account)
    }
}

/// Why `account` is not one of `listed`, if it is not: it is no longer registered, or one of
/// its name is, with another home, which is another account (R2). The app asks this of its
/// rows for what it held on to while they changed (an open form, a prompt), the launch of the
/// registry itself ([`Reading::refusal`]).
pub fn unlisted<'a>(
    listed: impl IntoIterator<Item = &'a Account>,
    account: &Account,
) -> Option<String> {
    let mut again = false;
    for other in listed {
        if other == account {
            return None;
        }
        again |= other.provider == account.provider && other.name == account.name;
    }
    let qualified = account.qualified();
    let name = app::short_account(&qualified);
    Some(if again {
        format!("{name} is now registered with another home")
    } else {
        format!("{name} is no longer registered")
    })
}

/// Every account of `lists`, once, in first-seen order.
fn union(lists: &[&[Account]]) -> Vec<Account> {
    let mut all: Vec<Account> = Vec::new();
    for account in lists.iter().copied().flatten() {
        if !all.contains(account) {
            all.push(account.clone());
        }
    }
    all
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::sync::mpsc::{self, Receiver};
    use std::thread;

    use super::*;
    use crate::registry::{CLAUDE, Home};

    fn account(name: &str, home: &str) -> Account {
        Account {
            provider: CLAUDE,
            name: name.into(),
            home: Home::Path(home.into()),
        }
    }

    fn default() -> Account {
        Account::default_for(CLAUDE)
    }

    /// `config.toml` with these claude accounts, as another `remuda` would leave it.
    fn register(config: &Path, accounts: &[&Account]) {
        let mut text = String::new();
        for a in accounts {
            text.push_str(&format!(
                "[[account]]\nprovider = \"claude\"\nname = \"{}\"\nhome = \"{}\"\n\n",
                a.name, a.home
            ));
        }
        fs::write(config, text).unwrap();
    }

    /// A listing opened on `accounts`; no `PATH` and no `~/.codex`, so codex's default is not
    /// listed.
    fn listing(dir: &Path, accounts: &[&Account]) -> Listing {
        let config = dir.join("config.toml");
        register(&config, accounts);
        let env: Env = [("HOME".to_string(), dir.join("home").display().to_string())].into();
        Listing::open(config, env).unwrap()
    }

    fn told(rx: &Receiver<Event>) -> Vec<Event> {
        rx.try_iter().collect()
    }

    /// R16: the same accounts again are not told; the registry's order is kept, the implicit
    /// default first.
    #[test]
    fn the_same_accounts_again_tell_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (max, team) = (account("max", "/p/max"), account("team", "/p/team"));
        let listing = listing(dir.path(), &[&max, &team]);
        let (tx, rx) = mpsc::channel();
        for _ in 0..3 {
            let reading = listing.read(&tx);
            assert_eq!(reading.accounts, [default(), max.clone(), team.clone()]);
            assert_eq!(reading.seen, reading.accounts);
            assert_eq!(reading.unreadable, None);
        }
        assert_eq!(told(&rx), []);
        // Rewritten with the same accounts: still nothing to tell.
        register(listing.config(), &[&max, &team]);
        listing.read(&tx);
        assert_eq!(told(&rx), []);
    }

    /// R16: a change is told once, by the read that finds it, before that read returns.
    #[test]
    fn a_change_is_told_once() {
        let dir = tempfile::tempdir().unwrap();
        let (max, team) = (account("max", "/p/max"), account("team", "/p/team"));
        let listing = listing(dir.path(), &[&max, &team]);
        let (tx, rx) = mpsc::channel();
        register(listing.config(), &[&max]);
        let reading = listing.read(&tx);
        assert_eq!(reading.accounts, [default(), max.clone()]);
        assert_eq!(
            told(&rx),
            [Event::Accounts(vec![default(), max.clone()])],
            "in the channel when the read returns"
        );
        listing.read(&tx);
        listing.read(&tx);
        assert_eq!(told(&rx), [], "later reads find nothing new");
        // Each further change is one more event.
        register(listing.config(), &[&team, &max]);
        listing.read(&tx);
        assert_eq!(
            told(&rx),
            [Event::Accounts(vec![default(), team, max])],
            "a new order is a change"
        );
    }

    /// Workers read at the same time: one change is one event, whoever finds it.
    #[test]
    fn readers_at_once_tell_one_change_once() {
        let dir = tempfile::tempdir().unwrap();
        let (max, team) = (account("max", "/p/max"), account("team", "/p/team"));
        let listing = Arc::new(listing(dir.path(), &[&max, &team]));
        let (tx, rx) = mpsc::channel();
        register(listing.config(), &[&max]);
        let readers: Vec<_> = (0..16)
            .map(|_| {
                let (listing, tx) = (Arc::clone(&listing), tx.clone());
                thread::spawn(move || listing.read(&tx).accounts)
            })
            .collect();
        for reader in readers {
            assert_eq!(reader.join().unwrap(), [default(), max.clone()]);
        }
        assert_eq!(told(&rx), [Event::Accounts(vec![default(), max])]);
    }

    /// The one answer to a registry that cannot be read: the accounts stay as last read and
    /// nothing is told; nothing is shared, the prices are the built-in ones, and the reason
    /// names the file.
    #[test]
    fn an_unreadable_registry_keeps_the_accounts_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let max = account("max", "/p/max");
        let listing = listing(dir.path(), &[&max]);
        let (tx, rx) = mpsc::channel();
        fs::write(
            listing.config(),
            "[[account]]\nprovider = \"claude\"\nname = \"max\"\nhome = \"/p/max\"\n\
             [share.claude]\nfrom = \"max\"\n\
             [prices.\"claude-test\"]\ninput = 1\noutput = 1\n",
        )
        .unwrap();
        let readable = listing.read(&tx);
        assert_eq!(readable.sharing.source, Some(max.clone()));
        assert_eq!(readable.prices.overrides.len(), 1);
        assert_eq!(told(&rx), []);

        fs::write(listing.config(), "[[account]]\nprovider = 7\n").unwrap();
        let reading = listing.read(&tx);
        assert_eq!(reading.accounts, [default(), max.clone()]);
        assert_eq!(reading.seen, reading.accounts);
        assert_eq!(reading.sharing, Sharing::default());
        assert_eq!(reading.prices, Prices::default());
        let why = reading.unreadable.clone().unwrap();
        assert!(why.contains("config.toml"), "{why}");
        assert_eq!(told(&rx), []);
        // Nothing is launched from it, not even as an account it listed.
        assert_eq!(reading.refusal(&max), Some(why.clone()));
        assert_eq!(reading.refusal(&default()), Some(why));

        // Readable again with the same accounts: still nothing to tell.
        register(listing.config(), &[&max]);
        let reading = listing.read(&tx);
        assert_eq!((&reading.unreadable, reading.refusal(&max)), (&None, None));
        assert_eq!(told(&rx), []);
    }

    /// A registry that cannot be read when the TUI starts gives no list to start from; a
    /// missing one is empty.
    #[test]
    fn opening_needs_a_registry_that_can_be_read() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let env: Env = [("HOME".to_string(), "/nohome".to_string())].into();
        let (tx, rx) = mpsc::channel();
        let listing = Listing::open(config.clone(), env.clone()).unwrap();
        assert_eq!(listing.read(&tx).accounts, [default()]);
        assert_eq!(told(&rx), []);
        fs::write(&config, "not toml [").unwrap();
        assert!(Listing::open(config, env).is_err());
    }

    /// R16: an account that leaves the registry stays among those a resume in place asks,
    /// after the listed ones, however often the registry changes.
    #[test]
    fn accounts_listed_before_stay_seen() {
        let dir = tempfile::tempdir().unwrap();
        let (max, team) = (account("max", "/p/max"), account("team", "/p/team"));
        let work = account("work", "/p/work");
        let listing = listing(dir.path(), &[&max, &team]);
        let (tx, _rx) = mpsc::channel();
        register(listing.config(), &[&max]);
        let reading = listing.read(&tx);
        assert_eq!(reading.accounts, [default(), max.clone()]);
        assert_eq!(reading.seen, [default(), max.clone(), team.clone()]);
        register(listing.config(), &[&work]);
        let reading = listing.read(&tx);
        assert_eq!(reading.accounts, [default(), work.clone()]);
        assert_eq!(
            reading.seen,
            [default(), work.clone(), max.clone(), team.clone()]
        );
        // Back again: listed, and seen once.
        register(listing.config(), &[&max, &team]);
        let reading = listing.read(&tx);
        assert_eq!(
            reading.seen,
            [default(), max.clone(), team.clone(), work.clone()]
        );
        // While the registry cannot be read, the same accounts are asked.
        fs::write(listing.config(), "not toml [").unwrap();
        assert_eq!(listing.read(&tx).seen, [default(), max, team, work]);
    }

    /// R16, R20: the result of long work is for the accounts it started from: sent while the
    /// registry still lists them, held back (and the change told instead) once it does not.
    #[test]
    fn a_result_for_accounts_that_changed_meanwhile_is_not_sent() {
        let dir = tempfile::tempdir().unwrap();
        let (max, team) = (account("max", "/p/max"), account("team", "/p/team"));
        let listing = listing(dir.path(), &[&max, &team]);
        let (tx, rx) = mpsc::channel();
        let result = || Event::Resize(1, 1);
        let started = listing.read(&tx).accounts;
        assert!(listing.answer(&tx, &started, result()));
        assert_eq!(told(&rx), [result()]);

        // The registry changes while the work runs: the change is told, the result is not.
        register(listing.config(), &[&max]);
        assert!(!listing.answer(&tx, &started, result()));
        assert_eq!(told(&rx), [Event::Accounts(vec![default(), max.clone()])]);
        // The app already knows of the change: still not sent.
        assert!(!listing.answer(&tx, &started, result()));
        assert_eq!(told(&rx), []);
        // Done again from the list as it is, the work is answered.
        let again = listing.read(&tx).accounts;
        assert!(listing.answer(&tx, &again, result()));
        assert_eq!(told(&rx), [result()]);

        // A registry that cannot be read keeps its accounts: their result is sent.
        fs::write(listing.config(), "not toml [").unwrap();
        assert!(listing.answer(&tx, &again, result()));
        assert_eq!(told(&rx), [result()]);
    }

    /// R2: a home is the registry's string, byte for byte: a trailing `/`, `//` and `.` stay.
    #[test]
    fn homes_are_the_registrys_strings() {
        let dir = tempfile::tempdir().unwrap();
        let slash = account("slash", "/p/max/");
        let odd = account("odd", "/p//a/./b ü");
        let listing = listing(dir.path(), &[&slash]);
        let (tx, rx) = mpsc::channel();
        assert_eq!(listing.read(&tx).accounts[1].home.to_string(), "/p/max/");
        register(listing.config(), &[&slash, &odd]);
        let reading = listing.read(&tx);
        assert_eq!(reading.accounts[2].home.to_string(), "/p//a/./b ü");
        assert_eq!(told(&rx), [Event::Accounts(vec![default(), slash, odd])]);
    }

    /// R16: nothing is launched as an account the registry no longer lists; the same name with
    /// another home is another account (R2).
    #[test]
    fn an_account_no_longer_listed_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (max, team) = (account("max", "/p/max"), account("team", "/p/team"));
        let listing = listing(dir.path(), &[&max, &team]);
        let (tx, _rx) = mpsc::channel();
        let reading = listing.read(&tx);
        assert_eq!(reading.refusal(&max), None);
        assert_eq!(reading.refusal(&default()), None);

        let moved = account("max", "/p/max/");
        register(listing.config(), &[&moved]);
        let reading = listing.read(&tx);
        assert_eq!(
            reading.refusal(&team).as_deref(),
            Some("team is no longer registered")
        );
        assert_eq!(
            reading.refusal(&max).as_deref(),
            Some("max is now registered with another home")
        );
        assert_eq!(reading.refusal(&moved), None);
        let codex = Account {
            provider: crate::registry::CODEX,
            name: "work".into(),
            home: Home::Path("/p/work".into()),
        };
        assert_eq!(
            reading.refusal(&codex).as_deref(),
            Some("codex:work is no longer registered")
        );
    }
}
