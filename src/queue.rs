//! pgmq helpers: the job message type and thin send/read/archive wrappers over
//! the Diesel pool. pgmq is installed at startup by `PgmqPlugin`; the `Queue`
//! trait methods run on a `&mut AsyncPgConnection` from that same pool.

use std::time::Duration;

use muxa::diesel::DieselPool;
use pgmq::{Message, Queue as _};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{AppError, AppResult};

/// The message queues this service uses — the single source of truth for queue
/// names. `PgmqPlugin` creates them at startup and the worker consumes them, so
/// both refer to a variant here rather than a bare string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Queue {
    /// The SGF analysis work queue.
    Analysis,
}

impl Queue {
    /// The pgmq queue name this variant maps to.
    pub fn as_str(self) -> &'static str {
        match self {
            Queue::Analysis => "analysis",
        }
    }
}

/// Body of an analysis-queue message. The queue is the authority for in-flight
/// work, so the message carries the **SGF** itself: a crash can replay the work
/// straight from the durable (logged) queue, with no other store needed.
#[derive(Debug, Serialize, Deserialize)]
pub struct JobMessage {
    /// The job id (the `jobs` row this message's work belongs to).
    pub job_id: Uuid,
    /// The SGF to analyze.
    pub sgf: String,
}

fn q_err<E: std::fmt::Display>(err: E) -> AppError {
    AppError::Queue(err.to_string())
}

/// Long-poll for one message, making it invisible for `vt_secs` while in flight.
pub async fn read_one(
    pool: &DieselPool,
    queue: Queue,
    vt_secs: i32,
    poll: Duration,
) -> AppResult<Option<Message<JobMessage>>> {
    let mut conn = pool.0.get().await.map_err(q_err)?;
    let msg = (&mut *conn)
        .read_with_poll::<JobMessage>(queue.as_str(), vt_secs, Some(poll), None)
        .await
        .map_err(q_err)?;
    Ok(msg)
}

/// Archive a processed message (removes it from the active queue).
pub async fn archive(pool: &DieselPool, queue: Queue, msg_id: i64) -> AppResult<()> {
    let mut conn = pool.0.get().await.map_err(q_err)?;
    (&mut *conn)
        .archive(queue.as_str(), msg_id)
        .await
        .map_err(q_err)?;
    Ok(())
}

/// Extend an in-flight message's visibility timeout to `vt_secs` from now.
///
/// Re-arms the lock: the message stays invisible to other consumers for another
/// `vt_secs`. Used as a heartbeat so a job that runs longer than the original
/// timeout isn't redelivered (and double-processed) while still being worked on.
pub async fn extend_vt(pool: &DieselPool, queue: Queue, msg_id: i64, vt_secs: i32) -> AppResult<()> {
    let mut conn = pool.0.get().await.map_err(q_err)?;
    (&mut *conn)
        .set_vt::<JobMessage>(queue.as_str(), msg_id, vt_secs)
        .await
        .map_err(q_err)?;
    Ok(())
}
