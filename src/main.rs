//! katago-ws — a queued SGF analysis microservice built on muxa.
//!
//! Pipeline: `POST /analyse` validates an SGF, persists a job, and enqueues it
//! on pgmq. A worker replays the game, runs net + light-MCTS analysis per move,
//! and stores the annotated result. `GET /analyse/{id}` returns status and, once
//! done, the analysis.
//!
//! ## Launch roles (first CLI argument, or `KATAGO_WS_ROLE` env; default `standalone`)
//!
//! - `standalone` — one process does everything: web API + in-process workers,
//!   sharing Postgres/pgmq. The original single-binary deployment.
//! - `orchestrator` — web API + pgmq + the cluster WebSocket dispatcher, but
//!   **no** engine. It owns the DB and proxies work to remote workers over the
//!   cluster socket.
//! - `worker` — runs KataGo and dials the orchestrator's cluster WebSocket; has
//!   **no** Postgres access. Serves only `/health` (for liveness/readiness probes).
//!
//! `orchestrator` + N `worker`s let the two tiers scale independently when the
//! workers can't reach Postgres directly (the queue lease still lives on the
//! orchestrator, so a worker crash is redelivered exactly as a local crash is).

mod cluster;
mod config;
mod db;
mod engine;
mod error;
mod http;
mod queue;
mod worker;

use std::sync::Arc;

use muxa::prelude::*;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use crate::config::{OrchestratorConfig, WorkerConfig};
use crate::engine::{AnalysisEngine, KataGoEnginePlugin};

/// SQL migrations embedded from the `migrations/` directory. Applied at startup
/// (in one transaction) by the diesel plugin's migrations mode — only in roles
/// that own the database (`standalone`/`orchestrator`).
const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

/// Which slice of the system this process runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Web + in-process workers (default).
    Standalone,
    /// Web + pgmq + cluster WebSocket dispatcher; no engine.
    Orchestrator,
    /// Engine + cluster WebSocket client; no Postgres.
    Worker,
}

impl Role {
    /// Resolve the launch role: the first CLI argument wins; if it's absent, the
    /// `KATAGO_WS_ROLE` environment variable is consulted (handy where setting
    /// argv is awkward — e.g. a Cloudflare Container, whose command is fixed by
    /// the image `ENTRYPOINT`); if neither is set, [`Role::Standalone`].
    fn resolve() -> Result<Self, String> {
        let raw = std::env::args()
            .nth(1)
            .or_else(|| std::env::var("KATAGO_WS_ROLE").ok());
        match raw.as_deref() {
            None | Some("standalone") => Ok(Self::Standalone),
            Some("orchestrator") => Ok(Self::Orchestrator),
            Some("worker") => Ok(Self::Worker),
            Some(other) => Err(format!(
                "unknown role {other:?}; expected one of: standalone, orchestrator, worker"
            )),
        }
    }
}

#[tokio::main]
async fn main() -> muxa::Result<()> {
    let role = Role::resolve().map_err(Error::other)?;
    tracing::info!(?role, "starting katago-ws");
    match role {
        Role::Standalone => run_standalone().await,
        Role::Orchestrator => run_orchestrator().await,
        Role::Worker => run_worker().await,
    }
}

/// `standalone`: the original single-process deployment — web API and in-process
/// workers sharing one Postgres/pgmq.
async fn run_standalone() -> muxa::Result<()> {
    let app = App::default()
        .with_plugin(SentryPlugin)
        .await?
        .with_plugin(OtelPlugin)
        .await?
        // `diesel-sentry` feature → DieselPlugin installs query tracing itself.
        .with_plugin(DieselPlugin::new().with_migrations(MigrationsRunner::new(MIGRATIONS)))
        .await?
        .with_plugin(PgmqPlugin::<DieselBackend, _>::new().queues([queue::Queue::Analysis.as_str()]))
        .await?
        .with_plugin(KataGoEnginePlugin)
        .await?;

    let db = Selector::<DieselPool, _>::select(app.state()).clone();
    let engine = Arc::clone(Selector::<Arc<AnalysisEngine>, _>::select(app.state()));
    let worker_cfg = extract::<WorkerConfig>(&app, "worker");
    let rl_cfg = extract::<RateLimitConfig>(&app, "ratelimit");

    let mut app = app;
    worker::register(app.ctx_mut(), db.clone(), engine, worker_cfg);

    // No remote workers in this role (the engine runs in-process), so `/workers`
    // is empty and the `/cluster` socket isn't mounted.
    let api = http::ApiState { db, workers: None, cluster: None };
    serve_api(app, api, &rl_cfg).await
}

