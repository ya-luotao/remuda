//! Estimated cost at API list prices (SPEC R20): the built-in prices, `[prices."<model>"]`
//! of config.toml (R3), and the cost of one request. Prices are integers in picodollars per
//! token (10⁻¹² USD; 1 USD per million tokens is 10⁶), which keeps every built-in price, its
//! fast-mode, US-only and long-context multiples, and so every request's cost exact.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use toml_edit::DocumentMut;

use crate::provider::Provider;
use crate::stats::Tokens;

/// When the built-in prices were read from the providers' pricing pages.
pub const PRICES_AS_OF: &str = "2026-09-24";

/// The keys of a `[prices."<model>"]` table.
pub const OVERRIDE_KEYS: [&str; 5] = [
    "input",
    "output",
    "cache_read",
    "cache_write_5m",
    "cache_write_1h",
];

/// A codex request with more input tokens than this (cached included) is a long-context one.
pub const LONG_CONTEXT_TOKENS: u64 = 272_000;

/// The largest price `[prices]` accepts, USD per million tokens.
const MAX_PRICE: f64 = 1_000_000.0;

/// The price of each count, picodollars per token; `None`: that count is not priced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    pub input: u64,
    pub output: u64,
    /// Claude's cache read; codex's cached input.
    pub cache_read: Option<u64>,
    pub cache_write_5m: Option<u64>,
    pub cache_write_1h: Option<u64>,
    /// Fast mode doubles every price.
    pub fast_mode: bool,
    /// US-only inference multiplies every price by 1.1.
    pub us_premium: bool,
    /// A request with more than [`LONG_CONTEXT_TOKENS`] input tokens (cached included) is
    /// priced at twice the input and cache prices and 1.5 times the output price, whole.
    pub long_context: bool,
}

impl Rate {
    /// Picodollars for one request's `tokens`; `None` when a nonzero count has no price.
    /// Reasoning is part of output and not priced again. `fast` / `us` are the request's
    /// flags and count only where the rate has `fast_mode` / `us_premium`; the long-context
    /// price follows from the counts where the rate has `long_context`.
    pub fn cost(&self, tokens: &Tokens, fast: bool, us: bool) -> Option<u128> {
        let long = self.long_context
            && tokens.input.saturating_add(tokens.cache_read) > LONG_CONTEXT_TOKENS;
        // The multiplier of the input and cache prices, and of the output price, as
        // (numerator, denominator).
        let (inputs, output) = match long {
            true => ((2, 1), (3, 2)),
            false => ((1, 1), (1, 1)),
        };
        let scale = |price: u64, (num, den): (u128, u128)| {
            let mut p = u128::from(price);
            if fast && self.fast_mode {
                p *= 2;
            }
            if us && self.us_premium {
                p = p * 11 / 10;
            }
            p * num / den
        };
        let part = |n: u64, price: Option<u64>, factor| match n {
            0 => Some(0),
            n => price.map(|p| u128::from(n) * scale(p, factor)),
        };
        Some(
            part(tokens.input, Some(self.input), inputs)?
                + part(tokens.cache_read, self.cache_read, inputs)?
                + part(tokens.cache_write_5m, self.cache_write_5m, inputs)?
                + part(tokens.cache_write_1h, self.cache_write_1h, inputs)?
                + part(tokens.output, Some(self.output), output)?,
        )
    }
}

/// A `[prices."<model>"]` table: picodollars per token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Override {
    pub input: u64,
    pub output: u64,
    pub cache_read: Option<u64>,
    pub cache_write_5m: Option<u64>,
    pub cache_write_1h: Option<u64>,
}

/// The prices in effect: the built-in ones and config.toml's overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prices {
    /// By model id as written in config.toml.
    pub overrides: BTreeMap<String, Override>,
}

