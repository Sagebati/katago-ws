//! Background analysis worker: leases the pgmq queue, runs the analysis, and
//! persists results.
//!
//! The loop is generic over an [`Executor`] — the one coarse step that turns an
//! SGF into a `GameAnalysis` JSON. Everything *around* it (pgmq lease, the
//! visibility-timeout heartbeat, the guarded DB writes, success-only archive,
//! redelivery on failure) is identical whether the analysis runs in-process or
//! on a remote worker, so both share this exact code:
//!
//! - **`standalone` role** — [`LocalExecutor`] runs KataGo in-process (registered
//!   by [`register`] on muxa's `TaskRegistry`, one loop per `concurrency`).
//! - **`orchestrator` role** — [`crate::cluster::server`] runs this same loop
//!   per connected worker with a remote executor that dispatches the SGF over the
//!   cluster WebSocket and awaits the result; the orchestrator keeps holding the pgmq
//!   lease, so a worker crash still triggers redelivery exactly as before.

use std::sync::Arc;
use std::time::Duration;

use muxa::diesel::DieselPool;
use muxa::prelude::*;
use pgmq::Message;
use serde_json::Value as Json;
use uuid::Uuid;

use crate::config::WorkerConfig;
use crate::db;
use crate::engine::AnalysisEngine;
use crate::error::{AppError, AppResult};
use crate::queue::{self, JobMessage};

/// The one coarse unit of work: analyze an SGF and return the annotated result
/// as a JSON [`Json`] value (the serde form of `GameAnalysis`), ready to persist
/// via [`db::set_done`]. `job_id` lets a remote implementation correlate the
/// response to its request.
///
/// `async fn` in trait (no `async-trait`): the loop is generic over `E`, so the
/// future is monomorphized, not boxed.
pub trait Executor: Send + Sync + 'static {
    /// Analyze `sgf` for `job_id`, yielding the result JSON or an error (a failed
    /// analysis is recorded as a non-terminal attempt and left for redelivery).
    fn analyze(
        &self,
        job_id: Uuid,
        sgf: &str,
    ) -> impl std::future::Future<Output = AppResult<Json>> + Send;
}

/// Runs the analysis in-process against a local KataGo engine. Cheap to clone
/// (the engine is `Arc`-backed).
#[derive(Clone)]
pub struct LocalExecutor {
    engine: Arc<AnalysisEngine>,
}

impl LocalExecutor {
    /// Wrap a shared engine.
    pub fn new(engine: Arc<AnalysisEngine>) -> Self {
        Self { engine }
    }
}

impl Executor for LocalExecutor {
    async fn analyze(&self, _job_id: Uuid, sgf: &str) -> AppResult<Json> {
        let analysis = self.engine.analyze(sgf).await?;
        serde_json::to_value(&analysis).map_err(|err| AppError::Internal(err.to_string()))
    }
}

/// Spawn `cfg.concurrency` local worker loops on the build context's task
/// registry (the `standalone` role: web + in-process workers in one process).
pub fn register(ctx: &mut BuildCtx, db: DieselPool, engine: Arc<AnalysisEngine>, cfg: WorkerConfig) {
    let n = cfg.concurrency.max(1);
    for _ in 0..n {
        let db = db.clone();
        let executor = LocalExecutor::new(Arc::clone(&engine));
        let cfg = cfg.clone();
        ctx.tasks
            .spawn("analysis-worker", move |shutdown| async move {
                worker_loop(db, executor, cfg, shutdown).await;
            });
    }
}

/// Lease jobs from the queue and process them with `executor` until `shutdown`.
///
/// Shared verbatim by the local (`standalone`) and remote (`orchestrator`) paths — the
/// only thing that varies is where `executor.analyze` actually runs.
pub async fn worker_loop<E: Executor>(
    db: DieselPool,
    executor: E,
    cfg: WorkerConfig,
    shutdown: ShutdownToken,
) {
    let poll = Duration::from_secs(cfg.poll_secs.max(1));
    tracing::info!(queue = queue::Queue::Analysis.as_str(), "analysis worker started");
    loop {
        tokio::select! {
            () = shutdown.cancelled() => {
                tracing::info!("analysis worker shutting down");
                break;
            }
            res = queue::read_one(&db, queue::Queue::Analysis, cfg.visibility_timeout_secs, poll) => {
                match res {
                    Ok(Some(msg)) => {
                        let msg_id = msg.msg_id;
                        if let Err(err) = handle(&db, &executor, &cfg, msg).await {
                            tracing::error!(error = %err, msg_id, "job processing failed");
                        }
                    }
                    Ok(None) => {} // poll timeout, loop again
                    Err(err) => {
                        tracing::error!(error = %err, "queue read failed");
                        tokio::time::sleep(poll).await;
                    }
                }
            }
        }
    }
}

