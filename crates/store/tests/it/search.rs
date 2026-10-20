use quorum_model::{BodyType, MessageBody, MessageId};
use quorum_store::{HitItem, SearchScope, Store};

use crate::common::{channel_message, chat_message, drive_item, message, person};

async fn seeded() -> Store {
    let store = Store::open_in_memory().unwrap();
    store
        .apply_message_delta(
            vec![
                message("m1", "inbox", "2026-01-01T00:00:00Z", "Quarterly budget review"),
                message("m2", "inbox", "2026-01-02T00:00:00Z", "Lunch plans"),
            ],
            vec![],
        )
        .await
        .unwrap();
    let body = MessageBody {
        content_type: BodyType::Html,
        content: "<p>The <b>kangaroo</b> invoice is attached &amp; ready.</p><script>ignored()</script>".into(),
    };
    store.put_message_body(MessageId::new("m2").unwrap(), body).await.unwrap();
    store
        .upsert_chat_messages(vec![
            chat_message("c1", "x1", "2026-01-01T00:00:00Z", "the budget spreadsheet is shared"),
            channel_message("t1", "19:c", "p1", None, "2026-01-01T00:00:00Z", "release notes for budget tool"),
        ])
        .await
        .unwrap();
    store
        .apply_drive_delta(
            quorum_model::DriveId::new("d1").unwrap(),
            vec![drive_item("d1", "i1", None, "Budget 2026.xlsx", false, "2026-01-01T00:00:00Z")],
            vec![],
        )
        .await
        .unwrap();
    store.upsert_people(vec![person("u1", "Budgetta Jones", "bj@example.com")]).await.unwrap();
    store
}

#[tokio::test]
async fn finds_each_scope_with_snippets() {
    let store = seeded().await;
    let res = store.search("budget".into(), SearchScope::ALL.to_vec(), 10).await.unwrap();
    let scopes: Vec<_> = res.hits.iter().map(|h| h.scope).collect();
    assert_eq!(
        scopes,
        [SearchScope::Mail, SearchScope::Chat, SearchScope::Channel, SearchScope::Files, SearchScope::People]
    );
    for h in &res.hits {
        if h.scope == SearchScope::People {
            continue;
        }
        let (a, b) = *h.snippet.highlights.first().unwrap_or_else(|| panic!("no highlight for {:?}", h.scope));
        assert!(h.snippet.text.as_str()[a as usize..b as usize].eq_ignore_ascii_case("budget"));
    }
    assert!(matches!(res.hits[0].item, HitItem::Mail { .. }));
    assert_eq!(res.hits[0].title.as_str(), "Quarterly budget review");
    assert!(matches!(&res.hits[2].item, HitItem::ChannelMessage { channel, .. } if channel.as_str() == "19:c"));
    assert_eq!(res.hits[2].title.as_str(), "post p1");
}

#[tokio::test]
async fn body_text_and_prefix_matches() {
    let store = seeded().await;
    let res = store.search("kanga".into(), vec![SearchScope::Mail], 10).await.unwrap();
    assert_eq!(res.hits.len(), 1);
    let snip = res.hits[0].snippet.text.as_str();
    assert!(snip.contains("kangaroo invoice"), "snippet from body text: {snip:?}");
    assert!(!snip.contains('<') && !snip.contains("ignored"), "markup and scripts are not indexed");
    // Script content was not indexed.
    assert!(store.search("ignored".into(), vec![SearchScope::Mail], 10).await.unwrap().hits.is_empty());
    // All terms must match (AND).
    assert_eq!(store.search("lunch kangaroo".into(), vec![SearchScope::Mail], 10).await.unwrap().hits.len(), 1);
    assert!(store.search("lunch budget".into(), vec![SearchScope::Mail], 10).await.unwrap().hits.is_empty());
    // People by surname prefix.
    let res = store.search("jon".into(), vec![SearchScope::People], 10).await.unwrap();
    assert_eq!(res.hits.len(), 1);
}

#[tokio::test]
async fn edits_and_deletes_update_the_index() {
    let store = seeded().await;
    let mut m = store.message(MessageId::new("m1").unwrap()).await.unwrap().unwrap();
    m.subject = "Renamed topic".into();
    m.body_preview = "nothing here".into();
    store.apply_message_delta(vec![m], vec![]).await.unwrap();
    assert!(store.search("quarterly".into(), vec![SearchScope::Mail], 10).await.unwrap().hits.is_empty());
    assert_eq!(store.search("renamed".into(), vec![SearchScope::Mail], 10).await.unwrap().hits.len(), 1);
    store.apply_message_delta(vec![], vec![MessageId::new("m1").unwrap()]).await.unwrap();
    assert!(store.search("renamed".into(), vec![SearchScope::Mail], 10).await.unwrap().hits.is_empty());
}

#[tokio::test]
async fn hostile_queries_never_error_or_panic() {
    let store = seeded().await;
    let hostile = [
        "",
        " ",
        "\"",
        "\"\"",
        "\"budget",
        "budget\"",
        "\"budget\" OR \"lunch\"",
        "NEAR(budget lunch, 2)",
        "NEAR",
        "budget NEAR/2 lunch",
        "*",
        "budget*",
        "*budget",
        "**",
        "(",
        ")",
        "((budget)",
        "budget)",
        "AND",
        "OR",
        "NOT",
        "budget NOT",
        "- budget",
        "+budget",
        "^budget",
        "subject:budget",
        "{subject sender}: budget",
        "budget\0lunch",
        "\0",
        "\u{1}\u{2}\u{7f}",
        "'; DROP TABLE message; --",
        "%_\\",
        "\u{202e}gdub",
        "🦀🦀🦀",
        "日本語 テスト",
        "İSTANBUL",
        "a\"b\"c\"d",
        "x".repeat(10_000).as_str(),
    ]
    .map(str::to_owned);
    for q in hostile.iter().cloned().chain(std::iter::once("budget ".repeat(100))) {
        let res = store.search(q.clone(), SearchScope::ALL.to_vec(), 50).await;
        assert!(res.is_ok(), "query {:?} failed: {:?}", q.get(..q.len().min(40)), res.err());
    }
    // The store still works and the table survived.
    assert_eq!(store.search("budget".into(), vec![SearchScope::Mail], 10).await.unwrap().hits.len(), 1);
    // Quoted syntax is matched as words, not operators.
    assert_eq!(store.search("\"budget\" OR".into(), vec![SearchScope::Mail], 10).await.unwrap().hits.len(), 0);
    assert_eq!(store.search("(budget)".into(), vec![SearchScope::Mail], 10).await.unwrap().hits.len(), 1);
}
