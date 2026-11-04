//! Turning token counts into money, or saying that we cannot.
//!
//! The extraction is deliberately small: a lookup and a multiplication, kept
//! apart from iteration logic so the rates can later move to configuration or
//! a control plane without touching the driver.
//!
//! It replaces a heuristic in the CLI loop (`agent_loop.rs:1411-1430`) that
//! matched model names by substring against three entries and sent everything
//! else — every Anthropic model, every Ollama model, every OpenAI-compatible
//! endpoint — down the `else` branch at **Claude Sonnet rates**. A local Ollama
//! model costing nothing was billed at $3/$15 per million; a model priced above
//! Sonnet was under-counted. Either way the number was fabricated, and it fed a
//! budget that was supposed to stop the session.
//!
//! The rule here: no rate, no price. `None` means unknown, and what a caller
//! does about that is the caller's decision — see `TurnBudget` and the driver's
//! strict-budget handling.

use crate::agent::model::Usage;

/// USD per million tokens for one model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rate {
    pub input_per_m: f64,
    pub output_per_m: f64,
    /// Anthropic bills cache writes at 1.25× input and reads at 0.10×. Other
    /// providers have no cache pricing, in which case these are `None` and
    /// cache tokens are counted at the plain input rate.
    pub cache_write_multiplier: Option<f64>,
    pub cache_read_multiplier: Option<f64>,
    /// Other rates for a request whose prompt is longer: Claude Haiku 5.5
    /// bills a prompt over 100,000 tokens at higher rates.
    pub long_prompt: Option<LongPrompt>,
}

impl Rate {
    /// Field by field, the higher of `self` and `other`.
    fn at_least(self, other: Rate) -> Rate {
        let max = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        Rate {
            input_per_m: self.input_per_m.max(other.input_per_m),
            output_per_m: self.output_per_m.max(other.output_per_m),
            cache_write_multiplier: max(self.cache_write_multiplier, other.cache_write_multiplier),
            cache_read_multiplier: max(self.cache_read_multiplier, other.cache_read_multiplier),
            long_prompt: self.long_prompt.map(|l| LongPrompt {
                input_per_m: l.input_per_m.max(other.input_per_m),
                output_per_m: l.output_per_m.max(other.output_per_m),
                ..l
            }),
        }
    }
}

/// The rates a request pays once its prompt (input, cache writes and cache
/// reads) exceeds `above_tokens`. Cache multipliers apply to `input_per_m`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LongPrompt {
    pub above_tokens: u64,
    pub input_per_m: f64,
    pub output_per_m: f64,
}

/// Resolves a model name to a rate.
///
/// A trait so pricing data can come from config, a control plane, or a test
/// fixture without the driver knowing which.
pub trait PriceList: Send + Sync {
    /// `None` when this model has no known price. Never a guess.
    fn rate_for(&self, model: &str) -> Option<Rate>;
}

/// Anthropic list prices, USD per million tokens: first-party API, global
/// endpoint, standard tier (no batch, data residency or fast mode). Read
/// 2026-10-27 on https://platform.claude.com/docs/en/about-claude/pricing;
/// re-read 2026-11-02: Fable 5/5.1 and Mythos 5/5.1 listed at $10/$50.
/// `(family, major, minor, input, output, cache-read multiplier)`; every
/// listed model writes its 5-minute cache at 1.25x input.
const ANTHROPIC_LISTED: &[(&str, u32, Option<u32>, f64, f64, f64)] = &[
    ("fable", 5, Some(1), 10.0, 50.0, 0.025),
    ("fable", 5, None, 10.0, 50.0, 0.10),
    ("mythos", 5, Some(1), 10.0, 50.0, 0.025),
    ("mythos", 5, None, 10.0, 50.0, 0.10),
    ("opus", 5, Some(5), 4.0, 20.0, 0.05),
    ("opus", 5, None, 5.0, 25.0, 0.10),
    ("opus", 4, Some(8), 5.0, 25.0, 0.10),
    ("opus", 4, Some(7), 5.0, 25.0, 0.10),
    ("opus", 4, Some(6), 5.0, 25.0, 0.10),
    ("opus", 4, Some(5), 5.0, 25.0, 0.10),
    ("opus", 4, Some(1), 15.0, 75.0, 0.10),
    ("opus", 4, None, 15.0, 75.0, 0.10),
    ("sonnet", 5, Some(5), 2.0, 10.0, 0.05),
    ("sonnet", 5, None, 2.0, 10.0, 0.10),
    ("sonnet", 4, Some(6), 3.0, 15.0, 0.10),
    ("sonnet", 4, Some(5), 3.0, 15.0, 0.10),
    ("sonnet", 4, None, 3.0, 15.0, 0.10),
    ("haiku", 5, Some(5), 0.10, 0.50, 0.10),
    ("haiku", 4, Some(5), 1.0, 5.0, 0.10),
    ("haiku", 3, Some(5), 0.80, 4.0, 0.10),
];

