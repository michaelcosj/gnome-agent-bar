pub mod claude;

use std::path::PathBuf;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::model::ProviderSnapshot;

/// One AI agent / subscription. Limits come from the network and are polled on a
/// timer; stats come from local logs and are refreshed on file changes.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Fetch live quota windows. Implementations rate-limit themselves, so this is
    /// safe to call on every manual refresh.
    async fn refresh_limits(&mut self);

    /// Ingest new local log data. Cheap when nothing changed.
    fn refresh_stats(&mut self);

    /// Directories whose changes should trigger `refresh_stats`.
    fn watch_paths(&self) -> Vec<PathBuf>;

    fn snapshot(&self, now: DateTime<Utc>) -> ProviderSnapshot;
}
