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

/// Query parameters for `POST /analyse`.
#[derive(Deserialize, JsonSchema, Default)]
pub struct SubmitQuery {
    /// Human SL profile to report the human policy for: `rank_20k` … `rank_9d`,
    /// `rank_<B>_<W>` (Black's and White's ranks), `preaz_…` (the same, 2016
    /// opening style) or `proyear_1800` … `proyear_2023`. The payload then
    /// carries `human_policy`, `human_profile` and `human_prior`s — when the
    /// worker has a human model; without one the job runs without them.
    /// Omit for none. An unknown profile is a 400.
    pub human_profile: Option<String>,
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

/// Body of `POST /analyse/batch`.
#[derive(Deserialize, JsonSchema)]
pub struct BatchSubmitRequest {
    /// The games to analyse, at most [`MAX_BATCH`](crate::http::handlers::MAX_BATCH):
    /// each a bare SGF string, or an object with options.
    pub sgfs: Vec<BatchGame>,
}

/// One game in a batch: a bare SGF, or the SGF with per-game options.
#[derive(Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum BatchGame {
    /// The SGF alone.
    Sgf(String),
    /// The SGF and how to analyse it.
    WithOptions {
        /// The SGF.
        sgf: String,
        /// Human SL profile, as for `POST /analyse?human_profile=`. An unknown
        /// profile refuses this game (its `error`), not the batch.
        #[serde(default)]
        human_profile: Option<String>,
    },
}

impl BatchGame {
    /// The SGF and the (unvalidated) human profile.
    pub fn into_parts(self) -> (String, Option<String>) {
        match self {
            Self::Sgf(sgf) => (sgf, None),
            Self::WithOptions { sgf, human_profile } => (sgf, human_profile),
        }
    }
}

/// One input's outcome in a batch, at the same index as its SGF. Exactly one of
/// `job_id` and `error` is set.
#[derive(Serialize, JsonSchema)]
pub struct BatchItem {
    /// The created job, when the SGF was accepted.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub job_id: Option<Uuid>,
    /// Why the SGF was refused (it didn't parse), when it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Response to `POST /analyse/batch`.
#[derive(Serialize, JsonSchema)]
pub struct BatchSubmitResponse {
    /// One entry per submitted SGF, in input order.
    pub results: Vec<BatchItem>,
}

/// Query parameters for `GET /analyses/changes`.
#[derive(Deserialize, JsonSchema)]
pub struct ChangesQuery {
    /// Return changes after this cursor: the `cursor` of the previous response,
    /// or 0 to start from the beginning.
    #[serde(default)]
    pub since: i64,
    /// Most changes to return, clamped to 1..=1000. Defaults to 500.
    pub limit: Option<i64>,
}

/// One job's state as of one change.
#[derive(Serialize, JsonSchema)]
pub struct Change {
    /// The change's position in the feed (the job's `change_seq` at the time).
    pub seq: i64,
    /// Job id.
    #[schemars(with = "String")]
    pub id: Uuid,
    /// The job's lifecycle state after this change.
    pub status: JobStatus,
    /// Failure detail, present when `status = failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// When the change happened (UTC, RFC 3339).
    #[schemars(with = "String")]
    pub updated_at: DateTime<Utc>,
}

/// Response to `GET /analyses/changes`.
#[derive(Serialize, JsonSchema)]
pub struct ChangesResponse {
    /// Changes after `since`, oldest first. A job appears once per change still
    /// in the window; only its latest state is current.
    pub changes: Vec<Change>,
    /// Pass as `since` next time: the last change's `seq`, or `since` itself
    /// when there were none.
    pub cursor: i64,
    /// The newest `seq` this service has issued. Lower than a client's cursor
    /// means the service's store was reset, and the client should start again
    /// from 0.
    pub latest: i64,
    /// True when more changes are waiting — the page was full, or recent ones
    /// were held back to let them settle. Ask again soon.
    pub more: bool,
}
