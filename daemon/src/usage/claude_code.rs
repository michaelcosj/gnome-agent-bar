//! Claude Code session logs: `<config>/projects/**/*.jsonl` (incl. subagent logs).
//!
//! Each API response is written once per content block, and the copies can hold
//! streaming partials (e.g. `output_tokens` 4 vs 168), so records are keyed by
//! `(message.id, requestId)` and the store keeps the field-wise max.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::tail::{JsonlTail, contains};
use super::{Account, UsageRecord, UsageSource, UsageStore};
use crate::model::Tokens;

pub const TOOL: &str = "Claude Code";

pub struct ClaudeCodeLogs {
    tail: JsonlTail,
}

impl ClaudeCodeLogs {
    /// `config_dir` is Claude Code's config dir (`~/.claude`).
    pub fn new(config_dir: &Path) -> Self {
        Self { tail: JsonlTail::new(config_dir.join("projects")) }
    }
}

impl UsageSource for ClaudeCodeLogs {
    fn scan(&mut self, store: &mut UsageStore, now: DateTime<Utc>) {
        self.tail.scan(now, |_, line| {
            if let Some((key, rec)) = parse_line(line) {
                store.upsert(key, rec);
            }
        });
    }

    fn watch_paths(&self) -> Vec<(PathBuf, bool)> {
        vec![(self.tail.root().to_owned(), true)]
    }
}

fn parse_line(line: &[u8]) -> Option<(String, UsageRecord)> {
    // Most lines (attachments, user turns) have no usage.
    if !contains(line, b"\"usage\"") {
        return None;
    }
    let l: LogLine = serde_json::from_slice(line).ok()?;
    if l.kind.as_deref() != Some("assistant") {
        return None;
    }
    let msg = l.message?;
    let (id, model, u) = (msg.id?, msg.model?, msg.usage?);
    if model.starts_with('<') {
        return None; // "<synthetic>" placeholder messages
    }
    let (w5m, w1h) = match u.cache_creation {
        Some(c) => (c.ephemeral_5m_input_tokens, c.ephemeral_1h_input_tokens),
        None => (u.cache_creation_input_tokens, 0),
    };
    let rec = UsageRecord {
        ts: l.timestamp?,
        account: Account::Claude,
        tool: TOOL,
        model,
        session: l.session_id.unwrap_or_default(),
        tokens: Tokens {
            input: u.input_tokens,
            output: u.output_tokens,
            cache_read: u.cache_read_input_tokens,
            cache_write_5m: w5m,
            cache_write_1h: w1h,
        },
        fast: u.speed.as_deref() == Some("fast"),
    };
    Some((format!("claude-code:{id}:{}", l.request_id.unwrap_or_default()), rec))
}

#[derive(Deserialize)]
struct LogLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<DateTime<Utc>>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    message: Option<LogMessage>,
}

#[derive(Deserialize)]
struct LogMessage {
    id: Option<String>,
    model: Option<String>,
    usage: Option<LogUsage>,
}

#[derive(Deserialize)]
struct LogUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    cache_creation: Option<CacheCreation>,
    speed: Option<String>,
}

#[derive(Deserialize)]
struct CacheCreation {
    #[serde(default)]
    ephemeral_5m_input_tokens: u64,
    #[serde(default)]
    ephemeral_1h_input_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::Pricing;
    use std::io::Write;

    fn line(id: &str, req: &str, ts: &str, session: &str, input: u64, output: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","sessionId":"{session}","requestId":"{req}","message":{{"id":"{id}","model":"claude-opus-5-5","content":[],"usage":{{"input_tokens":{input},"output_tokens":{output},"cache_read_input_tokens":0,"cache_creation_input_tokens":1000,"cache_creation":{{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":1000}}}}}}}}"#
        ) + "\n"
    }

    #[test]
    fn dedups_streaming_partials_and_buckets_ranges() {
        let now: DateTime<Utc> = "2026-10-11T12:00:00Z".parse().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("projects/proj/session/subagents");
        std::fs::create_dir_all(&sub).unwrap();
        let mut f = std::fs::File::create(dir.path().join("projects/proj/a.jsonl")).unwrap();
        // Same response logged twice; second copy has the final output count.
        f.write_all(line("m1", "r1", "2026-10-11T10:00:00Z", "s1", 10, 4).as_bytes()).unwrap();
        f.write_all(line("m1", "r1", "2026-10-11T10:00:00Z", "s1", 10, 168).as_bytes()).unwrap();
        f.write_all(b"{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n").unwrap();
        // 3 days ago: in 7d and 30d, not 1d.
        f.write_all(line("m2", "r2", "2026-10-08T10:00:00Z", "s2", 20, 30).as_bytes()).unwrap();
        // Subagent file, 20 days ago.
        std::fs::write(sub.join("agent-x.jsonl"), line("m3", "r3", "2026-09-21T10:00:00Z", "s2", 5, 5)).unwrap();

        let mut store = UsageStore::default();
        ClaudeCodeLogs::new(dir.path()).scan(&mut store, now);
        let p = Pricing::default();
        let stats = store.stats(Account::Claude, now, &p);

        let d1 = &stats["1d"];
        assert_eq!((d1.requests, d1.input, d1.output, d1.cache_write), (1, 10, 168, 1000));
        // 10*4 + 168*20 + 1000*4*2 (1h cache write) per MTok
        assert!((d1.cost_usd - (40.0 + 3360.0 + 8000.0) / 1e6).abs() < 1e-12);
        assert_eq!(d1.by_tool[0].name, TOOL);
        assert_eq!(stats["7d"].requests, 2);
        assert_eq!((stats["30d"].requests, stats["30d"].sessions), (3, 2));

        // now = 12:00 → buckets 13:00 yesterday .. 12:00 today; m1 at 10:00 is index 21.
        let hourly = store.hourly(Account::Claude, now, &p);
        assert_eq!(hourly.len(), 24);
        assert_eq!(hourly[0].start, "2026-10-10T13:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert_eq!(hourly[21].total_tokens, 10 + 168 + 1000);
        assert_eq!(hourly.iter().filter(|b| b.total_tokens > 0).count(), 1);
    }
}
