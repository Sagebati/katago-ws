//! Configuration structs read from the figment (`[engine]`, `[worker]`).
//! muxa's own plugins (`[web]`, `[otel]`, `[sentry]`, `[diesel]`, `[pgmq]`)
//! own their sections.

use secrecy::SecretString;
use serde::{Deserialize, Serialize};

fn default_binary() -> String {
    "katago-rs".to_owned()
}
fn default_config() -> String {
    "analysis.cfg".to_owned()
}
/// katago-rs loads a network compiled into it from its `.bpk` weights, matched on
/// the file name (`g170e-b10c128.bpk` is that net).
fn default_model() -> String {
    "g170e-b10c128.bpk".to_owned()
}
fn default_max_visits() -> u32 {
    100
}
/// Long enough for a whole local sequence (a joseki, a fight) rather than its
/// first few moves; KataGo's own default.
fn default_pv_len() -> u32 {
    15
}
/// The candidates are already searched by the time they're reported, so a
/// larger list costs payload size only, not engine time. Ten rather than five
/// because a consumer checking a proposed move against the engine's opinion can
/// only judge moves that appear here — everything else it has to call unknown.
fn default_top_k() -> usize {
    10
}
fn default_komi() -> f32 {
    7.5
}
fn default_board_size() -> u8 {
    19
}
fn default_timeout() -> u64 {
    600
}
/// Positions the engine searches in parallel: by default, one per hardware thread
/// this process may use (cgroup CPU limits included). A game is submitted as one
/// query covering every turn, so there are always hundreds of positions
/// waiting: the throughput preset (many positions × one thread each) keeps the
/// NN batch full without threads contending over the same tree. Falls back to
/// the stock config's 32 if the thread count can't be read.
fn default_analysis_threads() -> u32 {
    std::thread::available_parallelism().map_or(32, |threads| {
        u32::try_from(threads.get()).unwrap_or(u32::MAX)
    })
}
fn default_search_threads_per_analysis() -> u32 {
    1
}
fn default_refine_winrate_delta() -> f32 {
    0.03
}
fn default_refine_score_delta() -> f32 {
    1.0
}
fn default_autotune_timeout() -> u64 {
    300
}

/// katago-rs's compute backend (`-backend`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineBackend {
    /// The CPU (burn flex).
    #[default]
    Flex,
    /// Any GPU with a Vulkan (or Metal) driver, through wgpu.
    Wgpu,
    /// AMD GPUs through ROCm (HIP).
    Rocm,
    /// Not katago-rs: a stock KataGo binary (no `-backend`, no `burnUseFP16`), for comparisons
    /// (`scripts/bench.sh` with `BENCH_BACKEND=upstream`). The payload names the engine `katago`.
    Upstream,
}

impl EngineBackend {
    /// The backend's name: the value of katago-rs's `-backend` (`upstream` for stock KataGo).
    #[must_use]
    pub fn as_arg(self) -> &'static str {
        match self {
            Self::Flex => "flex",
            Self::Wgpu => "wgpu",
            Self::Rocm => "rocm",
            Self::Upstream => "upstream",
        }
    }

    /// Whether the engine is katago-rs (it takes `-backend` and `burnUseFP16`), not stock KataGo.
    #[must_use]
    pub fn is_katago_rs(self) -> bool {
        self != Self::Upstream
    }

    /// The payload's `engine.name`.
    #[must_use]
    pub fn engine_name(self) -> &'static str {
        if self.is_katago_rs() {
            "katago-rs"
        } else {
            "katago"
        }
    }

    /// Whether the backend runs on a GPU.
    #[must_use]
    pub fn is_gpu(self) -> bool {
        self != Self::Flex
    }
}

/// How hard `[engine] autotune` searches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AutotuneMode {
    /// Every candidate for the backend, near-ties re-measured.
    #[default]
    Thorough,
    /// A few trials guided by the hardware and the backend; under ~30 s.
    Fast,
}

