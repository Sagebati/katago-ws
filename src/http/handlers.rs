//! Axum handlers for the analysis API.

use std::convert::Infallible;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use muxa::diesel::DieselPool;
use tokio_stream::Stream;
use uuid::Uuid;

use crate::db;
use crate::db::models::Job;
use crate::db::status::JobStatus;
use crate::error::{AppError, AppResult};
use crate::http::ApiState;
use crate::http::dto::{
    AnalysisSummary, JobId, ListResponse, StatusQuery, StatusResponse, SubmitResponse,
    WorkersResponse, WorkerSummary,
};

/// How often the live stream polls Postgres for job-state changes.
const STREAM_POLL: Duration = Duration::from_secs(1);

/// `POST /analyse` — body is the raw SGF. Validates it parses, persists a
/// queued job, enqueues it, and returns `202` with the job id.
pub async fn submit(
    State(api): State<ApiState>,
    body: String,
) -> AppResult<(StatusCode, Json<SubmitResponse>)> {
    // Cheap structural validation; full replay happens in the worker.
    sgf_parser::parse(&body).map_err(|err| AppError::Sgf(err.to_string()))?;

    // The database mints the (time-ordered v7) id and enqueues the work in one
    // transaction; we just return the id it hands back.
    let job_id = db::submit(&api.db, &body).await?;

    Ok((
        StatusCode::ACCEPTED,
        Json(SubmitResponse {
            job_id,
            status: JobStatus::Queued,
        }),
    ))
}

/// How often `?wait=` re-checks the job's state.
const WAIT_POLL: Duration = Duration::from_secs(1);
/// Upper bound on how long `?wait=` holds a request open (clamps the client's value).
const MAX_WAIT: Duration = Duration::from_secs(300);

/// `GET /analyse/{id}` — returns the job status and, when done, its result.
///
/// With `?wait=<secs>` it long-polls: the request blocks until the job reaches a
/// terminal state (`done`/`failed`) or the (server-capped) budget elapses, then
/// returns the current state. Without it, responds immediately.
pub async fn status(
    State(api): State<ApiState>,
    Path(JobId(id)): Path<JobId>,
    Query(query): Query<StatusQuery>,
) -> AppResult<Json<StatusResponse>> {
    let job = match query.wait {
        Some(secs) if secs > 0 => {
            wait_for_job(&api.db, id, Duration::from_secs(secs).min(MAX_WAIT)).await?
        }
        _ => db::get_job(&api.db, id).await?.ok_or(AppError::NotFound)?,
    };
    Ok(Json(StatusResponse {
        id: job.id,
        status: job.status,
        result: job.result,
        error: job.error,
    }))
}

/// Poll the job until it reaches a terminal state or `budget` elapses, then
/// return whatever state it's in. Polling **persisted** state keeps this correct
/// across instances — any worker's progress is visible. A missing job is a 404
/// immediately; we don't wait on one that doesn't exist.
async fn wait_for_job(db: &DieselPool, id: Uuid, budget: Duration) -> AppResult<Job> {
    let deadline = Instant::now() + budget;
    loop {
        let job = db::get_job(db, id).await?.ok_or(AppError::NotFound)?;
        let terminal = matches!(job.status, JobStatus::Done | JobStatus::Failed);
        let remaining = deadline.saturating_duration_since(Instant::now());
        if terminal || remaining.is_zero() {
            return Ok(job);
        }
        tokio::time::sleep(WAIT_POLL.min(remaining)).await;
    }
}

/// `GET /analyses` — lifecycle summary of every job, newest first.
pub async fn list(State(api): State<ApiState>) -> AppResult<Json<ListResponse>> {
    let analyses = db::list_jobs(&api.db)
        .await?
        .into_iter()
        .map(AnalysisSummary::from)
        .collect();
    Ok(Json(ListResponse { analyses }))
}

/// `GET /workers` — the workers currently holding a live gRPC session to this
/// orchestrator, oldest connection first.
///
/// This reads the orchestrator's **in-memory** registry, so it's an
/// instance-local view: each orchestrator replica lists only the workers
/// connected to it. In the `standalone` role there are no remote workers, so the
/// list is always empty.
pub async fn workers(State(api): State<ApiState>) -> Json<WorkersResponse> {
    let workers = match &api.workers {
        Some(registry) => registry
            .snapshot()
            .await
            .into_iter()
            .map(WorkerSummary::from)
            .collect(),
        None => Vec::new(),
    };
    Json(WorkersResponse { workers })
}

/// `GET /analyses/stream` — Server-Sent Events feed of every job lifecycle
/// transition across all jobs, in real time.
///
/// Driven entirely by **persisted** state, so it reflects work done by *any*
/// worker on *any* instance — correct under horizontal scaling, and a
/// reconnecting client resumes from the live DB, not lost in-memory state.
///
/// It first emits a **snapshot** of every job, then **tails** the `jobs` table
/// using a monotonic `change_seq` cursor (exact — no equal-timestamp races): each
/// second it fetches rows changed since the cursor and emits them. The changed
/// row *is* the new state, so there's no dedup-and-refetch. Each message has
/// `event: analysis` and an [`AnalysisSummary`] JSON `data` payload.
pub async fn stream(
    State(api): State<ApiState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let db = api.db.clone();
    let body = async_stream::stream! {
        // Snapshot: every job currently known.
        match db::list_jobs(&db).await {
            Ok(rows) => {
                for row in rows {
                    yield analysis_event(&AnalysisSummary::from(row));
                }
            }
            Err(err) => yield error_event(&err),
        }
        // Tail the table from the newest change at snapshot time.
        let mut cursor = db::latest_change_seq(&db).await.unwrap_or(0);
        loop {
            tokio::time::sleep(STREAM_POLL).await;
            match db::changes_since(&db, cursor).await {
                Ok(rows) => {
                    for (seq, row) in rows {
                        cursor = cursor.max(seq);
                        yield analysis_event(&AnalysisSummary::from(row));
                    }
                }
                Err(err) => yield error_event(&err),
            }
        }
    };
    Sse::new(body).keep_alive(KeepAlive::default())
}

/// Build an `analysis` SSE message (falling back to an `error` event if the
/// summary somehow fails to serialize).
fn analysis_event(summary: &AnalysisSummary) -> Result<Event, Infallible> {
    let event = Event::default()
        .event("analysis")
        .json_data(summary)
        .unwrap_or_else(|err| Event::default().event("error").data(err.to_string()));
    Ok(event)
}

/// Build an `error` SSE message carrying the error text.
fn error_event(err: &AppError) -> Result<Event, Infallible> {
    Ok(Event::default().event("error").data(err.to_string()))
}
