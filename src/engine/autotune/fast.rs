//! The fast search (`[engine] autotune_mode = "fast"`, `katago-ws autotune
//! --fast`): a few trials, guided by the hardware and the backend.
//!
//! The space is one-dimensional on purpose. Measured on an RX 6800 XT:
//!
//! - **Positions in flight** (`numNNServerThreadsPerModel` × `nnMaxBatchSize`,
//!   and `numAnalysisThreads` to feed them) only has to be big enough to keep
//!   the GPU busy; beyond that, more changes nothing. It is derived, not
//!   searched: from the GPU's SIMD lanes when known, capped by the queue depth
//!   the worker's in-flight games provide.
//! - **How that total splits into server threads** is the lever: katago-rs's
//!   NN server threads are tasks on its one search thread, each with a batch
//!   in flight, so the split decides how well the GPU stays fed (8 server
//!   threads won on ROCm). It is searched along a doubling ladder (1, 2, 4,
//!   8, 16 server threads) starting from the backend's usual winner, climbing
//!   while it improves.
//!
//! - **Each server's batch cap** (`nnMaxBatchSize`) is its share of the total but
//!   at least [`HEADROOM_BATCH`] (and at most the total): the servers drain one
//!   queue unevenly, so a cap at exactly the share leaves positions waiting
//!   (ROCm, b18c384nbt: 8 servers x 64 did 13.3 positions/s, 8 x 32 13.0; the
//!   thorough search finds 8 x 64 on ROCm and 4 x 64 on Vulkan).
//!
//! Trials are expensive (an engine restart each), so beyond the first two the
//! search stops as soon as the next trial wouldn't fit the time budget,
//! trusting the prior.

use std::future::Future;

use crate::config::EngineBackend;

use super::hw::Hardware;
use super::overrides::{ANALYSIS_THREADS, BATCH_SIZE, Overrides, SERVER_THREADS};
use super::search::{Candidate, Outcome, Report};

/// Server-thread counts tried, as a ladder walked from the backend's prior.
const SERVER_LADDER: [u32; 5] = [1, 2, 4, 8, 16];

/// Batches below this are too small to be worth a server thread.
const MIN_BATCH: u32 = 8;

/// The smallest batch cap a server thread gets (module docs).
const HEADROOM_BATCH: u32 = 64;

/// `nnMaxBatchSize` for `servers` server threads sharing `total` positions in flight: the share, at least
/// [`HEADROOM_BATCH`], at most `total`.
fn batch_cap(total: u32, servers: u32) -> u32 {
    (total / servers).max(HEADROOM_BATCH.min(total)).max(1)
}

/// Usual winners: ROCm → 8 server threads (measured), wgpu → 4 (not yet).
const ROCM_PRIOR: u32 = 8;
const WGPU_PRIOR: u32 = 4;

/// A trial running below this fraction of the best so far is stopped half-way.
const PRUNE_RATIO: f64 = 0.92;

/// A step must beat the best so far by more than this to move the search: a
/// single window varies by a few percent, and near-ties keep the prior.
const TIE: f64 = 0.03;

/// Positions the GPU should have in flight: ~1 per 16 SIMD lanes (256 on a
/// 72-CU RX 6800 XT), as a power of two in 128..=512, and never more than the
/// queue holds (`depth`). Unknown lanes → 256.
#[must_use]
pub fn in_flight(gpu_lanes: Option<u32>, depth: usize) -> u32 {
    let want = gpu_lanes.map_or(256, |lanes| floor_pow2(lanes / 16).clamp(128, 512));
    let room = floor_pow2(u32::try_from(depth).unwrap_or(u32::MAX)).max(64);
    want.min(room)
}

fn floor_pow2(num: u32) -> u32 {
    if num == 0 {
        0
    } else {
        1 << (31 - num.leading_zeros())
    }
}

/// The search space left once pinned keys are taken out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Space {
    /// Positions in flight (also `numAnalysisThreads` unless pinned).
    total: u32,
    analysis_threads: Option<u32>,
    /// `(servers, batch)` in ladder order.
    ladder: Vec<(Option<u32>, Option<u32>)>,
    /// Index into `ladder` to start from.
    prior: usize,
}

