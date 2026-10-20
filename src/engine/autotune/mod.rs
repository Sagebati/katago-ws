//! Startup throughput auto-tune for KataGo's analysis engine.
//!
//! KataGo's own `benchmark`/`genconfig` only tune GTP's `numSearchThreads`; for
//! the analysis engine, throughput at a fixed `max_visits` is decided by
//! settings they don't touch, and the best values depend on the GPU, the
//! driver *and* the KataGo build:
//!
//! - `numNNServerThreadsPerModel` × `nnMaxBatchSize` — the biggest lever. Stock
//!   OpenCL KataGo shares one in-order command queue across server threads, so
//!   4×64 wins and 8×32 is slower; a build with a queue per server thread
//!   (`patches/`) is fastest at 8×32. So these are measured, not hardcoded.
//! - `numAnalysisThreads` — positions searched in parallel (256 is the usual
//!   sweet spot; 128 is tried, and 512 when the queue is deep enough to feed
//!   it, i.e. with three or more games in flight).
//! - `numSearchThreadsPerAnalysisThread` — always 1: best throughput, and equal
//!   or better quality, for whole-game analysis.
//! - the OpenCL device — when one GPU is exposed by both a vendor platform
//!   (AMD ROCm "AMD Accelerated Parallel Processing", NVIDIA CUDA, Intel) and a
//!   Mesa one (rusticl, Clover), KataGo may pick Mesa's, which is ~1.6× slower
//!   on AMD. The device list is read from KataGo's own startup log and the
//!   vendor device preferred; this is not trialled (each device would pay its
//!   own first-use kernel tuning).
//!
//! Each candidate is a fresh KataGo process (these are startup settings) fed
//! an embedded 17-game corpus in 16-turn queries that keep the queue as deep as
//! `[worker].concurrency` games in flight would (≈ 230 positions each) —
//! without a finishing game's drain, which makes whole-game measurements
//! noisy. Throughput is read off a turn-level counter after one wave of
//! warm-up, so neither process startup nor KataGo's first-use OpenCL kernel
//! tuning is charged to a candidate. Clearly losing candidates are stopped
//! half-way, near-ties are re-measured, and within 2% the higher-prior
//! candidate wins. The winning keys are cached (see [`cache`]) so later starts
//! skip all of this.
//!
//! Precedence: settings given **explicitly** are pinned — `[engine]`'s
//! `analysis_threads` / `search_threads_per_analysis` / `nn_server_threads` /
//! `nn_max_batch_size` when present in the config file or env (their built-in
//! defaults don't count), and any key in `[engine] override_config`. The tuner
//! never changes them and only searches (and fills in) the rest, writing its
//! choices back into those `[engine]` fields (the device keys go to
//! `override_config`). Any failure falls back to the configuration as given;
//! the auto-tune never fails startup.

pub mod cache;
pub mod device;
pub mod fast;
pub mod hw;
pub mod overrides;
pub mod search;
pub mod thorough;

use std::cell::Cell;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use tokio::sync::{mpsc, watch};

use crate::config::{AutotuneMode, EngineConfig};
use crate::engine::AnalysisEngine;
use crate::engine::sgf::{self, ParsedGame};

use self::cache::{CacheEntry, CacheKey, TrialRecord};
use self::device::OpenClDevice;
use self::hw::Hardware;
use self::overrides::Overrides;
use self::search::{Candidate, Outcome};

/// The embedded trial corpus: 17 public 19×19 games (the ear-reddening game
/// and 16 OGS games, ~3,900 positions) — enough that a trial never wraps
/// around into KataGo's NN cache.
const CORPUS: [&str; 17] = [
    include_str!("corpus/ear_reddening.sgf"),
    include_str!("corpus/ogs-60000000.sgf"),
    include_str!("corpus/ogs-60015838.sgf"),
    include_str!("corpus/ogs-60102947.sgf"),
    include_str!("corpus/ogs-60126704.sgf"),
    include_str!("corpus/ogs-60134623.sgf"),
    include_str!("corpus/ogs-60213813.sgf"),
    include_str!("corpus/ogs-60261327.sgf"),
    include_str!("corpus/ogs-60356355.sgf"),
    include_str!("corpus/ogs-60372193.sgf"),
    include_str!("corpus/ogs-60388031.sgf"),
    include_str!("corpus/ogs-60435545.sgf"),
    include_str!("corpus/ogs-60530573.sgf"),
    include_str!("corpus/ogs-60538492.sgf"),
    include_str!("corpus/ogs-60554330.sgf"),
    include_str!("corpus/ogs-60562249.sgf"),
    include_str!("corpus/ogs-60586006.sgf"),
];

