//! Session spending pace derived from timestamped usage snapshots.
//!
//! Claude reports whole percentages, so a single delta is noisy. Keep enough
//! timed intervals to average over half an hour, and refuse to bridge a long
//! outage: a change observed after a disconnect says nothing about when the
//! spending happened.

const WINDOW_SECONDS: u32 = 30 * 60;
const MINIMUM_COVERAGE_SECONDS: u32 = 5 * 60;
const MAXIMUM_SAMPLE_GAP_SECONDS: u32 = 3 * 60;
const SEGMENTS: usize = 64;
const SCALE: u64 = 1_000_000;

#[derive(Clone, Copy)]
struct Segment {
    delta: u8,
    elapsed_seconds: u32,
}

impl Segment {
    const EMPTY: Self = Self {
        delta: 0,
        elapsed_seconds: 0,
    };
}

/// A fixed-size, no-allocation pace estimator.
pub struct Pace {
    segments: [Segment; SEGMENTS],
    head: usize,
    len: usize,
    previous: Option<u8>,
}

impl Default for Pace {
    fn default() -> Self {
        Self::new()
    }
}

impl Pace {
    pub const fn new() -> Self {
        Self {
            segments: [Segment::EMPTY; SEGMENTS],
            head: 0,
            len: 0,
            previous: None,
        }
    }

    /// Record a percentage and the real number of seconds since the previous
    /// reading.
    ///
    /// The first reading establishes a baseline. A gap longer than three
    /// expected polls establishes a new one, because spreading its delta over
    /// the outage would manufacture a pace from data that was never observed.
    pub fn record(&mut self, percent: u8, elapsed_seconds: u32) {
        let Some(previous) = self.previous.replace(percent) else {
            return;
        };

        if elapsed_seconds == 0 || elapsed_seconds > MAXIMUM_SAMPLE_GAP_SECONDS {
            self.clear_intervals();
            return;
        }

        self.segments[self.head] = Segment {
            // A drop is a rolling-window reset, not negative spending.
            delta: percent.saturating_sub(previous),
            elapsed_seconds,
        };
        self.head = (self.head + 1) % SEGMENTS;
        self.len = (self.len + 1).min(SEGMENTS);
    }

    /// Whole percent per hour over at most the last half hour.
    ///
    /// Returns nothing until five minutes of continuous observations exist.
    /// The calculation uses the measured duration of every interval rather
    /// than assuming that the host delivered it exactly on a minute boundary.
    pub fn rate_per_hour(&self) -> Option<u16> {
        let mut covered_seconds = 0_u32;
        let mut scaled_spent = 0_u64;

        for n in 0..self.len {
            if covered_seconds == WINDOW_SECONDS {
                break;
            }

            let segment = self.recent(n);
            let included = segment
                .elapsed_seconds
                .min(WINDOW_SECONDS - covered_seconds);
            scaled_spent += u64::from(segment.delta) * u64::from(included) * SCALE
                / u64::from(segment.elapsed_seconds);
            covered_seconds += included;
        }

        if covered_seconds < MINIMUM_COVERAGE_SECONDS {
            return None;
        }

        let scaled_rate = scaled_spent * 60 * 60 / u64::from(covered_seconds);
        let rounded = (scaled_rate + SCALE / 2) / SCALE;
        Some(rounded.min(u64::from(u16::MAX)) as u16)
    }

    /// Seconds still needed before the estimator can report a useful rate.
    pub fn readiness_remaining_seconds(&self) -> u32 {
        MINIMUM_COVERAGE_SECONDS.saturating_sub(self.covered_seconds())
    }

    /// Continuous time since the last observed whole-percent increase.
    ///
    /// The source only reports whole percentages, so this deliberately means
    /// "unchanged", not "no activity". Small usage may be hidden by rounding.
    pub fn unchanged_seconds(&self) -> u32 {
        let mut unchanged = 0_u32;

        for n in 0..self.len {
            if unchanged == WINDOW_SECONDS {
                break;
            }

            let segment = self.recent(n);
            if segment.delta > 0 {
                break;
            }
            unchanged += segment.elapsed_seconds.min(WINDOW_SECONDS - unchanged);
        }

        unchanged
    }

    fn covered_seconds(&self) -> u32 {
        let mut covered = 0_u32;
        for n in 0..self.len {
            if covered == WINDOW_SECONDS {
                break;
            }
            covered += self.recent(n).elapsed_seconds.min(WINDOW_SECONDS - covered);
        }
        covered
    }

    fn recent(&self, n: usize) -> Segment {
        self.segments[(self.head + SEGMENTS - 1 - n) % SEGMENTS]
    }

    fn clear_intervals(&mut self) {
        self.head = 0;
        self.len = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_has_no_rate() {
        let mut pace = Pace::new();
        pace.record(40, 0);
        assert_eq!(pace.rate_per_hour(), None);
    }

    #[test]
    fn rate_uses_measured_intervals() {
        let mut pace = Pace::new();
        pace.record(0, 0);
        for (percent, seconds) in [(1, 60), (2, 120), (3, 60), (4, 120), (5, 120)] {
            pace.record(percent, seconds);
        }

        // Five percent over eight measured minutes is 37.5 percent per hour.
        assert_eq!(pace.rate_per_hour(), Some(38));
    }

    #[test]
    fn long_gap_discards_the_old_pace() {
        let mut pace = Pace::new();
        pace.record(0, 0);
        for percent in 1..=5 {
            pace.record(percent, 60);
        }
        assert_eq!(pace.rate_per_hour(), Some(60));

        pace.record(20, MAXIMUM_SAMPLE_GAP_SECONDS + 1);
        assert_eq!(pace.rate_per_hour(), None);
    }

    #[test]
    fn a_window_rollover_is_not_spending() {
        let mut pace = Pace::new();
        pace.record(95, 0);
        for percent in [96, 97, 98, 99, 100] {
            pace.record(percent, 60);
        }
        pace.record(0, 60);

        assert_eq!(pace.rate_per_hour(), Some(50));
    }

    #[test]
    fn rate_uses_only_the_last_half_hour() {
        let mut pace = Pace::new();
        pace.record(0, 0);
        for minute in 1..=35 {
            pace.record(minute, 60);
        }

        assert_eq!(pace.rate_per_hour(), Some(60));
    }

    #[test]
    fn readiness_counts_down_with_observed_time() {
        let mut pace = Pace::new();
        pace.record(10, 0);
        pace.record(10, 60);
        pace.record(10, 120);

        assert_eq!(pace.readiness_remaining_seconds(), 120);

        pace.record(10, 120);
        assert_eq!(pace.readiness_remaining_seconds(), 0);
        assert_eq!(pace.rate_per_hour(), Some(0));
    }

    #[test]
    fn unchanged_time_stops_at_the_last_increase() {
        let mut pace = Pace::new();
        pace.record(10, 0);
        pace.record(11, 60);
        pace.record(11, 60);
        pace.record(11, 120);

        assert_eq!(pace.unchanged_seconds(), 180);
    }

    #[test]
    fn long_gap_resets_readiness_and_unchanged_time() {
        let mut pace = Pace::new();
        pace.record(10, 0);
        pace.record(10, 60);
        pace.record(10, 60);
        pace.record(12, MAXIMUM_SAMPLE_GAP_SECONDS + 1);

        assert_eq!(pace.readiness_remaining_seconds(), MINIMUM_COVERAGE_SECONDS);
        assert_eq!(pace.unchanged_seconds(), 0);
    }
}
