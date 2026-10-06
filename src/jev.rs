//! The one network request of remuda's own (SPEC R23): `remuda pick` asks TypeSafe's Jev model
//! to choose among the options the rules found feasible. The state is plain text with account
//! names aliased (R21's aliases); the request goes through `curl`, the key on its standard input,
//! never in its arguments.

use std::path::Path;
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde_json::{Map, Value, json};

use crate::pick::{self, Answers, Asked, Candidate, Config, Entry, Reason};
use crate::privacy::{self, Aliases};
use crate::probe::{self, Outcome};
use crate::provider::Provider;
use crate::usage::{self, Source};

pub const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
/// The model asked; the response names the version that answered.
pub const MODEL: &str = "jev-latest";
/// How long the request may take, all included (`curl --max-time`).
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// The variable holding the API key; read from remuda's environment only.
pub const KEY_VAR: &str = "TYPESAFE_API_KEY";
/// At most this much of a response goes into an error message.
const SNIPPET: usize = 200;

/// A request and what its answers mean.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub body: Value,
    /// Each option and its candidate, the rules' best first. With one, there is no `launch`
    /// Choice: that option is the launch, and only efforts are asked.
    pub offered: Vec<(String, usize)>,
    /// Whether the `launch` Choice is asked (two options or more).
    pub launch: bool,
    /// The providers asked an effort Score, with their levels.
    pub efforts: Vec<(Provider, Vec<String>)>,
}

/// What a level means, for the effort Score; a level remuda does not know goes by its name.
fn effort_meaning(level: &str) -> Option<&'static str> {
    Some(match level {
        "minimal" => "the least reasoning, for quick and simple edits",
        "low" => "light reasoning, for routine tasks",
        "medium" => "moderate reasoning, for everyday coding",
        "high" => "deep reasoning, for complex changes",
        "xhigh" => "very deep reasoning, for hard problems; uses the limits much faster",
        "max" => "the most reasoning, for the hardest problems; uses the limits fastest",
        _ => return None,
    })
}

/// The request for the feasible `candidates` (R23): a `launch` Choice over the rules' best
/// [`pick::MAX_OPTIONS`] pairs, when there are two or more, and an effort Score for each provider
/// with at least two `efforts` and an option. The criteria are a JSON object: their order carries
/// no meaning.
pub fn request(
    entries: &[Entry],
    candidates: &[Candidate],
    config: &Config,
    aliases: &Aliases,
    now: Timestamp,
    tz: &TimeZone,
) -> Request {
    let offered: Vec<(String, usize)> = pick::ranked(candidates)
        .into_iter()
        .take(pick::MAX_OPTIONS)
        .map(|c| {
            let entry = &entries[candidates[c].entry];
            let label = format!(
                "{} / {}",
                aliases.qualified(&entry.account.qualified()),
                candidates[c].model.as_deref().unwrap_or("default")
            );
            (label, c)
        })
        .collect();
    let criteria: Map<String, Value> = offered
        .iter()
        .map(|(label, c)| {
            let candidate = &candidates[*c];
            let entry = &entries[candidate.entry];
            (
                label.clone(),
                Value::String(option_text(entry, candidate, config, now)),
            )
        })
        .collect();
    let mut questions = Map::new();
    let launch = offered.len() > 1;
    if launch {
        questions.insert(
            "launch".into(),
            json!({
                "type": "choice",
                "instructions": "Which account and model should the user launch now? Weigh \
                    remaining headroom, how soon limits reset, how fresh the usage data is, and \
                    the user's notes; follow the notes when they apply.",
                "criteria": criteria,
            }),
        );
    }
    let mut efforts = Vec::new();
    for provider in Provider::ALL {
        let levels = &config.choices(provider).efforts;
        let has_option = offered
            .iter()
            .any(|(_, c)| entries[candidates[*c].entry].account.provider == provider);
        if levels.len() < 2 || !has_option {
            continue;
        }
        let criteria: Vec<String> = levels
            .iter()
            .map(|level| match effort_meaning(level) {
                Some(meaning) => format!("{level}: {meaning}"),
                None => level.clone(),
            })
            .collect();
        questions.insert(
            format!("effort_{provider}"),
            json!({
                "type": "score",
                "instructions": format!(
                    "How much reasoning effort should a {provider} session use now, given the \
                     notes and remaining headroom?"
                ),
                "criteria": criteria,
            }),
        );
        efforts.push((provider, levels.clone()));
    }
    let body = json!({
        "state": state_text(entries, candidates, &offered, config, aliases, now, tz),
        "model": MODEL,
        "questions": questions,
    });
    Request {
        body,
        offered,
        launch,
        efforts,
    }
}