/// Turns per query. Feeding games in small chunks keeps KataGo's queue at a
/// constant depth, without the dips a whole game's tail leaves — which make
/// short measurements noisy.
const CHUNK_TURNS: usize = 16;
/// Positions kept queued per game a worker has in flight (≈ a game's length),
/// so trials see the queue depth production does.
const POSITIONS_PER_GAME: usize = 230;

/// Positions in a trial's measured window: ~2.5 completion waves at 256
/// analysis threads (all of a wave's positions finish at about the same
/// time). Measured on an RX 6800 XT, b10: ±2–5% run to run.
const WINDOW_POSITIONS: u64 = 640;
/// A trial's measured window never runs longer than this (slow GPUs measure
/// fewer positions instead of blowing the budget).
const MAX_WINDOW: Duration = Duration::from_secs(12);
/// Fast mode: after the first two trials, another one starts only if it
/// should finish within this much time since tuning began.
const FAST_BUDGET: Duration = Duration::from_secs(25);
/// A first KataGo start slower than this is paying for one-off OpenCL kernel
/// tuning; the excess isn't charged to the fast mode's budget.
const SLOW_START: Duration = Duration::from_secs(15);
/// How long the device probe waits for KataGo to list its OpenCL devices.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
/// `katago version` should answer at once (an `AppImage` extracts first).
const VERSION_TIMEOUT: Duration = Duration::from_secs(60);

/// `[engine]` fields the tuner sets, with the KataGo key each one becomes.
pub const TUNING_FIELDS: [(&str, &str); 4] = [
    ("analysis_threads", overrides::ANALYSIS_THREADS),
    ("search_threads_per_analysis", overrides::SEARCH_THREADS),
    ("nn_server_threads", overrides::SERVER_THREADS),
    ("nn_max_batch_size", overrides::BATCH_SIZE),
];

