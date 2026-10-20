//! Settings, blob and URL commands in mock mode (nothing touches the disk).

use quorum_engine::{ErrorKind, Session, SessionOptions};
use serde_json::{Value, json};

async fn mock() -> Session {
    let s = Session::new(SessionOptions { mock_graph: true }).unwrap();
    s.start().await;
    s
}

#[tokio::test]
async fn settings_round_trip_and_validate() {
    let s = mock().await;
    let v = s.query("settings.get", json!({ "key": "mail.undo_send_seconds" })).await.unwrap();
    assert_eq!(v["value"], json!(5));
    assert_eq!(s.query("settings.get", json!({ "key": "presence.keep_alive" })).await.unwrap()["value"], json!(true));
    s.execute("settings.set", json!({ "key": "mail.undo_send_seconds", "value": 12 })).await.unwrap();
    let v = s.query("settings.get", json!({ "key": "mail.undo_send_seconds" })).await.unwrap();
    assert_eq!(v["value"], json!(12));
    let all = s.query("settings.get", Value::Null).await.unwrap();
    assert_eq!(all["mail.undo_send_seconds"], json!(12));
    assert_eq!(all["appearance.theme"], json!("system"));
    s.execute("settings.reset", json!({ "key": "mail.undo_send_seconds" })).await.unwrap();
    assert_eq!(s.query("settings.get", json!({ "key": "mail.undo_send_seconds" })).await.unwrap()["value"], json!(5));
    for bad in [
        json!({ "key": "mail.undo_send_seconds", "value": 99 }),
        json!({ "key": "appearance.theme", "value": "neon" }),
        json!({ "key": "nope", "value": true }),
        json!({ "key": "presence.keep_alive", "value": "yes" }),
        json!({ "value": true }),
    ] {
        let e = s.execute("settings.set", bad.clone()).await.err().map(|e| e.kind());
        assert_eq!(e, Some(ErrorKind::InvalidParams), "{bad}");
    }
    let schema = s.query("settings.schema", Value::Null).await.unwrap();
    assert!(schema.as_array().is_some_and(|a| a.iter().any(|d| d["key"] == "agents.destructive_policy")));
}

#[tokio::test]
async fn settings_changes_are_announced() {
    let s = mock().await;
    let mut rx = s.subscribe();
    s.execute("settings.set", json!({ "key": "appearance.density", "value": "compact" })).await.unwrap();
    let mut seen = false;
    while let Ok(ev) = rx.try_recv() {
        if let quorum_ipc::CoreEvent::StoreChanged { tables } = ev {
            seen |= tables.iter().any(|t| t == "settings");
        }
    }
    assert!(seen);
}

#[tokio::test]
async fn links_and_blobs_are_checked() {
    let s = mock().await;
    let ok = s.execute("link.confirm", json!({ "url": "https://example.com/x" })).await.unwrap();
    assert_eq!(ok["allowed"], json!(true));
    assert_eq!(ok["host"], json!("example.com"));
    let bad = s.execute("link.confirm", json!({ "url": "javascript:alert(1)" })).await.unwrap();
    assert_eq!(bad["allowed"], json!(false));
    let e = s.execute("app.open_url", json!({ "url": "file:///etc/passwd" })).await.err().map(|e| e.kind());
    assert_eq!(e, Some(ErrorKind::InvalidParams));
    let e = s.query("blob.get", json!({ "id": "missing" })).await.err().map(|e| e.kind());
    assert_eq!(e, Some(ErrorKind::NotFound));
    let e = s.query("blob.get", json!({ "id": "m/a" })).await.err().map(|e| e.kind());
    assert!(matches!(e, Some(ErrorKind::NotFound | ErrorKind::InvalidParams)));
    let about = s.query("app.about", Value::Null).await.unwrap();
    assert!(about["contributors"].is_array());
    let queries = s.query("app.queries", Value::Null).await.unwrap();
    assert!(queries.as_array().is_some_and(|q| q.iter().any(|q| q["id"] == "settings.get")));
}
