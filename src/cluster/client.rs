//! Worker-side gRPC client, modelled as an explicit state machine.
//!
//! A worker runs KataGo but has no Postgres access, so it dials the orchestrator
//! and opens one long-lived `Session` stream: it announces its capacity
//! (`Hello{slots}`), then runs each pushed [`JobRequest`](super::proto::JobRequest)
//! through the local engine and streams back a
//! [`JobResult`](super::proto::JobResult).
//!
//! The lifecycle is a [`State`] machine driven by [`WorkerClient::event_loop`]:
//!
//! ```text
//!            ┌──────────────┐  dial ok   ┌──────────┐
//!  start ──▶ │  Connecting  │ ─────────▶ │ Serving  │
//!            └──────────────┘            └──────────┘
//!              ▲   │ dial err               │  stream closed / error
//!     sleep ok │   ▼                        ▼
//!            ┌──────────────┐ ◀────────────────────┘
//!            │   Backoff    │
//!            └──────────────┘
//!  (shutdown in any state ──▶ Stopped ──▶ loop exits)
//! ```

use std::sync::Arc;
use std::time::Duration;

use muxa::prelude::*;
use tokio::sync::{Semaphore, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tonic::Streaming;
use tonic::transport::Channel;

use crate::cluster::proto::cluster_client::ClusterClient;
use crate::cluster::proto::{Hello, JobRequest, JobResult, WorkerMsg, job_result, worker_msg};
use crate::config::WorkerConfig;
use crate::engine::AnalysisEngine;

/// Boxed error unifying the connect (`transport::Error`), Hello-send, and stream
/// (`Status`) failures a dial can hit; all are handled by entering `Backoff`.
type SessionError = Box<dyn std::error::Error + Send + Sync>;

/// Mount the worker client as a background task bound to the app's shutdown token.
pub fn register_client(ctx: &mut BuildCtx, engine: Arc<AnalysisEngine>, cfg: WorkerConfig) {
    ctx.tasks
        .spawn("cluster-grpc-client", move |shutdown| async move {
            WorkerClient::new(engine, cfg, shutdown).event_loop().await;
        });
}

/// The worker client and the resources its states share across reconnects.
struct WorkerClient {
    engine: Arc<AnalysisEngine>,
    cfg: WorkerConfig,
    /// Slots advertised to the orchestrator = max concurrent analyses.
    slots: u32,
    /// Wait between a failed/closed session and the next dial.
    backoff: Duration,
    /// Caps in-flight analyses at `slots` (the orchestrator already respects
    /// this; the semaphore is a local safety net, shared across reconnects).
    limiter: Arc<Semaphore>,
    shutdown: ShutdownToken,
}

/// Lifecycle state. Each variant maps to one handler that runs until the next
/// transition and returns the successor state.
enum State {
    /// Dial the orchestrator and open the session.
    Connecting,
    /// Stream open: service pushed jobs until it ends. Boxed so the open streams
    /// don't bloat the (otherwise unit-sized) other states.
    Serving(Box<Session>),
    /// Wait out the reconnect backoff, then redial.
    Backoff,
    /// Terminal: the event loop exits.
    Stopped,
}

/// An established session — the open streams for one connection.
struct Session {
    /// Held to keep the gRPC channel alive for the session's lifetime.
    #[allow(dead_code, reason = "owns the channel handle while the streams are live")]
    client: ClusterClient<Channel>,
    /// Server→worker: pushed job requests.
    inbound: Streaming<JobRequest>,
    /// Worker→server sink: the Hello (already sent) then one result per job.
    outbound: mpsc::Sender<WorkerMsg>,
}

impl WorkerClient {
    fn new(engine: Arc<AnalysisEngine>, cfg: WorkerConfig, shutdown: ShutdownToken) -> Self {
        let slots = u32::try_from(cfg.concurrency.max(1)).unwrap_or(u32::MAX);
        let backoff = Duration::from_secs(cfg.reconnect_backoff_secs.max(1));
        let limiter = Arc::new(Semaphore::new(slots as usize));
        Self { engine, cfg, slots, backoff, limiter, shutdown }
    }

    /// Drive the state machine until it reaches [`State::Stopped`].
    async fn event_loop(self) {
        tracing::info!(url = %self.cfg.orchestrator_url, slots = self.slots, "worker client starting");
        let mut state = State::Connecting;
        loop {
            state = match state {
                State::Connecting => self.on_connecting().await,
                State::Serving(session) => self.on_serving(session).await,
                State::Backoff => self.on_backoff().await,
                State::Stopped => break,
            };
        }
        tracing::info!("worker client stopped");
    }

    /// `Connecting`: dial (interruptible by shutdown). Success → `Serving`,
    /// failure → `Backoff`.
    async fn on_connecting(&self) -> State {
        tokio::select! {
            () = self.shutdown.cancelled() => State::Stopped,
            result = self.dial() => match result {
                Ok(session) => {
                    tracing::info!("connected to orchestrator");
                    State::Serving(Box::new(session))
                }
                Err(err) => {
                    tracing::warn!(error = %err, "connect failed; backing off");
                    State::Backoff
                }
            },
        }
    }

    /// `Serving`: pull events off the session — a pushed job, the stream ending,
    /// or shutdown — until one moves us out of this state.
    async fn on_serving(&self, mut session: Box<Session>) -> State {
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => return State::Stopped,
                message = session.inbound.message() => match message {
                    Ok(Some(job)) => self.dispatch(job, &session.outbound).await,
                    Ok(None) => {
                        tracing::info!("orchestrator closed the stream; backing off");
                        return State::Backoff;
                    }
                    Err(status) => {
                        tracing::warn!(%status, "session stream error; backing off");
                        return State::Backoff;
                    }
                },
            }
        }
    }

    /// `Backoff`: sleep (interruptible by shutdown), then redial.
    async fn on_backoff(&self) -> State {
        tokio::select! {
            () = self.shutdown.cancelled() => State::Stopped,
            () = tokio::time::sleep(self.backoff) => State::Connecting,
        }
    }

    /// Dial the orchestrator, announce capacity, and open the bidi session.
    async fn dial(&self) -> Result<Session, SessionError> {
        let mut client = ClusterClient::connect(self.cfg.orchestrator_url.clone()).await?;
        // Outbound: Hello first, then one JobResult per finished job.
        let (outbound, out_rx) = mpsc::channel::<WorkerMsg>(self.slots as usize + 1);
        outbound
            .send(WorkerMsg {
                kind: Some(worker_msg::Kind::Hello(Hello { slots: self.slots })),
            })
            .await?;
        let response = client.session(ReceiverStream::new(out_rx)).await?;
        Ok(Session {
            client,
            inbound: response.into_inner(),
            outbound,
        })
    }

    /// Accept one job: take a slot (back-pressuring the inbound read), then run
    /// the analysis off-loop and stream the result back.
    async fn dispatch(&self, job: JobRequest, outbound: &mpsc::Sender<WorkerMsg>) {
        // Wait for a free slot before reading more work off the stream.
        let Ok(permit) = Arc::clone(&self.limiter).acquire_owned().await else {
            return; // semaphore closed — only on shutdown teardown
        };
        let engine = Arc::clone(&self.engine);
        let outbound = outbound.clone();
        tokio::spawn(async move {
            let _permit = permit; // released when the analysis finishes
            let outcome = Self::run_job(&engine, &job.sgf).await;
            let reply = WorkerMsg {
                kind: Some(worker_msg::Kind::Result(JobResult {
                    job_id: job.job_id,
                    outcome: Some(outcome),
                })),
            };
            // Best-effort: if the session is gone, the pgmq lease redelivers.
            let _ = outbound.send(reply).await;
        });
    }

    /// Run one job through the local engine, mapping the result into the wire form.
    async fn run_job(engine: &AnalysisEngine, sgf: &str) -> job_result::Outcome {
        match engine.analyze(sgf).await {
            Ok(analysis) => match serde_json::to_string(&analysis) {
                Ok(json) => job_result::Outcome::AnalysisJson(json),
                Err(err) => job_result::Outcome::Error(format!("serialize analysis: {err}")),
            },
            Err(err) => job_result::Outcome::Error(err.to_string()),
        }
    }
}
