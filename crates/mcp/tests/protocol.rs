//! MCP protocol round trips against a headless session.

use quorum_engine::{Session, SessionOptions};
use quorum_mcp::{Backend, DestructivePolicy, Server};
use serde_json::{Value, json};

fn server() -> Server {
    Server::new(
        Backend::Local(Session::new(SessionOptions { mock_graph: true }).unwrap()),
        DestructivePolicy::RequireConfirm,
    )
}

#[tokio::test]
async fn initialize_and_list_tools() {
    let s = server();
    let init = s.handle(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})).await.unwrap_or_default();
    assert_eq!(init["result"]["serverInfo"]["name"], "quorum");
    let list = s.handle(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).await.unwrap_or_default();
    let names: Vec<&str> =
        list["result"]["tools"].as_array().into_iter().flatten().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"app__about"));
    assert!(names.contains(&"query__app__commands"));
}

#[tokio::test]
async fn call_tool_runs_command() {
    let s = server();
    let r = s
        .handle(&json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"app__about","arguments":{}}}))
        .await
        .unwrap_or_default();
    assert_eq!(r["result"]["isError"], false);
    assert_eq!(r["result"]["structuredContent"]["result"]["name"], "Quorum");
}

#[tokio::test]
async fn notifications_get_no_reply_and_garbage_is_survived() {
    let s = server();
    assert!(s.handle(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await.is_none());
    let input = b"not json\n{\"id\":1}\n\n" as &[u8];
    let mut out = Vec::new();
    s.serve(tokio::io::BufReader::new(input), &mut out).await.unwrap_or_default();
    let text = String::from_utf8_lossy(&out);
    assert!(text.lines().all(|l| serde_json::from_str::<Value>(l).is_ok()));
}