/// Tune (or recall) the throughput settings and return `cfg` with them filled
/// in. `explicit` names the [`TUNING_FIELDS`] given explicitly (config file or
/// env): those are kept, as are keys in `override_config`. `force` ignores a
/// cached result (`katago-ws autotune --force`). Never fails: on any problem,
/// `cfg` comes back as given and startup continues.
pub async fn apply(
    cfg: EngineConfig,
    concurrency: usize,
    explicit: &[&str],
    force: bool,
) -> EngineConfig {
    if !cfg.autotune {
        tracing::info!("autotune: disabled ([engine] autotune = false)");
        return cfg;
    }
    let pinned = pins(&cfg, explicit);
    if !pinned.is_empty() {
        tracing::info!(
            pinned = %pinned,
            "autotune: explicitly set keys are kept; tuning only the rest"
        );
    }
    let concurrency = concurrency.max(1);
    let pins_all = [
        overrides::ANALYSIS_THREADS,
        overrides::SEARCH_THREADS,
        overrides::SERVER_THREADS,
        overrides::BATCH_SIZE,
    ]
    .iter()
    .all(|key| pinned.get(key).is_some());
    if pins_all && pinned.pins_opencl_device() {
        tracing::info!("autotune: every tuned key is set explicitly; skipped");
        return cfg;
    }

    let key = CacheKey::build(&cfg, concurrency, &pinned);
    let cache_path = cache::resolve_path(&cfg.autotune_cache);
    if let Some(hit) = cache::lookup(&cache_path, &key).filter(|_| !force) {
        let tuned = Overrides::parse(&hit.tuned);
        tracing::info!(
            tuned = %tuned,
            positions_per_sec = hit.positions_per_sec,
            cache = %cache_path.display(),
            "autotune: cache hit"
        );
        return with_tuned(cfg, explicit, &tuned);
    }

    let started = Instant::now();
    let budget = Duration::from_secs(cfg.autotune_timeout_secs.max(1));
    tracing::info!(
        mode = ?cfg.autotune_mode,
        budget_secs = budget.as_secs(),
        concurrency,
        "autotune: no cached result for this GPU/binary/model; tuning KataGo throughput \
         (first run on a new GPU or model also tunes OpenCL kernels)"
    );
    match tokio::time::timeout(budget, tune(&cfg, &pinned, concurrency)).await {
        Ok(Ok(Tuned::Skipped(why))) => {
            tracing::info!("autotune: skipped ({why})");
            cfg
        }
        Ok(Ok(Tuned::Done {
            tuned,
            rate,
            trials,
        })) => {
            let secs = started.elapsed().as_secs_f64();
            tracing::info!(
                tuned = %tuned,
                positions_per_sec = rate,
                trials = trials.len(),
                secs,
                "autotune: done"
            );
            let entry = CacheEntry {
                key,
                tuned: tuned.to_string(),
                positions_per_sec: rate,
                tune_secs: secs,
                tuned_at_unix: cache::now_unix(),
                trials,
            };
            if let Err(err) = cache::store(&cache_path, entry) {
                tracing::warn!(
                    cache = %cache_path.display(),
                    "autotune: could not write cache ({err}); will retune next start"
                );
            }
            with_tuned(cfg, explicit, &tuned)
        }
        Ok(Err(err)) => {
            tracing::warn!("autotune: failed ({err}); using the KataGo config as given");
            cfg
        }
        Err(_) => {
            tracing::warn!(
                budget_secs = budget.as_secs(),
                "autotune: timed out; using the KataGo config as given"
            );
            cfg
        }
    }
}

/// The explicitly set throughput keys: the `explicit` [`TUNING_FIELDS`] (a
/// `0` there means "the config file's value", which leaves the key to the
/// tuner), then `override_config`'s keys, which win on a clash.
fn pins(cfg: &EngineConfig, explicit: &[&str]) -> Overrides {
    let mut pinned = Overrides::default();
    for (field, key) in TUNING_FIELDS {
        let value = field_value(cfg, key);
        if explicit.contains(&field) && value > 0 {
            pinned.set(key, &value.to_string());
        }
    }
    for (key, value) in Overrides::parse(&cfg.override_config).iter() {
        pinned.set(key, value);
    }
    pinned
}

fn field_value(cfg: &EngineConfig, key: &str) -> u32 {
    match key {
        overrides::ANALYSIS_THREADS => cfg.analysis_threads,
        overrides::SEARCH_THREADS => cfg.search_threads_per_analysis,
        overrides::SERVER_THREADS => cfg.nn_server_threads,
        overrides::BATCH_SIZE => cfg.nn_max_batch_size,
        _ => 0,
    }
}

fn field_mut<'cfg>(cfg: &'cfg mut EngineConfig, key: &str) -> Option<&'cfg mut u32> {
    match key {
        overrides::ANALYSIS_THREADS => Some(&mut cfg.analysis_threads),
        overrides::SEARCH_THREADS => Some(&mut cfg.search_threads_per_analysis),
        overrides::SERVER_THREADS => Some(&mut cfg.nn_server_threads),
        overrides::BATCH_SIZE => Some(&mut cfg.nn_max_batch_size),
        _ => None,
    }
}

/// `cfg` with the tuned keys applied: the throughput keys into their
/// `[engine]` fields, the rest (OpenCL device) appended to `override_config`.
/// A field neither explicit nor tuned but pinned through `override_config` is
/// zeroed, so its built-in default isn't emitted alongside the pin.
fn with_tuned(mut cfg: EngineConfig, explicit: &[&str], tuned: &Overrides) -> EngineConfig {
    let in_override = Overrides::parse(&cfg.override_config);
    for (field, key) in TUNING_FIELDS {
        if !explicit.contains(&field)
            && in_override.get(key).is_some()
            && let Some(slot) = field_mut(&mut cfg, key)
        {
            *slot = 0;
        }
    }
    let mut extra = in_override;
    for (key, value) in tuned.iter() {
        match (field_mut(&mut cfg, key), value.parse::<u32>()) {
            (Some(slot), Ok(num)) => *slot = num,
            _ => extra.set(key, value),
        }
    }
    cfg.override_config = extra.to_string();
    cfg
}

