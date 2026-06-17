//! In-memory registry of currently-connected workers (orchestrator only).
//!
//! Follows the codebase's channel/actor convention (see the KataGo client in
//! [`crate::engine::katago`]): one owner task exclusively holds the map of
//! connected workers, and the gRPC sessions and the HTTP `/workers` handler touch
//! it only by message-passing — no `Mutex`/`Arc<Mutex>`. A session [`register`]s
//! on connect and the returned [`WorkerGuard`] deregisters it on drop, so a
//! closed stream, a cancellation, or a panic all clean up the entry. The handler
//! asks for a [`snapshot`].
//!
//! This is *per-process* state: it lists the workers holding a live session to
//! *this* orchestrator. Under multiple orchestrator replicas each instance sees
//! only its own — the `/workers` endpoint is therefore an instance-local view.
//!
//! [`register`]: WorkerRegistry::register
//! [`snapshot`]: WorkerRegistry::snapshot

use std::collections::HashMap;
use std::net::SocketAddr;

use chrono::{DateTime, Utc};
use muxa::prelude::*;
use tokio::sync::{mpsc, oneshot};

/// Per-connection session id, minted by the owner task. Job ids are UUIDs minted
/// by Postgres, but a worker session never touches the DB, so a process-local
/// monotonic counter (owned solely by [`owner_loop`], hence no atomics) is the
/// natural fit and keeps `uuid` to its `serde`-only feature set.
pub type SessionId = u64;

/// Buffer for the command channel. Register/deregister happen once per worker
/// connection — a rare event — so this is generous headroom; it mainly matters
/// for the guard's non-blocking [`try_send`](mpsc::Sender::try_send) on drop.
const CMD_BUFFER: usize = 128;

/// One connected worker, as the orchestrator sees it.
#[derive(Clone, Debug)]
pub struct WorkerInfo {
    /// Session id — minted per connection by the orchestrator. Not stable across
    /// a worker's reconnects (the worker announces a name + slots, not an id).
    pub id: SessionId,
    /// The name the worker registered with (from its `Hello`).
    pub name: String,
    /// Peer socket address, when the transport exposed one.
    pub peer: Option<SocketAddr>,
    /// Slots (max concurrent jobs) the worker advertised in its `Hello`.
    pub slots: u32,
    /// When the session connected (UTC).
    pub connected_at: DateTime<Utc>,
}

/// Messages the owner task accepts.
enum Cmd {
    Register {
        name: String,
        peer: Option<SocketAddr>,
        slots: u32,
        /// The owner replies with the freshly-minted session id.
        reply: oneshot::Sender<SessionId>,
    },
    Deregister(SessionId),
    Snapshot(oneshot::Sender<Vec<WorkerInfo>>),
}

/// Cheap, cloneable handle to the registry actor (just an `mpsc::Sender`).
#[derive(Clone)]
pub struct WorkerRegistry {
    tx: mpsc::Sender<Cmd>,
}

impl WorkerRegistry {
    /// Spawn the owner task (bound to the app's shutdown token) and return a
    /// handle. The task exits on shutdown or once every handle is dropped.
    pub fn spawn(ctx: &mut BuildCtx) -> Self {
        let (tx, rx) = mpsc::channel(CMD_BUFFER);
        ctx.tasks
            .spawn("worker-registry", move |shutdown| owner_loop(rx, shutdown));
        Self { tx }
    }

    /// Record a connected worker (the owner stamps `connected_at` and mints the
    /// session id); the returned guard deregisters it on drop.
    ///
    /// Best-effort: if the registry task has already stopped (shutdown), the guard
    /// carries id `0` and its drop is a no-op.
    pub async fn register(&self, name: String, peer: Option<SocketAddr>, slots: u32) -> WorkerGuard {
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(Cmd::Register { name, peer, slots, reply })
            .await
            .is_err()
        {
            return WorkerGuard { tx: self.tx.clone(), id: 0 };
        }
        let id = rx.await.unwrap_or(0);
        WorkerGuard { tx: self.tx.clone(), id }
    }

    /// Snapshot the currently-connected workers, oldest connection first. Returns
    /// empty if the registry task is gone.
    pub async fn snapshot(&self) -> Vec<WorkerInfo> {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Cmd::Snapshot(reply)).await.is_err() {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }
}

/// The owner task: exclusively holds the worker map (and the id counter) and
/// serves it over the channel.
async fn owner_loop(mut rx: mpsc::Receiver<Cmd>, shutdown: ShutdownToken) {
    let mut workers: HashMap<SessionId, WorkerInfo> = HashMap::new();
    let mut next_id: SessionId = 1;
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            cmd = rx.recv() => match cmd {
                None => break, // every handle dropped
                Some(Cmd::Register { name, peer, slots, reply }) => {
                    let id = next_id;
                    next_id += 1;
                    workers.insert(id, WorkerInfo { id, name, peer, slots, connected_at: Utc::now() });
                    let _ = reply.send(id); // caller may have been cancelled — fine
                }
                Some(Cmd::Deregister(id)) => {
                    workers.remove(&id);
                }
                Some(Cmd::Snapshot(reply)) => {
                    let mut list: Vec<WorkerInfo> = workers.values().cloned().collect();
                    // Sort by the monotonic id = connection order (deterministic;
                    // `connected_at` can tie within a millisecond).
                    list.sort_by_key(|w| w.id);
                    drop(reply.send(list)); // receiver may have timed out — fine
                }
            },
        }
    }
}

/// RAII guard returned by [`WorkerRegistry::register`]: deregisters its worker
/// when dropped (stream end, connection cancellation, or panic). Uses a
/// non-blocking [`try_send`](mpsc::Sender::try_send) since `Drop` can't await; the
/// [`CMD_BUFFER`] headroom makes a miss vanishingly unlikely at the real
/// connect/disconnect rate, and on shutdown a missed deregister is moot.
pub struct WorkerGuard {
    tx: mpsc::Sender<Cmd>,
    id: SessionId,
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        drop(self.tx.try_send(Cmd::Deregister(self.id)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spawn an owner task and hand back a handle wired to it (bypasses
    /// `BuildCtx`, which a unit test has no access to).
    fn registry() -> WorkerRegistry {
        let (tx, rx) = mpsc::channel(CMD_BUFFER);
        tokio::spawn(owner_loop(rx, ShutdownToken::new()));
        WorkerRegistry { tx }
    }

    #[tokio::test]
    async fn registers_then_guard_drop_deregisters() {
        let registry = registry();
        assert!(registry.snapshot().await.is_empty());

        // Two connects: monotonic ids, oldest first, advertised slots preserved.
        let g1 = registry.register("alpha".to_owned(), None, 4).await;
        let g2 = registry.register("beta".to_owned(), None, 8).await;
        let snap = registry.snapshot().await;
        assert_eq!(snap.iter().map(|w| (w.id, w.slots)).collect::<Vec<_>>(), vec![(1, 4), (2, 8)]);

        // Dropping a guard deregisters that worker. The `Deregister` is enqueued
        // (try_send) before the following `Snapshot`, and the owner processes the
        // channel in order, so one snapshot already reflects the removal.
        drop(g1);
        let snap = registry.snapshot().await;
        assert_eq!(snap.iter().map(|w| w.id).collect::<Vec<_>>(), vec![2]);

        drop(g2);
        assert!(registry.snapshot().await.is_empty());
    }
}
