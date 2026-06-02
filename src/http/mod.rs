//! HTTP surface: shared handler state and the aide-documented API router.
//!
//! aide types come through muxa's re-export (`muxa::aide`) so they're the exact
//! version the framework finishes/serves with. `ApiPlugin` (muxa-web) turns the
//! [`ApiRouter`] this builds into the axum router + OpenAPI document and serves
//! the spec — so this module only *declares* the API.

pub mod dto;
pub mod handlers;

use axum::routing::get;
use muxa::Result;
use muxa::aide::axum::ApiRouter;
use muxa::aide::axum::routing::{get_with, post_with};
use muxa::aide::openapi::{Info, OpenApi};
use muxa::diesel::DieselPool;
use muxa::web::ratelimit::{self, RateLimitConfig};

use crate::cluster::registry::WorkerRegistry;

/// Cloneable state shared with axum handlers (the Diesel pool is Arc-backed, so
/// cloning is a cheap refcount bump).
#[derive(Clone)]
pub struct ApiState {
    /// Postgres pool — also used for pgmq operations (the queue lives in it).
    pub db: DieselPool,
    /// Registry of connected remote workers, in the `orchestrator` role. `None`
    /// in `standalone` (no remote workers) — `/workers` then reports an empty set.
    pub workers: Option<WorkerRegistry>,
}

/// Build the aide-documented API router with handler state applied.
///
/// `ApiPlugin` finishes this into the axum router + the OpenAPI document and
/// serves `/openapi.json` + `/docs`, so this only declares routes. The JSON
/// endpoints use `api_route` (so they appear in the spec); `/analyses/stream`
/// (SSE) and `/health` are plain routes — streaming responses aren't
/// expressible as a JSON schema, so they're served but not documented.
///
/// `POST /analyse` is the only expensive endpoint (it queues KataGo work), so
/// it's the only one rate-limited — by client IP, via the muxa-web layer. It
/// lives in its own sub-router so the layer applies to it alone; reads, the SSE
/// stream, and `/health` stay unthrottled.
pub fn api_router(state: ApiState, rl: &RateLimitConfig) -> Result<ApiRouter> {
    let mut submit = ApiRouter::new().api_route(
        "/analyse",
        post_with(handlers::submit, |op| {
            op.summary("Submit an SGF for analysis")
                .description("Validates the SGF, persists a queued job, enqueues it, and returns the job id.")
                .tag("analyses")
        }),
    );
    if rl.enabled {
        submit = submit.layer(ratelimit::per_ip_layer(rl)?);
    }

    let router = submit
        .api_route(
            "/analyse/{id}",
            get_with(handlers::status, |op| {
                op.summary("Get one analysis")
                    .description(
                        "Returns the job status and, once done, the analysis result. \
                         Pass `?wait=<secs>` to long-poll until the job finishes \
                         (blocks up to the given budget, server-capped).",
                    )
                    .tag("analyses")
            }),
        )
        .api_route(
            "/analyses",
            get_with(handlers::list, |op| {
                op.summary("List analyses")
                    .description("Lifecycle summary of every analysis job, newest first.")
                    .tag("analyses")
            }),
        )
        .api_route(
            "/workers",
            get_with(handlers::workers, |op| {
                op.summary("List connected workers")
                    .description(
                        "Workers currently holding a live gRPC session to this orchestrator. \
                         In-memory and per-instance (each orchestrator replica sees only its \
                         own); empty in the standalone role.",
                    )
                    .tag("cluster")
            }),
        )
        .route("/analyses/stream", get(handlers::stream))
        .route("/health", get(|| async { "ok" }))
        .with_state(state);
    Ok(router)
}

/// Seed OpenAPI document carrying service metadata. aide fills in the routes and
/// schemas when `ApiPlugin` finishes the router; this supplies `info`.
pub fn openapi_seed() -> OpenApi {
    OpenApi {
        info: Info {
            title: "katago-ws".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            summary: Some("KataGo-backed SGF analysis service".to_owned()),
            ..Info::default()
        },
        ..OpenApi::default()
    }
}

/// Wrap a pre-built `ApiRouter` as an `ApiPlugin` routes callback.
///
/// The `impl FnOnce(&S) -> ApiRouter` return type forces the higher-ranked
/// lifetime binder (`for<'a> FnOnce(&'a S)`) the plugin requires; a bare
/// `move |_state| ...` closure infers a single concrete lifetime instead.
pub fn api_fn<S>(router: ApiRouter) -> impl FnOnce(&S) -> ApiRouter + Send + 'static {
    move |_state: &S| router
}
