//! Estimated API-equivalent cost. Subscriptions aren't billed per token, so this is
//! only a "what would this have cost on the API" figure.
//!
//! Rates are first-party list prices in USD per million tokens (source: Anthropic
//! pricing docs, as of 2026-10-06). Cache writes are 1.25x input (5 min TTL) and
//! 2x input (1 h TTL) on every model; cache reads vary per model.

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
}

const fn r(input: f64, output: f64, cache_read: f64) -> Rates {
    Rates { input, output, cache_read }
}

const TABLE: &[Entry] = &[
    Entry { prefix: "claude-fable-5-1", rates: r(10.0, 50.0, 0.25), long_prompt: None },
    Entry { prefix: "claude-mythos-5-1", rates: r(10.0, 50.0, 0.25), long_prompt: None },
    Entry { prefix: "claude-fable-5", rates: r(10.0, 50.0, 1.0), long_prompt: None },
    Entry { prefix: "claude-opus-5-5", rates: r(4.0, 20.0, 0.20), long_prompt: None },
    Entry { prefix: "claude-opus-5", rates: r(5.0, 25.0, 0.50), long_prompt: None },
    Entry { prefix: "claude-opus-4-8", rates: r(5.0, 25.0, 0.50), long_prompt: None },
    Entry { prefix: "claude-opus-4-7", rates: r(5.0, 25.0, 0.50), long_prompt: None },
    Entry { prefix: "claude-opus-4-6", rates: r(5.0, 25.0, 0.50), long_prompt: None },
    Entry { prefix: "claude-sonnet-5-5", rates: r(2.0, 10.0, 0.20), long_prompt: None },
    Entry { prefix: "claude-sonnet-5", rates: r(2.0, 10.0, 0.20), long_prompt: None },
    Entry { prefix: "claude-sonnet-4-6", rates: r(3.0, 15.0, 0.30), long_prompt: None },
    Entry {
        prefix: "claude-haiku-5-5",
        rates: r(0.10, 0.50, 0.01),
        long_prompt: Some((100_000, r(0.50, 2.50, 0.05))),
    },
    Entry { prefix: "claude-haiku-4-5", rates: r(1.0, 5.0, 0.10), long_prompt: None },
];

/// Unknown models fall back to the current model of the same family.
const FAMILY_FALLBACK: &[(&str, &str)] = &[
    ("fable", "claude-fable-5-1"),
    ("mythos", "claude-mythos-5-1"),
    ("opus", "claude-opus-5-5"),
    ("sonnet", "claude-sonnet-5-5"),
    ("haiku", "claude-haiku-5-5"),
];

const FAST_MODE_MULTIPLIER: f64 = 2.0;

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

/// Estimated cost in USD; 0.0 for models we can't price.
pub fn cost_usd(model: &str, t: &Tokens, fast: bool) -> f64 {
    let Some(entry) = lookup(model) else { return 0.0 };
    let rates = match entry.long_prompt {
        Some((threshold, long)) if t.prompt() > threshold => long,
        _ => entry.rates,
    };
    let per_mtok = t.input as f64 * rates.input
        + t.output as f64 * rates.output
        + t.cache_read as f64 * rates.cache_read
        + t.cache_write_5m as f64 * rates.input * 1.25
        + t.cache_write_1h as f64 * rates.input * 2.0;
    let cost = per_mtok / 1_000_000.0;
    if fast { cost * FAST_MODE_MULTIPLIER } else { cost }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
