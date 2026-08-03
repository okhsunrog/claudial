//! Spend history over the session window.
//!
//! The percentage on the dial is a level, and a level cannot answer the
//! question that actually matters when you are deciding whether to keep
//! going: 40 % reached in half an hour and 40 % reached over four hours look
//! identical on the ring. So keep the last session window's worth of samples
//! and derive a rate from them.
//!
//! What is stored is the per-minute *increase*, not the level. Deltas are what
//! the sparkline draws and what the rate label sums, and storing them directly
//! means the rolling window costs 300 bytes and no arithmetic at read time
//! beyond addition.

/// Columns in the sparkline.
pub const BUCKETS: usize = 60;
/// Minutes per column. 60 x 5 spans the five-hour session window, so the chart
/// and the ring above it cover exactly the same period and reset together.
const MINUTES_PER_BUCKET: usize = 5;
const SAMPLES: usize = BUCKETS * MINUTES_PER_BUCKET;

/// Minutes of history the rate label averages over. Short enough to react
/// within a working stretch, long enough that one busy minute does not read as
/// a crisis.
const RATE_WINDOW_MINUTES: usize = 30;

/// Minutes of history before a rate is worth showing at all. One or two
/// samples scale up to wildly confident numbers — a single busy minute would
/// read as 60 %/h — so say nothing until the average means something.
const MINIMUM_SAMPLES: usize = 5;

pub struct History {
    /// Percent gained in each of the last [`SAMPLES`] minutes, oldest first
    /// once wrapped.
    deltas: [u8; SAMPLES],
    head: usize,
    len: usize,
    previous: Option<u8>,
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

impl History {
    pub const fn new() -> Self {
        Self {
            deltas: [0; SAMPLES],
            head: 0,
            len: 0,
            previous: None,
        }
    }

    /// Record a fresh session percentage.
    ///
    /// A drop means the five-hour window rolled over, which is not spending;
    /// it records as zero rather than as a negative that would have to be
    /// special-cased everywhere downstream.
    pub fn push(&mut self, percent: u8) {
        // The first sample after boot has nothing to compare against. It must
        // record no delta at all rather than a zero one: a zero would sit in
        // the window as if it were a measured idle minute and drag the average
        // down for the next half hour.
        if let Some(previous) = self.previous {
            self.deltas[self.head] = percent.saturating_sub(previous);
            self.head = (self.head + 1) % SAMPLES;
            self.len = (self.len + 1).min(SAMPLES);
        }
        self.previous = Some(percent);
    }

    /// The `n`th most recent sample, 0 being the newest.
    fn recent(&self, n: usize) -> u8 {
        if n >= self.len {
            return 0;
        }
        self.deltas[(self.head + SAMPLES - 1 - n) % SAMPLES]
    }

    /// Percent per hour over the last [`RATE_WINDOW_MINUTES`], or `None` until
    /// there is enough history to mean anything.
    pub fn rate_per_hour(&self) -> Option<u16> {
        if self.len < MINIMUM_SAMPLES {
            return None;
        }
        let window = RATE_WINDOW_MINUTES.min(self.len);
        let spent: u16 = (0..window).map(|n| u16::from(self.recent(n))).sum();
        // Scale the partial window up to an hour rather than reporting a rate
        // that silently means "per however long we have been running".
        Some(spent * 60 / window as u16)
    }

    /// Sparkline columns, oldest first, each normalised against the busiest
    /// column.
    ///
    /// Normalising against the window's own maximum rather than an absolute
    /// keeps the shape readable on a quiet day, when every column would
    /// otherwise round to nothing.
    pub fn buckets(&self) -> [f32; BUCKETS] {
        let mut totals = [0_u16; BUCKETS];
        for (bucket, total) in totals.iter_mut().enumerate() {
            // Bucket 0 is the oldest, so count back from the newest sample.
            let newest = (BUCKETS - 1 - bucket) * MINUTES_PER_BUCKET;
            *total = (newest..newest + MINUTES_PER_BUCKET)
                .map(|n| u16::from(self.recent(n)))
                .sum();
        }

        let peak = totals.iter().copied().max().unwrap_or(0);
        let mut out = [0.0; BUCKETS];
        if peak == 0 {
            return out;
        }
        for (slot, total) in out.iter_mut().zip(totals) {
            *slot = f32::from(total) / f32::from(peak);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_has_no_rate() {
        let mut history = History::new();
        history.push(40);
        assert_eq!(history.rate_per_hour(), None);
    }

    #[test]
    fn rate_scales_a_partial_window_to_an_hour() {
        let mut history = History::new();
        history.push(0);
        // Ten minutes at one percent a minute is sixty percent an hour.
        for percent in 1..=10 {
            history.push(percent);
        }
        assert_eq!(history.rate_per_hour(), Some(60));
    }

    #[test]
    fn a_window_rollover_is_not_spending() {
        let mut climbing = History::new();
        let mut rolling = History::new();
        for percent in [1, 2, 3, 4, 5, 6] {
            climbing.push(percent);
            rolling.push(percent);
        }

        // The five-hour window resets: the level collapses, but nothing was
        // spent in that minute and the rate must not move as though it had.
        rolling.push(0);
        climbing.push(7);

        assert_eq!(climbing.rate_per_hour(), Some(60));
        assert_eq!(rolling.rate_per_hour(), Some(50));
    }

    #[test]
    fn buckets_put_the_newest_last() {
        let mut history = History::new();
        history.push(0);
        for percent in 1..=5 {
            history.push(percent);
        }
        let buckets = history.buckets();
        assert_eq!(buckets[BUCKETS - 1], 1.0);
        assert_eq!(buckets[0], 0.0);
    }
}
