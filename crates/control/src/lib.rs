//! Local JSON-lines control channel for driving Quorum from scripts and tests.
//!
//! Binds to `127.0.0.1` only. The first line must authenticate with the random token written
//! to the token file (mode 0600). Then each line is `{"id":n,"method":"...","params":{...}}`
//! and each reply is `{"id":n,"result":...}` or `{"id":n,"error":{"kind":"...","message":"..."}}`.
//!
//! Methods: `auth`, `engine.execute`, `engine.query`, `engine.commands`, `engine.queries`,
//! `ui.screenshot` (see `docs/control-protocol.md`).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable,
    clippy::indexing_slicing
)]
#![warn(missing_docs)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use quorum_engine::Session;
use rand::RngCore;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// Longest accepted request line; longer lines close the connection.
const MAX_LINE: usize = 1 << 20;

/// Errors from the control channel.
#[derive(Debug, thiserror::Error)]
#[error("control channel: {0}")]
pub struct Error(#[from] std::io::Error);

/// UI operations the app provides to the control channel (screenshots).
pub trait UiHooks: Send + Sync {
    /// Captures the main window to a PNG at `path`.
    ///
    /// # Errors
    /// Returns a user-safe message when capture is not possible.
    fn screenshot(&self, path: &Path) -> Result<(), String>;
}

/// Where the control token lives: `control-token` in Quorum's data folder
/// (`~/Library/Application Support/Quorum/control-token` on macOS).
pub fn token_path() -> PathBuf {
    quorum_engine::account::data_dir().join("control-token")
}

fn write_token() -> Result<String, Error> {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let path = token_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, &token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(token)
}

/// Serves the control channel on `127.0.0.1:port` (0 picks a free port) until the process exits.
///
/// # Errors
/// Binding or writing the token file failed.
pub async fn serve(session: Session, port: u16) -> Result<(), Error> {
    serve_with(session, port, None).await
}

/// Like [`serve`], with UI hooks for screenshots.
///
/// # Errors
/// Binding or writing the token file failed.
pub async fn serve_with(session: Session, port: u16, hooks: Option<Arc<dyn UiHooks>>) -> Result<(), Error> {
    let token: Arc<str> = write_token()?.into();
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!(target: "quorum::control", addr = %listener.local_addr()?, "control channel listening");
    loop {
        let (stream, _) = listener.accept().await?;
        let (s, t, h) = (session.clone(), token.clone(), hooks.clone());
        tokio::spawn(async move {
            if let Err(e) = connection(stream, s, &t, h).await {
                tracing::debug!(target: "quorum::control", error = %e, "connection closed");
            }
        });
    }
}

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

/// Every command with its params documentation (for MCP tool lists).
fn commands_json(session: &Session) -> Value {
    let docs: std::collections::HashMap<&str, &str> = session.registry().iter().map(|c| (c.id, c.params_doc)).collect();
    Value::Array(
        session
            .commands()
            .into_iter()
            .map(|c| {
                let doc = docs.get(c.id.as_str()).copied().unwrap_or_default();
                json!({
                    "id": c.id, "label": c.label, "menu": c.menu, "shortcut": c.shortcut,
                    "destructive": c.destructive, "enabled": c.enabled, "params_doc": doc,
                })
            })
            .collect(),
    )
}

/// Every query (read-only) with its params documentation.
fn queries_json(session: &Session) -> Value {
    Value::Array(
        session
            .queries()
            .iter()
            .map(|q| json!({ "id": q.id, "label": q.label, "params_doc": q.params_doc, "destructive": false }))
            .collect(),
    )
}

async fn connection(
    stream: TcpStream,
    session: Session,
    token: &str,
    hooks: Option<Arc<dyn UiHooks>>,
) -> std::io::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    let mut authed = false;
    while let Some(line) = lines.next_line().await? {
        if line.len() > MAX_LINE {
            break;
        }
        let reply = match serde_json::from_str::<Request>(&line) {
            Err(_) => json!({ "id": null, "error": { "kind": "invalid_request", "message": "not a JSON request" } }),
            Ok(req) => {
                let id = req.id.clone();
                match handle(&session, token, &mut authed, hooks.as_deref(), req).await {
                    Ok(result) => json!({ "id": id, "result": result }),
                    Err((kind, message)) => json!({ "id": id, "error": { "kind": kind, "message": message } }),
                }
            }
        };
        let mut out = reply.to_string();
        out.push('\n');
        w.write_all(out.as_bytes()).await?;
    }
    Ok(())
}

