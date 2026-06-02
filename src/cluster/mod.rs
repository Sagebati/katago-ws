//! Orchestrator↔worker gRPC control plane.
//!
//! Workers run on hosts that cannot reach Postgres; they only talk to the
//! orchestrator. The orchestrator owns the DB + pgmq and *proxies* the work: it
//! reads the queue, pushes each job to a connected worker over a bidirectional
//! gRPC stream, and writes the result back. This module holds the generated
//! protobuf types ([`proto`]), the orchestrator-side dispatcher ([`server`]),
//! and the worker-side client ([`client`]).
//!
//! The unit of work is the same coarse call the in-process worker makes —
//! `analyze(sgf) → GameAnalysis` — so both the monolith (`standalone` role) and the
//! orchestrator drive the *identical* [`crate::worker::worker_loop`]; only the
//! [`crate::worker::Executor`] differs (local engine vs. remote stream).

pub mod client;
pub mod registry;
pub mod server;

/// Generated tonic/prost code for `proto/cluster.proto` (package `cluster`).
///
/// Compiled by `build.rs` into `OUT_DIR` and included here. The blanket `allow`
/// silences our curated clippy `restriction` set and rustc lints on machine
/// generated code (it isn't ours to style-check).
pub mod proto {
    #![allow(
        clippy::all,
        clippy::pedantic,
        clippy::nursery,
        clippy::restriction,
        unreachable_pub,
        missing_docs,
        dead_code,
        reason = "tonic/prost-generated code is not ours to lint"
    )]
    tonic::include_proto!("cluster");
}
