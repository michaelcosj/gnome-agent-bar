mod config;
mod dbus;
mod limits;
mod model;
mod pricing;
mod usage;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Context;
use chrono::Utc;
use notify::{EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval, sleep};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::limits::{Limits, chatgpt::ChatGptApi, claude::ClaudeApi};
use crate::model::{ProviderSnapshot, SCHEMA_VERSION, Snapshot};
use crate::pricing::Pricing;
use crate::usage::{
    Account, UsageSource, UsageStore, claude_code::ClaudeCodeLogs, codex::CodexLogs, opencode::OpencodeDb,
    pi::PiSessions,
};

/// Coalesces bursts of log writes into one rescan.
const FS_DEBOUNCE: Duration = Duration::from_millis(750);

/// A subscription: its live quota plus the usage attributed to it.
struct AccountState {
    account: Account,
    id: &'static str,
    name: &'static str,
    limits: Limits,
}

struct App {
    pricing: Pricing,
    store: UsageStore,
    sources: Vec<Box<dyn UsageSource>>,
    accounts: Vec<AccountState>,
}

impl App {
    fn new(config: &Config) -> anyhow::Result<Self> {
        let http = Limits::http_client()?;
        let claude_dir = ClaudeApi::default_config_dir().context("cannot locate Claude config dir")?;
        let codex_home = CodexLogs::default_home().context("cannot locate Codex home")?;

        let mut sources: Vec<Box<dyn UsageSource>> =
            vec![Box::new(ClaudeCodeLogs::new(&claude_dir)), Box::new(CodexLogs::new(codex_home.clone()))];
        if let Some(dir) = PiSessions::default_agent_dir() {
            sources.push(Box::new(PiSessions::new(dir)));
        }
        if let Some(path) = OpencodeDb::default_path() {
            sources.push(Box::new(OpencodeDb::new(path)));
        }

        let accounts = vec![
            AccountState {
                account: Account::Claude,
                id: "claude",
                name: "Claude",
                limits: Limits::new(Box::new(ClaudeApi::new(claude_dir)), http.clone()),
            },
            AccountState {
                account: Account::ChatGpt,
                id: "openai",
                name: "ChatGPT",
                limits: Limits::new(Box::new(ChatGptApi::new(codex_home)), http),
            },
        ];
        Ok(Self { pricing: Pricing::new(&config.pricing), store: UsageStore::default(), sources, accounts })
    }

    fn scan(&mut self) {
        let now = Utc::now();
        for s in &mut self.sources {
            s.scan(&mut self.store, now);
        }
        self.store.prune(now);
    }

    async fn refresh_limits(&mut self) {
        for a in &mut self.accounts {
            if a.limits.configured() {
                a.limits.refresh().await;
            } else {
                a.limits.forget(); // signed out: a later sign-in starts from scratch
            }
        }
    }

    async fn refresh_all(&mut self) {
        self.scan();
        self.refresh_limits().await;
    }

    fn snapshot(&self) -> Snapshot {
        let now = Utc::now();
        let providers = self
            .accounts
            .iter()
            // Only accounts the user actually has: signed in, or with usage on record.
            .filter(|a| a.limits.configured() || self.store.has(a.account))
            .map(|a| self.account_snapshot(a, now))
            .collect();
        Snapshot { version: SCHEMA_VERSION, updated_at: now, providers }
    }

    fn account_snapshot(&self, a: &AccountState, now: chrono::DateTime<Utc>) -> ProviderSnapshot {
        let mut snap = ProviderSnapshot {
            id: a.id.into(),
            name: a.name.into(),
            plan: None,
            // Usage from other tools only: nothing live to be stale about.
            status: model::Status::Ok,
            error: None,
            limits_updated_at: None,
            limits: Vec::new(),
            limit_reached: false,
            breakdown: Vec::new(),
            stats: self.store.stats(a.account, now, &self.pricing),
            hourly: self.store.hourly(a.account, now, &self.pricing),
        };
        // Quota data is only shown while signed in; credentials may vanish between polls.
        let l = &a.limits;
        if l.configured() {
            snap.plan = l.plan.clone();
            snap.status = l.status;
            snap.error = l.error.clone();
            snap.limits_updated_at = l.updated_at;
            snap.limits = l.windows.clone();
            snap.limit_reached = l.limit_reached;
            snap.breakdown = l.breakdown.clone();
        }
        snap
    }