async fn handle(
    session: &Session,
    token: &str,
    authed: &mut bool,
    hooks: Option<&dyn UiHooks>,
    req: Request,
) -> Result<Value, (String, String)> {
    let str_param = |k: &str| req.params.get(k).and_then(Value::as_str).map(str::to_owned);
    if req.method == "auth" {
        *authed = str_param("token").is_some_and(|t| t == token);
        return if *authed { Ok(Value::Bool(true)) } else { Err(("auth".into(), "bad token".into())) };
    }
    if !*authed {
        return Err(("auth".into(), "send {\"method\":\"auth\"} with the control token first".into()));
    }
    let engine_err = |e: quorum_engine::Error| (e.kind().as_str().to_owned(), e.message().to_owned());
    let params = req.params.get("params").cloned().unwrap_or(Value::Null);
    match req.method.as_str() {
        "engine.execute" => {
            let cmd = str_param("command").ok_or(("invalid_params".to_owned(), "missing `command`".to_owned()))?;
            session.execute(&cmd, params).await.map_err(engine_err)
        }
        "engine.query" => {
            let q = str_param("query").ok_or(("invalid_params".to_owned(), "missing `query`".to_owned()))?;
            session.query(&q, params).await.map_err(engine_err)
        }
        "engine.commands" => Ok(commands_json(session)),
        "engine.queries" => Ok(queries_json(session)),
        "ui.screenshot" => {
            let path = str_param("path").ok_or(("invalid_params".to_owned(), "missing `path`".to_owned()))?;
            let hooks = hooks.ok_or(("not_available".to_owned(), "no window in this process".to_owned()))?;
            hooks.screenshot(Path::new(&path)).map_err(|m| ("ui".to_owned(), m))?;
            Ok(json!({ "path": path }))
        }
        other => Err(("unknown_method".into(), format!("no method `{other}`"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str, params: Value) -> Request {
        Request { id: json!(1), method: method.into(), params }
    }

    #[tokio::test]
    async fn auth_gates_methods_and_lists_include_docs() {
        let Ok(s) = Session::new(quorum_engine::SessionOptions { mock_graph: true }) else { return };
        let mut authed = false;
        let r = handle(&s, "t", &mut authed, None, req("engine.commands", Value::Null)).await;
        assert!(matches!(r, Err((ref k, _)) if k == "auth"));
        let r = handle(&s, "t", &mut authed, None, req("auth", json!({ "token": "wrong" }))).await;
        assert!(r.is_err() && !authed);
        assert!(handle(&s, "t", &mut authed, None, req("auth", json!({ "token": "t" }))).await.is_ok());
        let cmds = handle(&s, "t", &mut authed, None, req("engine.commands", Value::Null)).await.unwrap_or_default();
        assert!(cmds.as_array().is_some_and(|a| a.iter().any(|c| c["id"] == "settings.set" && c["params_doc"] != "")));
        let qs = handle(&s, "t", &mut authed, None, req("engine.queries", Value::Null)).await.unwrap_or_default();
        assert!(qs.as_array().is_some_and(|a| a.iter().any(|q| q["id"] == "ui.state")));
        let r = handle(&s, "t", &mut authed, None, req("ui.screenshot", json!({ "path": "x.png" }))).await;
        assert!(matches!(r, Err((ref k, _)) if k == "not_available"));
        let r = handle(&s, "t", &mut authed, None, req("nope", Value::Null)).await;
        assert!(matches!(r, Err((ref k, _)) if k == "unknown_method"));
    }
}