/// An option's description: its binding window, the windows of unknown usage (reset since
/// cached), the per-model windows that may apply to the agent's default model, and how old the
/// data is.
fn option_text(entry: &Entry, c: &Candidate, config: &Config, now: Timestamp) -> String {
    let unknown = pick::reset_passed_text(c);
    let limit = match (&c.binding, unknown) {
        (Some(binding), None) => format!("tightest: {}", pick::binding_text(binding, now)),
        (Some(binding), Some(unknown)) => format!(
            "tightest: {}; usage unknown: {unknown}",
            pick::binding_text(binding, now)
        ),
        (None, Some(unknown)) => format!("usage unknown: {unknown}"),
        (None, None) => return "no usage data".to_string(),
    };
    let also: String = c
        .default_model_windows
        .iter()
        .map(|w| format!("; also: {}", pick::used_text(w, now)))
        .collect();
    let age = match entry.usage.as_ref().map(|u| (u.source, u.age_text())) {
        Some((Source::Live, Some(age))) => format!("live, {} old", age.trim_end_matches(" ago")),
        Some((Source::Live, None)) => "live".to_string(),
        Some((Source::Cached, Some(age))) => format!("{} old", age.trim_end_matches(" ago")),
        _ => "of unknown age".to_string(),
    };
    let stale = if entry.stale(config) { " (stale)" } else { "" };
    format!("{limit}{also}; data {age}{stale}")
}

