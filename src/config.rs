//! Configuration structs read from the figment (`[engine]`, `[worker]`).
//! muxa's own plugins (`[web]`, `[otel]`, `[sentry]`, `[diesel]`, `[pgmq]`)
//! own their sections.

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
fn default_top_k() -> usize {
    5
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
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            binary: default_binary(),
            config: default_config(),
            model: default_model(),
            max_visits: default_max_visits(),
            analysis_pv_len: default_pv_len(),
            include_ownership: false,
            top_k: default_top_k(),
            default_komi: default_komi(),
            default_board_size: default_board_size(),
            request_timeout_secs: default_timeout(),
        }
    }
}

fn default_concurrency() -> usize {
    2
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
    "http://127.0.0.1:50051".to_owned()
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
/// - `orchestrator_url` / `reconnect_backoff_secs` — the `worker` client; the
///   gRPC endpoint to dial and how long to wait between reconnects.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WorkerConfig {
    /// Number of concurrent worker loops (consumer) / advertised slots (worker).
    pub concurrency: usize,
    /// pgmq visibility timeout (seconds) while a job is in flight.
    pub visibility_timeout_secs: i32,
    /// Long-poll duration (seconds) when the queue is empty.
    pub poll_secs: u64,
    /// Max delivery attempts before a job is marked failed.
    pub max_attempts: i32,
    /// `worker` role: gRPC endpoint of the orchestrator to dial.
    pub orchestrator_url: String,
    /// `worker` role: backoff (seconds) between reconnect attempts.
    pub reconnect_backoff_secs: u64,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            concurrency: default_concurrency(),
            visibility_timeout_secs: default_visibility(),
            poll_secs: default_poll(),
            max_attempts: default_max_attempts(),
            orchestrator_url: default_orchestrator_url(),
            reconnect_backoff_secs: default_reconnect_backoff(),
        }
    }
}

fn default_grpc_listen() -> String {
    "0.0.0.0:50051".to_owned()
}

/// `[orchestrator]` — the gRPC dispatcher workers dial (the `orchestrator` role).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OrchestratorConfig {
    /// `host:port` the gRPC dispatcher binds (e.g. `0.0.0.0:50051`).
    pub grpc_listen: String,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            grpc_listen: default_grpc_listen(),
        }
    }
}
