//! Estimated API-equivalent cost. Subscriptions aren't billed per token, so this is
//! only a "what would this have cost on the API" figure.
//!
//! Rates are first-party list prices in USD per million tokens (Anthropic and OpenAI
//! pricing docs). Anthropic cache writes are 1.25x input (5 min TTL) and 2x input
//! (1 h TTL) on every model; cache reads vary per model. Fast mode is 2x, except
//! where an entry says otherwise.
//!
//! Config overrides (`[pricing."<model prefix>"]`) replace the matching model's base
//! rates field by field, and disable its long-prompt tier.

use std::collections::BTreeMap;

use crate::config::RateOverride;
use crate::model::Tokens;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
}

struct Entry {
    prefix: &'static str,
    rates: Rates,
    /// Higher rates once the prompt exceeds the given token count.
    long_prompt: Option<(u64, Rates)>,
    /// Price multiplier for fast/priority mode.
    fast: f64,
}

const fn r(input: f64, output: f64, cache_read: f64) -> Rates {
    Rates { input, output, cache_read }
}

const fn e(prefix: &'static str, input: f64, output: f64, cache_read: f64) -> Entry {
    Entry { prefix, rates: r(input, output, cache_read), long_prompt: None, fast: DEFAULT_FAST_MULTIPLIER }
}

const TABLE: &[Entry] = &[
    // Anthropic (as of 2026-10-06)
    e("claude-fable-5-1", 10.0, 50.0, 0.25),
    e("claude-mythos-5-1", 10.0, 50.0, 0.25),
    e("claude-fable-5", 10.0, 50.0, 1.0),
    e("claude-opus-5-5", 4.0, 20.0, 0.20),
    e("claude-opus-5", 5.0, 25.0, 0.50),
    e("claude-opus-4-8", 5.0, 25.0, 0.50),
    e("claude-opus-4-7", 5.0, 25.0, 0.50),
    e("claude-opus-4-6", 5.0, 25.0, 0.50),
    e("claude-sonnet-5-5", 2.0, 10.0, 0.20),
    e("claude-sonnet-5", 2.0, 10.0, 0.20),
    e("claude-sonnet-4-6", 3.0, 15.0, 0.30),
    Entry { long_prompt: Some((100_000, r(0.50, 2.50, 0.05))), ..e("claude-haiku-5-5", 0.10, 0.50, 0.01) },
    e("claude-haiku-4-5", 1.0, 5.0, 0.10),
    // OpenAI (as of 2026-10-11). No cache-write charge.
    e("gpt-6-astra", 10.0, 50.0, 1.0),
    e("gpt-6.1-sol", 2.0, 10.0, 0.10),
    e("gpt-6-sol", 2.0, 10.0, 0.20),
    e("gpt-6-luna", 0.10, 0.50, 0.01),
    e("gpt-5.6-sol", 4.0, 20.0, 0.40),
    e("gpt-5.6-terra", 2.0, 12.0, 0.20),
    e("gpt-5.6-luna", 0.20, 1.20, 0.02),
    Entry { fast: 2.5, ..e("gpt-5.5", 5.0, 30.0, 0.50) },
    e("gpt-5.4", 2.50, 15.0, 0.25),
    e("gpt-5.4-mini", 0.75, 4.50, 0.075),
    e("gpt-5.4-nano", 0.20, 1.25, 0.02),
    e("gpt-5.3-codex", 1.75, 14.0, 0.175),
    e("gpt-5.2", 1.75, 14.0, 0.175),
    e("gpt-5.1", 1.25, 10.0, 0.125),
    e("gpt-5", 1.25, 10.0, 0.125),
    e("gpt-5-mini", 0.25, 2.0, 0.025),
    e("gpt-5-nano", 0.05, 0.40, 0.005),
];

/// Unknown models fall back to the current model of the same family.
const FAMILY_FALLBACK: &[(&str, &str)] = &[
    ("fable", "claude-fable-5-1"),
    ("mythos", "claude-mythos-5-1"),
    ("opus", "claude-opus-5-5"),
    ("sonnet", "claude-sonnet-5-5"),
    ("haiku", "claude-haiku-5-5"),
];

const DEFAULT_FAST_MULTIPLIER: f64 = 2.0;

/// Fast variants are listed as `<model>-fast` (opencode, pi).
fn split_fast(model: &str) -> (&str, bool) {
    match model.strip_suffix("-fast") {
        Some(base) => (base, true),
        None => (model, false),
    }
}

fn lookup(model: &str) -> Option<&'static Entry> {
    // Longest prefix wins so "claude-opus-5-5" isn't priced as "claude-opus-5".
    let exact = TABLE
        .iter()
        .filter(|e| model.starts_with(e.prefix))
        .max_by_key(|e| e.prefix.len());
    exact.or_else(|| {
        let (_, fallback) = FAMILY_FALLBACK.iter().find(|(family, _)| model.contains(family))?;
        TABLE.iter().find(|e| e.prefix == *fallback)
    })
}

#[derive(Debug, Default)]
pub struct Pricing {
    overrides: Vec<(String, RateOverride)>,
}

impl Pricing {
    pub fn new(overrides: &BTreeMap<String, RateOverride>) -> Self {
        Self { overrides: overrides.iter().map(|(k, v)| (k.clone(), *v)).collect() }
    }

