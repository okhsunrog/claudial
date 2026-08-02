//! Host daemon: pushes Claude Code usage to a Clawdmeter over BLE.
//!
//! The usage source is not wired up yet — this publishes a synthetic snapshot
//! so the link can be exercised end to end before the API polling lands. When
//! it does, the shape is already fixed by how the credential works: Claude Code
//! rotates the OAuth access token, so it has to be re-read from the credential
//! store on every poll rather than cached at startup. That is the whole reason
//! this runs on the host rather than on a device that cannot refresh a token it
//! was handed once.

mod transport;

use std::time::Duration;

use anyhow::Result;
use clawdmeter_icd::{UsageSnapshot, UsageStatus, UsageTopic};
use ergot::interface_manager::{InterfaceState, Profile};
use tracing::{info, warn};

/// How often a snapshot goes out. The upstream project polls every 60 s; the
/// synthetic source has no reason to be slower.
const PUBLISH_INTERVAL: Duration = Duration::from_secs(5);
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
    let mut workers = Vec::new();

    warn!("publishing synthetic usage — API polling is not implemented yet");

    loop {
        if let Err(e) = session(&stack, &queue, &mut workers).await {
            warn!("{e:#}");
        }
        tokio::time::sleep(RETRY_DELAY).await;
    }
}

/// One connect-and-publish cycle, returning when the link drops.
async fn session(
    stack: &transport::Stack,
    queue: &ergot::interface_manager::utils::std::StdQueue,
    workers: &mut Vec<tokio::task::JoinHandle<()>>,
) -> Result<()> {
    let device = transport::find_device(SCAN_TIMEOUT).await?;
    transport::connect(stack, queue, &device, workers).await?;

    let mut tick = 0_u32;
    while link_is_up(stack) {
        let snapshot = synthetic_snapshot(tick);
        match stack.topics().broadcast::<UsageTopic>(&snapshot, None) {
            Ok(()) => info!(
                "published session {}% weekly {}%",
                snapshot.session_pct, snapshot.weekly_pct
            ),
            Err(e) => warn!("publish failed: {e:?}"),
        }
        tick = tick.wrapping_add(1);
        tokio::time::sleep(PUBLISH_INTERVAL).await;
    }

    warn!("link went down");
    Ok(())
}

fn link_is_up(stack: &transport::Stack) -> bool {
    stack.manage_profile(|im| matches!(im.interface_state(()), Some(InterfaceState::Active { .. })))
}

/// A moving synthetic reading, so the display visibly changes between pushes
/// and a stuck value is obvious.
fn synthetic_snapshot(tick: u32) -> UsageSnapshot {
    let session = (tick * 7 % 101) as u8;
    UsageSnapshot {
        session_pct: session,
        session_reset_mins: 300 - u16::from(session).min(299),
        weekly_pct: (tick * 3 % 101) as u8,
        weekly_reset_mins: 7200 - (tick % 60) as u16,
        status: if session > 90 {
            UsageStatus::Limited
        } else {
            UsageStatus::Allowed
        },
    }
}
