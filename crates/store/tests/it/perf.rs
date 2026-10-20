use std::time::{Duration, Instant};

use quorum_model::{CalendarId, ChatId, MailFolderId};
use quorum_store::{MessageQuery, Store};

use crate::common::message;

fn assert_indexed(plan: &[String], index: &str) {
    let joined = plan.join(" | ");
    assert!(joined.contains(index), "expected index {index}: {joined}");
    assert!(!joined.contains("TEMP B-TREE"), "sort not served by the index: {joined}");
    assert!(!joined.contains("SCAN message") && !joined.contains("SCAN chat_message"), "table scan: {joined}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_page_of_100k_messages_uses_indexes_and_is_fast() {
    let store = Store::open_in_memory().unwrap();
    for chunk in 0..20u32 {
        let msgs: Vec<_> = (0..5000u32)
            .map(|i| {
                let n = chunk * 5000 + i;
                let folder = if n % 10 == 0 { "archive" } else { "inbox" };
                let mut m = message(
                    &format!("m{n:06}"),
                    folder,
                    &format!("2025-{:02}-{:02}T{:02}:{:02}:00Z", 1 + n % 12, 1 + n % 28, n % 24, n % 60),
                    &format!("subject {n}"),
                );
                m.is_read = n % 3 != 0;
                m
            })
            .collect();
        store.apply_message_delta(msgs, vec![]).await.unwrap();
    }
    let inbox = MailFolderId::new("inbox").unwrap();
    let mut q = MessageQuery::folder(inbox.clone());
    let plan = store.read({
        let q = q.clone();
        move |r| r.explain_list_messages(&q)
    });
    assert_indexed(&plan.await.unwrap(), "message_folder_received");
    q.focused = Some(true);
    let plan = store.read({
        let q = q.clone();
        move |r| r.explain_list_messages(&q)
    });
    assert_indexed(&plan.await.unwrap(), "message_folder_focused");
    q.focused = None;
    q.unread_only = true;
    let plan = store.read({
        let q = q.clone();
        move |r| r.explain_list_messages(&q)
    });
    assert_indexed(&plan.await.unwrap(), "message_folder_unread");

    let mut q = MessageQuery::folder(inbox);
    q.limit = 50;
    let start = Instant::now();
    let page = store.list_messages(q.clone()).await.unwrap();
    let first = start.elapsed();
    assert_eq!(page.items.len(), 50);
    let start = Instant::now();
    let page2 = store.list_messages(MessageQuery { after: page.next.clone(), ..q }).await.unwrap();
    assert_eq!(page2.items.len(), 50);
    let second = start.elapsed();
    // The release-build goal is < 50 ms; debug builds only get a generous sanity bound.
    let budget = if cfg!(debug_assertions) { Duration::from_millis(1000) } else { Duration::from_millis(50) };
    assert!(first < budget && second < budget, "first page {first:?}, next page {second:?}");
    let counts = store.unread_counts().await.unwrap();
    assert_eq!(counts.iter().map(|c| c.unread).sum::<u32>(), 33_334);
}

#[tokio::test]
async fn other_hot_queries_use_indexes() {
    let store = Store::open_in_memory().unwrap();
    let plan = |sql: &str| {
        let store = store.clone();
        let sql = sql.to_owned();
        async move { quorum_store::testing::query_plan(&store, &sql).await.unwrap() }
    };
    let _ = (CalendarId::new("c").unwrap(), ChatId::new("c").unwrap());
    assert_indexed(
        &plan(
            "SELECT created_at, id, meta, body FROM chat_message WHERE chat_id = 'c' AND (created_at, id) < (1, 'x')
             ORDER BY created_at DESC, id DESC LIMIT 51",
        )
        .await,
        "chat_message_created",
    );
    let p = plan(
        "SELECT start_utc, id, json FROM event INDEXED BY event_calendar_start
         WHERE calendar_id = 'c' AND start_utc < 10 AND end_utc > 0 ORDER BY start_utc LIMIT 10",
    )
    .await;
    assert!(p.join(" ").contains("event_calendar_start"), "{p:?}");
    let p = plan(
        "SELECT last_activity, id, json FROM chat WHERE is_hidden = 0 ORDER BY last_activity DESC, id DESC LIMIT 51",
    )
    .await;
    assert!(p.join(" ").contains("chat_activity"), "{p:?}");
    let p = plan(
        "SELECT json FROM drive_item INDEXED BY drive_item_parent WHERE drive_id = 'd' AND parent_id IS 'p'
         ORDER BY is_folder DESC, name_sort ASC, id ASC LIMIT 51",
    )
    .await;
    assert!(!p.join(" ").contains("TEMP B-TREE"), "{p:?}");
}
