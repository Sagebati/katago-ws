//! The job lifecycle status — an enum at every layer (Rust, OpenAPI, and the
//! Postgres `job_status` type), never a free-form string. This is the Rust
//! mapping of the Postgres `job_status` enum, via `diesel-derive-enum`.

use diesel_derive_enum::DbEnum;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Lifecycle state of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, DbEnum)]
#[serde(rename_all = "snake_case")]
#[ExistingTypePath = "crate::db::schema::sql_types::JobStatus"]
#[DbValueStyle = "snake_case"]
pub enum JobStatus {
    /// Submitted and waiting to be picked up.
    Queued,
    /// Being analyzed by a worker.
    Running,
    /// Analysis finished successfully (carries a `result`).
    Done,
    /// Analysis was given up on after exhausting retries (carries an `error`).
    Failed,
}
