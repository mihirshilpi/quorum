//! Chat and channel commands and queries end to end in mock mode (fixture Graph, in-memory store,
//! running sync engine): query shapes, optimistic writes, outbox replay and hostile params.

use std::time::Duration;

use quorum_engine::{ErrorKind, Session, SessionOptions};
use serde_json::{Value, json};

const MORGAN_CHAT: &str = "19:mock-morgan_mock-me@unq.gbl.spaces";
const TEAM: &str = "00000000-0000-0000-0000-00000000f001";
const GENERAL: &str = "19:mock-product-general@thread.tacv2";

async fn session() -> Session {
    let s = Session::new(SessionOptions { mock_graph: true }).unwrap();
    s.start().await;
    s
}

/// Polls `query` until `ok` holds (sync runs in the background).
async fn wait_for(s: &Session, query: &str, params: Value, ok: impl Fn(&Value) -> bool) -> Value {
    let mut last = Value::Null;
    for _ in 0..200 {
        if let Ok(v) = s.query(query, params.clone()).await {
            if ok(&v) {
                return v;
            }
            last = v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{query} never satisfied the condition; last: {last}");
}

fn items(v: &Value) -> Vec<Value> {
    v.get("items").and_then(Value::as_array).cloned().unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_list_and_messages_have_the_documented_shape() {
    let s = session().await;
    let list = wait_for(&s, "chat.list", json!({}), |v| v.as_array().is_some_and(|a| a.len() >= 5)).await;
    let rows = list.as_array().unwrap();
    let first = &rows[0];
    for key in ["id", "kind", "title", "preview", "from_name", "time", "unread", "pinned", "members", "member_ids"] {
        assert!(first.get(key).is_some(), "chat row lacks {key}: {first}");
    }
    let morgan = rows.iter().find(|r| r["id"] == MORGAN_CHAT).unwrap();
    assert_eq!(morgan["title"], "Morgan Sample", "1:1 title is the other person");
    assert_eq!(morgan["unread"], true);
    assert_eq!(morgan["presence_id"], "00000000-0000-0000-0000-000000000101");
    let group = rows.iter().find(|r| r["kind"] == "group").unwrap();
    assert_eq!(group["title"], "Launch crew");

    let unread = s.query("chat.list", json!({"filter": "unread"})).await.unwrap();
    assert!(unread.as_array().unwrap().iter().all(|r| r["unread"] == true));
    let found = s.query("chat.list", json!({"query": "quinn"})).await.unwrap();
    assert_eq!(found.as_array().unwrap().len(), 1);

    let msgs = wait_for(&s, "chat.messages", json!({"chat": MORGAN_CHAT}), |v| items(v).len() >= 5).await;
    let list = items(&msgs);
    // Oldest first.
    assert!(list.first().unwrap()["time"].as_str() <= list.last().unwrap()["time"].as_str());
    let with_file = list.iter().find(|m| !m["files"].as_array().unwrap().is_empty()).unwrap();
    assert_eq!(with_file["files"][0]["name"], "Budget draft.xlsx");
    let mine = list.iter().find(|m| m["mine"] == true).unwrap();
    assert_eq!(mine["reactions"][0]["emoji"], "👍");
    for key in ["id", "from_name", "html", "edited", "deleted", "system", "pending", "reactions", "mentions_me"] {
        assert!(mine.get(key).is_some(), "message lacks {key}");
    }
    assert_eq!(msgs["me"], "00000000-0000-0000-0000-000000000001");

    let crew = rows.iter().find(|r| r["title"] == "Launch crew").unwrap()["id"].as_str().unwrap().to_owned();
    let crew_msgs = wait_for(&s, "chat.messages", json!({"chat": crew}), |v| items(v).len() >= 5).await;
    let l = items(&crew_msgs);
    assert!(l.iter().any(|m| m["deleted"] == true && m["html"] == ""));
    assert!(l.iter().any(|m| m["edited"] == true));
    assert!(l.iter().any(|m| m["mentions_me"] == true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_is_optimistic_then_replayed() {
    let s = session().await;
    wait_for(&s, "chat.messages", json!({"chat": MORGAN_CHAT}), |v| items(v).len() >= 5).await;
    let out = s
        .execute(
            "chat.send",
            json!({"chat": MORGAN_CHAT, "html": "<p>On it <at id=\"0\">Morgan</at></p><script>x()</script>",
                   "mentions": [{"id": 0, "text": "Morgan", "user": "00000000-0000-0000-0000-000000000101"}]}),
        )
        .await
        .unwrap();
    let local = out["local_id"].as_str().unwrap().to_owned();
    let now = s.query("chat.messages", json!({"chat": MORGAN_CHAT})).await.unwrap();
    let pending = items(&now).into_iter().find(|m| m["id"] == local.as_str());
    if let Some(p) = pending {
        assert_eq!(p["pending"], true);
        assert!(!p["html"].as_str().unwrap().contains("script"));
    }
    // The outbox replays against the fixture (POST /chats/{id}/messages) and swaps in the real id.
    wait_for(&s, "chat.messages", json!({"chat": MORGAN_CHAT}), |v| {
        let l = items(v);
        l.iter().all(|m| m["id"] != local.as_str()) && l.iter().any(|m| m["id"] == "1760700000999")
    })
    .await;
    // Chat list moved the chat to the top with "You" as sender.
    let list = s.query("chat.list", json!({"limit": 1})).await.unwrap();
    assert_eq!(list[0]["id"], MORGAN_CHAT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn react_edit_delete_drafts_pins_and_mark_read_apply_locally() {
    let s = session().await;
    wait_for(&s, "chat.messages", json!({"chat": MORGAN_CHAT}), |v| items(v).len() >= 5).await;
    s.execute("chat.react", json!({"chat": MORGAN_CHAT, "message": "1760690000005", "reaction": "🎉"})).await.unwrap();
    let v = s.query("chat.messages", json!({"chat": MORGAN_CHAT})).await.unwrap();
    let m = items(&v).into_iter().find(|m| m["id"] == "1760690000005").unwrap();
    assert!(m["reactions"].as_array().unwrap().iter().any(|r| r["emoji"] == "🎉" && r["mine"] == true));

    // Only your own messages can be edited or deleted.
    let e = s
        .execute("chat.edit", json!({"chat": MORGAN_CHAT, "message": "1760690000005", "text": "x"}))
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::NotAvailable);
    s.execute("chat.edit", json!({"chat": MORGAN_CHAT, "message": "1760690000002", "text": "Yes, done."}))
        .await
        .unwrap();
    let v = s.query("chat.messages", json!({"chat": MORGAN_CHAT})).await.unwrap();
    let m = items(&v).into_iter().find(|m| m["id"] == "1760690000002").unwrap();
    assert_eq!(m["edited"], true);
    assert!(m["html"].as_str().unwrap().contains("Yes, done."));
    s.execute("chat.delete", json!({"chat": MORGAN_CHAT, "message": "1760690000002"})).await.unwrap();
    let v = s.query("chat.messages", json!({"chat": MORGAN_CHAT})).await.unwrap();
    assert!(items(&v).iter().any(|m| m["id"] == "1760690000002" && m["deleted"] == true));

    s.execute("chat.save_draft", json!({"chat": MORGAN_CHAT, "html": "<p>half a thought</p>"})).await.unwrap();
    let d = s.query("chat.draft", json!({"chat": MORGAN_CHAT})).await.unwrap();
    assert_eq!(d["html"], "<p>half a thought</p>");
    s.execute("chat.save_draft", json!({"chat": MORGAN_CHAT, "html": ""})).await.unwrap();
    assert_eq!(s.query("chat.draft", json!({"chat": MORGAN_CHAT})).await.unwrap(), Value::Null);

    s.execute("chat.pin", json!({"id": MORGAN_CHAT})).await.unwrap();
    let pinned = s.query("chat.list", json!({"filter": "pinned"})).await.unwrap();
    assert_eq!(pinned.as_array().unwrap().len(), 1);
    s.execute("chat.unpin", json!({"id": MORGAN_CHAT})).await.unwrap();

    s.execute("chat.mark_read", json!({"id": MORGAN_CHAT})).await.unwrap();
    let list = s.query("chat.list", json!({})).await.unwrap();
    let row = list.as_array().unwrap().iter().find(|r| r["id"] == MORGAN_CHAT).unwrap().clone();
    assert_eq!(row["unread"], false);

    let hits =
        wait_for(&s, "chat.search", json!({"query": "budget"}), |v| v.as_array().is_some_and(|a| !a.is_empty())).await;
    assert!(hits.as_array().unwrap().iter().any(|h| h["scope"] == "chat"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_chat_reuses_one_on_one_and_creates_groups() {
    let s = session().await;
    wait_for(&s, "chat.list", json!({}), |v| v.as_array().is_some_and(|a| a.len() >= 5)).await;
    let same = s.execute("chat.new", json!({"members": ["00000000-0000-0000-0000-000000000101"]})).await.unwrap();
    assert_eq!(same["existing"], true);
    assert_eq!(same["id"], MORGAN_CHAT);
    let group = s
        .execute("chat.new", json!({"members": ["00000000-0000-0000-0000-000000000103"], "emails": ["sage.exemplar@contoso.example"], "topic": "Offsite"}))
        .await
        .unwrap();
    assert_eq!(group["existing"], false);
    let id = group["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("local-"));
    let list = s.query("chat.list", json!({})).await.unwrap();
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == id.as_str() && r["title"] == "Offsite" && r["pending"] == true)
    );
    let e = s.execute("chat.new", json!({"members": []})).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::InvalidParams);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teams_channels_posts_and_replies() {
    let s = session().await;
    let teams = wait_for(&s, "teams.list", json!({}), |v| v.as_array().is_some_and(|a| a.len() == 2)).await;
    let product = teams.as_array().unwrap().iter().find(|t| t["id"] == TEAM).unwrap().clone();
    assert_eq!(product["name"], "Product");
    assert!(product["channels"].as_array().unwrap().iter().any(|c| c["id"] == GENERAL && c["general"] == true));

    let posts = wait_for(&s, "channel.posts", json!({"channel": GENERAL}), |v| items(v).len() >= 3).await;
    let welcome = items(&posts).into_iter().find(|p| p["subject"] == "Welcome").unwrap();
    assert_eq!(welcome["reply_count"], 2);
    assert_eq!(welcome["replies"].as_array().unwrap().len(), 2);
    let replies =
        s.query("channel.replies", json!({"team": TEAM, "channel": GENERAL, "post": welcome["id"]})).await.unwrap();
    assert_eq!(replies.as_array().unwrap().len(), 2);

    let out =
        s.execute("channel.reply", json!({"channel": GENERAL, "post": welcome["id"], "text": "Agreed"})).await.unwrap();
    assert!(out["local_id"].as_str().unwrap().starts_with("local-"));
    let after = s.query("channel.replies", json!({"channel": GENERAL, "post": welcome["id"]})).await.unwrap();
    assert_eq!(after.as_array().unwrap().len(), 3, "optimistic reply shows at once");
    let e = s.execute("channel.reply", json!({"channel": GENERAL, "text": "no post"})).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::InvalidParams);
    s.execute("channel.post", json!({"team": TEAM, "channel": GENERAL, "subject": "Hi", "text": "New thread"}))
        .await
        .unwrap();
    s.execute("channel.open", json!({"id": GENERAL})).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hostile_chat_params_are_rejected_cleanly() {
    let s = session().await;
    let cases = [
        ("chat.send", json!({"chat": MORGAN_CHAT})),
        ("chat.send", json!({"chat": "", "text": "x"})),
        ("chat.send", json!({"chat": MORGAN_CHAT, "text": "x".repeat(300_000)})),
        (
            "chat.send",
            json!({"chat": MORGAN_CHAT, "html": "<p>x</p>", "mentions": [{"id": 0, "text": "a", "user": "u"}]}),
        ),
        (
            "chat.send",
            json!({"chat": MORGAN_CHAT, "text": "x", "images": [{"content_type": "image/svg+xml", "data": "AAAA"}]}),
        ),
        ("chat.send", json!({"chat": MORGAN_CHAT, "text": "x", "files": [{"name": "../", "data": "AAAA"}]})),
        ("chat.react", json!({"chat": MORGAN_CHAT, "message": "1760690000005", "reaction": "\u{7}"})),
        ("chat.messages", json!({"chat": MORGAN_CHAT, "before": "not-a-cursor"})),
        ("chat.list", json!({"filter": "everything"})),
        ("channel.posts", json!({"channel": "19:unknown@thread"})),
    ];
    for (id, p) in cases {
        let r = if id.contains("messages") || id.contains("list") || id.contains("posts") {
            s.query(id, p.clone()).await
        } else {
            s.execute(id, p.clone()).await
        };
        let e = r.expect_err(&format!("{id} accepted {p}"));
        assert_ne!(e.kind(), ErrorKind::Bug, "{id}");
    }
}
