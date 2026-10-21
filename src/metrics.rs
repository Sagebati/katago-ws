//! Custom OpenTelemetry metrics for the cluster.
//!
//! These use the process-global meter provider that `muxa-otel`'s `otel-metrics`
//! feature installs. When no OTLP endpoint is configured (`[otel].endpoint`
//! unset), `global::meter` returns a no-op meter, so every instrument is a no-op
//! and recording costs ~nothing — safe to call unconditionally from any role.
//!
//! Seven instruments, matching the operational questions:
//! - `katago_ws.workers.connected` — gauge: cluster workers with a live session.
//! - `katago_ws.jobs.waiting`       — gauge: jobs queued and waiting to run.
//! - `katago_ws.jobs.processed`     — counter (by `outcome`): job throughput.
//! - `katago_ws.job.duration`       — histogram (seconds, by `outcome`): time per job.
//! - `katago_ws.job.queue_wait`     — histogram (seconds): time a job sat queued
//!   before its first delivery — the wait a client sees on top of the analysis.
//! - `katago_ws.moves.analyzed`     — counter: moves in completed analyses. Its
//!   rate is the service's throughput — jobs/s isn't, game lengths vary too much.
//! - `katago_ws.positions.analyzed` — counter (by `pass` ∈ {full, screen,
//!   refine}): positions KataGo searched — the engine's work, and with two-pass
//!   analysis `refine / screen` is the share of positions searched in depth.
//!
//! Exported over OTLP to whatever collector `[otel].endpoint` points at
//! (Prometheus/Grafana, an OTel Collector, …), on the `[otel].metric_interval_secs`
//! cadence.

use std::sync::OnceLock;
use std::time::Duration;

use muxa::diesel::DieselPool;
use muxa::prelude::{BuildCtx, ShutdownToken};
use opentelemetry::metrics::{Counter, Gauge, Histogram};
use opentelemetry::{KeyValue, global};

use crate::db::{self, status::JobStatus};

/// How often the queue-depth sampler reads `jobs.waiting` from the DB. Kept well
/// below a typical metric export interval so the exported gauge is fresh, while a
/// cheap `COUNT` every few seconds is negligible next to the dashboard's queries.
const QUEUE_SAMPLE_INTERVAL: Duration = Duration::from_secs(10);

// Metric names — shared by the OTEL instruments and the Sentry SDK emission so
// the two backends agree.
const NAME_WORKERS: &str = "katago_ws.workers.connected";
const NAME_WAITING: &str = "katago_ws.jobs.waiting";
const NAME_PROCESSED: &str = "katago_ws.jobs.processed";
const NAME_DURATION: &str = "katago_ws.job.duration";
const NAME_QUEUE_WAIT: &str = "katago_ws.job.queue_wait";
const NAME_MOVES: &str = "katago_ws.moves.analyzed";
const NAME_POSITIONS: &str = "katago_ws.positions.analyzed";

/// Process-wide instrument handles, built once on first use.
///
/// Each metric is **dual-emitted**: to OpenTelemetry (→ OTLP collector / Grafana)
/// and to Sentry's Application Metrics product via the SDK. The OTEL handles are
/// stored here; the Sentry side is stateless (`sentry::metrics::*` go through the
/// global hub) and a no-op when no DSN is set. Sentry does not ingest OTLP
/// metrics, so its native SDK API is the only way to land these in Sentry.
pub struct Metrics {
    /// Cluster workers currently holding a live session to this orchestrator.
    /// A gauge — the registry sets the absolute count on every connect/disconnect.
    workers_connected: Gauge<i64>,
    /// Jobs queued and waiting to be picked up (queue depth). Sampled from the
    /// DB by [`spawn_queue_depth_sampler`], since the value lives in Postgres.
    jobs_waiting: Gauge<i64>,
    /// Jobs that reached a terminal state, tagged `outcome` ∈ {done, retry, failed}.
    jobs_processed: Counter<u64>,
    /// Wall-clock seconds spent analyzing one job, tagged `outcome`.
    job_duration: Histogram<f64>,
    /// Seconds a job waited in the queue before its first delivery.
    job_queue_wait: Histogram<f64>,
    /// Moves in successfully completed analyses.
    moves_analyzed: Counter<u64>,
    /// Positions KataGo searched, tagged `pass`. Recorded where the engine runs
    /// (`standalone` or `worker`), not on the orchestrator.
    positions_analyzed: Counter<u64>,
}

impl Metrics {
    /// Set the absolute number of connected workers.
    pub fn set_workers_connected(&self, count: i64) {
        self.workers_connected.record(count, &[]);
        sentry::metrics::gauge(NAME_WORKERS, count as f64).capture();
    }

