//! Axum handlers for the analysis API.

use std::convert::Infallible;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::response::sse::{Event, KeepAlive, Sse};
use chrono::{DateTime, Utc};
use muxa::diesel::DieselPool;
use tokio_stream::Stream;
use uuid::Uuid;

use crate::cluster::registry::WorkerInfo;
use crate::db;
use crate::db::models::{Job, JobSummary};
use crate::db::status::JobStatus;
use crate::error::{AppError, AppResult};
use crate::http::ApiState;
use crate::http::dto::{
    AnalysisSummary, JobId, ListResponse, StatusQuery, StatusResponse, SubmitResponse,
    WorkersResponse, WorkerSummary,
};

/// How often the live stream polls Postgres for job-state changes.
const STREAM_POLL: Duration = Duration::from_secs(1);

/// Number of queued jobs the dashboard previews (the "top of the queue").
const QUEUE_PREVIEW: i64 = 20;
/// Upper bound on running jobs the dashboard lists. Bounded by total worker slots
/// in practice; the cap just guards the page against a pathological backlog.
const RUNNING_CAP: i64 = 100;

/// `GET /` — a small, dependency-free HTML status dashboard: connected workers,
/// jobs currently running, and the head of the queue, plus links to the docs and
/// key endpoints. Server-rendered plain HTML that auto-refreshes every few seconds
/// via `<meta refresh>` (no JavaScript).
///
/// It reads live, **persisted** job state, so the running/queue figures stay
/// correct under horizontal scaling; the worker list is the orchestrator's
/// in-memory, per-instance registry (absent in the standalone role).
pub async fn index(State(api): State<ApiState>) -> AppResult<Html<String>> {
    // Remote workers: `Some(list)` in the orchestrator role (possibly empty), or
    // `None` in standalone — there, analyses run in-process and nothing registers.
    let workers = match &api.workers {
        Some(registry) => Some(registry.snapshot().await),
        None => None,
    };
    let running = db::jobs_in_state(&api.db, JobStatus::Running, RUNNING_CAP).await?;
    let queued_total = db::count_in_state(&api.db, JobStatus::Queued).await?;
    let queued = db::jobs_in_state(&api.db, JobStatus::Queued, QUEUE_PREVIEW).await?;
    Ok(Html(render_index(
        workers.as_deref(),
        &running,
        queued_total,
        &queued,
    )))
}