impl Space {
    /// The candidates for everything `pinned` leaves open, with `depth`
    /// positions kept queued, on `backend`.
    #[must_use]
    pub fn new(pinned: &Overrides, depth: usize, backend: EngineBackend, hw: Hardware) -> Self {
        let total = in_flight(hw.gpu_lanes, depth);
        let pinned_num = |key: &str| pinned.get(key).map(|val| val.parse::<u32>().ok());
        let analysis_threads = pinned.get(ANALYSIS_THREADS).is_none().then_some(total);
        let ladder: Vec<(Option<u32>, Option<u32>)> =
            match (pinned_num(SERVER_THREADS), pinned_num(BATCH_SIZE)) {
                (Some(_), Some(_)) => vec![(None, None)],
                // One of the pair pinned: the other follows from the total.
                (Some(servers), None) => {
                    vec![(
                        None,
                        servers
                            .filter(|&srv| srv > 0)
                            .map(|srv| batch_cap(total, srv)),
                    )]
                }
                (None, Some(batch)) => {
                    vec![(
                        batch.filter(|&bat| bat > 0).map(|bat| (total / bat).max(1)),
                        None,
                    )]
                }
                // Not capped by the CPU count: katago-rs's server threads are
                // tasks on one OS thread, not threads of their own.
                (None, None) => SERVER_LADDER
                    .iter()
                    .copied()
                    .filter(|&srv| srv == 1 || total / srv >= MIN_BATCH)
                    .map(|srv| (Some(srv), Some(batch_cap(total, srv))))
                    .collect(),
            };
        let want = match backend {
            EngineBackend::Rocm => ROCM_PRIOR,
            EngineBackend::Wgpu | EngineBackend::Flex | EngineBackend::Upstream => WGPU_PRIOR,
        };
        // The prior, or the closest rung below it that survived the filters.
        let prior = ladder
            .iter()
            .rposition(|&(srv, _)| srv.is_none_or(|srv| srv <= want))
            .unwrap_or(0);
        Self {
            total,
            analysis_threads,
            ladder,
            prior,
        }
    }

    /// Positions kept in flight.
    #[must_use]
    pub fn total(&self) -> u32 {
        self.total
    }

    /// Nothing to measure: the split is pinned (the rest is derived).
    #[must_use]
    pub fn is_fixed(&self) -> bool {
        self.ladder.len() <= 1
    }

    fn candidate(&self, idx: usize) -> Option<Candidate> {
        let &(servers, batch) = self.ladder.get(idx)?;
        Some(Candidate {
            analysis_threads: self.analysis_threads,
            servers,
            batch,
        })
    }
}

