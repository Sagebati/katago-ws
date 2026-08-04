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
mod metrics;
mod paths;
mod queue;
mod worker;

use std::process::ExitCode;
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
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            report(&err);
            ExitCode::FAILURE
        }
    }
}

async fn run() -> muxa::Result<()> {
    let role = Role::resolve().map_err(Error::other)?;
    tracing::info!(?role, "starting katago-ws");
    match role {
        Role::Standalone => run_standalone().await,
        Role::Orchestrator => run_orchestrator().await,
        Role::Worker => run_worker().await,
    }
}

/// Turn a startup failure into one readable message instead of the raw
/// `Debug`-formatted error chain (which buries a carefully-written message,
/// e.g. a preflight hint, inside nested struct/enum syntax exposing internal
/// Rust type names). Walks `source()` so the actual cause — not just "a
/// plugin failed during build" — reaches the operator.
fn report(err: &muxa::Error) {
    let mut lines = vec![err.to_string()];
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        lines.push(cause.to_string());
        source = cause.source();
    }
    let message = lines.join("\n  caused by: ");
    // The tracing subscriber is installed once the figment is built
    // (`BuildCtx::new`); the one failure that can happen before that is an
    // unknown `--role` (`Role::resolve`, above) — exactly what a first-time
    // user might hit, so it must still be visible.
    if tracing::dispatcher::has_been_set() {
        tracing::error!("{message}");
    } else {
        #[allow(
            clippy::print_stderr,
            reason = "fatal startup error before the tracing subscriber exists"
        )]
        {
            eprintln!("Error: {message}");
        }
    }
}

/// Build the app from the resolved config-file location (`$MUXA_CONFIG` >
/// `./muxa.toml` > the XDG config path > the bare default), logging which one
/// won so "which config is this process actually using?" has one answer.
///
/// The log comes *after* construction, not before: the tracing subscriber
/// isn't installed until `BuildCtx::new` runs as part of building the app
/// (see `report`'s doc comment) — logging first would silently vanish, same
/// as the pre-existing `tracing::info!(?role, ...)` in `run()` above.
fn build_app() -> App {
    let source = paths::resolve_config_file();
    let app = App::with_config_file(source.path());
    tracing::info!(
        config = %source.path().display(),
        origin = source.origin(),
        "resolved configuration file"
    );
    app
}

/// `standalone`: the original single-process deployment — web API and in-process
/// workers sharing one Postgres/pgmq.
async fn run_standalone() -> muxa::Result<()> {
    let app = build_app()
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
    let worker_cfg = extract::<WorkerConfig>(&app, "worker")?;
    let rl_cfg = extract::<RateLimitConfig>(&app, "ratelimit")?;

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
    };
    serve_api(app, api, &rl_cfg).await
}

/// `orchestrator`: web API + pgmq + the cluster WebSocket dispatcher; no engine.
/// Owns the DB and proxies work to remote workers.
async fn run_orchestrator() -> muxa::Result<()> {
    let app = build_app()
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
    let worker_cfg = extract::<WorkerConfig>(&app, "worker")?;
    let orch_cfg = extract::<OrchestratorConfig>(&app, "orchestrator")?;
    let rl_cfg = extract::<RateLimitConfig>(&app, "ratelimit")?;
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
    };
    serve_api(app, api, &rl_cfg).await
}

/// `worker`: KataGo engine + cluster WebSocket client dialing the orchestrator; no
/// Postgres. Serves only `/health` so `App::run` has a serve loop and probes have a target.
async fn run_worker() -> muxa::Result<()> {
    let app = build_app()
        .with_plugin(SentryPlugin)
        .await?
        .with_plugin(OtelPlugin)
        .await?
        .with_plugin(KataGoEnginePlugin)
        .await?;

    let engine = Arc::clone(Selector::<Arc<AnalysisEngine>, _>::select(app.state()));
    let worker_cfg = extract::<WorkerConfig>(&app, "worker")?;

    let mut app = app;
    let engine_for_health = Arc::clone(&engine);
    cluster::client::register_client(app.ctx_mut(), engine, worker_cfg);

    // `/health` reflects whether the KataGo subprocess is actually still
    // alive (not a static "ok") — so the orchestrator stops routing jobs to
    // a worker whose engine has died, instead of only learning via failed
    // jobs.
    app.with_plugin(WebPlugin::new(move |_state: &_| {
        axum::Router::new().route(
            "/health",
            axum::routing::get(move || {
                let engine = Arc::clone(&engine_for_health);
                async move {
                    if engine.is_alive() {
                        (axum::http::StatusCode::OK, "ok")
                    } else {
                        (axum::http::StatusCode::SERVICE_UNAVAILABLE, "engine down")
                    }
                }
            }),
        )
    }))
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

/// Extract a config section, defaulting when it's genuinely absent but
/// propagating a real error when it's present-and-malformed (e.g.
/// `concurrency = "two"`). A typo silently running on defaults instead of
/// failing loudly is exactly the class of confusing failure the rest of this
/// startup path works hard to eliminate.
fn extract<T: serde::de::DeserializeOwned + Default>(
    app: &AppBuilder<impl State>,
    key: &str,
) -> muxa::Result<T> {
    match app.ctx().figment().extract_inner(key) {
        Ok(value) => Ok(value),
        Err(err) if err.missing() => Ok(T::default()),
        Err(err) => Err(err.into()),
    }
}
