//! Token usage from Claude Code's session logs (`<config>/projects/**/*.jsonl`).
//!
//! Each API response is written once per content block, and the copies can hold
//! streaming partials (e.g. `output_tokens` 4 vs 168), so entries are keyed by
//! `(message.id, requestId)` and merged with a field-wise max.
//!
//! Files are tailed by byte offset, so a rescan only reads appended data.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use tracing::{debug, warn};

use crate::model::{ModelUsage, Tokens, UsageStats};
use crate::pricing;

pub const RANGES: &[(&str, i64)] = &[("1d", 1), ("7d", 7), ("30d", 30)];
/// Keep a day beyond the longest range so a range never loses entries mid-scan.
const RETENTION_DAYS: i64 = 31;

#[derive(Debug, Clone)]
struct Entry {
    ts: DateTime<Utc>,
    model: String,
    session: String,
    tokens: Tokens,
    fast: bool,
}

pub struct LogIndex {
    root: PathBuf,
    /// Bytes consumed per file (always at a line boundary).
    offsets: HashMap<PathBuf, u64>,
    entries: HashMap<(String, String), Entry>,
}

impl LogIndex {
    pub fn new(root: PathBuf) -> Self {
        Self { root, offsets: HashMap::new(), entries: HashMap::new() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn scan(&mut self, now: DateTime<Utc>) {
        let cutoff = now - Duration::days(RETENTION_DAYS);
        let cutoff_sys = SystemTime::from(cutoff);
        let mut files = Vec::new();
        collect_jsonl(&self.root, &mut files);

        for path in files {
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            let offset = self.offsets.get(&path).copied();
            // Skip untouched old files entirely on first sight.
            if offset.is_none() && meta.modified().is_ok_and(|m| m < cutoff_sys) {
                continue;
            }
            let mut offset = offset.unwrap_or(0);
            if meta.len() < offset {
                offset = 0; // truncated or replaced; dedup makes re-reading safe
            }
            if meta.len() > offset {
                match self.ingest_from(&path, offset) {
                    Ok(consumed) => offset += consumed,
                    Err(e) => warn!("reading {}: {e}", path.display()),
                }
            }
            self.offsets.insert(path, offset);
        }

        self.entries.retain(|_, e| e.ts >= cutoff);
        self.offsets.retain(|p, _| p.exists());
        debug!(entries = self.entries.len(), files = self.offsets.len(), "claude logs scanned");
    }

    /// Ingests complete lines after `offset`; returns bytes consumed.
    fn ingest_from(&mut self, path: &Path, offset: u64) -> std::io::Result<u64> {
        let mut f = File::open(path)?;
        f.seek(SeekFrom::Start(offset))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        // A trailing partial line is still being written; pick it up next scan.
        let Some(end) = buf.iter().rposition(|&b| b == b'\n') else { return Ok(0) };
        for line in buf[..end].split(|&b| b == b'\n') {
            self.ingest_line(line);
        }
        Ok(end as u64 + 1)
    }

    fn ingest_line(&mut self, line: &[u8]) {
        // Cheap prefilter: most lines (attachments, user turns) have no usage.
        if !contains(line, b"\"usage\"") {
            return;
        }
        let Ok(l) = serde_json::from_slice::<LogLine>(line) else { return };
        if l.kind.as_deref() != Some("assistant") {
            return;
        }
        let (Some(ts), Some(msg)) = (l.timestamp, l.message) else { return };
        let (Some(id), Some(model), Some(u)) = (msg.id, msg.model, msg.usage) else { return };
        if model.starts_with('<') {
            return; // "<synthetic>" placeholder messages
        }

        let (w5m, w1h) = match u.cache_creation {
            Some(c) => (c.ephemeral_5m_input_tokens, c.ephemeral_1h_input_tokens),
            None => (u.cache_creation_input_tokens, 0),
        };
        let tokens = Tokens {
            input: u.input_tokens,
            output: u.output_tokens,
            cache_read: u.cache_read_input_tokens,
            cache_write_5m: w5m,
            cache_write_1h: w1h,
        };
        let entry = Entry {
            ts,
            model,
            session: l.session_id.unwrap_or_default(),
            tokens,
            fast: u.speed.as_deref() == Some("fast"),
        };
        self.entries
            .entry((id, l.request_id.unwrap_or_default()))
            .and_modify(|e| e.tokens = e.tokens.max(tokens))
            .or_insert(entry);
    }

    pub fn stats(&self, now: DateTime<Utc>) -> BTreeMap<String, UsageStats> {
        RANGES
            .iter()
            .map(|&(id, days)| (id.to_owned(), self.range_stats(now - Duration::days(days), now)))
            .collect()
    }

    fn range_stats(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> UsageStats {
        let mut s = UsageStats::default();
        let mut sessions = HashSet::new();
        let mut by_model: HashMap<&str, ModelUsage> = HashMap::new();

        for e in self.entries.values().filter(|e| e.ts > from && e.ts <= to) {
            let t = &e.tokens;
            let cost = pricing::cost_usd(&e.model, t, e.fast);
            s.input += t.input;
            s.output += t.output;
            s.cache_read += t.cache_read;
            s.cache_write += t.cache_write();
            s.cost_usd += cost;
            s.requests += 1;
            sessions.insert(e.session.as_str());

            let m = by_model.entry(&e.model).or_insert_with(|| ModelUsage { model: e.model.clone(), ..Default::default() });
            m.total_tokens += t.total();
            m.cost_usd += cost;
            m.requests += 1;
        }

        s.total_tokens = s.input + s.output + s.cache_read + s.cache_write;
        s.sessions = sessions.len() as u64;
        s.by_model = by_model.into_values().collect();
        s.by_model.sort_by(|a, b| b.cost_usd.total_cmp(&a.cost_usd));
        s
    }
}

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => collect_jsonl(&path, out),
            Ok(ft) if ft.is_file() && path.extension().is_some_and(|e| e == "jsonl") => out.push(path),
            _ => {}
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
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
    use std::io::Write;

    fn line(id: &str, req: &str, ts: &str, session: &str, input: u64, output: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","sessionId":"{session}","requestId":"{req}","message":{{"id":"{id}","model":"claude-opus-5-5","content":[],"usage":{{"input_tokens":{input},"output_tokens":{output},"cache_read_input_tokens":0,"cache_creation_input_tokens":1000,"cache_creation":{{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":1000}}}}}}}}"#
        ) + "\n"
    }

    fn now() -> DateTime<Utc> {
        "2026-10-11T12:00:00Z".parse().unwrap()
    }

    #[test]
    fn dedups_streaming_partials_and_buckets_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("proj/session/subagents");
        std::fs::create_dir_all(&sub).unwrap();
        let mut f = File::create(dir.path().join("proj/a.jsonl")).unwrap();
        // Same response logged twice; second copy has the final output count.
        f.write_all(line("m1", "r1", "2026-10-11T10:00:00Z", "s1", 10, 4).as_bytes()).unwrap();
        f.write_all(line("m1", "r1", "2026-10-11T10:00:00Z", "s1", 10, 168).as_bytes()).unwrap();
        f.write_all(br#"{"type":"user","message":{"content":"hi"}}"#).unwrap();
        f.write_all(b"\n").unwrap();
        // 3 days ago: in 7d and 30d, not 1d.
        f.write_all(line("m2", "r2", "2026-10-08T10:00:00Z", "s2", 20, 30).as_bytes()).unwrap();
        // Subagent file, 20 days ago.
        std::fs::write(sub.join("agent-x.jsonl"), line("m3", "r3", "2026-09-21T10:00:00Z", "s2", 5, 5)).unwrap();

        let mut idx = LogIndex::new(dir.path().to_owned());
        idx.scan(now());
        let stats = idx.stats(now());

        let d1 = &stats["1d"];
        assert_eq!((d1.requests, d1.input, d1.output, d1.cache_write), (1, 10, 168, 1000));
        assert_eq!(d1.sessions, 1);
        // 10*4 + 168*20 + 1000*4*2 (1h cache write) per MTok
        assert!((d1.cost_usd - (40.0 + 3360.0 + 8000.0) / 1e6).abs() < 1e-12);

        assert_eq!(stats["7d"].requests, 2);
        assert_eq!(stats["30d"].requests, 3);
        assert_eq!(stats["30d"].sessions, 2);
        assert_eq!(stats["30d"].by_model.len(), 1);
    }

    #[test]
    fn tails_appended_lines_and_waits_for_partial_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        std::fs::write(&path, line("m1", "r1", "2026-10-11T10:00:00Z", "s1", 1, 1)).unwrap();

        let mut idx = LogIndex::new(dir.path().to_owned());
        idx.scan(now());
        assert_eq!(idx.stats(now())["1d"].requests, 1);

        // Half-written line: not consumed yet.
        let full = line("m2", "r2", "2026-10-11T11:00:00Z", "s1", 1, 1);
        let (head, tail) = full.split_at(40);
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(head.as_bytes()).unwrap();
        idx.scan(now());
        assert_eq!(idx.stats(now())["1d"].requests, 1);

        f.write_all(tail.as_bytes()).unwrap();
        idx.scan(now());
        assert_eq!(idx.stats(now())["1d"].requests, 2);
    }
}