/// Same page: a Claude Haiku 5.5 prompt over 100,000 tokens pays $0.50/$2.50.
const HAIKU_5_5_LONG_PROMPT: LongPrompt = LongPrompt {
    above_tokens: 100_000,
    input_per_m: 0.50,
    output_per_m: 2.50,
};

const CLAUDE_FAMILIES: &[&str] = &["fable", "mythos", "opus", "sonnet", "haiku"];

/// `(family, major, minor)` of the Claude model a name designates, read from
/// its first `claude-`: `claude-opus-4-5-20251101`, `claude-3-5-haiku-…`,
/// `anthropic/claude-sonnet-4.5`. An 8-digit date is not a minor version.
fn claude_version(name: &str) -> Option<(&'static str, u32, Option<u32>)> {
    let lower = name.to_ascii_lowercase();
    let rest = &lower[lower.find("claude-")? + "claude-".len()..];
    let tokens: Vec<&str> = rest.split(['-', '.']).collect();
    let family = |t: Option<&&str>| CLAUDE_FAMILIES.iter().copied().find(|f| Some(f) == t);
    let number = |t: Option<&&str>| -> Option<u32> {
        let digits: String = t?.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() || digits.len() > 2 {
            return None;
        }
        digits.parse().ok()
    };
    if let Some(f) = family(tokens.first()) {
        return Some((f, number(tokens.get(1))?, number(tokens.get(2))));
    }
    // The older order: version first, then the family.
    let major = number(tokens.first())?;
    match number(tokens.get(1)) {
        Some(minor) => Some((family(tokens.get(2))?, major, Some(minor))),
        None => Some((family(tokens.get(1))?, major, None)),
    }
}

/// The list price of a Claude model the list names, else `None`.
fn listed_claude_rate(name: &str) -> Option<Rate> {
    let (family, major, minor) = claude_version(name)?;
    let &(_, _, _, input, output, read) = ANTHROPIC_LISTED
        .iter()
        .find(|row| row.0 == family && row.1 == major && row.2 == minor)?;
    Some(Rate {
        input_per_m: input,
        output_per_m: output,
        cache_write_multiplier: Some(1.25),
        cache_read_multiplier: Some(read),
        long_prompt: (family == "haiku" && major == 5 && minor == Some(5))
            .then_some(HAIKU_5_5_LONG_PROMPT),
    })
}

/// Prices of the Claude models Anthropic lists (`ANTHROPIC_LISTED`), then
/// those read from `[cost]`.
///
/// `[cost]` names four families: a Sonnet or Haiku the list does not carry
/// (the `sonnet` alias) keeps its configured rate. Anything else is unknown,
/// which is the correction — the previous heuristic's `else` branch claimed
/// to know every model in existence.
///
/// Two readings: [`ConfiguredPrices::from_config`] shows the list price;
/// [`ConfiguredPrices::for_budget`], what the money cap counts, takes the
/// higher of the list price and the `[cost]` rate of the same family.
pub struct ConfiguredPrices {
    sonnet: Rate,
    haiku: Rate,
    gpt4o: Rate,
    gpt4o_mini: Rate,
    budget: bool,
}

impl ConfiguredPrices {
    /// For a cap: a `[cost]` rate above the list price is not undercut by it.
    /// Review of 2026-10-27: the list won, and a cap counted below `[cost]`.
    pub fn for_budget(cost: &crate::config::CostConfig) -> Self {
        Self {
            budget: true,
            ..Self::from_config(cost)
        }
    }