/// Walk the ladder from the prior: measure it, then step up (more server
/// threads, smaller batches) while each step wins by more than [`TIE`]; when a
/// step loses, try the other direction once. The first two trials always run;
/// after that `may_continue()` is asked before each one (the time budget).
/// `measure(candidate, prune_below)` runs one trial and may stop early once
/// the rate is clearly below `prune_below`.
pub async fn run<F, Fut>(
    space: &Space,
    mut measure: F,
    mut may_continue: impl FnMut() -> bool,
) -> Report
where
    F: FnMut(Candidate, Option<f64>) -> Fut,
    Fut: Future<Output = Outcome>,
{
    let mut report = Report::default();
    let Some(start) = space.candidate(space.prior) else {
        return report;
    };
    if space.is_fixed() {
        // Nothing to compare: derived keys (and the device) are all there is.
        report.best = Some((start, 0.0));
        return report;
    }

    let mut measured = vec![false; space.ladder.len()];
    let mut best: Option<(usize, f64)> = None;
    let mut at = space.prior;
    let mut step: isize = 1;
    let mut flipped = false;
    let mut trials = 0;
    // The prior first; it can't be pruned (nothing to compare with yet).
    let mut next = Some(space.prior);
    while let Some(idx) = next {
        if trials >= 2 && !may_continue() {
            break;
        }
        let Some(cand) = space.candidate(idx) else {
            break;
        };
        let prune_below = best.map(|(_, rate)| rate * PRUNE_RATIO);
        let outcome = measure(cand, prune_below).await;
        trials += 1;
        measured[idx] = true;
        let won = match (&outcome, best) {
            (Outcome::Measured(rate), None) => {
                best = Some((idx, *rate));
                true
            }
            (Outcome::Measured(rate), Some((_, top))) if *rate > top * (1.0 + TIE) => {
                best = Some((idx, *rate));
                true
            }
            _ => false,
        };
        report.history.push((cand, outcome));
        if won {
            at = idx;
        } else if best.is_some() && !flipped {
            // This direction lost: look once on the other side of the best.
            flipped = true;
            step = -step;
        } else if best.is_some() {
            break;
        }
        next = at
            .checked_add_signed(step)
            .filter(|&idx| idx < space.ladder.len() && !measured.get(idx).copied().unwrap_or(true));
        if next.is_none() && !flipped && best.is_some() {
            // Ran off the end going up: try below the best instead.
            flipped = true;
            step = -step;
            next = at
                .checked_add_signed(step)
                .filter(|&idx| !measured.get(idx).copied().unwrap_or(true));
        }
        if best.is_none() && next.is_none() {
            // The prior failed outright: fall back down the ladder.
            next = measured.iter().position(|done| !done);
        }
    }
    report.best = best.and_then(|(idx, rate)| Some((space.candidate(idx)?, rate)));
    report
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const RX6800XT: Hardware = Hardware {
        cpus: 12,
        gpu_lanes: Some(72 * 64),
    };

    /// Run the search against rates keyed by server threads, with `budget`
    /// trials allowed in total.
    fn search_with(space: &Space, rates: &HashMap<u32, f64>, budget: usize) -> Report {
        let fake = |cand: Candidate, prune_below: Option<f64>| {
            let rate = rates
                .get(&cand.servers.unwrap_or(0))
                .copied()
                .unwrap_or(1.0);
            async move {
                match prune_below {
                    Some(floor) if rate < floor => Outcome::Pruned(rate),
                    _ => Outcome::Measured(rate),
                }
            }
        };
        let mut left = budget;
        let may = move || {
            // Two trials always run before the first ask.
            left = left.saturating_sub(1);
            left >= 2
        };
        futures_util::FutureExt::now_or_never(run(space, fake, may)).expect("the fake never waits")
    }

    fn picked(report: &Report) -> (Option<u32>, Option<u32>) {
        let (cand, _) = report.best.expect("a winner");
        (cand.servers, cand.batch)
    }

    fn tried(report: &Report) -> Vec<u32> {
        report
            .history
            .iter()
            .filter_map(|(cand, _)| cand.servers)
            .collect()
    }

    fn full() -> Overrides {
        Overrides::default()
    }

    /// The prior (4 server threads on wgpu) wins in two trials.
    #[test]
    fn the_prior_wins_in_two_trials() {
        let rates = HashMap::from([(1, 50.0), (2, 82.0), (4, 88.0), (8, 75.0), (16, 60.0)]);
        let space = Space::new(&full(), 460, EngineBackend::Wgpu, RX6800XT);
        let report = search_with(&space, &rates, 2);
        assert_eq!(picked(&report), (Some(4), Some(64)));
        assert_eq!(tried(&report), vec![4, 8]);
        // With more budget it also checks the other side, and still keeps 4.
        let report = search_with(&space, &rates, 9);
        assert_eq!(tried(&report), vec![4, 8, 2]);
        assert_eq!(picked(&report), (Some(4), Some(64)));
    }

    /// A better rung above the prior: climbs from 4 to 8, then stops at 16.
    #[test]
    fn the_search_climbs_while_it_improves() {
        let rates = HashMap::from([(2, 101.0), (4, 145.0), (8, 155.0), (16, 140.0)]);
        let space = Space::new(&full(), 460, EngineBackend::Wgpu, RX6800XT);
        let report = search_with(&space, &rates, 9);
        assert_eq!(tried(&report), vec![4, 8, 16]);
        assert_eq!(picked(&report), (Some(8), Some(64)));
        // Out of budget after two trials, the winner so far stands.
        let report = search_with(&space, &rates, 2);
        assert_eq!(tried(&report), vec![4, 8]);
        assert_eq!(picked(&report), (Some(8), Some(64)));
    }

    /// katago-rs on ROCm: starts at 8 server threads, which wins.
    #[test]
    fn rocm_picks_eight_server_threads() {
        let rates = HashMap::from([(2, 9.0), (4, 12.3), (8, 13.4), (16, 12.5)]);
        let space = Space::new(&full(), 460, EngineBackend::Rocm, RX6800XT);
        let report = search_with(&space, &rates, 2);
        assert_eq!(picked(&report), (Some(8), Some(64)));
        assert_eq!(tried(&report), vec![8, 16]);
    }

    /// A step within the tie band doesn't move the search off the prior.
    #[test]
    fn near_ties_keep_the_prior() {
        let rates = HashMap::from([(8, 100.0), (16, 102.0), (4, 90.0)]);
        let space = Space::new(&full(), 460, EngineBackend::Rocm, RX6800XT);
        let report = search_with(&space, &rates, 9);
        assert_eq!(picked(&report), (Some(8), Some(64)));
    }

    #[test]
    fn a_failing_prior_falls_back_down_the_ladder() {
        let space = Space::new(&full(), 460, EngineBackend::Wgpu, RX6800XT);
        let fake = |cand: Candidate, _prune: Option<f64>| async move {
            match cand.servers {
                Some(4 | 8 | 16) => Outcome::Failed("out of memory".to_owned()),
                Some(srv) => Outcome::Measured(f64::from(srv)),
                None => Outcome::Measured(0.0),
            }
        };
        let report =
            futures_util::FutureExt::now_or_never(run(&space, fake, || true)).expect("no waits");
        // 4, 8 and 16 fail; the walk restarts at the bottom and climbs to 2.
        assert_eq!(picked(&report), (Some(2), Some(128)));
        let none =
            |_cand: Candidate, _prune: Option<f64>| async { Outcome::Failed("no gpu".to_owned()) };
        let report =
            futures_util::FutureExt::now_or_never(run(&space, none, || true)).expect("no waits");
        assert!(report.best.is_none());
    }

    #[test]
    fn positions_in_flight_follow_the_gpu_and_the_queue() {
        // RX 6800 XT: 4608 lanes → 256; two games queue ~460 → still 256.
        assert_eq!(in_flight(Some(4608), 460), 256);
        // A small iGPU needs fewer; a big GPU wants 512 but only gets it with
        // a deep enough queue (three games in flight).
        assert_eq!(in_flight(Some(768), 460), 128);
        assert_eq!(in_flight(Some(16384), 460), 256);
        assert_eq!(in_flight(Some(16384), 690), 512);
        assert_eq!(in_flight(None, 460), 256);
    }

    #[test]
    fn the_ladder_respects_cores_batch_size_and_pins() {
        let full_space = Space::new(&full(), 460, EngineBackend::Wgpu, RX6800XT);
        assert_eq!(full_space.total(), 256);
        // Every rung: server threads are not capped by the CPU count.
        assert_eq!(full_space.ladder.len(), 5);
        assert_eq!(full_space.ladder[full_space.prior], (Some(4), Some(64)));
        assert_eq!(full_space.analysis_threads, Some(256));

        // A small GPU: 128 in flight, and no batch below 8.
        let small = Hardware {
            cpus: 4,
            gpu_lanes: Some(768),
        };
        let space = Space::new(&full(), 460, EngineBackend::Wgpu, small);
        assert_eq!(space.total(), 128);
        assert_eq!(
            space.ladder,
            vec![
                (Some(1), Some(128)),
                (Some(2), Some(64)),
                (Some(4), Some(64)),
                (Some(8), Some(64)),
                (Some(16), Some(64))
            ]
        );

        // One of the pair pinned: the other follows from the total (with the batch headroom).
        let srv = Space::new(
            &Overrides::parse("numNNServerThreadsPerModel=8"),
            460,
            EngineBackend::Wgpu,
            RX6800XT,
        );
        assert_eq!(srv.ladder, vec![(None, Some(64))]);
        assert!(srv.is_fixed());
        let report = search_with(&srv, &HashMap::new(), 9);
        assert!(report.history.is_empty(), "nothing is measured");
        assert_eq!(picked(&report), (None, Some(64)));

        let threads = Space::new(
            &Overrides::parse("numAnalysisThreads=64"),
            460,
            EngineBackend::Wgpu,
            RX6800XT,
        );
        assert_eq!(threads.analysis_threads, None);
        assert!(!threads.is_fixed());
    }
}
