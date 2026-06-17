//! Orchestrator-side dispatcher — an axum WebSocket handler.
//!
//! Mounted on the orchestrator's own web server at `GET /cluster` (so it shares
//! the HTTP port and a TLS terminator like Cloudflare can front it). Each worker
//! opens one socket and announces its capacity ([`ClientMsg::Hello`]); the
//! orchestrator then drives `slots` copies of [`crate::worker::worker_loop`] for
//! that connection, each leasing one pgmq job and dispatching it to the worker via
//! a [`RemoteExecutor`] — so the queue lease, heartbeat, DB writes and redelivery
//! are exactly the local path's, with the analysis itself shipped over the socket.
//!
//! Backpressure is implicit: a loop won't lease its next job until the current
//! one's result returns, so a worker never has more than `slots` jobs in flight. A
//! dropped socket cancels that connection's loops and fails their in-flight jobs,
//! which the heartbeat-less pgmq lease then redelivers.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use futures_util::stream::SplitStream;
use futures_util::{SinkExt as _, StreamExt as _};
use muxa::diesel::DieselPool;
use muxa::prelude::*;
use secrecy::{ExposeSecret as _, SecretString};
use serde_json::Value as Json;
use tokio::sync::{Mutex, mpsc, oneshot};
use uuid::Uuid;

use crate::cluster::message::{ClientMsg, JobRequest, Outcome};
use crate::cluster::registry::WorkerRegistry;
use crate::config::WorkerConfig;
use crate::error::{AppError, AppResult};
use crate::http::ApiState;
use crate::worker::{Executor, worker_loop};

/// Upper bound on the slots one worker may advertise — a sanity clamp so a
/// misbehaving client can't make us spawn an unbounded number of lease loops.
const MAX_SLOTS: u32 = 64;

/// Per-connection map of in-flight jobs: `job_id` → the sender that delivers that
/// job's outcome back to the waiting [`RemoteExecutor`].
type Pending = Arc<Mutex<HashMap<Uuid, oneshot::Sender<Result<Json, String>>>>>;

/// Orchestrator-only state the cluster socket handler needs, held behind an `Arc`
/// in [`ApiState`] so the (cloneable) state can carry a non-`Clone` secret.
pub struct ClusterDispatcher {
    /// Consumer config (concurrency/visibility/poll/max_attempts) for the loops.
    cfg: WorkerConfig,
    /// Secret a worker must present (Bearer). Empty ⇒ auth disabled.
    token: SecretString,
    /// App-wide shutdown; each connection derives a child token from it.
    shutdown: ShutdownToken,
}

impl ClusterDispatcher {
    /// Build the dispatcher state for the `orchestrator` role.
    pub fn new(cfg: WorkerConfig, token: SecretString, shutdown: ShutdownToken) -> Self {
        Self { cfg, token, shutdown }
    }

    /// Whether the request carries the expected `Authorization: Bearer <token>`
    /// (constant-time compared). When no token is configured, auth is disabled.
    fn authorized(&self, headers: &HeaderMap) -> bool {
        let expected = self.token.expose_secret();
        if expected.is_empty() {
            return true;
        }
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|presented| tokens_match(presented, expected))
    }
}

/// Length-checked constant-time token comparison (avoids leaking match length via
/// early return on the byte loop).
fn tokens_match(presented: &str, expected: &str) -> bool {
    let (presented, expected) = (presented.as_bytes(), expected.as_bytes());
    if presented.len() != expected.len() {
        return false;
    }
    let mut diff = 0u8;
    for (left, right) in presented.iter().zip(expected) {
        diff |= left ^ right;
    }
    diff == 0
}

/// `GET /cluster` — upgrade a worker's connection to the cluster WebSocket.
///
/// Only mounted in the `orchestrator` role (where `ApiState.cluster` is set).
/// Rejects the upgrade with 401 if the Bearer token is missing/wrong.
pub async fn cluster_ws(State(api): State<ApiState>, headers: HeaderMap, upgrade: WebSocketUpgrade) -> Response {
    let (Some(dispatcher), Some(registry)) = (api.cluster.as_ref(), api.workers.as_ref()) else {
        return (StatusCode::NOT_FOUND, "cluster socket not enabled").into_response();
    };
    if !dispatcher.authorized(&headers) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid cluster token").into_response();
    }
    let db = api.db.clone();
    let registry = registry.clone();
    let cfg = dispatcher.cfg.clone();
    let shutdown = dispatcher.shutdown.clone();
    upgrade.on_upgrade(move |socket| run_session(socket, db, registry, cfg, shutdown))
}

