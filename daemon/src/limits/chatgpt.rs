//! ChatGPT plan limits (the Codex usage windows), from the endpoint Codex CLI's
//! `/status` uses: `GET chatgpt.com/backend-api/wham/usage` with Codex's OAuth token
//! and the `ChatGPT-Account-Id` header.
//!
//! Only Codex's own sign-in (`$CODEX_HOME/auth.json`) works here; pi and opencode
//! use a separate token-sharing login that this endpoint doesn't accept. Codex
//! refreshes its token when it runs, so a stale token means "run codex once".

use std::path::PathBuf;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::{LimitApi, NotReady, Parsed, endpoint};
use crate::model::{LimitWindow, Status};

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const USAGE_URL_ENV: &str = "AGENT_BAR_CHATGPT_USAGE_URL";
const REFRESH_HINT: &str = "Codex sign-in expired; run `codex` once to refresh it";

pub struct ChatGptApi {
    codex_home: PathBuf,
    usage_url: String,
}

impl ChatGptApi {
    pub fn new(codex_home: PathBuf) -> Self {
        Self { codex_home, usage_url: endpoint(USAGE_URL, USAGE_URL_ENV) }
    }
}

impl LimitApi for ChatGptApi {
    fn name(&self) -> &'static str {
        "chatgpt"
    }

    fn configured(&self) -> bool {
        read_credentials(&self.codex_home).is_ok()
    }

    fn request(&self, http: &reqwest::Client) -> Result<(reqwest::RequestBuilder, Option<String>), NotReady> {
        let creds = read_credentials(&self.codex_home)
            .map_err(|e| NotReady { status: Status::Error, message: format!("{e:#}") })?;
        if jwt_expiry(&creds.access_token).is_some_and(|exp| exp <= Utc::now().timestamp()) {
            return Err(NotReady { status: Status::Stale, message: REFRESH_HINT.into() });
        }
        let mut req = http.get(&self.usage_url).bearer_auth(&creds.access_token);
        if let Some(id) = &creds.account_id {
            req = req.header("ChatGPT-Account-Id", id);
        }
        Ok((req, None))
    }

    fn parse(&self, body: &str) -> anyhow::Result<Parsed> {
        parse_usage(body)
    }

    fn rejected_hint(&self) -> &'static str {
        REFRESH_HINT
    }
}

// No Debug derive: keeps the token out of accidental `{:?}` logging.
struct Credentials {
    access_token: String,
    account_id: Option<String>,
}

fn read_credentials(codex_home: &std::path::Path) -> anyhow::Result<Credentials> {
    #[derive(Deserialize)]
    struct File {
        auth_mode: Option<String>,
        tokens: Option<Tokens>,
    }
    #[derive(Deserialize)]
    struct Tokens {
        access_token: String,
        account_id: Option<String>,
    }

    let path = codex_home.join("auth.json");
    let raw = std::fs::read(&path).with_context(|| format!("no Codex sign-in at {}", path.display()))?;
    let file: File = serde_json::from_slice(&raw).context("unreadable Codex auth.json")?;
    if !file.auth_mode.is_some_and(|m| m.eq_ignore_ascii_case("chatgpt")) {
        anyhow::bail!("Codex is not signed in with ChatGPT");
    }
    let t = file.tokens.context("Codex auth.json has no tokens; run `codex login`")?;
    Ok(Credentials { access_token: t.access_token, account_id: t.account_id })
}

/// The `exp` claim of a JWT, without verifying it (only used to skip doomed requests).
fn jwt_expiry(token: &str) -> Option<i64> {
    #[derive(Deserialize)]
    struct Claims {
        exp: Option<i64>,
    }
    let payload = base64url_decode(token.split('.').nth(1)?)?;
    serde_json::from_slice::<Claims>(&payload).ok()?.exp
}

fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.trim_end_matches('=').bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

