use std::time::Duration;

use quorum_store::{MAX_OUTBOX_ATTEMPTS, NewOutboxEntry, OutboxFailure, OutboxState, Store};

use crate::common::{message, open, t};

fn entry(kind: &str, resource: &str, n: u32) -> NewOutboxEntry {
    NewOutboxEntry {
        kind: kind.into(),
        resource: resource.into(),
        local_id: Some(format!("local-{n}")),
        payload: serde_json::json!({"n": n}),
    }
}

fn n_of(e: &quorum_store::OutboxEntry) -> u64 {
    e.payload["n"].as_u64().unwrap()
}

#[tokio::test]
async fn ordered_per_resource_and_parallel_across() {
    let store = Store::open_in_memory().unwrap();
    store.outbox_enqueue(entry("mail.flag", "message:a", 1)).await.unwrap();
    store.outbox_enqueue(entry("mail.move", "message:a", 2)).await.unwrap();
    store.outbox_enqueue(entry("chat.send", "chat:b", 3)).await.unwrap();
    let now = t("2026-01-01T00:00:00Z");
    let first = store.outbox_claim_next(now).await.unwrap().unwrap();
    assert_eq!(n_of(&first), 1);
    assert_eq!(first.state, OutboxState::Claimed);
    // Entry 2 waits behind claimed entry 1; entry 3 (another resource) is ready.
    let second = store.outbox_claim_next(now).await.unwrap().unwrap();
    assert_eq!(n_of(&second), 3);
    assert!(store.outbox_claim_next(now).await.unwrap().is_none());
    store.outbox_complete(first.id).await.unwrap();
    let third = store.outbox_claim_next(now).await.unwrap().unwrap();
    assert_eq!(n_of(&third), 2);
    assert_eq!(third.local_id.as_deref(), Some("local-2"));
}

#[tokio::test]
async fn backoff_then_failed_then_retry_or_discard() {
    let store = Store::open_in_memory().unwrap();
    let id = store.outbox_enqueue(entry("mail.send", "message:a", 1)).await.unwrap();
    store.outbox_enqueue(entry("mail.flag", "message:a", 2)).await.unwrap();
    let mut now = t("2026-01-01T00:00:00Z");
    let claimed = store.outbox_claim_next(now).await.unwrap().unwrap();
    assert_eq!(claimed.id, id);
    let state = store
        .outbox_fail(id, OutboxFailure { error: "ServiceUnavailable".into(), ..Default::default() }, now)
        .await
        .unwrap();
    assert_eq!(state, OutboxState::Pending);
    // Not ready during the backoff, and the later entry for the same resource stays blocked.
    assert!(store.outbox_claim_next(now).await.unwrap().is_none());
    let ready_at = store.read(|r| r.outbox_next_ready_at()).await.unwrap().unwrap();
    assert_eq!(ready_at, now + quorum_store::backoff(1));
    now = ready_at;
    assert_eq!(store.outbox_claim_next(now).await.unwrap().unwrap().id, id);
    // Retry-After overrides a shorter backoff.
    store
        .outbox_fail(
            id,
            OutboxFailure {
                error: "TooManyRequests".into(),
                retry_after: Some(Duration::from_secs(120)),
                permanent: false,
            },
            now,
        )
        .await
        .unwrap();
    assert!(store.outbox_claim_next(now + Duration::from_secs(119)).await.unwrap().is_none());
    now += Duration::from_secs(120);
    // Exhaust attempts.
    let mut state = OutboxState::Pending;
    for _ in 2..MAX_OUTBOX_ATTEMPTS {
        let c = store.outbox_claim_next(now + Duration::from_secs(3600)).await.unwrap().unwrap();
        assert_eq!(c.id, id);
        state = store.outbox_fail(id, OutboxFailure { error: "x".into(), ..Default::default() }, now).await.unwrap();
    }
    assert_eq!(state, OutboxState::Failed);
    let failed = store.outbox_failed(10).await.unwrap();
    assert_eq!(failed.items.len(), 1);
    assert_eq!(failed.items[0].attempts, MAX_OUTBOX_ATTEMPTS);
    assert_eq!(failed.items[0].last_error.as_deref(), Some("x"));
    // A failed entry still blocks its resource.
    assert!(store.outbox_claim_next(now + Duration::from_secs(36_000)).await.unwrap().is_none());
    // Retry puts it back immediately.
    store.outbox_retry(id).await.unwrap();
    assert_eq!(store.outbox_claim_next(now).await.unwrap().unwrap().attempts, 0);
    // Permanent failure goes straight to failed; discard unblocks the next entry.
    let s = store
        .outbox_fail(id, OutboxFailure { error: "BadRequest".into(), permanent: true, ..Default::default() }, now)
        .await
        .unwrap();
    assert_eq!(s, OutboxState::Failed);
    let discarded = store.outbox_discard(id).await.unwrap().unwrap();
    assert_eq!(n_of(&discarded), 1);
    assert_eq!(n_of(&store.outbox_claim_next(now).await.unwrap().unwrap()), 2);
    assert!(store.outbox_discard(id).await.unwrap().is_none());
}

