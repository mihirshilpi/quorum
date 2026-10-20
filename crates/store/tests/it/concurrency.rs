use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use quorum_model::MailFolderId;
use quorum_store::MessageQuery;

use crate::common::{message, open};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_readers_and_a_writer() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path()).await;
    let done = Arc::new(AtomicBool::new(false));
    let writer = {
        let store = store.clone();
        let done = Arc::clone(&done);
        tokio::spawn(async move {
            for batch in 0..20u32 {
                let msgs: Vec<_> = (0..50u32)
                    .map(|i| {
                        let n = batch * 50 + i;
                        message(&format!("m{n:05}"), "inbox", "2026-01-01T00:00:00Z", &format!("s{n}"))
                    })
                    .collect();
                store.apply_message_delta(msgs, vec![]).await.unwrap();
            }
            done.store(true, Ordering::SeqCst);
        })
    };
    let mut readers = Vec::new();
    for _ in 0..16 {
        let store = store.clone();
        let done = Arc::clone(&done);
        readers.push(tokio::spawn(async move {
            let mut last = 0usize;
            let mut rounds = 0u32;
            while !done.load(Ordering::SeqCst) || rounds == 0 {
                let mut q = MessageQuery::folder(MailFolderId::new("inbox").unwrap());
                q.limit = 500;
                let page = store.list_messages(q).await.unwrap();
                // Each transaction is atomic: readers only ever see whole batches of 50.
                assert_eq!(page.items.len() % 50, 0);
                let counts = store.unread_counts().await.unwrap();
                let unread = counts.first().map_or(0, |c| c.unread as usize);
                assert_eq!(unread % 50, 0);
                assert!(unread >= last, "snapshots never go backwards");
                last = unread;
                rounds += 1;
            }
            rounds
        }));
    }
    writer.await.unwrap();
    for r in readers {
        assert!(r.await.unwrap() > 0);
    }
    assert_eq!(store.unread_counts().await.unwrap()[0].unread, 1000);
}
