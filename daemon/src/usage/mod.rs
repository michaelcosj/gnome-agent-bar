//! Token usage collected from local tool logs, attributed to subscription accounts.
//!
//! Each source (Claude Code, Codex CLI, pi, opencode) parses its own format into
//! `UsageRecord`s in one shared `UsageStore`; stats are then aggregated per account.
//! Usage that isn't on a subscription (API keys, free models) is not recorded.

pub mod claude_code;
pub mod codex;
pub mod opencode;
pub mod pi;
mod tail;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use chrono::{DateTime, Duration, DurationRound, Utc};
use serde::Serialize;

use crate::model::{HourBucket, NamedUsage, Tokens, UsageStats};
use crate::pricing::Pricing;

pub const RANGES: &[(&str, i64)] = &[("1d", 1), ("7d", 7), ("30d", 30)];
pub const HOURLY_BUCKETS: i64 = 24;
/// Keep a day beyond the longest range so a range never loses entries mid-scan.
pub const RETENTION_DAYS: i64 = 31;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum Account {
    Claude,
    ChatGpt,
}

#[derive(Debug, Clone)]
pub struct UsageRecord {
    pub ts: DateTime<Utc>,
    pub account: Account,
    pub tool: &'static str,
    pub model: String,
    pub session: String,
    pub tokens: Tokens,
    pub fast: bool,
}

pub trait UsageSource: Send + Sync {
    /// Ingest new data into the store. Cheap when nothing changed.
    fn scan(&mut self, store: &mut UsageStore, now: DateTime<Utc>);

    /// Paths whose changes should trigger a scan, and whether to watch recursively.
    fn watch_paths(&self) -> Vec<(PathBuf, bool)>;
}

#[derive(Default)]
pub struct UsageStore {
    records: HashMap<String, UsageRecord>,
}

impl UsageStore {
    /// Records are keyed per source; a repeated key keeps the field-wise max tokens
    /// (sources log streaming partials and duplicates).
    pub fn upsert(&mut self, key: String, rec: UsageRecord) {
        self.records.entry(key).and_modify(|e| e.tokens = e.tokens.max(rec.tokens)).or_insert(rec);
    }

    pub fn prune(&mut self, now: DateTime<Utc>) {
        let cutoff = now - Duration::days(RETENTION_DAYS);
        self.records.retain(|_, r| r.ts >= cutoff);
    }

    pub fn has(&self, account: Account) -> bool {
        self.records.values().any(|r| r.account == account)
    }

    fn of(&self, account: Account) -> impl Iterator<Item = &UsageRecord> {
        self.records.values().filter(move |r| r.account == account)
    }

    pub fn stats(&self, account: Account, now: DateTime<Utc>, pricing: &Pricing) -> BTreeMap<String, UsageStats> {
        RANGES
            .iter()
            .map(|&(id, days)| (id.to_owned(), self.range_stats(account, now - Duration::days(days), now, pricing)))
            .collect()
    }

    fn range_stats(&self, account: Account, from: DateTime<Utc>, to: DateTime<Utc>, pricing: &Pricing) -> UsageStats {
        let mut s = UsageStats::default();
        let mut sessions = HashSet::new();
        let mut by_model: HashMap<&str, NamedUsage> = HashMap::new();
        let mut by_tool: HashMap<&str, NamedUsage> = HashMap::new();

        for r in self.of(account).filter(|r| r.ts > from && r.ts <= to) {
            let t = &r.tokens;
            let cost = pricing.cost_usd(&r.model, t, r.fast);
            s.input += t.input;
            s.output += t.output;
            s.cache_read += t.cache_read;
            s.cache_write += t.cache_write();
            s.cost_usd += cost;
            s.requests += 1;
            sessions.insert((r.tool, r.session.as_str()));
            for (map, name) in [(&mut by_model, r.model.as_str()), (&mut by_tool, r.tool)] {
                let u = map.entry(name).or_insert_with(|| NamedUsage { name: name.to_owned(), ..Default::default() });
                u.total_tokens += t.total();
                u.cost_usd += cost;
                u.requests += 1;
            }
        }

        s.total_tokens = s.input + s.output + s.cache_read + s.cache_write;
        s.sessions = sessions.len() as u64;
        s.by_model = sorted(by_model);
        s.by_tool = sorted(by_tool);
        s
    }

    pub fn hourly(&self, account: Account, now: DateTime<Utc>, pricing: &Pricing) -> Vec<HourBucket> {
        let current = now.duration_trunc(Duration::hours(1)).unwrap_or(now);
        let first = current - Duration::hours(HOURLY_BUCKETS - 1);
        let mut buckets: Vec<HourBucket> = (0..HOURLY_BUCKETS)
            .map(|i| HourBucket { start: first + Duration::hours(i), total_tokens: 0, cost_usd: 0.0 })
            .collect();
        for r in self.of(account).filter(|r| r.ts >= first && r.ts <= now) {
            let b = &mut buckets[(r.ts - first).num_hours() as usize];
            b.total_tokens += r.tokens.total();
            b.cost_usd += pricing.cost_usd(&r.model, &r.tokens, r.fast);
        }
        buckets
    }
}

fn sorted(map: HashMap<&str, NamedUsage>) -> Vec<NamedUsage> {
    let mut v: Vec<NamedUsage> = map.into_values().collect();
    v.sort_by(|a, b| b.cost_usd.total_cmp(&a.cost_usd).then(b.total_tokens.cmp(&a.total_tokens)));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(ts: &str, account: Account, tool: &'static str, input: u64) -> UsageRecord {
        UsageRecord {
            ts: ts.parse().unwrap(),
            account,
            tool,
            model: "claude-opus-5-5".into(),
            session: "s".into(),
            tokens: Tokens { input, ..Default::default() },
            fast: false,
        }
    }

    #[test]
    fn aggregates_per_account_and_tool() {
        let now: DateTime<Utc> = "2026-10-11T12:00:00Z".parse().unwrap();
        let mut store = UsageStore::default();
        store.upsert("a".into(), rec("2026-10-11T10:00:00Z", Account::Claude, "Claude Code", 100));
        store.upsert("b".into(), rec("2026-10-11T11:00:00Z", Account::Claude, "pi", 300));
        store.upsert("b".into(), rec("2026-10-11T11:00:00Z", Account::Claude, "pi", 200)); // dup: max kept
        store.upsert("c".into(), rec("2026-10-11T11:00:00Z", Account::ChatGpt, "pi", 50));
        let s = &store.stats(Account::Claude, now, &Pricing::default())["1d"];
        assert_eq!((s.input, s.requests, s.sessions), (400, 2, 2));
        assert_eq!(s.by_tool.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["pi", "Claude Code"]);
        assert!(store.has(Account::ChatGpt));
        assert_eq!(store.stats(Account::ChatGpt, now, &Pricing::default())["1d"].input, 50);

        store.prune("2026-11-20T00:00:00Z".parse().unwrap());
        assert!(!store.has(Account::Claude));
    }
}