/// The state Jev reads (R23): the local time, the rules already applied, each account with an
/// option (aliased) and its usage, the models, the user's notes (qualified account names
/// aliased, otherwise as written), and the task slot. No email, organization, plan, path,
/// working directory or session content: nothing of an entry's local notes is used.
pub fn state_text(
    entries: &[Entry],
    candidates: &[Candidate],
    offered: &[(String, usize)],
    config: &Config,
    aliases: &Aliases,
    now: Timestamp,
    tz: &TimeZone,
) -> String {
    let mut out = String::new();
    out.push_str(
        "remuda pick: choose which coding-agent account and model to launch now, and at what \
         effort.\n",
    );
    out.push_str(&format!(
        "local time: {}\n",
        now.to_zoned(tz.clone()).strftime("%a %H:%M")
    ));
    out.push_str(&format!(
        "rules already enforced (every option satisfies them): at least {}% left on each \
         window that applies and whose usage is known (one that reset since it was cached is \
         unknown: not checked); excluded accounts are not listed.\n",
        config.min_headroom
    ));
    let mut listed: Vec<usize> = Vec::new();
    for (_, c) in offered {
        let e = candidates[*c].entry;
        if !listed.contains(&e) {
            listed.push(e);
        }
    }
    listed.sort();
    for e in &listed {
        let entry = &entries[*e];
        let provider = entry.account.provider;
        out.push_str(&format!(
            "\naccount {} ({provider})\n",
            aliases.qualified(&entry.account.qualified())
        ));
        let data = match entry.usage.as_ref().map(|u| (u.source, u.age_text())) {
            None => "unknown (no usage data)".to_string(),
            Some((Source::Live, Some(age))) => format!("live, {age}"),
            Some((Source::Live, None)) => "live".to_string(),
            Some((Source::Cached, age)) => {
                let when = age.unwrap_or_else(|| "time unknown".to_string());
                let fresh = if entry.stale(config) {
                    "stale: may be higher now"
                } else {
                    "fresh"
                };
                format!("cached {when} ({fresh})")
            }
        };
        out.push_str(&format!("  usage: {data}\n"));
        let models = &config.choices(provider).models;
        for w in entry.usage.iter().flat_map(|u| &u.windows) {
            // Without models, the agent's default model is unknown: a per-model window is shown
            // as one that may apply.
            let (applies, maybe) = match &w.model {
                None => (true, ""),
                Some(_) if models.is_empty() => (
                    true,
                    " (per-model; applies only if the agent's default model is in this family)",
                ),
                Some(name) => (
                    models.iter().any(|m| pick::limits_model(provider, m, name)),
                    "",
                ),
            };
            if !applies {
                continue;
            }
            let state = match w.used() {
                Some(used) => format!(
                    "{} used{}",
                    usage::format_percent(used),
                    w.resets_at()
                        .map(|at| format!(", resets in {}", pick::format_in(at, now)))
                        .unwrap_or_default()
                ),
                None => format!("usage unknown ({})", w.reset_since()),
            };
            out.push_str(&format!("  {}: {state}{maybe}\n", w.label));
        }
        if entry.usage.is_some() && entry.per_model_unknown() {
            out.push_str("  per-model limits: unknown (codex reports them only live)\n");
        }
    }
    let mut providers: Vec<Provider> = listed
        .iter()
        .map(|e| entries[*e].account.provider)
        .collect();
    providers.dedup();
    let models: Vec<String> = providers
        .iter()
        .map(|p| {
            let models = &config.choices(*p).models;
            let list = if models.is_empty() {
                "agent default".to_string()
            } else {
                models
                    .iter()
                    .enumerate()
                    .map(|(i, m)| {
                        if i == 0 {
                            format!("{m} (default)")
                        } else {
                            m.clone()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            format!("{p}: {list}")
        })
        .collect();
    out.push_str(&format!("\nmodels: {}\n", models.join("; ")));
    out.push_str(&format!(
        "\nuser notes (written by the user; follow them when they apply):\n{}\n",
        alias_notes(&config.notes, aliases)
    ));
    out.push_str("\ntask: none given\n");
    out
}

/// `notes` with each qualified account name (`claude:max`) replaced by its alias, whatever is
/// written around it and whether or not the account is registered
/// ([`privacy::alias_qualified`]); bare names stay as written (R23: `max` may be an effort as
/// well as an account).
pub fn alias_notes(notes: &str, aliases: &Aliases) -> String {
    privacy::alias_qualified(notes, aliases)
}

/// Why Jev is not asked, if it is not (R23): `--offline`, no key, no notes, or nothing to
/// choose (one option and no effort to score).
pub fn skip_reason(
    offline: bool,
    key: Option<&str>,
    config: &Config,
    request: &Request,
) -> Option<Reason> {
    if offline {
        Some(Reason::Offline)
    } else if key.is_none_or(str::is_empty) {
        Some(Reason::NoKey)
    } else if config.notes.is_empty() {
        Some(Reason::NoNotes)
    } else if request.offered.len() < 2 && request.efforts.is_empty() {
        Some(Reason::SingleOption)
    } else {
        None
    }
}

/// Sends `request` with `curl` and reads the answers; every failure is [`Asked::Failed`], its
/// text free of the key.
pub fn ask(curl: Option<&Path>, key: &str, request: &Request) -> Asked {
    let Some(curl) = curl else {
        return Asked::Failed("`curl` not found on PATH".to_string());
    };
    let result = send(curl, key, &request.body.to_string(), TIMEOUT)
        .and_then(|text| parse_response(&text, request));
    match result {
        Ok(mut answers) => {
            for effort in answers.efforts.values_mut() {
                if let Err(e) = effort {
                    *e = scrub(e, key);
                }
            }
            Asked::Answered(answers)
        }
        Err(e) => Asked::Failed(scrub(&e, key)),
    }
}

/// `curl`'s arguments: no `~/.curlrc` (`-q`, first), HTTPS only, the configuration (the key's
/// header and the body) from stdin, the status code on the last line of the output.
pub fn curl_args(timeout: Duration) -> Vec<String> {
    [
        "-q",
        "-sS",
        "--proto",
        "=https",
        "--max-time",
        &timeout.as_secs().to_string(),
        "-X",
        "POST",
        "-H",
        "Content-Type: application/json",
        "-o",
        "-",
        "-w",
        "\n%{http_code}",
        "-K",
        "-",
        ENDPOINT,
    ]
    .map(str::to_string)
    .to_vec()
}

/// The configuration `curl -K -` reads: the authorization header and the body, each a quoted
/// value (`\` and `"` escaped). A key that cannot be quoted safely is refused.
pub fn curl_config(key: &str, body: &str) -> Result<String, String> {
    if key.chars().any(|c| c == '"' || c == '\\' || c.is_control()) {
        return Err(format!(
            "{KEY_VAR} contains a quote, a backslash or a control character; not sent"
        ));
    }
    let quoted = body.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!(
        "header = \"Authorization: Bearer {key}\"\ndata-binary = \"{quoted}\"\n"
    ))
}

/// POSTs `body` to [`ENDPOINT`] with `curl`; the response body of a 2xx answer.
pub fn send(curl: &Path, key: &str, body: &str, timeout: Duration) -> Result<String, String> {
    let config = curl_config(key, body)?;
    let args = curl_args(timeout);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    // curl stops itself at `timeout`; the extra seconds only catch a curl that does not.
    let outcome = probe::run_with_stdin(
        curl,
        &args,
        config.as_bytes(),
        timeout + Duration::from_secs(5),
    );
    let result = match outcome {
        Outcome::Exited {
            code: Some(0),
            stdout,
            ..
        } => {
            let (response, status) = stdout.rsplit_once('\n').unwrap_or(("", stdout.as_str()));
            match status.trim().parse::<u16>() {
                Ok(200..=299) => Ok(response.to_string()),
                Ok(status) => Err(format!("HTTP {status}: {}", snippet(response))),
                Err(_) => Err(format!("unexpected curl output: {}", snippet(&stdout))),
            }
        }
        Outcome::Exited { code: Some(28), .. } => {
            Err(format!("timed out after {}s", timeout.as_secs()))
        }
        other => Err(format!("curl {}", other.describe(timeout))),
    };
    result.map_err(|e| scrub(&e, key))
}

/// Jev's answers to `request` (R23). Anything unexpected in `launch` fails the whole response:
/// not JSON, a missing answer, a choice or probability that names no offered option. Without a
/// `launch` question, the one option is the launch. An effort answer that is missing or out of
/// range is kept as that effort's error only.
pub fn parse_response(text: &str, request: &Request) -> Result<Answers, String> {
    let bad = |what: &str| format!("{what} in the response: {}", snippet(text));
    let v: Value = serde_json::from_str(text).map_err(|_| bad("not JSON"))?;
    let answers = v
        .get("answers")
        .and_then(Value::as_object)
        .ok_or_else(|| bad("no `answers`"))?;
    let mut probabilities = std::collections::BTreeMap::new();
    let launch = if request.launch {
        let launch = answers
            .get("launch")
            .ok_or_else(|| bad("no `launch` answer"))?;
        let offered = |label: &str| {
            request
                .offered
                .iter()
                .find(|(l, _)| l == label)
                .map(|(_, c)| *c)
        };
        let choice = launch
            .get("choice")
            .and_then(Value::as_str)
            .ok_or_else(|| bad("no `launch.choice`"))?;
        let choice = offered(choice)
            .ok_or_else(|| format!("Jev chose {choice:?}, which is not an offered option"))?;
        let confidence =
            unit(launch.get("confidence")).ok_or_else(|| bad("no `launch.confidence`"))?;
        let Some(probs) = launch.get("probabilities").and_then(Value::as_object) else {
            return Err(bad("no `launch.probabilities`"));
        };
        for (label, p) in probs {
            let c = offered(label).ok_or_else(|| {
                format!("Jev gave a probability for {label:?}, which is not offered")
            })?;
            let p = unit(Some(p)).ok_or_else(|| bad("a probability that is not from 0 to 1"))?;
            probabilities.insert(c, p);
        }
        Some((choice, confidence))
    } else {
        None
    };
    let efforts = request
        .efforts
        .iter()
        .map(|(provider, levels)| (*provider, effort_answer(answers, *provider, levels.len())))
        .collect();
    Ok(Answers {
        model: v.get("model").and_then(Value::as_str).map(str::to_string),
        launch,
        probabilities,
        efforts,
    })
}

/// The `effort_<provider>` answer over `levels` levels: its score and confidence, or why it is
/// not usable.
fn effort_answer(
    answers: &Map<String, Value>,
    provider: Provider,
    levels: usize,
) -> Result<(f64, f64), String> {
    let id = format!("effort_{provider}");
    let answer = answers.get(&id).ok_or(format!("no `{id}` answer"))?;
    let last = levels.saturating_sub(1);
    let score = answer
        .get("score")
        .and_then(Value::as_f64)
        .filter(|s| (0.0..=last as f64).contains(s))
        .ok_or(format!("`{id}.score` is not a number from 0 to {last}"))?;
    let confidence = unit(answer.get("confidence"))
        .ok_or(format!("`{id}.confidence` is not a number from 0 to 1"))?;
    Ok((score, confidence))
}

/// A number from 0 to 1.
fn unit(v: Option<&Value>) -> Option<f64> {
    v?.as_f64().filter(|x| (0.0..=1.0).contains(x))
}

/// `text` on one line, at most [`SNIPPET`] characters.
fn snippet(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(SNIPPET) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line,
    }
}

/// `text` with the key masked, should anything have echoed it.
fn scrub(text: &str, key: &str) -> String {
    if key.is_empty() {
        text.to_string()
    } else {
        text.replace(key, "•••")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pick::Choices;
    use crate::registry::{Account, Home};
    use crate::usage::{CachedUsage, Reading, Resets, Snapshot, UsageRow};

    const NOW: &str = "2026-09-26T18:05:00Z";

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn row(label: &str, percent: f64, resets: &str) -> UsageRow {
        UsageRow {
            label: label.into(),
            percent,
            severity: None,
            resets: Some(Resets::At(ts(resets))),
        }
    }

    /// `rows`, cached `age` before [`NOW`] and read then.
    fn cached(age: jiff::SignedDuration, rows: Vec<UsageRow>) -> Reading {
        let now = ts(NOW);
        let cached = CachedUsage {
            fetched_at: Some(now - age),
            rows,
        };
        Snapshot::cached(&cached).at(now)
    }

    fn fixture() -> (Vec<Entry>, Config, Aliases) {
        let account = |provider, name: &str| Account {
            provider,
            name: name.into(),
            home: Home::Path(format!("/Users/you/.{name}")),
        };
        let entries = vec![
            Entry {
                account: account(Provider::Claude, "max"),
                blocked: None,
                usage: Some(cached(
                    jiff::SignedDuration::from_mins(25),
                    vec![
                        row("Session", 34.0, "2026-09-26T19:25:00Z"),
                        row("Week (all models)", 77.0, "2026-09-28T21:05:00Z"),
                        row("Week (Fable)", 100.0, "2026-09-28T21:05:00Z"),
                    ],
                )),
                notes: vec!["no /Users/you/.max/.claude.json".into()],
            },
            Entry {
                account: account(Provider::Codex, "work"),
                blocked: None,
                usage: Some(cached(
                    jiff::SignedDuration::from_hours(5),
                    vec![row("Week (all models)", 40.0, "2026-09-30T18:05:00Z")],
                )),
                notes: Vec::new(),
            },
        ];
        let strings = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let config = Config {
            notes: "Keep claude:max for long refactors; max effort only for claude:maxi.\n\
                    codex:work is the company's."
                .into(),
            claude: Choices {
                models: strings(&["claude-opus-5-5", "claude-fable-5-1"]),
                efforts: strings(&["medium", "high", "xhigh", "max"]),
                default_effort: Some("high".into()),
            },
            codex: Choices {
                models: strings(&["gpt-6-astra"]),
                ..Choices::default()
            },
            ..Config::default()
        };
        let mut aliases = Aliases::default();
        for q in ["claude:default", "claude:max", "codex:work"] {
            aliases.note(q);
        }
        (entries, config, aliases)
    }

    /// R23: the state as Jev reads it, golden.
    #[test]
    fn state_is_plain_text_with_aliases() {
        let (entries, config, aliases) = fixture();
        let now = ts(NOW);
        let c = pick::candidates(&entries, &config, now);
        let r = request(&entries, &c, &config, &aliases, now, &TimeZone::UTC);
        assert_eq!(
            r.body["state"].as_str().unwrap(),
            "remuda pick: choose which coding-agent account and model to launch now, and at \
             what effort.\n\
             local time: Sat 18:05\n\
             rules already enforced (every option satisfies them): at least 10% left on each \
             window that applies and whose usage is known (one that reset since it was cached \
             is unknown: not checked); excluded accounts are not listed.\n\
             \n\
             account claude:account-1 (claude)\n\
             \x20 usage: cached 25m ago (fresh)\n\
             \x20 Session: 34% used, resets in 1h20m\n\
             \x20 Week (all models): 77% used, resets in 2d3h\n\
             \x20 Week (Fable): 100% used, resets in 2d3h\n\
             \n\
             account codex:account-1 (codex)\n\
             \x20 usage: cached 5h ago (stale: may be higher now)\n\
             \x20 Week (all models): 40% used, resets in 4d\n\
             \x20 per-model limits: unknown (codex reports them only live)\n\
             \n\
             models: claude: claude-opus-5-5 (default), claude-fable-5-1; codex: gpt-6-astra \
             (default)\n\
             \n\
             user notes (written by the user; follow them when they apply):\n\
             Keep claude:account-1 for long refactors; max effort only for claude:account-2.\n\
             codex:account-1 is the company's.\n\
             \n\
             task: none given\n"
        );
        let offered: Vec<&str> = r.offered.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            offered,
            // Rules order: 60% left before 23%.
            [
                "codex:account-1 / gpt-6-astra",
                "claude:account-1 / claude-opus-5-5"
            ]
        );
        assert_eq!(
            r.body["questions"]["launch"]["criteria"]["codex:account-1 / gpt-6-astra"],
            "tightest: Week (all models) 60% left, resets in 4d; data 5h old (stale)"
        );
        assert_eq!(
            r.body["questions"]["effort_claude"]["criteria"][3],
            "max: the most reasoning, for the hardest problems; uses the limits fastest"
        );
        assert!(r.body["questions"].get("effort_codex").is_none());
        assert_eq!(
            r.efforts,
            [(Provider::Claude, config.claude.efforts.clone())]
        );
        let body = r.body.to_string();
        assert!(!body.contains("/Users/you"), "{body}");
        assert!(
            !body.contains("claude:max ") && !body.contains("codex:work"),
            "{body}"
        );
    }

    /// R23: without `models`, the per-model windows go out as ones that may apply to the agent's
    /// default model, in the state and in the option's description.
    #[test]
    fn per_model_windows_without_models_are_described() {
        let (entries, mut config, aliases) = fixture();
        config.claude.models.clear();
        let now = ts(NOW);
        let c = pick::candidates(&entries, &config, now);
        let r = request(&entries, &c, &config, &aliases, now, &TimeZone::UTC);
        let state = r.body["state"].as_str().unwrap();
        assert!(
            state.contains(
                "  Week (Fable): 100% used, resets in 2d3h (per-model; applies only if the \
                 agent's default model is in this family)\n"
            ),
            "{state}"
        );
        assert_eq!(
            r.body["questions"]["launch"]["criteria"]["claude:account-1 / default"],
            "tightest: Week (all models) 23% left, resets in 2d3h; also: Week (Fable) 100% \
             used, resets in 2d3h; data 25m old"
        );
    }

    /// R23: the key and body survive curl's config quoting; a key that cannot be quoted is
    /// refused.
    /// R23: a window that reset since it was cached goes out as unknown, never as 0% used; in
    /// an option's description it is named after the tightest known window, or alone.
    #[test]
    fn windows_past_their_reset_are_described_as_unknown() {
        let (mut entries, mut config, aliases) = fixture();
        let now = ts(NOW);
        // max: the session reset an hour ago. work: its only window reset yesterday.
        entries[0].usage = Some(cached(
            jiff::SignedDuration::from_hours(3),
            vec![
                row("Session", 100.0, "2026-09-26T17:05:00Z"),
                row("Week (all models)", 77.0, "2026-09-28T21:05:00Z"),
                row("Week (Fable)", 100.0, "2026-09-26T17:05:00Z"),
            ],
        ));
        entries[1].usage = Some(cached(
            jiff::SignedDuration::from_hours(30),
            vec![row("Week (all models)", 100.0, "2026-09-25T18:05:00Z")],
        ));
        let c = pick::candidates(&entries, &config, now);
        let r = request(&entries, &c, &config, &aliases, now, &TimeZone::UTC);
        let state = r.body["state"].as_str().unwrap();
        for line in [
            "  usage: cached 3h ago (stale: may be higher now)\n",
            "  Session: usage unknown (reset since cached)\n",
            "  Week (all models): 77% used, resets in 2d3h\n",
            "  Week (Fable): usage unknown (reset since cached)\n",
            "  usage: cached 1d ago (stale: may be higher now)\n\
             \x20 Week (all models): usage unknown (reset since cached)\n",
        ] {
            assert!(state.contains(line), "{line:?} in:\n{state}");
        }
        assert!(!state.contains(": 0% used"), "{state}");
        let criteria = &r.body["questions"]["launch"]["criteria"];
        assert_eq!(
            criteria["claude:account-1 / claude-opus-5-5"],
            "tightest: Week (all models) 23% left, resets in 2d3h; usage unknown: Session: \
             reset since cached; data 3h old (stale)"
        );
        // Fable's own week reset too: both of its unknown windows are named.
        assert_eq!(
            criteria["claude:account-1 / claude-fable-5-1"],
            "tightest: Week (all models) 23% left, resets in 2d3h; usage unknown: Session, \
             Week (Fable): reset since cached; data 3h old (stale)"
        );
        assert_eq!(
            criteria["codex:account-1 / gpt-6-astra"],
            "usage unknown: Week (all models): reset since cached; data 1d old (stale)"
        );
        // Known headroom first: the pair of unknown headroom is offered last.
        let offered: Vec<&str> = r.offered.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(offered.last(), Some(&"codex:account-1 / gpt-6-astra"));

        // Without models, a per-model window past its reset is still one that may apply.
        config.claude.models.clear();
        let c = pick::candidates(&entries, &config, now);
        let r = request(&entries, &c, &config, &aliases, now, &TimeZone::UTC);
        let state = r.body["state"].as_str().unwrap();
        assert!(
            state.contains(
                "  Week (Fable): usage unknown (reset since cached) (per-model; applies only \
                 if the agent's default model is in this family)\n"
            ),
            "{state}"
        );
        assert_eq!(
            r.body["questions"]["launch"]["criteria"]["claude:account-1 / default"],
            "tightest: Week (all models) 23% left, resets in 2d3h; usage unknown: Session: \
             reset since cached; also: Week (Fable) usage unknown (reset since cached); data \
             3h old (stale)"
        );
    }

    #[test]
    fn curl_config_quotes_the_body() {
        let body = r#"{"state":"a \"q\"\nb\\c é"}"#;
        let config = curl_config("test-key-DO-NOT-LEAK", body).unwrap();
        let (header, data) = config.split_once('\n').unwrap();
        assert_eq!(
            header,
            "header = \"Authorization: Bearer test-key-DO-NOT-LEAK\""
        );
        let quoted = data
            .strip_prefix("data-binary = \"")
            .and_then(|d| d.strip_suffix("\"\n"))
            .unwrap();
        // curl's unquoting: `\\` → `\`, `\"` → `"`.
        let mut unquoted = String::new();
        let mut chars = quoted.chars();
        while let Some(c) = chars.next() {
            unquoted.push(if c == '\\' { chars.next().unwrap() } else { c });
        }
        assert_eq!(unquoted, body);
        for bad in ["a\"b", "a\\b", "a\nb"] {
            assert!(curl_config(bad, body).unwrap_err().contains(KEY_VAR));
        }
        assert_eq!(curl_args(TIMEOUT)[0], "-q");
        assert!(!curl_args(TIMEOUT).iter().any(|a| a.contains("Bearer")));
    }

    fn offered_request() -> Request {
        Request {
            body: Value::Null,
            offered: vec![
                ("claude:account-1 / m".into(), 0),
                ("codex:account-1 / g".into(), 2),
            ],
            launch: true,
            efforts: vec![(Provider::Claude, vec!["medium".into(), "high".into()])],
        }
    }

    #[test]
    fn parses_a_response() {
        let text = r#"{"model": "jev-1.13.0", "answers": {
            "launch": {"type": "choice", "choice": "codex:account-1 / g", "confidence": 0.9,
                       "probabilities": {"claude:account-1 / m": 0.05, "codex:account-1 / g": 0.95}},
            "effort_claude": {"type": "score", "score": 1.0, "confidence": 0.8,
                              "probabilities": {"0": 0.1, "1": 0.9}}},
            "usage": {"input_tokens": 1, "output_tokens": 1}}"#;
        let a = parse_response(text, &offered_request()).unwrap();
        assert_eq!(a.model.as_deref(), Some("jev-1.13.0"));
        assert_eq!(a.launch, Some((2, 0.9)));
        assert_eq!(a.probabilities[&0], 0.05);
        assert_eq!(a.efforts[&Provider::Claude], Ok((1.0, 0.8)));
    }

    /// R23: an unusable effort answer is that effort's error only; the launch answer stands.
    #[test]
    fn a_bad_effort_answer_costs_only_the_effort() {
        let r = offered_request();
        let with_effort = |effort: &str| {
            format!(
                r#"{{"answers": {{"launch": {{"choice": "codex:account-1 / g", "confidence": 0.9,
                   "probabilities": {{"codex:account-1 / g": 1.0}}}}{effort}}}}}"#
            )
        };
        for (effort, want) in [
            ("", "no `effort_claude` answer"),
            (
                r#", "effort_claude": {"score": "high", "confidence": 0.9}"#,
                "`effort_claude.score` is not a number from 0 to 1",
            ),
            (
                r#", "effort_claude": {"score": 1.6, "confidence": 0.9}"#,
                "`effort_claude.score` is not a number from 0 to 1",
            ),
            (
                r#", "effort_claude": {"score": -0.2, "confidence": 0.9}"#,
                "`effort_claude.score` is not a number from 0 to 1",
            ),
            (
                r#", "effort_claude": {"score": 1, "confidence": 2}"#,
                "`effort_claude.confidence` is not a number from 0 to 1",
            ),
        ] {
            let a = parse_response(&with_effort(effort), &r).unwrap();
            assert_eq!(a.launch, Some((2, 0.9)), "{effort}");
            assert_eq!(
                a.efforts[&Provider::Claude],
                Err(want.to_string()),
                "{effort}"
            );
        }
    }

    /// R23: with one option, `launch` is not asked, and its answer is not expected.
    #[test]
    fn one_option_asks_only_the_effort() {
        let (entries, mut config, aliases) = fixture();
        config.exclude = vec!["codex:work".into()];
        config.claude.models.truncate(1);
        let entries: Vec<Entry> = entries.into_iter().take(1).collect();
        let now = ts(NOW);
        let c = pick::candidates(&entries, &config, now);
        let r = request(&entries, &c, &config, &aliases, now, &TimeZone::UTC);
        assert_eq!(r.offered.len(), 1);
        assert!(!r.launch);
        assert!(r.body["questions"].get("launch").is_none());
        assert_eq!(r.body["questions"]["effort_claude"]["type"], "score");
        assert_eq!(skip_reason(false, Some("k"), &config, &r), None);
        let a = parse_response(
            r#"{"answers": {"effort_claude": {"score": 3, "confidence": 0.9}}}"#,
            &r,
        )
        .unwrap();
        assert_eq!(a.launch, None);
        let d = pick::decide(&c, &entries, &config, Asked::Answered(a)).unwrap();
        assert_eq!(
            (d.decided_by, d.reason, d.effort.as_deref(), d.effort_by_jev),
            (
                pick::DecidedBy::Rules,
                Some(Reason::SingleOption),
                Some("max"),
                true
            )
        );
    }

    #[test]
    fn rejects_unexpected_responses() {
        let r = offered_request();
        let launch = |choice: &str, extra: &str| {
            format!(
                r#"{{"answers": {{"launch": {{"choice": "{choice}", "confidence": 0.9,
                   "probabilities": {{"{choice}": 1.0{extra}}}}},
                   "effort_claude": {{"score": 0, "confidence": 1}}}}}}"#
            )
        };
        for (text, want) in [
            ("not json".to_string(), "not JSON"),
            ("{}".to_string(), "no `answers`"),
            (launch("claude:max / m", ""), "not an offered option"),
            (
                launch("claude:account-1 / m", r#", "x": 0.0"#),
                "\"x\", which is not offered",
            ),
            (
                r#"{"answers": {"effort_claude": {"score": 0, "confidence": 1}}}"#.to_string(),
                "no `launch` answer",
            ),
        ] {
            let err = parse_response(&text, &r).unwrap_err();
            assert!(err.contains(want), "{text}\n=> {err}");
        }
        assert!(snippet(&"x".repeat(500)).chars().count() == SNIPPET + 1);
    }

    /// R23: aliasing in the notes touches qualified names only, and every one of them: a name
    /// that is not registered (`claude:maxi`) gets an alias of its own.
    #[test]
    fn notes_alias_only_qualified_names() {
        let (_, _, aliases) = fixture();
        assert_eq!(
            alias_notes(
                "claude:max, max, claude:maxi, (codex:work) claude:default",
                &aliases
            ),
            "claude:account-1, max, claude:account-2, (codex:account-1) claude:default"
        );
        assert_eq!(
            alias_notes(
                "把claude:max留给大重构；-codex:work、_claude:gone_ 不用",
                &aliases
            ),
            "把claude:account-1留给大重构；-codex:account-1、_claude:account-2 不用"
        );
    }

    /// R23: at most 255 options, the rules' best first.
    #[test]
    fn the_choice_offers_at_most_255_options() {
        let (entries, mut config, aliases) = fixture();
        config.claude.models = (0..300).map(|i| format!("claude-opus-{i}")).collect();
        let now = ts(NOW);
        let c = pick::candidates(&entries, &config, now);
        assert_eq!(pick::ranked(&c).len(), 301);
        let r = request(&entries, &c, &config, &aliases, now, &TimeZone::UTC);
        assert_eq!(r.offered.len(), pick::MAX_OPTIONS);
        assert_eq!(
            r.body["questions"]["launch"]["criteria"]
                .as_object()
                .unwrap()
                .len(),
            255
        );
        let ranks: Vec<Option<usize>> = r.offered.iter().map(|(_, i)| c[*i].rules_rank).collect();
        assert_eq!(ranks, (1..=255).map(Some).collect::<Vec<_>>());
    }

    #[test]
    fn skip_reasons() {
        let (entries, mut config, aliases) = fixture();
        let now = ts(NOW);
        let c = pick::candidates(&entries, &config, now);
        let r = request(&entries, &c, &config, &aliases, now, &TimeZone::UTC);
        assert_eq!(
            skip_reason(true, Some("k"), &config, &r),
            Some(Reason::Offline)
        );
        assert_eq!(skip_reason(false, None, &config, &r), Some(Reason::NoKey));
        assert_eq!(
            skip_reason(false, Some(""), &config, &r),
            Some(Reason::NoKey)
        );
        assert_eq!(skip_reason(false, Some("k"), &config, &r), None);
        config.notes.clear();
        assert_eq!(
            skip_reason(false, Some("k"), &config, &r),
            Some(Reason::NoNotes)
        );
        config.notes = "x".into();
        let one = Request {
            offered: r.offered[..1].to_vec(),
            efforts: Vec::new(),
            ..r
        };
        assert_eq!(
            skip_reason(false, Some("k"), &config, &one),
            Some(Reason::SingleOption)
        );
    }

    /// R23: curl gets the key on stdin only; its output's last line is the status.
    #[test]
    fn send_reads_the_status_and_keeps_the_key_out_of_errors() {
        let dir = tempfile::tempdir().unwrap();
        let script_ok = probe::script(dir.path(), "ok", "cat >/dev/null; printf '{\"a\":1}\\n200'");
        assert_eq!(send(&script_ok, "k", "{}", TIMEOUT).unwrap(), "{\"a\":1}");
        let script_401 = probe::script(
            dir.path(),
            "e401",
            "cat >/dev/null; printf 'bad key test-key-DO-NOT-LEAK\\n401'",
        );
        let err = send(&script_401, "test-key-DO-NOT-LEAK", "{}", TIMEOUT).unwrap_err();
        assert_eq!(err, "HTTP 401: bad key •••");
        let script_28 = probe::script(dir.path(), "t", "cat >/dev/null; exit 28");
        assert_eq!(
            send(&script_28, "k", "{}", TIMEOUT).unwrap_err(),
            "timed out after 10s"
        );
        let script_7 = probe::script(
            dir.path(),
            "c",
            "cat >/dev/null; echo 'curl: (7) Failed to connect' >&2; exit 7",
        );
        assert_eq!(
            send(&script_7, "k", "{}", TIMEOUT).unwrap_err(),
            "curl exited with status 7: curl: (7) Failed to connect"
        );
    }
}
