//! `remuda pick` (SPEC R23): which account and model to launch now, and at what effort. Rules
//! decide what is feasible (headroom on every window that applies, exclusions, logins) and rank
//! it; with a key and notes, Jev chooses among the feasible options ([`crate::jev`]), and its
//! answer is taken only when it is confident enough. What a usage window holds now is
//! [`crate::usage::snapshot`]'s to say.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Result, bail};
use jiff::Timestamp;
use serde_json::{Value, json};
use toml_edit::DocumentMut;

use crate::account_command::Runner;
use crate::identity::{self, Identity};
use crate::index::{self, Index};
use crate::launch::{self, Intent};
use crate::provider::{Provider, codex};
use crate::registry::{Account, Registry};
use crate::transcript::{self, SessionTail};
use crate::usage::{self, CachedUsage, LiveUsage, Reading, Snapshot, Source, UsageRow, Window};
use crate::{Env, attribution, probe, wait};

/// Percent left required on every window that applies, unless `[pick] min_headroom` says (R23).
pub const DEFAULT_MIN_HEADROOM: u32 = 10;
/// Minutes after which cached usage is stale, unless `[pick] stale_after` says (R23).
pub const DEFAULT_STALE_AFTER: u32 = 120;
/// Minutes after its last activity that the account which ran a session stays preferred for
/// resuming it, unless `[pick] affinity_minutes` says (R23): claude writes its prompt cache for
/// an hour (1h ephemeral, verified on 2.1.292).
pub const DEFAULT_AFFINITY_MINUTES: u32 = 60;
/// The longest `[pick] affinity_minutes`: a day.
pub const MAX_AFFINITY_MINUTES: u32 = 1440;
/// The longest `[pick] notes`, in characters (R3).
pub const MAX_NOTES: usize = 4000;
/// Jev's pair is taken from this confidence on (R23).
pub const PAIR_CONFIDENCE: f64 = 0.50;
/// ... else its most probable account, when the probabilities of its pairs add up to this (R23).
pub const ACCOUNT_PROBABILITY: f64 = 0.70;
/// Jev's effort is taken from this confidence on (R23).
pub const EFFORT_CONFIDENCE: f64 = 0.50;
/// The most options one Choice may offer (R23).
pub const MAX_OPTIONS: usize = 255;
/// Why an account whose store does not hold the session resumed is not feasible (R23).
pub const NOT_SEEN: &str = "cannot see this session (its store is not this account's)";
/// How long `codex login status` may take (R4: about 0.05 s), as for `remuda list`.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(15);
/// The fewest seconds a pace spreads what is left over, an hour (R23): a window that resets in a
/// minute would otherwise outrank everything.
pub const MIN_PACE_SECONDS: i64 = 3600;
/// A window this long or longer is a budget window, whose pace counts first (R23): what is left
/// of it at its reset is lost, while a shorter window refills within the budget.
pub const BUDGET_WINDOW: jiff::SignedDuration = jiff::SignedDuration::from_hours(24);
/// A pace below this share of its band's top opens a new band (R23): nine tenths, as
/// `(numerator, denominator)`, so that the boundary is compared in whole numbers
/// ([`Pace::below_band_of`]).
pub const PACE_BAND: (i128, i128) = (9, 10);
/// The unit a pace's percent left is counted in: a millionth of a percent.
const PACE_LEFT_UNIT: f64 = 1_000_000.0;

/// A pair's pace (R23 Rules) as two whole numbers: the percent left in millionths of a percent
/// (rounded to the nearest), and the seconds until the reset it is spread over, never fewer than
/// [`MIN_PACE_SECONDS`]. Every comparison of paces, the order, equality and the bands, is
/// made on these exactly, by cross products in `i128` ([`Pace::cmp_pace`],
/// [`Pace::below_band_of`]): no rounding can turn one over. The quotient, [`Pace::per_hour`],
/// is only shown (the text report, `--json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pace {
    pub left_micro: i64,
    pub seconds: i64,
}

impl Pace {
    /// `left` percent spread over `seconds`, at least [`MIN_PACE_SECONDS`]. A percentage is at
    /// most a few hundred and a time a few hundred billion seconds (jiff's range), so the cross
    /// products below stay far inside `i128`.
    pub fn new(left: f64, seconds: i64) -> Pace {
        Pace {
            left_micro: (left * PACE_LEFT_UNIT).round().clamp(0.0, 1e15) as i64,
            seconds: seconds.max(MIN_PACE_SECONDS),
        }
    }

    /// Percent left per hour until reset.
    pub fn per_hour(self) -> f64 {
        self.left_micro as f64 / PACE_LEFT_UNIT / (self.seconds as f64 / 3600.0)
    }

    /// This pace against `other`, exactly: `left / seconds` against `other.left /
    /// other.seconds`, as `left × other.seconds` against `other.left × seconds`. Paces over
    /// different seconds with the same quotient are equal.
    pub fn cmp_pace(self, other: Pace) -> std::cmp::Ordering {
        (i128::from(self.left_micro) * i128::from(other.seconds))
            .cmp(&(i128::from(other.left_micro) * i128::from(self.seconds)))
    }

    /// Whether this pace is below [`PACE_BAND`] of `top`'s, exactly: `10 × left × top.seconds <
    /// 9 × top.left × seconds`. One exactly on the boundary is not below: it is in the band.
    fn below_band_of(self, top: Pace) -> bool {
        PACE_BAND.1 * i128::from(self.left_micro) * i128::from(top.seconds)
            < PACE_BAND.0 * i128::from(top.left_micro) * i128::from(self.seconds)
    }
}

/// `[pick]` of `config.toml` (R3, R23).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// `provider:name` of the accounts never recommended.
    pub exclude: Vec<String>,
    /// `provider:name` in the order that breaks the rules' last ties.
    pub prefer: Vec<String>,
    pub min_headroom: u32,
    /// Minutes.
    pub stale_after: u32,
    /// Minutes; 0: a session's last account is never preferred.
    pub affinity_minutes: u32,
    /// How the rules rank the feasible pairs.
    pub strategy: Strategy,
    /// Free text sent to Jev; empty: none.
    pub notes: String,
    pub claude: Choices,
    pub codex: Choices,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            exclude: Vec::new(),
            prefer: Vec::new(),
            min_headroom: DEFAULT_MIN_HEADROOM,
            stale_after: DEFAULT_STALE_AFTER,
            affinity_minutes: DEFAULT_AFFINITY_MINUTES,
            strategy: Strategy::default(),
            notes: String::new(),
            claude: Choices::default(),
            codex: Choices::default(),
        }
    }
}

/// `[pick.<provider>]`: what may be injected at launch. Empty lists: the agent's own default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Choices {
    /// In order of preference; the first is the default.
    pub models: Vec<String>,
    /// From low to high.
    pub efforts: Vec<String>,
    pub default_effort: Option<String>,
}

/// `[pick] strategy`: how the rules rank the feasible pairs (R23 Rules).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Strategy {
    /// By the least percent left, in 10-point bands.
    #[default]
    Headroom,
    /// By the percent left per hour until the reset of the tightest budget window, in bands of
    /// a tenth.
    Pace,
}

impl Strategy {
    pub fn name(self) -> &'static str {
        match self {
            Strategy::Headroom => "headroom",
            Strategy::Pace => "pace",
        }
    }
}

const KEYS: [&str; 9] = [
    "exclude",
    "prefer",
    "min_headroom",
    "stale_after",
    "affinity_minutes",
    "strategy",
    "notes",
    "claude",
    "codex",
];
const PROVIDER_KEYS: [&str; 3] = ["models", "efforts", "default_effort"];

impl Config {
    /// `[pick]` of `doc` (R3); without it, the defaults. Account names resolve against
    /// `registry` as in R1. Errors name the table.
    pub fn from_document(doc: &DocumentMut, registry: &Registry) -> Result<Config> {
        let mut config = Config::default();
        let Some(item) = doc.get("pick") else {
            return Ok(config);
        };
        let Some(table) = item.as_table_like() else {
            bail!("`pick` must be a table ([pick])");
        };
        for (key, value) in table.iter() {
            match key {
                "exclude" | "prefer" => {
                    let names = strings(value).ok_or_else(|| {
                        anyhow::anyhow!("[pick]: `{key}` must be an array of account names")
                    })?;
                    let mut resolved: Vec<String> = Vec::new();
                    for name in names {
                        let account = registry
                            .resolve(&name)
                            .map_err(|e| anyhow::anyhow!("[pick]: {key} = {name:?}: {e:#}"))?;
                        if !resolved.contains(&account.qualified()) {
                            resolved.push(account.qualified());
                        }
                    }
                    if key == "exclude" {
                        config.exclude = resolved;
                    } else {
                        config.prefer = resolved;
                    }
                }
                "min_headroom" => match value.as_integer() {
                    Some(n) if (0..=100).contains(&n) => config.min_headroom = n as u32,
                    _ => bail!("[pick]: `min_headroom` must be an integer from 0 to 100 (percent)"),
                },
                "stale_after" => match value.as_integer() {
                    Some(n) if n > 0 && n <= i64::from(u32::MAX) => config.stale_after = n as u32,
                    _ => bail!("[pick]: `stale_after` must be a positive integer (minutes)"),
                },
                "affinity_minutes" => match value.as_integer() {
                    Some(n) if (0..=i64::from(MAX_AFFINITY_MINUTES)).contains(&n) => {
                        config.affinity_minutes = n as u32
                    }
                    _ => bail!(
                        "[pick]: `affinity_minutes` must be an integer from 0 to \
                         {MAX_AFFINITY_MINUTES} (minutes)"
                    ),
                },
                "strategy" => match value.as_str() {
                    Some("headroom") => config.strategy = Strategy::Headroom,
                    Some("pace") => config.strategy = Strategy::Pace,
                    _ => bail!("[pick]: `strategy` must be \"headroom\" or \"pace\""),
                },
                "notes" => {
                    let Some(notes) = value.as_str() else {
                        bail!("[pick]: `notes` must be a string");
                    };
                    if notes.chars().count() > MAX_NOTES {
                        bail!("[pick]: `notes` is longer than {MAX_NOTES} characters");
                    }
                    config.notes = notes.trim().to_string();
                }
                "claude" => config.claude = choices(value, Provider::Claude)?,
                "codex" => config.codex = choices(value, Provider::Codex)?,
                other => bail!("[pick]: unknown key `{other}` (known: {})", KEYS.join(", ")),
            }
        }
        Ok(config)
    }

    pub fn choices(&self, provider: Provider) -> &Choices {
        match provider {
            Provider::Claude => &self.claude,
            Provider::Codex => &self.codex,
        }
    }

    /// This configuration for resuming or forking a session (R23): the session's model is the
    /// session's, and nothing is injected, so neither `models` nor `efforts` nor
    /// `default_effort` has a say.
    pub fn for_resume(&self) -> Config {
        Config {
            claude: Choices::default(),
            codex: Choices::default(),
            ..self.clone()
        }
    }
}

/// The strings of an array value; `None` if it is not an array of strings.
fn strings(item: &toml_edit::Item) -> Option<Vec<String>> {
    item.as_array()?
        .iter()
        .map(|v| v.as_str().map(str::to_string))
        .collect()
}

fn choices(item: &toml_edit::Item, provider: Provider) -> Result<Choices> {
    let t = format!("[pick.{provider}]");
    let Some(table) = item.as_table_like() else {
        bail!("{t} must be a table");
    };
    let mut out = Choices::default();
    for (key, value) in table.iter() {
        match key {
            "models" => {
                let Some(models) = strings(value) else {
                    bail!("{t}: `models` must be an array of model names");
                };
                for model in &models {
                    if !valid_model(model) {
                        bail!(
                            "{t}: invalid model {model:?}: must match [A-Za-z0-9._:-]+ and not \
                             start with `-`"
                        );
                    }
                    check_not_subcommand(&t, provider, model)?;
                }
                if let Some(dup) = duplicate(&models) {
                    bail!("{t}: model {dup:?} is listed twice");
                }
                out.models = models;
            }
            "efforts" => {
                let Some(efforts) = strings(value) else {
                    bail!("{t}: `efforts` must be an array of effort levels");
                };
                for effort in &efforts {
                    if effort.is_empty() || !effort.bytes().all(|b| b.is_ascii_lowercase()) {
                        bail!("{t}: invalid effort {effort:?}: must match [a-z]+");
                    }
                    check_not_subcommand(&t, provider, effort)?;
                }
                if let Some(dup) = duplicate(&efforts) {
                    bail!("{t}: effort {dup:?} is listed twice");
                }
                out.efforts = efforts;
            }
            "default_effort" => {
                let Some(effort) = value.as_str() else {
                    bail!("{t}: `default_effort` must be a string");
                };
                out.default_effort = Some(effort.to_string());
            }
            other => bail!(
                "{t}: unknown key `{other}` (known: {})",
                PROVIDER_KEYS.join(", ")
            ),
        }
    }
    if let Some(effort) = &out.default_effort
        && !out.efforts.contains(effort)
    {
        bail!("{t}: default_effort = {effort:?} is not one of `efforts`");
    }
    Ok(out)
}

/// A claude model or effort that is a claude subcommand would turn `pick --run` into that
/// subcommand (R6's classification sees subcommand names anywhere).
fn check_not_subcommand(table: &str, provider: Provider, value: &str) -> Result<()> {
    if provider == Provider::Claude && launch::SUBCOMMANDS.contains(&value) {
        bail!("{table}: {value:?} is a claude subcommand, not a model or effort");
    }
    Ok(())
}

/// `[A-Za-z0-9._:-]+`, not starting with `-` (it goes to the agent as an option's value).
fn valid_model(model: &str) -> bool {
    !model.is_empty()
        && !model.starts_with('-')
        && model
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".:_-".contains(&b))
}

fn duplicate(items: &[String]) -> Option<&String> {
    items
        .iter()
        .enumerate()
        .find(|(i, item)| items[..*i].contains(item))
        .map(|(_, item)| item)
}

/// A claude model's family: `claude-fable-5-1` → `fable`; a bare alias is its own family.
pub fn family(model: &str) -> &str {
    let rest = model.strip_prefix("claude-").unwrap_or(model);
    rest.split('-').next().unwrap_or(rest)
}

/// Whether the per-model window named `name` limits `model` (R23): claude by family, codex by
/// id, both ignoring case.
pub fn limits_model(provider: Provider, model: &str, name: &str) -> bool {
    match provider {
        Provider::Claude => family(model).eq_ignore_ascii_case(name),
        Provider::Codex => model.eq_ignore_ascii_case(name),
    }
}

/// Whether a session is resumed in place or forked (R23 Resuming).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Resume,
    Fork,
}

impl SessionKind {
    pub fn name(self) -> &'static str {
        match self {
            SessionKind::Resume => "resume",
            SessionKind::Fork => "fork",
        }
    }
}

/// The session the arguments after `--` resume or fork (R23 Resuming).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resume {
    /// Whose arguments name it: only this provider's accounts can resume it.
    pub provider: Provider,
    pub kind: SessionKind,
    pub id: String,
}

/// What arguments do for one provider's agent.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Opens {
    New,
    Session(SessionKind, String),
    /// Neither a new session nor one named: a continue, a resume without an id, a subcommand.
    Other,
}

/// What `args` do for `provider`'s agent (R23): claude by R6's classification, accepting of
/// the existing sessions only `--resume <id>` (`-r <id>`, `--resume=<id>`) with nothing else
/// resuming, and the fork `--resume <id> --fork-session`; codex by `resume <id>` / `fork <id>`
/// first (R17).
fn opens(provider: Provider, args: &[String]) -> Opens {
    match provider {
        Provider::Claude => match launch::classify(args) {
            Intent::NewSession => Opens::New,
            // R6 takes the first of several resumes for the fork: here, as for a resume, there
            // must be exactly one. Without it and `--fork-session`, what is left must start a
            // new session.
            Intent::Fork { of }
                if launch::classify(&without_fork(&without_resume(args, &of)))
                    == Intent::NewSession =>
            {
                Opens::Session(SessionKind::Fork, of)
            }
            // `--session-id <id>` alone reads so too: that is a new session's id. Without the
            // resume, what is left must start a new session: no other resume, continue or id.
            Intent::Existing {
                session_id: Some(id),
                fork_of: None,
            } if launch::classify(&without_resume(args, &id)) == Intent::NewSession => {
                Opens::Session(SessionKind::Resume, id)
            }
            _ => Opens::Other,
        },
        Provider::Codex => match args.first().map(String::as_str) {
            Some("resume" | "fork") => match launch::codex_intent(args) {
                Intent::Existing {
                    session_id: Some(id),
                    ..
                } => Opens::Session(SessionKind::Resume, id),
                Intent::Existing {
                    fork_of: Some(id), ..
                } => Opens::Session(SessionKind::Fork, id),
                _ => Opens::Other,
            },
            _ => Opens::New,
        },
    }
}

/// `args` without `--fork-session`.
fn without_fork(args: &[String]) -> Vec<String> {
    args.iter()
        .filter(|a| *a != "--fork-session")
        .cloned()
        .collect()
}

/// `args` without their first `--resume <id>`, `-r <id>` or `--resume=<id>`.
fn without_resume(args: &[String], id: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut done = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if !done {
            if matches!(arg, "--resume" | "-r") && args.get(i + 1).map(String::as_str) == Some(id) {
                done = true;
                i += 2;
                continue;
            }
            if arg.strip_prefix("--resume=") == Some(id) {
                done = true;
                i += 1;
                continue;
            }
        }
        out.push(args[i].clone());
        i += 1;
    }
    out
}

/// The session `args` resume or fork, by whichever agent's reading names one (R23 Resuming);
/// `None` when neither does: a new session, or arguments [`run_args`] refuses. Arguments both
/// readings take for a session (`resume x --resume y`) are refused as ambiguous.
pub fn session_args(args: &[String]) -> Result<Option<Resume>> {
    let mut named = Provider::ALL
        .into_iter()
        .filter_map(|provider| match opens(provider, args) {
            Opens::Session(kind, id) => Some(Resume { provider, kind, id }),
            _ => None,
        });
    let first = named.next();
    if let (Some(a), Some(b)) = (&first, named.next()) {
        bail!(
            "these arguments name a session for claude ({}) and for codex ({}): ambiguous",
            a.id,
            b.id
        );
    }
    Ok(first)
}

/// The account that last ran a session, and when (R23 Resuming).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastRun {
    /// `provider:name`.
    pub account: String,
    /// The home string the launch log has for it, byte for byte (R2): an account registered
    /// again under that name with another home is another login, with another cache.
    pub home: String,
    pub launched_at: Timestamp,
    /// The later of the launch and the last record of what it ran, where that can be told.
    pub active_at: Timestamp,
}

impl LastRun {
    /// Whether `account` is the one that ran it: the same name and the same home string.
    pub fn is(&self, account: &Account) -> bool {
        account.qualified() == self.account && account.home.to_string() == self.home
    }
}

/// The session resumed or forked, and what remuda knows of it (R23 Resuming).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub resume: Resume,
    /// From the launch log; `None` when remuda never launched it.
    pub last: Option<LastRun>,
    /// For each account that can see the session (`provider:name`), the model of the copy it
    /// would resume: the one in its own store, from the end of that transcript; `None` when
    /// unknown. `None` as a whole when the session index does not have the session, and so
    /// cannot say who sees it.
    pub seen_by: Option<BTreeMap<String, Option<String>>>,
}

