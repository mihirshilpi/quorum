//! Every command and query survives hostile parameters without panicking.

use quorum_engine::{ErrorKind, Session, SessionOptions};
use serde_json::{Value, json};

fn hostile_values() -> Vec<Value> {
    let deep = (0..200).fold(json!(1), |acc, _| json!([acc]));
    vec![
        Value::Null,
        json!(true),
        json!(-1),
        json!(f64::MAX),
        json!(""),
        json!("x".repeat(100_000)),
        json!([]),
        json!({}),
        json!({ "query": 5, "id": [], "to": "not-a-list", "limit": -9_007_199_254_740_991_i64 }),
        json!({ "\u{0}": "\u{202e}" }),
        deep,
    ]
}

#[tokio::test]
async fn hostile_params_never_panic() {
    let s = Session::new(SessionOptions { mock_graph: true }).unwrap();
    let commands: Vec<String> = s.registry().iter().map(|c| c.id.to_owned()).collect();
    let queries: Vec<String> = s.queries().iter().map(|c| c.id.to_owned()).collect();
    for v in hostile_values() {
        for id in &commands {
            if let Err(e) = s.execute(id, v.clone()).await {
                assert_ne!(e.kind(), ErrorKind::Bug, "{id} panicked on {v}");
            }
        }
        for id in &queries {
            if let Err(e) = s.query(id, v.clone()).await {
                assert_ne!(e.kind(), ErrorKind::Bug, "{id} panicked on {v}");
            }
        }
    }
}

#[tokio::test]
async fn unknown_command_is_an_error() {
    let s = Session::new(SessionOptions::default()).unwrap();
    let e = s.execute("nope.nothing", Value::Null).await.err().map(|e| e.kind());
    assert_eq!(e, Some(ErrorKind::UnknownCommand));
}

#[tokio::test]
async fn guard_turns_panics_into_bug_errors() {
    let r: quorum_engine::Result<()> = quorum_engine::guard::guarded("test", async { panic!("boom") }).await;
    assert_eq!(r.err().map(|e| e.kind()), Some(ErrorKind::Bug));
}
