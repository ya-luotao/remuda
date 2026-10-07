//! `remuda pick` (SPEC R23): which account and model to launch now, and at what effort. Rules
//! decide what is feasible (headroom on every window that applies, exclusions, logins) and rank
//! it; with a key and notes, Jev chooses among the feasible options ([`crate::jev`]), and its
//! answer is taken only when it is confident enough. What a usage window holds now is
//! [`crate::usage::snapshot`]'s to say.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Result, bail};
use jiff::Timestamp;
use serde_json::{Value, json};
use toml_edit::DocumentMut;

use crate::account_command::Runner;
use crate::identity::{self, Identity};
use crate::launch::{self, Intent};
use crate::provider::Provider;
use crate::registry::{Account, Registry};
use crate::usage::{self, CachedUsage, LiveUsage, Reading, Snapshot, Source, UsageRow, Window};
use crate::{Env, probe};

/// Percent left required on every window that applies, unless `[pick] min_headroom` says (R23).
pub const DEFAULT_MIN_HEADROOM: u32 = 10;
/// Minutes after which cached usage is stale, unless `[pick] stale_after` says (R23).
pub const DEFAULT_STALE_AFTER: u32 = 120;
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
/// How long `codex login status` may take (R4: about 0.05 s), as for `remuda list`.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(15);

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

const KEYS: [&str; 7] = [
    "exclude",
    "prefer",
    "min_headroom",
    "stale_after",
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
    /// The windows that apply and have reset since their usage was recorded: of unknown
    /// usage, shown, never counted.
    pub reset_passed: Vec<Window>,
    /// For a pair without a model: the per-model windows, which apply only if the agent's
    /// default model is of their family. Shown, never counted: remuda does not know that model.
    pub default_model_windows: Vec<Window>,
    /// 1-based position among the feasible pairs by the rules.
    pub rules_rank: Option<usize>,
}

impl Candidate {
    pub fn feasible(&self) -> bool {
        self.why_not.is_none()
    }
}

/// Every candidate of `entries`, feasibility decided and the feasible ones ranked by the rules
/// (R23); in entry order, each account's models in `models` order.
pub fn candidates(entries: &[Entry], config: &Config, now: Timestamp) -> Vec<Candidate> {
    let mut out = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        if let Some(blocked) = &entry.blocked {
            out.push(Candidate {
                entry: i,
                model: None,
                model_rank: 0,
                why_not: Some(blocked.clone()),
                headroom: None,
                binding: None,
                reset_passed: Vec::new(),
                default_model_windows: Vec::new(),
                rules_rank: None,
            });
            continue;
        }
        let provider = entry.account.provider;
        let models: Vec<Option<&String>> = match &config.choices(provider).models {
            models if models.is_empty() => vec![None],
            models => models.iter().map(Some).collect(),
        };
        for (rank, model) in models.into_iter().enumerate() {
            let windows: Vec<&Window> = entry
                .usage
                .iter()
                .flat_map(|u| &u.windows)
                .filter(|w| match (&w.model, model) {
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
            let reset_passed: Vec<Window> = windows
                .iter()
                .filter(|w| w.reset_passed())
                .map(|w| (*w).clone())
                .collect();
            let default_model_windows: Vec<Window> = match model {
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
                reset_passed,
                default_model_windows,
                rules_rank: None,
            });
        }
    }
    rank_rules(&mut out, entries, config);
    out
}

/// Numbers the feasible candidates by the rules (R23): known headroom before unknown (stale
/// data is still known; no usage data, or every window reset since, is not); then the model's
/// position in `models`; the 10-point headroom band, higher first (90% left and more is one
/// band); fresh before stale; the binding window's reset, sooner first; `prefer` order;
/// registry order.
pub fn rank_rules(candidates: &mut [Candidate], entries: &[Entry], config: &Config) {
    let mut order: Vec<usize> = (0..candidates.len())
        .filter(|&i| candidates[i].feasible())
        .collect();
    let key = |c: &Candidate| {
        let entry = &entries[c.entry];
        let qualified = entry.account.qualified();
        let prefer = config
            .prefer
            .iter()
            .position(|p| *p == qualified)
            .unwrap_or(config.prefer.len());
        (
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
    })
}

