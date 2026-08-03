//! Reading Claude Code usage off the API's rate-limit headers.
//!
//! There is no usage endpoint. The numbers ride on the `anthropic-ratelimit-*`
//! response headers of an ordinary request, so this makes the smallest one it
//! can and throws the completion away — the headers are the payload.

use anyhow::{Context, Result, anyhow};
use claudial_icd::{UsageSnapshot, UsageStatus};
use reqwest::header::HeaderMap;
use serde_json::json;
use tracing::debug;

use super::clamp_percent;
use crate::credentials;

const API_URL: &str = "https://api.anthropic.com/v1/messages";

/// The token from Claude Code's credential store is an OAuth token, so it goes
/// on `Authorization: Bearer` with this beta header — **not** on `x-api-key`,
/// which is for API keys. `/v1/messages` rejects the request without it.
const OAUTH_BETA: &str = "oauth-2025-04-20";

/// The cheapest model available, asked for a single token.
///
/// This request exists only to be answered with headers; its completion is
/// discarded. That is why it is not the default model the rest of the
/// ecosystem would reach for.
const PROBE_MODEL: &str = "claude-haiku-4-5";

pub struct UsageClient {
    http: reqwest::Client,
}

impl UsageClient {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .context("building HTTP client")?;
        Ok(Self { http })
    }

    /// Make the probe request and read usage out of the response headers.
    pub async fn poll(&self) -> Result<UsageSnapshot> {
        // Re-read the token every poll — Claude Code rotates it, so a value
        // cached at startup eventually starts returning 401.
        let token = credentials::read_access_token()?;

        let response = self
            .http
            .post(API_URL)
            .bearer_auth(token)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", OAUTH_BETA)
            .json(&json!({
                "model": PROBE_MODEL,
                "max_tokens": 1,
                "messages": [{"role": "user", "content": "hi"}],
            }))
            .send()
            .await
            .context("calling the messages API")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let hint = if status == reqwest::StatusCode::UNAUTHORIZED {
                " — token expired? re-run `claude login`"
            } else {
                ""
            };
            return Err(anyhow!(
                "API returned {status}{hint}: {}",
                body.chars().take(200).collect::<String>()
            ));
        }

        snapshot_from_headers(response.headers())
    }
}

fn header<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    headers.get(name)?.to_str().ok()
}

/// Utilisation arrives as a 0..1 fraction; the display wants whole percent.
fn percent(headers: &HeaderMap, name: &str) -> u8 {
    header(headers, name)
        .and_then(|raw| raw.parse::<f64>().ok())
        .map(|fraction| clamp_percent(fraction * 100.0))
        .unwrap_or(0)
}

/// Preserve reset stamps as UTC epoch seconds; the device's RTC owns the
/// countdown after the snapshot crosses the wire.
fn reset_at(headers: &HeaderMap, name: &str) -> i64 {
    header(headers, name)
        .and_then(|raw| raw.parse::<f64>().ok())
        .filter(|reset_at| reset_at.is_finite() && *reset_at > 0.0)
        .map(|reset_at| reset_at.round() as i64)
        .unwrap_or(0)
}

fn snapshot_from_headers(headers: &HeaderMap) -> Result<UsageSnapshot> {
    // Pro and Max accounts report rolling 5-hour and 7-day windows. Enterprise
    // accounts report a single spending limit under different headers instead,
    // which this does not model — better to say Unknown than to show a number
    // that means something else.
    let Some(_) = header(headers, "anthropic-ratelimit-unified-5h-utilization") else {
        debug!("no unified 5h headers; account is probably not Pro/Max");
        return Ok(UsageSnapshot::UNKNOWN);
    };

    let status = match header(headers, "anthropic-ratelimit-unified-5h-status") {
        Some(raw) if raw.starts_with("allowed") => UsageStatus::Allowed,
        Some(_) => UsageStatus::Limited,
        None => UsageStatus::Unknown,
    };

    Ok(UsageSnapshot {
        session_pct: percent(headers, "anthropic-ratelimit-unified-5h-utilization"),
        session_reset_at: reset_at(headers, "anthropic-ratelimit-unified-5h-reset"),
        weekly_pct: percent(headers, "anthropic-ratelimit-unified-7d-utilization"),
        weekly_reset_at: reset_at(headers, "anthropic-ratelimit-unified-7d-reset"),
        status,
    })
}

#[cfg(test)]
mod tests {
    use reqwest::header::{HeaderMap, HeaderValue};

    use super::reset_at;

    #[test]
    fn preserves_absolute_reset_epoch() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-5h-reset",
            HeaderValue::from_static("1785792600.4"),
        );

        assert_eq!(
            reset_at(&headers, "anthropic-ratelimit-unified-5h-reset"),
            1_785_792_600
        );
    }

    #[test]
    fn invalid_reset_epoch_is_unknown() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-5h-reset",
            HeaderValue::from_static("not-a-timestamp"),
        );

        assert_eq!(
            reset_at(&headers, "anthropic-ratelimit-unified-5h-reset"),
            0
        );
    }
}
