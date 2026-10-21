//! Postgres access (Diesel async).
//!
//! One LOGGED `jobs` table is both the durable system of record and the realtime
//! read model; the pgmq queue is the authority for in-flight work and carries the
//! SGF (so a crash replays unfinished jobs). A job's row is updated in place as it
//! moves through its lifecycle, and every write stamps a monotonic `change_seq`
//! (the stream cursor) and `updated_at` via a DB trigger.
//!
//! Transitions are guarded on a non-terminal status, so a job redelivered by the
//! queue after it already finished can't be reverted or double-written.

pub mod models;
pub mod schema;
pub mod status;

use diesel::{
    ExpressionMethods as _, OptionalExtension as _, QueryDsl as _, SelectableHelper as _,
};
use diesel_async::{AsyncConnection as _, AsyncPgConnection, RunQueryDsl as _};
use muxa::prelude::DieselPool;
use pgmq::queue::Queue as _;
use serde_json::Value as Json;
use tracing::Instrument as _;
use uuid::Uuid;

use crate::db::models::{Job, JobSummary};
use crate::db::schema::jobs;
use crate::db::status::JobStatus;
use crate::error::{AppError, AppResult};
use crate::queue::{JobMessage, Queue};

fn db_err<E: std::fmt::Display>(err: E) -> AppError {
    AppError::Db(err.to_string())
}

/// The statuses a transition may move *from* — i.e. not yet terminal. Guarding
/// updates with this makes them idempotent against queue redelivery.
const NON_TERMINAL: [JobStatus; 2] = [JobStatus::Queued, JobStatus::Running];

/// What a client submits: the game, and how to analyse it.
pub struct NewJob {
    /// The SGF to analyse.
    pub sgf: String,
    /// Human SL profile to report the human policy for, already validated.
    pub human_profile: Option<String>,
}

impl NewJob {
    fn into_message(self, job_id: Uuid) -> JobMessage {
        JobMessage {
            job_id,
            sgf: self.sgf,
            human_profile: self.human_profile,
        }
    }
}

/// Submit a job. The **database** mints the (time-ordered v7) id via the `jobs.id`
/// DEFAULT — `RETURNING` hands it back — and the same transaction enqueues the
/// work (SGF in the message). One transaction, so a client never gets an id for a
/// job that isn't both visible and queued.
pub async fn submit(pool: &DieselPool, job: NewJob) -> AppResult<Uuid> {
    let mut conn = pool.0.get().await.map_err(db_err)?;
    conn.transaction::<Uuid, AppError, _>(async move |conn| {
        // All columns default (uuidv7 id, 'queued' status, timestamps, change_seq);
        // RETURNING reads the minted id back.
        let job_id: Uuid = diesel::insert_into(jobs::table)
            .default_values()
            .returning(jobs::id)
            .get_result(conn)
            .await?;
        publish(conn, job.into_message(job_id)).await?;
        Ok(job_id)
    })
    .await
}

/// Enqueue one job's message on the caller's connection (inside its
/// transaction), under a `queue.publish` span: Sentry's Queues view pairs it with
/// the worker's `queue.process` span by message id.
async fn publish(conn: &mut AsyncPgConnection, message: JobMessage) -> AppResult<()> {
    let span = tracing::info_span!(
        "queue.publish",
        sentry.op = "queue.publish",
        messaging.system = "pgmq",
        messaging.destination.name = Queue::Analysis.as_str(),
        messaging.message.id = tracing::field::Empty,
    );
    // No headers, no delay.
    let msg_id = conn
        .send(Queue::Analysis.as_str(), message, (), 0)
        .instrument(span.clone())
        .await
        .map_err(|err| AppError::Queue(err.to_string()))?;
    span.record("messaging.message.id", msg_id);
    Ok(())
}