impl Session {
    /// Seconds since its last account's last activity at `now`; never negative.
    pub fn age(&self, now: Timestamp) -> Option<i64> {
        let last = self.last.as_ref()?;
        Some((now.as_second() - last.active_at.as_second()).max(0))
    }

    /// The run within `affinity_minutes` of `now`: its account's prompt cache is warm.
    pub fn warm(&self, config: &Config, now: Timestamp) -> Option<&LastRun> {
        let age = self.age(now)?;
        (config.affinity_minutes > 0 && age <= i64::from(config.affinity_minutes) * 60)
            .then_some(self.last.as_ref())
            .flatten()
    }

    /// The model of the copy `account` (`provider:name`) would resume, when known.
    pub fn model_for(&self, account: &str) -> Option<&str> {
        self.seen_by.as_ref()?.get(account)?.as_deref()
    }

    /// The model every copy has, when they agree and it is known: what to say of the session
    /// when no account is chosen.
    pub fn model(&self) -> Option<&str> {
        let mut models = self.seen_by.as_ref()?.values();
        let first = models.next()?.as_deref()?;
        models.all(|m| m.as_deref() == Some(first)).then_some(first)
    }
}

/// What remuda knows of the session `resume` names (R23 Resuming), read without a request or a
/// scan of the stores: the latest launch that ran it (`launch_log`, R6), and, through the
/// session index (`index`, R8), the stores of `accounts` that hold a copy of it and the end of
/// each copy. Every fact is tied to a store: the activity is that of the copy in the store of
/// the account that launched it (under the home it was launched with), and each account's
/// model that of the copy in its own store. A copy in another store is another account's.
pub fn read_session(
    resume: Resume,
    accounts: &[Account],
    env: &Env,
    launch_log: &Path,
    index: &Path,
) -> Session {
    let index = Index::load(index);
    let provider = resume.provider;
    let stores: Vec<index::Store> = index::stores(accounts, env)
        .into_iter()
        .filter(|s| s.provider == provider)
        .collect();
    let store_of = |account: &str| {
        stores
            .iter()
            .find(|s| s.accounts.iter().any(|a| a == account))
    };
    let launched = attribution::last_launch(launch_log, provider, &resume.id);
    let cwd = launched.as_ref().and_then(|l| l.cwd.clone());
    // The end of session `id`'s copy in `store` ([`copy_in`]).
    let tail_in = |store: &index::Store, id: &str| {
        let tails: Vec<SessionTail> = transcripts(&index, provider, id)
            .filter(|e| e.store == store.path)
            .filter_map(|e| transcript::session_tail(&e.path, provider).ok())
            .collect();
        copy_in(tails, cwd.as_deref())
    };
    let last = launched.map(|launched| {
        // What it ran (the session itself, or the fork it made) in the store of the account
        // as launched. Its last record counts only when written in the launch's directory: one
        // written elsewhere is someone else's.
        let account = accounts
            .iter()
            .find(|a| a.qualified() == launched.account && a.home.to_string() == launched.home);
        let active = account
            .and_then(|a| store_of(&a.qualified()))
            .zip(launched.session_id.as_deref())
            .and_then(|(store, sid)| tail_in(store, sid))
            .filter(|tail| same_dir(tail.cwd_last.as_deref(), launched.cwd.as_deref()))
            .and_then(|tail| tail.ts_last?.parse::<Timestamp>().ok());
        LastRun {
            active_at: active.map_or(launched.ts, |at| at.max(launched.ts)),
            launched_at: launched.ts,
            account: launched.account,
            home: launched.home,
        }
    });
    let indexed = transcripts(&index, provider, &resume.id).next().is_some();
    let seen_by = indexed.then(|| {
        let mut seen = BTreeMap::new();
        for store in &stores {
            if transcripts(&index, provider, &resume.id).any(|e| e.store == store.path) {
                let model = tail_in(store, &resume.id).and_then(|tail| tail.model);
                for account in &store.accounts {
                    seen.insert(account.clone(), model.clone());
                }
            }
        }
        seen
    });
    Session {
        resume,
        last,
        seen_by,
    }
}

/// Of the copies of one session in one store (their ends, `tails`), the one it goes on in
/// (R23 Resuming): the one written to last; of those written to last at the same time, the
/// one whose last record is in the launch's directory `cwd`. `None` when there is none, or
/// that does not single one out: then neither its time nor its model is known. A copy whose
/// last time cannot be read was written to before any whose can.
fn copy_in(tails: Vec<SessionTail>, cwd: Option<&str>) -> Option<SessionTail> {
    let time = |t: &SessionTail| {
        t.ts_last
            .as_deref()
            .and_then(|t| t.parse::<Timestamp>().ok())
    };
    let latest = tails.iter().map(time).max()?;
    let mut last: Vec<SessionTail> = tails.into_iter().filter(|t| time(t) == latest).collect();
    if last.len() > 1 {
        last.retain(|t| same_dir(t.cwd_last.as_deref(), cwd));
    }
    match <[SessionTail; 1]>::try_from(last) {
        Ok([one]) => Some(one),
        Err(_) => None,
    }
}

/// The index entries of session `id` whose file is there and is that session's by its name.
fn transcripts<'a>(
    index: &'a Index,
    provider: Provider,
    id: &'a str,
) -> impl Iterator<Item = &'a index::Entry> {
    index.entries.values().filter(move |e| {
        let name = e.path.file_name().and_then(|n| n.to_str());
        let named = match provider {
            Provider::Claude => e.path.file_stem().and_then(|n| n.to_str()) == Some(id),
            Provider::Codex => name.and_then(codex::rollout_id) == Some(id),
        };
        e.provider == provider && e.session_id == id && named && e.path.is_file()
    })
}

/// Whether a transcript's last directory is the launch's: both known, and equal as written or
/// once resolved.
fn same_dir(tail: Option<&str>, launch: Option<&str>) -> bool {
    let (Some(tail), Some(launch)) = (tail, launch) else {
        return false;
    };
    tail == launch
        || matches!(
            (std::fs::canonicalize(tail), std::fs::canonicalize(launch)),
            (Ok(a), Ok(b)) if a == b
        )
}

/// One account, as gathered: why it cannot be recommended at all, or its usage.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub account: Account,
    /// Why no model of this account is feasible (excluded, logged out, ...).
    pub blocked: Option<String>,
    /// Its usage, read at the instant of the recommendation ([`gather`]).
    pub usage: Option<Reading>,
    /// For the user only, never sent: they may name paths (a failed live query, no cache).
    pub notes: Vec<String>,
}

impl Entry {
    /// Cached codex usage is the general limit only (R10): per-model limits are unknown.
    pub fn per_model_unknown(&self) -> bool {
        self.account.provider == Provider::Codex
            && self
                .usage
                .as_ref()
                .is_none_or(|u| u.source == Source::Cached)
    }

    /// Its usage is older than `stale_after` minutes, or of unknown age; live usage never is.
    pub fn stale(&self, config: &Config) -> bool {
        self.usage
            .as_ref()
            .is_some_and(|usage| usage.stale(config.stale_after))
    }
}

/// One (account, model) pair, or an account that is blocked as a whole (`model` `None`).
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// Index into the entries.
    pub entry: usize,
    /// `None`: the agent's default (nothing injected).
    pub model: Option<String>,
    /// Position of `model` in its provider's `models`.
    pub model_rank: usize,
    pub why_not: Option<String>,
    /// Least percent left over the windows that apply and are known; `None` when none is:
    /// without usage data, or when each has reset since its usage was recorded.
    pub headroom: Option<f64>,
    /// The window that gives `headroom`.
    pub binding: Option<Window>,
    /// Percent left per hour until reset on `pace_window` ([`pace`]); `None` when unknown. A
    /// way to rank, never to decide feasibility; given whatever the strategy.
    pub pace: Option<Pace>,
    /// The window that gives `pace`: not `binding`'s kind of fact (the least left), but the
    /// tightest budget window, else the tightest shorter one.
    pub pace_window: Option<Window>,
    /// The windows that apply and have reset since their usage was recorded: of unknown
    /// usage, shown, never counted.
    pub reset_passed: Vec<Window>,
    /// For a pair without a model: the per-model windows, which apply only if the agent's
    /// default model is of their family. Shown, never counted: remuda does not know that model.
    pub default_model_windows: Vec<Window>,
    /// 1-based position among the feasible pairs by the rules.
    pub rules_rank: Option<usize>,
    /// Its account ran the session resumed within `affinity_minutes` (R23 Resuming): its
    /// prompt cache is warm. A fact, not feasibility: a pair that is not feasible may have it.
    pub affine: bool,
}

impl Candidate {
    pub fn feasible(&self) -> bool {
        self.why_not.is_none()
    }
}

/// Every candidate of `entries`, feasibility decided and the feasible ones ranked by the rules
/// (R23); in entry order, each account's models in `models` order. Resuming `session`, each
/// account is one candidate without a model (the session's is the session's), whose windows
/// are those of the session's model when it is known; the account that ran it lately is marked
/// [`Candidate::affine`].
pub fn candidates(
    entries: &[Entry],
    config: &Config,
    now: Timestamp,
    session: Option<&Session>,
) -> Vec<Candidate> {
    let warm = session.and_then(|s| s.warm(config, now));
    let mut out = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        // The account as it ran the session: the same name and home (R2).
        let affine = warm.is_some_and(|run| run.is(&entry.account));
        // The model of the copy this account would resume: the one in its own store.
        let session_model = session.and_then(|s| s.model_for(&entry.account.qualified()));
        if let Some(blocked) = &entry.blocked {
            out.push(Candidate {
                entry: i,
                model: None,
                model_rank: 0,
                why_not: Some(blocked.clone()),
                headroom: None,
                binding: None,
                pace: None,
                pace_window: None,
                reset_passed: Vec::new(),
                default_model_windows: Vec::new(),
                rules_rank: None,
                affine,
            });
            continue;
        }
        let provider = entry.account.provider;
        let models: Vec<Option<&String>> = match &config.choices(provider).models {
            _ if session.is_some() => vec![None],
            models if models.is_empty() => vec![None],
            models => models.iter().map(Some).collect(),
        };
        for (rank, model) in models.into_iter().enumerate() {
            // The model whose windows apply: the one injected, else the session's.
            let model_of_windows = model.map(String::as_str).or(session_model);
            let windows: Vec<&Window> = entry
                .usage
                .iter()
                .flat_map(|u| &u.windows)
                .filter(|w| match (&w.model, model_of_windows) {
                    (None, _) => true,
                    (Some(name), Some(model)) => limits_model(provider, model, name),
                    (Some(_), None) => false,
                })
                .collect();
            // Of the known ones, the least left; of equals, the one that resets last binds
            // longest. A window that has reset since is unknown: it binds nothing.
            let binding = windows.iter().filter_map(|w| Some((*w, w.left()?))).min_by(
                |(a, a_left), (b, b_left)| {
                    a_left.total_cmp(b_left).then_with(|| {
                        let at = |w: &Window| w.resets_at().map_or(i64::MAX, |t| t.as_second());
                        at(b).cmp(&at(a))
                    })
                },
            );
            let headroom = binding.map(|(_, left)| left);
            let binding = binding.map(|(w, _)| w.clone());
            let pace = pace(&windows, now);
            let reset_passed: Vec<Window> = windows
                .iter()
                .filter(|w| w.reset_passed())
                .map(|w| (*w).clone())
                .collect();
            let default_model_windows: Vec<Window> = match model_of_windows {
                Some(_) => Vec::new(),
                None => entry
                    .usage
                    .iter()
                    .flat_map(|u| &u.windows)
                    .filter(|w| w.model.is_some())
                    .cloned()
                    .collect(),
            };
            // Only a known percentage can fall short: a window of unknown usage blocks nothing.
            let why_not = binding
                .as_ref()
                .and_then(|w| Some((w, w.used()?, w.left()?)))
                .filter(|(_, _, left)| *left < f64::from(config.min_headroom))
                .map(|(w, used, _)| {
                    format!(
                        "{}: {} used, below the {}% left required{}",
                        w.label,
                        usage::format_percent(used),
                        config.min_headroom,
                        resets_text(w, now)
                    )
                });
            out.push(Candidate {
                entry: i,
                model: model.cloned(),
                model_rank: rank,
                why_not,
                headroom,
                binding,
                pace: pace.map(|(pace, _)| pace),
                pace_window: pace.map(|(_, w)| w.clone()),
                reset_passed,
                default_model_windows,
                rules_rank: None,
                affine,
            });
        }
    }
    rank_rules(&mut out, entries, config);
    out
}

/// The pace of a pair whose windows that apply are `windows`, read at `now` (R23 Rules): of
/// each window of known usage and known length, the percent left per hour until its reset (the
/// window's whole length when its reset is unknown), never over less than [`MIN_PACE_SECONDS`].
/// With a budget window ([`BUDGET_WINDOW`] or longer, whether its usage is known or not), the
/// pair's is the least of its budget windows' known ones, and `None` when each has reset since:
/// a budget whose usage is unknown is not a budget the pair lacks. Without one, the least of the
/// shorter windows'; `None` without either. A window that has reset since is of unknown usage
/// and gives no pace (not that of a full window); a window of unknown length is neither a budget
/// window nor a short one. Of equal paces, the first window.
pub fn pace<'a>(windows: &[&'a Window], now: Timestamp) -> Option<(Pace, &'a Window)> {
    // Each window of known length: whether it is a budget window, and its pace when known.
    let paces: Vec<(Option<Pace>, &Window, bool)> = windows
        .iter()
        .filter_map(|w| {
            let length = usage::history::window_length(&w.label)?;
            let pace = w.left().and_then(|left| {
                let seconds = match w.reset {
                    usage::Reset::Ahead(at) => at.as_second() - now.as_second(),
                    usage::Reset::Unknown => length.as_secs(),
                    usage::Reset::Passed(_) => return None,
                };
                Some(Pace::new(left, seconds))
            });
            Some((pace, *w, length >= BUDGET_WINDOW))
        })
        .collect();
    let budget = paces.iter().any(|(_, _, b)| *b);
    paces
        .iter()
        .filter(|(_, _, b)| *b == budget)
        .filter_map(|(pace, w, _)| Some(((*pace)?, *w)))
        .min_by(|a, b| a.0.cmp_pace(b.0))
}

/// The pace band of each of `order`'s candidates (R23 Rules), by candidate index: the pairs of
/// known pace, highest first; the first opens band 0, whose top is its pace, and each pair whose
/// pace is below [`PACE_BAND`] of the current band's top ([`Pace::below_band_of`]) opens the
/// next band, topped by its own; one on the boundary stays. The order and the boundary are the
/// same exact relation ([`Pace::cmp_pace`]), so equal paces, over the same seconds or not, are
/// interchangeable as a band's top: the bands do not depend on the order ties come in. Pairs of
/// unknown pace have none.
fn pace_bands(candidates: &[Candidate], order: &[usize]) -> BTreeMap<usize, usize> {
    let mut known: Vec<(Pace, usize)> = order
        .iter()
        .filter_map(|&i| Some((candidates[i].pace?, i)))
        .collect();
    known.sort_by(|a, b| b.0.cmp_pace(a.0));
    let mut bands = BTreeMap::new();
    let mut band: Option<(usize, Pace)> = None;
    for (pace, i) in known {
        let (n, top) = match band {
            Some((n, top)) if !pace.below_band_of(top) => (n, top),
            Some((n, _)) => (n + 1, pace),
            None => (0, pace),
        };
        band = Some((n, top));
        bands.insert(i, n);
    }
    bands
}

/// Numbers the feasible candidates by the rules (R23): resuming a session, the account whose
/// prompt cache is warm first (among the feasible only: affinity orders, it never admits);
/// then known headroom before unknown (stale
/// data is still known; no usage data, or every window reset since, is not); then the model's
/// position in `models`; the 10-point headroom band, higher first (90% left and more is one
/// band); fresh before stale; the binding window's reset, sooner first; `prefer` order;
/// registry order. With `strategy = "pace"`, the pace takes the headroom's three places: known
/// pace before unknown, the pace band, highest first ([`pace_bands`]), and the reset of the
/// window that gives the pace.
pub fn rank_rules(candidates: &mut [Candidate], entries: &[Entry], config: &Config) {
    let mut order: Vec<usize> = (0..candidates.len())
        .filter(|&i| candidates[i].feasible())
        .collect();
    let prefer = |c: &Candidate| {
        let qualified = entries[c.entry].account.qualified();
        config
            .prefer
            .iter()
            .position(|p| *p == qualified)
            .unwrap_or(config.prefer.len())
    };
    if config.strategy == Strategy::Pace {
        let bands = pace_bands(candidates, &order);
        order.sort_by_key(|&i| {
            let c = &candidates[i];
            (
                !c.affine,
                c.pace.is_none(),
                c.model_rank,
                bands.get(&i).copied().unwrap_or(usize::MAX),
                entries[c.entry].stale(config),
                c.pace_window
                    .as_ref()
                    .and_then(Window::resets_at)
                    .map_or(i64::MAX, |t| t.as_second()),
                prefer(c),
                c.entry,
            )
        });
        for (rank, i) in order.into_iter().enumerate() {
            candidates[i].rules_rank = Some(rank + 1);
        }
        return;
    }
    let key = |c: &Candidate| {
        let entry = &entries[c.entry];
        let qualified = entry.account.qualified();
        let prefer = config
            .prefer
            .iter()
            .position(|p| *p == qualified)
            .unwrap_or(config.prefer.len());
        (
            !c.affine,
            c.headroom.is_none(),
            c.model_rank,
            std::cmp::Reverse(c.headroom.map_or(0, band)),
            entry.stale(config),
            c.binding
                .as_ref()
                .and_then(Window::resets_at)
                .map_or(i64::MAX, |t| t.as_second()),
            prefer,
            c.entry,
        )
    };
    order.sort_by_key(|&i| key(&candidates[i]));
    for (rank, i) in order.into_iter().enumerate() {
        candidates[i].rules_rank = Some(rank + 1);
    }
}

/// The 10-point band of `headroom` percent left: 0-9 is 0, ..., 90-100 is 9 (a full window is
/// not a band of its own: 100% and 99% left differ by nothing that matters).
fn band(headroom: f64) -> i64 {
    ((headroom / 10.0).floor() as i64).clamp(0, 9)
}

/// The feasible candidates' indices in rules order.
pub fn ranked(candidates: &[Candidate]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..candidates.len())
        .filter(|&i| candidates[i].rules_rank.is_some())
        .collect();
    order.sort_by_key(|&i| candidates[i].rules_rank);
    order
}

