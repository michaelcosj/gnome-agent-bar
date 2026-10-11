//! Optional user config: `$XDG_CONFIG_HOME/agent-bar/config.toml` (default `~/.config`).
//! Read once at startup; restart the service to apply changes. A missing file means
//! defaults; an invalid one is logged and ignored so the bar keeps working.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use tracing::{info, warn};

/// Be gentle with the (undocumented) usage endpoint.
const MIN_LIMITS_INTERVAL: u64 = 60;
const MIN_STATS_INTERVAL: u64 = 10;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Seconds between subscription-limit polls.
    pub limits_interval_secs: u64,
    /// Seconds between log rescans (file changes trigger sooner).
    pub stats_interval_secs: u64,
    /// Per-model price overrides in USD per million tokens, keyed by model-id prefix.
    pub pricing: BTreeMap<String, RateOverride>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateOverride {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
}

impl Default for Config {
    fn default() -> Self {
        Self { limits_interval_secs: 180, stats_interval_secs: 60, pricing: BTreeMap::new() }
    }
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("agent-bar/config.toml"))
    }

    pub fn load() -> Self {
        let Some(path) = Self::path() else { return Self::default() };
        match std::fs::read_to_string(&path) {
            Ok(text) => match Self::parse(&text) {
                Ok(c) => {
                    info!("loaded {}", path.display());
                    c
                }
                Err(e) => {
                    warn!("ignoring {}: {e}", path.display());
                    Self::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                warn!("cannot read {}: {e}", path.display());
                Self::default()
            }
        }
    }

    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    pub fn limits_interval(&self) -> Duration {
        Duration::from_secs(self.limits_interval_secs.max(MIN_LIMITS_INTERVAL))
    }

    pub fn stats_interval(&self) -> Duration {
        Duration::from_secs(self.stats_interval_secs.max(MIN_STATS_INTERVAL))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_default() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn parses_intervals_and_pricing() {
        let c = Config::parse(
            r#"
            limits_interval_secs = 300
            [pricing."claude-opus-5-5"]
            input = 3.5
            "#,
        )
        .unwrap();
        assert_eq!(c.limits_interval(), Duration::from_secs(300));
        assert_eq!(c.stats_interval(), Duration::from_secs(60));
        assert_eq!(c.pricing["claude-opus-5-5"], RateOverride { input: Some(3.5), ..Default::default() });
    }

    #[test]
    fn clamps_intervals() {
        let c = Config::parse("limits_interval_secs = 5\nstats_interval_secs = 1").unwrap();
        assert_eq!(c.limits_interval(), Duration::from_secs(60));
        assert_eq!(c.stats_interval(), Duration::from_secs(10));
    }

    #[test]
    fn example_config_is_valid() {
        let example = include_str!("../../packaging/config.example.toml");
        assert_eq!(Config::parse(example).unwrap(), Config::default());
        // Commented-out settings ("#key", not "# prose") must parse when uncommented.
        let uncommented: Vec<&str> = example
            .lines()
            .filter_map(|l| l.strip_prefix('#'))
            .filter(|l| !l.is_empty() && !l.starts_with(' '))
            .collect();
        let c = Config::parse(&uncommented.join("\n")).unwrap();
        assert_eq!(c.pricing["claude-opus-5-5"].output, Some(20.0));
    }

    #[test]
    fn rejects_typos() {
        assert!(Config::parse("limit_interval_secs = 300").is_err());
        assert!(Config::parse("[pricing.x]\ninptu = 1.0").is_err());
    }
}