    /// Estimated cost in USD; 0.0 for models we can't price.
    pub fn cost_usd(&self, model: &str, t: &Tokens, fast: bool) -> f64 {
        let (model, suffix_fast) = split_fast(model);
        let Some((rates, multiplier)) = self.rates(model, t) else { return 0.0 };
        let cost = cost_with(rates, t);
        if fast || suffix_fast { cost * multiplier } else { cost }
    }

    fn rates(&self, model: &str, t: &Tokens) -> Option<(Rates, f64)> {
        let entry = lookup(model);
        let ov = self
            .overrides
            .iter()
            .filter(|(prefix, _)| model.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, o)| o);
        let fast = entry.map_or(DEFAULT_FAST_MULTIPLIER, |e| e.fast);
        let rates = match (entry, ov) {
            (None, None) => return None,
            (Some(e), None) => match e.long_prompt {
                Some((threshold, long)) if t.prompt() > threshold => long,
                _ => e.rates,
            },
            (base, Some(o)) => {
                let b = base.map_or(r(0.0, 0.0, 0.0), |e| e.rates);
                Rates {
                    input: o.input.unwrap_or(b.input),
                    output: o.output.unwrap_or(b.output),
                    cache_read: o.cache_read.unwrap_or(b.cache_read),
                }
            }
        };
        Some((rates, fast))
    }
}

fn cost_with(rates: Rates, t: &Tokens) -> f64 {
    let per_mtok = t.input as f64 * rates.input
        + t.output as f64 * rates.output
        + t.cache_read as f64 * rates.cache_read
        + t.cache_write_5m as f64 * rates.input * 1.25
        + t.cache_write_1h as f64 * rates.input * 2.0;
    per_mtok / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cost_usd(model: &str, t: &Tokens, fast: bool) -> f64 {
        Pricing::default().cost_usd(model, t, fast)
    }

    const M: u64 = 1_000_000;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn longest_prefix_wins() {
        let t = Tokens { input: M, ..Default::default() };
        approx(cost_usd("claude-opus-5-5", &t, false), 4.0);
        approx(cost_usd("claude-opus-5", &t, false), 5.0);
        approx(cost_usd("claude-haiku-4-5-20251001", &t, false), 1.0);
    }

    #[test]
    fn cache_write_ttls() {
        let t = Tokens { cache_write_5m: M, cache_write_1h: M, ..Default::default() };
        approx(cost_usd("claude-opus-5-5", &t, false), 4.0 * 1.25 + 4.0 * 2.0);
    }

    #[test]
    fn haiku_long_prompt_tier() {
        let short = Tokens { input: 50_000, output: M, ..Default::default() };
        let long = Tokens { input: 150_000, output: M, ..Default::default() };
        approx(cost_usd("claude-haiku-5-5", &short, false), 0.05 * 0.10 + 0.50);
        approx(cost_usd("claude-haiku-5-5", &long, false), 0.15 * 0.50 + 2.50);
    }

    #[test]
    fn fast_mode_and_fallbacks() {
        let t = Tokens { output: M, ..Default::default() };
        approx(cost_usd("claude-opus-5-5", &t, true), 40.0);
        approx(cost_usd("claude-opus-9", &t, false), 20.0);
        approx(cost_usd("<synthetic>", &t, false), 0.0);
    }

    #[test]
    fn openai_models_and_fast_variants() {
        let t = Tokens { input: M, output: M, cache_read: M, ..Default::default() };
        approx(cost_usd("gpt-6.1-sol", &t, false), 2.0 + 10.0 + 0.10);
        approx(cost_usd("gpt-6.1-sol-fast", &t, false), 2.0 * 12.10);
        approx(cost_usd("gpt-5.4-mini", &t, false), 0.75 + 4.50 + 0.075);
        approx(cost_usd("gpt-5-mini-2025-08-07", &t, false), 0.25 + 2.0 + 0.025);
        approx(cost_usd("gpt-5.5", &t, true), 2.5 * 35.50);
        approx(cost_usd("gpt-5.5-fast", &t, false), 2.5 * 35.50);
    }

    #[test]
    fn overrides_merge_over_builtin_rates() {
        let mut o = BTreeMap::new();
        o.insert("claude-opus".to_string(), RateOverride { output: Some(10.0), ..Default::default() });
        o.insert("claude-opus-5-5".to_string(), RateOverride { input: Some(1.0), ..Default::default() });
        o.insert("my-local-model".to_string(), RateOverride { input: Some(2.0), ..Default::default() });
        let p = Pricing::new(&o);
        let t = Tokens { input: M, output: M, ..Default::default() };
        // Longest prefix wins: only input overridden, output stays built-in $20.
        approx(p.cost_usd("claude-opus-5-5", &t, false), 1.0 + 20.0);
        approx(p.cost_usd("claude-opus-5", &t, false), 5.0 + 10.0);
        // Unknown model: missing fields are 0.
        approx(p.cost_usd("my-local-model-7b", &t, false), 2.0);
        // Override disables Haiku's long-prompt tier.
        o.insert("claude-haiku-5-5".to_string(), RateOverride::default());
        let long = Tokens { input: 150_000, ..Default::default() };
        approx(Pricing::new(&o).cost_usd("claude-haiku-5-5", &long, false), 0.15 * 0.10);
    }
}