/// Render the `/` dashboard HTML from the gathered worker + job state.
fn render_index(
    workers: Option<&[WorkerInfo]>,
    running: &[JobSummary],
    queued_total: i64,
    queued: &[JobSummary],
) -> String {
    use std::fmt::Write as _;
    let mut html = String::with_capacity(4096);
    html.push_str(INDEX_HEAD);
    let _ = write!(
        html,
        "<h1>katago-ws <small>v{}</small></h1>\
         <p class=\"sub\">KataGo-backed SGF analysis service.</p>",
        env!("CARGO_PKG_VERSION"),
    );

    // ── Workers ──────────────────────────────────────────────────────────────
    match workers {
        None => html.push_str(
            "<h2>Workers</h2><p class=\"muted\">Standalone mode — analyses run \
             in-process (no remote workers).</p>",
        ),
        Some(list) => {
            let _ = write!(html, "<h2>Workers <span class=\"count\">{}</span></h2>", list.len());
            if list.is_empty() {
                html.push_str("<p class=\"muted\">No workers connected.</p>");
            } else {
                html.push_str(
                    "<table><tr><th>Worker</th><th>Session</th><th>Slots</th>\
                     <th>Uptime</th></tr>",
                );
                for worker in list {
                    let _ = write!(
                        html,
                        "<tr><td class=\"mono\">{}</td>\
                         <td class=\"mono muted\">#{}</td><td>{}</td>\
                         <td class=\"muted\">{}</td></tr>",
                        worker.name,
                        worker.id,
                        worker.slots,
                        ago(worker.connected_at),
                    );
                }
                html.push_str("</table>");
            }
        }
    }

    // ── Running ──────────────────────────────────────────────────────────────
    let _ = write!(
        html,
        "<h2>Running <span class=\"count\">{}</span></h2>",
        running.len(),
    );
    if running.is_empty() {
        html.push_str("<p class=\"muted\">Nothing running.</p>");
    } else {
        html.push_str("<table><tr><th>Job</th><th>Running for</th></tr>");
        for job in running {
            let _ = write!(
                html,
                "<tr><td class=\"mono\"><span class=\"dot run\"></span>{}</td>\
                 <td class=\"muted\">{}</td></tr>",
                job.id,
                ago(job.updated_at),
            );
        }
        html.push_str("</table>");
    }

    // ── Queue ────────────────────────────────────────────────────────────────
    if queued_total > QUEUE_PREVIEW {
        let _ = write!(
            html,
            "<h2>Queue <span class=\"count\">top {} of {queued_total}</span></h2>",
            queued.len(),
        );
    } else {
        let _ = write!(html, "<h2>Queue <span class=\"count\">{queued_total}</span></h2>");
    }
    if queued.is_empty() {
        html.push_str("<p class=\"muted\">Queue is empty.</p>");
    } else {
        html.push_str("<table><tr><th>#</th><th>Job</th><th>Waiting</th></tr>");
        for (idx, job) in queued.iter().enumerate() {
            let _ = write!(
                html,
                "<tr><td class=\"muted\">{}</td>\
                 <td class=\"mono\"><span class=\"dot q\"></span>{}</td>\
                 <td class=\"muted\">{}</td></tr>",
                idx + 1,
                job.id,
                ago(job.created_at),
            );
        }
        html.push_str("</table>");
    }

    html.push_str(INDEX_FOOT);
    html
}

/// Compact "time since" label for a timestamp (`45s`, `3m 12s`, `2h 5m`, `4d 1h`).
fn ago(since: DateTime<Utc>) -> String {
    let secs = (Utc::now() - since).num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else if secs < 86_400 {
        format!("{}h {}m", secs / 3_600, (secs % 3_600) / 60)
    } else {
        format!("{}d {}h", secs / 86_400, (secs % 86_400) / 3_600)
    }
}

/// Document head + styles for the `/` dashboard (everything before the body).
const INDEX_HEAD: &str = concat!(
    "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">",
    "<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">",
    "<meta http-equiv=\"refresh\" content=\"5\">",
    "<title>katago-ws</title><style>",
    ":root{color-scheme:light dark}",
    "body{font:15px/1.6 system-ui,sans-serif;max-width:52rem;margin:3rem auto;padding:0 1.25rem}",
    "h1{margin:0 0 .25rem;font-size:1.6rem}small{font-weight:400;color:#888}",
    ".sub{color:#888;margin:0 0 1.5rem}",
    "h2{font-size:1.05rem;margin:1.8rem 0 .5rem;",
    "border-bottom:1px solid rgba(127,127,127,.25);padding-bottom:.3rem}",
    ".count{font-weight:400;color:#888;font-size:.85rem}",
    "table{width:100%;border-collapse:collapse;font-size:.86rem}",
    "th{text-align:left;color:#888;font-weight:600;padding:.3rem .6rem .3rem 0}",
    "td{padding:.28rem .6rem .28rem 0;border-top:1px solid rgba(127,127,127,.14)}",
    ".mono{font-family:ui-monospace,SFMono-Regular,Menlo,monospace}",
    ".muted{color:#888}",
    ".dot{display:inline-block;width:.5rem;height:.5rem;border-radius:50%;",
    "margin-right:.45rem;vertical-align:middle}",
    ".run{background:#0a7}.q{background:#d49a3a}",
    ".links{margin:2rem 0 1rem;font-size:.9rem}",
    ".links a{margin-right:.9rem;text-decoration:none}",
    "pre{background:rgba(127,127,127,.12);padding:.8rem 1rem;border-radius:.5rem;",
    "overflow-x:auto;font-size:.85rem}",
    "</style></head><body>",
);

