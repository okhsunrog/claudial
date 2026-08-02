//! Host daemon: pushes Claude Code usage to a Clawdmeter over BLE.
//!
//! Usage is read from the `anthropic-ratelimit-*` headers of a deliberately
//! tiny API request — there is no usage endpoint, so the completion is thrown
//! away and the headers are the payload.
//!
//! The credential is re-read on every poll rather than cached: Claude Code
//! rotates the OAuth access token. That single fact is why this runs on the
//! host instead of on a device, which would have no way to refresh a token it
//! was handed once.

mod credentials;
mod transport;
mod usage;

use std::time::Duration;

use anyhow::Result;
use clawdmeter_icd::UsageTopic;
use ergot::interface_manager::{InterfaceState, Profile};
use tracing::{info, warn};

use crate::usage::UsageClient;

/// How often usage is polled and published. Each poll is one near-free API
/// request, so this matches the upstream project's cadence rather than going
/// faster.
const POLL_INTERVAL: Duration = Duration::from_secs(60);
const SCAN_TIMEOUT: Duration = Duration::from_secs(30);
const RETRY_DELAY: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "clawdmeter_host=info".into()),
        )
        .init();

    let (stack, queue) = transport::new_stack();
    let usage = UsageClient::new()?;
    let mut workers = Vec::new();

    loop {
        if let Err(e) = session(&stack, &queue, &usage, &mut workers).await {
            warn!("{e:#}");
        }
        tokio::time::sleep(RETRY_DELAY).await;
    }
}

/// One connect-and-publish cycle, returning when the link drops.
async fn session(
    stack: &transport::Stack,
    queue: &ergot::interface_manager::utils::std::StdQueue,
    usage: &UsageClient,
    workers: &mut Vec<tokio::task::JoinHandle<()>>,
) -> Result<()> {
    let device = transport::find_device(SCAN_TIMEOUT).await?;
    transport::connect(stack, queue, &device, workers).await?;

    while link_is_up(stack) {
        // Re-read the token every poll — Claude Code rotates it, so a value
        // cached at startup eventually starts returning 401.
        match credentials::read_access_token() {
            Ok(token) => match usage.poll(&token).await {
                Ok(snapshot) => match stack.topics().broadcast::<UsageTopic>(&snapshot, None) {
                    Ok(()) => info!(
                        "session {}% (resets in {} min), weekly {}% (resets in {} min)",
                        snapshot.session_pct,
                        snapshot.session_reset_mins,
                        snapshot.weekly_pct,
                        snapshot.weekly_reset_mins
                    ),
                    Err(e) => warn!("publish failed: {e:?}"),
                },
                Err(e) => warn!("{e:#}"),
            },
            Err(e) => warn!("{e:#}"),
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    warn!("link went down");
    Ok(())
}

fn link_is_up(stack: &transport::Stack) -> bool {
    stack.manage_profile(|im| matches!(im.interface_state(()), Some(InterfaceState::Active { .. })))
}