/// Submit several jobs at once, in **one** transaction: every SGF gets a row and
/// a queue message, or none do. Ids come back in input order.
pub async fn submit_many(pool: &DieselPool, jobs: Vec<NewJob>) -> AppResult<Vec<Uuid>> {
    if jobs.is_empty() {
        return Ok(Vec::new());
    }
    let mut conn = pool.0.get().await.map_err(db_err)?;
    conn.transaction::<Vec<Uuid>, AppError, _>(async move |conn| {
        let mut ids = Vec::with_capacity(jobs.len());
        for job in jobs {
            let job_id: Uuid = diesel::insert_into(jobs::table)
                .default_values()
                .returning(jobs::id)
                .get_result(conn)
                .await?;
            publish(conn, job.into_message(job_id)).await?;
            ids.push(job_id);
        }
        Ok(ids)
    })
    .await
}

/// Mark a job as running (a worker picked it up). No-op if already terminal.
pub async fn set_running(pool: &DieselPool, job_id: Uuid) -> AppResult<()> {
    let mut conn = pool.0.get().await.map_err(db_err)?;
    diesel::update(jobs::table)
        .filter(jobs::id.eq(job_id))
        .filter(jobs::status.eq_any(NON_TERMINAL))
        .set(jobs::status.eq(JobStatus::Running))
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;
    Ok(())
}

/// Record a transient attempt failure: the job stays retryable (non-terminal),
/// and the latest error is kept for observability. The attempt *count* is the
/// queue message's `read_ct`, so it isn't duplicated here.
pub async fn record_attempt_failure(pool: &DieselPool, job_id: Uuid, error: &str) -> AppResult<()> {
    let mut conn = pool.0.get().await.map_err(db_err)?;
    diesel::update(jobs::table)
        .filter(jobs::id.eq(job_id))
        .filter(jobs::status.eq_any(NON_TERMINAL))
        .set(jobs::last_error.eq(Some(error.to_owned())))
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;
    Ok(())
}

/// Store a successful analysis result (terminal `done`). Idempotent: a job
/// redelivered after it already finished is left untouched.
pub async fn set_done(pool: &DieselPool, job_id: Uuid, result: Json) -> AppResult<()> {
    let mut conn = pool.0.get().await.map_err(db_err)?;
    diesel::update(jobs::table)
        .filter(jobs::id.eq(job_id))
        .filter(jobs::status.eq_any(NON_TERMINAL))
        .set((
            jobs::status.eq(JobStatus::Done),
            jobs::result.eq(Some(result)),
            jobs::error.eq::<Option<String>>(None),
        ))
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;
    Ok(())
}

/// Give up on a job after exhausting retries (terminal `failed`). Idempotent in
/// the same way as [`set_done`].
pub async fn set_failed(pool: &DieselPool, job_id: Uuid, error: &str) -> AppResult<()> {
    let mut conn = pool.0.get().await.map_err(db_err)?;
    diesel::update(jobs::table)
        .filter(jobs::id.eq(job_id))
        .filter(jobs::status.eq_any(NON_TERMINAL))
        .set((
            jobs::status.eq(JobStatus::Failed),
            jobs::error.eq(Some(error.to_owned())),
            jobs::result.eq::<Option<Json>>(None),
        ))
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;
    Ok(())
}

/// List all jobs, newest first.
pub async fn list_jobs(pool: &DieselPool) -> AppResult<Vec<JobSummary>> {
    let mut conn = pool.0.get().await.map_err(db_err)?;
    let rows = jobs::table
        .order(jobs::created_at.desc())
        .select(JobSummary::as_select())
        .load(&mut *conn)
        .await
        .map_err(db_err)?;
    Ok(rows)
}

/// Up to `limit` jobs in a given lifecycle state, **oldest first**. Oldest-first
/// is the queue's FIFO processing order, so for [`JobStatus::Queued`] the head of
/// the result is the next job a worker will pick up — the "top of the queue".
pub async fn jobs_in_state(
    pool: &DieselPool,
    status: JobStatus,
    limit: i64,
) -> AppResult<Vec<JobSummary>> {
    let mut conn = pool.0.get().await.map_err(db_err)?;
    let rows = jobs::table
        .filter(jobs::status.eq(status))
        .order(jobs::created_at.asc())
        .limit(limit)
        .select(JobSummary::as_select())
        .load(&mut *conn)
        .await
        .map_err(db_err)?;
    Ok(rows)
}

