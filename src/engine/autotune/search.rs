//! Types shared by the two search strategies ([`super::thorough`] and
//! [`super::fast`]), plus the pure helpers the tuner measures with.

use std::fmt;
use std::time::Instant;

/// One point of the search. `None` = the key is pinned by `override_config`
/// (the tuner neither sets nor varies it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    pub analysis_threads: Option<u32>,
    pub servers: Option<u32>,
    pub batch: Option<u32>,
}

impl fmt::Display for Candidate {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let show =
            |val: Option<u32>| val.map_or_else(|| "pinned".to_owned(), |num| num.to_string());
        write!(
            out,
            "analysisThreads={} servers={} batch={}",
            show(self.analysis_threads),
            show(self.servers),
            show(self.batch)
        )
    }
}

/// How one trial went.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Full window measured: positions/s.
    Measured(f64),
    /// Stopped half-way, clearly slower than the best so far: positions/s then.
    Pruned(f64),
    /// KataGo failed to start or died (e.g. out of GPU memory at this batch).
    Failed(String),
}

/// What [`run`] found: the winner and its positions/s (0 when nothing was
/// measured), or `None` when every trial failed; plus every trial in order.
#[derive(Debug, Default)]
pub struct Report {
    pub best: Option<(Candidate, f64)>,
    pub history: Vec<(Candidate, Outcome)>,
}

/// Positions/s over a trial's measured window, from the running count of
/// analyzed positions.
#[derive(Debug, Clone, Copy)]
pub struct Meter {
    start: (Instant, u64),
    last: (Instant, u64),
}

impl Meter {
    #[must_use]
    pub fn start(at: Instant, count: u64) -> Self {
        Self {
            start: (at, count),
            last: (at, count),
        }
    }

    pub fn record(&mut self, at: Instant, count: u64) {
        self.last = (at, count);
    }

    /// Positions analyzed since the window opened.
    #[must_use]
    pub fn positions(&self) -> u64 {
        self.last.1.saturating_sub(self.start.1)
    }

    #[allow(
        clippy::cast_precision_loss,
        reason = "position counts stay far below 2^52"
    )]
    #[must_use]
    pub fn rate(&self) -> f64 {
        let secs = self.last.0.duration_since(self.start.0).as_secs_f64();
        if secs <= 0.0 {
            return 0.0;
        }
        self.positions() as f64 / secs
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn meter_measures_the_window_only() {
        let t0 = Instant::now();
        let mut meter = Meter::start(t0, 160);
        assert!(meter.rate().abs() < f64::EPSILON);
        meter.record(t0 + Duration::from_secs(2), 360);
        assert_eq!(meter.positions(), 200);
        assert!((meter.rate() - 100.0).abs() < 1e-9);
    }
}