    pub fn from_config(cost: &crate::config::CostConfig) -> Self {
        Self {
            budget: false,
            sonnet: Rate {
                input_per_m: cost.sonnet_input_per_m,
                output_per_m: cost.sonnet_output_per_m,
                cache_write_multiplier: Some(1.25),
                cache_read_multiplier: Some(0.10),
                long_prompt: None,
            },
            haiku: Rate {
                input_per_m: cost.haiku_input_per_m,
                output_per_m: cost.haiku_output_per_m,
                cache_write_multiplier: Some(1.25),
                cache_read_multiplier: Some(0.10),
                long_prompt: None,
            },
            gpt4o: Rate {
                input_per_m: cost.gpt4o_input_per_m,
                output_per_m: cost.gpt4o_output_per_m,
                cache_write_multiplier: None,
                cache_read_multiplier: None,
                long_prompt: None,
            },
            gpt4o_mini: Rate {
                input_per_m: cost.gpt4o_mini_input_per_m,
                output_per_m: cost.gpt4o_mini_output_per_m,
                cache_write_multiplier: None,
                cache_read_multiplier: None,
                long_prompt: None,
            },
        }
    }
}

impl PriceList for ConfiguredPrices {
    fn rate_for(&self, model: &str) -> Option<Rate> {
        let m = model.to_lowercase();
        // Most specific first: "gpt-4o-mini" also contains "gpt-4o".
        if m.contains("gpt-4o-mini") {
            return Some(self.gpt4o_mini);
        }
        if m.contains("gpt-4o") {
            return Some(self.gpt4o);
        }
        if let Some(rate) = listed_claude_rate(&m) {
            let family = if m.contains("sonnet") {
                Some(self.sonnet)
            } else if m.contains("haiku") {
                Some(self.haiku)
            } else {
                None
            };
            return Some(match family {
                Some(configured) if self.budget => rate.at_least(configured),
                _ => rate,
            });
        }
        if m.contains("sonnet") {
            return Some(self.sonnet);
        }
        if m.contains("haiku") {
            return Some(self.haiku);
        }
        // Everything else — an unlisted Opus, Ollama, any OpenAI-compatible
        // endpoint — has no rate. Saying so is the point.
        None
    }
}

/// A price list that knows nothing. For tests and for deployments that have
/// not configured pricing.
pub struct NoPrices;

impl PriceList for NoPrices {
    fn rate_for(&self, _model: &str) -> Option<Rate> {
        None
    }
}