impl Prices {
    /// `[prices."<model>"]` of `doc` (R3); without `prices`, none. Errors name the table.
    pub fn from_document(doc: &DocumentMut) -> Result<Prices> {
        let mut prices = Prices::default();
        let Some(item) = doc.get("prices") else {
            return Ok(prices);
        };
        let Some(tables) = item.as_table_like() else {
            bail!("`prices` must be a table ([prices.\"<model>\"])");
        };
        for (model, item) in tables.iter() {
            if model.is_empty() {
                bail!("[prices]: empty model id");
            }
            let Some(table) = item.as_table_like() else {
                bail!("[prices.{model:?}] must be a table");
            };
            let mut found: [Option<u64>; 5] = [None; 5];
            for (key, value) in table.iter() {
                let Some(i) = OVERRIDE_KEYS.iter().position(|k| *k == key) else {
                    bail!(
                        "[prices.{model:?}]: unknown key `{key}` (known: {})",
                        OVERRIDE_KEYS.join(", ")
                    );
                };
                let Some(x) = value
                    .as_float()
                    .or_else(|| value.as_integer().map(|n| n as f64))
                else {
                    bail!("[prices.{model:?}]: `{key}` must be a number (USD per million tokens)");
                };
                if !(0.0..=MAX_PRICE).contains(&x) {
                    bail!(
                        "[prices.{model:?}]: `{key}` must be from 0 to 1000000 \
                         (USD per million tokens), got {x}"
                    );
                }
                found[i] = Some((x * 1e6).round() as u64);
            }
            let [input, output, cache_read, cache_write_5m, cache_write_1h] = found;
            let (Some(input), Some(output)) = (input, output) else {
                let missing = if input.is_none() { "input" } else { "output" };
                bail!("[prices.{model:?}]: missing `{missing}`");
            };
            prices.overrides.insert(
                model.to_string(),
                Override {
                    input,
                    output,
                    cache_read,
                    cache_write_5m,
                    cache_write_1h,
                },
            );
        }
        Ok(prices)
    }

    /// The rate of `model` for a request of `provider`: an override for the id as recorded,
    /// else for the id without its date, else the built-in price of `provider` for the id
    /// without its date. An override keeps the built-in model's `fast_mode`, `us_premium` and
    /// `long_context`.
    pub fn rate(&self, provider: Provider, model: &str) -> Option<Rate> {
        let bare = undated(model);
        let built_in = built_in(provider, bare);
        let Some(o) = self
            .overrides
            .get(model)
            .or_else(|| self.overrides.get(bare))
        else {
            return built_in;
        };
        Some(Rate {
            input: o.input,
            output: o.output,
            cache_read: o.cache_read,
            cache_write_5m: o.cache_write_5m,
            cache_write_1h: o.cache_write_1h,
            fast_mode: built_in.is_some_and(|r| r.fast_mode),
            us_premium: built_in.is_some_and(|r| r.us_premium),
            long_context: built_in.is_some_and(|r| r.long_context),
        })
    }
}

/// `model` without a trailing `-` + 8 ASCII digits (`claude-haiku-4-5-20251001` →
/// `claude-haiku-4-5`); otherwise unchanged.
pub fn undated(model: &str) -> &str {
    let bytes = model.as_bytes();
    let n = bytes.len();
    if n > 9 && bytes[n - 9] == b'-' && bytes[n - 8..].iter().all(u8::is_ascii_digit) {
        &model[..n - 9]
    } else {
        model
    }
}

/// Picodollars per token of a price in thousandths of a USD per million tokens.
const PICO_PER_MILLI: u64 = 1_000;

