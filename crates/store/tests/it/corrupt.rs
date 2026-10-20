use quorum_model::{ChatId, MailFolderId};
use quorum_store::{MessageQuery, Store};

use crate::common::{chat_message, message};

#[tokio::test]
async fn corrupt_rows_are_skipped_and_counted() {
    let store = Store::open_in_memory().unwrap();
    store
        .apply_message_delta(
            vec![
                message("m1", "inbox", "2026-01-01T00:00:00Z", "a"),
                message("m2", "inbox", "2026-01-02T00:00:00Z", "b"),
                message("m3", "inbox", "2026-01-03T00:00:00Z", "c"),
            ],
            vec![],
        )
        .await
        .unwrap();
    quorum_store::testing::execute(&store, "UPDATE message SET json = '{not json' WHERE id = 'm2'").await.unwrap();
    quorum_store::testing::execute(&store, "UPDATE message SET json = '{\"id\":\"\"}' WHERE id = 'm3'").await.unwrap();
    let mut q = MessageQuery::folder(MailFolderId::new("inbox").unwrap());
    q.limit = 1;
    // Paging advances past corrupt rows instead of stalling.
    let mut ok = Vec::new();
    let mut skipped = 0;
    for _ in 0..10 {
        let page = store.list_messages(q.clone()).await.unwrap();
        ok.extend(page.items.iter().map(|m| m.id.as_str().to_owned()));
        skipped += page.skipped;
        match page.next {
            Some(c) => q.after = Some(c),
            None => break,
        }
    }
    assert_eq!(ok, ["m1"]);
    assert_eq!(skipped, 2);
    assert!(store.corrupt_rows_skipped() >= 2);
    let err = store.message(quorum_model::MessageId::new("m2").unwrap()).await.unwrap_err();
    assert_eq!(err.kind(), quorum_store::ErrorKind::Corrupt);
    // Search skips them too.
    let res = store.search("preview".into(), vec![quorum_store::SearchScope::Mail], 10).await.unwrap();
    assert_eq!(res.hits.len(), 1);
    assert_eq!(res.skipped, 2);

    store.upsert_chat_messages(vec![chat_message("c1", "x1", "2026-01-01T00:00:00Z", "hi")]).await.unwrap();
    quorum_store::testing::execute(&store, "UPDATE chat_message SET body = x'01'").await.unwrap();
    let page = store.chat_messages(ChatId::new("c1").unwrap(), None, 10).await.unwrap();
    assert_eq!((page.items.len(), page.skipped), (0, 1));
}

#[tokio::test]
async fn hostile_model_values_round_trip() {
    let store = Store::open_in_memory().unwrap();
    let mut m = message("m1", "inbox", "2026-01-01T00:00:00Z", "");
    m.subject = "\u{0}\u{202e}<script>alert(1)</script>'; DROP TABLE message;--".into();
    m.categories = vec!["\"quoted\"".into(), "".into()];
    m.received_date_time = None;
    store.apply_message_delta(vec![m.clone()], vec![]).await.unwrap();
    assert_eq!(store.message(m.id.clone()).await.unwrap().unwrap(), m);
    let page = store.list_messages(MessageQuery::folder(MailFolderId::new("inbox").unwrap())).await.unwrap();
    assert_eq!(page.items.len(), 1);
}
