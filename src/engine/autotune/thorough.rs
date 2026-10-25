//! The thorough search (`[engine] autotune_mode = "thorough"`, the default):
//! every server×batch candidate for the backend, then `numAnalysisThreads`
//! around the winner, re-measuring near-ties. A few minutes on a GPU; the fast
//! search ([`super::fast`]) gets close in a few trials.

use std::future::Future;

use crate::config::EngineBackend;

use super::overrides::{ANALYSIS_THREADS, BATCH_SIZE, Overrides, SERVER_THREADS};
use super::search::{Candidate, Outcome, Report};

/// `numAnalysisThreads` candidates, the expected winner first. Values above
/// the queue depth (≈ 230 positions per game in flight) are skipped: threads
/// with no position to search can't help, and their lockstep completion waves
/// make a short measurement over-read them.
const ANALYSIS_CANDIDATES: [u32; 3] = [256, 128, 512];

/// katago-rs on ROCm: `numNNServerThreadsPerModel` × `nnMaxBatchSize`
/// candidates in prior order. katago-rs's NN server threads are tasks on its one
/// search thread, so more of them means more batches in flight while the GPU
/// works: on the RX 6800 XT (b18c384nbt, f16, 256 analysis threads) 8×64 did
/// 13.4 positions/s, 4×96 13.0, 4×64 12.3.
const ROCM_SERVER_BATCH: [(u32, u32); 4] = [(8, 64), (4, 96), (4, 64), (8, 32)];

/// katago-rs on wgpu (Vulkan): not measured yet, so a spread around 4×64.
const WGPU_SERVER_BATCH: [(u32, u32); 4] = [(4, 64), (8, 32), (6, 48), (2, 128)];

/// A trial running below this fraction of the best so far is stopped half-way.
const PRUNE_RATIO: f64 = 0.92;

/// Candidates within this fraction of a stage's leader are re-measured (and
/// their measurements averaged) before the stage picks its winner, so trial
/// noise doesn't decide a near-tie. A single 640-position window varies by
/// up to ±5% (stock OpenCL KataGo is the noisiest), hence 8%.
const CLOSE: f64 = 0.08;

/// After re-measuring, a candidate must beat every earlier (higher-prior) one
/// by more than this to win: within it, the prior order decides. Two averaged
/// 640-position windows still differ by a few percent run to run.
const TIE: f64 = 0.03;

/// The search space left once pinned keys are taken out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Space {
    analysis: Vec<Option<u32>>,
    server_batch: Vec<(Option<u32>, Option<u32>)>,
}

impl Space {
    /// The candidates for everything `pinned` leaves open, with `depth`
    /// positions kept queued, on `backend`.
    #[must_use]
    pub fn new(pinned: &Overrides, depth: usize, backend: EngineBackend) -> Self {
        let pairs: &[(u32, u32)] = match backend {
            EngineBackend::Rocm => &ROCM_SERVER_BATCH,
            EngineBackend::Wgpu | EngineBackend::Flex => &WGPU_SERVER_BATCH,
        };
        let analysis = if pinned.get(ANALYSIS_THREADS).is_some() {
            vec![None]
        } else {
            ANALYSIS_CANDIDATES
                .iter()
                .enumerate()
                .filter(|&(idx, &threads)| {
                    idx == 0 || usize::try_from(threads).is_ok_and(|threads| threads <= depth)
                })
                .map(|(_, &threads)| Some(threads))
                .collect()
        };
        let pin_srv = pinned.get(SERVER_THREADS).is_some();
        let pin_batch = pinned.get(BATCH_SIZE).is_some();
        let server_batch = match (pin_srv, pin_batch) {
            (true, true) => vec![(None, None)],
            // One of the pair pinned: vary the other over its candidate values.
            (true, false) => dedup(pairs.iter().map(|&(_, batch)| (None, Some(batch)))),
            (false, true) => dedup(pairs.iter().map(|&(srv, _)| (Some(srv), None))),
            (false, false) => pairs
                .iter()
                .map(|&(srv, batch)| (Some(srv), Some(batch)))
                .collect(),
        };
        Self {
            analysis,
            server_batch,
        }
    }

