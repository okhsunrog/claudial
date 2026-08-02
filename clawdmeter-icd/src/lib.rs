//! Interface Control Document: everything on the wire between the Clawdmeter
//! firmware and its host daemon.
//!
//! Both sides depend on this crate, so the protocol is agreed at compile time
//! rather than at parse time — adding or reshaping a field is a type error on
//! whichever side has not caught up, instead of a field that silently reads
//! back as zero.
//!
//! # Direction
//!
//! | Path | Kind | Direction | Payload |
//! |---|---|---|---|
//! | `usage/snapshot` | topic | host → device | [`UsageSnapshot`] |
//! | `req/usage` | endpoint | device → host | `()` → [`UsageSnapshot`] |
//!
//! The host owns the data: it holds the credential, polls the API and pushes
//! a snapshot whenever it has a fresh one. The endpoint exists so the device
//! can ask for one out of band — after a reconnect, say — instead of waiting
//! out the host's poll interval.

#![no_std]

use ergot::{endpoint, topic};
use postcard_schema::Schema;
use serde::{Deserialize, Serialize};

/// How the account's limits are currently behaving.
///
/// An enum rather than the free-form string the upstream project sends, so an
/// unrecognised value cannot reach the UI as text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Schema)]
pub enum UsageStatus {
    /// Requests are going through.
    Allowed,
    /// A limit has been hit.
    Limited,
    /// The host could not determine the state.
    Unknown,
}

/// One reading of Claude Code usage, as the host sees it.
///
/// Percentages are whole numbers because that is all the display shows, and
/// reset times are minutes because the device keeps its own wall clock on the
/// PCF85063 — the host has no reason to send an epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Schema)]
pub struct UsageSnapshot {
    /// Five-hour session utilisation, 0..=100.
    pub session_pct: u8,
    /// Minutes until the session window resets.
    pub session_reset_mins: u16,
    /// Seven-day utilisation, 0..=100.
    pub weekly_pct: u8,
    /// Minutes until the weekly window resets.
    pub weekly_reset_mins: u16,
    /// What the limits are doing.
    pub status: UsageStatus,
}

impl UsageSnapshot {
    /// A snapshot representing "nothing known yet", shown before the first
    /// push arrives.
    pub const UNKNOWN: Self = Self {
        session_pct: 0,
        session_reset_mins: 0,
        weekly_pct: 0,
        weekly_reset_mins: 0,
        status: UsageStatus::Unknown,
    };
}

topic!(UsageTopic, UsageSnapshot, "usage/snapshot");
endpoint!(RefreshEndpoint, (), UsageSnapshot, "req/usage");