/// `cfg` launched with exactly `over` as `-override-config` (the four
/// `[engine]` tuning fields zeroed so their defaults don't join in), and
/// without the human model.
fn launch_cfg(cfg: &EngineConfig, over: &Overrides) -> EngineConfig {
    EngineConfig {
        analysis_threads: 0,
        search_threads_per_analysis: 0,
        nn_server_threads: 0,
        nn_max_batch_size: 0,
        override_config: over.to_string(),
        // Trials never ask for a human profile, so the second net would only
        // cost load time and VRAM.
        human_model: String::new(),
        ..cfg.clone()
    }
}

enum Tuned {
    Skipped(String),
    Done {
        /// Only the keys the tuner chose (pinned keys excluded).
        tuned: Overrides,
        /// Measured positions/s of the winner (0 when nothing was measured).
        rate: f64,
        trials: Vec<TrialRecord>,
    },
}

async fn tune(cfg: &EngineConfig, pinned: &Overrides, concurrency: usize) -> Result<Tuned, String> {
    let backend = katago_backend(&cfg.binary).await?;
    tracing::info!(%backend, "autotune: KataGo backend");
    if backend.to_ascii_lowercase().contains("eigen") {
        return Ok(Tuned::Skipped(format!(
            "{backend} backend: CPU inference, these settings are GPU knobs"
        )));
    }

    let opencl = backend.eq_ignore_ascii_case("opencl");
    let depth = concurrency * POSITIONS_PER_GAME;
    let clock = Instant::now();
    let device = if opencl && !pinned.pins_opencl_device() {
        choose_opencl_device(cfg, pinned).await?
    } else {
        None
    };

    // Trial timing for the fast mode's budget (single task: plain Cells).
    let forgiven: Cell<Option<Duration>> = Cell::new(None);
    let last = Cell::new(Duration::ZERO);
    let (forgiven, last) = (&forgiven, &last);
    let trial = move |cand: Candidate, prune_below: Option<f64>| async move {
        // The first wave (all threads start together) is warm-up, as is
        // startup; KataGo's first-use kernel tuning happens before either.
        let warmup = cand
            .analysis_threads
            .or_else(|| pinned.get(overrides::ANALYSIS_THREADS)?.parse().ok())
            .map_or(256, u64::from)
            .min(u64::try_from(depth).unwrap_or(u64::MAX));
        let over = trial_overrides(pinned, cand, device);
        let started = Instant::now();
        let (outcome, startup) = run_trial(cfg, &over, concurrency, warmup, prune_below).await;
        let mut wall = started.elapsed();
        if forgiven.get().is_none() {
            let extra = startup.unwrap_or_default().saturating_sub(SLOW_START);
            forgiven.set(Some(extra));
            wall = wall.saturating_sub(extra);
        }
        last.set(wall);
        match &outcome {
            Outcome::Measured(rate) => {
                tracing::info!(candidate = %cand, positions_per_sec = rate, "autotune: trial");
            }
            Outcome::Pruned(rate) => tracing::info!(
                candidate = %cand,
                positions_per_sec = rate,
                "autotune: trial stopped early (clearly slower)"
            ),
            Outcome::Failed(err) => {
                tracing::warn!(candidate = %cand, "autotune: trial failed: {err}");
            }
        }
        outcome
    };

    let report = match cfg.autotune_mode {
        AutotuneMode::Thorough => {
            thorough::run(&thorough::Space::new(pinned, depth, opencl), trial).await
        }
        AutotuneMode::Fast => {
            let hw = Hardware::probe().await;
            let space = fast::Space::new(pinned, depth, opencl, hw);
            tracing::info!(
                cpus = hw.cpus,
                gpu_lanes = ?hw.gpu_lanes,
                positions_in_flight = space.total(),
                "autotune: fast search"
            );
            let within_budget = || {
                let spent = clock
                    .elapsed()
                    .saturating_sub(forgiven.get().unwrap_or_default());
                spent + last.get() <= FAST_BUDGET
            };
            fast::run(&space, trial, within_budget).await
        }
    };

    let trials = report
        .history
        .iter()
        .map(|(cand, outcome)| TrialRecord::new(&cand.to_string(), outcome))
        .collect();
    let (best, rate) = report.best.ok_or("every trial failed")?;
    Ok(Tuned::Done {
        tuned: tuned_keys(pinned, best, device),
        rate,
        trials,
    })
}

