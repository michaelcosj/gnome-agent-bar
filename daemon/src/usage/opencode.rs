//! opencode's SQLite store: `$XDG_DATA_HOME/opencode/opencode.db` (default `~/.local/share`).
//!
//! Assistant rows in `session_message` hold JSON with `time.created` (ms),
//! `model.{providerID,id}`, `tokens.{input, output, reasoning, cache.{read,write}}`
//! (input excludes cache) and `cost`. opencode reports cost 0 for subscription
//! (OAuth) usage, which is how we tell it apart from API-key usage.
//!
//! Opened read-only (WAL allows reading while opencode writes; SQLite still needs to
//! update the `-shm` file, so the directory must be writable for the service).
//! Rows are read incrementally by `time_updated`, since tokens are filled in when a
//! streamed message completes.

use std::path::PathBuf;

use chrono::{DateTime, Duration, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags, params};
use serde::Deserialize;
use tracing::{debug, warn};

use super::pi::account_for;
use super::{Account, RETENTION_DAYS, UsageRecord, UsageSource, UsageStore};
use crate::model::Tokens;

pub const TOOL: &str = "opencode";

pub struct OpencodeDb {
    path: PathBuf,
    /// Highest `time_updated` seen (ms).
    cursor: i64,
    warned: bool,
}

impl OpencodeDb {
    pub fn new(path: PathBuf) -> Self {
        Self { path, cursor: 0, warned: false }
    }

    pub fn default_path() -> Option<PathBuf> {
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
        Some(data.join("opencode/opencode.db"))
    }

    fn read(&mut self, store: &mut UsageStore, now: DateTime<Utc>) -> rusqlite::Result<usize> {
        let conn = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(1))?;
        let since = (now - Duration::days(RETENTION_DAYS)).timestamp_millis();
        let mut stmt = conn.prepare_cached(
            "SELECT id, session_id, time_updated, data FROM session_message
             WHERE type = 'assistant' AND time_updated >= ?1 AND time_created >= ?2
             ORDER BY time_updated",
        )?;
        let mut rows = stmt.query(params![self.cursor, since])?;
        let mut n = 0;
        while let Some(row) = rows.next()? {
            let (id, session, updated, data): (String, String, i64, String) =
                (row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?);
            self.cursor = self.cursor.max(updated);
            if let Some(rec) = parse_message(&data, session) {
                store.upsert(format!("opencode:{id}"), rec);
                n += 1;
            }
        }
        Ok(n)
    }
}

impl UsageSource for OpencodeDb {
    fn scan(&mut self, store: &mut UsageStore, now: DateTime<Utc>) {
        if !self.path.exists() {
            return;
        }
        match self.read(store, now) {
            Ok(n) => {
                self.warned = false;
                debug!(rows = n, "opencode scanned");
            }
            Err(e) if !self.warned => {
                warn!("reading {}: {e}", self.path.display());
                self.warned = true;
            }
            Err(_) => {}
        }
    }

    fn watch_paths(&self) -> Vec<(PathBuf, bool)> {
        // Non-recursive: the dir also holds busy snapshot/ and repos/ trees.
        self.path.parent().map(|d| vec![(d.to_owned(), false)]).unwrap_or_default()
    }
}

fn parse_message(data: &str, session: String) -> Option<UsageRecord> {
    let m: Message = serde_json::from_str(data).ok()?;
    if m.cost.unwrap_or(0.0) != 0.0 {
        return None; // billed per token: API key, not a subscription
    }
    let account = account_for(&m.model.provider_id)?;
    let t = m.tokens?;
    let tokens = Tokens {
        input: t.input,
        output: t.output + t.reasoning,
        cache_read: t.cache.read,
        cache_write_5m: if account == Account::Claude { t.cache.write } else { 0 },
        cache_write_1h: 0,
    };
    if tokens.total() == 0 {
        return None; // errored or still streaming
    }
    Some(UsageRecord {
        ts: Utc.timestamp_millis_opt(m.time.created).single()?,
        account,
        tool: TOOL,
        model: m.model.id,
        session,
        tokens,
        fast: false,
    })
}

#[derive(Deserialize)]
struct Message {
    time: Time,
    model: Model,
    cost: Option<f64>,
    tokens: Option<RawTokens>,
}

#[derive(Deserialize)]
struct Time {
    created: i64,
}

#[derive(Deserialize)]
struct Model {
    id: String,
    #[serde(rename = "providerID")]
    provider_id: String,
}

#[derive(Deserialize)]
struct RawTokens {
    #[serde(default)]
    input: u64,
    #[serde(default)]
    output: u64,
    #[serde(default)]
    reasoning: u64,
    #[serde(default)]
    cache: Cache,
}

#[derive(Deserialize, Default)]
struct Cache {
    #[serde(default)]
    read: u64,
    #[serde(default)]
    write: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::Pricing;

    fn row(conn: &Connection, id: &str, updated: i64, data: &str) {
        conn.execute(
            "INSERT OR REPLACE INTO session_message (id, session_id, type, seq, time_created, time_updated, data)
             VALUES (?1, 'ses_1', 'assistant', 0, ?2, ?2, ?3)",
            params![id, updated, data],
        )
        .unwrap();
    }

    /// `input == 0` stands for a message still streaming: all counts zero.
    fn msg(created: i64, provider: &str, model: &str, cost: f64, input: u64, output: u64) -> String {
        let (reasoning, cache_read) = if input == 0 { (0, 0) } else { (7, 900) };
        format!(
            r#"{{"time":{{"created":{created}}},"model":{{"id":"{model}","providerID":"{provider}"}},"finish":"stop","cost":{cost},"tokens":{{"input":{input},"output":{output},"reasoning":{reasoning},"cache":{{"read":{cache_read},"write":0}}}}}}"#
        )
    }

    #[test]
    fn reads_subscription_rows_incrementally() {
        let now: DateTime<Utc> = "2026-10-11T12:00:00Z".parse().unwrap();
        let t = "2026-10-11T10:00:00Z".parse::<DateTime<Utc>>().unwrap().timestamp_millis();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER,
               time_created INTEGER, time_updated INTEGER, data TEXT);",
        )
        .unwrap();
        row(&conn, "m1", t, &msg(t, "openai", "gpt-6.1-sol", 0.0, 0, 0)); // still streaming
        row(&conn, "m2", t, &msg(t, "openai", "gpt-6.1-sol", 0.0, 100, 10));
        row(&conn, "m3", t, &msg(t, "openai", "gpt-6.1-sol", 0.42, 100, 10)); // API key
        row(&conn, "m4", t, &msg(t, "opencode", "nemotron-free", 0.0, 100, 10)); // not a subscription

        let mut db = OpencodeDb::new(path.clone());
        let mut store = UsageStore::default();
        db.scan(&mut store, now);
        let p = Pricing::default();
        let s = &store.stats(Account::ChatGpt, now, &p)["1d"];
        assert_eq!((s.requests, s.input, s.output, s.cache_read), (1, 100, 17, 900));

        // m1 completes later: picked up via time_updated.
        row(&conn, "m1", t + 5000, &msg(t, "openai", "gpt-6.1-sol", 0.0, 200, 20));
        db.scan(&mut store, now);
        let s = &store.stats(Account::ChatGpt, now, &p)["1d"];
        assert_eq!((s.requests, s.input), (2, 300));
        assert!(!store.has(Account::Claude));
    }
}
