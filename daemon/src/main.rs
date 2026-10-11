mod dbus;
mod model;
mod pricing;
mod providers;

use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Context;
use chrono::Utc;
use notify::{EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval, sleep};
use tracing::{info, warn};

use crate::model::{SCHEMA_VERSION, Snapshot};
use crate::providers::{Provider, claude::ClaudeProvider};

const LIMITS_INTERVAL: Duration = Duration::from_secs(180);
/// Re-evaluates rolling ranges and catches changes the watcher missed.
const STATS_INTERVAL: Duration = Duration::from_secs(60);
/// Coalesces bursts of log writes into one rescan.
const FS_DEBOUNCE: Duration = Duration::from_millis(750);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();

    let claude_dir = ClaudeProvider::default_config_dir().context("cannot locate Claude config dir")?;
    let mut providers: Vec<Box<dyn Provider>> = vec![Box::new(ClaudeProvider::new(claude_dir)?)];

    if std::env::args().any(|a| a == "--print") {
        refresh_all(&mut providers).await;
        println!("{}", serde_json::to_string_pretty(&snapshot(&providers))?);
        return Ok(());
    }

    let (refresh_tx, mut refresh_rx) = mpsc::channel(1);
    let (fs_tx, mut fs_rx) = mpsc::channel(1);
    let _watcher = watch(&providers, fs_tx);

    let state = Arc::new(RwLock::new(String::new()));
    refresh_all(&mut providers).await;
    let mut last = publish(None, &state, &providers, None).await;
    let conn = dbus::serve(state.clone(), refresh_tx)
        .await
        .context("cannot register on the session bus (already running?)")?;
    info!("serving {} on the session bus", dbus::BUS_NAME);

    let mut limits_tick = interval(LIMITS_INTERVAL);
    let mut stats_tick = interval(STATS_INTERVAL);
    for t in [&mut limits_tick, &mut stats_tick] {
        t.set_missed_tick_behavior(MissedTickBehavior::Delay);
        t.reset(); // initial refresh already done
    }

    loop {
        tokio::select! {
            _ = limits_tick.tick() => {
                for p in &mut providers { p.refresh_limits().await }
            }
            _ = stats_tick.tick() => {
                for p in &mut providers { p.refresh_stats() }
            }
            Some(()) = refresh_rx.recv() => {
                refresh_all(&mut providers).await;
                limits_tick.reset();
            }
            Some(()) = fs_rx.recv() => {
                sleep(FS_DEBOUNCE).await;
                while fs_rx.try_recv().is_ok() {}
                for p in &mut providers { p.refresh_stats() }
            }
            _ = tokio::signal::ctrl_c() => break,
        }
        last = publish(Some(&conn), &state, &providers, last).await;
    }
    info!("shutting down");
    Ok(())
}

async fn refresh_all(providers: &mut [Box<dyn Provider>]) {
    for p in providers {
        p.refresh_stats();
        p.refresh_limits().await;
    }
}

fn snapshot(providers: &[Box<dyn Provider>]) -> Snapshot {
    let now = Utc::now();
    Snapshot {
        version: SCHEMA_VERSION,
        updated_at: now,
        providers: providers.iter().map(|p| p.snapshot(now)).collect(),
    }
}

/// Updates the served snapshot; emits `Changed` only when provider data differs.
/// Returns the provider data used for that comparison.
async fn publish(
    conn: Option<&zbus::Connection>,
    state: &RwLock<String>,
    providers: &[Box<dyn Provider>],
    last: Option<Vec<model::ProviderSnapshot>>,
) -> Option<Vec<model::ProviderSnapshot>> {
    let snap = snapshot(providers);
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

/// Returns the watcher, which must be kept alive. Failure only costs latency:
/// the stats timer still picks changes up.
fn watch(providers: &[Box<dyn Provider>], tx: mpsc::Sender<()>) -> Option<notify::RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res
            && matches!(ev.kind, EventKind::Create(_) | EventKind::Modify(_))
        {
            let _ = tx.try_send(());
        }
    })
    .inspect_err(|e| warn!("file watcher unavailable: {e}"))
    .ok()?;

    for path in providers.iter().flat_map(|p| p.watch_paths()) {
        if let Err(e) = watcher.watch(&path, RecursiveMode::Recursive) {
            warn!("cannot watch {}: {e}", path.display());
        }
    }
    Some(watcher)
}
