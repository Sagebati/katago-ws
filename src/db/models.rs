//! Diesel row models for the `jobs` table.
//!
//! `Job` is the full read projection (the columns `GET /analyse/{id}` returns);
//! `JobSummary` is the lighter projection for the list endpoint and the stream
//! (no `result`). Inserts use the table's column DEFAULTs (`uuidv7()` id, `queued`
//! status, timestamps, `change_seq`), so there's no insert struct — see
//! [`crate::db::submit`].

use chrono::{DateTime, Utc};
use diesel::prelude::{Queryable, Selectable};
use serde_json::Value as Json;
use uuid::Uuid;

use crate::db::status::JobStatus;

/// A job's full current state (the columns the single-job endpoint reads).
#[derive(Queryable, Selectable)]
#[diesel(table_name = crate::db::schema::jobs)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Job {
    /// Job id.
    pub id: Uuid,
    /// Current lifecycle state.
    pub status: JobStatus,
    /// Analysis result (present when `status = done`).
    pub result: Option<Json>,
    /// Failure detail (present when `status = failed`).
    pub error: Option<String>,
}

/// Lightweight projection of a job's lifecycle state — no `result`.
/// Used by the list endpoint and the live stream.
#[derive(Queryable, Selectable)]
#[diesel(table_name = crate::db::schema::jobs)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct JobSummary {
    /// Job id.
    pub id: Uuid,
    /// Current lifecycle state.
    pub status: JobStatus,
    /// Failure detail (present when `status = failed`).
    pub error: Option<String>,
    /// When the job was first queued.
    pub created_at: DateTime<Utc>,
    /// When the job last changed state.
    pub updated_at: DateTime<Utc>,
}
