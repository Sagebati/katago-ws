//! HTTP surface: shared handler state and the aide-documented API router.
//!
//! aide types come through muxa's re-export (`muxa::aide`) so they're the exact
//! version the framework finishes/serves with. `ApiPlugin` (muxa-web) turns the
//! [`ApiRouter`] this builds into the axum router + OpenAPI document and serves
//! the spec — so this module only *declares* the API.

pub mod dto;
pub mod handlers;

use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::routing::get;
use muxa::Result;
use muxa::aide::axum::ApiRouter;
use muxa::aide::axum::routing::{get_with, post_with};
use muxa::aide::openapi::{Info, OpenApi};
use muxa::diesel::DieselPool;
use muxa::web::ratelimit::{self, RateLimitConfig};
use secrecy::{ExposeSecret as _, SecretString};

use crate::cluster::registry::WorkerRegistry;
use crate::cluster::server::ClusterDispatcher;

/// Cloneable state shared with axum handlers (the Diesel pool is Arc-backed, so
/// cloning is a cheap refcount bump).
#[derive(Clone)]
pub struct ApiState {
    /// Postgres pool — also used for pgmq operations (the queue lives in it).
    pub db: DieselPool,
    /// Registry of connected remote workers, in the `orchestrator` role. `None`
    /// in `standalone` (no remote workers) — `/workers` then reports an empty set.
    pub workers: Option<WorkerRegistry>,
    /// Cluster dispatcher state, set only in the `orchestrator` role: it gates the
    /// `/cluster` WebSocket and holds the worker-auth secret. `None` elsewhere.
    pub cluster: Option<Arc<ClusterDispatcher>>,
    /// The client secret for `POST /analyse/batch` (`[api].client_token`).
    /// `None` when unset, which leaves batch submission open to anyone.
    pub client_token: Option<Arc<SecretString>>,
}

impl ApiState {
    /// Wrap a configured client token, treating an empty one as unset.
    pub fn client_token(token: SecretString) -> Option<Arc<SecretString>> {
        (!token.expose_secret().is_empty()).then(|| Arc::new(token))
    }
}

/// Build the aide-documented API router with handler state applied.
///
/// `ApiPlugin` finishes this into the axum router + the OpenAPI document and
/// serves `/openapi.json` + `/docs`, so this only declares routes. The JSON
/// endpoints use `api_route` (so they appear in the spec); `/` (HTML landing
/// page), `/analyses/stream` (SSE) and `/healthz` are plain routes — HTML and
/// streaming responses aren't expressible as a JSON schema, so they're served
/// but not documented.
///
/// `POST /analyse` is the public way to queue KataGo work, so it's the only
/// rate-limited endpoint — by client IP, via the muxa-web layer. It lives in its
/// own sub-router so the layer applies to it alone; reads, the SSE stream, and
/// `/healthz` stay unthrottled. `POST /analyse/batch` queues work too but isn't
/// IP-limited: it's gated by `[api].client_token` when one is set, and open
/// when not.
pub fn api_router(state: ApiState, rl: &RateLimitConfig) -> Result<ApiRouter> {
    let mut submit = ApiRouter::new().api_route(
        "/analyse",
        post_with(handlers::submit, |op| {
            op.summary("Submit an SGF for analysis")
                .description(
                    "Validates the SGF (raw, as the request body) and the optional \
                     `human_profile`, persists a queued job, enqueues it, and returns the job id.",
                )
                .tag("analyses")
        }),
    );
    if rl.enabled {
        submit = submit.layer(ratelimit::per_ip_layer(rl)?);
    }

    let batch = ApiRouter::new()
        .api_route(
            "/analyse/batch",
            post_with(handlers::submit_batch, |op| {
                op.summary("Submit a batch of SGFs")
                    .description(
                        "Up to 100 SGFs in one request, not IP rate-limited. When the \
                         deployment sets `[api].client_token`, requires \
                         `Authorization: Bearer <token>` (401 otherwise); when it doesn't, \
                         anyone may call it. Each entry is a bare SGF string or \
                         `{\"sgf\": …, \"human_profile\": …}`, and gets a result at its \
                         own index — a job id, or why it was refused.",
                    )
                    .tag("analyses")
            }),
        )
        .layer(DefaultBodyLimit::max(handlers::BATCH_BODY_LIMIT));

    let router = submit
        .merge(batch)
        .api_route(
            "/analyses/changes",
            get_with(handlers::changes, |op| {
                op.summary("Job changes since a cursor")
                    .description(
                        "Every job state change after `since`, oldest first, with the cursor \
                         to pass next time. For clients tracking many jobs: one request \
                         instead of one per job. Changes are released once a few seconds \
                         old, so a cursor never skips a write still committing.",
                    )
                    .tag("analyses")
            }),
        )
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
                        "Workers currently holding a live WebSocket session to this orchestrator. \
                         In-memory and per-instance (each orchestrator replica sees only its \
                         own); empty in the standalone role.",
                    )
                    .tag("cluster")
            }),
        )
        .route("/", get(handlers::index))
        .route("/analyses/stream", get(handlers::stream))
        .route("/healthz", get(|| async { "ok" }));

    // Orchestrator role only: the worker control-plane WebSocket. Its auth (a
    // shared Bearer token) is enforced inside the handler. Not an `api_route` — a
    // WS upgrade isn't expressible in the OpenAPI schema.
    let router = if state.cluster.is_some() {
        router.route("/cluster", get(crate::cluster::server::cluster_ws))
    } else {
        router
    };

    let router = router.with_state(state);
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
