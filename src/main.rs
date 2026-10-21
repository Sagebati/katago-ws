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
//!   **no** Postgres access. Serves only `/healthz` (for liveness/readiness probes).
//!
//! - `bench` — KataGo only: analyzes a directory of SGFs at a fixed concurrency
//!   and writes a throughput report (see [`bench`]). No Postgres, no web server.
//! - `healthcheck` — probes this instance's own `GET /healthz` on `[web].port` and
//!   exits 0 or 1; the images' Docker `HEALTHCHECK` (see [`healthcheck`]).
//! - `autotune [--fast] [--force]` — run KataGo's throughput auto-tune once
//!   (see [`engine::autotune`]), write the cache, log the settings, exit. The
//!   engine roles then start from the cache. `--fast` = the under-30 s search;
//!   `--force` re-tunes even when a cached result exists.
//!
//! The engine roles also take flags for the common `[engine]` / `[worker]` keys
//! (`katago-ws worker --threads 8 --slots 4`; `--help` lists them), layered over
//! the config file and env — see [`cli`].
//!
//! `orchestrator` + N `worker`s let the two tiers scale independently when the
//! workers can't reach Postgres directly (the queue lease still lives on the
//! orchestrator, so a worker crash is redelivered exactly as a local crash is).

mod auth;
mod bench;
mod blocking;
mod cli;
mod cluster;
mod config;
mod db;
mod engine;
mod error;
mod healthcheck;
mod http;
mod metrics;
mod queue;
mod trace;
mod worker;

use std::sync::Arc;

use muxa::HNil;
use muxa::prelude::*;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use crate::cli::Role;
use crate::config::{ApiConfig, OrchestratorConfig, WorkerConfig};
use crate::engine::{AnalysisEngine, AutotunePlugin, KataGoEnginePlugin};

/// SQL migrations embedded from the `migrations/` directory. Applied at startup
/// (in one transaction) by the diesel plugin's migrations mode — only in roles
/// that own the database (`standalone`/`orchestrator`).
const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

// One thread: every role waits on I/O (Postgres, KataGo's pipes, sockets) —
// the engine's work happens in the KataGo process. Anything that blocks must go
// through `spawn_blocking` (see `blocking`), and `block_in_place` would panic.
#[tokio::main(flavor = "current_thread")]
async fn main() -> muxa::Result<()> {
    let role = Role::resolve();
    // The probe runs every few seconds; keep it quiet and dependency-free.
    if let Role::Healthcheck = role {
        return run_healthcheck().await;
    }
    tracing::info!(role = role.name(), "starting katago-ws");
    let app = app(&role);
    match role {
        Role::Standalone { .. } => run_standalone(app).await,
        Role::Orchestrator => run_orchestrator(app).await,
        Role::Worker { .. } => run_worker(app).await,
        Role::Bench { .. } => run_bench(app).await,
        Role::Healthcheck => run_healthcheck().await,
        Role::Autotune { fast, force, .. } => run_autotune(app, fast, force).await,
    }
}

/// `muxa::app!()` — the config file merged with `KATAGO_WS_*` env — with the
/// role's flags merged over both.
fn app(role: &Role) -> AppBuilder<HNil> {
    App::with_figment(muxa::load_figment(env!("CARGO_PKG_NAME")).merge(role.overrides()))
}

/// `standalone`: the original single-process deployment — web API and in-process
/// workers sharing one Postgres/pgmq.
async fn run_standalone(app: AppBuilder<HNil>) -> muxa::Result<()> {
    let app = app
        .with_plugin(SentryPlugin)
        .await?
        .with_plugin(OtelPlugin)
        .await?
        // `diesel-sentry` feature → DieselPlugin installs query tracing itself.
        .with_plugin(DieselPlugin::new().with_migrations(MigrationsRunner::new(MIGRATIONS)))
        .await?
        .with_plugin(
            PgmqPlugin::<DieselBackend, _>::new().queues([queue::Queue::Analysis.as_str()]),
        )
        .await?
        .with_plugin(KataGoEnginePlugin)
        .await?;

    let db = Selector::<DieselPool, _>::select(app.state()).clone();
    let engine = Arc::clone(Selector::<Arc<AnalysisEngine>, _>::select(app.state()));
    let worker_cfg = extract::<WorkerConfig>(&app, "worker");
    let rl_cfg = extract::<RateLimitConfig>(&app, "ratelimit");
    let api_cfg = extract::<ApiConfig>(&app, "api");

    let mut app = app;
    worker::register(app.ctx_mut(), db.clone(), engine, worker_cfg);
    // Sample queue depth → `katago_ws.jobs.waiting`.
    metrics::spawn_queue_depth_sampler(app.ctx_mut(), db.clone());

    // No remote workers in this role (the engine runs in-process), so `/workers`
    // is empty and the `/cluster` socket isn't mounted.
    let api = http::ApiState {
        db,
        workers: None,
        cluster: None,
        client_token: http::ApiState::client_token(api_cfg.client_token),
    };
    serve_api(app, api, &rl_cfg).await
}