/// Footer for the `/` dashboard: endpoint links + the submit example.
const INDEX_FOOT: &str = concat!(
    "<section class=\"links\">",
    "<a href=\"/docs\">/docs</a><a href=\"/openapi.json\">/openapi.json</a>",
    "<a href=\"/analyses\">/analyses</a><a href=\"/analyses/stream\">/analyses/stream</a>",
    "<a href=\"/health\">/health</a></section>",
    "<p class=\"muted\">Submit a game (raw SGF in the request body):</p>",
    "<pre>curl -X POST --data-binary @game.sgf https://&lt;host&gt;/analyse</pre>",
    "</body></html>",
);

/// `POST /analyse` — body is the raw SGF. Validates it parses, persists a
/// queued job, enqueues it, and returns `202` with the job id.
pub async fn submit(
    State(api): State<ApiState>,
    body: String,
) -> AppResult<(StatusCode, Json<SubmitResponse>)> {
    // Cheap structural validation; full replay happens in the worker.
    sgf_parser::parse(&body).map_err(|err| AppError::Sgf(err.to_string()))?;

    // The database mints the (time-ordered v7) id and enqueues the work in one
    // transaction; we just return the id it hands back. `body` is owned and unused
    // after this, so it moves straight into the queue message — no extra copy of
    // the (potentially large) SGF.
    let job_id = db::submit(&api.db, body).await?;

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

/// `GET /workers` — the workers currently holding a live WebSocket session to this
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

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;

    fn job(status: JobStatus) -> JobSummary {
        let now = Utc::now();
        JobSummary { id: Uuid::nil(), status, error: None, created_at: now, updated_at: now }
    }

    #[test]
    fn dashboard_standalone_omits_worker_table() {
        let html = render_index(None, &[], 0, &[]);
        assert!(html.contains("Standalone mode"));
        assert!(!html.contains("<th>Worker</th>"));
        assert!(html.contains("Nothing running."));
        assert!(html.contains("Queue is empty."));
        // Always links to the docs + carries the submit example.
        assert!(html.contains("href=\"/docs\""));
        assert!(html.contains("--data-binary"));
    }

    #[test]
    fn dashboard_renders_workers_running_and_truncated_queue() {
        let peer: SocketAddr = "10.0.0.7:50713".parse().unwrap();
        let workers =
            vec![WorkerInfo { id: 3, name: "rig-7".to_owned(), peer: Some(peer), slots: 4, connected_at: Utc::now() }];
        let running = vec![job(JobStatus::Running)];
        let queued = vec![job(JobStatus::Queued), job(JobStatus::Queued)];

        let html = render_index(Some(&workers), &running, 25, &queued);

        // Worker count, its registered name, advertised slots, and session id.
        assert!(html.contains("<h2>Workers <span class=\"count\">1</span>"));
        assert!(html.contains("rig-7"));
        assert!(html.contains("#3"));
        assert!(html.contains("<td>4</td>"));
        // Running section.
        assert!(html.contains("<h2>Running <span class=\"count\">1</span>"));
        assert!(html.contains("dot run"));
        // Queue is shown as a truncated preview: 2 listed of 25 total.
        assert!(html.contains("top 2 of 25"));
        assert!(html.contains("dot q"));
    }

    #[test]
    fn dashboard_shows_worker_name() {
        let workers =
            vec![WorkerInfo { id: 7, name: "brave-otter-42".to_owned(), peer: None, slots: 1, connected_at: Utc::now() }];
        let html = render_index(Some(&workers), &[], 0, &[]);
        assert!(html.contains("brave-otter-42"));
    }

    #[test]
    fn dashboard_shows_empty_worker_set() {
        let html = render_index(Some(&[]), &[], 0, &[]);
        assert!(html.contains("No workers connected."));
        assert!(html.contains("<h2>Queue <span class=\"count\">0</span>"));
    }
}
