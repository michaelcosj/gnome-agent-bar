//! Subscription limits from the (undocumented) OAuth usage endpoint that Claude
//! Code's `/usage` uses. Parse defensively: the response is full of internal
//! codename fields, so we read the generic `limits[]` array instead.
//!
//! The OAuth token is read from Claude Code's credentials file on every request,
//! held only in memory and never logged. We never refresh it ourselves: rotating
//! it could invalidate Claude Code's own session. An expired token marks the data
//! stale until Claude Code refreshes it.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::{DateTime, Utc};
use reqwest::{StatusCode, header};
use serde::Deserialize;
use tracing::{debug, warn};

use crate::model::{BreakdownRow, LimitWindow, Status};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA: &str = "oauth-2025-04-20";
/// Floor between requests, so manual refreshes can't hammer the endpoint.
const MIN_INTERVAL: Duration = Duration::from_secs(30);
const MIN_BACKOFF: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);

pub struct ClaudeLimits {
    config_dir: PathBuf,
    http: reqwest::Client,
    last_attempt: Option<Instant>,
    backoff: Duration,
    backoff_until: Option<Instant>,

    pub status: Status,
    pub error: Option<String>,
    pub plan: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
    pub windows: Vec<LimitWindow>,
    pub breakdown: Vec<BreakdownRow>,
}

impl ClaudeLimits {
    pub fn new(config_dir: PathBuf) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("agent-bar/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            config_dir,
            http,
            last_attempt: None,
            backoff: MIN_BACKOFF,
            backoff_until: None,
            status: Status::Stale,
            error: Some("not fetched yet".into()),
            plan: None,
            updated_at: None,
            windows: Vec::new(),
            breakdown: Vec::new(),
        })
    }

    pub async fn refresh(&mut self) {
        let now = Instant::now();
        if self.backoff_until.is_some_and(|t| now < t)
            || self.last_attempt.is_some_and(|t| now - t < MIN_INTERVAL)
        {
            return;
        }
        self.last_attempt = Some(now);

        let creds = match read_credentials(&self.config_dir) {
            Ok(c) => c,
            Err(e) => return self.fail(Status::Error, format!("{e:#}")),
        };
        self.plan = creds.subscription_type;
        if creds.expires_at.is_some_and(|ms| ms <= Utc::now().timestamp_millis()) {
            return self.fail(Status::Stale, "OAuth token expired; open Claude Code to refresh it".into());
        }

        let resp = self
            .http
            .get(USAGE_URL)
            .bearer_auth(&creds.access_token)
            .header("anthropic-beta", OAUTH_BETA)
            .send()
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                self.back_off(None);
                return self.fail(Status::Stale, format!("request failed: {}", e.without_url()));
            }
        };

        let status = resp.status();
        match status {
            StatusCode::OK => {}
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                return self.fail(Status::Stale, "token rejected; open Claude Code to sign in again".into());
            }
            s if s == StatusCode::TOO_MANY_REQUESTS || s.is_server_error() => {
                let retry_after = resp
                    .headers()
                    .get(header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok()?.parse().ok())
                    .map(Duration::from_secs);
                self.back_off(retry_after);
                return self.fail(Status::Stale, format!("usage endpoint returned {s}; backing off"));
            }
            s => return self.fail(Status::Error, format!("usage endpoint returned {s}")),
        }

        let parsed = match resp.text().await {
            Ok(body) => parse_usage(&body),
            Err(e) => Err(anyhow::anyhow!("reading body: {}", e.without_url())),
        };
        match parsed {
            Ok((windows, breakdown)) => {
                debug!(windows = windows.len(), "claude limits updated");
                self.windows = windows;
                self.breakdown = breakdown;
                self.updated_at = Some(Utc::now());
                self.status = Status::Ok;
                self.error = None;
                self.backoff = MIN_BACKOFF;
                self.backoff_until = None;
            }
            Err(e) => self.fail(Status::Error, format!("unexpected response: {e:#}")),
        }
    }

    /// Keep last-known windows so the UI can still show them, marked stale.
    fn fail(&mut self, status: Status, msg: String) {
        warn!("claude limits: {msg}");
        self.status = status;
        self.error = Some(msg);
    }

    fn back_off(&mut self, retry_after: Option<Duration>) {
        let wait = retry_after.unwrap_or(self.backoff).clamp(MIN_BACKOFF, MAX_BACKOFF);
        self.backoff_until = Some(Instant::now() + wait);
        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
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

    const FIXTURE: &str = include_str!("../../../tests/fixtures/claude_oauth_usage.json");

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