/// Drive one worker connection: read its `Hello`, run `slots` lease loops that
/// push jobs over the socket, and route results back — until the socket closes or
/// the app shuts down.
async fn run_session(
    socket: WebSocket,
    db: DieselPool,
    registry: WorkerRegistry,
    cfg: WorkerConfig,
    shutdown: ShutdownToken,
) {
    let (mut sink, mut stream) = socket.split();

    // First frame must be Hello; it carries the worker's name + slot count.
    let Some((slots, name)) = read_hello(&mut stream).await else {
        tracing::warn!("worker socket closed before a valid Hello");
        return;
    };
    let slots = slots.clamp(1, MAX_SLOTS);
    tracing::info!(%name, slots, "worker connected (ws)");

    // Registry entry (peer is unknown behind a proxy); the guard deregisters on drop.
    let guard = registry.register(name, None, slots).await;

    // Outbound JobRequests; cap matches slots so it never holds more than in-flight.
    let (out_tx, mut out_rx) = mpsc::channel::<JobRequest>(slots as usize);
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let conn_token = shutdown.child_token();

    // Drive `slots` lease loops, each dispatching over the shared socket.
    let executor = RemoteExecutor { out_tx, pending: Arc::clone(&pending) };
    for _ in 0..slots {
        let db = db.clone();
        let cfg = cfg.clone();
        let executor = executor.clone();
        let token = conn_token.clone();
        tokio::spawn(async move { worker_loop(db, executor, cfg, token).await });
    }
    drop(executor); // only the per-loop clones keep out_tx alive now

    // Single I/O loop over the split halves: pull jobs to send, route results back,
    // answer pings (keepalive through idle WS proxies).
    loop {
        tokio::select! {
            () = conn_token.cancelled() => break,
            job = out_rx.recv() => match job {
                Some(job) => {
                    let Ok(text) = serde_json::to_string(&job) else { continue };
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                None => break, // every lease loop exited (shutdown)
            },
            inbound = stream.next() => match inbound {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<ClientMsg>(&text) {
                    Ok(ClientMsg::Result { job_id, outcome }) => complete_job(&pending, job_id, outcome).await,
                    Ok(ClientMsg::Hello { .. }) => {} // stray Hello — ignore
                    Err(err) => tracing::warn!(%err, "worker sent an unparseable frame"),
                },
                Some(Ok(Message::Ping(payload))) => drop(sink.send(Message::Pong(payload)).await),
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {} // pong / binary — ignore
                Some(Err(err)) => {
                    tracing::warn!(%err, "worker socket error");
                    break;
                }
            },
        }
    }

    // Teardown: stop this connection's lease loops and fail anything still in
    // flight (dropping the senders makes the waiting `analyze` calls error → a
    // non-terminal attempt → the pgmq lease lapses → redelivery to another worker).
    conn_token.cancel();
    pending.lock().await.clear();
    drop(guard);
    tracing::info!("worker disconnected (ws)");
}

/// Read frames until the worker's opening `Hello{slots}`; `None` if the socket
/// ends/errors or the first text frame isn't a Hello.
async fn read_hello(stream: &mut SplitStream<WebSocket>) -> Option<(u32, String)> {
    while let Some(message) = stream.next().await {
        match message {
            Ok(Message::Text(text)) => {
                return match serde_json::from_str::<ClientMsg>(&text) {
                    Ok(ClientMsg::Hello { slots, name }) => Some((slots, name)),
                    _ => None,
                };
            }
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {} // ping/pong/binary before Hello — keep waiting
        }
    }
    None
}

/// Deliver one job's outcome to its waiting executor (no-op if unknown/stale).
async fn complete_job(pending: &Pending, job_id: Uuid, outcome: Outcome) {
    let Some(sender) = pending.lock().await.remove(&job_id) else {
        return; // no waiter: a duplicate, or already failed over to redelivery
    };
    let result = match outcome {
        Outcome::Ok(value) => Ok(value),
        Outcome::Err(message) => Err(message),
    };
    drop(sender.send(result)); // receiver may be gone (loop cancelled) — fine
}

/// Executor that ships the analysis to a remote worker over its socket and awaits
/// the result. Cloned once per lease loop; all clones share the one outbound
/// channel and the pending-jobs map.
#[derive(Clone)]
struct RemoteExecutor {
    out_tx: mpsc::Sender<JobRequest>,
    pending: Pending,
}

impl Executor for RemoteExecutor {
    async fn analyze(&self, job_id: Uuid, sgf: &str) -> AppResult<Json> {
        let (tx, rx) = oneshot::channel();
        // Register the waiter before sending, so a fast result can't race us.
        self.pending.lock().await.insert(job_id, tx);

        let request = JobRequest { job_id, sgf: sgf.to_owned() };
        if self.out_tx.send(request).await.is_err() {
            self.pending.lock().await.remove(&job_id);
            return Err(AppError::Queue("worker socket closed before dispatch".to_owned()));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatcher(token: &str) -> ClusterDispatcher {
        ClusterDispatcher::new(
            WorkerConfig::default(),
            SecretString::from(token.to_owned()),
            ShutdownToken::new(),
        )
    }

    fn bearer(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
        headers
    }

    #[test]
    fn constant_time_compare_matches_only_identical() {
        assert!(tokens_match("s3cret", "s3cret"));
        assert!(!tokens_match("s3cret", "s3crew"));
        assert!(!tokens_match("s3cret", "s3cre")); // length differs
    }

    #[test]
    fn auth_disabled_when_token_empty() {
        let disp = dispatcher("");
        assert!(disp.authorized(&HeaderMap::new()));
        assert!(disp.authorized(&bearer("anything")));
    }

    #[test]
    fn auth_requires_matching_bearer() {
        let disp = dispatcher("topsecret");
        assert!(disp.authorized(&bearer("topsecret")));
        assert!(!disp.authorized(&bearer("wrong")));
        assert!(!disp.authorized(&HeaderMap::new()));
    }

    #[test]
    fn messages_round_trip_as_json() {
        let hello = serde_json::to_string(&ClientMsg::Hello { slots: 4, name: "rig".to_owned() }).unwrap();
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(&hello).unwrap(),
            ClientMsg::Hello { slots: 4, .. }
        ));

        let job = JobRequest { job_id: Uuid::nil(), sgf: "(;GM[1])".to_owned() };
        let wire = serde_json::to_string(&job).unwrap();
        assert_eq!(serde_json::from_str::<JobRequest>(&wire).unwrap().sgf, "(;GM[1])");

        let result = serde_json::to_string(&ClientMsg::Result {
            job_id: Uuid::nil(),
            outcome: Outcome::Err("boom".to_owned()),
        })
        .unwrap();
        assert!(matches!(
            serde_json::from_str::<ClientMsg>(&result).unwrap(),
            ClientMsg::Result { outcome: Outcome::Err(message), .. } if message == "boom"
        ));
    }
}
