//! Sentry trace boundaries.
//!
//! The `tracing`→Sentry bridge parents a new span on the Sentry span active on
//! the current hub, not on its `tracing` parent: `info_span!(parent: None, …)`
//! inside a background task still lands under that task's long-lived `muxa.task`
//! span — a transaction that only ends at shutdown, so its children never reach
//! Sentry. Work that should be its own transaction runs under [`in_new_trace`];
//! routine background queries that shouldn't be traced at all (idle queue polls,
//! lock heartbeats) run under [`untraced`] — outside any span, each of their
//! queries would otherwise become a transaction of its own.

use sentry::{Hub, SentryFutureExt as _};

/// Run `work` on a fork of the current hub with no active span, so the first
/// span it creates starts a new Sentry transaction (or continues the trace its
/// `sentry.trace` field names).
pub async fn in_new_trace<F: Future>(work: F) -> F::Output {
    let hub = Hub::new_from_top(Hub::current());
    hub.configure_scope(|scope| scope.set_span(None));
    work.bind_hub(hub).await
}

/// Run `work` under a never-sampled transaction, so none of its spans reach
/// Sentry.
pub async fn untraced<F: Future>(work: F) -> F::Output {
    let mut ctx = sentry::TransactionContext::new("untraced", "untraced");
    ctx.set_sampled(false);
    let transaction = sentry::start_transaction(ctx);
    let hub = Hub::new_from_top(Hub::current());
    hub.configure_scope(|scope| scope.set_span(Some(transaction.into())));
    work.bind_hub(hub).await
}

/// The `sentry-trace` header of the active span, for another process to
/// continue this trace (a span's `sentry.trace` field). `None` without an active
/// span — Sentry disabled, or nothing traced here.
pub fn propagation_header() -> Option<String> {
    let span = sentry::configure_scope(|scope| scope.get_span())?;
    span.iter_headers()
        .find(|(name, _)| *name == "sentry-trace")
        .map(|(_, value)| value)
}