    /// Set the current queue depth (jobs waiting to be processed).
    pub fn set_jobs_waiting(&self, count: i64) {
        self.jobs_waiting.record(count, &[]);
        sentry::metrics::gauge(NAME_WAITING, count as f64).capture();
    }

    /// Record one processed job. `outcome` is `"done"`, `"retry"` (a non-terminal
    /// attempt failure left for redelivery), or `"failed"` (terminal). `duration`
    /// is the analysis wall-time when one ran (`None` for the poison/max-attempts
    /// path, which gives up before analyzing).
    pub fn job_processed(&self, outcome: &'static str, duration: Option<f64>) {
        self.jobs_processed
            .add(1, &[KeyValue::new("outcome", outcome)]);
        sentry::metrics::counter(NAME_PROCESSED, 1.0)
            .attribute("outcome", outcome)
            .capture();
        if let Some(secs) = duration {
            self.job_duration
                .record(secs, &[KeyValue::new("outcome", outcome)]);
            sentry::metrics::distribution(NAME_DURATION, secs)
                .unit("second")
                .attribute("outcome", outcome)
                .capture();
        }
    }
}

impl Metrics {
    /// Record how long a job waited in the queue before a worker picked it up.
    pub fn job_queue_wait(&self, secs: f64) {
        self.job_queue_wait.record(secs, &[]);
        sentry::metrics::distribution(NAME_QUEUE_WAIT, secs)
            .unit("second")
            .capture();
    }

    /// Record the moves of one successfully analyzed game.
    pub fn moves_analyzed(&self, moves: u64) {
        self.moves_analyzed.add(moves, &[]);
        sentry::metrics::counter(NAME_MOVES, moves as f64).capture();
    }

    /// Record `positions` searched by one KataGo `pass`.
    pub fn positions_analyzed(&self, pass: &'static str, positions: u64) {
        self.positions_analyzed
            .add(positions, &[KeyValue::new("pass", pass)]);
        sentry::metrics::counter(NAME_POSITIONS, positions as f64)
            .attribute("pass", pass)
            .capture();
    }
}

/// The shared metrics handles (built on first access).
pub fn metrics() -> &'static Metrics {
    static METRICS: OnceLock<Metrics> = OnceLock::new();
    METRICS.get_or_init(|| {
        let meter = global::meter("katago-ws");
        Metrics {
            workers_connected: meter
                .i64_gauge(NAME_WORKERS)
                .with_description("Cluster workers with a live session to this orchestrator")
                .build(),
            jobs_waiting: meter
                .i64_gauge(NAME_WAITING)
                .with_description("Jobs queued and waiting to be processed")
                .build(),
            jobs_processed: meter
                .u64_counter(NAME_PROCESSED)
                .with_description("Analysis jobs that reached a terminal state, by outcome")
                .build(),
            job_duration: meter
                .f64_histogram(NAME_DURATION)
                .with_unit("s")
                .with_description("Wall-clock time to analyze one job")
                // Buckets tuned for seconds-to-minutes KataGo analyses.
                .with_boundaries(vec![0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0])
                .build(),
            job_queue_wait: meter
                .f64_histogram(NAME_QUEUE_WAIT)
                .with_unit("s")
                .with_description("Time a job waited in the queue before its first delivery")
                // From an idle worker's poll (sub-second) to a backlog (hours).
                .with_boundaries(vec![
                    0.1, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0, 900.0, 3600.0, 14400.0,
                ])
                .build(),
            moves_analyzed: meter
                .u64_counter(NAME_MOVES)
                .with_description("Moves in successfully analyzed games")
                .build(),
            positions_analyzed: meter
                .u64_counter(NAME_POSITIONS)
                .with_description("Positions searched by KataGo, by pass")
                .build(),
        }
    })
}

/// Spawn a background task that samples the queue depth (jobs in `Queued`) and
/// records it into the `katago_ws.jobs.waiting` gauge. The value lives in
/// Postgres, and OTEL's gauge-collection callback is synchronous (can't await a
/// diesel-async query), so a small async sampler is the clean way to feed it.
///
/// Only the DB-owning roles (`standalone` / `orchestrator`) should call this.
pub fn spawn_queue_depth_sampler(ctx: &mut BuildCtx, db: DieselPool) {
    ctx.tasks.spawn(
        "jobs-waiting-sampler",
        move |shutdown: ShutdownToken| async move {
            let metrics = metrics();
            loop {
                match db::count_in_state(&db, JobStatus::Queued).await {
                    Ok(count) => metrics.set_jobs_waiting(count),
                    Err(err) => tracing::warn!(error = %err, "queue-depth sample failed"),
                }
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    () = tokio::time::sleep(QUEUE_SAMPLE_INTERVAL) => {}
                }
            }
        },
    );
}