/// Process one delivered message. The message is **archived only on success**;
/// on failure it's left in the queue so that — once its visibility timeout
/// lapses — pgmq redelivers it for another attempt (until `max_attempts`).
async fn handle<E: Executor>(
    db: &DieselPool,
    executor: &E,
    cfg: &WorkerConfig,
    msg: Message<JobMessage>,
) -> AppResult<()> {
    let job_id = msg.message.job_id;
    let msg_id = msg.msg_id;

    // Poison message: redelivered too many times → give up and remove it.
    if msg.read_ct > cfg.max_attempts {
        tracing::warn!(%job_id, read_ct = msg.read_ct, "max attempts exceeded; failing job");
        db::set_failed(db, job_id, "max delivery attempts exceeded").await?;
        queue::archive(db, queue::Queue::Analysis, msg_id).await?;
        return Ok(());
    }

    db::set_running(db, job_id).await?;

    // The SGF rides the message, so there's no DB round-trip to fetch it — and
    // no orphan case, because the message *is* the job.
    //
    // Hold the message's lock for as long as the analysis runs, so a job that
    // outlasts the visibility timeout isn't redelivered and double-processed.
    // (In `orchestrator` mode the analysis happens on a remote worker, but the
    // lease is still held here — a worker crash drops the result, this records a
    // non-terminal failure, and the job is redelivered, same as a local crash.)
    let heartbeat = VtHeartbeat::start(db.clone(), msg_id, cfg.visibility_timeout_secs);
    let result = executor.analyze(job_id, &msg.message.sgf).await;
    drop(heartbeat); // stop re-arming the lock now that analysis is done

    match result {
        Ok(value) => {
            let num_moves = value
                .get("summary")
                .and_then(|summary| summary.get("num_moves"))
                .and_then(serde_json::Value::as_u64);
            db::set_done(db, job_id, value).await?;
            // Success — and only now — remove the message from the queue.
            queue::archive(db, queue::Queue::Analysis, msg_id).await?;
            tracing::info!(%job_id, num_moves, "analysis complete");
            Ok(())
        }
        Err(err) => {
            // Don't archive: record a *non-terminal* attempt failure and leave
            // the message. Once its visibility timeout lapses pgmq redelivers it
            // for another attempt (until the poison check above gives up and
            // writes the terminal `Failed`). Recording it as a transient attempt
            // — not a terminal failure — keeps a later retry free to succeed.
            tracing::warn!(%job_id, error = %err, "analysis attempt failed; leaving for redelivery");
            db::record_attempt_failure(db, job_id, &err.to_string()).await?;
            // Handled: attempt recorded, message left for redelivery.
            Ok(())
        }
    }
}

/// The visibility-timeout lock is re-armed every `vt / HEARTBEAT_DIVISOR`
/// seconds — comfortably before it lapses. Doubles as the floor on `vt`, so the
/// refresh interval is never below one second.
const HEARTBEAT_DIVISOR: i32 = 2;

/// Keeps an in-flight message's visibility-timeout lock alive while its job
/// runs, re-arming it at half the timeout interval. Dropping the guard stops
/// the heartbeat; if the worker crashes the lock simply lapses and pgmq
/// redelivers the job.
struct VtHeartbeat {
    cancel: ShutdownToken,
}

impl VtHeartbeat {
    fn start(pool: DieselPool, msg_id: i64, vt_secs: i32) -> Self {
        let cancel = ShutdownToken::new();
        let child = cancel.clone();
        // Re-arm well before the current lock lapses (and never faster than 1s).
        let interval =
            Duration::from_secs((vt_secs.max(HEARTBEAT_DIVISOR) as u64) / HEARTBEAT_DIVISOR as u64);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = child.cancelled() => break,
                    () = tokio::time::sleep(interval) => {
                        if let Err(err) =
                            queue::extend_vt(&pool, queue::Queue::Analysis, msg_id, vt_secs).await
                        {
                            tracing::warn!(msg_id, error = %err, "failed to extend visibility timeout");
                        }
                    }
                }
            }
        });
        Self { cancel }
    }
}

impl Drop for VtHeartbeat {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
