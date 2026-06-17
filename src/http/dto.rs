//! Request/response payloads for the HTTP API.
//!
//! Every response type derives `JsonSchema` so aide can document it in the
//! OpenAPI spec. `Uuid` and `DateTime` fields are documented as plain strings
//! via `#[schemars(with = "String")]` (schemars 1.0 ships no uuid/chrono
//! integration features), which matches their JSON serialization.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::cluster::registry::{SessionId, WorkerInfo};
use crate::db::models::JobSummary;
use crate::db::status::JobStatus;

/// Path parameter for `GET /analyse/{id}`.
///
/// A transparent `Uuid` newtype: it still deserializes/validates as a UUID, but
/// is documented as a `string` in the OpenAPI spec (schemars 1.0 has no `Uuid`
/// schema, so `Path<Uuid>` can't satisfy aide's `OperationInput` directly).
#[derive(Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct JobId(#[schemars(with = "String")] pub Uuid);

/// Query parameters for `GET /analyse/{id}`.
#[derive(Deserialize, JsonSchema)]
pub struct StatusQuery {
    /// Long-poll: maximum seconds to wait for the job to reach a terminal state
    /// (`done`/`failed`) before responding. When set, the request blocks until
    /// the job finishes or this budget elapses (whichever first), then returns
    /// the current state. Capped server-side. Omit (or `0`) to respond at once.
    pub wait: Option<u64>,
}

/// Response to a successful `POST /analyse`.
#[derive(Serialize, JsonSchema)]
pub struct SubmitResponse {
    /// The created job id.
    #[schemars(with = "String")]
    pub job_id: Uuid,
    /// Always [`JobStatus::Queued`].
    pub status: JobStatus,
}

/// Response to `GET /analyse/{id}`.
#[derive(Serialize, JsonSchema)]
pub struct StatusResponse {
    /// Job id.
    #[schemars(with = "String")]
    pub id: Uuid,
    /// Current lifecycle state.
    pub status: JobStatus,
    /// The analysis payload, present when `status = done`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Failure detail, present when `status = failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A single job's lifecycle state — the element of `GET /analyses` and the
/// payload of each `GET /analyses/stream` event. Lightweight (no SGF/result).
#[derive(Serialize, JsonSchema)]
pub struct AnalysisSummary {
    /// Job id.
    #[schemars(with = "String")]
    pub id: Uuid,
    /// Current lifecycle state.
    pub status: JobStatus,
    /// Failure detail, present when `status = failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// When the job was first queued (UTC, RFC 3339).
    #[schemars(with = "String")]
    pub created_at: DateTime<Utc>,
    /// When the job last changed state (UTC, RFC 3339).
    #[schemars(with = "String")]
    pub updated_at: DateTime<Utc>,
}

impl From<JobSummary> for AnalysisSummary {
    fn from(summary: JobSummary) -> Self {
        Self {
            id: summary.id,
            status: summary.status,
            error: summary.error,
            created_at: summary.created_at,
            updated_at: summary.updated_at,
        }
    }
}

/// Response to `GET /analyses`.
#[derive(Serialize, JsonSchema)]
pub struct ListResponse {
    /// All analysis jobs, newest first.
    pub analyses: Vec<AnalysisSummary>,
}

/// One connected worker in the `GET /workers` response.
#[derive(Serialize, JsonSchema)]
pub struct WorkerSummary {
    /// Orchestrator-minted session id (not stable across the worker's reconnects).
    pub id: SessionId,
    /// The name the worker registered with (from its `Hello`).
    pub name: String,
    /// Peer address (`host:port`), when the transport exposed one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    /// Max concurrent jobs the worker advertised on connect.
    pub slots: u32,
    /// When the session connected (UTC, RFC 3339).
    #[schemars(with = "String")]
    pub connected_at: DateTime<Utc>,
}

impl From<WorkerInfo> for WorkerSummary {
    fn from(info: WorkerInfo) -> Self {
        Self {
            id: info.id,
            name: info.name,
            peer: info.peer.map(|addr| addr.to_string()),
            slots: info.slots,
            connected_at: info.connected_at,
        }
    }
}

/// Response to `GET /workers`.
#[derive(Serialize, JsonSchema)]
pub struct WorkersResponse {
    /// Workers connected to this orchestrator, oldest connection first.
    pub workers: Vec<WorkerSummary>,
}
