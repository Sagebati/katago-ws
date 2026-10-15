//! `bench` role: a throughput benchmark of the analysis engine.
//!
//! Drives [`AnalysisEngine::analyze`] directly — no Postgres, no queue, no
//! cluster socket — over a directory of SGFs, keeping `concurrency` games in
//! flight (the same thing `[worker].concurrency` controls in production). It
//! measures what a worker actually delivers: games and positions per second,
//! per-game latency, and (on AMD, via sysfs) how busy the GPU was.
//!
//! Every run writes one JSON report, including a per-move fingerprint of the
//! evaluations (win rate, score lead, best move). Comparing two reports'
//! fingerprints is how a tuning change is checked for having bought speed with
//! analysis quality; see `scripts/bench_compare.py`.
//!
//! Configured from `[bench]` (or `KATAGO_WS_BENCH__*`); the engine itself from
//! `[engine]` exactly as in the other roles, so a KataGo config under test is
//! selected with `KATAGO_WS_ENGINE__CONFIG`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::config::EngineConfig;
use crate::engine::AnalysisEngine;
use crate::engine::annotate::GameAnalysis;

fn default_corpus() -> PathBuf {
    PathBuf::from("bench/sgf")
}
fn default_concurrency() -> usize {
    2
}
fn default_rounds() -> usize {
    1
}
fn default_out() -> PathBuf {
    PathBuf::from("bench/results/latest.json")
}
fn default_sample_ms() -> u64 {
    250
}

/// `[bench]` — what to run and where to write the report.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BenchConfig {
    /// Directory of `.sgf` files; every file is one game.
    pub corpus: PathBuf,
    /// Games kept in flight at once (mirrors `[worker].concurrency`).
    pub concurrency: usize,
    /// How many times the whole corpus is analyzed.
    pub rounds: usize,
    /// Report path (JSON).
    pub out: PathBuf,
    /// Free-form label copied into the report, to tell runs apart.
    pub label: String,
    /// GPU utilization sampling interval.
    pub sample_ms: u64,
    /// Skip the untimed warm-up game.
    pub no_warmup: bool,
}

impl Default for BenchConfig {
    fn default() -> Self {
        Self {
            corpus: default_corpus(),
            concurrency: default_concurrency(),
            rounds: default_rounds(),
            out: default_out(),
            label: String::new(),
            sample_ms: default_sample_ms(),
            no_warmup: false,
        }
    }
}

/// One analyzed game in the report.
#[derive(Serialize)]
struct GameRun {
    file: String,
    round: usize,
    /// Positions analyzed (moves + the empty board).
    positions: usize,
    secs: f64,
    /// Per-move evaluation fingerprint, for cross-run quality comparison.
    fingerprint: Vec<MoveFingerprint>,
}

/// The parts of one move's annotation that a tuning change could move.
#[derive(Serialize)]
struct MoveFingerprint {
    /// Win rate after the played move, for the player who made it.
    wr: f32,
    /// Score lead after the played move, for the player who made it.
    sl: f32,
    /// The engine's preferred move at this turn.
    best: String,
}

#[derive(Serialize)]
struct Summary {
    games: usize,
    positions: usize,
    failures: usize,
    wall_secs: f64,
    games_per_sec: f64,
    positions_per_sec: f64,
    /// `positions_per_sec × max_visits`: an upper bound on visits/s, comparable
    /// across runs at a fixed `max_visits`.
    visits_per_sec: f64,
    latency_p50_secs: f64,
    latency_p95_secs: f64,
    latency_max_secs: f64,
    /// Mean GPU busy % over the timed run (`None` where sysfs has no reading).
    gpu_busy_mean: Option<f64>,
}

#[derive(Serialize)]
struct Report<'cfg> {
    label: &'cfg str,
    concurrency: usize,
    rounds: usize,
    engine: &'cfg EngineConfig,
    /// The KataGo config file's contents, so a report is self-describing.
    katago_cfg: String,
    summary: Summary,
    games: Vec<GameRun>,
}