    /// Nothing to measure: every throughput key is pinned.
    #[must_use]
    pub fn is_fully_pinned(&self) -> bool {
        self.analysis.len() <= 1 && self.server_batch.len() <= 1
    }

    fn first(&self) -> Candidate {
        let (servers, batch) = self.server_batch.first().copied().unwrap_or_default();
        Candidate {
            analysis_threads: self.analysis.first().copied().flatten(),
            servers,
            batch,
        }
    }
}

fn dedup<T: PartialEq>(items: impl IntoIterator<Item = T>) -> Vec<T> {
    let mut out = Vec::new();
    for item in items {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

/// Search `space`: first server threads × batch size (at the first
/// `numAnalysisThreads` candidate), then `numAnalysisThreads` around that
/// winner. `measure(candidate, prune_below)` runs one trial; it may stop early
/// once the rate is clearly below `prune_below`.
pub async fn run<F, Fut>(space: &Space, mut measure: F) -> Report
where
    F: FnMut(Candidate, Option<f64>) -> Fut,
    Fut: Future<Output = Outcome>,
{
    let mut report = Report::default();
    let base = space.first();
    if space.is_fully_pinned() {
        // Nothing to compare: the device choice (if any) is all there is.
        report.best = Some((base, 0.0));
        return report;
    }

    let stage_a: Vec<Candidate> = space
        .server_batch
        .iter()
        .map(|&(servers, batch)| Candidate {
            servers,
            batch,
            ..base
        })
        .collect();
    let leader = stage(&stage_a, None, &mut measure, &mut report).await;

    let Some((leader, leader_samples)) = leader else {
        return report;
    };
    let stage_b: Vec<Candidate> = space
        .analysis
        .iter()
        .skip(1)
        .map(|&analysis_threads| Candidate {
            analysis_threads,
            ..leader
        })
        .collect();
    let winner = stage(
        &stage_b,
        Some((leader, leader_samples)),
        &mut measure,
        &mut report,
    )
    .await;
    report.best = winner.map(|(cand, samples)| (cand, mean(&samples)));
    report
}

/// Measure `cands` (pruning against the best so far), then re-measure every
/// contender within [`CLOSE`] of the leader and return the best by mean.
/// `incumbent` is a previous stage's winner with its samples.
async fn stage<F, Fut>(
    cands: &[Candidate],
    incumbent: Option<(Candidate, Vec<f64>)>,
    measure: &mut F,
    report: &mut Report,
) -> Option<(Candidate, Vec<f64>)>
where
    F: FnMut(Candidate, Option<f64>) -> Fut,
    Fut: Future<Output = Outcome>,
{
    let mut scored: Vec<(Candidate, Vec<f64>)> = incumbent.into_iter().collect();
    for &cand in cands {
        let best = scored
            .iter()
            .map(|(_, samples)| mean(samples))
            .fold(0.0, f64::max);
        let prune_below = (best > 0.0).then_some(best * PRUNE_RATIO);
        let outcome = measure(cand, prune_below).await;
        if let Outcome::Measured(rate) = outcome {
            scored.push((cand, vec![rate]));
        }
        report.history.push((cand, outcome));
    }

    let leader = scored
        .iter()
        .map(|(_, samples)| mean(samples))
        .fold(0.0, f64::max);
    let contenders: Vec<usize> = scored
        .iter()
        .enumerate()
        .filter(|(_, (_, samples))| mean(samples) >= leader * (1.0 - CLOSE))
        .map(|(idx, _)| idx)
        .collect();
    if contenders.len() > 1 {
        for idx in contenders {
            let Some((cand, _)) = scored.get(idx) else {
                continue;
            };
            let cand = *cand;
            let outcome = measure(cand, None).await;
            if let (Outcome::Measured(rate), Some((_, samples))) = (&outcome, scored.get_mut(idx)) {
                samples.push(*rate);
            }
            report.history.push((cand, outcome));
        }
    }

    // The earliest (highest-prior) candidate within TIE of the best wins.
    let top = scored
        .iter()
        .map(|(_, samples)| mean(samples))
        .fold(0.0, f64::max);
    scored
        .into_iter()
        .find(|(_, samples)| top > 0.0 && mean(samples) >= top * (1.0 - TIE))
}

#[allow(
    clippy::cast_precision_loss,
    reason = "a handful of samples per candidate"
)]
fn mean(samples: &[f64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.iter().sum::<f64>() / samples.len() as f64
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// Run the search against a table of rates keyed by (threads, servers, batch).
    fn search_with(space: &Space, rates: &HashMap<(u32, u32, u32), f64>, default: f64) -> Report {
        let fake = |cand: Candidate, prune_below: Option<f64>| {
            let key = (
                cand.analysis_threads.unwrap_or(0),
                cand.servers.unwrap_or(0),
                cand.batch.unwrap_or(0),
            );
            let rate = rates.get(&key).copied().unwrap_or(default);
            async move {
                match prune_below {
                    Some(floor) if rate < floor => Outcome::Pruned(rate),
                    _ => Outcome::Measured(rate),
                }
            }
        };
        futures_util::FutureExt::now_or_never(run(space, fake)).expect("the fake never waits")
    }

    fn best_of(report: &Report) -> (Option<u32>, Option<u32>, Option<u32>) {
        let (cand, _) = report.best.expect("a winner");
        (cand.analysis_threads, cand.servers, cand.batch)
    }

    /// The prior wins: 4×64, and a clearly slower candidate is cut short.
    #[test]
    fn the_prior_wins_when_it_is_fastest() {
        let rates = HashMap::from([
            ((256, 4, 64), 75.0),
            ((256, 8, 32), 65.7),
            ((256, 6, 48), 69.0),
            ((256, 2, 128), 55.0),
            ((128, 4, 64), 68.0),
            ((512, 4, 64), 74.0),
        ]);
        let report = search_with(
            &Space::new(&Overrides::default(), 1000, EngineBackend::Wgpu),
            &rates,
            1.0,
        );
        assert_eq!(best_of(&report), (Some(256), Some(4), Some(64)));
        // 2×128 was clearly slower than 4×64 and was cut short.
        assert!(report.history.iter().any(|(cand, outcome)| {
            cand.servers == Some(2) && matches!(outcome, Outcome::Pruned(_))
        }));
    }

    /// A clearly faster later candidate wins over the prior.
    #[test]
    fn a_faster_candidate_beats_the_prior() {
        let rates = HashMap::from([
            ((256, 4, 64), 120.0),
            ((256, 8, 32), 165.0),
            ((256, 6, 48), 150.0),
            ((256, 2, 128), 70.0),
            ((128, 8, 32), 130.0),
            ((512, 8, 32), 160.0),
        ]);
        let report = search_with(
            &Space::new(&Overrides::default(), 1000, EngineBackend::Wgpu),
            &rates,
            1.0,
        );
        assert_eq!(best_of(&report), (Some(256), Some(8), Some(32)));
    }

    /// katago-rs on ROCm (RX 6800 XT, b18, f16): 8×64 first, and it wins;
    /// 4×96 is within the tie band, so the prior stands.
    #[test]
    fn rocm_rates_pick_eight_by_sixty_four() {
        let rates = HashMap::from([
            ((256, 8, 64), 13.4),
            ((256, 4, 96), 13.0),
            ((256, 4, 64), 12.3),
            ((256, 8, 32), 12.0),
            ((128, 8, 64), 11.0),
        ]);
        let space = Space::new(&Overrides::default(), 460, EngineBackend::Rocm);
        assert_eq!(space.server_batch.first(), Some(&(Some(8), Some(64))));
        let report = search_with(&space, &rates, 1.0);
        assert_eq!(best_of(&report), (Some(256), Some(8), Some(64)));
    }

    /// A near-tie is re-measured rather than decided by one noisy trial.
    #[test]
    fn near_ties_are_measured_twice() {
        let rates = HashMap::from([((256, 4, 64), 100.0), ((256, 8, 32), 98.0)]);
        let report = search_with(
            &Space::new(&Overrides::default(), 1000, EngineBackend::Wgpu),
            &rates,
            10.0,
        );
        let count = |srv| {
            report
                .history
                .iter()
                .filter(|(cand, _)| cand.servers == Some(srv) && cand.analysis_threads == Some(256))
                .count()
        };
        assert_eq!(count(4), 2);
        assert_eq!(count(8), 2);
        assert_eq!(best_of(&report), (Some(256), Some(4), Some(64)));
    }

    /// Within the tie band the prior order decides, not trial noise.
    #[test]
    fn a_near_tie_goes_to_the_higher_prior_candidate() {
        let rates = HashMap::from([((256, 8, 32), 165.0), ((256, 6, 48), 166.0)]);
        let report = search_with(
            &Space::new(&Overrides::default(), 1000, EngineBackend::Wgpu),
            &rates,
            10.0,
        );
        assert_eq!(best_of(&report), (Some(256), Some(8), Some(32)));
        // A clear win still counts.
        let rates = HashMap::from([((256, 8, 32), 160.0), ((256, 6, 48), 166.0)]);
        let report = search_with(
            &Space::new(&Overrides::default(), 1000, EngineBackend::Wgpu),
            &rates,
            10.0,
        );
        assert_eq!(best_of(&report), (Some(256), Some(6), Some(48)));
    }

    #[test]
    fn failed_trials_are_skipped() {
        let space = Space::new(&Overrides::default(), 1000, EngineBackend::Wgpu);
        let fake = |cand: Candidate, _prune: Option<f64>| async move {
            if cand.servers == Some(4) {
                Outcome::Failed("out of memory".to_owned())
            } else {
                Outcome::Measured(f64::from(cand.servers.unwrap_or(0)))
            }
        };
        let report = futures_util::FutureExt::now_or_never(run(&space, fake)).expect("no waits");
        assert_eq!(best_of(&report).1, Some(8));
        let none =
            |_cand: Candidate, _prune: Option<f64>| async { Outcome::Failed("no gpu".to_owned()) };
        let report = futures_util::FutureExt::now_or_never(run(&space, none)).expect("no waits");
        assert!(report.best.is_none());
    }

    #[test]
    fn pinned_keys_shrink_the_space() {
        let full = Space::new(&Overrides::default(), 1000, EngineBackend::Wgpu);
        assert_eq!(full.analysis.len(), 3);
        // Two games in flight queue ~460 positions: 512 threads can't all work.
        let shallow = Space::new(&Overrides::default(), 460, EngineBackend::Wgpu);
        assert_eq!(shallow.analysis, vec![Some(256), Some(128)]);
        assert_eq!(full.server_batch.len(), 4);
        assert!(!full.is_fully_pinned());

        let srv = Space::new(
            &Overrides::parse("numNNServerThreadsPerModel=8"),
            1000,
            EngineBackend::Wgpu,
        );
        assert_eq!(
            srv.server_batch,
            vec![
                (None, Some(64)),
                (None, Some(32)),
                (None, Some(48)),
                (None, Some(128))
            ]
        );

        let threads = Space::new(
            &Overrides::parse("numAnalysisThreads=64"),
            1000,
            EngineBackend::Wgpu,
        );
        assert_eq!(threads.analysis, vec![None]);
        let rates = HashMap::from([((0, 8, 32), 9.0)]);
        let report = search_with(&threads, &rates, 1.0);
        assert_eq!(best_of(&report), (None, Some(8), Some(32)));
        // Only the server×batch stage ran.
        assert!(
            report
                .history
                .iter()
                .all(|(cand, _)| cand.analysis_threads.is_none())
        );

        let all = Space::new(
            &Overrides::parse(
                "numAnalysisThreads=64,numNNServerThreadsPerModel=2,nnMaxBatchSize=16",
            ),
            1000,
            EngineBackend::Wgpu,
        );
        assert!(all.is_fully_pinned());
        let report = search_with(&all, &HashMap::new(), 1.0);
        assert!(report.history.is_empty(), "nothing is measured");
        assert_eq!(best_of(&report), (None, None, None));
    }
}