/// `--wait` (R23): when to try again after an attempt whose candidates, read at `now`, found
/// nothing feasible. A pair infeasible by a usage window waits on that window (its binding
/// one) to reset; one whose account is blocked (excluded, another provider, not logged in,
/// `codex` not on PATH) waits on nothing time changes. With no pair of the first kind:
/// [`wait::Wait::Never`], the blocked accounts' reasons.
pub fn next_attempt(candidates: &[Candidate], entries: &[Entry], now: Timestamp) -> wait::Wait {
    let blocking = candidates.iter().filter_map(|c| {
        let entry = &entries[c.entry];
        let window = c.binding.as_ref()?;
        (c.why_not.is_some() && entry.blocked.is_none()).then_some((&entry.account, window))
    });
    wait::schedule(blocking, now).unwrap_or_else(|| {
        let reasons: Vec<String> = entries
            .iter()
            .filter_map(|e| {
                Some(format!(
                    "{} {}",
                    e.account.qualified(),
                    e.blocked.as_deref()?
                ))
            })
            .collect();
        wait::Wait::Never(match reasons.is_empty() {
            true => "no account is listed".to_string(),
            false => reasons.join("; "),
        })
    })
}

/// `1h20m`, `2d3h`, `4d`, `45m` from `now` until `at` (0 when past).
pub fn format_in(at: Timestamp, now: Timestamp) -> String {
    let secs = (at.as_second() - now.as_second()).max(0);
    let (d, h, m) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    match (d, h, m) {
        (0, 0, m) => format!("{m}m"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h{m}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d{h}h"),
    }
}

/// Why the rules decided (R23).
#[derive(Debug, Clone, PartialEq)]
pub enum Reason {
    NoKey,
    Offline,
    NoNotes,
    SingleOption,
    JevError(String),
    LowConfidence,
}

impl Reason {
    pub fn name(&self) -> &'static str {
        match self {
            Reason::NoKey => "no_key",
            Reason::Offline => "offline",
            Reason::NoNotes => "no_notes",
            Reason::SingleOption => "single_option",
            Reason::JevError(_) => "jev_error",
            Reason::LowConfidence => "low_confidence",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecidedBy {
    /// Jev's pair, confident enough.
    Jev,
    /// Jev's most probable account, with that account's most probable model.
    JevAccount,
    Rules,
}

impl DecidedBy {
    pub fn name(self) -> &'static str {
        match self {
            DecidedBy::Jev => "jev",
            DecidedBy::JevAccount => "jev_account",
            DecidedBy::Rules => "rules",
        }
    }
}

/// What Jev answered, as far as it was usable.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct JevReport {
    /// The model that answered (`jev-1.13.0`).
    pub model: Option<String>,
    pub confidence: Option<f64>,
    /// The probability of the chosen account's pairs together, for `jev (account)`.
    pub account_probability: Option<f64>,
    pub effort_confidence: Option<f64>,
    /// Why the chosen provider's effort answer was not usable; the rest of the answer was.
    pub effort_error: Option<String>,
    pub error: Option<String>,
    /// By candidate index.
    pub probabilities: BTreeMap<usize, f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// Index of the chosen candidate.
    pub chosen: usize,
    pub effort: Option<String>,
    /// The effort is Jev's (else `default_effort`, or none).
    pub effort_by_jev: bool,
    pub decided_by: DecidedBy,
    pub reason: Option<Reason>,
    /// `None` when Jev was not asked.
    pub jev: Option<JevReport>,
    /// How the rules ranked the pairs.
    pub strategy: Strategy,
}

/// Jev's answers, mapped onto the candidates.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Answers {
    pub model: Option<String>,
    /// The chosen candidate and the confidence; `None` when `launch` was not asked (one
    /// option, R23).
    pub launch: Option<(usize, f64)>,
    /// By candidate index; offered options only.
    pub probabilities: BTreeMap<usize, f64>,
    /// Per provider asked: the score (an index into its `efforts`) and its confidence, or why
    /// the answer is not usable.
    pub efforts: BTreeMap<Provider, Result<(f64, f64), String>>,
}

/// What asking Jev came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Asked {
    /// Not asked, and why.
    Skipped(Reason),
    Failed(String),
    Answered(Answers),
}

/// The decision (R23): Jev's pair from [`PAIR_CONFIDENCE`]; else its most probable account from
/// [`ACCOUNT_PROBABILITY`], with that account's most probable model; else the rules' first (and
/// only it, when `launch` was not asked). The effort is Jev's score for the chosen provider from
/// [`EFFORT_CONFIDENCE`], else `default_effort`; an unusable effort answer costs only the
/// effort. `None` when nothing is feasible.
pub fn decide(
    candidates: &[Candidate],
    entries: &[Entry],
    config: &Config,
    asked: Asked,
) -> Option<Decision> {
    let rules = *ranked(candidates).first()?;
    let effort_default = |c: usize| {
        config
            .choices(entries[candidates[c].entry].account.provider)
            .default_effort
            .clone()
    };
    let answers = match asked {
        Asked::Skipped(reason) => {
            return Some(Decision {
                chosen: rules,
                effort: effort_default(rules),
                effort_by_jev: false,
                decided_by: DecidedBy::Rules,
                reason: Some(reason),
                jev: None,
                strategy: config.strategy,
            });
        }
        Asked::Failed(error) => {
            return Some(Decision {
                chosen: rules,
                effort: effort_default(rules),
                effort_by_jev: false,
                decided_by: DecidedBy::Rules,
                reason: Some(Reason::JevError(error.clone())),
                jev: Some(JevReport {
                    error: Some(error),
                    ..JevReport::default()
                }),
                strategy: config.strategy,
            });
        }
        Asked::Answered(answers) => answers,
    };
    let mut report = JevReport {
        model: answers.model.clone(),
        confidence: answers.launch.map(|(_, confidence)| confidence),
        probabilities: answers.probabilities.clone(),
        ..JevReport::default()
    };
    let offered = |c: usize| candidates.get(c).is_some_and(Candidate::feasible);
    let (chosen, decided_by, reason) = match answers.launch {
        // One option: Jev was asked only the effort.
        None => (rules, DecidedBy::Rules, Some(Reason::SingleOption)),
        Some((choice, confidence)) if offered(choice) && confidence >= PAIR_CONFIDENCE => {
            (choice, DecidedBy::Jev, None)
        }
        Some(_) => {
            let mut by_account: BTreeMap<usize, f64> = BTreeMap::new();
            for (&c, &p) in &answers.probabilities {
                *by_account.entry(candidates[c].entry).or_default() += p;
            }
            // Of equal sums, the account whose best pair the rules rank higher.
            let best_rank = |e: usize| {
                candidates
                    .iter()
                    .filter(|c| c.entry == e)
                    .filter_map(|c| c.rules_rank)
                    .min()
                    .unwrap_or(usize::MAX)
            };
            let top = by_account.iter().max_by(|a, b| {
                a.1.total_cmp(b.1)
                    .then_with(|| best_rank(*b.0).cmp(&best_rank(*a.0)))
            });
            match top {
                Some((&entry, &p)) if p >= ACCOUNT_PROBABILITY => {
                    report.account_probability = Some(p);
                    let model = answers
                        .probabilities
                        .iter()
                        .filter(|(c, _)| candidates[**c].entry == entry)
                        .max_by(|a, b| {
                            a.1.total_cmp(b.1).then_with(|| {
                                candidates[*b.0]
                                    .rules_rank
                                    .cmp(&candidates[*a.0].rules_rank)
                            })
                        })
                        .map(|(c, _)| *c)
                        .expect("the account has a probability");
                    (model, DecidedBy::JevAccount, None)
                }
                _ => (rules, DecidedBy::Rules, Some(Reason::LowConfidence)),
            }
        }
    };
    let provider = entries[candidates[chosen].entry].account.provider;
    let efforts = &config.choices(provider).efforts;
    let (effort, effort_by_jev) = match answers.efforts.get(&provider) {
        Some(Ok((score, confidence))) => {
            report.effort_confidence = Some(*confidence);
            if *confidence >= EFFORT_CONFIDENCE && !efforts.is_empty() {
                let i = (score.round().max(0.0) as usize).min(efforts.len() - 1);
                (Some(efforts[i].clone()), true)
            } else {
                (effort_default(chosen), false)
            }
        }
        Some(Err(e)) => {
            report.effort_error = Some(e.clone());
            (effort_default(chosen), false)
        }
        None => (effort_default(chosen), false),
    };
    Some(Decision {
        chosen,
        effort,
        effort_by_jev,
        decided_by,
        reason,
        jev: Some(report),
        strategy: config.strategy,
    })
}

/// The options `remuda run` gets for `model` and `effort` (R23): claude `--model <m> --effort
/// <e>`, codex `-m <m> -c model_reasoning_effort=<e>`, before `user_args`. One the user's
/// arguments already set is not injected, with a notice. Arguments that resume or fork a
/// named session get nothing: they are passed as given (R23 Resuming). Refuses arguments that
/// do neither and do not start a new session.
pub fn run_args(
    provider: Provider,
    model: Option<&str>,
    effort: Option<&str>,
    user_args: &[String],
) -> Result<(Vec<String>, Vec<String>)> {
    let mut injected: Vec<String> = Vec::new();
    let mut notices: Vec<String> = Vec::new();
    let skip = |what: &str, value: &str, notices: &mut Vec<String>| {
        notices.push(format!(
            "{what} is already in the arguments; not injecting the recommended {value}"
        ));
    };
    match opens(provider, user_args) {
        Opens::New => {}
        Opens::Session(..) => return Ok((user_args.to_vec(), notices)),
        Opens::Other if provider == Provider::Claude => bail!(
            "`pick --run` starts a new session; these arguments do not: {}",
            user_args.join(" ")
        ),
        Opens::Other => bail!(
            "`pick --run` starts a new session; `codex {}` does not",
            user_args[0]
        ),
    }
    match provider {
        Provider::Claude => {
            let has = |name: &str| {
                let prefix = format!("{name}=");
                user_args
                    .iter()
                    .any(|a| a == name || a.starts_with(&prefix))
            };
            if let Some(model) = model {
                if has("--model") {
                    skip("--model", &format!("model ({model})"), &mut notices);
                } else {
                    injected.extend(["--model".to_string(), model.to_string()]);
                }
            }
            if let Some(effort) = effort {
                if has("--effort") {
                    skip("--effort", &format!("effort ({effort})"), &mut notices);
                } else {
                    injected.extend(["--effort".to_string(), effort.to_string()]);
                }
            }
        }
        Provider::Codex => {
            let has_model = user_args.iter().any(|a| {
                a == "--model"
                    || a.starts_with("--model=")
                    || (a.starts_with("-m") && !a.starts_with("--"))
            });
            let has_effort = user_args.iter().enumerate().any(|(i, a)| {
                let setting = match a.as_str() {
                    "-c" | "--config" => user_args.get(i + 1).map(String::as_str),
                    _ => a
                        .strip_prefix("--config=")
                        .or_else(|| a.strip_prefix("-c").filter(|_| !a.starts_with("--"))),
                };
                setting.is_some_and(|s| s.trim_start().starts_with("model_reasoning_effort"))
            });
            if let Some(model) = model {
                if has_model {
                    skip("-m/--model", &format!("model ({model})"), &mut notices);
                } else {
                    injected.extend(["-m".to_string(), model.to_string()]);
                }
            }
            if let Some(effort) = effort {
                if has_effort {
                    skip(
                        "model_reasoning_effort",
                        &format!("effort ({effort})"),
                        &mut notices,
                    );
                } else {
                    injected.extend(["-c".to_string(), format!("model_reasoning_effort={effort}")]);
                }
            }
        }
    }
    let args = [injected, user_args.to_vec()].concat();
    // A model id must not turn the launch into something else (a claude subcommand).
    if provider == Provider::Claude && launch::classify(&args) != launch::classify(user_args) {
        bail!(
            "the recommended options would not start a new session: {}",
            args.join(" ")
        );
    }
    Ok((args, notices))
}

/// How the usage is gathered (R23).
pub struct Sources<'a> {
    pub env: &'a Env,
    /// Asked when a live query answers, and once more when everything is gathered: a query may
    /// take as long as its timeout, and a reset does not wait for it.
    pub clock: fn() -> Timestamp,
    /// Runs the agents' commands: `codex login status`, the live queries.
    pub agents: &'a dyn Runner,
    /// Query usage live (R10) instead of reading the cache; a failed query falls back to it.
    pub live: Option<Duration>,
    /// Only this provider's accounts are candidates, and why (`--provider claude`, or whose
    /// session is resumed).
    pub provider: Option<(Provider, String)>,
    /// Only these accounts (`provider:name`) can see the session resumed: the others are not
    /// asked anything (R23 Resuming). `None`: every account.
    pub seen_by: Option<Vec<String>>,
}

/// Each of `accounts` with its usage, or why it is blocked (R23): excluded, another provider,
/// codex not logged in (`codex login status`; with a live query, the `account/read` of its one
/// `codex app-server` run, and `codex login status` only when that told nothing), claude with
/// neither `oauthAccount` in `.claude.json` nor a usage cache. Accounts are queried in parallel.
///
/// Also the instant of the recommendation: the time once every account has answered or failed.
/// Each account's usage is read at that one instant, whenever its agent said it, and whatever
/// is said of the recommendation (time to a reset, age) counts from it.
pub fn gather(accounts: &[Account], config: &Config, sources: &Sources) -> (Vec<Entry>, Timestamp) {
    let gathered = probe::parallel(accounts, |account| {
        let mut said = None;
        let entry = gather_one(account, config, sources, &mut said);
        (entry, said)
    });
    let now = (sources.clock)();
    let entries = gathered
        .into_iter()
        .map(|(entry, said)| Entry {
            usage: said.map(|said| said.at(now)),
            ..entry
        })
        .collect();
    (entries, now)
}

/// What an account's agent said about its usage, not yet read at an instant.
enum Said {
    Cached(CachedUsage),
    /// A live answer, and when it arrived.
    Live(Vec<UsageRow>, Timestamp),
}

impl Said {
    fn at(&self, now: Timestamp) -> Reading {
        match self {
            Said::Cached(cached) => Snapshot::cached(cached).at(now),
            Said::Live(rows, answered_at) => Snapshot::live(rows, *answered_at).at(now),
        }
    }
}

/// `account`'s entry, without its usage: what its agent said goes to `said`, for [`gather`] to
/// read once every account is in.
fn gather_one(
    account: &Account,
    config: &Config,
    sources: &Sources,
    said: &mut Option<Said>,
) -> Entry {
    let mut entry = Entry {
        account: account.clone(),
        blocked: None,
        usage: None,
        notes: Vec::new(),
    };
    let provider = account.provider;
    if config.exclude.contains(&account.qualified()) {
        entry.blocked = Some("excluded ([pick] exclude)".to_string());
        return entry;
    }
    if let Some((only, why)) = &sources.provider
        && *only != provider
    {
        entry.blocked = Some(format!("not a {only} account ({why})"));
        return entry;
    }
    if let Some(seen_by) = &sources.seen_by
        && !seen_by.contains(&account.qualified())
    {
        entry.blocked = Some(NOT_SEEN.to_string());
        return entry;
    }
    // What the live query answered, when one is asked, and when the answer arrived: it is
    // recorded then, not after whatever this account is asked next.
    let mut live = None;
    if provider == Provider::Codex {
        if !sources.agents.has(Provider::Codex) {
            entry.blocked = Some("`codex` not found on PATH".to_string());
            return entry;
        }
        // The live query's one `codex app-server` run says whether the account is logged in
        // as well (`account/read`): `codex login status` is asked only when it did not.
        let asked = sources.live.map(|timeout| {
            let asked = usage::live_codex(account, sources.agents, timeout);
            (asked, (sources.clock)())
        });
        let told = asked
            .as_ref()
            .and_then(|(asked, _)| asked.as_ref().ok())
            .and_then(|live| live.login.clone());
        let (login, source) = match told {
            Some(identity) => ((identity, None), "`codex app-server` account/read"),
            None => (
                identity::identify(account, sources.agents, sources.env, LOGIN_TIMEOUT),
                "`codex login status`",
            ),
        };
        match login {
            (Identity::NotLoggedIn, _) => {
                entry.blocked = Some(format!("not logged in ({source})"));
                return entry;
            }
            (Identity::Unknown, warning) => entry.notes.push(format!(
                "login unknown{}",
                warning.map(|w| format!(": {w}")).unwrap_or_default()
            )),
            (Identity::LoggedIn { .. }, _) => {}
        }
        live = asked.map(|(asked, at)| (asked.and_then(|live| live.usage), at));
    } else if let Some(timeout) = sources.live {
        let asked = usage::live_usage(account, sources.agents, timeout).map(|live| live.usage);
        live = Some((asked, (sources.clock)()));
    }
    match live {
        None => {}
        Some((Ok(LiveUsage::Rows(rows)), answered_at)) => {
            *said = Some(Said::Live(rows, answered_at));
        }
        Some((Ok(LiveUsage::Untold(untold)), _)) => entry
            .notes
            .push(format!("live: {}; using cached usage", untold.reason())),
        Some((Ok(LiveUsage::Unrecognized(_)), _)) => entry
            .notes
            .push("live output not recognized; using cached usage".to_string()),
        Some((Err(e), _)) => entry
            .notes
            .push(format!("live query failed ({e}); using cached usage")),
    }
    if said.is_none() {
        match usage::cached_usage(account, sources.env) {
            Ok(cached) => *said = Some(Said::Cached(cached)),
            Err(notice) => entry.notes.push(format!("no usage data ({notice})")),
        }
    }
    if provider == Provider::Claude && said.is_none() {
        let logged_in = account
            .claude_json(sources.env)
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| identity::parse_claude_json(&text))
            .is_some();
        if !logged_in {
            entry.blocked =
                Some("not logged in (no oauthAccount in .claude.json, no usage cache)".to_string());
        }
    }
    entry
}

/// `remuda run <account> <options> <args>` for the decision, as words: `args` are the user's,
/// after `--`, which must be ones [`run_args`] takes (without a `--` of their own after the
/// account: `remuda run` gives the agent every word after the account, R5).
pub fn command(
    entry: &Entry,
    candidate: &Candidate,
    effort: Option<&str>,
    args: &[String],
) -> Vec<String> {
    let provider = entry.account.provider;
    let (args, _) = run_args(provider, candidate.model.as_deref(), effort, args)
        .expect("the arguments were checked before deciding");
    [
        vec![
            "remuda".to_string(),
            "run".to_string(),
            entry.account.qualified(),
        ],
        args,
    ]
    .concat()
}

