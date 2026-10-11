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

    /// Drops fetched data, e.g. after sign-out; request throttling is kept.
    pub fn forget(&mut self) {
        self.status = Status::Stale;
        self.error = Some("not fetched yet".into());
        self.plan = None;
        self.updated_at = None;
        self.windows.clear();
        self.breakdown.clear();
        self.limit_reached = false;
    }

    pub async fn refresh(&mut self) {
        let now = Instant::now();
        if self.backoff_until.is_some_and(|t| now < t) || self.last_attempt.is_some_and(|t| now - t < MIN_INTERVAL) {
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
                self.limit_reached = p.limit_reached.unwrap_or_else(|| p.windows.iter().any(|w| w.used_pct >= 100.0));
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
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const OK_BODY: &str = include_str!("../../tests/fixtures/chatgpt_wham_usage.json");

    /// Local HTTP server answering each request with the next scripted response.
    struct Server {
        url: String,
        hits: Arc<AtomicUsize>,
        script: Arc<Mutex<VecDeque<String>>>,
    }

    impl Server {
        async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/usage", listener.local_addr().unwrap());
            let hits = Arc::new(AtomicUsize::new(0));
            let script: Arc<Mutex<VecDeque<String>>> = Arc::default();
            let (h, sc) = (hits.clone(), script.clone());
            tokio::spawn(async move {
                while let Ok((mut sock, _)) = listener.accept().await {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    h.fetch_add(1, Ordering::SeqCst);
                    let resp = sc.lock().unwrap().pop_front().unwrap_or_default();
                    let _ = sock.write_all(resp.as_bytes()).await;
                }
            });
            Self { url, hits, script }
        }

        fn respond(&self, status: &str, headers: &str, body: &str) {
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
                body.len()
            );
            self.script.lock().unwrap().push_back(resp);
        }

        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
    }

    struct TestApi {
        url: String,
    }

    impl LimitApi for TestApi {
        fn name(&self) -> &'static str {
            "test"
        }
        fn configured(&self) -> bool {
            true
        }
        fn request(&self, http: &reqwest::Client) -> Result<(reqwest::RequestBuilder, Option<String>), NotReady> {
            if self.url.is_empty() {
                return Err(NotReady { status: Status::Stale, message: "token expired".into() });
            }
            Ok((http.get(&self.url), None))
        }
        fn parse(&self, body: &str) -> anyhow::Result<Parsed> {
            chatgpt::parse_usage(body)
        }
        fn rejected_hint(&self) -> &'static str {
            "sign in again"
        }
    }

    /// Pretend the request floor and any backoff have elapsed.
    fn let_time_pass(l: &mut Limits) {
        l.last_attempt = None;
        l.backoff_until = None;
    }

    #[tokio::test]
    async fn request_policy_and_error_handling() {
        let srv = Server::start().await;
        let mut l = Limits::new(Box::new(TestApi { url: srv.url.clone() }), Limits::http_client().unwrap());

        srv.respond("200 OK", "", OK_BODY);
        l.refresh().await;
        assert_eq!((l.status, l.windows.len(), l.plan.as_deref(), srv.hits()), (Status::Ok, 2, Some("plus"), 1));
        let fetched = l.updated_at;

        // At most one request per minute, manual refreshes included.
        l.refresh().await;
        assert_eq!(srv.hits(), 1);

        // 429: stale, last-known windows kept, Retry-After lengthens the wait.
        l.last_attempt = None;
        srv.respond("429 Too Many Requests", "Retry-After: 300\r\n", "");
        l.refresh().await;
        assert_eq!((l.status, l.windows.len(), l.updated_at), (Status::Stale, 2, fetched));
        let wait = l.backoff_until.unwrap() - Instant::now();
        assert!(wait > Duration::from_secs(290) && wait <= Duration::from_secs(300));
        // Still backing off even once the per-minute floor has passed.
        l.last_attempt = None;
        l.refresh().await;
        assert_eq!(srv.hits(), 2);

        // 5xx: also backs off, now for longer.
        let_time_pass(&mut l);
        srv.respond("503 Service Unavailable", "", "");
        l.refresh().await;
        assert_eq!((l.status, l.windows.len(), srv.hits()), (Status::Stale, 2, 3));
        assert!(l.backoff_until.unwrap() - Instant::now() > Duration::from_secs(110));

        // 401: stale with the sign-in hint, no backoff.
        let_time_pass(&mut l);
        srv.respond("401 Unauthorized", "", "");
        l.refresh().await;
        assert_eq!((l.status, l.error.as_deref(), l.windows.len()), (Status::Stale, Some("sign in again"), 2));
        assert!(l.backoff_until.is_none());

        // Other 4xx and malformed bodies: errors, data kept.
        let_time_pass(&mut l);
        srv.respond("404 Not Found", "", "");
        l.refresh().await;
        assert_eq!((l.status, l.windows.len()), (Status::Error, 2));
        let_time_pass(&mut l);
        srv.respond("200 OK", "", r#"{"rate_limit":{}}"#);
        l.refresh().await;
        assert_eq!((l.status, l.windows.len()), (Status::Error, 2));
        assert!(l.error.as_deref().unwrap().starts_with("unexpected response"));

        // Recovery clears the error and resets the backoff.
        let_time_pass(&mut l);
        srv.respond("200 OK", "", OK_BODY);
        l.refresh().await;
        assert_eq!((l.status, l.error.as_deref(), l.backoff, srv.hits()), (Status::Ok, None, MIN_BACKOFF, 7));
        assert!(l.updated_at > fetched);
    }

    #[tokio::test]
    async fn no_request_without_usable_credentials() {
        let mut l = Limits::new(Box::new(TestApi { url: String::new() }), Limits::http_client().unwrap());
        l.refresh().await;
        assert_eq!((l.status, l.error.as_deref()), (Status::Stale, Some("token expired")));
    }

    #[tokio::test]
    async fn network_failure_backs_off() {
        // Nothing listens on a port we just released.
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let api = TestApi { url: format!("http://127.0.0.1:{port}/usage") };
        let mut l = Limits::new(Box::new(api), Limits::http_client().unwrap());
        l.refresh().await;
        assert_eq!(l.status, Status::Stale);
        assert!(l.error.as_deref().unwrap().starts_with("request failed"));
        assert!(l.backoff_until.is_some());
    }

    #[test]
    fn backoff_grows_despite_short_retry_after() {
        let s = Duration::from_secs;
        assert_eq!(backoff_wait(s(240), Some(s(5))), s(240));
        assert_eq!(backoff_wait(s(60), Some(s(600))), s(600));
        assert_eq!(backoff_wait(s(60), None), s(60));
        assert_eq!(backoff_wait(s(60), Some(s(99_999))), MAX_BACKOFF);
    }
}