/// `orchestrator`: web API + pgmq + the cluster WebSocket dispatcher; no engine.
/// Owns the DB and proxies work to remote workers.
async fn run_orchestrator(app: AppBuilder<HNil>) -> muxa::Result<()> {
    let app = app
        .with_plugin(SentryPlugin)
        .await?
        .with_plugin(OtelPlugin)
        .await?
        .with_plugin(DieselPlugin::new().with_migrations(MigrationsRunner::new(MIGRATIONS)))
        .await?
        .with_plugin(
            PgmqPlugin::<DieselBackend, _>::new().queues([queue::Queue::Analysis.as_str()]),
        )
        .await?;

    let db = Selector::<DieselPool, _>::select(app.state()).clone();
    let worker_cfg = extract::<WorkerConfig>(&app, "worker");
    let orch_cfg = extract::<OrchestratorConfig>(&app, "orchestrator");
    let rl_cfg = extract::<RateLimitConfig>(&app, "ratelimit");
    let api_cfg = extract::<ApiConfig>(&app, "api");
    // Connection lifetimes derive from the app-wide shutdown token.
    let shutdown = app.ctx().shutdown.clone();

    let mut app = app;
    // The dispatcher records each connected worker here; `/workers` reads it too.
    let registry = cluster::registry::WorkerRegistry::spawn(app.ctx_mut());
    // Sample queue depth → `katago_ws.jobs.waiting`.
    metrics::spawn_queue_depth_sampler(app.ctx_mut(), db.clone());

    // The cluster socket rides the web router (`GET /cluster`); set the dispatcher
    // state so `api_router` mounts it and the handler can auth + dispatch jobs.
    let dispatcher =
        cluster::server::ClusterDispatcher::new(worker_cfg, orch_cfg.auth_token, shutdown);
    let api = http::ApiState {
        db,
        workers: Some(registry),
        cluster: Some(Arc::new(dispatcher)),
        client_token: http::ApiState::client_token(api_cfg.client_token),
    };
    serve_api(app, api, &rl_cfg).await
}

/// `worker`: KataGo engine + cluster WebSocket client dialing the orchestrator; no
/// Postgres. Serves only `/healthz` so `App::run` has a serve loop and probes have a target.
async fn run_worker(app: AppBuilder<HNil>) -> muxa::Result<()> {
    let app = app
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

/// `bench`: launch KataGo, run the throughput benchmark, exit. No telemetry
/// export, DB, or web server — only the engine is under test.
async fn run_bench(app: AppBuilder<HNil>) -> muxa::Result<()> {
    let app = app.with_plugin(KataGoEnginePlugin).await?;
    let engine = Arc::clone(Selector::<Arc<AnalysisEngine>, _>::select(app.state()));
    // The engine's own copy: it carries the auto-tuned settings.
    let engine_cfg = engine.config().clone();
    let bench_cfg = extract::<bench::BenchConfig>(&app, "bench");
    bench::run(engine, &engine_cfg, bench_cfg)
        .await
        .map_err(Error::other)
}

/// `autotune [--fast] [--force]`: tune KataGo for this host into the cache
/// (`[engine] autotune_cache`) and exit, so the engine roles start from it.
async fn run_autotune(app: AppBuilder<HNil>, fast: bool, force: bool) -> muxa::Result<()> {
    app.with_plugin(AutotunePlugin { fast, force }).await?;
    Ok(())
}

/// `healthcheck`: `GET /healthz` on this instance's `[web].port` (the same config
/// and env vars the serving roles read), for the images' Docker `HEALTHCHECK`.
/// Builds no plugins, so it starts and exits in milliseconds.
async fn run_healthcheck() -> muxa::Result<()> {
    let port = muxa::app!()
        .figment()
        .extract_inner::<u16>("web.port")
        .unwrap_or(DEFAULT_WEB_PORT);
    healthcheck::probe(port).await.map_err(Error::other)
}

/// muxa-web's default `[web].port`, for a config that doesn't set one.
const DEFAULT_WEB_PORT: u16 = 3000;

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
    app.figment().extract_inner(key).unwrap_or_default()
}

/// Minimal routes for the `worker` role: just a liveness/readiness `/healthz`.
fn health_routes<S>(_state: &S) -> axum::Router {
    axum::Router::new().route("/healthz", axum::routing::get(|| async { "ok" }))
}
