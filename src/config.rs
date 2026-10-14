//! Configuration structs read from the figment (`[engine]`, `[worker]`).
//! muxa's own plugins (`[web]`, `[otel]`, `[sentry]`, `[diesel]`, `[pgmq]`)
//! own their sections.

use secrecy::SecretString;
use serde::Deserialize;

fn default_binary() -> String {
    "katago".to_owned()
}
fn default_config() -> String {
    "analysis.cfg".to_owned()
}
fn default_model() -> String {
    "model.bin.gz".to_owned()
}
fn default_max_visits() -> u32 {
    100
}
fn default_pv_len() -> u32 {
    8
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
/// Positions KataGo searches in parallel: by default, one per hardware thread
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

/// `[engine]` — how to launch and drive the KataGo analysis engine.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    /// Path to (or name of) the `katago` binary.
    pub binary: String,
    /// KataGo analysis config file (`-config`).
    pub config: String,
    /// KataGo model/weights file (`-model`).
    pub model: String,
    /// Max visits per analyzed position.
    pub max_visits: u32,
    /// Length of principal variation returned per candidate.
    pub analysis_pv_len: u32,
    /// Whether to request ownership maps.
    pub include_ownership: bool,
    /// Number of candidate moves to report per position.
    pub top_k: usize,
    /// Komi to assume when an SGF omits it.
    pub default_komi: f32,
    /// Board size to assume when an SGF omits it.
    pub default_board_size: u8,
    /// Per-game analysis timeout (seconds).
    pub request_timeout_secs: u64,

    // KataGo search/batching tuning, passed as `-override-config` so it is set
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
}

impl EngineConfig {
    /// Whether the two-pass (screen, then refine) analysis is enabled.
    #[must_use]
    pub fn two_pass(&self) -> bool {
        self.screen_visits > 0 && self.screen_visits < self.max_visits
    }

    /// The `-override-config` argument for KataGo, or `None` when nothing is
    /// overridden.
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
        let extra = self
            .override_config
            .split(',')
            .map(str::trim)
            .filter(|pair| !pair.is_empty())
            .map(str::to_owned);
        let joined = pairs.chain(extra).collect::<Vec<_>>().join(",");
        (!joined.is_empty()).then_some(joined)
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            binary: default_binary(),
            config: default_config(),
            model: default_model(),
            max_visits: default_max_visits(),
            analysis_pv_len: default_pv_len(),
            // On by default: ownership is what lets a consumer say *which*
            // stones a mistake cost, rather than only how many points. It
            // roughly doubles KataGo's memory use and costs a little speed —
            // turn it off on a memory-constrained worker.
            include_ownership: true,
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
        }
    }
}

/// Games in flight per engine. Each one is hundreds of positions, so a few
/// suffice to keep KataGo's queue from draining while one game's last turns
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
