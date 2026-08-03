//! Runtime-selectable sources for Claude subscription usage.
//!
//! `claude-code` is the default: it authenticates with Claude Code's rotating
//! OAuth token and reads usage from the rate-limit headers of a minimal API
//! probe. `claude-proxy` reads the snapshot already cached by
//! [claude-proxy-rs], avoiding another upstream request.
//!
//! [claude-proxy-rs]: https://github.com/okhsunrog/claude-proxy-rs

mod claude_code;
mod claude_proxy;
mod proxy_credentials;

use anyhow::Result;
use clap::ValueEnum;
use claudial_icd::UsageSnapshot;

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum UsageSource {
    /// Probe Anthropic using Claude Code's local OAuth credential.
    #[default]
    ClaudeCode,
    /// Read the usage snapshot cached by claude-proxy-rs.
    ClaudeProxy,
}

pub enum UsageClient {
    ClaudeCode(claude_code::UsageClient),
    ClaudeProxy(claude_proxy::UsageClient),
}

impl UsageClient {
    pub fn new(source: UsageSource) -> Result<Self> {
        match source {
            UsageSource::ClaudeCode => Ok(Self::ClaudeCode(claude_code::UsageClient::new()?)),
            UsageSource::ClaudeProxy => Ok(Self::ClaudeProxy(claude_proxy::UsageClient::new()?)),
        }
    }

    pub async fn poll(&self) -> Result<UsageSnapshot> {
        match self {
            Self::ClaudeCode(client) => client.poll().await,
            Self::ClaudeProxy(client) => client.poll().await,
        }
    }
}

/// Round a percentage into the byte the device displays.
fn clamp_percent(percent: f64) -> u8 {
    percent.round().clamp(0.0, 100.0) as u8
}
