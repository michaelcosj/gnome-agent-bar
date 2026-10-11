//! Codex CLI rollout logs: `$CODEX_HOME/sessions/**/rollout-*.jsonl` (default `~/.codex`).
//!
//! Lines are `{"timestamp", "type", "payload"}`. `turn_context` carries the model;
//! `event_msg` with `payload.type == "token_count"` carries cumulative
//! `info.total_token_usage` for the session. We record the delta between
//! consecutive totals (repeated totals are skipped), keyed by the running total so
//! re-reads are idempotent. OpenAI's `input_tokens` includes cached tokens.
//!
//! Only counted when Codex is signed in with ChatGPT (subscription), not an API key.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::tail::{JsonlTail, contains};
use super::{Account, UsageRecord, UsageSource, UsageStore};
use crate::model::Tokens;

pub const TOOL: &str = "Codex";

pub struct CodexLogs {
    home: PathBuf,
    tail: JsonlTail,
    files: HashMap<PathBuf, FileState>,
}

#[derive(Default)]
struct FileState {
    model: Option<String>,
    last: Option<RawUsage>,
}

impl CodexLogs {
    pub fn new(home: PathBuf) -> Self {
        Self { tail: JsonlTail::new(home.join("sessions")), home, files: HashMap::new() }
    }

    /// `$CODEX_HOME`, else `~/.codex`.
    pub fn default_home() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("CODEX_HOME") {
            return Some(dir.into());
        }
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex"))
    }

    fn uses_chatgpt(&self) -> bool {
        #[derive(Deserialize)]
        struct Auth {
            auth_mode: Option<String>,
        }
        std::fs::read(self.home.join("auth.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Auth>(&b).ok())
            .and_then(|a| a.auth_mode)
            .is_some_and(|m| m.eq_ignore_ascii_case("chatgpt"))
    }
}

impl UsageSource for CodexLogs {
    fn scan(&mut self, store: &mut UsageStore, now: DateTime<Utc>) {
        if !self.uses_chatgpt() {
            return;
        }
        let files = &mut self.files;
        self.tail.scan(now, |path, line| {
            let state = files.entry(path.to_owned()).or_default();
            if let Some((key, rec)) = parse_line(path, line, state) {
                store.upsert(key, rec);
            }
        });
        files.retain(|p, _| p.exists());
    }

    fn watch_paths(&self) -> Vec<(PathBuf, bool)> {
        vec![(self.tail.root().to_owned(), true)]
    }
}

fn parse_line(path: &Path, line: &[u8], state: &mut FileState) -> Option<(String, UsageRecord)> {
    if !contains(line, b"turn_context") && !contains(line, b"token_count") {
        return None;
    }
    let l: Line = serde_json::from_slice(line).ok()?;
    match l.kind.as_str() {
        "turn_context" => {
            state.model = l.payload.model;
            None
        }
        "event_msg" if l.payload.kind.as_deref() == Some("token_count") => {
            let total = l.payload.info?.total_token_usage;
            let prev = state.last.replace(total).unwrap_or_default();
            if total.total_tokens <= prev.total_tokens {
                return None; // repeated snapshot or a reset
            }
            let d = total.minus(&prev);
            let cached = d.cached_input_tokens.min(d.input_tokens);
            let rec = UsageRecord {
                ts: l.timestamp?,
                account: Account::ChatGpt,
                tool: TOOL,
                model: state.model.clone().unwrap_or_else(|| "unknown".into()),
                session: path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
                // Cache writes are part of input_tokens and billed as input by OpenAI.
                tokens: Tokens {
                    input: d.input_tokens - cached,
                    output: d.output_tokens,
                    cache_read: cached,
                    ..Default::default()
                },
                fast: false,
            };
            Some((format!("codex:{}:{}", path.display(), total.total_tokens), rec))
        }
        _ => None,
    }
}

#[derive(Deserialize)]
struct Line {
    timestamp: Option<DateTime<Utc>>,
    #[serde(rename = "type")]
    kind: String,
    payload: Payload,
}

#[derive(Deserialize)]
struct Payload {
    #[serde(rename = "type")]
    kind: Option<String>,
    model: Option<String>,
    info: Option<Info>,
}

#[derive(Deserialize)]
struct Info {
    total_token_usage: RawUsage,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
struct RawUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
}

impl RawUsage {
    fn minus(&self, o: &RawUsage) -> RawUsage {
        RawUsage {
            input_tokens: self.input_tokens.saturating_sub(o.input_tokens),
            cached_input_tokens: self.cached_input_tokens.saturating_sub(o.cached_input_tokens),
            output_tokens: self.output_tokens.saturating_sub(o.output_tokens),
            total_tokens: self.total_tokens.saturating_sub(o.total_tokens),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::Pricing;

    fn token_count(ts: &str, input: u64, cached: u64, output: u64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"output_tokens":{output},"reasoning_output_tokens":0,"total_tokens":{}}},"last_token_usage":{{}},"model_context_window":272000}},"rate_limits":null}}}}"#,
            input + output
        ) + "\n"
    }

    #[test]
    fn records_deltas_between_cumulative_totals() {
        let now: DateTime<Utc> = "2026-10-11T12:00:00Z".parse().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("auth.json"), r#"{"auth_mode":"chatgpt"}"#).unwrap();
        let day = home.path().join("sessions/2026/10/11");
        std::fs::create_dir_all(&day).unwrap();
        let log = [
            r#"{"timestamp":"2026-10-11T10:00:00Z","type":"session_meta","payload":{"id":"x"}}"#.to_string() + "\n",
            r#"{"timestamp":"2026-10-11T10:00:01Z","type":"turn_context","payload":{"model":"gpt-6.1-sol","cwd":"/"}}"#.to_string() + "\n",
            token_count("2026-10-11T10:00:05Z", 1000, 0, 100),
            token_count("2026-10-11T10:00:06Z", 1000, 0, 100), // repeated snapshot
            token_count("2026-10-11T10:05:00Z", 3000, 1500, 300),
        ]
        .concat();
        std::fs::write(day.join("rollout-2026-10-11T10-00-00-x.jsonl"), log).unwrap();

        let mut store = UsageStore::default();
        let mut codex = CodexLogs::new(home.path().to_owned());
        codex.scan(&mut store, now);
        codex.scan(&mut store, now); // idempotent
        let s = &store.stats(Account::ChatGpt, now, &Pricing::default())["1d"];
        assert_eq!(s.requests, 2);
        assert_eq!((s.input, s.cache_read, s.output), (1000 + 500, 1500, 300));
        assert_eq!(s.by_model[0].name, "gpt-6.1-sol");
        // gpt-6.1-sol: $2 in, $0.10 cached, $10 out per MTok
        assert!((s.cost_usd - (1500.0 * 2.0 + 1500.0 * 0.10 + 300.0 * 10.0) / 1e6).abs() < 1e-12);
    }

    #[test]
    fn ignores_api_key_mode() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("auth.json"), r#"{"auth_mode":"apikey"}"#).unwrap();
        let day = home.path().join("sessions");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("rollout-x.jsonl"), token_count("2026-10-11T10:00:05Z", 1000, 0, 100)).unwrap();
        let mut store = UsageStore::default();
        CodexLogs::new(home.path().to_owned()).scan(&mut store, "2026-10-11T12:00:00Z".parse().unwrap());
        assert!(!store.has(Account::ChatGpt));
    }
}
