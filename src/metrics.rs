//! Custom OpenTelemetry metrics for the cluster.
//!
//! These use the process-global meter provider that `muxa-otel`'s `otel-metrics`
//! feature installs. When no OTLP endpoint is configured (`[otel].endpoint`
//! unset), `global::meter` returns a no-op meter, so every instrument is a no-op
//! and recording costs ~nothing — safe to call unconditionally from any role.
//!
//! Three instruments, matching the operational questions:
//! - `katago_ws.workers.connected` — gauge: cluster workers with a live session.
//! - `katago_ws.jobs.processed`     — counter (by `outcome`): job throughput.
//! - `katago_ws.job.duration`       — histogram (seconds, by `outcome`): time per job.
//!
//! Exported over OTLP to whatever collector `[otel].endpoint` points at
//! (Prometheus/Grafana, an OTel Collector, …), on the `[otel].metric_interval_secs`
//! cadence.

use std::sync::OnceLock;

use opentelemetry::metrics::{Counter, Histogram, UpDownCounter};
use opentelemetry::{KeyValue, global};

/// Process-wide instrument handles, built once on first use.
pub struct Metrics {
    /// Cluster workers currently holding a live session to this orchestrator.
    /// An up/down counter used as a gauge (+1 on connect, -1 on disconnect).
    workers_connected: UpDownCounter<i64>,
    /// Jobs that reached a terminal state, tagged `outcome` ∈ {done, retry, failed}.
    jobs_processed: Counter<u64>,
    /// Wall-clock seconds spent analyzing one job, tagged `outcome`.
    job_duration: Histogram<f64>,
}

impl Metrics {
    /// A worker connected (`+1`) or disconnected (`-1`).
    pub fn worker_delta(&self, delta: i64) {
        self.workers_connected.add(delta, &[]);
    }

    /// Record one processed job. `outcome` is `"done"`, `"retry"` (a non-terminal
    /// attempt failure left for redelivery), or `"failed"` (terminal). `duration`
    /// is the analysis wall-time when one ran (`None` for the poison/max-attempts
    /// path, which gives up before analyzing).
    pub fn job_processed(&self, outcome: &'static str, duration: Option<f64>) {
        let attrs = [KeyValue::new("outcome", outcome)];
        self.jobs_processed.add(1, &attrs);
        if let Some(secs) = duration {
            self.job_duration.record(secs, &attrs);
        }
    }
}

/// The shared metrics handles (built on first access).
pub fn metrics() -> &'static Metrics {
    static METRICS: OnceLock<Metrics> = OnceLock::new();
    METRICS.get_or_init(|| {
        let meter = global::meter("katago-ws");
        Metrics {
            workers_connected: meter
                .i64_up_down_counter("katago_ws.workers.connected")
                .with_description("Cluster workers with a live session to this orchestrator")
                .build(),
            jobs_processed: meter
                .u64_counter("katago_ws.jobs.processed")
                .with_description("Analysis jobs that reached a terminal state, by outcome")
                .build(),
            job_duration: meter
                .f64_histogram("katago_ws.job.duration")
                .with_unit("s")
                .with_description("Wall-clock time to analyze one job")
                // Buckets tuned for seconds-to-minutes KataGo analyses.
                .with_boundaries(vec![0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0])
                .build(),
        }
    })
}
