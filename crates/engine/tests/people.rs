//! People, presence and universal search commands and queries against the mock Graph.

use quorum_engine::{ErrorKind, Session, SessionOptions};
use quorum_ipc::CoreEvent;
use quorum_model::{MailAddress, Person, Sensitive, UserId};
use serde_json::{Value, json};

const ME: &str = "00000000-0000-0000-0000-000000000001";

async fn session() -> Session {
    let s = Session::new(SessionOptions { mock_graph: true }).unwrap();
    s.start().await;
    let store = s.store().unwrap();
    store.set_setting("people.me_id".into(), ME.to_owned()).await.unwrap();
    s
}

/// A session whose first presence pass has landed, so a write made next is not raced by
/// the startup refresh.
async fn settled_session() -> Session {
    let s = session().await;
    for _ in 0..200 {
        if !s.query("presence.me", Value::Null).await.unwrap()["fetched_at"].is_null() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    s
}

fn person(id: &str, name: &str, email: &str) -> Person {
    Person {
        id: Some(UserId::new(id).unwrap()),
        display_name: Sensitive::new(name),
        email: Some(MailAddress::new(email).unwrap()),
        ..Person::default()
    }
}

#[tokio::test]
async fn presence_set_applies_optimistically() {
    let s = settled_session().await;
    s.execute("presence.set", json!({ "availability": "dnd", "duration_minutes": 60 })).await.unwrap();
    let me = s.query("presence.me", Value::Null).await.unwrap();
    assert_eq!(me["user_id"], ME);
    assert_eq!(me["presence"], "dnd");
    assert_eq!(me["availability"], "DoNotDisturb");
    s.execute("presence.set", json!({ "availability": "be_right_back" })).await.unwrap();
    assert_eq!(s.query("presence.me", Value::Null).await.unwrap()["presence"], "brb");
    s.execute("presence.set", json!({ "availability": "reset" })).await.unwrap();

    for bad in [
        json!({ "availability": "online" }),
        json!({ "availability": "busy", "duration_minutes": 0 }),
        json!({ "availability": "busy", "duration_minutes": 20_000 }),
        json!({ "availability": "reset", "duration_minutes": 30 }),
        json!({}),
    ] {
        let e = s.execute("presence.set", bad.clone()).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::InvalidParams, "{bad}");
    }
}

#[tokio::test]
async fn status_message_round_trip_and_validation() {
    let s = settled_session().await;
    s.execute(
        "presence.set_status_message",
        json!({ "message": "  Synthetic focus  ", "expires": "2099-01-01T00:00:00Z" }),
    )
    .await
    .unwrap();
    let me = s.query("presence.me", Value::Null).await.unwrap();
    assert_eq!(me["status_message"], "Synthetic focus");
    assert_eq!(me["expiry"], "2099-01-01T00:00:00Z");
    s.execute("presence.clear_status_message", Value::Null).await.unwrap();
    assert_eq!(s.query("presence.me", Value::Null).await.unwrap()["status_message"], Value::Null);
    for bad in [
        json!({ "message": "x".repeat(281) }),
        json!({ "message": "" }),
        json!({ "message": "bell\u{7}" }),
        json!({ "message": "ok", "expires": "2001-01-01T00:00:00Z" }),
        json!({ "message": "ok", "expires": "tomorrow" }),
    ] {
        let e = s.execute("presence.set_status_message", bad.clone()).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::InvalidParams, "{bad}");
    }
}

