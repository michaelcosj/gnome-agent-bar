//! Claude subscription limits from the (undocumented) OAuth usage endpoint that
//! Claude Code's `/usage` uses. Parse defensively: the response is full of internal
//! codename fields, so we read the generic `limits[]` array instead.

use std::path::PathBuf;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::{LimitApi, NotReady, Parsed, endpoint};
use crate::model::{BreakdownRow, LimitWindow, Status};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const USAGE_URL_ENV: &str = "AGENT_BAR_USAGE_URL";
const OAUTH_BETA: &str = "oauth-2025-04-20";

pub struct ClaudeApi {
    config_dir: PathBuf,
    usage_url: String,
}

impl ClaudeApi {
    pub fn new(config_dir: PathBuf) -> Self {
        Self { config_dir, usage_url: endpoint(USAGE_URL, USAGE_URL_ENV) }
    }

    /// `$CLAUDE_CONFIG_DIR`, else `~/.claude` (same lookup as Claude Code).
    pub fn default_config_dir() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
            return Some(dir.into());
        }
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude"))
    }
}

impl LimitApi for ClaudeApi {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn configured(&self) -> bool {
        self.config_dir.join(".credentials.json").exists()
    }

    fn request(&self, http: &reqwest::Client) -> Result<(reqwest::RequestBuilder, Option<String>), NotReady> {
        let creds = read_credentials(&self.config_dir)
            .map_err(|e| NotReady { status: Status::Error, message: format!("{e:#}") })?;
        if creds.expires_at.is_some_and(|ms| ms <= Utc::now().timestamp_millis()) {
            return Err(NotReady {
                status: Status::Stale,
                message: "OAuth token expired; open Claude Code to refresh it".into(),
            });
        }
        let req = http.get(&self.usage_url).bearer_auth(&creds.access_token).header("anthropic-beta", OAUTH_BETA);
        Ok((req, creds.subscription_type))
    }

    fn parse(&self, body: &str) -> anyhow::Result<Parsed> {
        let (windows, breakdown) = parse_usage(body)?;
        Ok(Parsed { windows, breakdown, limit_reached: None, plan: None })
    }

    fn rejected_hint(&self) -> &'static str {
        "token rejected; open Claude Code to sign in again"
    }
}

// No Debug derive: keeps the token out of accidental `{:?}` logging.
struct Credentials {
    access_token: String,
    expires_at: Option<i64>,
    subscription_type: Option<String>,
}

fn read_credentials(config_dir: &std::path::Path) -> anyhow::Result<Credentials> {
    #[derive(Deserialize)]
    struct File {
        #[serde(rename = "claudeAiOauth")]
        oauth: Option<OAuth>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct OAuth {
        access_token: String,
        expires_at: Option<i64>,
        subscription_type: Option<String>,
    }

    let path = config_dir.join(".credentials.json");
    let raw = std::fs::read(&path).with_context(|| format!("no Claude credentials at {}", path.display()))?;
    let file: File = serde_json::from_slice(&raw).context("unreadable Claude credentials file")?;
    let o = file.oauth.context("not signed in to Claude with a subscription (no claudeAiOauth)")?;
    Ok(Credentials { access_token: o.access_token, expires_at: o.expires_at, subscription_type: o.subscription_type })
}

pub(crate) fn parse_usage(body: &str) -> anyhow::Result<(Vec<LimitWindow>, Vec<BreakdownRow>)> {
    #[derive(Deserialize)]
    struct Resp {
        #[serde(default)]
        limits: Vec<RawLimit>,
        five_hour: Option<RawWindow>,
        seven_day: Option<RawWindow>,
        seven_day_breakdown: Option<RawBreakdown>,
    }
    #[derive(Deserialize)]
    struct RawLimit {
        kind: String,
        percent: f64,
        severity: Option<String>,
        resets_at: Option<DateTime<Utc>>,
        #[serde(default)]
        is_active: bool,
    }
    #[derive(Deserialize)]
    struct RawWindow {
        utilization: Option<f64>,
        resets_at: Option<DateTime<Utc>>,
    }
    #[derive(Deserialize)]
    struct RawBreakdown {
        #[serde(default)]
        rows: Vec<RawRow>,
    }
    #[derive(Deserialize)]
    struct RawRow {
        display_name: String,
        percent: f64,
    }

    let resp: Resp = serde_json::from_str(body)?;

    let mut windows: Vec<LimitWindow> = resp
        .limits
        .into_iter()
        .map(|l| LimitWindow {
            label: label_for(&l.kind),
            id: l.kind,
            used_pct: l.percent,
            severity: l.severity,
            resets_at: l.resets_at,
            is_active: l.is_active,
        })
        .collect();

    // Older response shape without `limits[]`.
    if windows.is_empty() {
        for (id, w) in [("session", resp.five_hour), ("weekly_all", resp.seven_day)] {
            if let Some(RawWindow { utilization: Some(pct), resets_at }) = w {
                windows.push(LimitWindow {
                    id: id.into(),
                    label: label_for(id),
                    used_pct: pct,
                    severity: None,
                    resets_at,
                    is_active: false,
                });
            }
        }
    }
    if windows.is_empty() {
        anyhow::bail!("no limit windows in response");
    }

    let breakdown = resp
        .seven_day_breakdown
        .map(|b| b.rows.into_iter().map(|r| BreakdownRow { label: r.display_name, pct: r.percent }).collect())
        .unwrap_or_default();

    Ok((windows, breakdown))
}

fn label_for(kind: &str) -> String {
    match kind {
        "session" => "5-hour".into(),
        "weekly_all" => "Weekly".into(),
        "weekly_opus" => "Weekly (Opus)".into(),
        "weekly_sonnet" => "Weekly (Sonnet)".into(),
        other => {
            let mut s = other.replace('_', " ");
            if let Some(first) = s.get_mut(..1) {
                first.make_ascii_uppercase();
            }
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/claude_oauth_usage.json");

    #[test]
    fn parses_limits_array() {
        let (windows, breakdown) = parse_usage(FIXTURE).unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].id, "session");
        assert_eq!(windows[0].label, "5-hour");
        assert_eq!(windows[0].used_pct, 37.0);
        assert!(windows[0].is_active);
        assert_eq!(windows[1].label, "Weekly");
        assert_eq!(windows[1].severity.as_deref(), Some("warning"));
        assert_eq!(windows[1].resets_at.unwrap().to_rfc3339(), "2026-01-20T16:59:59.616794+00:00");
        assert_eq!(breakdown.len(), 4);
        assert_eq!(breakdown[0].label, "Claude Code");
        assert_eq!(breakdown[0].pct, 70.0);
    }

    #[test]
    fn falls_back_to_named_windows() {
        let body = r#"{"five_hour":{"utilization":12.5,"resets_at":null},"seven_day":null}"#;
        let (windows, breakdown) = parse_usage(body).unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].used_pct, 12.5);
        assert!(breakdown.is_empty());
    }

    #[test]
    fn rejects_empty_response() {
        assert!(parse_usage("{}").is_err());
    }

    #[test]
    fn labels_unknown_kinds() {
        assert_eq!(label_for("monthly_spend"), "Monthly spend");
    }
}
