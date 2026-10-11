mod limits;
mod logs;

use std::path::PathBuf;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use self::{limits::ClaudeLimits, logs::LogIndex};
use super::Provider;
use crate::model::ProviderSnapshot;

pub struct ClaudeProvider {
    limits: ClaudeLimits,
    logs: LogIndex,
}

impl ClaudeProvider {
    pub fn new(config_dir: PathBuf) -> anyhow::Result<Self> {
        Ok(Self {
            limits: ClaudeLimits::new(config_dir.clone())?,
            logs: LogIndex::new(config_dir.join("projects")),
        })
    }

    /// `$CLAUDE_CONFIG_DIR`, else `~/.claude` (same lookup as Claude Code).
    pub fn default_config_dir() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
            return Some(dir.into());
        }
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude"))
    }
}

#[async_trait]
impl Provider for ClaudeProvider {
    async fn refresh_limits(&mut self) {
        self.limits.refresh().await;
    }

    fn refresh_stats(&mut self) {
        self.logs.scan(Utc::now());
    }

    fn watch_paths(&self) -> Vec<PathBuf> {
        vec![self.logs.root().to_owned()]
    }

    fn snapshot(&self, now: DateTime<Utc>) -> ProviderSnapshot {
        let l = &self.limits;
        ProviderSnapshot {
            id: "claude".into(),
            name: "Claude".into(),
            plan: l.plan.clone(),
            status: l.status,
            error: l.error.clone(),
            limits_updated_at: l.updated_at,
            limits: l.windows.clone(),
            breakdown: l.breakdown.clone(),
            stats: self.logs.stats(now),
        }
    }
}
