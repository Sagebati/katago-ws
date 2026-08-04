//! Auto-tune KataGo's thread/batch settings for the host via
//! `katago benchmark -tune`, caching the result so later launches skip it.
//!
//! Runs once, inside `KataGoEnginePlugin::build`, strictly before any
//! concurrency exists — no actor/lock is needed for that reason alone (unlike
//! the KataGo client itself, which genuinely has concurrent callers).
//!
//! A failed/unparsable/timed-out tune is never fatal: it's logged and the
//! caller falls back to KataGo's stock config. A wrong guess derived from a
//! misread benchmark would be worse than the stock (known-tested) settings.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::config::EngineConfig;
use crate::engine::preflight::EngineLaunch;

/// Cache format version — bump to invalidate every existing cache file.
const CACHE_SCHEMA: u32 = 1;

/// Derived KataGo analysis-engine thread/batch settings.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Tuning {
    pub num_analysis_threads: u32,
    pub num_search_threads_per_analysis_thread: u32,
    pub nn_max_batch_size: u32,
}

impl Tuning {
    /// As `-override-config key=value` pairs for the `katago analysis` argv.
    pub fn overrides(&self) -> Vec<(String, String)> {
        vec![
            (
                "numAnalysisThreads".to_owned(),
                self.num_analysis_threads.to_string(),
            ),
            (
                "numSearchThreadsPerAnalysisThread".to_owned(),
                self.num_search_threads_per_analysis_thread.to_string(),
            ),
            (
                "nnMaxBatchSize".to_owned(),
                self.nn_max_batch_size.to_string(),
            ),
        ]
    }
}

/// A coarse signal of "is this still the same machine/install" — enough to
/// catch a moved home directory, a KataGo upgrade, or a different network,
/// without real hardware enumeration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Fingerprint {
    cpu_cores: usize,
    gpu: String,
    binary_len: u64,
    binary_mtime_secs: u64,
    model_len: u64,
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    schema: u32,
    tuned_at: String,
    fingerprint: Fingerprint,
    tuning: Tuning,
    benchmark_threads: u32,
}

#[derive(Debug, thiserror::Error)]
enum TuneError {
    #[error("benchmark did not complete within the timeout")]
    TimedOut,
    #[error("failed to launch 'katago benchmark': {0}")]
    Spawn(std::io::Error),
    #[error("katago benchmark exited with {0}")]
    Failed(std::process::ExitStatus),
}

/// Best throughput line parsed out of `katago benchmark`'s stdout.
#[derive(Debug, Clone, Copy, PartialEq)]
struct BenchmarkBest {
    num_search_threads: u32,
    visits_per_sec: f64,
}

/// Run the auto-tune benchmark (or reuse a cached result), returning derived
/// settings — or `None` if tuning failed for any reason, in which case the
/// caller should proceed with KataGo's stock config.
pub async fn resolve(launch: &EngineLaunch, cfg: &EngineConfig) -> Option<Tuning> {
    let current = fingerprint(launch);
    if let Some(entry) = load_cache()
        && entry.schema == CACHE_SCHEMA
        && entry.fingerprint == current
    {
        tracing::info!(tuning = ?entry.tuning, "using cached KataGo auto-tune result");
        return Some(entry.tuning);
    }

    let timeout = Duration::from_secs(cfg.tune_timeout_secs.max(1));
    let stdout = match run_benchmark(launch, timeout).await {
        Ok(stdout) => stdout,
        Err(err) => {
            tracing::warn!(error = %err, "KataGo auto-tune benchmark failed; running with stock settings");
            return None;
        }
    };

    let Some(best) = parse_benchmark_output(&stdout) else {
        tracing::warn!("could not parse KataGo benchmark output; running with stock settings");
        return None;
    };

    let cores = std::thread::available_parallelism()
        .map(NonZeroUsize::get)
        .unwrap_or(1);
    let tuning = derive_tuning(best.num_search_threads, cores);
    tracing::info!(
        benchmark_threads = best.num_search_threads,
        ?tuning,
        "derived KataGo analysis tuning"
    );

    save_cache(&CacheEntry {
        schema: CACHE_SCHEMA,
        tuned_at: chrono::Utc::now().to_rfc3339(),
        fingerprint: current,
        tuning,
        benchmark_threads: best.num_search_threads,
    });

    Some(tuning)
}