/// The keys the tuner sets for `cand` (and `device`), minus any pinned ones.
fn tuned_keys(pinned: &Overrides, cand: Candidate, device: Option<usize>) -> Overrides {
    let mut keys = Overrides::default();
    if pinned.get(overrides::SEARCH_THREADS).is_none() {
        keys.set(overrides::SEARCH_THREADS, "1");
    }
    if let Some(threads) = cand.analysis_threads {
        keys.set(overrides::ANALYSIS_THREADS, &threads.to_string());
    }
    if let Some(servers) = cand.servers {
        keys.set(overrides::SERVER_THREADS, &servers.to_string());
    }
    if let Some(batch) = cand.batch {
        keys.set(overrides::BATCH_SIZE, &batch.to_string());
    }
    if let Some(index) = device {
        // Every server thread, or KataGo picks its own device for the rest.
        let servers = cand
            .servers
            .or_else(|| pinned.get(overrides::SERVER_THREADS)?.parse().ok())
            .unwrap_or(1);
        for thread in 0..servers {
            keys.set(
                &format!("openclDeviceToUseThread{thread}"),
                &index.to_string(),
            );
        }
    }
    keys.retain(|key| pinned.get(key).is_none());
    keys
}

/// The full `-override-config` a trial runs with: the pinned keys plus the
/// candidate's.
fn trial_overrides(pinned: &Overrides, cand: Candidate, device: Option<usize>) -> Overrides {
    let mut over = pinned.clone();
    over.fill(&tuned_keys(pinned, cand, device));
    over
}

