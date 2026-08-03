//! Reading Claude usage from a [claude-proxy-rs] instance.
//!
//! The proxy already keeps a subscription snapshot for its own rate limiting
//! and admin UI, so this just asks it for that snapshot instead of spending an
//! API request of its own. Anthropic's usage endpoint is aggressively rate
//! limited; the proxy caches it and can fall back to a claude.ai web session,
//! and reading through it inherits both.
//!
//! This is the same endpoint [claude-plasmoid] reads, so the two agree by
//! construction.
//!
//! [claude-proxy-rs]: https://github.com/okhsunrog/claude-proxy-rs
//! [claude-plasmoid]: https://github.com/okhsunrog/claude-plasmoid

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use claudial_icd::{UsageSnapshot, UsageStatus};
use serde::Deserialize;
use tracing::{debug, warn};

use super::{clamp_minutes, clamp_percent};

const URL_VAR: &str = "CLAUDIAL_PROXY_URL";
const USERNAME_VAR: &str = "CLAUDIAL_PROXY_USERNAME";
const PASSWORD_VAR: &str = "CLAUDIAL_PROXY_PASSWORD";

pub struct UsageClient {
    http: reqwest::Client,
    url: String,
    username: String,
    password: String,
}

impl UsageClient {
    /// Read the proxy's address and admin credentials from the environment.
    ///
    /// Failing here rather than on the first poll means a missing variable is
    /// reported at startup, before the daemon settles into its retry loop.
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .context("building HTTP client")?;

        Ok(Self {
            // Accept a base URL with or without a trailing `/admin`, since
            // that is the address a user has bookmarked for the admin UI.
            url: env(URL_VAR)?
                .trim_end_matches('/')
                .trim_end_matches("/admin")
                .to_owned(),
            username: env(USERNAME_VAR)?,
            password: env(PASSWORD_VAR)?,
            http,
        })
    }

    pub async fn poll(&self) -> Result<UsageSnapshot> {
        // Deliberately not `?force=true`: the plain endpoint honours the
        // proxy's freshness throttle, which exists because Anthropic rate
        // limits the upstream hard. Forcing it once a minute would defeat the
        // cache this backend exists to reuse.
        let url = format!("{}/admin/oauth/usage", self.url);
        let response = self
            .http
            .get(&url)
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
            .with_context(|| format!("calling {url}"))?;

        let status = response.status();
        if !status.is_success() {
            let hint = if status == reqwest::StatusCode::UNAUTHORIZED {
                " — check the admin username and password"
            } else {
                ""
            };
            return Err(anyhow!("proxy returned {status}{hint}"));
        }

        let usage: SubscriptionUsage = response
            .json()
            .await
            .context("decoding the proxy's usage response")?;
        Ok(usage.into_snapshot())
    }
}

fn env(name: &str) -> Result<String> {
    std::env::var(name).map_err(|_| anyhow!("{name} is not set"))
}

/// The fields of the proxy's `SubscriptionUsageResponse` this needs.
///
/// Serde ignores the rest — per-model limits, extra-usage credits, cache
/// metadata — which the device has nowhere to show.
#[derive(Deserialize)]
struct SubscriptionUsage {
    five_hour: Option<UsageLimit>,
    seven_day: Option<UsageLimit>,
    /// Set when the proxy's own last fetch failed; the numbers below are then
    /// the last good ones rather than current.
    #[serde(default)]
    upstream_error: Option<String>,
}

#[derive(Deserialize)]
struct UsageLimit {
    /// Whole percent, 0..100 — unlike the rate-limit headers the `direct`
    /// backend reads, which are 0..1 fractions.
    utilization: Option<f64>,
    /// RFC 3339 timestamp.
    resets_at: Option<String>,
}

impl UsageLimit {
    fn percent(&self) -> u8 {
        self.utilization.map(clamp_percent).unwrap_or(0)
    }

    fn reset_mins(&self) -> u16 {
        self.resets_at
            .as_deref()
            .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
            .map(|at| {
                clamp_minutes((at.with_timezone(&Utc) - Utc::now()).num_seconds() as f64 / 60.0)
            })
            .unwrap_or(0)
    }
}

impl SubscriptionUsage {
    fn into_snapshot(self) -> UsageSnapshot {
        if let Some(error) = &self.upstream_error {
            warn!("proxy's last upstream fetch failed, numbers may be stale: {error}");
        }

        // A proxy that has never completed a fetch answers with every field
        // null. Reporting Unknown is better than showing a confident 0%.
        let Some(five_hour) = self.five_hour else {
            debug!("proxy has no five_hour window yet");
            return UsageSnapshot::UNKNOWN;
        };
        let seven_day = self.seven_day.unwrap_or(UsageLimit {
            utilization: None,
            resets_at: None,
        });

        // The proxy has no allowed/limited flag of its own; it decides the
        // same way, by whether either window has run out.
        let status = if five_hour.percent() >= 100 || seven_day.percent() >= 100 {
            UsageStatus::Limited
        } else {
            UsageStatus::Allowed
        };

        UsageSnapshot {
            session_pct: five_hour.percent(),
            session_reset_mins: five_hour.reset_mins(),
            weekly_pct: seven_day.percent(),
            weekly_reset_mins: seven_day.reset_mins(),
            status,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one thing worth pinning: the proxy reports whole percent and an
    /// RFC 3339 reset, where the `direct` backend gets a 0..1 fraction and
    /// epoch seconds. Reading one as the other is silent and plausible.
    #[test]
    fn reads_percent_and_rfc3339() {
        let resets_at = (Utc::now() + chrono::Duration::minutes(90)).to_rfc3339();
        let raw = format!(
            r#"{{"five_hour":{{"utilization":21.4,"resets_at":"{resets_at}"}},
                 "seven_day":{{"utilization":80.0,"resets_at":null}}}}"#
        );

        let snapshot = serde_json::from_str::<SubscriptionUsage>(&raw)
            .unwrap()
            .into_snapshot();

        assert_eq!(snapshot.session_pct, 21);
        assert_eq!(snapshot.weekly_pct, 80);
        assert_eq!(snapshot.session_reset_mins, 90);
        assert_eq!(snapshot.weekly_reset_mins, 0);
        assert_eq!(snapshot.status, UsageStatus::Allowed);
    }

    /// A proxy that has not fetched yet answers with nulls throughout.
    #[test]
    fn empty_snapshot_is_unknown() {
        let snapshot = serde_json::from_str::<SubscriptionUsage>(r#"{"five_hour":null}"#)
            .unwrap()
            .into_snapshot();
        assert_eq!(snapshot.status, UsageStatus::Unknown);
    }
}