pub(crate) fn parse_usage(body: &str) -> anyhow::Result<Parsed> {
    #[derive(Deserialize)]
    struct Resp {
        plan_type: Option<String>,
        rate_limit: Option<RateLimit>,
    }
    #[derive(Deserialize)]
    struct RateLimit {
        limit_reached: Option<bool>,
        primary_window: Option<Window>,
        secondary_window: Option<Window>,
    }
    #[derive(Deserialize)]
    struct Window {
        used_percent: f64,
        limit_window_seconds: Option<i64>,
        reset_at: Option<i64>,
        reset_after_seconds: Option<i64>,
    }

    let resp: Resp = serde_json::from_str(body)?;
    let rl = resp.rate_limit.context("no rate_limit in response")?;
    let now = Utc::now();
    let windows: Vec<LimitWindow> = [("primary", rl.primary_window), ("secondary", rl.secondary_window)]
        .into_iter()
        .filter_map(|(id, w)| {
            let w = w?;
            let resets_at = w
                .reset_at
                .and_then(|s| DateTime::from_timestamp(s, 0))
                .or_else(|| w.reset_after_seconds.map(|s| now + chrono::Duration::seconds(s)));
            Some(LimitWindow {
                id: id.into(),
                label: w.limit_window_seconds.map_or_else(|| "Limit".into(), label_for),
                used_pct: w.used_percent,
                severity: None,
                resets_at,
                is_active: false,
            })
        })
        .collect();
    if windows.is_empty() {
        anyhow::bail!("no limit windows in response");
    }
    Ok(Parsed { windows, breakdown: Vec::new(), limit_reached: rl.limit_reached, plan: resp.plan_type })
}

fn label_for(secs: i64) -> String {
    const HOUR: i64 = 3600;
    const DAY: i64 = 24 * HOUR;
    match secs {
        s if s == 7 * DAY => "Weekly".into(),
        s if s % DAY == 0 => format!("{}-day", s / DAY),
        s if s % HOUR == 0 => format!("{}-hour", s / HOUR),
        s => format!("{}-minute", s / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/chatgpt_wham_usage.json");

    #[test]
    fn parses_windows() {
        let p = parse_usage(FIXTURE).unwrap();
        assert_eq!(p.plan.as_deref(), Some("plus"));
        assert_eq!(p.limit_reached, Some(false));
        assert_eq!(p.windows.len(), 2);
        assert_eq!((p.windows[0].id.as_str(), p.windows[0].label.as_str()), ("primary", "5-hour"));
        assert_eq!(p.windows[0].used_pct, 42.0);
        assert_eq!(p.windows[0].resets_at.unwrap().timestamp(), 1_791_720_000);
        assert_eq!(p.windows[1].label, "Weekly");
    }

    #[test]
    fn missing_secondary_and_relative_reset() {
        let body = r#"{"plan_type":"pro","rate_limit":{"limit_reached":true,"primary_window":{"used_percent":100,"limit_window_seconds":18000,"reset_after_seconds":60},"secondary_window":null}}"#;
        let p = parse_usage(body).unwrap();
        assert_eq!(p.windows.len(), 1);
        assert_eq!(p.limit_reached, Some(true));
        assert!(p.windows[0].resets_at.unwrap() > Utc::now());
        assert!(parse_usage(r#"{"rate_limit":{}}"#).is_err());
    }

    #[test]
    fn reads_jwt_expiry() {
        // header.{"exp":1791720000}.sig
        assert_eq!(jwt_expiry("e30.eyJleHAiOjE3OTE3MjAwMDB9.c2ln"), Some(1_791_720_000));
        assert_eq!(jwt_expiry("not-a-jwt"), None);
    }

    #[test]
    fn requires_chatgpt_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        let api = ChatGptApi::new(dir.path().to_owned());
        assert!(!api.configured());
        std::fs::write(dir.path().join("auth.json"), r#"{"auth_mode":"apikey","OPENAI_API_KEY":"x"}"#).unwrap();
        assert!(!api.configured());
        std::fs::write(
            dir.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"a","account_id":"b"}}"#,
        )
        .unwrap();
        assert!(api.configured());
    }

    #[test]
    fn labels() {
        assert_eq!(label_for(18_000), "5-hour");
        assert_eq!(label_for(604_800), "Weekly");
        assert_eq!(label_for(2_592_000), "30-day");
    }
}
