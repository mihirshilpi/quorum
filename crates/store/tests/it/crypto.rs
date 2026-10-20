use quorum_model::{BodyType, ChatId, MessageBody, MessageId};
use quorum_store::{Draft, DraftKind, NewOutboxEntry};

use crate::common::{chat_message, message, open};

const SECRET: &str = "Zebra quartz pineapple seventeen lighthouse";

fn files_contain(dir: &std::path::Path, needle: &[u8]) -> Vec<String> {
    let mut hits = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let bytes = std::fs::read(&p).unwrap();
                if bytes.windows(needle.len()).any(|w| w == needle) {
                    hits.push(p.display().to_string());
                }
            }
        }
    }
    hits
}

#[tokio::test]
async fn bodies_drafts_outbox_and_blobs_are_encrypted_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path()).await;
    store.apply_message_delta(vec![message("m1", "inbox", "2026-01-01T00:00:00Z", "harmless")], vec![]).await.unwrap();
    store
        .put_message_body(
            MessageId::new("m1").unwrap(),
            MessageBody { content_type: BodyType::Html, content: format!("<p>{SECRET}</p>").into() },
        )
        .await
        .unwrap();
    store.upsert_chat_messages(vec![chat_message("c1", "x1", "2026-01-01T00:00:00Z", SECRET)]).await.unwrap();
    let mut draft = Draft::new("d1", DraftKind::Mail);
    draft.body = SECRET.into();
    draft.fields = serde_json::json!({"subject": SECRET});
    store.save_draft(draft).await.unwrap();
    store
        .outbox_enqueue(NewOutboxEntry {
            kind: "mail.send".into(),
            resource: "message:m1".into(),
            local_id: None,
            payload: serde_json::json!({"body": SECRET}),
        })
        .await
        .unwrap();
    store.put_blob("att:1".into(), SECRET.as_bytes().to_vec()).await.unwrap();

    // Check both before and after a WAL checkpoint.
    assert!(files_contain(dir.path(), SECRET.as_bytes()).is_empty());
    quorum_store::testing::execute_file(dir.path(), "PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    assert!(files_contain(dir.path(), SECRET.as_bytes()).is_empty());
    // The single words are in the (contentless) search index, but no run of plaintext is.
    assert!(files_contain(dir.path(), b"quartz pineapple").is_empty());
    drop(store);

    // Everything decrypts after reopening with the same key.
    let store = open(dir.path()).await;
    let body = store.message_body(MessageId::new("m1").unwrap()).await.unwrap().unwrap();
    assert!(body.content.as_str().contains(SECRET));
    let msgs = store.chat_messages(ChatId::new("c1").unwrap(), None, 10).await.unwrap();
    assert!(msgs.items[0].body.content.as_str().contains(SECRET));
    assert_eq!(store.draft("d1".into()).await.unwrap().unwrap().body.as_str(), SECRET);
    assert_eq!(store.get_blob("att:1".into()).await.unwrap().unwrap(), SECRET.as_bytes());
    let claimed = store.outbox_claim_next(jiff::Timestamp::now()).await.unwrap().unwrap();
    assert_eq!(claimed.payload["body"], SECRET);
}

#[tokio::test]
async fn debug_output_never_contains_content() {
    let mut draft = Draft::new("d1", DraftKind::Chat);
    draft.body = SECRET.into();
    draft.fields = serde_json::json!({"to": SECRET});
    let entry = NewOutboxEntry {
        kind: "chat.send".into(),
        resource: "chat:c1".into(),
        local_id: None,
        payload: serde_json::json!({"text": SECRET}),
    };
    let state = quorum_store::DeltaState {
        delta_link: Some(format!("https://graph.microsoft.com/v1.0/x?$deltatoken={SECRET}")),
        ..quorum_store::DeltaState::new("mail:inbox")
    };
    for s in [
        format!("{draft:?}"),
        format!("{entry:?}"),
        format!("{state:?}"),
        format!("{:?}", quorum_store::StoreKey::generate()),
    ] {
        assert!(!s.contains("Zebra"), "leaked: {s}");
    }
}

#[tokio::test]
async fn tampered_ciphertext_is_reported_not_panicked() {
    let store = quorum_store::Store::open_in_memory().unwrap();
    store.apply_message_delta(vec![message("m1", "inbox", "2026-01-01T00:00:00Z", "s")], vec![]).await.unwrap();
    store
        .put_message_body(
            MessageId::new("m1").unwrap(),
            MessageBody { content_type: BodyType::Text, content: "x".into() },
        )
        .await
        .unwrap();
    quorum_store::testing::execute(&store, "UPDATE message_body SET body = x'0102030405'").await.unwrap();
    let err = store.message_body(MessageId::new("m1").unwrap()).await.unwrap_err();
    assert_eq!(err.kind(), quorum_store::ErrorKind::Corrupt);
    quorum_store::testing::execute(&store, "UPDATE message_body SET body = randomblob(80)").await.unwrap();
    let err = store.message_body(MessageId::new("m1").unwrap()).await.unwrap_err();
    assert!(matches!(err.kind(), quorum_store::ErrorKind::Corrupt | quorum_store::ErrorKind::Crypto));
}