/// Count of jobs currently in a given lifecycle state.
pub async fn count_in_state(pool: &DieselPool, status: JobStatus) -> AppResult<i64> {
    let mut conn = pool.0.get().await.map_err(db_err)?;
    let total: i64 = jobs::table
        .filter(jobs::status.eq(status))
        .count()
        .get_result(&mut *conn)
        .await
        .map_err(db_err)?;
    Ok(total)
}

/// Fetch a job's current state by id.
pub async fn get_job(pool: &DieselPool, job_id: Uuid) -> AppResult<Option<Job>> {
    let mut conn = pool.0.get().await.map_err(db_err)?;
    let row = jobs::table
        .filter(jobs::id.eq(job_id))
        .select(Job::as_select())
        .first(&mut *conn)
        .await
        .optional()
        .map_err(db_err)?;
    Ok(row)
}

/// Jobs whose `change_seq` is greater than `cursor`, oldest change first — the
/// live stream's tail. Each row is returned with its `change_seq` so the caller
/// can advance its monotonic cursor; the row itself *is* the new state to emit
/// (no second fetch). Reading persisted state (not an in-process broadcast) is
/// what makes the stream correct under horizontal scaling.
pub async fn changes_since(pool: &DieselPool, cursor: i64) -> AppResult<Vec<(i64, JobSummary)>> {
    let mut conn = pool.0.get().await.map_err(db_err)?;
    let rows = jobs::table
        .filter(jobs::change_seq.gt(cursor))
        .order(jobs::change_seq.asc())
        .select((jobs::change_seq, JobSummary::as_select()))
        .load::<(i64, JobSummary)>(&mut *conn)
        .await
        .map_err(db_err)?;
    Ok(rows)
}

/// Up to `limit` changes after `cursor`, oldest first, for the
/// `GET /analyses/changes` feed — cut short at the first change younger than
/// `settle_secs`.
///
/// `change_seq` is taken when a row is written, not when its transaction
/// commits, so a slow transaction can become visible *after* a later number
/// already has. A reader that had moved its cursor past it would never see it.
/// Holding back recent changes, and everything after them, gives in-flight
/// writes time to commit before the cursor can pass them. Returns the rows and
/// whether any were held back.
pub async fn settled_changes(
    pool: &DieselPool,
    cursor: i64,
    limit: i64,
    settle_secs: i64,
) -> AppResult<(Vec<(i64, JobSummary)>, bool)> {
    let mut conn = pool.0.get().await.map_err(db_err)?;
    // `now()` on the database's clock, the one that stamped `updated_at`.
    let settled =
        diesel::dsl::sql::<diesel::sql_types::Bool>("updated_at <= now() - make_interval(secs => ")
            .bind::<diesel::sql_types::BigInt, _>(settle_secs)
            .sql(")");
    let rows: Vec<(i64, JobSummary, bool)> = jobs::table
        .filter(jobs::change_seq.gt(cursor))
        .order(jobs::change_seq.asc())
        .limit(limit)
        .select((jobs::change_seq, JobSummary::as_select(), settled))
        .load(&mut *conn)
        .await
        .map_err(db_err)?;
    let total = rows.len();
    let ready: Vec<(i64, JobSummary)> = rows
        .into_iter()
        .take_while(|(_, _, settled)| *settled)
        .map(|(seq, row, _)| (seq, row))
        .collect();
    let held_back = ready.len() < total;
    Ok((ready, held_back))
}

/// The newest `change_seq` (or 0 if empty) — where the stream starts tailing
/// after emitting its initial snapshot.
pub async fn latest_change_seq(pool: &DieselPool) -> AppResult<i64> {
    let mut conn = pool.0.get().await.map_err(db_err)?;
    let max: Option<i64> = jobs::table
        .select(diesel::dsl::max(jobs::change_seq))
        .first(&mut *conn)
        .await
        .map_err(db_err)?;
    Ok(max.unwrap_or(0))
}