#[tokio::test]
async fn survives_restart_and_reclaims_in_flight() {
    let dir = tempfile::tempdir().unwrap();
    let now = t("2026-01-01T00:00:00Z");
    {
        let store = open(dir.path()).await;
        store.outbox_enqueue(entry("mail.send", "message:a", 1)).await.unwrap();
        store.outbox_enqueue(entry("mail.send", "message:b", 2)).await.unwrap();
        // Claimed but the process "crashes" before completing.
        assert_eq!(n_of(&store.outbox_claim_next(now).await.unwrap().unwrap()), 1);
    }
    let store = open(dir.path()).await;
    assert_eq!(store.read(|r| r.outbox_pending_count()).await.unwrap(), 2);
    assert_eq!(n_of(&store.outbox_claim_next(now).await.unwrap().unwrap()), 1);
    assert_eq!(n_of(&store.outbox_claim_next(now).await.unwrap().unwrap()), 2);
}

#[tokio::test]
async fn enqueue_and_optimistic_apply_are_atomic() {
    let store = Store::open_in_memory().unwrap();
    store.apply_message_delta(vec![message("m1", "inbox", "2026-01-01T00:00:00Z", "s")], vec![]).await.unwrap();
    // A failing closure rolls back both the optimistic change and the enqueue.
    let err = store
        .write(|tx| {
            tx.set_message_read(&quorum_model::MessageId::new("m1")?, true)?;
            tx.outbox_enqueue(&entry("mail.read", "message:m1", 1))?;
            Err::<(), _>(quorum_store::Error::new(quorum_store::ErrorKind::Bug, "simulated"))
        })
        .await;
    assert!(err.is_err());
    assert!(!store.message(quorum_model::MessageId::new("m1").unwrap()).await.unwrap().unwrap().is_read);
    assert_eq!(store.read(|r| r.outbox_pending_count()).await.unwrap(), 0);
    store
        .write(|tx| {
            tx.set_message_read(&quorum_model::MessageId::new("m1")?, true)?;
            tx.outbox_enqueue(&entry("mail.read", "message:m1", 1))
        })
        .await
        .unwrap();
    assert!(store.message(quorum_model::MessageId::new("m1").unwrap()).await.unwrap().unwrap().is_read);
    assert_eq!(store.read(|r| r.outbox_pending_count()).await.unwrap(), 1);
}

#[tokio::test]
async fn corrupt_entry_is_failed_not_stuck() {
    let store = Store::open_in_memory().unwrap();
    store.outbox_enqueue(entry("mail.send", "message:a", 1)).await.unwrap();
    store.outbox_enqueue(entry("mail.send", "message:b", 2)).await.unwrap();
    quorum_store::testing::execute(&store, "UPDATE outbox SET payload = x'00' WHERE resource = 'message:a'")
        .await
        .unwrap();
    let next = store.outbox_claim_next(t("2026-01-01T00:00:00Z")).await.unwrap().unwrap();
    assert_eq!(n_of(&next), 2);
    let failed = store.outbox_failed(10).await.unwrap();
    assert_eq!(failed.items.len(), 1);
    assert!(failed.items[0].payload.is_null());
    assert!(store.corrupt_rows_skipped() >= 1);
}

#[tokio::test]
async fn rejects_bad_fields() {
    let store = Store::open_in_memory().unwrap();
    let err = store.outbox_enqueue(entry("", "message:a", 1)).await.unwrap_err();
    assert_eq!(err.kind(), quorum_store::ErrorKind::InvalidInput);
    let err = store.outbox_complete(quorum_store::OutboxId(999)).await.unwrap_err();
    assert_eq!(err.kind(), quorum_store::ErrorKind::NotFound);
}

#[tokio::test]
async fn delayed_enqueue_and_undo_cannot_race_a_claim() {
    let store = Store::open_in_memory().unwrap();
    let now = t("2026-01-01T00:00:00Z");
    let send_at = now + Duration::from_secs(10);
    let id = store.outbox_enqueue_at(entry("mail.send", "message:a", 1), send_at).await.unwrap();
    assert!(store.outbox_claim_next(now).await.unwrap().is_none(), "held until not_before");
    assert_eq!(store.read(|r| r.outbox_next_ready_at()).await.unwrap(), Some(send_at));
    // Undo within the window withdraws it.
    assert_eq!(n_of(&store.outbox_discard_pending(id).await.unwrap().unwrap()), 1);
    assert_eq!(store.read(|r| r.outbox_pending_count()).await.unwrap(), 0);
    // Once claimed, undo is refused and the entry stays.
    let id = store.outbox_enqueue_at(entry("mail.send", "message:b", 2), send_at).await.unwrap();
    assert_eq!(store.outbox_claim_next(send_at).await.unwrap().unwrap().id, id);
    assert!(store.outbox_discard_pending(id).await.unwrap().is_none());
    assert_eq!(store.read(|r| r.outbox_pending_count()).await.unwrap(), 1);
}