/// `[engine]` — how to launch and drive the analysis engine: katago-rs, speaking
/// KataGo's analysis protocol.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct EngineConfig {
    /// Path to (or name of) the `katago-rs` binary.
    pub binary: String,
    /// katago-rs's compute backend (`-backend`): `flex` (CPU), `wgpu`
    /// (Vulkan) or `rocm` (AMD HIP). The binary must have it compiled in.
    pub backend: EngineBackend,
    /// Run the GPU network's 3x3 convolutions in f16 (`burnUseFP16 = true`):
    /// about +20% throughput, within upstream KataGo's own FP16 error. GPU
    /// backends only (katago-rs rejects it on the CPU).
    pub fp16: bool,
    /// Analysis config file (`-config`), KataGo's format.
    pub config: String,
    /// Network weights (`-model`): a `.bpk` named after one of the networks
    /// compiled into katago-rs (`g170e-b10c128.bpk`, `kata1-b18c384nbt.bpk`).
    pub model: String,
    /// Max visits per analyzed position.
    pub max_visits: u32,
    /// Length of principal variation returned per candidate.
    pub analysis_pv_len: u32,
    /// Whether to request ownership maps.
    pub include_ownership: bool,
    /// Whether to request the per-point standard deviation of ownership across
    /// the search tree (`includeOwnershipStdev`): how settled each point is.
    /// No extra search, only payload.
    pub include_ownership_stdev: bool,
    /// Whether to request the raw policy (`includePolicy`): the network's
    /// prior over every move before any search, and — with a human model and a
    /// job's `human_profile` — the human policy. No extra search, only payload.
    pub include_policy: bool,
    /// Human SL network (`-human-model`, e.g. `b18c384nbt-humanv0.bpk`).
    /// Empty (the default) loads none: jobs asking for a `human_profile` get
    /// no human policy. Loaded beside the main model — roughly another 100 MB
    /// of VRAM — and evaluated once per analysed position, not per visit.
    pub human_model: String,
    /// Number of candidate moves to report per position.
    pub top_k: usize,
    /// Komi to assume when an SGF omits it.
    pub default_komi: f32,
    /// Board size to assume when an SGF omits it.
    pub default_board_size: u8,
    /// Per-game analysis timeout (seconds).
    pub request_timeout_secs: u64,

    // Search/batching tuning, passed as `-override-config` so it is set
    // per deploy (env vars) without editing or rebuilding the shipped
    // `analysis.cfg`. For each numeric knob, `0` keeps the config file's value.
    /// `numAnalysisThreads`: positions searched in parallel.
    pub analysis_threads: u32,
    /// `numSearchThreadsPerAnalysisThread`: threads per position's search tree.
    pub search_threads_per_analysis: u32,
    /// `nnMaxBatchSize`: max positions per NN batch, per GPU. Should be at least
    /// `analysis_threads * search_threads_per_analysis / GPUs`.
    pub nn_max_batch_size: u32,
    /// `numNNServerThreadsPerModel`: NN server threads (one per GPU).
    pub nn_server_threads: u32,
    /// Extra raw `key=value,key=value` overrides appended verbatim.
    pub override_config: String,

    // Two-pass analysis. Every turn is first searched with `screen_visits`;
    // then only the turns around moves whose screened evaluation swings by at
    // least a refine delta are re-searched at `max_visits`. Quiet moves (most
    // of a game) keep their cheap screening result. `0` (the default), or a
    // value `>= max_visits`, disables it: one pass at `max_visits`.
    /// Visits per position in the screening pass.
    pub screen_visits: u32,
    /// Refine a move whose screened win-rate swing (either direction) is at
    /// least this. Default: the inaccuracy threshold.
    pub refine_winrate_delta: f32,
    /// Refine a move whose screened score swing (points, either direction) is
    /// at least this. Default: the inaccuracy threshold.
    pub refine_score_delta: f32,

    /// Auto-tune the engine's throughput settings (`analysis_threads`,
    /// `nn_server_threads` × `nn_max_batch_size`) before launching it,
    /// caching the result (see `engine::autotune`). Settings
    /// given explicitly (config file or env, including keys in
    /// `override_config`) are kept and only the rest are tuned.
    /// `KATAGO_WS_ENGINE__AUTOTUNE=false` turns it off completely.
    pub autotune: bool,
    /// `thorough` (default: every candidate, near-ties re-measured; a few
    /// minutes on a GPU) or `fast` (from the backend's usual winner, a few
    /// trials). `katago-ws autotune --fast` runs the latter once and exits.
    pub autotune_mode: AutotuneMode,
    /// Where the auto-tune result is cached. Empty =
    /// `~/.katago/katago-ws-autotune.json`: persist `~/.katago` in a container
    /// volume to keep it across restarts.
    pub autotune_cache: String,
    /// Give up tuning (and launch with the config as given) after this many
    /// seconds. Generous because a first run on a new GPU also compiles every
    /// GPU kernel once.
    pub autotune_timeout_secs: u64,
}

impl EngineConfig {
    /// Whether the two-pass (screen, then refine) analysis is enabled.
    #[must_use]
    pub fn two_pass(&self) -> bool {
        self.screen_visits > 0 && self.screen_visits < self.max_visits
    }

