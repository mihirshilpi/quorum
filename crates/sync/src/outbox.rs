//! The outbox replayer: claims queued writes in order and replays them through the matching
//! [`crate::OutboxHandler`].
//!
//! Failure mapping: [`ErrorKind::Permanent`], [`ErrorKind::Invalid`] and [`ErrorKind::Bug`]
//! (including a caught panic) fail the entry at once for the user to Retry or Discard;
//! [`ErrorKind::Throttled`] retries after the server's `Retry-After`; other errors retry with
//! the store's backoff; [`ErrorKind::Auth`] puts the entry back untouched and pauses sync. An
//! entry whose kind has no handler fails permanently. After a success the entry's resource key
//! (and any keys listed in the handler's result under `"poke"`) are poked so fresh server state
//! is pulled.

use std::sync::Arc;
use std::time::Duration;

use quorum_store::{OutboxEntry, OutboxFailure, OutboxId};
use serde_json::Value;

use crate::engine::{DiscardOutcome, Shared, kind_name};
use crate::error::{ErrorKind, Result};
use crate::guard::guarded;
use crate::schedule;

/// Most extra keys a handler result may ask us to poke.
const MAX_RESULT_POKES: usize = 16;

/// Replays the outbox until the engine stops.
pub(crate) async fn replay(shared: Arc<Shared>) {
    let mut rx = shared.control.subscribe();
    let mut store_failures: u32 = 0;
    shared.refresh_outbox_counts().await;
    loop {
        let ctl = rx.borrow_and_update().clone();
        if ctl.stopped {
            return;
        }
        if ctl.halted() {
            if rx.changed().await.is_err() {
                return;
            }
            continue;
        }
        let wait = match shared.ctx.store.outbox_claim_next(shared.now_utc()).await {
            Ok(Some(entry)) => {
                store_failures = 0;
                process(&shared, entry).await;
                shared.refresh_outbox_counts().await;
                tokio::task::yield_now().await;
                continue;
            }
            Ok(None) => {
                store_failures = 0;
                shared.refresh_outbox_counts().await;
                next_ready_in(&shared).await
            }
            Err(e) => {
                store_failures = store_failures.saturating_add(1);
                tracing::warn!(name: "sync.outbox_claim_failed", error = %e, "cannot claim outbox entry");
                schedule::backoff(store_failures, shared.config.base_backoff, shared.config.max_backoff)
            }
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = shared.outbox_wake.notified() => {}
            r = rx.changed() => if r.is_err() { return; },
        }
    }
}

/// How long until some entry becomes claimable (at most the poll interval).
async fn next_ready_in(shared: &Shared) -> Duration {
    let poll = shared.config.outbox_poll.max(Duration::from_secs(1));
    match shared.ctx.store.read(|r| r.outbox_next_ready_at()).await {
        Ok(Some(at)) => {
            let now = shared.now_utc();
            let ms = at.as_millisecond().saturating_sub(now.as_millisecond());
            let ms = u64::try_from(ms).unwrap_or(0);
            // Entries are stored with millisecond precision; wait at least a moment.
            Duration::from_millis(ms.max(10)).min(poll)
        }
        Ok(None) => poll,
        Err(_) => poll,
    }
}

async fn fail(shared: &Shared, id: OutboxId, failure: OutboxFailure) {
    if let Err(e) = shared.ctx.store.outbox_fail(id, failure, shared.now_utc()).await {
        tracing::warn!(name: "sync.outbox_fail_failed", error = %e, "cannot record outbox failure");
    }
}