/// Which backend the binary was built with (`OpenCL`, `CUDA`, `Eigen`, …),
/// from `katago version`.
async fn katago_backend(binary: &str) -> Result<String, String> {
    let output = tokio::time::timeout(
        VERSION_TIMEOUT,
        tokio::process::Command::new(binary)
            .arg("version")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_elapsed| format!("'{binary} version' timed out"))?
    .map_err(|err| format!("'{binary} version': {err}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    search::parse_backend(&text)
        .map(str::to_owned)
        .ok_or_else(|| format!("no backend in '{binary} version' output: {}", text.trim()))
}

/// Start KataGo just long enough to read its OpenCL device list, then pick
/// the device to pin (see [`device::choose`]).
async fn choose_opencl_device(
    cfg: &EngineConfig,
    pinned: &Overrides,
) -> Result<Option<usize>, String> {
    let mut over = pinned.clone();
    over.set("logToStderr", "true");
    let probe_cfg = launch_cfg(cfg, &over);
    let (tap_tx, mut tap_rx) = mpsc::channel::<String>(256);
    let engine = AnalysisEngine::spawn_with_stderr(probe_cfg, Some(tap_tx))
        .await
        .map_err(|err| format!("device probe: {err}"))?;

    let mut log = Vec::new();
    let read = async {
        while let Some(line) = tap_rx.recv().await {
            let done = device::ends_device_listing(&line);
            log.push(line);
            if done {
                break;
            }
        }
    };
    let timed_out = tokio::time::timeout(PROBE_TIMEOUT, read).await.is_err();
    engine.shutdown().await;

    let listing = device::parse(log.iter().map(String::as_str));
    if listing.devices.is_empty() {
        return Err(if timed_out {
            "device probe: KataGo listed no OpenCL device in time".to_owned()
        } else {
            "device probe: KataGo exited without listing an OpenCL device".to_owned()
        });
    }
    for dev in &listing.devices {
        tracing::info!(
            index = dev.index,
            platform = %dev.platform,
            score = ?dev.score,
            "autotune: found OpenCL device {}",
            dev.name
        );
    }
    let choice = device::choose(&listing);
    match choice.and_then(|index| listing.devices.iter().find(|dev| dev.index == index)) {
        Some(OpenClDevice {
            index,
            name,
            platform,
            ..
        }) => {
            tracing::info!(index, %platform, "autotune: pinning OpenCL device {name}");
        }
        None => tracing::info!("autotune: leaving the OpenCL device choice to KataGo"),
    }
    Ok(choice)
}

/// One candidate: a fresh KataGo with `over`, fed the corpus, measured.
async fn run_trial(
    cfg: &EngineConfig,
    over: &Overrides,
    concurrency: usize,
    warmup: u64,
    prune_below: Option<f64>,
) -> (Outcome, Option<Duration>) {
    let trial_cfg = launch_cfg(cfg, over);
    let games: Vec<ParsedGame> = CORPUS
        .iter()
        .filter_map(|sgf| sgf::parse(sgf, cfg.default_board_size, cfg.default_komi).ok())
        .collect();
    let engine = match AnalysisEngine::spawn(trial_cfg).await {
        Ok(engine) => Arc::new(engine),
        Err(err) => return (Outcome::Failed(err.to_string()), None),
    };
    let positions = engine.positions();
    let startup = Cell::new(None);
    let outcome = tokio::select! {
        err = feed(&engine, &games, concurrency * POSITIONS_PER_GAME) => Outcome::Failed(err),
        outcome = measure(positions, warmup, prune_below, &startup) => outcome,
    };
    engine.shutdown().await;
    (outcome, startup.get())
}

/// Keep about `depth` positions queued in KataGo, in [`CHUNK_TURNS`]-turn
/// queries cycling through `games`, until a query fails (returned as the
/// error). Runs until dropped otherwise.
async fn feed(engine: &AnalysisEngine, games: &[ParsedGame], depth: usize) -> String {
    let plan = chunks(games.iter().map(|game| game.moves.len() + 1), CHUNK_TURNS);
    let mut next = plan.iter().cycle();
    let mut queued = 0;
    let mut in_flight = FuturesUnordered::new();
    loop {
        while queued < depth {
            let Some((idx, turns)) = next.next() else {
                break;
            };
            let Some(game) = games.get(*idx) else { break };
            queued += turns.len();
            let turns = turns.clone();
            in_flight.push(async move {
                let len = turns.len();
                (len, engine.analyze_positions(game, turns).await)
            });
        }
        match in_flight.next().await {
            Some((len, Ok(_))) => queued = queued.saturating_sub(len),
            Some((_, Err(err))) => return err.to_string(),
            None => return "no corpus games".to_owned(),
        }
    }
}

/// Split games of the given turn counts into `(game, turns)` chunks of at most
/// `size` turns, game by game, covering every turn exactly once.
fn chunks(turn_counts: impl IntoIterator<Item = usize>, size: usize) -> Vec<(usize, Range<usize>)> {
    let size = size.max(1);
    let mut out = Vec::new();
    for (idx, count) in turn_counts.into_iter().enumerate() {
        let mut start = 0;
        while start < count {
            let end = (start + size).min(count);
            out.push((idx, start..end));
            start = end;
        }
    }
    out
}

/// Wait out `warmup` positions, then measure positions/s over the window.
/// `startup` gets the time until KataGo's first analyzed position.
async fn measure(
    mut positions: watch::Receiver<u64>,
    warmup: u64,
    prune_below: Option<f64>,
    startup: &Cell<Option<Duration>>,
) -> Outcome {
    let gone = || Outcome::Failed("katago exited during the trial".to_owned());
    let origin = Instant::now();
    if positions.wait_for(|&count| count >= 1).await.is_err() {
        return gone();
    }
    startup.set(Some(origin.elapsed()));
    if positions.wait_for(|&count| count >= warmup).await.is_err() {
        return gone();
    }
    let mut meter = search::Meter::start(Instant::now(), *positions.borrow());
    let deadline = tokio::time::Instant::now() + MAX_WINDOW;
    loop {
        let changed = tokio::time::timeout_at(deadline, positions.changed()).await;
        let now = Instant::now();
        match changed {
            Ok(Ok(())) => {
                let count = *positions.borrow_and_update();
                meter.record(now, count);
                if meter.positions() >= WINDOW_POSITIONS {
                    return Outcome::Measured(meter.rate());
                }
                if let Some(floor) = prune_below
                    && meter.positions() >= WINDOW_POSITIONS / 2
                    && meter.rate() < floor
                {
                    return Outcome::Pruned(meter.rate());
                }
            }
            Ok(Err(_)) => return gone(),
            Err(_) if meter.positions() > 0 => return Outcome::Measured(meter.rate()),
            Err(_) => return Outcome::Failed("no positions analyzed in the window".to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(threads: Option<u32>, servers: Option<u32>, batch: Option<u32>) -> Candidate {
        Candidate {
            analysis_threads: threads,
            servers,
            batch,
        }
    }

    #[test]
    fn chunks_cover_every_turn_once() {
        let plan = chunks([33, 16, 1], 16);
        assert_eq!(
            plan,
            vec![(0, 0..16), (0, 16..32), (0, 32..33), (1, 0..16), (2, 0..1)]
        );
    }

    #[test]
    fn the_embedded_corpus_parses() {
        let cfg = EngineConfig::default();
        let positions: usize = CORPUS
            .iter()
            .map(|sgf| sgf::parse(sgf, cfg.default_board_size, cfg.default_komi).unwrap())
            .map(|game| {
                assert_eq!(game.board_size, 19);
                game.moves.len() + 1
            })
            .sum();
        assert!(positions > 3_000, "{positions} positions");
    }

    #[test]
    fn tuned_keys_cover_every_server_thread_with_the_device() {
        let keys = tuned_keys(
            &Overrides::default(),
            cand(Some(256), Some(4), Some(64)),
            Some(0),
        );
        assert_eq!(
            keys.to_string(),
            "numSearchThreadsPerAnalysisThread=1,numAnalysisThreads=256,\
             numNNServerThreadsPerModel=4,nnMaxBatchSize=64,openclDeviceToUseThread0=0,\
             openclDeviceToUseThread1=0,openclDeviceToUseThread2=0,openclDeviceToUseThread3=0"
        );
        let no_device = tuned_keys(
            &Overrides::default(),
            cand(Some(128), Some(8), Some(32)),
            None,
        );
        assert!(!no_device.pins_opencl_device());
    }

    #[test]
    fn explicit_fields_and_override_keys_are_pinned_defaults_are_not() {
        let cfg = EngineConfig {
            analysis_threads: 12,
            search_threads_per_analysis: 1,
            nn_server_threads: 4,
            nn_max_batch_size: 0,
            override_config: "nnMaxBatchSize=48, logDir=/tmp/x".to_owned(),
            ..EngineConfig::default()
        };
        // Only nn_server_threads was given explicitly; analysis_threads=12 and
        // search=1 are built-in defaults, so they stay open to the tuner.
        let pinned = pins(&cfg, &["nn_server_threads"]);
        assert_eq!(
            pinned.to_string(),
            "numNNServerThreadsPerModel=4,nnMaxBatchSize=48,logDir=/tmp/x"
        );
        // An explicit 0 means "the config file's value": left to the tuner.
        let zero = EngineConfig {
            nn_server_threads: 0,
            ..cfg.clone()
        };
        assert!(
            pins(&zero, &["nn_server_threads"])
                .get(overrides::SERVER_THREADS)
                .is_none()
        );
        // override_config wins over an explicit field.
        let clash = EngineConfig {
            override_config: "numNNServerThreadsPerModel=8".to_owned(),
            ..cfg
        };
        assert_eq!(
            pins(&clash, &["nn_server_threads"]).to_string(),
            "numNNServerThreadsPerModel=8"
        );
    }

    #[test]
    fn tuned_values_land_in_the_engine_fields_and_devices_in_the_override() {
        let cfg = EngineConfig {
            analysis_threads: 12,
            search_threads_per_analysis: 1,
            nn_server_threads: 2,
            nn_max_batch_size: 0,
            override_config: "nnMaxBatchSize=48".to_owned(),
            ..EngineConfig::default()
        };
        let explicit = ["nn_server_threads"];
        let pinned = pins(&cfg, &explicit);
        let tuned = tuned_keys(&pinned, cand(Some(256), None, None), Some(1));
        assert_eq!(
            tuned.to_string(),
            "numSearchThreadsPerAnalysisThread=1,numAnalysisThreads=256,\
             openclDeviceToUseThread0=1,openclDeviceToUseThread1=1"
        );
        let out = with_tuned(cfg, &explicit, &tuned);
        assert_eq!(out.analysis_threads, 256);
        assert_eq!(out.search_threads_per_analysis, 1);
        assert_eq!(out.nn_server_threads, 2, "explicit: untouched");
        assert_eq!(out.nn_max_batch_size, 0, "pinned in override_config");
        assert_eq!(
            out.katago_overrides().as_deref(),
            Some(
                "numAnalysisThreads=256,numSearchThreadsPerAnalysisThread=1,\
                 numNNServerThreadsPerModel=2,nnMaxBatchSize=48,\
                 openclDeviceToUseThread0=1,openclDeviceToUseThread1=1"
            )
        );
    }

    #[test]
    fn a_default_is_not_emitted_beside_an_override_pin() {
        // analysis_threads defaults to the CPU count; pinning
        // numAnalysisThreads in override_config must not emit both.
        let cfg = EngineConfig {
            analysis_threads: 12,
            override_config: "numAnalysisThreads=64".to_owned(),
            ..EngineConfig::default()
        };
        let pinned = pins(&cfg, &[]);
        let tuned = tuned_keys(&pinned, cand(None, Some(8), Some(32)), None);
        let out = with_tuned(cfg, &[], &tuned);
        assert_eq!(out.analysis_threads, 0);
        let overrides = out.katago_overrides().unwrap_or_default();
        assert_eq!(
            overrides.matches("numAnalysisThreads").count(),
            1,
            "{overrides}"
        );
        assert!(overrides.contains("numAnalysisThreads=64"));
    }

    #[test]
    fn trials_launch_with_exactly_the_trial_overrides() {
        let cfg = EngineConfig::default();
        let over = trial_overrides(
            &Overrides::default(),
            cand(Some(256), Some(8), Some(32)),
            None,
        );
        let launch = launch_cfg(&cfg, &over);
        assert_eq!(
            launch.katago_overrides().as_deref(),
            Some(
                "numSearchThreadsPerAnalysisThread=1,numAnalysisThreads=256,\
                 numNNServerThreadsPerModel=8,nnMaxBatchSize=32"
            )
        );
    }

    #[tokio::test]
    async fn disabled_autotune_returns_the_config_untouched() {
        let cfg = EngineConfig {
            autotune: false,
            binary: "/nonexistent/katago".to_owned(),
            override_config: "nnMaxBatchSize=16".to_owned(),
            ..EngineConfig::default()
        };
        let out = apply(cfg, 2, &[], false).await;
        assert_eq!(out.override_config, "nnMaxBatchSize=16");
    }

    #[tokio::test]
    async fn a_missing_binary_falls_back_to_the_config() {
        let cache = std::env::temp_dir().join(format!(
            "katago-ws-autotune-fallback-{}.json",
            std::process::id()
        ));
        let cfg = EngineConfig {
            binary: "/nonexistent/katago".to_owned(),
            override_config: "nnMaxBatchSize=16".to_owned(),
            autotune_cache: cache.display().to_string(),
            ..EngineConfig::default()
        };
        let out = apply(cfg, 2, &[], false).await;
        assert_eq!(out.override_config, "nnMaxBatchSize=16");
        assert!(!cache.exists(), "a failure is not cached");
    }
}