#[tokio::test]
async fn presence_watch_and_of() {
    let s = session().await;
    let ids =
        json!(["00000000-0000-0000-0000-000000000101", "00000000-0000-0000-0000-000000000102", 5, "", "bad\u{1}"]);
    let r = s.execute("presence.watch", json!({ "user_ids": ids })).await.unwrap();
    assert!(r["watching"].as_u64().unwrap() >= 2);
    let too_many: Vec<String> = (0..651).map(|i| format!("u{i}")).collect();
    assert!(s.execute("presence.watch", json!({ "user_ids": too_many })).await.is_err());
    // Seed a cached presence and read it back.
    let store = s.store().unwrap();
    let p: quorum_model::Presence = serde_json::from_value(
        json!({ "userId": "00000000-0000-0000-0000-000000000101", "availability": "Busy", "activity": "InAMeeting" }),
    )
    .unwrap();
    store.upsert_presences(vec![p], jiff::Timestamp::now()).await.unwrap();
    let of = s
        .query("presence.of", json!({ "user_ids": ["00000000-0000-0000-0000-000000000101", "nobody"] }))
        .await
        .unwrap();
    assert_eq!(of["00000000-0000-0000-0000-000000000101"]["presence"], "busy");
    assert!(of.get("nobody").is_none());
}

#[tokio::test]
async fn people_search_merges_local_and_directory() {
    let s = session().await;
    let store = s.store().unwrap();
    store
        .upsert_people(vec![person(
            "00000000-0000-0000-0000-000000000105",
            "Casey Mockwell",
            "casey.mockwell@contoso.example",
        )])
        .await
        .unwrap();
    let rows = s.query("people.search", json!({ "query": "casey", "limit": 10 })).await.unwrap();
    let rows = rows.as_array().cloned().unwrap();
    let ids: Vec<&str> = rows.iter().filter_map(|r| r["id"].as_str()).collect();
    assert_eq!(ids.first(), Some(&"00000000-0000-0000-0000-000000000105"), "local first");
    assert!(ids.contains(&"00000000-0000-0000-0000-000000000109"), "directory hit merged");
    assert_eq!(ids.iter().filter(|i| **i == "00000000-0000-0000-0000-000000000105").count(), 1, "no duplicates");
    for r in &rows {
        for k in ["id", "display_name", "name", "email", "job_title", "department", "presence"] {
            assert!(r.get(k).is_some(), "{k}");
        }
    }
    // Directory hits are cached: a local-only search now finds both.
    let local = s.query("people.search", json!({ "query": "casey", "remote": false })).await.unwrap();
    assert_eq!(local.as_array().map(Vec::len), Some(2));
    let local = s.query("people.search", json!({ "query": "mockwell", "remote": false })).await.unwrap();
    assert_eq!(local.as_array().map(Vec::len), Some(1));
    assert_eq!(s.query("people.search", json!({ "query": "   " })).await.unwrap(), json!([]));
}

#[tokio::test]
async fn person_details_include_org_chart() {
    let s = session().await;
    let store = s.store().unwrap();
    store.upsert_people(vec![person(ME, "Test User", "test.user@contoso.example")]).await.unwrap();
    let d = s.query("people.person", json!({ "user": ME })).await.unwrap();
    assert_eq!(d["person"]["display_name"], "Test User");
    let managers: Vec<&str> =
        d["managers"].as_array().unwrap().iter().filter_map(|m| m["display_name"].as_str()).collect();
    assert_eq!(managers, ["Morgan Sample", "Avery Director"]);
    assert_eq!(d["reports"].as_array().map(Vec::len), Some(2));
    // By address too.
    let d = s.query("people.person", json!({ "user": "test.user@contoso.example" })).await.unwrap();
    assert_eq!(d["person"]["id"], ME);
    let e = s.query("people.person", json!({ "user": "00000000-0000-0000-0000-00000000dead" })).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::NotFound);
}