async fn run_benchmark(launch: &EngineLaunch, timeout: Duration) -> Result<String, TuneError> {
    tracing::info!(
        timeout_secs = timeout.as_secs(),
        "running KataGo auto-tune benchmark (first run only; can take a while, especially on a fresh GPU host)"
    );

    let run = Command::new(&launch.binary)
        .arg("benchmark")
        .arg("-model")
        .arg(&launch.model)
        .arg("-config")
        .arg(&launch.config)
        .arg("-tune")
        .kill_on_drop(true)
        .output();

    let output = tokio::time::timeout(timeout, run)
        .await
        .map_err(|_elapsed| TuneError::TimedOut)?
        .map_err(TuneError::Spawn)?;

    if !output.status.success() {
        for line in String::from_utf8_lossy(&output.stderr).lines() {
            tracing::warn!(target: "katago-benchmark", "{line}");
        }
        return Err(TuneError::Failed(output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Scan every line of `katago benchmark`'s stdout for a
/// `numSearchThreads = N ... visits/s = F` pair and keep the highest-throughput
/// one. Deliberately doesn't trust ordering or a "summary" banner — those are
/// prose, and version drift is expected.
fn parse_benchmark_output(stdout: &str) -> Option<BenchmarkBest> {
    let mut best: Option<BenchmarkBest> = None;
    for raw_line in stdout.lines() {
        let line = raw_line.trim_start_matches('\r').trim();
        let Some(threads) = after(line, "numSearchThreads = ")
            .and_then(|rest| rest.split(':').next())
            .and_then(|token| token.trim().parse::<u32>().ok())
        else {
            continue;
        };
        let Some(visits_per_sec) = after(line, "visits/s = ")
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|token| token.parse::<f64>().ok())
        else {
            continue;
        };
        if best.is_none_or(|found| visits_per_sec > found.visits_per_sec) {
            best = Some(BenchmarkBest {
                num_search_threads: threads,
                visits_per_sec,
            });
        }
    }
    best
}

fn after<'line>(line: &'line str, marker: &str) -> Option<&'line str> {
    line.find(marker).map(|idx| &line[idx + marker.len()..])
}

/// Translate the benchmark's single `numSearchThreads` figure into the
/// analysis engine's two-knob split. `analysis_example.cfg`'s own guidance:
/// the analysis engine wants breadth across positions (high
/// `numAnalysisThreads`, low `numSearchThreadsPerAnalysisThread`, 1-4), not
/// deep per-position search — the opposite of what GTP benchmarking optimizes.
/// `num_analysis_threads` is also capped at the host's core count, since
/// running more parallel-searched positions than there are cores to run them
/// on doesn't buy anything.
fn derive_tuning(total_threads: u32, cores: usize) -> Tuning {
    let total = total_threads.max(1);
    let per = if total <= 4 { 1 } else { 2 };
    let cores = u32::try_from(cores).unwrap_or(u32::MAX).max(1);
    let num_analysis_threads = (total / per).clamp(1, cores);
    let nn_max_batch_size = total.clamp(8, 256);
    Tuning {
        num_analysis_threads,
        num_search_threads_per_analysis_thread: per,
        nn_max_batch_size,
    }
}

fn fingerprint(launch: &EngineLaunch) -> Fingerprint {
    let cpu_cores = std::thread::available_parallelism()
        .map(NonZeroUsize::get)
        .unwrap_or(1);
    let gpu = if Path::new("/dev/nvidia0").exists() {
        "nvidia"
    } else if Path::new("/dev/dri").exists() {
        "dri"
    } else {
        "none"
    }
    .to_owned();
    let (binary_len, binary_mtime_secs) = std::fs::metadata(&launch.binary)
        .map(|meta| (meta.len(), mtime_secs(&meta)))
        .unwrap_or((0, 0));
    let model_len = std::fs::metadata(&launch.model)
        .map(|meta| meta.len())
        .unwrap_or(0);
    Fingerprint {
        cpu_cores,
        gpu,
        binary_len,
        binary_mtime_secs,
        model_len,
    }
}

fn mtime_secs(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn cache_path() -> Option<PathBuf> {
    crate::paths::state_dir().map(|dir| dir.join("tuning.json"))
}

fn load_cache() -> Option<CacheEntry> {
    let path = cache_path()?;
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save_cache(entry: &CacheEntry) {
    let Some(path) = cache_path() else {
        tracing::warn!("no state directory available; KataGo auto-tune result won't be cached");
        return;
    };
    if let Some(parent) = path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(error = %err, "could not create KataGo tuning cache directory");
        return;
    }
    let bytes = match serde_json::to_vec_pretty(entry) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not serialize KataGo tuning cache");
            return;
        }
    };
    if let Err(err) = std::fs::write(&path, bytes) {
        tracing::warn!(error = %err, path = %path.display(), "could not write KataGo tuning cache");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_best_throughput_line_regardless_of_order() {
        let stdout = "\r\
            numSearchThreads =  4:  4 / 4 positions, visits/s = 120.50 nnEvals/s = 100.00 nnBatches/s = 10.00 avgBatchSize = 10.00 (1.0 secs) (EloDiff baseline)\n\
            \rnumSearchThreads = 16: 16 / 16 positions, visits/s = 480.25 nnEvals/s = 400.00 nnBatches/s = 25.00 avgBatchSize = 16.00 (1.0 secs) (EloDiff +50)\n\
            \rnumSearchThreads = 32: 32 / 32 positions, visits/s = 300.00 nnEvals/s = 250.00 nnBatches/s = 15.00 avgBatchSize = 20.00 (1.0 secs) (EloDiff +10)\n";
        let best = parse_benchmark_output(stdout).expect("a best result");
        assert_eq!(best.num_search_threads, 16);
        assert!((best.visits_per_sec - 480.25).abs() < 1e-6);
    }

    #[test]
    fn ignores_unrelated_and_truncated_lines() {
        let stdout = "Testing using 100 visits.\n\
            Your GTP config is currently set to use numSearchThreads = 6\n\
            numSearchThreads =  8: garbage line with no visits/s here\n";
        assert_eq!(parse_benchmark_output(stdout), None);
    }

    #[test]
    fn garbage_input_yields_no_result() {
        assert_eq!(
            parse_benchmark_output("not a benchmark output at all"),
            None
        );
        assert_eq!(parse_benchmark_output(""), None);
    }

    #[test]
    fn derive_tuning_invariants_hold_across_a_range_of_totals() {
        for total in [1_u32, 2, 4, 8, 16, 32, 64] {
            for cores in [1_usize, 4, 8, 16, 64] {
                let tuning = derive_tuning(total, cores);
                assert!((1..=4).contains(&tuning.num_search_threads_per_analysis_thread));
                assert!(tuning.num_analysis_threads >= 1);
                assert!(
                    u64::from(tuning.num_analysis_threads)
                        * u64::from(tuning.num_search_threads_per_analysis_thread)
                        <= u64::from(total.max(1)) * 2
                );
            }
        }
    }

    #[test]
    fn overrides_format_as_override_config_pairs() {
        let tuning = Tuning {
            num_analysis_threads: 8,
            num_search_threads_per_analysis_thread: 2,
            nn_max_batch_size: 16,
        };
        let overrides = tuning.overrides();
        assert_eq!(
            overrides,
            vec![
                ("numAnalysisThreads".to_owned(), "8".to_owned()),
                (
                    "numSearchThreadsPerAnalysisThread".to_owned(),
                    "2".to_owned()
                ),
                ("nnMaxBatchSize".to_owned(), "16".to_owned()),
            ]
        );
    }

    #[test]
    fn cache_entry_round_trips_through_json() {
        let entry = CacheEntry {
            schema: CACHE_SCHEMA,
            tuned_at: "2026-08-04T00:00:00Z".to_owned(),
            fingerprint: Fingerprint {
                cpu_cores: 8,
                gpu: "none".to_owned(),
                binary_len: 123,
                binary_mtime_secs: 456,
                model_len: 789,
            },
            tuning: Tuning {
                num_analysis_threads: 4,
                num_search_threads_per_analysis_thread: 2,
                nn_max_batch_size: 16,
            },
            benchmark_threads: 8,
        };
        let json = serde_json::to_vec(&entry).expect("serializes");
        let back: CacheEntry = serde_json::from_slice(&json).expect("deserializes");
        assert_eq!(back.schema, entry.schema);
        assert_eq!(back.fingerprint, entry.fingerprint);
        assert_eq!(back.tuning, entry.tuning);
    }

    #[test]
    fn a_malformed_cache_file_does_not_panic() {
        let result: Result<CacheEntry, _> = serde_json::from_str("not json");
        assert!(result.is_err());
    }
}
