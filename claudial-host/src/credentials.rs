//! Reading Claude Code's OAuth access token.
//!
//! The token is re-read from disk on every poll rather than cached at startup.
//! Claude Code rotates it, so a copy taken once goes stale — and that single
//! fact is why this daemon exists at all instead of the device talking to the
//! API itself. A device handed a token has no way to refresh it.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};

/// Where Claude Code keeps its credentials on Linux and Windows.
///
/// macOS stores them in the Keychain instead, under the service name
/// `Claude Code-credentials`, which this does not read yet.
fn credentials_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    Ok(home.join(".claude").join(".credentials.json"))
}

/// Read the current access token.
///
/// Claude Code has stored the token both directly and nested under a provider
/// key, so both shapes are accepted rather than assuming today's layout.
pub fn read_access_token() -> Result<String> {
    let path = credentials_path()?;
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;

    if let Some(token) = value.get("accessToken").and_then(|t| t.as_str()) {
        return Ok(token.to_owned());
    }
    if let Some(token) = value
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(_, nested)| nested.get("accessToken"))
        .find_map(|t| t.as_str())
    {
        return Ok(token.to_owned());
    }

    Err(anyhow!(
        "no accessToken in {} — is Claude Code logged in?",
        path.display()
    ))
}
