use quorum_model::{Attachment, BodyType, ConversationId, FlagStatus, MailFolderId, MessageBody, MessageId};
use quorum_store::{ErrorKind, MessageQuery, Store};

use crate::common::{folder, message};

fn fid(s: &str) -> MailFolderId {
    MailFolderId::new(s).unwrap()
}

fn mid(s: &str) -> MessageId {
    MessageId::new(s).unwrap()
}

async fn seeded() -> Store {
    let store = Store::open_in_memory().unwrap();
    let mut msgs = Vec::new();
    for i in 0..25u32 {
        let mut m =
            message(&format!("m{i:02}"), "inbox", &format!("2026-03-01T00:{i:02}:00Z"), &format!("subject {i}"));
        m.is_read = i % 2 == 0;
        if i % 5 == 0 {
            m.inference_classification = quorum_model::InferenceClassification::Other;
        }
        msgs.push(m);
    }
    // Two messages with the same received time, to exercise the id tie-break.
    msgs.push(message("tie-a", "inbox", "2026-03-01T00:10:00Z", "tie a"));
    msgs.push(message("tie-b", "inbox", "2026-03-01T00:10:00Z", "tie b"));
    msgs.push(message("x1", "archive", "2026-03-02T00:00:00Z", "archived"));
    store
        .write(move |tx| {
            tx.upsert_mail_folders(&[folder("inbox", "Inbox"), folder("archive", "Archive")])?;
            tx.upsert_messages(&msgs)
        })
        .await
        .unwrap();
    store
}

#[tokio::test]
async fn folder_round_trip() {
    let store = seeded().await;
    let folders = store.mail_folders().await.unwrap();
    assert_eq!(folders.items.len(), 2);
    assert_eq!(folders.items[0].display_name.as_str(), "Inbox");
}

#[tokio::test]
async fn keyset_paging_visits_every_message_once_in_order() {
    let store = seeded().await;
    let mut q = MessageQuery::folder(fid("inbox"));
    q.limit = 4;
    let mut seen = Vec::new();
    for _ in 0..20 {
        let page = store.list_messages(q.clone()).await.unwrap();
        seen.extend(page.items.iter().map(|m| (m.received_date_time.unwrap(), m.id.as_str().to_owned())));
        match page.next {
            Some(c) => q.after = Some(c),
            None => break,
        }
    }
    assert_eq!(seen.len(), 27);
    let mut sorted = seen.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(seen, sorted, "newest first, ties by id desc");
    let ids: std::collections::HashSet<_> = seen.iter().map(|s| s.1.clone()).collect();
    assert_eq!(ids.len(), 27);
}

#[tokio::test]
async fn focused_and_unread_filters() {
    let store = seeded().await;
    let mut q = MessageQuery::folder(fid("inbox"));
    q.limit = 500;
    q.focused = Some(false);
    let other = store.list_messages(q.clone()).await.unwrap();
    assert_eq!(other.items.len(), 5);
    q.focused = None;
    q.unread_only = true;
    let unread = store.list_messages(q.clone()).await.unwrap();
    assert!(unread.items.iter().all(|m| !m.is_read));
    assert_eq!(unread.items.len(), 12 + 2);
    q.focused = Some(true);
    let both = store.list_messages(q).await.unwrap();
    assert!(
        both.items
            .iter()
            .all(|m| !m.is_read && m.inference_classification != quorum_model::InferenceClassification::Other)
    );
}

#[tokio::test]
async fn limit_is_clamped_and_bad_cursor_rejected() {
    let store = seeded().await;
    let mut q = MessageQuery::folder(fid("inbox"));
    q.limit = 0;
    assert_eq!(store.list_messages(q.clone()).await.unwrap().items.len(), 1);
    q.limit = u32::MAX;
    assert_eq!(store.list_messages(q.clone()).await.unwrap().items.len(), 27);
    q.after = Some(quorum_store::Cursor::from_string("garbage"));
    assert_eq!(store.list_messages(q).await.unwrap_err().kind(), ErrorKind::InvalidInput);
}

#[tokio::test]
async fn unread_counts_and_optimistic_changes() {
    let store = seeded().await;
    let counts = store.unread_counts().await.unwrap();
    let inbox = counts.iter().find(|c| c.folder.as_str() == "inbox").unwrap();
    assert_eq!(inbox.unread, 14);
    let changed = store
        .write(|tx| {
            assert!(tx.set_message_read(&mid("m01"), true)?);
            assert!(tx.set_message_flag(&mid("m03"), FlagStatus::Flagged)?);
            assert!(tx.move_message(&mid("m05"), &fid("archive"))?);
            tx.set_message_read(&mid("missing"), true)
        })
        .await
        .unwrap();
    assert!(!changed);
    // m01 is now read and unread m05 moved away.
    let counts = store.unread_counts().await.unwrap();
    assert_eq!(counts.iter().find(|c| c.folder.as_str() == "inbox").unwrap().unread, 12);
    assert!(store.message(mid("m03")).await.unwrap().unwrap().is_flagged());
    assert_eq!(store.message(mid("m05")).await.unwrap().unwrap().parent_folder_id, Some(fid("archive")));
}

#[tokio::test]
async fn conversation_thread_oldest_first() {
    let store = Store::open_in_memory().unwrap();
    let a = message("t-1", "inbox", "2026-01-02T00:00:00Z", "re: plan");
    let b = message("t-2", "sent", "2026-01-01T00:00:00Z", "plan");
    let other = message("u-1", "inbox", "2026-01-03T00:00:00Z", "other");
    store.apply_message_delta(vec![a, b, other], vec![]).await.unwrap();
    let thread = store.conversation(ConversationId::new("conv-t").unwrap()).await.unwrap();
    let ids: Vec<_> = thread.items.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["t-2", "t-1"]);
}