/// `words` as one POSIX shell command line that gives back the same words: each is left as is
/// when it is made only of characters no shell treats specially, else single-quoted (a `'` in
/// it as `'\''`). An empty word is `''`. The user's arguments go into the command shown: a
/// prompt with spaces, quotes or `$(` must replay as the one word it was.
pub fn shell_line(words: &[String]) -> String {
    words
        .iter()
        .map(|word| {
            let plain = !word.is_empty()
                && word
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-+=:,./@%".contains(&b));
            if plain {
                word.clone()
            } else {
                format!("'{}'", word.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `claude:max / claude-opus-5-5`; `/ default` without a model.
pub fn pair_label(entry: &Entry, candidate: &Candidate) -> String {
    format!(
        "{} / {}",
        entry.account.qualified(),
        candidate.model.as_deref().unwrap_or("default")
    )
}

/// `usage: cached 25m ago`, `live 3s ago`, `cached (time unknown)`, `none`. A live answer has
/// an age too: it is read once every account's has arrived.
fn data_text(entry: &Entry, config: &Config) -> String {
    let stale = if entry.stale(config) {
        " (stale: may be higher now)"
    } else {
        ""
    };
    match &entry.usage {
        None => "none".to_string(),
        Some(u) => match (u.source, u.age_text()) {
            (Source::Live, Some(age)) => format!("live {age}"),
            (Source::Live, None) => "live".to_string(),
            (Source::Cached, Some(age)) => format!("cached {age}{stale}"),
            (Source::Cached, None) => format!("cached (time unknown){stale}"),
        },
    }
}

/// `, resets in 2d3h`; empty without a reset ahead.
fn resets_text(w: &Window, now: Timestamp) -> String {
    w.resets_at()
        .map(|at| format!(", resets in {}", format_in(at, now)))
        .unwrap_or_default()
}

/// The binding window as `Week (all models) 23% left, resets in 2d3h`; one of unknown usage
/// (never a binding window) as in [`used_text`].
pub fn binding_text(w: &Window, now: Timestamp) -> String {
    match w.left() {
        Some(left) => format!(
            "{} {} left{}",
            w.label,
            usage::format_percent(left),
            resets_text(w, now)
        ),
        None => used_text(w, now),
    }
}

/// The pace and the window that gives it: `1.25%/h on Week (all models) (resets in 2d3h)`;
/// `0.42%/h on Week (all models) (reset unknown: over its whole length, 7d)`; `unknown`.
pub fn pace_text(c: &Candidate, now: Timestamp) -> String {
    let (Some(pace), Some(w)) = (c.pace.map(Pace::per_hour), &c.pace_window) else {
        return "unknown".to_string();
    };
    let reset = match (w.resets_at(), usage::history::window_length(&w.label)) {
        (Some(at), _) => format!("resets in {}", format_in(at, now)),
        (None, Some(length)) => format!(
            "reset unknown: over its whole length, {}",
            format_in(now + length, now)
        ),
        (None, None) => "reset unknown".to_string(),
    };
    format!("{pace:.2}%/h on {} ({reset})", w.label)
}

/// A window as used: `Week (Fable) 100% used, resets in 3d`; `Week (Fable) usage unknown (reset
/// since cached)` when its reset has passed.
pub fn used_text(w: &Window, now: Timestamp) -> String {
    match w.used() {
        Some(used) => format!(
            "{} {} used{}",
            w.label,
            usage::format_percent(used),
            resets_text(w, now)
        ),
        None => format!("{} usage unknown ({})", w.label, w.reset_since()),
    }
}

/// The windows of unknown usage as `Session, Week (all models): reset since cached`; `None`
/// when the pair has none. (`reset since asked` for a live answer that another account's
/// slower one outlasted.)
pub fn reset_passed_text(c: &Candidate) -> Option<String> {
    let since = c.reset_passed.first()?.reset_since();
    let labels: Vec<&str> = c.reset_passed.iter().map(|w| w.label.as_str()).collect();
    Some(format!("{}: {since}", labels.join(", ")))
}

/// What to say where a pair has windows of unknown usage (R23): remuda never queries live on
/// its own, so for cached usage it names the option that does.
pub fn live_hint(entry: &Entry, c: &Candidate) -> Option<String> {
    let text = reset_passed_text(c)?;
    Some(match entry.usage.as_ref().map(|usage| usage.source) {
        Some(Source::Live) => text,
        _ => format!("{text} (--live asks the agent)"),
    })
}

/// The effort of the text report, with where it came from when Jev answered.
fn effort_text(decision: &Decision) -> String {
    let effort = decision
        .effort
        .clone()
        .unwrap_or_else(|| "agent default".to_string());
    let Some(jev) = &decision.jev else {
        return effort;
    };
    match (
        decision.effort_by_jev,
        &jev.effort_error,
        jev.effort_confidence,
    ) {
        (true, _, Some(c)) => format!("{effort} (jev, confidence {c:.2})"),
        (false, Some(e), _) => format!("{effort} (jev's effort answer is unusable: {e})"),
        (false, None, Some(c)) => format!("{effort} (jev's effort confidence {c:.2} is too low)"),
        _ => effort,
    }
}

/// `12m ago`: the time from `at` to `now`.
fn ago(at: Timestamp, now: Timestamp) -> String {
    format!("{} ago", format_in(now, at))
}

/// What the report says of the session resumed (R23 Resuming), whether or not a candidate is
/// `chosen`: the `session` line (its kind and id, who ran it last and how long ago, whether
/// that account's prompt cache is warm or why not, the model of the copy the chosen account
/// would resume), and the lines below it: that the index does not have it, and that the warm
/// account is not the one recommended, and why. Its id is the user's own, said back on their
/// terminal; none of this is sent.
pub fn session_rows(
    session: &Session,
    entries: &[Entry],
    candidates: &[Candidate],
    chosen: Option<usize>,
    config: &Config,
    now: Timestamp,
) -> Vec<(&'static str, String)> {
    let resume = &session.resume;
    let warm = session.warm(config, now);
    let warm_candidate = candidates.iter().position(|c| c.affine);
    let who = match &session.last {
        None => {
            "not launched through remuda: no account's prompt cache is known to be warm".to_string()
        }
        Some(last) => {
            let renamed = entries
                .iter()
                .any(|e| e.account.qualified() == last.account && !last.is(&e.account));
            let why = match (warm, warm_candidate) {
                (Some(_), Some(_)) => "prompt cache warm: preferred".to_string(),
                (Some(_), None) if renamed => {
                    "registered now with another home: another login, whose cache does not hold \
                     it"
                    .to_string()
                }
                (Some(_), None) => "no longer registered".to_string(),
                (None, _) if config.affinity_minutes == 0 => {
                    "affinity off: [pick] affinity_minutes = 0".to_string()
                }
                (None, _) => format!(
                    "more than {} minutes ago: its prompt cache has likely expired",
                    config.affinity_minutes
                ),
            };
            format!(
                "{} ran it {} ({why})",
                last.account,
                ago(last.active_at, now)
            )
        }
    };
    let model = match chosen {
        Some(c) => session.model_for(&entries[candidates[c].entry].account.qualified()),
        None => session.model(),
    };
    let mut rows = vec![(
        "session",
        format!(
            "{} {}: {who}; model {}",
            resume.kind.name(),
            resume.id,
            model.unwrap_or("unknown")
        ),
    )];
    if session.seen_by.is_none() {
        rows.push((
            "",
            "not in the session index: every account is offered, whether or not it can see \
             the session (`remuda sessions` indexes it)"
                .to_string(),
        ));
    }
    if let (Some(w), Some(last)) = (warm_candidate, &session.last)
        && chosen.is_none_or(|c| candidates[c].entry != candidates[w].entry)
    {
        // Feasible, the warm account is the rules' first: only Jev chooses another.
        let but = match (&candidates[w].why_not, chosen) {
            (Some(why), _) => why.clone(),
            (None, Some(c)) => format!(
                "jev chose {}",
                entries[candidates[c].entry].account.qualified()
            ),
            (None, None) => "nothing was chosen".to_string(),
        };
        rows.push((
            "",
            format!(
                "{} ran this session {}, but {but}; resuming as another account rewrites its \
                 prompt cache",
                last.account,
                ago(last.active_at, now)
            ),
        ));
    }
    rows
}

/// [`session_rows`] as lines of the text report.
pub fn format_session(rows: &[(&str, String)]) -> String {
    rows.iter()
        .map(|(k, v)| format!("{k:<10}  {v}\n"))
        .collect()
}

/// The plain-text report of `remuda pick` (R23): `args` are the user's, after `--`, and
/// `session` the session they resume or fork.
pub fn format_text(
    entries: &[Entry],
    candidates: &[Candidate],
    decision: &Decision,
    config: &Config,
    now: Timestamp,
    args: &[String],
    session: Option<&Session>,
) -> String {
    let c = &candidates[decision.chosen];
    let entry = &entries[c.entry];
    let model = match (session, &c.model) {
        (_, Some(model)) => model.clone(),
        (Some(session), None) => format!(
            "the session's ({})",
            session
                .model_for(&entry.account.qualified())
                .unwrap_or("unknown")
        ),
        (None, None) => "agent default".to_string(),
    };
    let mut rows: Vec<(&str, String)> = vec![
        ("account", entry.account.qualified()),
        ("model", model),
        ("effort", effort_text(decision)),
        ("decided by", decided_text(decision, candidates, entries)),
    ];
    let rules = ranked(candidates)[0];
    if rules != decision.chosen {
        rows.push((
            "rules",
            format!(
                "would choose {}",
                pair_label(&entries[candidates[rules].entry], &candidates[rules])
            ),
        ));
    }
    if let Some(session) = session {
        rows.extend(session_rows(
            session,
            entries,
            candidates,
            Some(decision.chosen),
            config,
            now,
        ));
    }
    rows.push((
        "limit",
        match (&c.binding, c.reset_passed.is_empty()) {
            (Some(w), _) => binding_text(w, now),
            (None, false) => "unknown".to_string(),
            (None, true) => "no usage data".to_string(),
        },
    ));
    if let Some(hint) = live_hint(entry, c) {
        rows.push(("unknown", hint));
    }
    if config.strategy == Strategy::Pace {
        rows.push(("pace", pace_text(c, now)));
    }
    for w in &c.default_model_windows {
        rows.push((
            "also",
            format!(
                "{} (counts only if the agent's default model is of that family)",
                used_text(w, now)
            ),
        ));
    }
    rows.push(("usage", data_text(entry, config)));
    if entry.usage.is_some() && entry.per_model_unknown() {
        rows.push((
            "",
            "per-model limits unknown (codex reports them only live)".into(),
        ));
    }
    for note in &entry.notes {
        rows.push(("", note.clone()));
    }
    if !config.exclude.is_empty() {
        rows.push(("excluded", config.exclude.join(", ")));
    }
    rows.push((
        "command",
        shell_line(&command(entry, c, decision.effort.as_deref(), args)),
    ));
    let mut out: String = rows
        .iter()
        .map(|(k, v)| format!("{k:<10}  {v}\n"))
        .collect();
    out.push_str(&format_not_feasible(entries, candidates));
    out
}

/// `claude:max / claude-opus-5-5, effort high; decided by jev (confidence 0.90)`; resuming,
/// `; resume <id> (its prompt cache is warm)`, or who ran it last.
pub fn summary(
    entries: &[Entry],
    candidates: &[Candidate],
    decision: &Decision,
    session: Option<&Session>,
    now: Timestamp,
) -> String {
    let c = &candidates[decision.chosen];
    let effort = decision.effort.as_deref().unwrap_or("agent default");
    let resuming = session.map(|session| {
        let resume = &session.resume;
        let warmth = match &session.last {
            _ if c.affine => " (its prompt cache is warm)".to_string(),
            Some(last) => format!(
                " (last ran as {} {})",
                last.account,
                ago(last.active_at, now)
            ),
            None => String::new(),
        };
        format!("; {} {}{warmth}", resume.kind.name(), resume.id)
    });
    format!(
        "{}, effort {effort}; decided by {}{}",
        pair_label(&entries[c.entry], c),
        decided_text(decision, candidates, entries),
        resuming.unwrap_or_default()
    )
}

/// `decided by` of the text report.
fn decided_text(decision: &Decision, candidates: &[Candidate], entries: &[Entry]) -> String {
    let jev = decision.jev.as_ref();
    let confidence = jev
        .and_then(|j| j.confidence)
        .map(|c| format!("confidence {c:.2}"));
    match (decision.decided_by, &decision.reason) {
        (DecidedBy::Jev, _) => format!("jev ({})", confidence.unwrap_or_default()),
        (DecidedBy::JevAccount, _) => format!(
            "jev (account, probability {:.2}; {})",
            jev.and_then(|j| j.account_probability).unwrap_or_default(),
            confidence.unwrap_or_default()
        ),
        (DecidedBy::Rules, Some(reason)) => {
            let why = match reason {
                Reason::NoKey => "TYPESAFE_API_KEY is not set".to_string(),
                Reason::Offline => "--offline".to_string(),
                Reason::NoNotes => "no [pick] notes".to_string(),
                Reason::SingleOption if jev.is_some() => {
                    "only one option; jev was asked only the effort".to_string()
                }
                Reason::SingleOption => "only one option".to_string(),
                Reason::JevError(e) => e.clone(),
                Reason::LowConfidence => {
                    let top = jev
                        .and_then(|j| {
                            let (c, p) =
                                j.probabilities.iter().max_by(|a, b| a.1.total_cmp(b.1))?;
                            Some(format!(
                                "; it leaned to {} at {p:.2}",
                                pair_label(&entries[candidates[*c].entry], &candidates[*c])
                            ))
                        })
                        .unwrap_or_default();
                    format!("jev {}{top}", confidence.unwrap_or_default())
                }
            };
            format!("{} ({}: {why})", rules_text(decision), reason.name())
        }
        (DecidedBy::Rules, None) => rules_text(decision),
    }
}

/// `rules`, with the strategy when it is not the default: `rules, strategy pace`.
fn rules_text(decision: &Decision) -> String {
    match decision.strategy {
        Strategy::Headroom => "rules".to_string(),
        strategy => format!("rules, strategy {}", strategy.name()),
    }
}

/// The candidates that are not feasible, with why; empty when all are.
pub fn format_not_feasible(entries: &[Entry], candidates: &[Candidate]) -> String {
    let rows: Vec<(String, &str)> = candidates
        .iter()
        .filter_map(|c| {
            let why = c.why_not.as_deref()?;
            let entry = &entries[c.entry];
            let name = match entry.blocked {
                Some(_) => entry.account.qualified(),
                None => pair_label(entry, c),
            };
            Some((name, why))
        })
        .collect();
    if rows.is_empty() {
        return String::new();
    }
    let width = rows.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    let mut out = "\nnot feasible:\n".to_string();
    for (name, why) in rows {
        out.push_str(&format!("  {name:<width$}  {why}\n"));
    }
    out
}

/// The `--json` report (R23); `decision` `None` when nothing is feasible. `args` are the
/// user's, after `--`; `session` is `null` without a session resumed or forked.
pub fn to_json(
    entries: &[Entry],
    candidates: &[Candidate],
    decision: Option<&Decision>,
    config: &Config,
    args: &[String],
    session: Option<&Session>,
    now: Timestamp,
) -> Value {
    let ts = |t: Option<Timestamp>| t.map(|t| t.to_string());
    let session = session.map(|s| {
        json!({
            "id": s.resume.id,
            "kind": s.resume.kind.name(),
            "last_account": s.last.as_ref().map(|l| l.account.clone()),
            "last_active_at": ts(s.last.as_ref().map(|l| l.active_at)),
            "age_seconds": s.age(now),
            "affine": candidates.iter().any(|c| c.affine),
            "model": decision
                .map(|d| entries[candidates[d.chosen].entry].account.qualified())
                .map_or(s.model(), |account| s.model_for(&account)),
            "indexed": s.seen_by.is_some(),
        })
    });
    let list: Vec<Value> = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let entry = &entries[c.entry];
            let usage = entry.usage.as_ref();
            json!({
                "account": entry.account.qualified(),
                "model": c.model,
                "feasible": c.feasible(),
                "why_not": c.why_not,
                "headroom": c.headroom,
                "binding": c.binding.as_ref().map(|w| w.label.clone()),
                "resets_at": ts(c.binding.as_ref().and_then(Window::resets_at)),
                "pace": c.pace.map(Pace::per_hour),
                "pace_window": c.pace_window.as_ref().map(|w| w.label.clone()),
                "reset_passed": !c.reset_passed.is_empty(),
                "source": usage.map(|u| u.source.name()),
                "fetched_at": ts(usage.and_then(|u| u.fetched_at)),
                "age_seconds": usage.and_then(|u| u.age_seconds),
                "stale": usage.map(|u| u.stale(config.stale_after)),
                "default_model_windows": c.default_model_windows.iter().map(|w| json!({
                    "label": w.label,
                    "percent": w.used(),
                    "resets_at": ts(w.resets_at()),
                    "reset_passed": w.reset_passed(),
                })).collect::<Vec<_>>(),
                "rules_rank": c.rules_rank,
                "affine": c.affine,
                "jev_probability": decision
                    .and_then(|d| d.jev.as_ref())
                    .and_then(|j| j.probabilities.get(&i).copied()),
            })
        })
        .collect();
    let Some(d) = decision else {
        return json!({
            "account": null, "provider": null, "model": null, "effort": null,
            "effort_by": null, "decided_by": null, "reason": null, "jev": null, "command": [],
            "session": session, "strategy": config.strategy.name(), "candidates": list,
        });
    };
    let c = &candidates[d.chosen];
    let entry = &entries[c.entry];
    json!({
        "account": entry.account.qualified(),
        "provider": entry.account.provider.name(),
        "model": c.model,
        "effort": d.effort,
        "effort_by": d.effort.as_ref().map(|_| if d.effort_by_jev { "jev" } else { "rules" }),
        "decided_by": d.decided_by.name(),
        "reason": d.reason.as_ref().map(Reason::name),
        "jev": d.jev.as_ref().map(|j| json!({
            "model": j.model,
            "confidence": j.confidence,
            "effort_confidence": j.effort_confidence,
            "effort_error": j.effort_error,
            "error": j.error,
        })),
        "command": command(entry, c, d.effort.as_deref(), args),
        "session": session,
        "strategy": config.strategy.name(),
        "candidates": list,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_command::Scripted;
    use crate::registry::Home;
    use crate::usage::{CachedUsage, Resets, UsageRow};

    fn parse(text: &str) -> Result<Config> {
        Registry::from_document(&text.parse::<DocumentMut>().unwrap()).map(|r| r.pick)
    }

    const ACCOUNTS: &str = "[[account]]\nprovider = \"claude\"\nname = \"max\"\nhome = \"/m\"\n\n\
                            [[account]]\nprovider = \"codex\"\nname = \"work\"\nhome = \"/w\"\n\n";

    /// R3, R23: `[pick]` with every key; account names resolve as in R1.
    #[test]
    fn parses_the_pick_table() {
        let config = parse(&format!(
            "{ACCOUNTS}[pick]\nexclude = [\"work\"]\nprefer = [\"max\", \"claude:max\"]\n\
             min_headroom = 25\nstale_after = 30\naffinity_minutes = 0\nstrategy = \"pace\"\nnotes = \"\"\"\n  Keep max for refactors.\n\"\"\"\n\n\
             [pick.claude]\nmodels = [\"claude-opus-5-5\", \"fable\"]\n\
             efforts = [\"high\", \"max\"]\ndefault_effort = \"high\"\n"
        ))
        .unwrap();
        assert_eq!(config.exclude, ["codex:work"]);
        assert_eq!(config.prefer, ["claude:max"]);
        assert_eq!((config.min_headroom, config.stale_after), (25, 30));
        assert_eq!(config.affinity_minutes, 0);
        assert_eq!(config.strategy, Strategy::Pace);
        assert_eq!(config.notes, "Keep max for refactors.");
        assert_eq!(config.claude.models, ["claude-opus-5-5", "fable"]);
        assert_eq!(config.claude.default_effort.as_deref(), Some("high"));
        assert_eq!(config.codex, Choices::default());
        // Without `[pick]`: the defaults, not zeros.
        let none = parse(ACCOUNTS).unwrap();
        assert_eq!((none.min_headroom, none.stale_after), (10, 120));
        assert_eq!(none.affinity_minutes, 60);
        assert_eq!(none, Config::default());
    }

    /// What `gather` made of one account, and what it ran for it.
    fn gathered(account: &Account, agents: &Scripted, live: bool) -> (Entry, Vec<String>) {
        let env = Env::new();
        let sources = Sources {
            env: &env,
            clock: || "2026-10-06T00:00:00Z".parse().unwrap(),
            agents,
            live: live.then_some(Duration::from_secs(90)),
            provider: None,
            seen_by: None,
        };
        let (mut entries, _) = gather(std::slice::from_ref(account), &Config::default(), &sources);
        (entries.remove(0), agents.ran())
    }

    /// R23: with `--live`, a codex account's one `codex app-server` run answers its usage and
    /// whether it is logged in; `codex login status` runs only when that run did not say, and
    /// alone without `--live`. Either way the same accounts are blocked.
    #[test]
    fn a_live_codex_query_also_says_whether_the_account_is_logged_in() {
        let work = Account {
            provider: Provider::Codex,
            name: "work".into(),
            home: Home::Path("/nonexistent/work".into()),
        };
        const SERVER: &str = "codex:work app-server account/rateLimits/read account/read";
        const STATUS: &str = "codex:work login status";
        let limits =
            json!({"rateLimits": {"primary": {"usedPercent": 4, "windowDurationMins": 300}}});
        let chatgpt = json!({"account": {"type": "chatgpt", "email": "c@example.com"}});
        let logged_in = Scripted::exited(0, "", "Logged in using ChatGPT\n");
        let source = |entry: &Entry| entry.usage.as_ref().map(|u| u.source);

        // Logged in, and the server answers both: one run.
        let agents = Scripted::new()
            .app_server_says("codex:work", Ok(vec![Ok(limits.clone()), Ok(chatgpt)]));
        let (entry, ran) = gathered(&work, &agents, true);
        assert_eq!(ran, [SERVER]);
        assert_eq!(
            (entry.blocked.as_deref(), source(&entry)),
            (None, Some(Source::Live))
        );

        // A codex without app-server: `codex login status` says, and the cache is used.
        let no_server = "`codex app-server` exited with status 2: error: unrecognized subcommand";
        let agents = Scripted::new()
            .app_server_says("codex:work", Err(no_server.to_string()))
            .on(STATUS, logged_in.clone());
        let (entry, ran) = gathered(&work, &agents, true);
        assert_eq!(ran, [SERVER, STATUS]);
        assert_eq!(entry.blocked, None);
        assert_eq!(
            entry.notes[0],
            format!("live query failed ({no_server}); using cached usage")
        );

        // Not logged in: the server says so, and nothing else runs.
        let denied = "`codex app-server` account/rateLimits/read: authentication required";
        let agents = Scripted::new().app_server_says(
            "codex:work",
            Ok(vec![
                Err(denied.to_string()),
                Ok(json!({"account": null, "requiresOpenaiAuth": true})),
            ]),
        );
        let (entry, ran) = gathered(&work, &agents, true);
        assert_eq!(ran, [SERVER]);
        assert_eq!(
            entry.blocked.as_deref(),
            Some("not logged in (`codex app-server` account/read)")
        );
        // ... as `codex login status` does where the server did not say.
        let agents = Scripted::new()
            .app_server_says("codex:work", Err(no_server.to_string()))
            .on(STATUS, Scripted::exited(1, "", "Not logged in\n"));
        let (entry, ran) = gathered(&work, &agents, true);
        assert_eq!(ran, [SERVER, STATUS]);
        assert_eq!(
            entry.blocked.as_deref(),
            Some("not logged in (`codex login status`)")
        );

        // `account/read` fails and the status cannot be read: noted, not blocking, and the
        // usage the server did answer is used.
        let agents = Scripted::new()
            .app_server_says(
                "codex:work",
                Ok(vec![
                    Ok(limits),
                    Err("`codex app-server` account/read: no".into()),
                ]),
            )
            .on(STATUS, crate::probe::Outcome::TimedOut);
        let (entry, ran) = gathered(&work, &agents, true);
        assert_eq!(ran, [SERVER, STATUS]);
        assert_eq!(
            (entry.blocked.as_deref(), source(&entry)),
            (None, Some(Source::Live))
        );
        assert_eq!(
            entry.notes,
            [
                "login unknown: codex:work: `codex login status` timed out after 15s; identity \
              unknown"
            ]
        );

        // Without `--live`, only `codex login status`.
        let agents = Scripted::new().on(STATUS, Scripted::exited(1, "", "Not logged in\n"));
        let (entry, ran) = gathered(&work, &agents, false);
        assert_eq!(ran, [STATUS]);
        assert_eq!(
            entry.blocked.as_deref(),
            Some("not logged in (`codex login status`)")
        );
        let agents = Scripted::new().on(STATUS, logged_in);
        let (entry, ran) = gathered(&work, &agents, false);
        assert_eq!(ran, [STATUS]);
        assert_eq!(entry.blocked, None);

        // No codex on PATH: blocked, and nothing runs.
        for live in [false, true] {
            let agents = Scripted::new().without(Provider::Codex);
            let (entry, ran) = gathered(&work, &agents, live);
            assert_eq!(entry.blocked.as_deref(), Some("`codex` not found on PATH"));
            assert!(ran.is_empty(), "{ran:?}");
        }
    }

    /// R23 (The instant): a codex account's live answer is recorded when its one
    /// `codex app-server` run answers, before `codex login status` is asked where that run did
    /// not say whether the account is logged in, and read once everything is gathered.
    #[test]
    fn a_live_codex_answer_is_recorded_when_it_arrives() {
        use std::sync::atomic::{AtomicI64, Ordering};
        static TICKS: AtomicI64 = AtomicI64::new(0);
        fn t0() -> Timestamp {
            "2026-10-06T00:00:00Z".parse().unwrap()
        }
        // The answer, then the end of the gathering, 100 seconds later.
        fn clock() -> Timestamp {
            t0() + jiff::SignedDuration::from_secs(100 * TICKS.fetch_add(1, Ordering::SeqCst))
        }
        let work = Account {
            provider: Provider::Codex,
            name: "work".into(),
            home: Home::Path("/nonexistent/work".into()),
        };
        let limits =
            json!({"rateLimits": {"primary": {"usedPercent": 4, "windowDurationMins": 300}}});
        let agents = Scripted::new()
            .app_server_says(
                "codex:work",
                Ok(vec![
                    Ok(limits),
                    Err("`codex app-server` account/read: no".into()),
                ]),
            )
            .on(
                "codex:work login status",
                Scripted::exited(0, "", "Logged in using ChatGPT\n"),
            );
        let env = Env::new();
        let sources = Sources {
            env: &env,
            clock,
            agents: &agents,
            live: Some(Duration::from_secs(90)),
            provider: None,
            seen_by: None,
        };
        let (entries, now) = gather(std::slice::from_ref(&work), &Config::default(), &sources);
        assert_eq!(
            agents.ran(),
            [
                "codex:work app-server account/rateLimits/read account/read",
                "codex:work login status"
            ]
        );
        assert_eq!(now, t0() + jiff::SignedDuration::from_secs(100));
        assert_eq!(TICKS.load(Ordering::SeqCst), 2, "one answer, one reading");
        let usage = entries[0].usage.as_ref().expect("live usage");
        assert_eq!(usage.source, Source::Live);
        assert_eq!(usage.fetched_at, Some(t0()));
        assert_eq!(usage.age_seconds, Some(100));
    }

    /// R23: a claude account's live query that cannot run (no claude on PATH) is a failed
    /// query like any other: noted, and the cache decides.
    #[test]
    fn a_live_claude_query_without_claude_is_a_failed_query() {
        let max = Account {
            provider: Provider::Claude,
            name: "max".into(),
            home: Home::Path("/nonexistent/max".into()),
        };
        let agents = Scripted::new().without(Provider::Claude);
        let (entry, ran) = gathered(&max, &agents, true);
        assert!(ran.is_empty(), "{ran:?}");
        assert_eq!(
            entry.notes[0],
            "live query failed (`claude` not found on PATH); using cached usage"
        );
        assert_eq!(entry.usage, None);
    }

    /// R3: an invalid `[pick]` fails loading, naming the table.
    #[test]
    fn rejects_invalid_pick_tables() {
        let long = "x".repeat(MAX_NOTES + 1);
        let cases = [
            (
                "[pick]\nexculde = []\n".to_string(),
                "[pick]: unknown key `exculde`",
            ),
            ("pick = 1\n".to_string(), "`pick` must be a table"),
            (
                "[pick]\nexclude = \"max\"\n".to_string(),
                "must be an array of account names",
            ),
            (
                "[pick]\nexclude = [\"nobody\"]\n".to_string(),
                "exclude = \"nobody\": unknown account",
            ),
            (
                "[pick]\nprefer = [\"codex:max\"]\n".to_string(),
                "unknown account",
            ),
            (
                "[pick]\nmin_headroom = 101\n".to_string(),
                "integer from 0 to 100",
            ),
            (
                "[pick]\nmin_headroom = 5.5\n".to_string(),
                "integer from 0 to 100",
            ),
            (
                "[pick]\nstale_after = 0\n".to_string(),
                "positive integer (minutes)",
            ),
            (
                "[pick]\naffinity_minutes = 1441\n".to_string(),
                "[pick]: `affinity_minutes` must be an integer from 0 to 1440 (minutes)",
            ),
            (
                "[pick]\naffinity_minutes = -1\n".to_string(),
                "integer from 0 to 1440",
            ),
            (
                "[pick]\naffinity_minutes = \"60\"\n".to_string(),
                "integer from 0 to 1440",
            ),
            (
                format!("[pick]\nnotes = \"{long}\"\n"),
                "longer than 4000 characters",
            ),
            (
                "[pick.claude]\nmodel = []\n".to_string(),
                "[pick.claude]: unknown key `model`",
            ),
            (
                "[pick.codex]\nmodels = [\"-x\"]\n".to_string(),
                "[pick.codex]: invalid model \"-x\"",
            ),
            (
                "[pick.claude]\nmodels = [\"a b\"]\n".to_string(),
                "invalid model",
            ),
            (
                "[pick.claude]\nmodels = [\"a\", \"a\"]\n".to_string(),
                "listed twice",
            ),
            (
                "[pick.claude]\nefforts = [\"High\"]\n".to_string(),
                "must match [a-z]+",
            ),
            (
                "[pick.claude]\nefforts = [\"high\"]\ndefault_effort = \"max\"\n".to_string(),
                "default_effort = \"max\" is not one of `efforts`",
            ),
            (
                "[pick.claude]\ndefault_effort = \"max\"\n".to_string(),
                "is not one of",
            ),
            (
                "[pick]\nclaude = 1\n".to_string(),
                "[pick.claude] must be a table",
            ),
            (
                "[pick.claude]\nmodels = [\"update\"]\n".to_string(),
                "\"update\" is a claude subcommand",
            ),
        ];
        for (text, want) in cases {
            // Before the accounts: after them, a bare key would belong to the last [[account]].
            let text = format!("{text}\n{ACCOUNTS}");
            let err = format!("{:#}", parse(&text).unwrap_err());
            assert!(err.contains(want), "{text}\n=> {err}");
        }
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    const NOW: &str = "2026-09-27T10:00:00Z";

    fn row(label: &str, percent: f64, resets: Option<&str>) -> UsageRow {
        UsageRow {
            label: label.into(),
            percent,
            severity: None,
            resets: resets.map(|r| Resets::At(ts(r))),
        }
    }

    fn account(provider: Provider, name: &str) -> Account {
        Account {
            provider,
            name: name.into(),
            home: Home::Path(format!("/h/{name}")),
        }
    }

    /// An account whose usage was cached `age_min` minutes before [`NOW`].
    fn entry(provider: Provider, name: &str, age_min: i64, rows: &[UsageRow]) -> Entry {
        let now = ts(NOW);
        let cached = CachedUsage {
            fetched_at: Some(now - jiff::SignedDuration::from_mins(age_min)),
            rows: rows.to_vec(),
        };
        Entry {
            account: account(provider, name),
            blocked: None,
            usage: Some(Snapshot::cached(&cached).at(now)),
            notes: Vec::new(),
        }
    }

    fn with_models(claude: &[&str], codex: &[&str]) -> Config {
        let mut config = Config::default();
        config.claude.models = claude.iter().map(|m| m.to_string()).collect();
        config.codex.models = codex.iter().map(|m| m.to_string()).collect();
        config
    }

    fn order(entries: &[Entry], candidates: &[Candidate]) -> Vec<String> {
        ranked(candidates)
            .into_iter()
            .map(|i| pair_label(&entries[candidates[i].entry], &candidates[i]))
            .collect()
    }

    /// R23: claude models match per-model windows by family, codex by id, ignoring case; general
    /// windows apply to every model.
    #[test]
    fn per_model_windows_match_by_family_or_id() {
        assert_eq!(family("claude-fable-5-1"), "fable");
        assert_eq!(family("fable"), "fable");
        assert!(limits_model(Provider::Claude, "claude-fable-5-1", "Fable"));
        assert!(!limits_model(Provider::Claude, "claude-opus-5-5", "Fable"));
        assert!(limits_model(
            Provider::Codex,
            "gpt-5.3-codex-spark",
            "GPT-5.3-Codex-Spark"
        ));

        let rows = [
            row("Session", 20.0, Some("2026-09-27T12:00:00Z")),
            row("Week (all models)", 50.0, Some("2026-09-30T10:00:00Z")),
            row("Week (Fable)", 100.0, Some("2026-09-30T10:00:00Z")),
            row("Week (Sonnet)", 95.0, Some("2026-09-30T10:00:00Z")),
        ];
        let entries = [entry(Provider::Claude, "max", 5, &rows)];
        let config = with_models(&["claude-opus-5-5", "claude-fable-5-1"], &[]);
        let c = candidates(&entries, &config, ts(NOW), None);
        assert_eq!(c.len(), 2);
        assert_eq!(
            c[0].headroom,
            Some(50.0),
            "Sonnet's window limits no configured model"
        );
        assert!(c[0].feasible());
        assert!(!c[1].feasible());
        assert_eq!(
            c[1].why_not.as_deref(),
            Some("Week (Fable): 100% used, below the 10% left required, resets in 3d")
        );
    }

    /// R23: a window whose reset has passed is of unknown usage: never counted, never blocking.
    /// A stale exhausted window with a reset still ahead stays exhausted.
    #[test]
    fn a_passed_reset_is_unknown_and_staleness_frees_nothing() {
        let now = ts(NOW);
        let rows = [
            row("Session", 100.0, Some("2026-09-27T09:00:00Z")),
            row("Week (all models)", 100.0, Some("2026-09-28T09:00:00Z")),
        ];
        let entries = [entry(Provider::Claude, "max", 600, &rows)];
        let config = Config::default();
        assert!(entries[0].stale(&config));
        let c = candidates(&entries, &config, now, None);
        assert!(!c[0].feasible(), "the week is known, and exhausted");
        assert_eq!(
            c[0].why_not.as_deref(),
            Some("Week (all models): 100% used, below the 10% left required, resets in 23h")
        );
        assert_eq!(c[0].headroom, Some(0.0));
        let unknown: Vec<&str> = c[0].reset_passed.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(unknown, ["Session"]);

        // The week has room: the pair is feasible, by the week alone. The session, exhausted
        // when it was cached, neither blocks nor counts as 100% left.
        let rows = [
            row("Session", 100.0, Some("2026-09-27T09:00:00Z")),
            row("Week (all models)", 50.0, Some("2026-09-28T09:00:00Z")),
        ];
        let entries = [entry(Provider::Claude, "max", 600, &rows)];
        let c = candidates(&entries, &config, now, None);
        assert!(c[0].feasible());
        assert_eq!(c[0].headroom, Some(50.0));
        assert_eq!(
            c[0].binding.as_ref().map(|w| w.label.as_str()),
            Some("Week (all models)")
        );
        assert_eq!(
            live_hint(&entries[0], &c[0]).as_deref(),
            Some("Session: reset since cached (--live asks the agent)")
        );
    }

    /// R23: when every window that applies has reset since, the headroom is unknown: the pair
    /// is feasible whatever `min_headroom` asks, with no binding window and no reset time.
    #[test]
    fn unknown_headroom_is_feasible_whatever_min_headroom() {
        let now = ts(NOW);
        let rows = [
            row("Session", 100.0, Some("2026-09-27T09:00:00Z")),
            row("Week (all models)", 100.0, Some("2026-09-26T09:00:00Z")),
            // Applies to no configured model: not this pair's.
            row("Week (Fable)", 100.0, Some("2026-09-26T09:00:00Z")),
        ];
        let entries = [entry(Provider::Claude, "old", 4 * 24 * 60, &rows)];
        let mut config = with_models(&["claude-opus-5-5"], &[]);
        config.min_headroom = 100;
        let c = candidates(&entries, &config, now, None);
        assert!(c[0].feasible(), "{:?}", c[0].why_not);
        assert_eq!((c[0].headroom, &c[0].binding), (None, &None));
        assert_eq!(
            reset_passed_text(&c[0]).as_deref(),
            Some("Session, Week (all models): reset since cached")
        );
        assert_eq!(c[0].rules_rank, Some(1));

        let decision = decide(&c, &entries, &config, Asked::Skipped(Reason::NoKey)).unwrap();
        let text = format_text(&entries, &c, &decision, &config, now, &[], None);
        for line in [
            "limit       unknown\n",
            "unknown     Session, Week (all models): reset since cached (--live asks the agent)\n",
            "usage       cached 4d ago (stale: may be higher now)\n",
        ] {
            assert!(text.contains(line), "{line:?} in:\n{text}");
        }
        let v = to_json(&entries, &c, Some(&decision), &config, &[], None, now);
        let pair = &v["candidates"][0];
        assert_eq!(pair["feasible"], true);
        assert_eq!(pair["reset_passed"], true);
        for null in ["headroom", "binding", "resets_at", "why_not"] {
            assert_eq!(pair[null], Value::Null, "{null}: {pair:#}");
        }
        assert_eq!(pair["stale"], true);
        assert_eq!(pair["age_seconds"], 4 * 86_400);
    }

    /// R23, a real run (review of 2026-10-02): four-day-old data whose resets have all passed
    /// must not outrank nine-minute-old data with 71% left. Old data that is still known
    /// (stale, no reset passed) ranks before it too; a pair without usage data is as unknown.
    #[test]
    fn stale_data_past_its_resets_ranks_after_known_headroom() {
        let now = ts(NOW);
        let entries = [
            entry(
                Provider::Claude,
                "team-alt",
                4 * 24 * 60,
                &[
                    row("Session", 12.0, Some("2026-09-23T14:00:00Z")),
                    row("Week (all models)", 100.0, Some("2026-09-25T09:00:00Z")),
                ],
            ),
            entry(
                Provider::Claude,
                "max",
                9,
                &[row("Week (all models)", 29.0, Some("2026-09-30T09:00:00Z"))],
            ),
            entry(
                Provider::Claude,
                "stale",
                4 * 24 * 60,
                &[row("Week (all models)", 85.0, Some("2026-09-28T09:00:00Z"))],
            ),
            Entry {
                usage: None,
                ..entry(Provider::Claude, "new", 0, &[])
            },
        ];
        let config = Config::default();
        let c = candidates(&entries, &config, now, None);
        assert!(c.iter().all(Candidate::feasible));
        assert_eq!(
            order(&entries, &c),
            [
                "claude:max / default",
                "claude:stale / default",
                // Unknown headroom, both, by the rest of the rules: no data is not stale.
                "claude:new / default",
                "claude:team-alt / default",
            ]
        );
        assert_eq!(c[0].headroom, None);
        assert_eq!(c[1].headroom, Some(71.0));
        assert_eq!(c[2].headroom, Some(15.0));
        // The report never names an instant in the past.
        let v = to_json(&entries, &c, None, &config, &[], None, now);
        let passed: Vec<(&Value, &Value, &Value)> = v["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (&c["account"], &c["reset_passed"], &c["resets_at"]))
            .collect();
        assert_eq!(
            passed,
            [
                (&json!("claude:team-alt"), &json!(true), &Value::Null),
                (
                    &json!("claude:max"),
                    &json!(false),
                    &json!("2026-09-30T09:00:00Z")
                ),
                (
                    &json!("claude:stale"),
                    &json!(false),
                    &json!("2026-09-28T09:00:00Z")
                ),
                (&json!("claude:new"), &json!(false), &Value::Null),
            ]
        );
    }

    /// R23: live usage is recorded now: a reset that reads as behind does not empty its window.
    #[test]
    fn live_usage_keeps_its_percentage_past_a_reset() {
        let now = ts(NOW);
        let rows = [UsageRow {
            resets: Some(Resets::Text("Sep 27 at 9am (UTC)".into())),
            ..row("Week (all models)", 95.0, None)
        }];
        let entries = [Entry {
            usage: Some(Snapshot::live(&rows, now).at(now)),
            ..entry(Provider::Claude, "max", 0, &[])
        }];
        let c = candidates(&entries, &Config::default(), now, None);
        assert!(!c[0].feasible());
        assert_eq!(
            c[0].why_not.as_deref(),
            Some("Week (all models): 95% used, below the 10% left required")
        );
        assert!(c[0].reset_passed.is_empty());
        let v = to_json(&entries, &c, None, &Config::default(), &[], None, now);
        assert_eq!(v["candidates"][0]["reset_passed"], false);
        assert_eq!(v["candidates"][0]["resets_at"], Value::Null);
        assert_eq!(v["candidates"][0]["headroom"], 5.0);
    }

    /// R23: a per-model window shown for the `default` pair reads the same way.
    #[test]
    fn a_default_model_window_past_its_reset_is_unknown() {
        let now = ts(NOW);
        let rows = [
            row("Week (all models)", 50.0, Some("2026-09-30T10:00:00Z")),
            row("Week (Fable)", 100.0, Some("2026-09-27T09:00:00Z")),
            row("Week (Sonnet)", 40.0, Some("2026-09-30T10:00:00Z")),
        ];
        let entries = [entry(Provider::Claude, "max", 600, &rows)];
        let c = candidates(&entries, &Config::default(), now, None);
        assert!(
            c[0].reset_passed.is_empty(),
            "it does not apply to the pair"
        );
        assert_eq!(
            used_text(&c[0].default_model_windows[0], now),
            "Week (Fable) usage unknown (reset since cached)"
        );
        let v = to_json(&entries, &c, None, &Config::default(), &[], None, now);
        assert_eq!(
            v["candidates"][0]["default_model_windows"],
            json!([
                {"label": "Week (Fable)", "percent": null, "resets_at": null,
                 "reset_passed": true},
                {"label": "Week (Sonnet)", "percent": 40.0,
                 "resets_at": "2026-09-30T10:00:00Z", "reset_passed": false},
            ])
        );
    }

    /// R23: rules rank by model order, then 10-point bands, then fresh before stale, then the
    /// sooner reset, then `prefer`, then registry order; no data ranks last.
    #[test]
    fn rules_rank_bands_resets_freshness_and_prefer() {
        let week = |pct: f64, reset: &str| [row("Week (all models)", pct, Some(reset))];
        let entries = [
            entry(
                Provider::Claude,
                "a",
                5,
                &week(35.0, "2026-09-30T00:00:00Z"),
            ),
            entry(
                Provider::Claude,
                "b",
                5,
                &week(31.0, "2026-09-29T00:00:00Z"),
            ),
            entry(
                Provider::Claude,
                "c",
                5,
                &week(45.0, "2026-09-28T00:00:00Z"),
            ),
            entry(
                Provider::Claude,
                "d",
                600,
                &week(39.0, "2026-09-29T00:00:00Z"),
            ),
            Entry {
                usage: None,
                ..entry(Provider::Claude, "e", 0, &[])
            },
            entry(
                Provider::Claude,
                "f",
                5,
                &week(31.0, "2026-09-29T00:00:00Z"),
            ),
        ];
        let mut config = with_models(&["claude-opus-5-5", "claude-sonnet-5"], &[]);
        let now = ts(NOW);
        // 60-69% left: the fresh b, f (sooner reset) and a, then the stale d despite its sooner
        // reset; c, of a lower band, after.
        assert_eq!(
            order(&entries, &candidates(&entries, &config, now, None))[..6],
            [
                "claude:b / claude-opus-5-5",
                "claude:f / claude-opus-5-5",
                "claude:a / claude-opus-5-5",
                "claude:d / claude-opus-5-5",
                "claude:c / claude-opus-5-5",
                "claude:b / claude-sonnet-5",
            ]
        );
        config.prefer = vec!["claude:f".into()];
        let c = candidates(&entries, &config, now, None);
        let all = order(&entries, &c);
        assert_eq!(
            all[..2],
            ["claude:f / claude-opus-5-5", "claude:b / claude-opus-5-5"]
        );
        assert_eq!(
            all[all.len() - 2..],
            ["claude:e / claude-opus-5-5", "claude:e / claude-sonnet-5"],
            "no data ranks after every known pair"
        );
    }

    /// R23: a real run: 22-hour-old data of an idle week (0% used) must not outrank fresh data
    /// of a week just as open (1% used, a later reset). 90% left and more is one band.
    #[test]
    fn fresh_beats_stale_within_a_band_whatever_the_reset() {
        let now = ts(NOW);
        let at = |secs: i64| (now + jiff::SignedDuration::from_secs(secs)).to_string();
        let entries = [
            entry(
                Provider::Claude,
                "max",
                22 * 60,
                &[row(
                    "Week (all models)",
                    0.0,
                    Some(&at(4 * 86_400 + 13 * 3600)),
                )],
            ),
            entry(
                Provider::Claude,
                "default",
                5,
                &[row(
                    "Week (all models)",
                    1.0,
                    Some(&at(6 * 86_400 + 8 * 3600)),
                )],
            ),
        ];
        assert_eq!(
            (band(100.0), band(99.0), band(90.0), band(89.9)),
            (9, 9, 9, 8)
        );
        let c = candidates(&entries, &Config::default(), now, None);
        assert_eq!(
            order(&entries, &c),
            ["claude:default / default", "claude:max / default"]
        );
    }

    /// R23: without `models`, per-model windows are shown for the `default` pair but never make
    /// it infeasible: the agent's default model is unknown.
    #[test]
    fn per_model_windows_without_models_are_shown_not_counted() {
        let rows = [
            row("Week (all models)", 50.0, Some("2026-09-30T10:00:00Z")),
            row("Week (Fable)", 100.0, Some("2026-09-30T10:00:00Z")),
        ];
        let entries = [entry(Provider::Claude, "max", 5, &rows)];
        let c = candidates(&entries, &Config::default(), ts(NOW), None);
        assert_eq!(c.len(), 1);
        assert!(c[0].feasible());
        assert_eq!(c[0].headroom, Some(50.0));
        let labels: Vec<&str> = c[0]
            .default_model_windows
            .iter()
            .map(|w| w.label.as_str())
            .collect();
        assert_eq!(labels, ["Week (Fable)"]);
        assert_eq!(
            used_text(&c[0].default_model_windows[0], ts(NOW)),
            "Week (Fable) 100% used, resets in 3d"
        );
        // With models, the pair of another family carries none.
        let config = with_models(&["claude-opus-5-5"], &[]);
        let c = candidates(&entries, &config, ts(NOW), None);
        assert!(c[0].default_model_windows.is_empty());
    }

    /// R3, R23: `[pick] strategy` is `"headroom"` (the default) or `"pace"`; resuming keeps it.
    #[test]
    fn parses_the_strategy() {
        let pace = parse(&format!("{ACCOUNTS}[pick]\nstrategy = \"pace\"\n")).unwrap();
        assert_eq!(pace.strategy, Strategy::Pace);
        assert_eq!(pace.for_resume().strategy, Strategy::Pace);
        let headroom = parse(&format!("{ACCOUNTS}[pick]\nstrategy = \"headroom\"\n")).unwrap();
        assert_eq!(headroom, Config::default());
        assert_eq!(Config::default().strategy, Strategy::Headroom);
        for bad in ["\"Pace\"", "\"fast\"", "\"\"", "1", "[\"pace\"]"] {
            let text = format!("[pick]\nstrategy = {bad}\n\n{ACCOUNTS}");
            let err = format!("{:#}", parse(&text).unwrap_err());
            assert!(
                err.contains(r#"[pick]: `strategy` must be "headroom" or "pace""#),
                "{bad} => {err}"
            );
        }
        let err = format!(
            "{:#}",
            parse(&format!("[pick]\nstrategi = \"pace\"\n\n{ACCOUNTS}")).unwrap_err()
        );
        assert!(
            err.contains(
                "unknown key `strategi` (known: exclude, prefer, min_headroom, stale_after, \
                 affinity_minutes, strategy, notes, claude, codex)"
            ),
            "{err}"
        );
    }

    /// The pace of the one pair of an account of `provider` with `rows` cached five minutes
    /// before [`NOW`], under `config`, and the window that gives it.
    fn pace_of(
        provider: Provider,
        rows: &[UsageRow],
        config: &Config,
    ) -> (Option<Pace>, Option<String>) {
        let entries = [entry(provider, "a", 5, rows)];
        let c = candidates(&entries, config, ts(NOW), None);
        (
            c[0].pace,
            c[0].pace_window.as_ref().map(|w| w.label.clone()),
        )
    }

    fn close(got: Option<Pace>, want: f64) -> bool {
        got.is_some_and(|got| (got.per_hour() - want).abs() < 1e-9)
    }

    /// R23 Rules (pace): percent left per hour until reset, on the tightest window of a day or
    /// longer; a shorter window does not count beside one; never over less than an hour.
    #[test]
    fn pace_is_left_per_hour_on_the_tightest_budget_window() {
        let config = with_models(&["claude-opus-5-5"], &[]);
        // Session: 100% left over 2h is 50%/h, but the week is the budget: 60% over 60h.
        let rows = [
            row("Session", 0.0, Some("2026-09-27T12:00:00Z")),
            row("Week (all models)", 40.0, Some("2026-09-29T22:00:00Z")),
        ];
        let (pace, window) = pace_of(Provider::Claude, &rows, &config);
        assert!(close(pace, 1.0), "{pace:?}");
        assert_eq!(window.as_deref(), Some("Week (all models)"));
        // Two budget windows: the tighter, 30% over 60h; the per-model week counts for its family.
        let rows = [
            row("Session", 0.0, Some("2026-09-27T12:00:00Z")),
            row("Week (all models)", 40.0, Some("2026-09-29T22:00:00Z")),
            row("Week (Opus)", 70.0, Some("2026-09-29T22:00:00Z")),
            row("Week (Fable)", 99.0, Some("2026-09-29T22:00:00Z")),
        ];
        let (pace, window) = pace_of(Provider::Claude, &rows, &config);
        assert!(close(pace, 0.5), "{pace:?}");
        assert_eq!(window.as_deref(), Some("Week (Opus)"));
        // A week that resets in half an hour: 50% over at least an hour, not 100%/h.
        let rows = [row("Week (all models)", 50.0, Some("2026-09-27T10:30:00Z"))];
        let (pace, _) = pace_of(Provider::Claude, &rows, &config);
        assert!(close(pace, 50.0), "{pace:?}");
    }

    /// R23 Rules (pace): a shorter window gives the pace only when no window of a day or
    /// longer does.
    #[test]
    fn pace_falls_back_to_short_windows_only_without_a_long_one() {
        let config = Config::default();
        let five = row("5h window", 20.0, Some("2026-09-27T14:00:00Z"));
        let week = row("7d window", 40.0, Some("2026-10-01T14:00:00Z"));
        let (pace, window) = pace_of(Provider::Codex, &[five.clone(), week], &config);
        assert!(close(pace, 0.6), "{pace:?}");
        assert_eq!(window.as_deref(), Some("7d window"));
        let (pace, window) = pace_of(Provider::Codex, &[five], &config);
        assert!(close(pace, 20.0), "{pace:?}");
        assert_eq!(window.as_deref(), Some("5h window"));
        // Of two short windows, the tighter.
        let rows = [
            row("Session", 30.0, Some("2026-09-27T11:00:00Z")),
            row("five_hour", 50.0, Some("2026-09-27T12:30:00Z")),
        ];
        let (pace, window) = pace_of(Provider::Claude, &rows, &config);
        assert!(close(pace, 20.0), "{pace:?}");
        assert_eq!(window.as_deref(), Some("five_hour"));
    }

    /// R23 Rules (pace): a window whose reset is unknown counts its whole length; a window of
    /// unknown length gives no pace, and is not a budget window that would hide a short one.
    #[test]
    fn an_unknown_reset_counts_the_whole_window() {
        let config = Config::default();
        let rows = [row("Week (all models)", 16.0, None)];
        let entries = [entry(Provider::Claude, "a", 5, &rows)];
        let c = candidates(&entries, &config, ts(NOW), None);
        assert!(close(c[0].pace, 0.5), "{:?}", c[0].pace);
        assert_eq!(
            pace_text(&c[0], ts(NOW)),
            "0.50%/h on Week (all models) (reset unknown: over its whole length, 7d)"
        );
        // Claude's wording that cannot be read is a reset unknown as well.
        let rows = [UsageRow {
            resets: Some(Resets::Text("someday".into())),
            ..row("Week (all models)", 16.0, None)
        }];
        let (pace, _) = pace_of(Provider::Claude, &rows, &config);
        assert!(close(pace, 0.5), "{pace:?}");
        // Of unknown length, with or without a reset: no pace, and not a budget window.
        let mystery = row("Mystery", 0.0, Some("2026-09-29T10:00:00Z"));
        assert_eq!(
            pace_of(Provider::Claude, std::slice::from_ref(&mystery), &config),
            (None, None)
        );
        assert_eq!(
            pace_of(Provider::Claude, &[row("Mystery", 0.0, None)], &config),
            (None, None)
        );
        let session = row("Session", 50.0, Some("2026-09-27T15:00:00Z"));
        let (pace, window) = pace_of(Provider::Claude, &[mystery, session], &config);
        assert!(close(pace, 10.0), "{pace:?}");
        assert_eq!(window.as_deref(), Some("Session"));
    }

    /// R23 Rules (pace): a window that has reset since its usage was cached is of unknown
    /// usage and gives no pace (an unknown is not a full window). It is still a budget window:
    /// a pair whose budget windows have all reset since has no pace, rather than the pace of a
    /// shorter window that holds no budget (`.lane/rulings.md` Q1); with another budget window
    /// known, that one's.
    #[test]
    fn a_window_reset_since_gives_no_pace() {
        let mut config = Config::default();
        let week = row("Week (all models)", 0.0, Some("2026-09-27T09:00:00Z"));
        let entries = [entry(
            Provider::Claude,
            "a",
            600,
            std::slice::from_ref(&week),
        )];
        let c = candidates(&entries, &config, ts(NOW), None);
        assert!(c[0].reset_passed.len() == 1 && c[0].feasible());
        assert_eq!((c[0].pace, c[0].pace_window.as_ref()), (None, None));
        assert_eq!(pace_text(&c[0], ts(NOW)), "unknown");
        // The session is known (50% over 2h, 25%/h), but the week holds the budget.
        let session = row("Session", 50.0, Some("2026-09-27T12:00:00Z"));
        let rows = [week.clone(), session.clone()];
        let entries = [entry(Provider::Claude, "a", 600, &rows)];
        let c = candidates(&entries, &config, ts(NOW), None);
        assert_eq!((c[0].pace, c[0].pace_window.as_ref()), (None, None));
        // Another budget window known: its pace (60% over 60h).
        let rows = [
            week.clone(),
            session,
            row("seven_day", 40.0, Some("2026-09-29T22:00:00Z")),
        ];
        let entries = [entry(Provider::Claude, "a", 600, &rows)];
        let c = candidates(&entries, &config, ts(NOW), None);
        assert!(close(c[0].pace, 1.0), "{:?}", c[0].pace);
        assert_eq!(c[0].pace_window.as_ref().unwrap().label, "seven_day");
        config.strategy = Strategy::Pace;
        // A, its week past its reset, does not outrank B by its session's pace: it is unknown.
        let entries = [
            entry(
                Provider::Claude,
                "a",
                5,
                &[
                    row("Week (all models)", 0.0, Some("2026-09-27T09:57:00Z")),
                    row("Session", 50.0, Some("2026-09-27T12:00:00Z")),
                ],
            ),
            entry(
                Provider::Claude,
                "b",
                5,
                &[row("Week (all models)", 20.0, Some("2026-09-29T10:00:00Z"))],
            ),
        ];
        let c = candidates(&entries, &config, ts(NOW), None);
        assert_eq!(c[0].pace, None);
        assert_eq!(
            order(&entries, &c),
            ["claude:b / default", "claude:a / default"]
        );
        // Unknown ranks after known, however low the known pace: an unknown is not a zero.
        let entries = [
            entry(
                Provider::Claude,
                "past",
                600,
                &[row("Week (all models)", 0.0, Some("2026-09-27T09:00:00Z"))],
            ),
            Entry {
                usage: None,
                ..entry(Provider::Claude, "none", 0, &[])
            },
            entry(
                Provider::Claude,
                "low",
                5,
                &[row("Week (all models)", 89.0, Some("2026-10-03T10:00:00Z"))],
            ),
        ];
        let c = candidates(&entries, &config, ts(NOW), None);
        // Of the two unknown, the one without data before the stale one, as for the headroom.
        assert_eq!(
            order(&entries, &c),
            [
                "claude:low / default",
                "claude:none / default",
                "claude:past / default"
            ]
        );
    }

    /// R23 Rules (pace): pairs whose paces are within a tenth of their band's top share a band,
    /// where `prefer` decides; the bands are cut from the highest pace down, so paces close in
    /// a chain do not all merge.
    #[test]
    fn pace_bands_hold_within_a_tenth() {
        // Over 100h: 1.0, 0.95, 0.91 | 0.85, 0.77 %/h.
        let week = |used: f64| [row("Week (all models)", used, Some("2026-10-01T14:00:00Z"))];
        let entries: Vec<Entry> = [("a", 0.0), ("b", 5.0), ("c", 9.0), ("d", 15.0), ("e", 23.0)]
            .iter()
            .map(|(name, used)| entry(Provider::Claude, name, 5, &week(*used)))
            .collect();
        let mut config = Config {
            strategy: Strategy::Pace,
            prefer: vec!["claude:e".into(), "claude:d".into(), "claude:c".into()],
            ..Config::default()
        };
        let c = candidates(&entries, &config, ts(NOW), None);
        let bands = pace_bands(&c, &ranked(&c));
        let by_name: Vec<usize> = (0..5).map(|i| bands[&i]).collect();
        assert_eq!(by_name, [0, 0, 0, 1, 1]);
        assert_eq!(
            order(&entries, &c),
            [
                "claude:c / default",
                "claude:a / default",
                "claude:b / default",
                "claude:e / default",
                "claude:d / default",
            ]
        );
        // The same, whatever order the pairs come in.
        let mut reversed = c.clone();
        reversed.reverse();
        let bands = pace_bands(&reversed, &ranked(&reversed));
        assert_eq!(bands.values().filter(|b| **b == 0).count(), 3);
        // Without `prefer`: the registry order within a band.
        config.prefer.clear();
        let c = candidates(&entries, &config, ts(NOW), None);
        assert_eq!(
            order(&entries, &c)[..3],
            [
                "claude:a / default",
                "claude:b / default",
                "claude:c / default"
            ]
        );
    }

    /// Two accounts of `strategy = "pace"`, `prefer = ["claude:b"]`, each with one fresh week:
    /// `(percent used, its reset)` for `a` and for `b`.
    fn two_paces(a: (f64, &str), b: (f64, &str)) -> (Vec<Entry>, Vec<Candidate>) {
        let week = |(used, resets): (f64, &str)| [row("Week (all models)", used, Some(resets))];
        let entries = vec![
            entry(Provider::Claude, "a", 5, &week(a)),
            entry(Provider::Claude, "b", 5, &week(b)),
        ];
        let config = Config {
            strategy: Strategy::Pace,
            prefer: vec!["claude:b".into()],
            ..Config::default()
        };
        let c = candidates(&entries, &config, ts(NOW), None);
        (entries, c)
    }

    /// R23 Rules (pace), GitHub review of PR #27: a pace exactly 0.9 of its band's top is in the
    /// band. 100% and 90% left of windows that both reset in 3601 s: the quotients, 99.97…%/h
    /// and 89.97…%/h, put the second about 1.4e-14 under 0.9 × the first in floating point;
    /// the terms compared without a division put it on the boundary. One band: `prefer`
    /// decides.
    #[test]
    fn a_pace_on_the_band_boundary_is_in_the_band() {
        let resets = "2026-09-27T11:00:01Z";
        let (entries, c) = two_paces((0.0, resets), (10.0, resets));
        let (a, b) = (c[0].pace.unwrap(), c[1].pace.unwrap());
        assert_eq!((a.seconds, b.seconds), (3601, 3601));
        assert!(
            b.per_hour() < 0.9 * a.per_hour(),
            "the case the review found: {} vs {}",
            b.per_hour(),
            0.9 * a.per_hour()
        );
        assert_eq!(
            pace_bands(&c, &ranked(&c))
                .into_values()
                .collect::<Vec<_>>(),
            [0, 0]
        );
        assert_eq!(
            order(&entries, &c),
            ["claude:b / default", "claude:a / default"]
        );
    }

    /// R23 Rules (pace), critic round 2: a pace truly below 0.9 of its band's top opens a band,
    /// however close. a: 99.99% left over 68743 s; b: 80% left over 61111 s. Exactly, b's pace
    /// is 0.9 × a's × (1 − 1/5499440001): below, by 1.8e-10, which a relative tolerance of 1e-9
    /// would have taken for the boundary. Two bands: a first, whatever `prefer` says.
    #[test]
    fn a_pace_just_below_the_band_boundary_opens_a_band() {
        let (entries, c) = two_paces(
            (0.01, "2026-09-28T05:05:43Z"),
            (20.0, "2026-09-28T02:58:31Z"),
        );
        let (a, b) = (c[0].pace.unwrap(), c[1].pace.unwrap());
        assert_eq!((a.seconds, b.seconds), (68743, 61111));
        let ratio = b.per_hour() / (0.9 * a.per_hour());
        assert!(
            ratio < 1.0 && ratio > 1.0 - 1e-9,
            "the critic's case: within 1e-9 under the boundary ({ratio})"
        );
        assert_eq!(
            pace_bands(&c, &ranked(&c))
                .into_values()
                .collect::<Vec<_>>(),
            [0, 1]
        );
        assert_eq!(
            order(&entries, &c),
            ["claude:a / default", "claude:b / default"]
        );
    }

    /// R23 Rules (pace), critic round 3: on the boundary over different seconds. a: 71.10% left
    /// over 5670 s; b: 41.08% over 3640 s; `10 × 41.08 × 5670 = 9 × 71.10 × 3640 = 2329236`.
    /// Multiplied in floating point the products differ by 5e-10 and b fell a band; in
    /// millionths of a percent and whole seconds they are equal. One band: b, preferred and
    /// resetting sooner, first.
    #[test]
    fn a_pace_on_the_boundary_over_other_seconds_is_in_the_band() {
        let (entries, c) = two_paces(
            (28.9, "2026-09-27T11:34:30Z"),
            (58.92, "2026-09-27T11:00:40Z"),
        );
        let (a, b) = (c[0].pace.unwrap(), c[1].pace.unwrap());
        assert_eq!(
            (a, b),
            (
                Pace {
                    left_micro: 71_100_000,
                    seconds: 5670
                },
                Pace {
                    left_micro: 41_080_000,
                    seconds: 3640
                }
            )
        );
        let (a_left, b_left) = (100.0 - 28.9, 100.0 - 58.92);
        assert!(
            10.0 * b_left * 5670.0 < 9.0 * a_left * 3640.0,
            "the critic's case: floating point puts b below"
        );
        assert_eq!(
            pace_bands(&c, &ranked(&c))
                .into_values()
                .collect::<Vec<_>>(),
            [0, 0]
        );
        assert_eq!(
            order(&entries, &c),
            ["claude:b / default", "claude:a / default"]
        );
    }

    /// R23 Rules (pace), critic round 3: equal paces over different seconds are interchangeable
    /// as a band's top. a: 34.60% left over 60480 s and b: 69.20% over 120960 s are the same
    /// pace; c, 31.14% over 60480 s, is exactly 0.9 of it. In whatever order the candidates
    /// come, one band, and the same ranking: c (preferred), then a (its reset sooner), then b.
    #[test]
    fn equal_paces_band_alike_in_any_order() {
        let entries = [
            entry(
                Provider::Claude,
                "a",
                5,
                &[row("Week (all models)", 65.4, Some("2026-09-28T02:48:00Z"))],
            ),
            entry(
                Provider::Claude,
                "b",
                5,
                &[row("Week (all models)", 30.8, Some("2026-09-28T19:36:00Z"))],
            ),
            entry(
                Provider::Claude,
                "c",
                5,
                &[row(
                    "Week (all models)",
                    68.86,
                    Some("2026-09-28T02:48:00Z"),
                )],
            ),
        ];
        let config = Config {
            strategy: Strategy::Pace,
            prefer: vec!["claude:c".into()],
            ..Config::default()
        };
        let c = candidates(&entries, &config, ts(NOW), None);
        let (a, b) = (c[0].pace.unwrap(), c[1].pace.unwrap());
        assert_eq!(a.cmp_pace(b), std::cmp::Ordering::Equal);
        assert_ne!(a, b);
        for perm in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            // The same candidates (each keeps its entry), in another order.
            let mut shuffled: Vec<Candidate> = perm.iter().map(|&i| c[i].clone()).collect();
            rank_rules(&mut shuffled, &entries, &config);
            let bands: BTreeMap<String, usize> = pace_bands(&shuffled, &ranked(&shuffled))
                .into_iter()
                .map(|(i, band)| (entries[shuffled[i].entry].account.qualified(), band))
                .collect();
            assert_eq!(
                bands.into_iter().collect::<Vec<_>>(),
                [
                    ("claude:a".to_string(), 0),
                    ("claude:b".to_string(), 0),
                    ("claude:c".to_string(), 0)
                ],
                "{perm:?}"
            );
            assert_eq!(
                order(&entries, &shuffled),
                [
                    "claude:c / default",
                    "claude:a / default",
                    "claude:b / default"
                ],
                "{perm:?}"
            );
        }
    }

    /// R23 Rules: `strategy = "headroom"` ranks as the rules always have, and the pace is only
    /// shown; `"pace"` ranks by the pace, then by the sooner reset of the window that gives it.
    #[test]
    fn the_strategy_decides_only_the_ranking() {
        let now = ts(NOW);
        let entries = [
            // 90% left over 160h: 0.5625%/h; headroom band 9.
            entry(
                Provider::Claude,
                "x",
                5,
                &[row("Week (all models)", 10.0, Some("2026-10-04T02:00:00Z"))],
            ),
            // 40% left over 20h: 2%/h; headroom band 4.
            entry(
                Provider::Claude,
                "y",
                5,
                &[row("Week (all models)", 60.0, Some("2026-09-28T06:00:00Z"))],
            ),
            // 50% left over 25h: 2%/h, as y, resetting later.
            entry(
                Provider::Claude,
                "z",
                5,
                &[row("Week (all models)", 50.0, Some("2026-09-28T11:00:00Z"))],
            ),
            // 5% left: not feasible, whatever its pace (5%/h).
            entry(
                Provider::Claude,
                "w",
                5,
                &[row("Week (all models)", 95.0, Some("2026-09-27T11:00:00Z"))],
            ),
        ];
        let headroom = candidates(&entries, &Config::default(), now, None);
        assert_eq!(
            order(&entries, &headroom),
            [
                "claude:x / default",
                "claude:z / default",
                "claude:y / default"
            ]
        );
        assert!(
            close(headroom[0].pace, 0.5625),
            "the pace is given all the same"
        );
        let config = Config {
            strategy: Strategy::Pace,
            ..Config::default()
        };
        let pace = candidates(&entries, &config, now, None);
        assert_eq!(
            order(&entries, &pace),
            [
                "claude:y / default",
                "claude:z / default",
                "claude:x / default"
            ]
        );
        // Feasibility, the binding window and when `--wait` would try again are the same.
        let without_rank = |c: &[Candidate]| -> Vec<Candidate> {
            c.iter()
                .map(|c| Candidate {
                    rules_rank: None,
                    ..c.clone()
                })
                .collect()
        };
        assert_eq!(without_rank(&pace), without_rank(&headroom));
        assert!(!pace[3].feasible() && close(pace[3].pace, 5.0));
        let blocked: Vec<Candidate> = pace.iter().filter(|c| !c.feasible()).cloned().collect();
        assert_eq!(
            next_attempt(&blocked, &entries, now),
            next_attempt(
                &headroom
                    .iter()
                    .filter(|c| !c.feasible())
                    .cloned()
                    .collect::<Vec<_>>(),
                &entries,
                now
            )
        );
    }

    fn two_accounts() -> (Vec<Entry>, Vec<Candidate>, Config) {
        let week = |pct: f64| [row("Week (all models)", pct, Some("2026-09-30T00:00:00Z"))];
        let entries = vec![
            entry(Provider::Claude, "max", 5, &week(20.0)),
            entry(Provider::Claude, "team", 5, &week(50.0)),
        ];
        let mut config = with_models(&["claude-opus-5-5", "claude-sonnet-5"], &[]);
        config.claude.efforts = vec!["medium".into(), "high".into(), "max".into()];
        config.claude.default_effort = Some("high".into());
        let c = candidates(&entries, &config, ts(NOW), None);
        (entries, c, config)
    }

    fn answered(choice: usize, confidence: f64, probs: &[(usize, f64)]) -> Asked {
        Asked::Answered(Answers {
            model: Some("jev-1.13.0".into()),
            launch: Some((choice, confidence)),
            probabilities: probs.iter().copied().collect(),
            efforts: BTreeMap::new(),
        })
    }

    /// R23: every branch of the combination.
    #[test]
    fn combine_takes_jev_only_when_confident() {
        let (entries, c, config) = two_accounts();
        // Candidates: 0 max/opus, 1 max/sonnet, 2 team/opus, 3 team/sonnet; rules: 0 first.
        assert_eq!(ranked(&c), [0, 2, 1, 3]);
        let d = decide(&c, &entries, &config, Asked::Skipped(Reason::NoKey)).unwrap();
        assert_eq!(
            (d.chosen, d.decided_by, d.reason),
            (0, DecidedBy::Rules, Some(Reason::NoKey))
        );
        assert_eq!(d.effort.as_deref(), Some("high"));
        assert_eq!(d.jev, None);

        let d = decide(
            &c,
            &entries,
            &config,
            answered(2, 0.9, &[(2, 0.95), (0, 0.05)]),
        )
        .unwrap();
        assert_eq!(
            (d.chosen, d.decided_by, d.reason),
            (2, DecidedBy::Jev, None)
        );

        // Unsure of the pair, sure of the account: its most probable model.
        let probs = [(0, 0.1), (1, 0.1), (2, 0.25), (3, 0.55)];
        let d = decide(&c, &entries, &config, answered(3, 0.3, &probs)).unwrap();
        assert_eq!((d.chosen, d.decided_by), (3, DecidedBy::JevAccount));
        let p = d.jev.unwrap().account_probability.unwrap();
        assert!((p - 0.8).abs() < 1e-9, "{p}");

        let probs = [(0, 0.3), (1, 0.2), (2, 0.3), (3, 0.2)];
        let d = decide(&c, &entries, &config, answered(0, 0.3, &probs)).unwrap();
        assert_eq!(
            (d.chosen, d.decided_by, d.reason),
            (0, DecidedBy::Rules, Some(Reason::LowConfidence))
        );
        assert_eq!(d.jev.unwrap().confidence, Some(0.3));

        let d = decide(&c, &entries, &config, Asked::Failed("HTTP 401".into())).unwrap();
        assert_eq!(d.reason, Some(Reason::JevError("HTTP 401".into())));
        assert_eq!(d.jev.unwrap().error.as_deref(), Some("HTTP 401"));
    }

    /// R23: Jev's effort from 0.50 confidence on, its score rounded to a level; else the default.
    #[test]
    fn combine_effort_rounds_the_score() {
        let (entries, c, config) = two_accounts();
        let with_effort = |score: f64, confidence: f64| {
            let Asked::Answered(mut a) = answered(0, 0.9, &[(0, 1.0)]) else {
                unreachable!()
            };
            a.efforts.insert(Provider::Claude, Ok((score, confidence)));
            decide(&c, &entries, &config, Asked::Answered(a)).unwrap()
        };
        assert_eq!(with_effort(1.6, 0.8).effort.as_deref(), Some("max"));
        assert_eq!(with_effort(0.4, 0.8).effort.as_deref(), Some("medium"));
        assert_eq!(with_effort(9.0, 0.8).effort.as_deref(), Some("max"));
        let low = with_effort(0.0, 0.4);
        assert_eq!(low.effort.as_deref(), Some("high"));
        assert_eq!(low.jev.unwrap().effort_confidence, Some(0.4));
    }

    #[test]
    fn nothing_feasible_decides_nothing() {
        let entries = [Entry {
            blocked: Some("excluded ([pick] exclude)".into()),
            ..entry(Provider::Claude, "max", 5, &[])
        }];
        let c = candidates(&entries, &Config::default(), ts(NOW), None);
        assert!(
            decide(
                &c,
                &entries,
                &Config::default(),
                Asked::Skipped(Reason::NoKey)
            )
            .is_none()
        );
        assert!(format_not_feasible(&entries, &c).contains("claude:max  excluded"));
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    /// R23: injected options go before the user's; one the user set is not injected. A named
    /// session to resume or fork gets nothing injected (R23 Resuming); arguments that neither
    /// start a session nor name one are refused.
    #[test]
    fn run_args_inject_before_the_users_and_respect_them() {
        let (args, notices) = run_args(
            Provider::Claude,
            Some("m"),
            Some("high"),
            &strings(&["-p", "hi"]),
        )
        .unwrap();
        assert_eq!(args, ["--model", "m", "--effort", "high", "-p", "hi"]);
        assert!(notices.is_empty());
        let (args, notices) = run_args(
            Provider::Claude,
            Some("m"),
            Some("high"),
            &strings(&["--model=x"]),
        )
        .unwrap();
        assert_eq!(args, ["--effort", "high", "--model=x"]);
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0].starts_with("--model is already in the arguments"),
            "{notices:?}"
        );
        let (args, _) = run_args(Provider::Claude, None, None, &[]).unwrap();
        assert!(args.is_empty());
        for resume in [
            &["--resume", "x"][..],
            &["-r", "x", "-p", "go on"],
            &["--resume=x"],
            &["--resume", "x", "--fork-session"],
        ] {
            let (args, notices) =
                run_args(Provider::Claude, Some("m"), Some("high"), &strings(resume)).unwrap();
            assert_eq!(args, resume, "nothing is injected into a resume");
            assert!(notices.is_empty());
        }
        for refused in [
            &["-c"][..],
            &["--resume"],
            &["--continue"],
            &["--session-id", "x"],
            &["--resume", "x", "-c"],
            &["--resume", "x", "--session-id", "y"],
            &["--resume", "x", "--fork-session", "--session-id", "y"],
            &["--resume", "x", "--resume", "y", "--fork-session"],
            &["agents"],
        ] {
            let err = run_args(Provider::Claude, None, None, &strings(refused)).unwrap_err();
            assert!(
                err.to_string()
                    .starts_with("`pick --run` starts a new session; these arguments do not"),
                "{refused:?}: {err}"
            );
        }
        assert!(run_args(Provider::Claude, Some("update"), None, &[]).is_err());

        let (args, _) =
            run_args(Provider::Codex, Some("g"), Some("xhigh"), &strings(&["hi"])).unwrap();
        assert_eq!(
            args,
            ["-m", "g", "-c", "model_reasoning_effort=xhigh", "hi"]
        );
        for user in [
            &["-c", "model_reasoning_effort=low"][..],
            &["--config=model_reasoning_effort=low"],
            &["-cmodel_reasoning_effort=low"],
        ] {
            let (args, notices) =
                run_args(Provider::Codex, Some("g"), Some("high"), &strings(user)).unwrap();
            assert_eq!(args[..2], ["-m", "g"], "{user:?}");
            assert_eq!(notices.len(), 1, "{user:?}");
        }
        let (args, _) = run_args(Provider::Codex, Some("g"), None, &strings(&["-mx"])).unwrap();
        assert_eq!(args, ["-mx"]);
        for resume in [&["resume", "id"][..], &["fork", "id", "go on"]] {
            let (args, _) =
                run_args(Provider::Codex, Some("g"), Some("high"), &strings(resume)).unwrap();
            assert_eq!(args, resume, "no -m, -c or -C");
        }
        for refused in [
            &["fork"][..],
            &["resume", "--last"],
            &["resume", "id", "--last"],
        ] {
            let err = run_args(Provider::Codex, Some("g"), None, &strings(refused)).unwrap_err();
            assert!(
                err.to_string()
                    .starts_with("`pick --run` starts a new session; `codex "),
                "{refused:?}: {err}"
            );
        }
    }

    /// R23 (Resuming): which arguments name a session, and whose.
    #[test]
    fn session_args_accept_only_a_named_session() {
        let id = "766560c5-0000-4000-8000-000000000000";
        let named = |args: &[&str]| {
            session_args(&strings(args))
                .unwrap()
                .map(|r| (r.provider, r.kind, r.id))
        };
        let claude = |kind| Some((Provider::Claude, kind, id.to_string()));
        let codex = |kind| Some((Provider::Codex, kind, id.to_string()));
        assert_eq!(named(&["--resume", id]), claude(SessionKind::Resume));
        assert_eq!(
            named(&["-r", id, "-p", "go on"]),
            claude(SessionKind::Resume)
        );
        assert_eq!(
            named(&[&format!("--resume={id}")]),
            claude(SessionKind::Resume)
        );
        assert_eq!(
            named(&["--resume", id, "--fork-session"]),
            claude(SessionKind::Fork)
        );
        assert_eq!(named(&["resume", id]), codex(SessionKind::Resume));
        assert_eq!(named(&["fork", id, "go on"]), codex(SessionKind::Fork));
        // A new session, or arguments `run_args` refuses: none named.
        for args in [
            &[][..],
            &["-p", "hi"],
            &["-c"],
            &["--resume"],
            &["--session-id", id],
            &["--resume", id, "-c"],
            &["--resume", id, "--resume", "other"],
            &["--resume", id, "--session-id", "other"],
            &["--resume", id, "--fork-session", "--session-id", "other"],
            // A fork names exactly one session too (R6 would fork the first).
            &["--resume", id, "--resume", "other", "--fork-session"],
            &["-r", id, "--resume", "other", "--fork-session"],
            &["--resume", id, "--fork-session", "--resume=other"],
            &["--resume", id, "--fork-session", "-c"],
            &["resume", "--last"],
            &["fork"],
        ] {
            assert_eq!(named(args), None, "{args:?}");
        }
        let err = session_args(&strings(&["resume", "a", "--resume", id])).unwrap_err();
        assert!(err.to_string().ends_with("ambiguous"), "{err}");
    }

    /// A session resumed with `claude:<last>` its last account, active `age_min` minutes
    /// before [`NOW`].
    fn resuming(last: &str, age_min: i64, model: Option<&str>) -> Session {
        let active_at = ts(NOW) - jiff::SignedDuration::from_mins(age_min);
        Session {
            resume: Resume {
                provider: Provider::Claude,
                kind: SessionKind::Resume,
                id: "766560c5-0000-4000-8000-000000000000".into(),
            },
            last: Some(LastRun {
                account: format!("claude:{last}"),
                home: format!("/h/{last}"),
                launched_at: active_at,
                active_at,
            }),
            // One store holds it, which both accounts share.
            seen_by: model.map(|model| {
                ["claude:max", "claude:team"]
                    .into_iter()
                    .map(|a| (a.to_string(), Some(model.to_string())))
                    .collect()
            }),
        }
    }

    /// R23 (Resuming): the account that ran the session within `affinity_minutes` ranks first
    /// among the feasible pairs, whatever its headroom; each account is one pair without a
    /// model, whatever `models` says.
    #[test]
    fn affinity_ranks_the_warm_account_first() {
        let week = |pct: f64| [row("Week (all models)", pct, Some("2026-09-30T00:00:00Z"))];
        let entries = vec![
            entry(Provider::Claude, "max", 5, &week(20.0)),
            entry(Provider::Claude, "team", 5, &week(70.0)),
        ];
        let config = with_models(&["claude-opus-5-5", "claude-sonnet-5"], &[]).for_resume();
        let session = resuming("team", 30, None);
        let c = candidates(&entries, &config, ts(NOW), Some(&session));
        assert_eq!(
            order(&entries, &c),
            ["claude:team / default", "claude:max / default"]
        );
        assert_eq!(c.len(), 2);
        assert_eq!(
            c.iter().map(|c| c.affine).collect::<Vec<_>>(),
            [false, true]
        );
        // Without the session, headroom decides.
        let c = candidates(&entries, &config, ts(NOW), None);
        assert_eq!(
            order(&entries, &c),
            ["claude:max / default", "claude:team / default"]
        );
        assert!(c.iter().all(|c| !c.affine));
        // Right at the limit it is warm; a minute past, nothing is preferred.
        let mut config = config;
        config.affinity_minutes = 30;
        let c = candidates(&entries, &config, ts(NOW), Some(&session));
        assert_eq!(order(&entries, &c)[0], "claude:team / default");
        config.affinity_minutes = 29;
        let c = candidates(&entries, &config, ts(NOW), Some(&session));
        assert_eq!(order(&entries, &c)[0], "claude:max / default");
        assert!(c.iter().all(|c| !c.affine));
        // 0: affinity off, however recent.
        config.affinity_minutes = 0;
        let c = candidates(&entries, &config, ts(NOW), Some(&resuming("team", 0, None)));
        assert_eq!(order(&entries, &c)[0], "claude:max / default");
        // Never launched through remuda: nothing to prefer.
        let mut unknown = resuming("team", 0, None);
        unknown.last = None;
        let c = candidates(&entries, &Config::default(), ts(NOW), Some(&unknown));
        assert_eq!(order(&entries, &c)[0], "claude:max / default");
    }

    /// R23 (Resuming): affinity orders, it never admits. A warm account short of headroom, or
    /// blocked, stays infeasible, and the report says resuming elsewhere rewrites its cache.
    #[test]
    fn an_infeasible_warm_account_stays_infeasible() {
        let week = |pct: f64| [row("Week (all models)", pct, Some("2026-09-30T00:00:00Z"))];
        let entries = vec![
            entry(Provider::Claude, "max", 5, &week(20.0)),
            entry(Provider::Claude, "team", 5, &week(95.0)),
        ];
        let config = Config::default().for_resume();
        let session = resuming("team", 12, Some("claude-opus-5-5"));
        let c = candidates(&entries, &config, ts(NOW), Some(&session));
        assert!(c[1].affine);
        assert!(!c[1].feasible());
        assert_eq!(c[1].rules_rank, None);
        assert_eq!(ranked(&c), [0]);
        let d = decide(&c, &entries, &config, Asked::Skipped(Reason::NoKey)).unwrap();
        assert_eq!((d.chosen, d.effort.as_deref()), (0, None));
        let text = format_text(&entries, &c, &d, &config, ts(NOW), &[], Some(&session));
        assert!(
            text.contains(
                "session     resume 766560c5-0000-4000-8000-000000000000: claude:team ran it 12m \
                 ago (prompt cache warm: preferred); model claude-opus-5-5\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "claude:team ran this session 12m ago, but Week (all models): 95% used, below \
                 the 10% left required, resets in 2d14h; resuming as another account rewrites \
                 its prompt cache"
            ),
            "{text}"
        );
        assert!(
            text.contains("model       the session's (claude-opus-5-5)\n"),
            "{text}"
        );
        // Blocked as a whole: the same.
        let blocked = vec![
            entries[0].clone(),
            Entry {
                blocked: Some("excluded ([pick] exclude)".into()),
                ..entries[1].clone()
            },
        ];
        let c = candidates(&blocked, &config, ts(NOW), Some(&session));
        assert!(c[1].affine && !c[1].feasible());
        assert_eq!(ranked(&c), [0]);
        let d = decide(&c, &blocked, &config, Asked::Skipped(Reason::NoKey)).unwrap();
        let v = to_json(
            &blocked,
            &c,
            Some(&d),
            &config,
            &[],
            Some(&session),
            ts(NOW),
        );
        assert_eq!(v["session"]["affine"], true);
        assert_eq!(v["session"]["last_account"], "claude:team");
        assert_eq!(v["session"]["age_seconds"], 720);
        assert_eq!(v["candidates"][1]["affine"], true);
        assert_eq!(v["candidates"][1]["feasible"], false);
        assert_eq!(v["account"], "claude:max");
    }

    /// R23 (Resuming): the windows of the session's model apply when it is known; unknown, the
    /// per-model windows are shown, not counted, as for the agent's default.
    #[test]
    fn a_known_session_model_counts_its_windows() {
        let rows = [
            row("Week (all models)", 30.0, Some("2026-09-30T10:00:00Z")),
            row("Week (Fable)", 95.0, Some("2026-09-30T10:00:00Z")),
        ];
        let entries = [entry(Provider::Claude, "max", 5, &rows)];
        let config = with_models(&["claude-opus-5-5"], &[]).for_resume();
        let fable = resuming("max", 500, Some("claude-fable-5-1"));
        let c = candidates(&entries, &config, ts(NOW), Some(&fable));
        assert_eq!(c[0].model, None);
        assert!(
            !c[0].feasible(),
            "the Fable week counts: {:?}",
            c[0].why_not
        );
        assert!(c[0].default_model_windows.is_empty());
        let opus = resuming("max", 500, Some("claude-opus-5-5"));
        let c = candidates(&entries, &config, ts(NOW), Some(&opus));
        assert_eq!(c[0].headroom, Some(70.0));
        let unknown = resuming("max", 500, None);
        let c = candidates(&entries, &config, ts(NOW), Some(&unknown));
        assert_eq!(c[0].headroom, Some(70.0));
        assert_eq!(c[0].default_model_windows.len(), 1);
    }

    /// R23 (Output): the command shown replays as the very words, whatever the user's
    /// arguments hold: spaces, quotes, an empty word, shell syntax.
    #[test]
    fn the_command_shown_is_quoted_for_the_shell() {
        let words = strings(&[
            "remuda",
            "run",
            "claude:max",
            "--model",
            "claude-opus-5-5",
            "-p",
            "Please print $(printf CHANGED) with spaces",
            "",
            "it's \"quoted\" `x` \\ $HOME *",
            "a\nb",
        ]);
        let line = shell_line(&words);
        assert!(
            line.starts_with("remuda run claude:max --model claude-opus-5-5 -p 'Please print"),
            "{line}"
        );
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("printf '%s\\0' {line}"))
            .output()
            .unwrap();
        assert!(out.status.success(), "{line}");
        let back: Vec<String> = String::from_utf8(out.stdout)
            .unwrap()
            .split_terminator('\0')
            .map(str::to_string)
            .collect();
        assert_eq!(back, words, "{line}");
    }

    /// R23 (Resuming): of several copies of a session in one store, the one written to last;
    /// of equal times, the one in the launch's directory; else none is known.
    #[test]
    fn copy_in_takes_the_latest_then_the_launch_directory() {
        let tail = |ts: Option<&str>, cwd: &str, model: &str| SessionTail {
            ts_last: ts.map(str::to_string),
            cwd_last: Some(cwd.to_string()),
            model: Some(model.to_string()),
        };
        let model = |tails: Vec<SessionTail>, cwd: Option<&str>| {
            copy_in(tails, cwd).map(|t| t.model.unwrap())
        };
        let old = tail(Some("2026-10-08T07:00:00Z"), "/w", "old");
        let new = tail(Some("2026-10-08T09:59:00Z"), "/w", "new");
        let none = tail(None, "/w", "untimed");
        // Whatever their order: the latest.
        assert_eq!(
            model(vec![old.clone(), new.clone(), none.clone()], Some("/w")).as_deref(),
            Some("new")
        );
        assert_eq!(
            model(vec![new.clone(), old.clone()], Some("/w")).as_deref(),
            Some("new")
        );
        assert_eq!(
            model(vec![none.clone(), old.clone()], None).as_deref(),
            Some("old")
        );
        // A tie: the one in the launch's directory, else none.
        let here = tail(Some("2026-10-08T09:59:00Z"), "/here", "here");
        assert_eq!(
            model(vec![new.clone(), here.clone()], Some("/here")).as_deref(),
            Some("here")
        );
        assert_eq!(
            model(vec![new.clone(), here.clone()], Some("/elsewhere")),
            None
        );
        assert_eq!(model(vec![new.clone(), here.clone()], None), None);
        assert_eq!(model(vec![new.clone(), new.clone()], Some("/w")), None);
        assert_eq!(model(vec![none.clone(), none.clone()], Some("/x")), None);
        assert_eq!(model(Vec::new(), Some("/w")), None);
    }

    #[test]
    fn durations_until_a_reset() {
        let now = ts(NOW);
        let at = |secs: i64| now + jiff::SignedDuration::from_secs(secs);
        assert_eq!(format_in(at(45 * 60), now), "45m");
        assert_eq!(format_in(at(3600), now), "1h");
        assert_eq!(format_in(at(80 * 60), now), "1h20m");
        assert_eq!(format_in(at(2 * 86_400 + 3 * 3600 + 59), now), "2d3h");
        assert_eq!(format_in(at(4 * 86_400), now), "4d");
        assert_eq!(format_in(at(-5), now), "0m");
    }

    /// R23 `--wait`: a pair infeasible by a usage window waits on its binding window: the
    /// earliest such reset over every pair, plus the margin; a blocked account does not count.
    #[test]
    fn next_attempt_waits_for_the_earliest_reset_plus_a_margin() {
        let now = ts(NOW);
        let mut excluded = entry(Provider::Claude, "work", 5, &[]);
        excluded.blocked = Some("excluded ([pick] exclude)".into());
        let entries = [
            entry(
                Provider::Claude,
                "max",
                5,
                &[row(
                    "Week (all models)",
                    100.0,
                    Some("2026-09-29T10:00:00Z"),
                )],
            ),
            entry(
                Provider::Claude,
                "team",
                5,
                &[
                    row("Session", 95.0, Some("2026-09-27T11:12:00Z")),
                    row("Week (all models)", 40.0, Some("2026-09-28T10:00:00Z")),
                ],
            ),
            excluded,
        ];
        let c = candidates(&entries, &Config::default(), now, None);
        assert!(ranked(&c).is_empty());
        match next_attempt(&c, &entries, now) {
            wait::Wait::At { at, on } => {
                assert_eq!(at, ts("2026-09-27T11:12:30Z"));
                assert_eq!(on.account, "claude:team");
                assert_eq!(on.window.label, "Session");
            }
            other => panic!("{other:?}"),
        }
    }

    /// R23 `--wait`: right after a reset that freed nothing (another window still blocks,
    /// its reset just ahead), the next attempt is still a minute away: no spinning.
    #[test]
    fn next_attempt_waits_at_least_a_minute() {
        let now = ts(NOW);
        let entries = [entry(
            Provider::Claude,
            "max",
            5,
            &[
                // Reset since it was cached: unknown, frees nothing by itself.
                row("Session", 100.0, Some("2026-09-27T09:59:50Z")),
                row("Week (all models)", 100.0, Some("2026-09-27T10:00:01Z")),
            ],
        )];
        let c = candidates(&entries, &Config::default(), now, None);
        assert!(!c[0].feasible());
        let wait = next_attempt(&c, &entries, now);
        assert_eq!(wait.next(), Some(ts("2026-09-27T10:01:00Z")));
    }

    /// R23 `--wait`: a blocking window with no reset ahead known is tried again in five
    /// minutes.
    #[test]
    fn next_attempt_without_a_known_reset_retries_in_five_minutes() {
        let now = ts(NOW);
        let entries = [entry(
            Provider::Claude,
            "max",
            5,
            &[row("Week (all models)", 97.0, None)],
        )];
        let c = candidates(&entries, &Config::default(), now, None);
        let wait = next_attempt(&c, &entries, now);
        assert!(matches!(wait, wait::Wait::Unknown { .. }), "{wait:?}");
        assert_eq!(wait.next(), Some(ts("2026-09-27T10:05:00Z")));
    }

    /// R23 `--wait`: what time does not change (excluded, another provider, not logged in,
    /// no `codex`) is nothing to wait for; one pair blocked by a window is enough to wait.
    #[test]
    fn next_attempt_never_waits_on_what_time_does_not_change() {
        let now = ts(NOW);
        let blocked = |provider, name: &str, why: &str| {
            let mut e = entry(provider, name, 5, &[]);
            e.usage = None;
            e.blocked = Some(why.into());
            e
        };
        let mut entries = vec![
            blocked(Provider::Claude, "work", "excluded ([pick] exclude)"),
            blocked(Provider::Codex, "cx", "`codex` not found on PATH"),
        ];
        let c = candidates(&entries, &Config::default(), now, None);
        assert_eq!(
            next_attempt(&c, &entries, now),
            wait::Wait::Never(
                "claude:work excluded ([pick] exclude); codex:cx `codex` not found on PATH".into()
            )
        );
        assert_eq!(
            next_attempt(&[], &[], now),
            wait::Wait::Never("no account is listed".into())
        );
        entries.push(entry(
            Provider::Claude,
            "max",
            5,
            &[row("Session", 100.0, Some("2026-09-27T12:00:00Z"))],
        ));
        let c = candidates(&entries, &Config::default(), now, None);
        assert_eq!(
            next_attempt(&c, &entries, now).next(),
            Some(ts("2026-09-27T12:00:30Z"))
        );
    }

    /// R23 `--wait` without `--live`: each attempt reads the same cache at a later instant.
    /// Once the blocking window's reset has passed it is unknown, and the pair feasible of
    /// unknown headroom: the wait ends at the reset plus the margin, the window named as reset
    /// since cached, with the hint to `--live`. Waiting never makes remuda query live.
    #[test]
    fn a_cached_wait_ends_right_after_the_reset_of_unknown_headroom() {
        struct Quiet;
        impl wait::Status for Quiet {
            fn show(&mut self, _: &wait::Wait, _: Timestamp) {}
            fn clear(&mut self) {}
        }
        let cached = CachedUsage {
            fetched_at: Some(ts(NOW) - jiff::SignedDuration::from_mins(5)),
            rows: vec![row(
                "Week (all models)",
                100.0,
                Some("2026-09-27T12:00:00Z"),
            )],
        };
        let clock = std::cell::Cell::new(ts(NOW));
        let mut attempts = Vec::new();
        let ((entries, c), ended) = wait::until(
            || {
                let now = clock.get();
                attempts.push(now);
                let entries = vec![Entry {
                    account: account(Provider::Claude, "max"),
                    blocked: None,
                    usage: Some(Snapshot::cached(&cached).at(now)),
                    notes: Vec::new(),
                }];
                let c = candidates(&entries, &Config::default(), now, None);
                let wait = ranked(&c)
                    .is_empty()
                    .then(|| next_attempt(&c, &entries, now));
                Ok::<_, std::convert::Infallible>(((entries, c), wait))
            },
            None,
            || clock.get(),
            |d| clock.set(clock.get().checked_add(d).unwrap()),
            &mut Quiet,
        )
        .unwrap();
        assert_eq!(ended, wait::Ended::Ready);
        assert_eq!(attempts, [ts(NOW), ts("2026-09-27T12:00:30Z")]);
        assert!(c[0].feasible());
        assert_eq!(c[0].headroom, None);
        assert_eq!(
            live_hint(&entries[0], &c[0]).as_deref(),
            Some("Week (all models): reset since cached (--live asks the agent)")
        );
    }
}