/// Run the benchmark and write the report. Returns an error string on setup
/// failure (bad corpus, unwritable output); per-game failures are counted.
pub async fn run(
    engine: Arc<AnalysisEngine>,
    engine_cfg: &EngineConfig,
    cfg: BenchConfig,
) -> Result<(), String> {
    let games = load_corpus(&cfg.corpus)?;
    if games.is_empty() {
        return Err(format!("no .sgf files in {}", cfg.corpus.display()));
    }
    let concurrency = cfg.concurrency.max(1);
    tracing::info!(
        games = games.len(),
        rounds = cfg.rounds,
        concurrency,
        label = %cfg.label,
        "bench: starting"
    );

    if !cfg.no_warmup
        && let Some((name, sgf)) = games.first()
    {
        let started = Instant::now();
        engine
            .analyze(sgf)
            .await
            .map_err(|err| format!("warm-up game {name} failed: {err}"))?;
        tracing::info!(
            secs = started.elapsed().as_secs_f64(),
            "bench: warm-up done"
        );
    }

    let (stop_tx, stop_rx) = watch::channel(false);
    let every = Duration::from_millis(cfg.sample_ms.max(10));
    let sampler = tokio::spawn(sample_gpu(every, stop_rx));

    let work: Vec<(usize, &str, &str)> = (0..cfg.rounds.max(1))
        .flat_map(|round| {
            games
                .iter()
                .map(move |(name, sgf)| (round, name.as_str(), sgf.as_str()))
        })
        .collect();

    let started = Instant::now();
    let results: Vec<Result<GameRun, String>> = futures_util::stream::iter(work)
        .map(|(round, name, sgf)| {
            let engine = Arc::clone(&engine);
            async move {
                let game_started = Instant::now();
                let analysis = engine
                    .analyze(sgf)
                    .await
                    .map_err(|err| format!("{name}: {err}"))?;
                Ok(game_run(name, round, game_started.elapsed(), &analysis))
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;
    let wall = started.elapsed().as_secs_f64();

    let _ = stop_tx.send(true);
    let gpu_busy_mean = sampler.await.ok().flatten();

    let mut runs = Vec::with_capacity(results.len());
    let mut failures = 0;
    for result in results {
        match result {
            Ok(run) => runs.push(run),
            Err(err) => {
                failures += 1;
                tracing::warn!("bench: game failed: {err}");
            }
        }
    }
    runs.sort_by(|lhs, rhs| (lhs.round, &lhs.file).cmp(&(rhs.round, &rhs.file)));

    let summary = summarize(&runs, failures, wall, engine_cfg.max_visits, gpu_busy_mean);
    tracing::info!(
        games_per_sec = summary.games_per_sec,
        positions_per_sec = summary.positions_per_sec,
        p50 = summary.latency_p50_secs,
        p95 = summary.latency_p95_secs,
        gpu_busy = ?summary.gpu_busy_mean,
        wall_secs = summary.wall_secs,
        failures,
        "bench: done"
    );

    let report = Report {
        label: &cfg.label,
        concurrency,
        rounds: cfg.rounds,
        engine: engine_cfg,
        katago_cfg: std::fs::read_to_string(&engine_cfg.config).unwrap_or_default(),
        summary,
        games: runs,
    };
    if let Some(dir) = cfg.out.parent() {
        std::fs::create_dir_all(dir).map_err(|err| format!("create {}: {err}", dir.display()))?;
    }
    let json =
        serde_json::to_vec_pretty(&report).map_err(|err| format!("serialize report: {err}"))?;
    std::fs::write(&cfg.out, json).map_err(|err| format!("write {}: {err}", cfg.out.display()))?;
    tracing::info!(out = %cfg.out.display(), "bench: report written");
    Ok(())
}

/// Every `*.sgf` under `dir` (not recursive), sorted by name.
fn load_corpus(dir: &Path) -> Result<Vec<(String, String)>, String> {
    let entries = std::fs::read_dir(dir).map_err(|err| format!("read {}: {err}", dir.display()))?;
    let mut games = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "sgf") {
            let sgf = std::fs::read_to_string(&path)
                .map_err(|err| format!("read {}: {err}", path.display()))?;
            let name = path
                .file_name()
                .map(|file| file.to_string_lossy().into_owned())
                .unwrap_or_default();
            games.push((name, sgf));
        }
    }
    games.sort();
    Ok(games)
}

fn game_run(name: &str, round: usize, elapsed: Duration, analysis: &GameAnalysis) -> GameRun {
    GameRun {
        file: name.to_owned(),
        round,
        positions: analysis.moves.len() + 1,
        secs: elapsed.as_secs_f64(),
        fingerprint: analysis
            .moves
            .iter()
            .map(|mv| MoveFingerprint {
                wr: mv.winrate,
                sl: mv.score_lead,
                best: mv
                    .top_moves
                    .first()
                    .map(|cand| cand.mv.clone())
                    .unwrap_or_default(),
            })
            .collect(),
    }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "benchmark statistics over at most thousands of games"
)]
fn summarize(
    runs: &[GameRun],
    failures: usize,
    wall: f64,
    max_visits: u32,
    gpu_busy_mean: Option<f64>,
) -> Summary {
    let positions: usize = runs.iter().map(|run| run.positions).sum();
    let mut latencies: Vec<f64> = runs.iter().map(|run| run.secs).collect();
    latencies.sort_by(f64::total_cmp);
    let percentile = |pct: f64| -> f64 {
        if latencies.is_empty() {
            return 0.0;
        }
        let idx = ((latencies.len() - 1) as f64 * pct).round() as usize;
        latencies.get(idx).copied().unwrap_or_default()
    };
    let wall = wall.max(f64::EPSILON);
    let positions_per_sec = positions as f64 / wall;
    Summary {
        games: runs.len(),
        positions,
        failures,
        wall_secs: wall,
        games_per_sec: runs.len() as f64 / wall,
        positions_per_sec,
        visits_per_sec: positions_per_sec * f64::from(max_visits),
        latency_p50_secs: percentile(0.5),
        latency_p95_secs: percentile(0.95),
        latency_max_secs: latencies.last().copied().unwrap_or_default(),
        gpu_busy_mean,
    }
}

/// Average `gpu_busy_percent` (amdgpu sysfs) until `stop` flips. `None` when no
/// card exposes it (non-AMD GPU, CPU build, container without `/sys`).
async fn sample_gpu(every: Duration, mut stop: watch::Receiver<bool>) -> Option<f64> {
    let path = find_gpu_busy_file()?;
    let mut total = 0.0;
    let mut samples = 0_u32;
    let mut tick = tokio::time::interval(every);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                // A sysfs read is a kernel counter, not disk I/O: no need for async fs.
                if let Some(busy) = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|raw| raw.trim().parse::<f64>().ok())
                {
                    total += busy;
                    samples += 1;
                }
            }
            _ = stop.changed() => break,
        }
    }
    (samples > 0).then(|| total / f64::from(samples))
}

fn find_gpu_busy_file() -> Option<PathBuf> {
    let mut cards: Vec<PathBuf> = std::fs::read_dir("/sys/class/drm")
        .ok()?
        .flatten()
        .map(|entry| entry.path().join("device/gpu_busy_percent"))
        .filter(|path| path.exists())
        .collect();
    cards.sort();
    cards.into_iter().next()
}