    fn watch_paths(&self) -> Vec<(PathBuf, bool)> {
        self.sources.iter().flat_map(|s| s.watch_paths()).collect()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();

    let config = Config::load();
    let mut app = App::new(&config)?;

    if std::env::args().any(|a| a == "--print") {
        app.refresh_all().await;
        println!("{}", serde_json::to_string_pretty(&app.snapshot())?);
        return Ok(());
    }

    let (refresh_tx, mut refresh_rx) = mpsc::channel(1);
    let (fs_tx, mut fs_rx) = mpsc::channel(1);
    let mut watch = Watch::new(fs_tx, app.watch_paths());

    let state = Arc::new(RwLock::new(String::new()));
    app.refresh_all().await;
    let mut last = publish(None, &state, &app, None).await;
    let conn = dbus::serve(state.clone(), refresh_tx)
        .await
        .context("cannot register on the session bus (already running?)")?;
    info!("serving {} on the session bus", dbus::BUS_NAME);

    let mut limits_tick = interval(config.limits_interval());
    // Re-evaluates rolling ranges and catches changes the watcher missed.
    let mut stats_tick = interval(config.stats_interval());
    for t in [&mut limits_tick, &mut stats_tick] {
        t.set_missed_tick_behavior(MissedTickBehavior::Delay);
        t.reset(); // initial refresh already done
    }

    loop {
        tokio::select! {
            _ = limits_tick.tick() => app.refresh_limits().await,
            _ = stats_tick.tick() => {
                watch.retry();
                app.scan();
            }
            Some(()) = refresh_rx.recv() => {
                app.refresh_all().await;
                limits_tick.reset();
            }
            Some(()) = fs_rx.recv() => {
                sleep(FS_DEBOUNCE).await;
                while fs_rx.try_recv().is_ok() {}
                app.scan();
            }
            _ = tokio::signal::ctrl_c() => break,
        }
        last = publish(Some(&conn), &state, &app, last).await;
    }
    info!("shutting down");
    Ok(())
}

/// Updates the served snapshot; emits `Changed` only when provider data differs.
/// Returns the provider data used for that comparison.
async fn publish(
    conn: Option<&zbus::Connection>,
    state: &RwLock<String>,
    app: &App,
    last: Option<Vec<ProviderSnapshot>>,
) -> Option<Vec<ProviderSnapshot>> {
    let snap = app.snapshot();
    let json = match serde_json::to_string(&snap) {
        Ok(j) => j,
        Err(e) => {
            warn!("serializing snapshot: {e}");
            return last;
        }
    };
    *state.write().unwrap() = json.clone();

    if last.as_ref() != Some(&snap.providers)
        && let Some(conn) = conn
        && let Err(e) = dbus::emit_changed(conn, &json).await
    {
        warn!("emitting Changed: {e}");
    }
    Some(snap.providers)
}

/// File watcher over the sources' paths. Paths that don't exist yet (e.g. a tool
/// that was never run) are retried on the stats timer. Failure only costs latency:
/// the stats timer still picks changes up.
struct Watch {
    watcher: Option<notify::RecommendedWatcher>,
    pending: Vec<(PathBuf, bool)>,
}

impl Watch {
    fn new(tx: mpsc::Sender<()>, paths: Vec<(PathBuf, bool)>) -> Self {
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res
                && matches!(ev.kind, EventKind::Create(_) | EventKind::Modify(_))
            {
                let _ = tx.try_send(());
            }
        })
        .inspect_err(|e| warn!("file watcher unavailable: {e}"))
        .ok();
        let mut w = Self { watcher, pending: paths };
        w.retry();
        w
    }

    fn retry(&mut self) {
        let Some(watcher) = &mut self.watcher else { return };
        self.pending.retain(|(path, recursive)| {
            if !path.exists() {
                return true;
            }
            let mode = if *recursive { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive };
            match watcher.watch(path, mode) {
                Ok(()) => debug!("watching {}", path.display()),
                Err(e) => warn!("cannot watch {}: {e}", path.display()),
            }
            false
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LimitWindow, Status};

    #[test]
    fn signing_out_drops_quota_but_keeps_usage() {
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join(".credentials.json");
        std::fs::write(&creds, r#"{"claudeAiOauth":{"accessToken":"t"}}"#).unwrap();
        let mut limits = Limits::new(Box::new(ClaudeApi::new(dir.path().to_owned())), Limits::http_client().unwrap());
        // As after a successful fetch.
        limits.status = Status::Ok;
        limits.error = None;
        limits.plan = Some("pro".into());
        limits.updated_at = Some(Utc::now());
        limits.limit_reached = true;
        limits.windows = vec![LimitWindow {
            id: "session".into(),
            label: "5-hour".into(),
            used_pct: 100.0,
            severity: None,
            resets_at: None,
            is_active: true,
        }];
        let mut store = UsageStore::default();
        store.upsert(
            "k".into(),
            usage::UsageRecord {
                ts: Utc::now(),
                account: Account::Claude,
                tool: "Claude Code",
                model: "claude-opus-5-5".into(),
                session: "s".into(),
                tokens: model::Tokens { input: 10, ..Default::default() },
                fast: false,
            },
        );
        let account = AccountState { account: Account::Claude, id: "claude", name: "Claude", limits };
        let mut app = App { pricing: Pricing::default(), store, sources: Vec::new(), accounts: vec![account] };

        let p = &app.snapshot().providers[0];
        assert_eq!((p.limits.len(), p.plan.as_deref(), p.limit_reached), (1, Some("pro"), true));

        std::fs::remove_file(&creds).unwrap();
        let p = &app.snapshot().providers[0];
        assert!(p.limits.is_empty() && p.plan.is_none() && !p.limit_reached && p.limits_updated_at.is_none());
        assert_eq!((p.status, p.stats["1d"].input), (Status::Ok, 10));

        // Signing in again doesn't resurrect the old data.
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(app.refresh_limits());
        std::fs::write(&creds, r#"{"claudeAiOauth":{"accessToken":"t"}}"#).unwrap();
        let p = &app.snapshot().providers[0];
        assert!(p.limits.is_empty() && p.plan.is_none());
        assert_eq!(p.status, Status::Stale);
    }
}
