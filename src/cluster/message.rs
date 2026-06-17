//! Wire messages for the orchestrator↔worker WebSocket control plane.
//!
//! Plain serde types, sent as JSON text frames over the socket. The orchestrator
//! serves the socket on its web port (`/cluster`) and a worker dials in. This
//! replaces the old protobuf/gRPC contract; the semantics are identical — a
//! worker announces its slot count, then receives job pushes and returns results.

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use uuid::Uuid;

/// Worker → orchestrator frames.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClientMsg {
    /// First frame: the worker announces its name and how many concurrent jobs
    /// it accepts.
    Hello {
        /// Max concurrent analyses = lease loops the orchestrator drives for it.
        slots: u32,
        /// Human-readable name the worker picked for itself (shown in `/workers`).
        name: String,
    },
    /// A finished job's outcome.
    Result {
        /// The job this result is for.
        job_id: Uuid,
        /// Success (the analysis JSON) or failure (a message).
        outcome: Outcome,
    },
}

/// Orchestrator → worker: one job to analyze.
#[derive(Debug, Serialize, Deserialize)]
pub struct JobRequest {
    /// Job id (echoed back in the [`ClientMsg::Result`]).
    pub job_id: Uuid,
    /// The SGF to analyze.
    pub sgf: String,
}

/// The result of running one job on a worker. Externally tagged (`{"ok": …}` /
/// `{"err": …}`) so it can carry the analysis `Value` as a newtype variant.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Analysis succeeded — the `GameAnalysis` JSON.
    Ok(Json),
    /// Analysis failed (bad SGF, engine error, …) — the message.
    Err(String),
}