/// The options `remuda run` gets for `model` and `effort` (R23): claude `--model <m> --effort
/// <e>`, codex `-m <m> -c model_reasoning_effort=<e>`, before `user_args`. One the user's
/// arguments already set is not injected, with a notice. Refuses arguments that do not start a
/// new session.
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
    match provider {
        Provider::Claude => {
            if launch::classify(user_args) != Intent::NewSession {
                bail!(
                    "`pick --run` starts a new session; these arguments do not: {}",
                    user_args.join(" ")
                );
            }
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
            if matches!(
                user_args.first().map(String::as_str),
                Some("resume" | "fork")
            ) {
                bail!(
                    "`pick --run` starts a new session; `codex {}` does not",
                    user_args[0]
                );
            }
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
    if provider == Provider::Claude && launch::classify(&args) != Intent::NewSession {
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
    /// Only this provider's accounts are candidates.
    pub provider: Option<Provider>,
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
    if let Some(only) = sources.provider
        && only != provider
    {
        entry.blocked = Some(format!("not a {only} account (--provider {only})"));
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

/// `remuda run <account> <options>` for the decision, as words.
pub fn command(entry: &Entry, candidate: &Candidate, effort: Option<&str>) -> Vec<String> {
    let provider = entry.account.provider;
    let (args, _) = run_args(provider, candidate.model.as_deref(), effort, &[])
        .expect("injected options alone start a new session");
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

/// The plain-text report of `remuda pick` (R23).
pub fn format_text(
    entries: &[Entry],
    candidates: &[Candidate],
    decision: &Decision,
    config: &Config,
    now: Timestamp,
) -> String {
    let c = &candidates[decision.chosen];
    let entry = &entries[c.entry];
    let mut rows: Vec<(&str, String)> = vec![
        ("account", entry.account.qualified()),
        (
            "model",
            c.model
                .clone()
                .unwrap_or_else(|| "agent default".to_string()),
        ),
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
        command(entry, c, decision.effort.as_deref()).join(" "),
    ));
    let mut out: String = rows
        .iter()
        .map(|(k, v)| format!("{k:<10}  {v}\n"))
        .collect();
    out.push_str(&format_not_feasible(entries, candidates));
    out
}

/// `claude:max / claude-opus-5-5, effort high; decided by jev (confidence 0.90)`.
pub fn summary(entries: &[Entry], candidates: &[Candidate], decision: &Decision) -> String {
    let c = &candidates[decision.chosen];
    let effort = decision.effort.as_deref().unwrap_or("agent default");
    format!(
        "{}, effort {effort}; decided by {}",
        pair_label(&entries[c.entry], c),
        decided_text(decision, candidates, entries)
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
            format!("rules ({}: {why})", reason.name())
        }
        (DecidedBy::Rules, None) => "rules".to_string(),
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

/// The `--json` report (R23); `decision` `None` when nothing is feasible.
pub fn to_json(
    entries: &[Entry],
    candidates: &[Candidate],
    decision: Option<&Decision>,
    config: &Config,
) -> Value {
    let ts = |t: Option<Timestamp>| t.map(|t| t.to_string());
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
            "candidates": list,
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
        "command": command(entry, c, d.effort.as_deref()),
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
             min_headroom = 25\nstale_after = 30\nnotes = \"\"\"\n  Keep max for refactors.\n\"\"\"\n\n\
             [pick.claude]\nmodels = [\"claude-opus-5-5\", \"fable\"]\n\
             efforts = [\"high\", \"max\"]\ndefault_effort = \"high\"\n"
        ))
        .unwrap();
        assert_eq!(config.exclude, ["codex:work"]);
        assert_eq!(config.prefer, ["claude:max"]);
        assert_eq!((config.min_headroom, config.stale_after), (25, 30));
        assert_eq!(config.notes, "Keep max for refactors.");
        assert_eq!(config.claude.models, ["claude-opus-5-5", "fable"]);
        assert_eq!(config.claude.default_effort.as_deref(), Some("high"));
        assert_eq!(config.codex, Choices::default());
        // Without `[pick]`: the defaults, not zeros.
        let none = parse(ACCOUNTS).unwrap();
        assert_eq!((none.min_headroom, none.stale_after), (10, 120));
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
        let c = candidates(&entries, &config, ts(NOW));
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
        let c = candidates(&entries, &config, now);
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
        let c = candidates(&entries, &config, now);
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
        let c = candidates(&entries, &config, now);
        assert!(c[0].feasible(), "{:?}", c[0].why_not);
        assert_eq!((c[0].headroom, &c[0].binding), (None, &None));
        assert_eq!(
            reset_passed_text(&c[0]).as_deref(),
            Some("Session, Week (all models): reset since cached")
        );
        assert_eq!(c[0].rules_rank, Some(1));

        let decision = decide(&c, &entries, &config, Asked::Skipped(Reason::NoKey)).unwrap();
        let text = format_text(&entries, &c, &decision, &config, now);
        for line in [
            "limit       unknown\n",
            "unknown     Session, Week (all models): reset since cached (--live asks the agent)\n",
            "usage       cached 4d ago (stale: may be higher now)\n",
        ] {
            assert!(text.contains(line), "{line:?} in:\n{text}");
        }
        let v = to_json(&entries, &c, Some(&decision), &config);
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
        let c = candidates(&entries, &config, now);
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
        let v = to_json(&entries, &c, None, &config);
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
        let c = candidates(&entries, &Config::default(), now);
        assert!(!c[0].feasible());
        assert_eq!(
            c[0].why_not.as_deref(),
            Some("Week (all models): 95% used, below the 10% left required")
        );
        assert!(c[0].reset_passed.is_empty());
        let v = to_json(&entries, &c, None, &Config::default());
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
        let c = candidates(&entries, &Config::default(), now);
        assert!(
            c[0].reset_passed.is_empty(),
            "it does not apply to the pair"
        );
        assert_eq!(
            used_text(&c[0].default_model_windows[0], now),
            "Week (Fable) usage unknown (reset since cached)"
        );
        let v = to_json(&entries, &c, None, &Config::default());
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
            order(&entries, &candidates(&entries, &config, now))[..6],
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
        let c = candidates(&entries, &config, now);
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
        let c = candidates(&entries, &Config::default(), now);
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
        let c = candidates(&entries, &Config::default(), ts(NOW));
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
        let c = candidates(&entries, &config, ts(NOW));
        assert!(c[0].default_model_windows.is_empty());
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
        let c = candidates(&entries, &config, ts(NOW));
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
        let c = candidates(&entries, &Config::default(), ts(NOW));
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

    /// R23: injected options go before the user's; one the user set is not injected.
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
        assert!(
            run_args(
                Provider::Claude,
                Some("m"),
                None,
                &strings(&["--resume", "x"])
            )
            .is_err()
        );
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
        assert!(
            run_args(
                Provider::Codex,
                Some("g"),
                None,
                &strings(&["resume", "id"])
            )
            .is_err()
        );
        assert!(run_args(Provider::Codex, Some("g"), None, &strings(&["fork"])).is_err());
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
}
