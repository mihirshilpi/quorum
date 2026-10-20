use quorum_store::{Changed, Store, tables};
use tokio::sync::broadcast::error::TryRecvError;

use crate::common::{chat_message, message};

#[tokio::test]
async fn one_coalesced_notification_per_transaction_after_commit() {
    let store = Store::open_in_memory().unwrap();
    let mut rx = store.subscribe();
    store
        .write(|tx| {
            tx.upsert_messages(&[message("m1", "inbox", "2026-01-01T00:00:00Z", "a")])?;
            tx.upsert_messages(&[message("m2", "inbox", "2026-01-01T00:00:00Z", "b")])?;
            tx.upsert_chat_messages(&[chat_message("c1", "x", "2026-01-01T00:00:00Z", "hi")])?;
            tx.set_setting("k", &1)
        })
        .await
        .unwrap();
    let got = rx.try_recv().unwrap();
    assert_eq!(got, Changed { tables: vec![tables::CHAT_MESSAGE, tables::MESSAGE, tables::SETTING] });
    assert!(got.touches("message"));
    assert_eq!(rx.try_recv().unwrap_err(), TryRecvError::Empty);

    // Readers see the data when the notification arrives.
    let page =
        store.list_messages(quorum_store::MessageQuery::folder(quorum_model::MailFolderId::new("inbox").unwrap()));
    assert_eq!(page.await.unwrap().items.len(), 2);
}

#[tokio::test]
async fn rolled_back_and_empty_transactions_notify_nothing() {
    let store = Store::open_in_memory().unwrap();
    let mut rx = store.subscribe();
    let _ = store
        .write(|tx| {
            tx.upsert_messages(&[message("m1", "inbox", "2026-01-01T00:00:00Z", "a")])?;
            Err::<(), _>(quorum_store::Error::new(quorum_store::ErrorKind::Bug, "abort"))
        })
        .await;
    store.write(|tx| tx.upsert_messages(&[])).await.unwrap();
    store.read(|r| r.unread_counts()).await.unwrap();
    assert_eq!(rx.try_recv().unwrap_err(), TryRecvError::Empty);
    assert!(store.message(quorum_model::MessageId::new("m1").unwrap()).await.unwrap().is_none());
}
