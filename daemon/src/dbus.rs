//! Session-bus API consumed by the GNOME Shell extension.
//!
//!   name:      dev.agentbar.Usage
//!   path:      /dev/agentbar/Usage
//!   interface: dev.agentbar.Usage
//!     GetSnapshot() -> s     snapshot JSON (see model.rs)
//!     Refresh()              poll now (providers still enforce their own rate limits)
//!     signal Changed(s)      emitted when snapshot content changes

use std::sync::{Arc, RwLock};

use tokio::sync::mpsc;
use zbus::object_server::SignalEmitter;
use zbus::{Connection, interface};

pub const BUS_NAME: &str = "dev.agentbar.Usage";
pub const OBJECT_PATH: &str = "/dev/agentbar/Usage";

pub struct UsageService {
    snapshot: Arc<RwLock<String>>,
    refresh_tx: mpsc::Sender<()>,
}

#[interface(name = "dev.agentbar.Usage")]
impl UsageService {
    fn get_snapshot(&self) -> String {
        self.snapshot.read().unwrap().clone()
    }

    fn refresh(&self) {
        // A refresh is already queued if the channel is full.
        let _ = self.refresh_tx.try_send(());
    }

    #[zbus(signal)]
    async fn changed(emitter: &SignalEmitter<'_>, snapshot: &str) -> zbus::Result<()>;
}

pub async fn serve(snapshot: Arc<RwLock<String>>, refresh_tx: mpsc::Sender<()>) -> zbus::Result<Connection> {
    zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, UsageService { snapshot, refresh_tx })?
        .build()
        .await
}

pub async fn emit_changed(conn: &Connection, snapshot: &str) -> zbus::Result<()> {
    let iface = conn.object_server().interface::<_, UsageService>(OBJECT_PATH).await?;
    UsageService::changed(iface.signal_emitter(), snapshot).await
}
