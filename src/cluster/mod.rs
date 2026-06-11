//! Orchestrator↔worker WebSocket control plane.
//!
//! Workers run on hosts that cannot reach Postgres; they only talk to the
//! orchestrator. The orchestrator owns the DB + pgmq and *proxies* the work over
//! a WebSocket it serves on its **own web port** (`/cluster`) — so it rides the
//! same axum server and a TLS terminator (e.g. Cloudflare) can front it with no
//! extra port. It reads the queue, pushes each job to a connected worker, and
//! writes the result back. This module holds the wire messages ([`message`]), the
//! orchestrator-side dispatcher ([`server`]), and the worker-side client
//! ([`client`]).
//!
//! The unit of work is the same coarse call the in-process worker makes —
//! `analyze(sgf) → GameAnalysis` — so both the monolith (`standalone` role) and the
//! orchestrator drive the *identical* [`crate::worker::worker_loop`]; only the
//! [`crate::worker::Executor`] differs (local engine vs. remote socket).

pub mod client;
pub mod message;
pub mod registry;
pub mod server;
