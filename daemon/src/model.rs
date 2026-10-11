//! Snapshot schema shared with the GNOME Shell extension (serialized as JSON over D-Bus).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Snapshot {
    pub version: u32,
    pub updated_at: DateTime<Utc>,
    pub providers: Vec<ProviderSnapshot>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    /// Showing last-known data (e.g. expired token, rate limited).
    Stale,
    Error,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProviderSnapshot {
    pub id: String,
    pub name: String,
    pub plan: Option<String>,
    pub status: Status,
    pub error: Option<String>,
    pub limits_updated_at: Option<DateTime<Utc>>,
    pub limits: Vec<LimitWindow>,
    pub breakdown: Vec<BreakdownRow>,
    /// Keyed by range id: "1d", "7d", "30d" (rolling windows ending now).
    pub stats: BTreeMap<String, UsageStats>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LimitWindow {
    pub id: String,
    pub label: String,
    pub used_pct: f64,
    /// Server-provided band ("normal", "warning", ...), if any.
    pub severity: Option<String>,
    pub resets_at: Option<DateTime<Utc>>,
    pub is_active: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BreakdownRow {
    pub label: String,
    pub pct: f64,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct UsageStats {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total_tokens: u64,
    pub cost_usd: f64,
    pub requests: u64,
    pub sessions: u64,
    pub by_model: Vec<ModelUsage>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ModelUsage {
    pub model: String,
    pub total_tokens: u64,
    pub cost_usd: f64,
    pub requests: u64,
}

/// Token counts for a single API response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
}

impl Tokens {
    pub fn cache_write(&self) -> u64 {
        self.cache_write_5m + self.cache_write_1h
    }

    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write()
    }

    /// Everything sent to the model, used for prompt-size pricing tiers.
    pub fn prompt(&self) -> u64 {
        self.input + self.cache_read + self.cache_write()
    }

    /// Field-wise max: duplicated log lines carry streaming partials, the largest is final.
    pub fn max(self, o: Tokens) -> Tokens {
        Tokens {
            input: self.input.max(o.input),
            output: self.output.max(o.output),
            cache_read: self.cache_read.max(o.cache_read),
            cache_write_5m: self.cache_write_5m.max(o.cache_write_5m),
            cache_write_1h: self.cache_write_1h.max(o.cache_write_1h),
        }
    }
}