/// Replays one claimed entry.
async fn process(shared: &Shared, entry: OutboxEntry) {
    let id = entry.id;
    let Some(handler) = shared.handlers.get(entry.kind.as_str()).cloned() else {
        tracing::warn!(name: "sync.outbox_unknown_kind", "no handler for outbox entry kind");
        let failure =
            OutboxFailure { error: "no handler for this kind of write".into(), retry_after: None, permanent: true };
        fail(shared, id, failure).await;
        return;
    };
    let result = guarded("outbox handler", handler.apply(&shared.ctx, entry.payload)).await;
    match result {
        Ok(value) => {
            shared.note_reachability(Ok(()));
            if let Err(e) = shared.ctx.store.outbox_complete(id).await {
                tracing::warn!(name: "sync.outbox_complete_failed", error = %e, "cannot complete outbox entry");
            }
            shared.wake_matching(&entry.resource);
            for key in result_pokes(&value) {
                shared.wake_matching(&key);
            }
        }
        Err(e) => {
            let kind = e.kind();
            shared.note_reachability(Err(kind));
            tracing::warn!(name: "sync.outbox_failed", kind = kind_name(kind), "outbox replay failed");
            let error = format!("{}: {}", kind_name(kind), e.message());
            match kind {
                ErrorKind::Auth => {
                    // Back in the queue as it was; replay resumes after sign-in.
                    if let Err(se) = shared.ctx.store.outbox_retry(id).await {
                        tracing::warn!(name: "sync.outbox_unclaim_failed", error = %se, "cannot requeue outbox entry");
                    }
                    shared.pause_for_auth();
                }
                ErrorKind::Throttled => {
                    let retry_after = e.retry_after().map(schedule::clamp_retry_after);
                    fail(shared, id, OutboxFailure { error, retry_after, permanent: false }).await;
                }
                ErrorKind::Permanent | ErrorKind::Invalid | ErrorKind::Bug => {
                    fail(shared, id, OutboxFailure { error, retry_after: None, permanent: true }).await;
                }
                ErrorKind::Remote | ErrorKind::Store => {
                    fail(shared, id, OutboxFailure { error, retry_after: None, permanent: false }).await;
                }
            }
        }
    }
}

/// Keys a handler asked to poke: `{"poke": "key"}` or `{"poke": ["k1", "k2"]}`.
fn result_pokes(v: &Value) -> Vec<String> {
    match v.get("poke") {
        Some(Value::String(s)) if !s.is_empty() => vec![s.chars().take(1024).collect()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .filter(|s| !s.is_empty())
            .take(MAX_RESULT_POKES)
            .map(|s| s.chars().take(1024).collect())
            .collect(),
        _ => Vec::new(),
    }
}

/// See [`crate::EngineHandle::discard_failed`].
pub(crate) async fn discard(shared: &Shared, id: OutboxId) -> Result<DiscardOutcome> {
    let failed = shared.ctx.store.outbox_failed(quorum_store::MAX_PAGE).await?;
    let Some(entry) = failed.items.into_iter().find(|e| e.id == id) else {
        return Ok(DiscardOutcome::default());
    };
    let mut reverted = false;
    if !entry.payload.is_null()
        && let Some(handler) = shared.handlers.get(entry.kind.as_str()).cloned()
    {
        match guarded("outbox revert", handler.revert(&shared.ctx, entry.payload)).await {
            Ok(()) => reverted = true,
            Err(e) => {
                tracing::warn!(name: "sync.outbox_revert_failed", kind = kind_name(e.kind()), "revert failed; discarding anyway")
            }
        }
    }
    // `None` here only means the payload was undecodable or a concurrent discard won; the
    // entry is gone either way.
    let _removed = shared.ctx.store.outbox_discard(id).await?;
    shared.refresh_outbox_counts().await;
    shared.outbox_wake.notify_one();
    Ok(DiscardOutcome { discarded: true, reverted })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn result_pokes_accepts_string_or_list_and_ignores_junk() {
        assert_eq!(result_pokes(&json!({"poke": "mail.folder:1"})), vec!["mail.folder:1".to_owned()]);
        assert_eq!(result_pokes(&json!({"poke": ["a", 5, "", "b"]})), vec!["a".to_owned(), "b".to_owned()]);
        assert!(result_pokes(&json!({"poke": ""})).is_empty());
        assert!(result_pokes(&json!(null)).is_empty());
        assert_eq!(result_pokes(&json!({"poke": vec!["k"; 100]})).len(), MAX_RESULT_POKES);
    }
}