/// Claude: id, [input, 5-minute cache write, 1-hour cache write, cache read, output] in
/// thousandths of a USD per million tokens, fast mode, US-only premium (R20).
const CLAUDE: &[(&str, [u64; 5], bool, bool)] = &[
    (
        "claude-fable-5-1",
        [10_000, 12_500, 20_000, 250, 50_000],
        false,
        true,
    ),
    (
        "claude-mythos-5-1",
        [10_000, 12_500, 20_000, 250, 50_000],
        false,
        true,
    ),
    (
        "claude-fable-5",
        [10_000, 12_500, 20_000, 1_000, 50_000],
        false,
        true,
    ),
    (
        "claude-mythos-5",
        [10_000, 12_500, 20_000, 1_000, 50_000],
        false,
        true,
    ),
    (
        "claude-opus-5-5",
        [4_000, 5_000, 8_000, 200, 20_000],
        true,
        true,
    ),
    (
        "claude-opus-5",
        [5_000, 6_250, 10_000, 500, 25_000],
        true,
        true,
    ),
    (
        "claude-opus-4-8",
        [5_000, 6_250, 10_000, 500, 25_000],
        true,
        true,
    ),
    (
        "claude-opus-4-7",
        [5_000, 6_250, 10_000, 500, 25_000],
        false,
        true,
    ),
    (
        "claude-opus-4-6",
        [5_000, 6_250, 10_000, 500, 25_000],
        false,
        true,
    ),
    (
        "claude-opus-4-5",
        [5_000, 6_250, 10_000, 500, 25_000],
        false,
        false,
    ),
    (
        "claude-opus-4-1",
        [15_000, 18_750, 30_000, 1_500, 75_000],
        false,
        false,
    ),
    (
        "claude-opus-4",
        [15_000, 18_750, 30_000, 1_500, 75_000],
        false,
        false,
    ),
    (
        "claude-sonnet-5",
        [2_000, 2_500, 4_000, 200, 10_000],
        false,
        true,
    ),
    (
        "claude-sonnet-4-6",
        [3_000, 3_750, 6_000, 300, 15_000],
        false,
        true,
    ),
    (
        "claude-sonnet-4-5",
        [3_000, 3_750, 6_000, 300, 15_000],
        false,
        false,
    ),
    (
        "claude-sonnet-4",
        [3_000, 3_750, 6_000, 300, 15_000],
        false,
        false,
    ),
    (
        "claude-haiku-4-5",
        [1_000, 1_250, 2_000, 100, 5_000],
        false,
        false,
    ),
    (
        "claude-3-5-haiku",
        [800, 1_000, 1_600, 80, 4_000],
        false,
        false,
    ),
];

/// Codex: id, [input, cached input, output] in thousandths of a USD per million tokens, and
/// whether the long-context price applies (R20). A codex model not listed is not priced.
const CODEX: &[(&str, [u64; 3], bool)] = &[
    ("gpt-5.6-sol", [4_000, 400, 20_000], true),
    ("gpt-6-astra", [10_000, 1_000, 50_000], true),
    ("gpt-6-sol", [2_000, 200, 10_000], true),
    ("gpt-5.6-terra", [2_000, 200, 12_000], true),
    ("gpt-5.5", [5_000, 500, 30_000], true),
    ("gpt-5.4", [2_500, 250, 15_000], true),
    ("gpt-5.3-codex", [1_750, 175, 14_000], false),
    ("gpt-5.2-codex", [1_750, 175, 14_000], false),
    ("gpt-5.2", [1_750, 175, 14_000], false),
    ("gpt-5.1-codex-max", [1_250, 125, 10_000], false),
    ("gpt-5.1-codex", [1_250, 125, 10_000], false),
    ("gpt-5.1-codex-mini", [250, 25, 2_000], false),
    ("gpt-5-codex", [1_250, 125, 10_000], false),
    ("gpt-5", [1_250, 125, 10_000], false),
    ("o4-mini", [1_100, 275, 4_400], false),
];

