//! Worker-side cluster client, modelled as an explicit state machine.
//!
//! A worker runs KataGo but has no Postgres access, so it dials the orchestrator's
//! WebSocket and opens one long-lived session: it announces its capacity
//! ([`ClientMsg::Hello`]), then runs each pushed [`JobRequest`] through the local
//! engine and sends back a [`ClientMsg::Result`].
//!
//! The lifecycle is a [`State`] machine driven by [`WorkerClient::event_loop`]:
//!
//! ```text
//!            ┌──────────────┐  dial ok   ┌──────────┐
//!  start ──▶ │  Connecting  │ ─────────▶ │ Serving  │
//!            └──────────────┘            └──────────┘
//!              ▲   │ dial err               │  socket closed / error
//!     sleep ok │   ▼                        ▼
//!            ┌──────────────┐ ◀────────────────────┘
//!            │   Backoff    │
//!            └──────────────┘
//!  (shutdown in any state ──▶ Stopped ──▶ loop exits)
//! ```

use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt as _, StreamExt as _};
use muxa::prelude::*;
use secrecy::ExposeSecret as _;
use tokio::net::TcpStream;
use tokio::sync::{Semaphore, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::cluster::message::{ClientMsg, JobRequest, Outcome};
use crate::config::WorkerConfig;
use crate::engine::AnalysisEngine;

/// The connected WebSocket transport (plaintext `ws://` or TLS `wss://`).
type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Boxed error unifying the connect, handshake, header, and send failures a dial
/// can hit; all are handled by entering `Backoff`.
type SessionError = Box<dyn std::error::Error + Send + Sync>;

/// Mount the worker client as a background task bound to the app's shutdown token.
pub fn register_client(ctx: &mut BuildCtx, engine: Arc<AnalysisEngine>, cfg: WorkerConfig) {
    // tokio-tungstenite's rustls connector needs a process-default crypto provider
    // for `wss://`. The graph carries a single provider (aws-lc-rs); install it
    // explicitly rather than relying on rustls's implicit default (idempotent —
    // ignore the "already installed" error).
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    ctx.tasks
        .spawn("cluster-ws-client", move |shutdown| async move {
            WorkerClient::new(engine, cfg, shutdown).event_loop().await;
        });
}

/// A friendly worker name like `brave-otter-42`, used when `[worker].name` is
/// unset — so a freshly-deployed worker still shows up legibly in `/workers`.
fn generate_name() -> String {
    const ADJECTIVES: [&str; 16] = [
        "brave", "calm", "clever", "eager", "fuzzy", "gentle", "jolly", "keen",
        "lucky", "mighty", "nimble", "proud", "quick", "sly", "witty", "zesty",
    ];
    const ANIMALS: [&str; 16] = [
        "otter", "fox", "panda", "heron", "lynx", "koi", "wren", "ibex",
        "gecko", "raven", "mole", "tapir", "yak", "quokka", "civet", "shrew",
    ];
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let adjective = ADJECTIVES[usize::from(bytes[0]) % ADJECTIVES.len()];
    let animal = ANIMALS[usize::from(bytes[1]) % ANIMALS.len()];
    format!("{adjective}-{animal}-{:02}", u16::from(bytes[2]) % 100)
}

/// The worker client and the resources its states share across reconnects.
struct WorkerClient {
    engine: Arc<AnalysisEngine>,
    cfg: WorkerConfig,
    /// Name announced to the orchestrator (from `[worker].name` or generated).
    name: String,
    /// Slots advertised to the orchestrator = max concurrent analyses.
    slots: u32,
    /// Wait between a failed/closed session and the next dial.
    backoff: Duration,
    /// Caps in-flight analyses at `slots` (a local safety net shared across
    /// reconnects; the orchestrator already respects the advertised slots).
    limiter: Arc<Semaphore>,
    shutdown: ShutdownToken,
}

/// Lifecycle state. Each variant maps to one handler that runs until the next
/// transition and returns the successor state.
enum State {
    /// Dial the orchestrator and open the session.
    Connecting,
    /// Session open: service pushed jobs until it ends. Boxed so the open streams
    /// don't bloat the (otherwise unit-sized) other states.
    Serving(Box<Session>),
    /// Wait out the reconnect backoff, then redial.
    Backoff,
    /// Terminal: the event loop exits.
    Stopped,
}

/// An established session — the split socket plus the outbound result channel.
struct Session {
    /// Worker→orchestrator sink.
    sink: SplitSink<WsStream, Message>,
    /// Orchestrator→worker stream of pushed jobs.
    stream: SplitStream<WsStream>,
    /// Finished analyses post their [`ClientMsg::Result`] here; the serving loop
    /// drains it to the sink.
    out_tx: mpsc::Sender<ClientMsg>,
    /// Receiver half drained by the serving loop.
    out_rx: mpsc::Receiver<ClientMsg>,
}

impl WorkerClient {
    fn new(engine: Arc<AnalysisEngine>, cfg: WorkerConfig, shutdown: ShutdownToken) -> Self {
        let slots = u32::try_from(cfg.concurrency.max(1)).unwrap_or(u32::MAX);
        let backoff = Duration::from_secs(cfg.reconnect_backoff_secs.max(1));
        let limiter = Arc::new(Semaphore::new(slots as usize));
        let name = if cfg.name.is_empty() { generate_name() } else { cfg.name.clone() };
        Self { engine, cfg, name, slots, backoff, limiter, shutdown }
    }

    /// Drive the state machine until it reaches [`State::Stopped`].
    async fn event_loop(self) {
        tracing::info!(name = %self.name, url = %self.cfg.orchestrator_url, slots = self.slots, "worker client starting");
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
                    tracing::info!(slots = self.slots, "connected to orchestrator; waiting for jobs");
                    State::Serving(Box::new(session))
                }
                Err(err) => {
                    tracing::warn!(error = %err, "connect failed; backing off");
                    State::Backoff
                }
            },
        }
    }

    /// `Serving`: drive the single I/O loop — send finished results, receive pushed
    /// jobs, answer pings — until the socket ends or shutdown.
    async fn on_serving(&self, session: Box<Session>) -> State {
        let Session { mut sink, mut stream, out_tx, mut out_rx } = *session;
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => return State::Stopped,
                outbound = out_rx.recv() => {
                    if let Some(message) = outbound {
                        let Ok(text) = serde_json::to_string(&message) else { continue };
                        if sink.send(Message::Text(text.into())).await.is_err() {
                            return State::Backoff;
                        }
                    }
                }
                inbound = stream.next() => match inbound {
                    Some(Ok(Message::Text(text))) => match serde_json::from_str::<JobRequest>(&text) {
                        Ok(job) => self.dispatch(job, &out_tx).await,
                        Err(err) => tracing::warn!(%err, "orchestrator sent an unparseable job"),
                    },
                    Some(Ok(Message::Ping(payload))) => drop(sink.send(Message::Pong(payload)).await),
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::info!("orchestrator closed the socket; backing off");
                        return State::Backoff;
                    }
                    Some(Ok(_)) => {} // pong / binary — ignore
                    Some(Err(err)) => {
                        tracing::warn!(%err, "session socket error; backing off");
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

    /// Dial the orchestrator (with the auth header), announce capacity, and open
    /// the session.
    async fn dial(&self) -> Result<Session, SessionError> {
        let mut request = self.cfg.orchestrator_url.as_str().into_client_request()?;
        let token = self.cfg.auth_token.expose_secret();
        if !token.is_empty() {
            let value = format!("Bearer {token}").parse::<http::HeaderValue>()?;
            request.headers_mut().insert(http::header::AUTHORIZATION, value);
        }

        let (socket, _response) = connect_async(request).await?;
        let (mut sink, stream) = socket.split();

        // Hello first, before any job can arrive.
        let hello = serde_json::to_string(&ClientMsg::Hello { slots: self.slots, name: self.name.clone() })?;
        sink.send(Message::Text(hello.into())).await?;

        let (out_tx, out_rx) = mpsc::channel::<ClientMsg>(self.slots as usize + 1);
        Ok(Session { sink, stream, out_tx, out_rx })
    }

    /// Accept one job: take a slot (back-pressuring the inbound read), then run the
    /// analysis off-loop and post the result to the outbound channel.
    async fn dispatch(&self, job: JobRequest, out_tx: &mpsc::Sender<ClientMsg>) {
        let job_id = job.job_id;
        // If every slot is busy this parks until one frees — log it so a lull where
        // a job arrived but hasn't started is visible rather than silent.
        if self.limiter.available_permits() == 0 {
            tracing::info!(%job_id, slots = self.slots, "all slots busy; waiting for one to free");
        }
        let Ok(permit) = Arc::clone(&self.limiter).acquire_owned().await else {
            return; // semaphore closed — only on shutdown teardown
        };
        let in_use = self.slots - self.limiter.available_permits() as u32;
        tracing::info!(%job_id, in_use, slots = self.slots, "job taken; running analysis");
        let engine = Arc::clone(&self.engine);
        let out_tx = out_tx.clone();
        tokio::spawn(async move {
            let _permit = permit; // released when the analysis finishes
            let started = std::time::Instant::now();
            let outcome = Self::run_job(&engine, &job.sgf).await;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            match &outcome {
                Outcome::Ok(_) => tracing::info!(%job_id, elapsed_ms, "analysis done; sending result"),
                Outcome::Err(error) => {
                    tracing::warn!(%job_id, elapsed_ms, %error, "analysis failed; sending error");
                }
            }
            let reply = ClientMsg::Result { job_id, outcome };
            // Best-effort: if the session is gone, the pgmq lease redelivers.
            drop(out_tx.send(reply).await);
        });
    }

    /// Run one job through the local engine, mapping the result into the wire form.
    async fn run_job(engine: &AnalysisEngine, sgf: &str) -> Outcome {
        match engine.analyze(sgf).await {
            Ok(analysis) => match serde_json::to_value(&analysis) {
                Ok(value) => Outcome::Ok(value),
                Err(err) => Outcome::Err(format!("serialize analysis: {err}")),
            },
            Err(err) => Outcome::Err(err.to_string()),
        }
    }
}