/// `orchestrator`: web API + pgmq + the cluster WebSocket dispatcher; no engine.
/// Owns the DB and proxies work to remote workers.
async fn run_orchestrator() -> muxa::Result<()> {
    let app = App::default()
        .with_plugin(SentryPlugin)
        .await?
        .with_plugin(OtelPlugin)
        .await?
        .with_plugin(DieselPlugin::new().with_migrations(MigrationsRunner::new(MIGRATIONS)))
        .await?
        .with_plugin(PgmqPlugin::<DieselBackend, _>::new().queues([queue::Queue::Analysis.as_str()]))
        .await?;

    let db = Selector::<DieselPool, _>::select(app.state()).clone();
    let worker_cfg = extract::<WorkerConfig>(&app, "worker");
    let orch_cfg = extract::<OrchestratorConfig>(&app, "orchestrator");
    let rl_cfg = extract::<RateLimitConfig>(&app, "ratelimit");
    // Connection lifetimes derive from the app-wide shutdown token.
    let shutdown = app.ctx().shutdown.clone();

    let mut app = app;
    // The dispatcher records each connected worker here; `/workers` reads it too.
    let registry = cluster::registry::WorkerRegistry::spawn(app.ctx_mut());

    // The cluster socket rides the web router (`GET /cluster`); set the dispatcher
    // state so `api_router` mounts it and the handler can auth + dispatch jobs.
    let dispatcher = cluster::server::ClusterDispatcher::new(worker_cfg, orch_cfg.auth_token, shutdown);
    let api = http::ApiState {
        db,
        workers: Some(registry),
        cluster: Some(Arc::new(dispatcher)),
    };
    serve_api(app, api, &rl_cfg).await
}

/// `worker`: KataGo engine + cluster WebSocket client dialing the orchestrator; no
/// Postgres. Serves only `/health` so `App::run` has a serve loop and probes have a target.
async fn run_worker() -> muxa::Result<()> {
    let app = App::default()
        .with_plugin(SentryPlugin)
        .await?
        .with_plugin(OtelPlugin)
        .await?
        .with_plugin(KataGoEnginePlugin)
        .await?;

    let engine = Arc::clone(Selector::<Arc<AnalysisEngine>, _>::select(app.state()));
    let worker_cfg = extract::<WorkerConfig>(&app, "worker");

    let mut app = app;
    cluster::client::register_client(app.ctx_mut(), engine, worker_cfg);

    app.with_plugin(WebPlugin::new(health_routes))
        .await?
        .run()
        .await
}

/// Finish + serve the aide API (shared by `standalone` and `orchestrator`). `ApiPlugin`
/// (added last) turns the `ApiRouter` into the axum router + OpenAPI document and
/// owns the serve loop.
async fn serve_api<S: State>(
    app: AppBuilder<S>,
    api: http::ApiState,
    rl_cfg: &RateLimitConfig,
) -> muxa::Result<()> {
    let router = http::api_router(api, rl_cfg)?;
    app.with_plugin(ApiPlugin::new(http::api_fn(router), http::openapi_seed()))
        .await?
        .run()
        .await
}

/// Extract a config section, falling back to its default when absent/invalid.
fn extract<T: serde::de::DeserializeOwned + Default>(app: &AppBuilder<impl State>, key: &str) -> T {
    app.ctx().figment().extract_inner(key).unwrap_or_default()
}

/// Minimal routes for the `worker` role: just a liveness/readiness `/health`.
fn health_routes<S>(_state: &S) -> axum::Router {
    axum::Router::new().route("/health", axum::routing::get(|| async { "ok" }))
}