/// The built-in rate of `provider`'s model `id` (without its date), found exactly.
fn built_in(provider: Provider, id: &str) -> Option<Rate> {
    let pico = |milli: u64| milli * PICO_PER_MILLI;
    match provider {
        Provider::Claude => CLAUDE.iter().find(|row| row.0 == id).map(
            |&(_, [input, write_5m, write_1h, read, output], fast_mode, us_premium)| Rate {
                input: pico(input),
                output: pico(output),
                cache_read: Some(pico(read)),
                cache_write_5m: Some(pico(write_5m)),
                cache_write_1h: Some(pico(write_1h)),
                fast_mode,
                us_premium,
                long_context: false,
            },
        ),
        Provider::Codex => CODEX.iter().find(|row| row.0 == id).map(
            |&(_, [input, cached, output], long_context)| Rate {
                input: pico(input),
                output: pico(output),
                cache_read: Some(pico(cached)),
                cache_write_5m: None,
                cache_write_1h: None,
                fast_mode: false,
                us_premium: false,
                long_context,
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: u64 = 1_000_000;

    fn parse(text: &str) -> Result<Prices> {
        Prices::from_document(&text.parse::<DocumentMut>().unwrap())
    }

    fn tokens(input: u64, cache_read: u64, write_5m: u64, write_1h: u64, output: u64) -> Tokens {
        Tokens {
            input,
            cache_read,
            cache_write_5m: write_5m,
            cache_write_1h: write_1h,
            output,
            reasoning: 0,
        }
    }

    fn rate(provider: Provider, model: &str) -> Rate {
        Prices::default()
            .rate(provider, model)
            .unwrap_or_else(|| panic!("{model} has no price"))
    }

    #[test]
    fn undated_strips_only_an_eight_digit_date() {
        assert_eq!(undated("claude-haiku-4-5-20251001"), "claude-haiku-4-5");
        assert_eq!(undated("claude-3-5-haiku-20241022"), "claude-3-5-haiku");
        for same in [
            "claude-opus-4-8",
            "x-2025100",
            "x-2025-10-01",
            "gpt-5.2",
            "-20251001",
            "",
        ] {
            assert_eq!(undated(same), same);
        }
    }

    /// R20: a model is found by its id without the date, exactly, and per provider.
    #[test]
    fn built_in_lookup_is_exact_after_the_date() {
        let claude = |m| rate(Provider::Claude, m);
        assert_eq!(claude("claude-fable-5-1").cache_read, Some(250_000));
        assert_eq!(claude("claude-fable-5").cache_read, Some(1_000_000));
        assert_eq!(
            claude("claude-opus-4-5-20251101"),
            claude("claude-opus-4-5")
        );
        // `claude-fable-5` and `claude-fable-5-1` differ (above); no prefix matches.
        assert_eq!(claude("claude-opus-4").input, 15_000_000);
        assert_eq!(claude("claude-opus-4-1").input, 15_000_000);
        let prices = Prices::default();
        for model in [
            "claude-opus-4-8-preview",
            "claude-opus",
            "claude-opus-4-2",
            "claude-test",
            "unknown",
        ] {
            assert_eq!(prices.rate(Provider::Claude, model), None, "{model}");
        }
        assert_eq!(prices.rate(Provider::Codex, "claude-opus-5"), None);
        assert_eq!(prices.rate(Provider::Claude, "gpt-5.4"), None);
        for model in ["codex-auto-review", "gpt-5.3-codex-spark", "gpt-test"] {
            assert_eq!(prices.rate(Provider::Codex, model), None, "{model}");
        }
    }

    /// R20: each count at its own price; reasoning is part of output.
    #[test]
    fn a_request_costs_each_count_at_its_price() {
        let opus = rate(Provider::Claude, "claude-opus-4-6");
        let mut t = tokens(M, M, M, M, M);
        assert_eq!(opus.cost(&t, false, false), Some(46_750_000_000_000));
        t.reasoning = 500_000;
        assert_eq!(opus.cost(&t, false, false), Some(46_750_000_000_000));
        assert_eq!(opus.cost(&Tokens::default(), false, false), Some(0));
    }

    /// R20: fast mode doubles every price on opus-5-5, opus-5 and opus-4-8 only.
    #[test]
    fn fast_mode_doubles_on_three_models_only() {
        let cost = |model, t: Tokens| rate(Provider::Claude, model).cost(&t, true, false);
        let input = tokens(M, 0, 0, 0, 0);
        let output = tokens(0, 0, 0, 0, M);
        assert_eq!(cost("claude-opus-5-5", input), Some(8_000_000_000_000));
        assert_eq!(cost("claude-opus-5-5", output), Some(40_000_000_000_000));
        assert_eq!(cost("claude-opus-5", input), Some(10_000_000_000_000));
        assert_eq!(cost("claude-opus-4-8", input), Some(10_000_000_000_000));
        assert_eq!(cost("claude-sonnet-4-6", input), Some(3_000_000_000_000));
        // Cache prices keep their ratio to input.
        assert_eq!(
            cost("claude-opus-5-5", tokens(0, M, M, M, 0)),
            Some((400_000 + 10_000_000 + 16_000_000) * M as u128)
        );
    }

    /// R20: US-only inference adds a tenth on the models from 4.6 on; with fast mode, both.
    #[test]
    fn us_inference_adds_a_tenth_from_4_6() {
        let input = tokens(M, 0, 0, 0, 0);
        let cost = |model, fast| rate(Provider::Claude, model).cost(&input, fast, true);
        assert_eq!(cost("claude-opus-4-6", false), Some(5_500_000_000_000));
        assert_eq!(cost("claude-opus-4-5", false), Some(5_000_000_000_000));
        assert_eq!(cost("claude-opus-5-5", true), Some(8_800_000_000_000));
        assert_eq!(cost("claude-haiku-4-5", false), Some(1_000_000_000_000));
    }

    /// R20: codex prices input without cached, cached input, and output (reasoning included);
    /// it records no cache write.
    #[test]
    fn codex_prices_input_cached_and_output() {
        let prices =
            parse("[prices.\"gpt-test\"]\ninput = 1.25\ncache_read = 0.125\noutput = 10\n")
                .unwrap();
        let gpt = prices.rate(Provider::Codex, "gpt-test").unwrap();
        let mut t = tokens(M, M, 0, 0, M);
        t.reasoning = 500_000;
        assert_eq!(gpt.cost(&t, false, false), Some(11_375_000_000_000));
        assert_eq!(gpt.cost(&tokens(0, 0, 1, 0, 0), false, false), None);
        let builtin = rate(Provider::Codex, "gpt-5.1-codex");
        assert_eq!(builtin.cost(&t, false, false), Some(11_375_000_000_000));
        assert_eq!(builtin.cost(&t, true, true), Some(11_375_000_000_000));
    }

    /// R20: a codex request with more than 272K input tokens, cached included, is priced at
    /// twice the input and cached prices and 1.5 times the output price, on the models that
    /// have a long-context price; exactly, since every such price is even.
    #[test]
    fn codex_long_context_reprices_the_whole_request() {
        let gpt = rate(Provider::Codex, "gpt-5.4");
        assert!(gpt.long_context);
        let at = tokens(200_000, 72_000, 0, 0, 1_000);
        assert_eq!(
            gpt.cost(&at, false, false),
            Some(200_000 * 2_500_000 + 72_000 * 250_000 + 1_000 * 15_000_000)
        );
        let over = tokens(200_001, 72_000, 0, 0, 1_000);
        assert_eq!(
            gpt.cost(&over, false, false),
            Some(200_001 * 5_000_000 + 72_000 * 500_000 + 1_000 * 22_500_000)
        );
        // Cached input alone crosses it too.
        let cached = tokens(0, 272_001, 0, 0, 0);
        assert_eq!(gpt.cost(&cached, false, false), Some(272_001 * 500_000));

        let plain = rate(Provider::Codex, "gpt-5.3-codex");
        assert!(!plain.long_context);
        let big = tokens(300_000, 100_000, 0, 0, 1_000);
        assert_eq!(
            plain.cost(&big, false, false),
            Some(300_000 * 1_750_000 + 100_000 * 175_000 + 1_000 * 14_000_000)
        );
        // Claude's rates have no long-context price.
        let opus = rate(Provider::Claude, "claude-opus-4-6");
        assert_eq!(
            opus.cost(&tokens(1_000_000, 0, 0, 0, 0), false, false),
            Some(5_000_000_000_000)
        );

        let long: Vec<_> = CODEX.iter().filter(|row| row.2).collect();
        assert_eq!(long.len(), 6);
        for &&(model, prices, _) in &long {
            assert!(prices.iter().all(|p| p % 2 == 0), "{model}: {prices:?}");
        }
        // An override keeps the flag.
        let prices =
            parse("[prices.\"gpt-5.4\"]\ninput = 2\noutput = 10\ncache_read = 0.2\n").unwrap();
        let over_rate = prices.rate(Provider::Codex, "gpt-5.4").unwrap();
        assert!(over_rate.long_context);
        assert_eq!(
            over_rate.cost(&over, false, false),
            Some(200_001 * 4_000_000 + 72_000 * 400_000 + 1_000 * 15_000_000)
        );
    }

    /// R3, R20: `[prices."<model>"]` replaces a built-in price or prices a model that has none.
    #[test]
    fn overrides() {
        let prices = parse(
            "[prices.\"claude-opus-4-6\"]\ninput = 1\noutput = 2\n\n\
             [prices.\"claude-haiku-4-5-20251001\"]\ninput = 7\noutput = 7\n\n\
             [prices.\"my-model\"]\ninput = 0.08\noutput = 3\ncache_read = 0\n\n\
             [prices.\"claude-opus-5-5\"]\ninput = 1\noutput = 1\n",
        )
        .unwrap();
        let opus = prices.rate(Provider::Claude, "claude-opus-4-6").unwrap();
        assert_eq!(
            opus.cost(&tokens(M, 0, 0, 0, 0), false, false),
            Some(1_000_000_000_000)
        );
        assert_eq!(opus.cost(&tokens(0, M, 0, 0, 0), false, false), None);
        assert!(opus.us_premium);

        let dated = prices
            .rate(Provider::Claude, "claude-haiku-4-5-20251001")
            .unwrap();
        assert_eq!(dated.input, 7 * M);
        let other_date = prices
            .rate(Provider::Claude, "claude-haiku-4-5-20260101")
            .unwrap();
        assert_eq!(other_date.input, 1_000_000, "built-in");

        let undated = parse("[prices.\"claude-haiku-4-5\"]\ninput = 7\noutput = 7\n").unwrap();
        assert_eq!(
            undated
                .rate(Provider::Claude, "claude-haiku-4-5-20251001")
                .unwrap()
                .input,
            7 * M
        );

        for provider in [Provider::Claude, Provider::Codex] {
            let mine = prices.rate(provider, "my-model").unwrap();
            assert_eq!(mine.input, 80_000);
            assert_eq!(mine.cache_read, Some(0));
            assert_eq!(mine.cache_write_1h, None);
            assert!(!mine.fast_mode && !mine.us_premium && !mine.long_context);
        }

        let fast = prices.rate(Provider::Claude, "claude-opus-5-5").unwrap();
        assert!(fast.fast_mode && fast.us_premium);
        assert_eq!(
            fast.cost(&tokens(M, 0, 0, 0, 0), true, true),
            Some(2_200_000_000_000)
        );

        // Inline tables too.
        let inline = parse("prices = { \"x\" = { input = 3, output = 15 } }\n").unwrap();
        assert_eq!(inline.overrides["x"].output, 15 * M);
        assert_eq!(parse("").unwrap(), Prices::default());
    }

    /// R3: `[prices]` is validated strictly; errors name the table and the key.
    #[test]
    fn rejects_invalid_overrides() {
        for (text, want) in [
            ("prices = 1\n", "`prices` must be a table"),
            ("[prices]\nx = 1\n", "[prices.\"x\"] must be a table"),
            (
                "[prices.\"x\"]\ninput = 1\noutput = 1\ncached = 1\n",
                "[prices.\"x\"]: unknown key `cached` (known: input, output, cache_read, \
                 cache_write_5m, cache_write_1h)",
            ),
            (
                "[prices.\"x\"]\noutput = 1\n",
                "[prices.\"x\"]: missing `input`",
            ),
            (
                "[prices.\"x\"]\ninput = 1\n",
                "[prices.\"x\"]: missing `output`",
            ),
            (
                "[prices.\"x\"]\ninput = -1\noutput = 1\n",
                "`input` must be from 0 to 1000000 (USD per million tokens), got -1",
            ),
            (
                "[prices.\"x\"]\ninput = \"3\"\noutput = 1\n",
                "[prices.\"x\"]: `input` must be a number",
            ),
            ("[prices.\"x\"]\ninput = nan\noutput = 1\n", "got NaN"),
            ("[prices.\"x\"]\ninput = inf\noutput = 1\n", "got inf"),
            (
                "[prices.\"x\"]\ninput = 1000001\noutput = 1\n",
                "must be from 0 to 1000000",
            ),
            (
                "[prices.\"\"]\ninput = 1\noutput = 1\n",
                "[prices]: empty model id",
            ),
        ] {
            let err = format!("{:#}", parse(text).unwrap_err());
            assert!(err.contains(want), "{text}\n=> {err}");
        }
        let max = parse("[prices.\"x\"]\ninput = 1000000\noutput = 0\n").unwrap();
        assert_eq!(max.overrides["x"].input, 1_000_000 * M);
    }
}
