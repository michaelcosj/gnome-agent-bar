//! pi coding agent sessions: `~/.pi/agent/sessions/**/*.jsonl`.
//!
//! Assistant turns are `{"type":"message", "id", "timestamp", "message": {"role":
//! "assistant", "provider", "model", "usage": {input, output, cacheRead, cacheWrite}}}`
//! (input excludes cache reads). A message counts toward a subscription account only
//! if pi is signed in to that provider with OAuth (`auth.json` type "oauth").

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::tail::{JsonlTail, contains};
use super::{Account, UsageRecord, UsageSource, UsageStore};
use crate::model::Tokens;

pub const TOOL: &str = "pi";

pub struct PiSessions {
    agent_dir: PathBuf,
    tail: JsonlTail,
}

impl PiSessions {
    pub fn new(agent_dir: PathBuf) -> Self {
        Self { tail: JsonlTail::new(agent_dir.join("sessions")), agent_dir }
    }

    /// `$PI_CODING_AGENT_DIR` (pi's own override), else `~/.pi/agent`.
    pub fn default_agent_dir() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("PI_CODING_AGENT_DIR") {
            return Some(dir.into());
        }
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".pi/agent"))
    }

    /// Providers pi is signed in to with OAuth, mapped to accounts.
    fn oauth_accounts(&self) -> Vec<(String, Account)> {
        #[derive(Deserialize)]
        struct Cred {
            #[serde(rename = "type")]
            kind: Option<String>,
        }
        let Ok(bytes) = std::fs::read(self.agent_dir.join("auth.json")) else { return Vec::new() };
        let creds: std::collections::HashMap<String, Cred> = serde_json::from_slice(&bytes).unwrap_or_default();
        creds
            .into_iter()
            .filter(|(_, c)| c.kind.as_deref() == Some("oauth"))
            .filter_map(|(provider, _)| account_for(&provider).map(|a| (provider, a)))
            .collect()
    }
}

pub(super) fn account_for(provider: &str) -> Option<Account> {
    if provider.starts_with("openai") {
        Some(Account::ChatGpt)
    } else if provider.starts_with("anthropic") {
        Some(Account::Claude)
    } else {
        None
    }
}

impl UsageSource for PiSessions {
    fn scan(&mut self, store: &mut UsageStore, now: DateTime<Utc>) {
        let accounts = self.oauth_accounts();
        if accounts.is_empty() {
            return;
        }
        self.tail.scan(now, |path, line| {
            if let Some((key, rec)) = parse_line(path, line, &accounts) {
                store.upsert(key, rec);
            }
        });
    }

    fn watch_paths(&self) -> Vec<(PathBuf, bool)> {
        vec![(self.tail.root().to_owned(), true)]
    }
}

fn parse_line(path: &Path, line: &[u8], accounts: &[(String, Account)]) -> Option<(String, UsageRecord)> {
    if !contains(line, b"\"usage\"") {
        return None;
    }
    let l: Line = serde_json::from_slice(line).ok()?;
    let m = l.message?;
    if l.kind != "message" || m.role != "assistant" {
        return None;
    }
    let (provider, model, u) = (m.provider?, m.model?, m.usage?);
    let account = accounts.iter().find(|(p, _)| *p == provider)?.1;
    let tokens = Tokens {
        input: u.input,
        output: u.output,
        cache_read: u.cache_read,
        cache_write_5m: if account == Account::Claude { u.cache_write } else { 0 },
        cache_write_1h: 0,
    };
    if tokens.total() == 0 {
        return None; // failed request
    }
    let session = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let rec = UsageRecord { ts: l.timestamp?, account, tool: TOOL, model, session: session.clone(), tokens, fast: false };
    Some((format!("pi:{session}:{}", l.id?), rec))
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: String,
    id: Option<String>,
    timestamp: Option<DateTime<Utc>>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    role: String,
    provider: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Usage {
    #[serde(default)]
    input: u64,
    #[serde(default)]
    output: u64,
    #[serde(default)]
    cache_read: u64,
    #[serde(default)]
    cache_write: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::Pricing;

    fn msg(id: &str, provider: &str, model: &str, input: u64, output: u64, cache_read: u64) -> String {
        format!(
            r#"{{"type":"message","id":"{id}","parentId":null,"timestamp":"2026-10-11T10:00:00.000Z","message":{{"role":"assistant","content":[],"api":"openai-responses","provider":"{provider}","model":"{model}","usage":{{"input":{input},"output":{output},"cacheRead":{cache_read},"cacheWrite":0,"totalTokens":{},"cost":{{"total":0}}}},"stopReason":"stop"}}}}"#,
            input + output + cache_read
        ) + "\n"
    }

    #[test]
    fn attributes_oauth_providers_and_skips_others() {
        let now: DateTime<Utc> = "2026-10-11T12:00:00Z".parse().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("auth.json"),
            r#"{"openai":{"type":"oauth","access":"x"},"anthropic":{"type":"api_key","key":"x"}}"#,
        )
        .unwrap();
        let sessions = dir.path().join("sessions/--home--");
        std::fs::create_dir_all(&sessions).unwrap();
        let log = [
            r#"{"type":"session","version":3,"id":"s","timestamp":"2026-10-11T09:59:00Z","cwd":"/"}"#.to_string() + "\n",
            msg("a1", "openai", "gpt-6.1-sol", 1000, 100, 4000),
            msg("a2", "openai", "gpt-6.1-sol", 0, 0, 0), // errored request
            msg("a3", "anthropic", "claude-opus-5-5", 50, 5, 0), // API key: not a subscription
        ]
        .concat();
        std::fs::write(sessions.join("2026-10-11T09-59-00-000Z_s.jsonl"), log).unwrap();

        let mut store = UsageStore::default();
        PiSessions::new(dir.path().to_owned()).scan(&mut store, now);
        let s = &store.stats(Account::ChatGpt, now, &Pricing::default())["1d"];
        assert_eq!((s.requests, s.input, s.output, s.cache_read), (1, 1000, 100, 4000));
        assert_eq!(s.by_tool[0].name, TOOL);
        assert!(!store.has(Account::Claude));
    }
}