/// Estimate what a call cost.
///
/// `None` when the model has no rate, or when the provider did not report the
/// token counts the rate would apply to. Both are genuinely unknown and are
/// reported as such.
pub fn estimate(prices: &dyn PriceList, model: &str, usage: &Usage) -> Option<f64> {
    let rate = prices.rate_for(model)?;
    let input = usage.input_tokens? as f64;
    let output = usage.output_tokens? as f64;

    // Cache tokens are optional; absent means the provider does not report
    // them, which is different from reporting zero.
    let cache_write = usage.cache_write_tokens.unwrap_or(0) as f64;
    let cache_read = usage.cache_read_tokens.unwrap_or(0) as f64;

    // One request, one tier: the prompt counts cache reads and writes too.
    let (input_per_m, output_per_m) = match rate.long_prompt {
        Some(long) if input + cache_write + cache_read > long.above_tokens as f64 => {
            (long.input_per_m, long.output_per_m)
        }
        _ => (rate.input_per_m, rate.output_per_m),
    };

    let per_m = 1_000_000.0;
    let write_rate = input_per_m * rate.cache_write_multiplier.unwrap_or(1.0);
    let read_rate = input_per_m * rate.cache_read_multiplier.unwrap_or(1.0);

    Some(
        (input * input_per_m
            + output * output_per_m
            + cache_write * write_rate
            + cache_read * read_rate)
            / per_m,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_write_tokens: Some(0),
            cache_read_tokens: Some(0),
            cost_usd: None,
        }
    }

    fn prices() -> ConfiguredPrices {
        ConfiguredPrices::from_config(&crate::config::CostConfig::default())
    }

    #[test]
    fn a_known_model_is_priced_from_its_rates() {
        let rate = prices().rate_for("claude-sonnet-5").expect("known");
        let cost = estimate(&prices(), "claude-sonnet-5", &usage(1_000_000, 0)).expect("priced");
        assert!(
            (cost - rate.input_per_m).abs() < 1e-9,
            "one million input tokens costs exactly the input rate, got {cost}"
        );
    }

    #[test]
    fn input_and_output_are_billed_at_their_own_rates() {
        let p = prices();
        let r = p.rate_for("gpt-4o").unwrap();
        let cost = estimate(&p, "gpt-4o", &usage(1_000_000, 1_000_000)).unwrap();
        assert!((cost - (r.input_per_m + r.output_per_m)).abs() < 1e-9);
    }

    /// The correction. Previously every unrecognised model was billed at
    /// Sonnet rates, so a free local model produced a real-looking charge.
    #[test]
    fn an_unknown_model_has_no_price() {
        assert_eq!(prices().rate_for("llama3:70b"), None);
        assert_eq!(prices().rate_for("ollama/qwen3:8b"), None);
        // An Opus the list does not name, and the bare alias: not guessed.
        assert_eq!(prices().rate_for("anthropic/claude-opus-9"), None);
        assert_eq!(prices().rate_for("anthropic/opus"), None);
        assert_eq!(estimate(&prices(), "llama3:70b", &usage(1000, 1000)), None);
    }

    /// Anthropic's list, read 2026-10-27 on
    /// https://platform.claude.com/docs/en/about-claude/pricing: (model as
    /// the backend names it, input, output, cache-read multiplier). Opus had
    /// no price at all before 2026-10-27, Sonnet 5 was billed $3/$15.
    const LISTED: &[(&str, f64, f64, f64)] = &[
        ("anthropic/claude-fable-5-1", 10.0, 50.0, 0.025),
        ("anthropic/claude-fable-5", 10.0, 50.0, 0.10),
        ("anthropic/claude-mythos-5-1", 10.0, 50.0, 0.025),
        ("anthropic/claude-mythos-5", 10.0, 50.0, 0.10),
        ("anthropic/claude-opus-5-5", 4.0, 20.0, 0.05),
        ("anthropic/claude-opus-5", 5.0, 25.0, 0.10),
        ("anthropic/claude-opus-4-8", 5.0, 25.0, 0.10),
        ("anthropic/claude-opus-4-7", 5.0, 25.0, 0.10),
        ("anthropic/claude-opus-4-6", 5.0, 25.0, 0.10),
        ("anthropic/claude-opus-4-5-20251101", 5.0, 25.0, 0.10),
        ("anthropic/claude-opus-4-1-20250805", 15.0, 75.0, 0.10),
        ("anthropic/claude-opus-4-20250514", 15.0, 75.0, 0.10),
        ("anthropic/claude-sonnet-5-5", 2.0, 10.0, 0.05),
        ("anthropic/claude-sonnet-5", 2.0, 10.0, 0.10),
        ("anthropic/claude-sonnet-4-6", 3.0, 15.0, 0.10),
        ("anthropic/claude-sonnet-4-5-20250929", 3.0, 15.0, 0.10),
        ("anthropic/claude-sonnet-4-20250514", 3.0, 15.0, 0.10),
        ("anthropic/claude-haiku-5-5", 0.10, 0.50, 0.10),
        ("anthropic/claude-haiku-4-5-20251001", 1.0, 5.0, 0.10),
        ("anthropic/claude-3-5-haiku-20241022", 0.80, 4.0, 0.10),
    ];

    #[test]
    fn every_listed_claude_model_has_the_list_price() {
        let p = prices();
        let mut wrong = Vec::new();
        for &(model, input, output, read) in LISTED {
            let got = p.rate_for(model);
            let ok = got.is_some_and(|r| {
                (r.input_per_m - input).abs() < 1e-9
                    && (r.output_per_m - output).abs() < 1e-9
                    && r.cache_write_multiplier == Some(1.25)
                    && r.cache_read_multiplier == Some(read)
            });
            if !ok {
                wrong.push(format!("{model}: {got:?}"));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// Haiku 5.5 is priced by prompt length: over 100,000 prompt tokens,
    /// cache reads and writes included, the request pays $0.50/$2.50.
    #[test]
    fn haiku_5_5_bills_a_long_prompt_at_its_higher_rates() {
        let p = prices();
        let short = estimate(&p, "anthropic/claude-haiku-5-5", &usage(100_000, 1_000_000)).unwrap();
        assert!((short - (0.01 + 0.50)).abs() < 1e-9, "{short}");
        let u = Usage {
            input_tokens: Some(1_000),
            output_tokens: Some(1_000_000),
            cache_write_tokens: Some(0),
            cache_read_tokens: Some(100_000),
            cost_usd: None,
        };
        let long = estimate(&p, "anthropic/claude-haiku-5-5", &u).unwrap();
        let expected = 1_000.0 * 0.50 / 1e6 + 100_000.0 * 0.05 / 1e6 + 2.50;
        assert!((long - expected).abs() < 1e-9, "{long} != {expected}");
    }

    /// A Sonnet or Haiku name the list does not carry keeps the `[cost]`
    /// rate, as before; the list wins for the names it carries.
    #[test]
    fn an_unlisted_sonnet_keeps_the_configured_rate() {
        let mut cost = crate::config::CostConfig::default();
        cost.sonnet_input_per_m = 7.0;
        let p = ConfiguredPrices::from_config(&cost);
        assert_eq!(p.rate_for("anthropic/sonnet").unwrap().input_per_m, 7.0);
        assert_eq!(
            p.rate_for("anthropic/claude-sonnet-5").unwrap().input_per_m,
            2.0
        );
    }

    /// Missing usage is unknown too — a rate cannot be applied to nothing.
    #[test]
    fn unknown_usage_has_no_price() {
        let u = Usage {
            input_tokens: None,
            output_tokens: Some(10),
            ..Default::default()
        };
        assert_eq!(estimate(&prices(), "claude-sonnet-5", &u), None);
    }

    /// Anthropic's cache multipliers are the reason cache tokens are tracked
    /// separately at all: a read is a tenth of an input token.
    #[test]
    fn cache_tokens_use_their_multipliers() {
        let p = prices();
        let r = p.rate_for("claude-sonnet-5").unwrap();
        let u = Usage {
            input_tokens: Some(0),
            output_tokens: Some(0),
            cache_write_tokens: Some(1_000_000),
            cache_read_tokens: Some(1_000_000),
            cost_usd: None,
        };
        let cost = estimate(&p, "claude-sonnet-5", &u).unwrap();
        let expected = r.input_per_m * 1.25 + r.input_per_m * 0.10;
        assert!(
            (cost - expected).abs() < 1e-9,
            "got {cost}, want {expected}"
        );
    }

    /// A provider with no cache pricing bills cache tokens as plain input
    /// rather than inventing a multiplier.
    #[test]
    fn a_provider_without_cache_pricing_bills_at_the_input_rate() {
        let p = prices();
        let r = p.rate_for("gpt-4o").unwrap();
        assert_eq!(r.cache_write_multiplier, None);
        let u = Usage {
            input_tokens: Some(0),
            output_tokens: Some(0),
            cache_write_tokens: Some(1_000_000),
            cache_read_tokens: Some(0),
            cost_usd: None,
        };
        let cost = estimate(&p, "gpt-4o", &u).unwrap();
        assert!((cost - r.input_per_m).abs() < 1e-9);
    }

    #[test]
    fn a_price_list_that_knows_nothing_prices_nothing() {
        assert_eq!(estimate(&NoPrices, "claude-sonnet-5", &usage(10, 10)), None);
    }

    /// gpt-4o-mini must not be caught by the gpt-4o arm.
    #[test]
    fn the_more_specific_model_wins() {
        let p = prices();
        assert_ne!(p.rate_for("gpt-4o-mini"), p.rate_for("gpt-4o"));
    }

    /// The cap counts the higher of the list and `[cost]`; `/cost` shows the
    /// list price.
    #[test]
    fn the_budget_reading_never_undercuts_cost() {
        let cost = crate::config::CostConfig {
            sonnet_input_per_m: 6.0,
            haiku_output_per_m: 9.0,
            ..Default::default()
        };
        let shown = ConfiguredPrices::from_config(&cost);
        let capped = ConfiguredPrices::for_budget(&cost);
        assert_eq!(shown.rate_for("claude-sonnet-5").unwrap().input_per_m, 2.0);
        let s = capped.rate_for("claude-sonnet-5").unwrap();
        // Output: the `[cost]` default, $15, above the list's $10.
        assert_eq!((s.input_per_m, s.output_per_m), (6.0, 15.0));
        let h = capped.rate_for("claude-haiku-5-5").unwrap();
        assert_eq!(h.output_per_m, 9.0);
        assert_eq!(h.long_prompt.unwrap().output_per_m, 9.0);
        // No `[cost]` family: the list alone.
        assert_eq!(capped.rate_for("claude-opus-5").unwrap().input_per_m, 5.0);
    }
}
