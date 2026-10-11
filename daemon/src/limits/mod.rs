//! Live subscription quota windows, polled from each account's usage endpoint.
//!
//! `Limits` owns the shared request policy: at most one request per minute per
//! endpoint (polls and manual refreshes alike) and exponential backoff on 429/5xx.
//! Each `LimitApi` only knows how to authorize a request and parse the response.
//!
//! Tokens are read from the owning tool's credentials file on every request, held
//! only in memory and never logged. We never refresh them ourselves: rotating a
//! token could sign the owning tool out. An expired token marks the data stale.

pub mod chatgpt;
pub mod claude;

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use reqwest::{StatusCode, header};
use tracing::{debug, warn};

use crate::model::{BreakdownRow, LimitWindow, Status};

/// Hard floor between requests to one endpoint: 1 per minute.
const MIN_INTERVAL: Duration = Duration::from_secs(60);
const MIN_BACKOFF: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);

/// Why no request could be made.
pub struct NotReady {
    pub status: Status,
    pub message: String,
}

pub struct Parsed {
    pub windows: Vec<LimitWindow>,
    pub breakdown: Vec<BreakdownRow>,
    /// Explicit flag from the server; otherwise derived from the windows.
    pub limit_reached: Option<bool>,
    pub plan: Option<String>,
}

pub trait LimitApi: Send + Sync {
    /// Short name for logs.
    fn name(&self) -> &'static str;

    /// Whether the user is signed in to this account at all (credentials exist).
    fn configured(&self) -> bool;

    /// An authorized request, plus the plan name if the credentials carry it.
    fn request(&self, http: &reqwest::Client) -> Result<(reqwest::RequestBuilder, Option<String>), NotReady>;

    fn parse(&self, body: &str) -> anyhow::Result<Parsed>;

    /// Shown on 401/403.
    fn rejected_hint(&self) -> &'static str;
}

pub struct Limits {
    api: Box<dyn LimitApi>,
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
    pub limit_reached: bool,
}

impl Limits {
    pub fn new(api: Box<dyn LimitApi>, http: reqwest::Client) -> Self {
        Self {
            api,
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
            limit_reached: false,
        }
    }

    pub fn http_client() -> anyhow::Result<reqwest::Client> {
        Ok(reqwest::Client::builder()
            .user_agent(concat!("agent-bar/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(10))
            .build()?)
    }

    pub fn configured(&self) -> bool {
        self.api.configured()
    }

    pub async fn refresh(&mut self) {
        let now = Instant::now();
        if self.backoff_until.is_some_and(|t| now < t)
            || self.last_attempt.is_some_and(|t| now - t < MIN_INTERVAL)
        {
            return;
        }
        self.last_attempt = Some(now);

        let req = match self.api.request(&self.http) {
            Ok((req, plan)) => {
                if plan.is_some() {
                    self.plan = plan;
                }
                req
            }
            Err(e) => return self.fail(e.status, e.message),
        };
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                self.back_off(None);
                return self.fail(Status::Stale, format!("request failed: {}", e.without_url()));
            }
        };

        match resp.status() {
            StatusCode::OK => {}
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                return self.fail(Status::Stale, self.api.rejected_hint().into());
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
            Ok(body) => self.api.parse(&body),
            Err(e) => Err(anyhow::anyhow!("reading body: {}", e.without_url())),
        };
        match parsed {
            Ok(p) => {
                debug!(windows = p.windows.len(), "{} limits updated", self.api.name());
                self.limit_reached =
                    p.limit_reached.unwrap_or_else(|| p.windows.iter().any(|w| w.used_pct >= 100.0));
                self.windows = p.windows;
                self.breakdown = p.breakdown;
                if p.plan.is_some() {
                    self.plan = p.plan;
                }
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
        warn!("{} limits: {msg}", self.api.name());
        self.status = status;
        self.error = Some(msg);
    }

    fn back_off(&mut self, retry_after: Option<Duration>) {
        let wait = backoff_wait(self.backoff, retry_after);
        self.backoff_until = Some(Instant::now() + wait);
        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
    }
}

/// The server's Retry-After can lengthen our exponential backoff but never shorten it,
/// otherwise a short Retry-After on every 429 would pin retries at the minimum.
fn backoff_wait(current: Duration, retry_after: Option<Duration>) -> Duration {
    retry_after.map_or(current, |r| r.max(current)).clamp(MIN_BACKOFF, MAX_BACKOFF)
}

/// Test-only endpoint override so automated tests never hit (and rate-limit) the real one.
fn endpoint(default: &str, env: &str) -> String {
    match std::env::var(env) {
        Ok(url) => {
            warn!("using {env}={url}");
            url
        }
        Err(_) => default.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_despite_short_retry_after() {
        let s = Duration::from_secs;
        assert_eq!(backoff_wait(s(240), Some(s(5))), s(240));
        assert_eq!(backoff_wait(s(60), Some(s(600))), s(600));
        assert_eq!(backoff_wait(s(60), None), s(60));
        assert_eq!(backoff_wait(s(60), Some(s(99_999))), MAX_BACKOFF);
    }
}
