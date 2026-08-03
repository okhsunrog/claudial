//! Host daemon: pushes Claude usage to a Claudial over BLE.
//!
//! Where the numbers come from is a compile-time choice between two backends —
//! see [`usage`]. Either way it runs on a host rather than on the device,
//! because both sources need a credential that rotates and the device has no
//! way to refresh one it was handed once.

#[cfg(all(feature = "direct", not(feature = "proxy")))]
mod credentials;
mod transport;
mod usage;

use std::time::Duration;

use anyhow::Result;
use claudial_icd::UsageTopic;
use ergot::interface_manager::{InterfaceState, Profile};
use tracing::{info, warn};

use crate::usage::UsageClient;

/// How often usage is polled and published. This matches the upstream
/// project's cadence rather than going faster: the `direct` backend spends a
/// (near-free) API request per poll, and the `proxy` backend would just be
/// re-reading a cache that refreshes on its own schedule.
const POLL_INTERVAL: Duration = Duration::from_secs(60);
const SCAN_TIMEOUT: Duration = Duration::from_secs(30);
const RETRY_DELAY: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "claudial_host=info".into()),
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
    if let Err(e) = transport::connect(stack, queue, &device, workers).await {
        // Most likely an adopted link that BlueZ still reports as up but whose
        // GATT is gone. Tear it down so the next cycle scans instead of
        // adopting the same dead connection forever.
        transport::disconnect(&device).await;
        return Err(e);
    }

    while link_is_up(stack) {
        match usage.poll().await {
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
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    warn!("link went down");
    Ok(())
}

fn link_is_up(stack: &transport::Stack) -> bool {
    stack.manage_profile(|im| matches!(im.interface_state(()), Some(InterfaceState::Active { .. })))
}
