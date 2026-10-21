//! CPU-bound work off the async runtime.
//!
//! The runtime is single-threaded (see `main`), so anything that holds the CPU
//! for milliseconds stalls every other task — requests, the cluster socket,
//! lease heartbeats. An analysis payload is ~0.35 MB of JSON on average and up to
//! ~2 MB; (de)serializing one takes a few milliseconds, so those steps run here.
//! Small JSON (queries, KataGo's per-turn lines, request bodies) stays inline:
//! the hand-off would cost more than it saves.

/// Run `work` on tokio's blocking pool and wait for it. A panic in `work`
/// resumes on the caller, as if it had run inline.
pub async fn offload<T, F>(work: F) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(value) => value,
        Err(err) => std::panic::resume_unwind(err.into_panic()),
    }
}