#[tokio::test]
async fn people_open_asks_the_ui() {
    let s = session().await;
    let mut events = s.subscribe();
    s.execute("people.open", json!({ "user": ME })).await.unwrap();
    let mut seen = false;
    for _ in 0..50 {
        match events.try_recv() {
            Ok(CoreEvent::Ui { command, params }) if command == "people.card.show" => {
                assert_eq!(params["user"], ME);
                seen = true;
                break;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    assert!(seen);
    // `id` is accepted as an alias (command-bar context params).
    s.execute("people.open", json!({ "id": ME })).await.unwrap();
    let e = s.execute("people.email", json!({ "user": "00000000-0000-0000-0000-00000000dead" })).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::NotFound);
}

#[tokio::test]
async fn search_all_groups_and_dedupes() {
    let s = session().await;
    let r = s.query("search.all", json!({ "query": "synthetic" })).await.unwrap();
    let groups = r["groups"].as_array().cloned().unwrap();
    assert!(!groups.is_empty());
    let order = ["mail", "chats", "channels", "files", "people", "events"];
    let mut last = 0;
    let mut keys = std::collections::HashSet::new();
    for g in &groups {
        let pos = order.iter().position(|o| *o == g["scope"]).unwrap();
        assert!(pos >= last, "groups out of order");
        last = pos;
        for h in g["hits"].as_array().unwrap() {
            assert!(keys.insert(h["key"].as_str().unwrap().to_owned()), "duplicate {}", h["key"]);
            assert!(!h["open"]["command"].as_str().unwrap().is_empty());
            let text = h["snippet"]["text"].as_str().unwrap();
            for hl in h["snippet"]["highlights"].as_array().unwrap() {
                let (a, b) = (hl[0].as_u64().unwrap() as usize, hl[1].as_u64().unwrap() as usize);
                assert!(text.get(a..b).is_some());
            }
        }
    }
    // Scope filter.
    let r = s.query("search.all", json!({ "query": "synthetic", "scopes": ["files", "nope"] })).await.unwrap();
    assert!(r["groups"].as_array().unwrap().iter().all(|g| g["scope"] == "files"));
}

#[tokio::test]
async fn search_all_survives_hostile_queries() {
    let s = session().await;
    for q in [
        "\"",
        "*",
        "NEAR(a b)",
        "a OR",
        "-",
        "^",
        "'; DROP TABLE message; --",
        "\u{0}\u{202e}",
        "<c0>x</c0>",
        &"é".repeat(50_000),
        "AND AND AND",
        "{}[]()",
    ] {
        let r = s.query("search.all", json!({ "query": q, "remote": false })).await;
        assert!(r.is_ok(), "{q:?}: {r:?}");
        let r = s.query("people.search", json!({ "query": q, "remote": false })).await;
        assert!(r.is_ok(), "{q:?}: {r:?}");
    }
}

#[tokio::test]
async fn hostile_params_never_panic_after_sign_in() {
    let s = session().await;
    let ids: Vec<String> = s
        .registry()
        .iter()
        .map(|c| c.id.to_owned())
        .chain(s.queries().iter().map(|c| c.id.to_owned()))
        .filter(|id| ["people.", "presence.", "search."].iter().any(|p| id.starts_with(p)))
        .collect();
    assert!(ids.len() >= 14, "{ids:?}");
    let deep = (0..200).fold(json!(1), |acc, _| json!([acc]));
    let values = [
        Value::Null,
        json!(-1),
        json!("x".repeat(100_000)),
        json!({ "user": "", "user_ids": "x", "query": [], "availability": 5 }),
        json!({ "user": "x".repeat(5000), "user_ids": [null, {}, []], "query": "\u{0}" }),
        json!({ "user": "a@b", "availability": "busy", "duration_minutes": u64::MAX, "limit": -1 }),
        json!({ "message": "\u{202e}".repeat(300), "expires": "9999-99-99" }),
        json!({ "scopes": vec!["mail"; 100], "query": "x" }),
        deep,
    ];
    for v in &values {
        for id in &ids {
            let r = if s.registry().get(id).is_ok() {
                s.execute(id, v.clone()).await
            } else {
                s.query(id, v.clone()).await
            };
            if let Err(e) = r {
                assert_ne!(e.kind(), ErrorKind::Bug, "{id} on {v}");
            }
        }
    }
}