    /// The `-override-config` argument for the engine, or `None` when nothing
    /// is overridden.
    #[must_use]
    pub fn katago_overrides(&self) -> Option<String> {
        let numeric = [
            ("numAnalysisThreads", self.analysis_threads),
            (
                "numSearchThreadsPerAnalysisThread",
                self.search_threads_per_analysis,
            ),
            ("nnMaxBatchSize", self.nn_max_batch_size),
            ("numNNServerThreadsPerModel", self.nn_server_threads),
        ];
        let pairs = numeric
            .into_iter()
            .filter(|&(_, value)| value > 0)
            .map(|(key, value)| format!("{key}={value}"));
        let fp16 =
            (self.fp16 && self.backend.is_katago_rs()).then(|| "burnUseFP16=true".to_owned());
        let extra = self
            .override_config
            .split(',')
            .map(str::trim)
            .filter(|pair| !pair.is_empty())
            .map(str::to_owned);
        let joined = pairs.chain(fp16).chain(extra).collect::<Vec<_>>().join(",");
        (!joined.is_empty()).then_some(joined)
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            binary: default_binary(),
            backend: EngineBackend::default(),
            fp16: false,
            config: default_config(),
            model: default_model(),
            max_visits: default_max_visits(),
            analysis_pv_len: default_pv_len(),
            // On by default: ownership is what lets a consumer say *which*
            // stones a mistake cost, rather than only how many points. It
            // roughly doubles the engine's memory use and costs a little speed —
            // turn it off on a memory-constrained worker.
            include_ownership: true,
            include_ownership_stdev: true,
            include_policy: true,
            human_model: String::new(),
            top_k: default_top_k(),
            default_komi: default_komi(),
            default_board_size: default_board_size(),
            request_timeout_secs: default_timeout(),
            analysis_threads: default_analysis_threads(),
            search_threads_per_analysis: default_search_threads_per_analysis(),
            nn_max_batch_size: 0,
            nn_server_threads: 0,
            override_config: String::new(),
            screen_visits: 0,
            refine_winrate_delta: default_refine_winrate_delta(),
            refine_score_delta: default_refine_score_delta(),
            autotune: true,
            autotune_mode: AutotuneMode::default(),
            autotune_cache: String::new(),
            autotune_timeout_secs: default_autotune_timeout(),
        }
    }
}

/// Games in flight per engine. Each one is hundreds of positions, so a few
/// suffice to keep the engine's queue from draining while one game's last turns
/// finish (and, with two-pass analysis, while a game waits on its refinement).
fn default_concurrency() -> usize {
    4
}
fn default_visibility() -> i32 {
    600
}
fn default_poll() -> u64 {
    5
}
fn default_max_attempts() -> i32 {
    3
}
fn default_orchestrator_url() -> String {
    "ws://127.0.0.1:3000/cluster".to_owned()
}
fn default_reconnect_backoff() -> u64 {
    3
}

/// `[worker]` configuration.
///
/// The queue is not configurable: it's fixed to [`crate::queue::Queue::Analysis`]
/// (the same queue `PgmqPlugin` creates and `db::submit` produces to), so the
/// consumer can't drift from the queue that actually exists.
///
/// Field relevance depends on the launch role:
/// - `concurrency` — consumer lease loops (`standalone`/`orchestrator`) **and**
///   the slots a `worker` advertises to the orchestrator.
/// - `visibility_timeout_secs` / `poll_secs` / `max_attempts` — the pgmq consumer
///   side (`standalone`/`orchestrator`); ignored by a `worker` (no DB).
/// - `orchestrator_url` / `auth_token` / `reconnect_backoff_secs` — the `worker`
///   client; the WebSocket URL to dial, the secret it presents, and the backoff.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WorkerConfig {
    /// Number of concurrent worker loops (consumer) / advertised slots (worker).
    pub concurrency: usize,
    /// `worker` role: human-readable name to register under. Empty ⇒ a friendly
    /// one is generated at startup (e.g. `brave-otter-42`).
    pub name: String,
    /// pgmq visibility timeout (seconds) while a job is in flight.
    pub visibility_timeout_secs: i32,
    /// Long-poll duration (seconds) when the queue is empty.
    pub poll_secs: u64,
    /// Max delivery attempts before a job is marked failed.
    pub max_attempts: i32,
    /// `worker` role: WebSocket URL of the orchestrator's cluster socket
    /// (e.g. `ws://orchestrator:3000/cluster`, or `wss://…/cluster` through TLS).
    pub orchestrator_url: String,
    /// `worker` role: shared secret presented (as a Bearer token) on connect.
    /// Must match the orchestrator's `[orchestrator].auth_token`; empty = none.
    pub auth_token: SecretString,
    /// `worker` role: backoff (seconds) between reconnect attempts.
    pub reconnect_backoff_secs: u64,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            concurrency: default_concurrency(),
            name: String::new(),
            visibility_timeout_secs: default_visibility(),
            poll_secs: default_poll(),
            max_attempts: default_max_attempts(),
            orchestrator_url: default_orchestrator_url(),
            auth_token: SecretString::from(String::new()),
            reconnect_backoff_secs: default_reconnect_backoff(),
        }
    }
}

