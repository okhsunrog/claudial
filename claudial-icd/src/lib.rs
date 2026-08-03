//! Interface Control Document: everything on the wire between the Claudial
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
//! | `clock/sync` | topic | host → device | [`ClockSync`] |
//! | `usage/snapshot` | topic | host → device | [`UsageSnapshot`] |
//!
//! The host owns the data: it holds the credential, polls the API, synchronizes
//! the RTC after a connection and pushes each fresh usage snapshot.

#![no_std]

use ergot::topic;
use postcard_schema::Schema;
use serde::{Deserialize, Serialize};

// Not on the wire, but derived from what is: the device turns the stream of
// snapshots below into a pace. It lives here rather than in the firmware so
// the time-aware arithmetic can be unit-tested on the host.
pub mod pace;
// Display policy is likewise platform-independent. Keeping its clamping and
// versioned storage value here gives the no_std firmware host-run tests.
pub mod settings;

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

/// UTC wall-clock state supplied by the host.
///
/// The RTC stores UTC so absolute usage deadlines can be compared without
/// timezone ambiguity. The offset is stored separately and is only applied
/// when the firmware renders the local clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Schema)]
pub struct ClockSync {
    /// Seconds since the Unix epoch, in UTC.
    pub unix_seconds: i64,
    /// The host's current local offset from UTC, including daylight saving.
    pub utc_offset_minutes: i16,
}

/// One reading of Claude Code usage, as the host sees it.
///
/// Percentages are whole numbers because that is all the display shows. Reset
/// times stay absolute on the wire so the battery-backed RTC can keep their
/// countdowns moving between host updates and across device resets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Schema)]
pub struct UsageSnapshot {
    /// Five-hour session utilisation, 0..=100.
    pub session_pct: u8,
    /// Session reset time as UTC Unix seconds, or zero when unknown.
    pub session_reset_at: i64,
    /// Seven-day utilisation, 0..=100.
    pub weekly_pct: u8,
    /// Weekly reset time as UTC Unix seconds, or zero when unknown.
    pub weekly_reset_at: i64,
    /// What the limits are doing.
    pub status: UsageStatus,
}

impl UsageSnapshot {
    /// A snapshot representing "nothing known yet", shown before the first
    /// push arrives.
    pub const UNKNOWN: Self = Self {
        session_pct: 0,
        session_reset_at: 0,
        weekly_pct: 0,
        weekly_reset_at: 0,
        status: UsageStatus::Unknown,
    };
}

/// Convert an absolute deadline into a display countdown.
///
/// Partial minutes round up: a deadline 20 seconds away should still read
/// `1m`, not disappear as though it were unknown. Values outside the display's
/// range saturate rather than wrapping.
pub fn minutes_until(reset_at: i64, now: i64) -> u16 {
    if reset_at <= 0 || reset_at <= now {
        return 0;
    }

    let seconds = reset_at.saturating_sub(now);
    let minutes = seconds.saturating_add(59) / 60;
    minutes.min(i64::from(u16::MAX)) as u16
}

topic!(ClockSyncTopic, ClockSync, "clock/sync");
topic!(UsageTopic, UsageSnapshot, "usage/snapshot");

#[cfg(test)]
mod tests {
    use super::minutes_until;

    #[test]
    fn countdown_rounds_partial_minutes_up() {
        assert_eq!(minutes_until(1_001, 1_000), 1);
        assert_eq!(minutes_until(1_060, 1_000), 1);
        assert_eq!(minutes_until(1_061, 1_000), 2);
    }

    #[test]
    fn countdown_rejects_unknown_and_past_deadlines() {
        assert_eq!(minutes_until(0, 1_000), 0);
        assert_eq!(minutes_until(999, 1_000), 0);
        assert_eq!(minutes_until(1_000, 1_000), 0);
    }

    #[test]
    fn countdown_saturates_to_the_wire_display_range() {
        assert_eq!(minutes_until(i64::MAX, 1), u16::MAX);
    }
}
