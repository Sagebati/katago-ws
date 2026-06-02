//! Orchestrator-side gRPC dispatcher.
//!
//! Implements the [`Cluster`](super::proto::cluster_server::Cluster) service.
//! Each connected worker opens one `Session` stream and announces its capacity
//! (`Hello{slots}`). The orchestrator then drives `slots` copies of
//! [`crate::worker::worker_loop`] for that connection, each leasing one pgmq job
//! and dispatching it to the worker via a [`RemoteExecutor`] — so the queue
//! lease, heartbeat, DB writes and redelivery semantics are exactly the local
//! path's, with the analysis itself shipped over the stream.
//!
//! Backpressure is implicit: a loop won't lease its next job until the current
//! one's result comes back, so a worker never has more than `slots` jobs in
//! flight. A dropped stream cancels that connection's loops and fails their
//! in-flight jobs, which the heartbeat-less pgmq lease then redelivers.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use muxa::diesel::DieselPool;
use muxa::prelude::*;
use serde_json::Value as Json;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Server;
use tonic::{Request, Response, Status, Streaming};
use uuid::Uuid;

use crate::cluster::proto::cluster_server::{Cluster, ClusterServer};
use crate::cluster::proto::{JobRequest, JobResult, WorkerMsg, job_result, worker_msg};
use crate::cluster::registry::{WorkerGuard, WorkerRegistry};
use crate::config::WorkerConfig;
use crate::error::{AppError, AppResult};
use crate::worker::{Executor, worker_loop};

/// Upper bound on the slots a single worker may advertise — a sanity clamp so a
/// misbehaving client can't make us spawn an unbounded number of lease loops.
const MAX_SLOTS: u32 = 64;

/// Channel of pending in-flight jobs for one connection: `job_id` → the sender
/// that delivers that job's outcome back to the waiting [`RemoteExecutor`].
type Pending = Arc<Mutex<HashMap<Uuid, oneshot::Sender<Result<Json, String>>>>>;

/// Mount the gRPC dispatcher as a background task bound to the app's shutdown
/// token. Serves `proto::cluster::Cluster` on `listen` until shutdown.
pub fn register_server(
    ctx: &mut BuildCtx,
    db: DieselPool,
    cfg: WorkerConfig,
    listen: SocketAddr,
    registry: WorkerRegistry,
) {
    // Derive connection lifetimes from the app-wide shutdown token, so a global
    // shutdown both stops `serve` and cancels every live worker connection.
    let shutdown = ctx.shutdown.clone();
    let service = ClusterService { db, cfg, shutdown: shutdown.clone(), registry };

    ctx.tasks.spawn("cluster-grpc-server", move |_task| async move {
        tracing::info!(%listen, "cluster gRPC dispatcher listening");
        let serve_shutdown = shutdown.clone();
        let result = Server::builder()
            .add_service(ClusterServer::new(service))
            .serve_with_shutdown(listen, async move { serve_shutdown.cancelled().await })
            .await;
        if let Err(err) = result {
            tracing::error!(error = %err, "cluster gRPC dispatcher failed");
        } else {
            tracing::info!("cluster gRPC dispatcher stopped");
        }
    });
}

/// The dispatcher service: shares the Diesel pool (queue + result writes) and
/// the consumer config across all worker connections.
struct ClusterService {
    db: DieselPool,
    cfg: WorkerConfig,
    shutdown: ShutdownToken,
    registry: WorkerRegistry,
}

#[tonic::async_trait]
impl Cluster for ClusterService {
    type SessionStream = ReceiverStream<Result<JobRequest, Status>>;

    async fn session(
        &self,
        request: Request<Streaming<WorkerMsg>>,
    ) -> Result<Response<Self::SessionStream>, Status> {
        let peer = request.remote_addr();
        let mut inbound = request.into_inner();

        // First message must be Hello{slots}; it gates how many lease loops run.
        let slots = match inbound.message().await? {
            Some(WorkerMsg { kind: Some(worker_msg::Kind::Hello(hello)) }) => {
                hello.slots.clamp(1, MAX_SLOTS)
            }
            _ => {
                return Err(Status::invalid_argument(
                    "first message must be Hello{slots}",
                ));
            }
        };
        tracing::info!(?peer, slots, "worker connected");

        // Track the connection in the in-memory registry; the returned guard
        // deregisters it when `read_results` returns (stream closed/errored, or
        // the connection token cancelled).
        let guard = self.registry.register(peer, slots).await;

        // Outbound: JobRequests pushed to the worker. Cap matches slots so the
        // channel never holds more than the in-flight jobs.
        let (out_tx, out_rx) = mpsc::channel::<Result<JobRequest, Status>>(slots as usize);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        // This connection's loops live until the stream drops (or global shutdown).
        let conn_token = self.shutdown.child_token();

        // Route inbound JobResults back to the waiting executors; on stream end,
        // cancel the loops and fail any in-flight jobs.
        tokio::spawn(read_results(
            inbound,
            Arc::clone(&pending),
            conn_token.clone(),
            peer,
            guard,
        ));

        // Drive `slots` lease loops, each dispatching over the shared stream.
        let executor = RemoteExecutor { out_tx, pending };
        for _ in 0..slots {
            let db = self.db.clone();
            let cfg = self.cfg.clone();
            let executor = executor.clone();
            let token = conn_token.clone();
            tokio::spawn(async move { worker_loop(db, executor, cfg, token).await });
        }
        // `executor` (and its `out_tx`) drops here; only the per-loop clones
        // remain, so `out_rx` closes once every loop has exited.

        Ok(Response::new(ReceiverStream::new(out_rx)))
    }
}