#[tokio::test]
async fn body_and_attachments_round_trip_and_cascade() {
    let store = seeded().await;
    let body = MessageBody { content_type: BodyType::Html, content: "<p>Hello <b>there</b></p>".into() };
    store.put_message_body(mid("m02"), body.clone()).await.unwrap();
    assert_eq!(store.message_body(mid("m02")).await.unwrap(), Some(body));
    assert_eq!(store.message_body(mid("m03")).await.unwrap(), None);
    let err = store.put_message_body(mid("nope"), MessageBody::default()).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotFound);

    let att: Attachment = serde_json::from_value(serde_json::json!({
        "id": "a1", "kind": "file", "name": "report.pdf", "size": 1234, "contentType": "application/pdf"
    }))
    .unwrap();
    store
        .write({
            let att = att.clone();
            move |tx| {
                tx.put_attachments(&mid("m02"), &[att])?;
                tx.set_attachment_blob(&mid("m02"), &quorum_model::AttachmentId::new("a1")?, Some("att:m02:a1"))
            }
        })
        .await
        .unwrap();
    store.put_blob("att:m02:a1".into(), b"pdf bytes".to_vec()).await.unwrap();
    let atts = store.attachments(mid("m02")).await.unwrap();
    assert_eq!(atts.items[0].attachment, att);
    assert_eq!(atts.items[0].blob_key.as_deref(), Some("att:m02:a1"));

    // Header upsert keeps the body; delete removes body, attachments and blob.
    let header = store.message(mid("m02")).await.unwrap().unwrap();
    store.apply_message_delta(vec![header], vec![]).await.unwrap();
    assert!(store.message_body(mid("m02")).await.unwrap().is_some());
    store.apply_message_delta(vec![], vec![mid("m02"), mid("never-existed")]).await.unwrap();
    assert!(store.message(mid("m02")).await.unwrap().is_none());
    assert!(store.message_body(mid("m02")).await.unwrap().is_none());
    assert!(store.attachments(mid("m02")).await.unwrap().items.is_empty());
    assert!(store.get_blob("att:m02:a1".into()).await.unwrap().is_none());
}

#[tokio::test]
async fn oversized_body_is_rejected() {
    let store = seeded().await;
    let huge = MessageBody { content_type: BodyType::Text, content: "x".repeat(quorum_store::MAX_BODY_LEN + 1).into() };
    let err = store.put_message_body(mid("m01"), huge).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[tokio::test]
async fn deleting_folder_removes_its_messages() {
    let store = seeded().await;
    store.write(|tx| tx.delete_mail_folders(&[fid("archive")])).await.unwrap();
    assert!(store.message(mid("x1")).await.unwrap().is_none());
    assert_eq!(store.mail_folders().await.unwrap().items.len(), 1);
    let hits = store.search("archived".into(), vec![quorum_store::SearchScope::Mail], 10).await.unwrap();
    assert!(hits.hits.is_empty());
}

#[tokio::test]
async fn retention_prunes_old_bodies_but_keeps_headers() {
    let store = seeded().await;
    for id in ["m00", "m20"] {
        let body = MessageBody { content_type: BodyType::Text, content: format!("zebra body of {id}").into() };
        store.put_message_body(mid(id), body).await.unwrap();
    }
    let now = crate::common::t("2026-03-01T00:15:00Z");
    // 0 days: everything received before 00:15 loses its body.
    let n = store.prune_message_bodies(now, 0).await.unwrap();
    assert_eq!(n, 1);
    assert!(store.message_body(mid("m00")).await.unwrap().is_none());
    assert!(store.message(mid("m00")).await.unwrap().is_some());
    assert!(store.message_body(mid("m20")).await.unwrap().is_some());
    let hits = store.search("zebra".into(), vec![quorum_store::SearchScope::Mail], 10).await.unwrap();
    let ids: Vec<_> = hits.hits.iter().map(|h| h.title.as_str().to_owned()).collect();
    assert_eq!(ids, ["subject 20"]);
}

#[tokio::test]
async fn bodies_to_prefetch() {
    let store = seeded().await;
    store
        .put_message_body(mid("m24"), MessageBody { content_type: BodyType::Text, content: "x".into() })
        .await
        .unwrap();
    let ids = store.messages_without_body(fid("inbox"), crate::common::t("2026-03-01T00:20:00Z"), 3).await.unwrap();
    let ids: Vec<_> = ids.iter().map(|m| m.as_str()).collect();
    assert_eq!(ids, ["m23", "m22", "m21"]);
    assert!(store.has_message_body(mid("m24")).await.unwrap());
    assert!(!store.has_message_body(mid("m23")).await.unwrap());
    let plan = quorum_store::testing::query_plan(
        &store,
        "SELECT m.id FROM message m INDEXED BY message_folder_received LEFT JOIN message_body b ON b.message_id = m.id
         WHERE m.folder_id = 'inbox' AND m.received_at >= 0 AND b.message_id IS NULL
         ORDER BY m.received_at DESC, m.id DESC LIMIT 3",
    )
    .await
    .unwrap()
    .join(" | ");
    assert!(!plan.contains("TEMP B-TREE") && !plan.contains("SCAN b"), "{plan}");
}