/// `[orchestrator]` — the `orchestrator` role's cluster settings. The dispatcher
/// is served on the web port at `/cluster` (no separate listen address), so the
/// only knob is the worker-auth secret.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OrchestratorConfig {
    /// Shared secret a worker must present (Bearer) to open the cluster socket.
    /// Empty disables auth — only safe on a trusted private network; set it
    /// whenever the socket is publicly reachable (e.g. behind a public reverse proxy).
    pub auth_token: SecretString,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            auth_token: SecretString::from(String::new()),
        }
    }
}

/// `[api]` — settings for trusted API clients.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ApiConfig {
    /// Shared secret a client presents (Bearer) to use `POST /analyse/batch`.
    /// That endpoint is not IP rate-limited — it exists so a backend submitting
    /// on behalf of many users isn't throttled like one anonymous caller. Empty
    /// (the default) leaves it open to anyone, like `[orchestrator].auth_token`;
    /// set it on a public deployment once its clients send it. From the
    /// environment (`KATAGO_WS_API__CLIENT_TOKEN`), never in this file.
    pub client_token: SecretString,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            client_token: SecretString::from(String::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_overrides_use_one_position_per_system_thread() {
        let threads = std::thread::available_parallelism().unwrap().get();
        assert_eq!(
            EngineConfig::default().katago_overrides(),
            Some(format!(
                "numAnalysisThreads={threads},numSearchThreadsPerAnalysisThread=1"
            ))
        );
    }

    #[test]
    fn zero_keeps_the_config_file_and_raw_overrides_are_appended() {
        let cfg = EngineConfig {
            analysis_threads: 0,
            search_threads_per_analysis: 0,
            nn_max_batch_size: 128,
            override_config: " requireMaxBoardSize=true , ,maxBoardXSizeForNNBuffer=19".to_owned(),
            ..EngineConfig::default()
        };
        assert_eq!(
            cfg.katago_overrides().as_deref(),
            Some("nnMaxBatchSize=128,requireMaxBoardSize=true,maxBoardXSizeForNNBuffer=19")
        );

        let none = EngineConfig {
            analysis_threads: 0,
            search_threads_per_analysis: 0,
            ..EngineConfig::default()
        };
        assert_eq!(none.katago_overrides(), None);
    }

    #[test]
    fn fp16_is_passed_as_the_burn_override_before_the_raw_ones() {
        let cfg = EngineConfig {
            analysis_threads: 0,
            search_threads_per_analysis: 0,
            fp16: true,
            override_config: "logToStderr=false".to_owned(),
            ..EngineConfig::default()
        };
        assert_eq!(
            cfg.katago_overrides().as_deref(),
            Some("burnUseFP16=true,logToStderr=false")
        );
    }

    #[test]
    fn backend_names_are_katago_rs_flags() {
        for (backend, arg) in [
            (EngineBackend::Flex, "flex"),
            (EngineBackend::Wgpu, "wgpu"),
            (EngineBackend::Rocm, "rocm"),
            (EngineBackend::Upstream, "upstream"),
        ] {
            assert_eq!(backend.as_arg(), arg);
            let parsed: EngineBackend = serde_json::from_value(serde_json::json!(arg)).unwrap();
            assert_eq!(parsed, backend);
        }
        assert!(!EngineBackend::Flex.is_gpu() && EngineBackend::Rocm.is_gpu());
        assert!(EngineBackend::Wgpu.is_katago_rs() && !EngineBackend::Upstream.is_katago_rs());
        assert_eq!(EngineBackend::Upstream.engine_name(), "katago");
    }

    #[test]
    fn two_pass_needs_a_screen_shallower_than_the_full_search() {
        let base = EngineConfig::default();
        assert!(!base.two_pass());
        assert!(
            EngineConfig {
                screen_visits: 16,
                ..base.clone()
            }
            .two_pass()
        );
        assert!(
            !EngineConfig {
                screen_visits: base.max_visits,
                ..base
            }
            .two_pass()
        );
    }
}