/// Reads `JobResult`s off the worker's stream and completes the matching pending
/// job. Ends (cancelling `token` and failing any in-flight jobs) when the stream
/// closes or errors.
async fn read_results(
    mut inbound: Streaming<WorkerMsg>,
    pending: Pending,
    token: ShutdownToken,
    peer: Option<SocketAddr>,
    // Held for the connection's lifetime; dropping it here deregisters the worker.
    _registration: WorkerGuard,
) {
    loop {
        tokio::select! {
            () = token.cancelled() => break,
            msg = inbound.message() => match msg {
                Ok(Some(WorkerMsg { kind: Some(worker_msg::Kind::Result(result)) })) => {
                    complete_job(&pending, result).await;
                }
                // A stray Hello or empty frame — ignore and keep reading.
                Ok(Some(_)) => {}
                Ok(None) => break,        // worker closed the stream
                Err(status) => {
                    tracing::warn!(?peer, %status, "worker stream error");
                    break;
                }
            }
        }
    }

    tracing::info!(?peer, "worker disconnected");
    // Stop this connection's lease loops…
    token.cancel();
    // …and fail anything still in flight (dropping the senders makes the waiting
    // `RemoteExecutor::analyze` calls return an error → non-terminal attempt →
    // the pgmq lease lapses and the job is redelivered to another worker).
    pending.lock().await.clear();
}

/// Deliver one job's outcome to its waiting executor (no-op if unknown/stale).
async fn complete_job(pending: &Pending, result: JobResult) {
    let Ok(job_id) = Uuid::parse_str(&result.job_id) else {
        tracing::warn!(job_id = %result.job_id, "worker sent result with unparseable job id");
        return;
    };
    let Some(sender) = pending.lock().await.remove(&job_id) else {
        // No waiter: a duplicate, or the job already failed over to redelivery.
        return;
    };
    let outcome = match result.outcome {
        Some(job_result::Outcome::AnalysisJson(json)) => {
            serde_json::from_str::<Json>(&json).map_err(|err| err.to_string())
        }
        Some(job_result::Outcome::Error(message)) => Err(message),
        None => Err("worker sent an empty job result".to_owned()),
    };
    // The receiver may have gone away (loop cancelled); that's fine.
    drop(sender.send(outcome));
}

/// Executor that ships the analysis to a remote worker over its gRPC stream and
/// awaits the result. Cloned once per lease loop on a connection; all clones
/// share the one outbound channel and the pending-jobs map.
#[derive(Clone)]
struct RemoteExecutor {
    out_tx: mpsc::Sender<Result<JobRequest, Status>>,
    pending: Pending,
}

impl Executor for RemoteExecutor {
    async fn analyze(&self, job_id: Uuid, sgf: &str) -> AppResult<Json> {
        let (tx, rx) = oneshot::channel();
        // Register the waiter before sending, so a fast result can't race us.
        self.pending.lock().await.insert(job_id, tx);

        let request = JobRequest { job_id: job_id.to_string(), sgf: sgf.to_owned() };
        if self.out_tx.send(Ok(request)).await.is_err() {
            self.pending.lock().await.remove(&job_id);
            return Err(AppError::Queue("worker stream closed before dispatch".to_owned()));
        }

        match rx.await {
            Ok(Ok(value)) => Ok(value),
            // Worker ran the analysis but it failed (bad SGF, engine error, …).
            Ok(Err(message)) => Err(AppError::Inference(message)),
            // Sender dropped: the worker disconnected before answering.
            Err(_) => Err(AppError::Queue("worker disconnected before result".to_owned())),
        }
    }
}
