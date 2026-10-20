//! MCP server (JSON-RPC 2.0 over stdio) exposing Quorum commands and queries to agents.
//!
//! Every engine command becomes a tool named after its id with dots replaced by `__`
//! (`mail.reply` → `mail__reply`); every query becomes a read-only tool prefixed `query__`.
//! Destructive commands (send, post, delete, share) carry `destructiveHint` and, under the
//! default [`DestructivePolicy::RequireConfirm`], only run when the call passes `"confirm": true`.
//!
//! The server either drives an in-process [`Session`] or bridges to a running app over the
//! control channel ([`Backend::Attached`]), so an agent can operate the window the user sees.
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

use quorum_engine::Session;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::Mutex;

/// MCP protocol revision this server speaks.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Longest accepted JSON-RPC line.
const MAX_LINE: usize = 4 << 20;

/// How destructive commands are treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DestructivePolicy {
    /// Never run destructive commands.
    Deny,
    /// Run only when the tool call carries `"confirm": true` (default).
    #[default]
    RequireConfirm,
    /// Run without confirmation (the user opted out).
    Allow,
}

/// Errors from the MCP server.
#[derive(Debug, thiserror::Error)]
#[error("mcp: {0}")]
pub struct Error(#[from] std::io::Error);

/// What the server drives.
#[derive(Debug)]
pub enum Backend {
    /// An in-process session (headless).
    Local(Session),
    /// A running Quorum app reached over its control channel.
    Attached(Attached),
}

/// A connection to a running app's control channel.
#[derive(Debug)]
pub struct Attached {
    io: Mutex<(BufReader<OwnedReadHalf>, OwnedWriteHalf)>,
}

impl Attached {
    /// Connects to `127.0.0.1:port` and authenticates with `token`.
    ///
    /// # Errors
    /// Connection or authentication failed.
    pub async fn connect(port: u16, token: &str) -> Result<Self, Error> {
        let (r, w) = TcpStream::connect(("127.0.0.1", port)).await?.into_split();
        let me = Self { io: Mutex::new((BufReader::new(r), w)) };
        me.call("auth", json!({ "token": token })).await.map_err(|m| std::io::Error::other(m.1))?;
        Ok(me)
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, (String, String)> {
        let mut io = self.io.lock().await;
        let line = json!({ "id": 1, "method": method, "params": params }).to_string() + "\n";
        let io_err = |e: std::io::Error| ("remote".to_owned(), e.to_string());
        io.1.write_all(line.as_bytes()).await.map_err(io_err)?;
        let mut reply = String::new();
        io.0.read_line(&mut reply).await.map_err(io_err)?;
        let v: Value = serde_json::from_str(&reply).map_err(|e| ("remote".to_owned(), e.to_string()))?;
        if let Some(err) = v.get("error") {
            let s = |k: &str| err.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
            return Err((s("kind"), s("message")));
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }
}

#[derive(Debug, Clone)]
struct ToolInfo {
    id: String,
    label: String,
    doc: String,
    destructive: bool,
    query: bool,
}

/// The MCP server.
#[derive(Debug)]
pub struct Server {
    backend: Backend,
    policy: DestructivePolicy,
}

fn tool_name(id: &str, query: bool) -> String {
    let base = id.replace('.', "__");
    if query { format!("query__{base}") } else { base }
}

impl Server {
    /// Creates a server over `backend`.
    pub fn new(backend: Backend, policy: DestructivePolicy) -> Self {
        Self { backend, policy }
    }

    async fn tools(&self) -> Result<Vec<ToolInfo>, (String, String)> {
        match &self.backend {
            Backend::Local(s) => {
                let mut v: Vec<ToolInfo> = s
                    .registry()
                    .iter()
                    .map(|c| ToolInfo {
                        id: c.id.into(),
                        label: c.label.into(),
                        doc: c.params_doc.into(),
                        destructive: c.destructive,
                        query: false,
                    })
                    .collect();
                v.extend(s.queries().iter().map(|c| ToolInfo {
                    id: c.id.into(),
                    label: c.label.into(),
                    doc: c.params_doc.into(),
                    destructive: false,
                    query: true,
                }));
                Ok(v)
            }
            Backend::Attached(a) => {
                let mut out = Vec::new();
                for (method, query) in [("engine.commands", false), ("engine.queries", true)] {
                    let list = a.call(method, Value::Null).await?;
                    for c in list.as_array().into_iter().flatten() {
                        let s = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
                        out.push(ToolInfo {
                            id: s("id"),
                            label: s("label"),
                            doc: s("params_doc"),
                            destructive: !query && c.get("destructive").and_then(Value::as_bool).unwrap_or(true),
                            query,
                        });
                    }
                }
                Ok(out)
            }
        }
    }

    async fn call_tool(&self, name: &str, mut args: Map<String, Value>) -> Result<Value, (String, String)> {
        let tools = self.tools().await?;
        let tool = tools
            .iter()
            .find(|t| tool_name(&t.id, t.query) == name)
            .ok_or_else(|| ("unknown_command".to_owned(), format!("no tool `{name}`")))?;
        let confirmed = args.remove("confirm").and_then(|v| v.as_bool()).unwrap_or(false);
        if tool.destructive {
            match self.policy {
                DestructivePolicy::Deny => return Err(("denied".into(), "destructive commands are disabled".into())),
                DestructivePolicy::RequireConfirm if !confirmed => {
                    return Err((
                        "confirm_required".into(),
                        format!(
                            "`{}` sends, posts, deletes or shares; ask the user, then call again with \"confirm\": true",
                            tool.id
                        ),
                    ));
                }
                _ => {}
            }
        }
        let params = Value::Object(args);
        let engine_err = |e: quorum_engine::Error| (e.kind().as_str().to_owned(), e.message().to_owned());
        match &self.backend {
            Backend::Local(s) if tool.query => s.query(&tool.id, params).await.map_err(engine_err),
            Backend::Local(s) => s.execute(&tool.id, params).await.map_err(engine_err),
            Backend::Attached(a) if tool.query => {
                a.call("engine.query", json!({ "query": tool.id, "params": params })).await
            }
            Backend::Attached(a) => a.call("engine.execute", json!({ "command": tool.id, "params": params })).await,
        }
    }

    /// Handles one JSON-RPC message; returns the response, or `None` for notifications.
    pub async fn handle(&self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result: Result<Value, (i64, String)> = match method {
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "quorum", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "Quorum: Microsoft 365 mail, calendar, Teams chat/channels, presence and files. \
                    Tools named query__* only read. Destructive tools need \"confirm\": true after the user agrees."
            })),
            "ping" => Ok(json!({})),
            "tools/list" => match self.tools().await {
                Ok(tools) => Ok(json!({ "tools": tools.iter().map(|t| json!({
                    "name": tool_name(&t.id, t.query),
                    "title": t.label,
                    "description": format!("{} (`{}`). Params: {}", t.label, t.id, if t.doc.is_empty() { "see app" } else { &t.doc }),
                    "inputSchema": { "type": "object", "additionalProperties": true },
                    "annotations": { "readOnlyHint": t.query, "destructiveHint": t.destructive },
                })).collect::<Vec<_>>() })),
                Err((_, m)) => Err((-32000, m)),
            },
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
                let args = params.get("arguments").and_then(Value::as_object).cloned().unwrap_or_default();
                Ok(match self.call_tool(name, args).await {
                    Ok(v) => {
                        json!({ "content": [{ "type": "text", "text": v.to_string() }], "structuredContent": { "result": v }, "isError": false })
                    }
                    Err((kind, m)) => {
                        json!({ "content": [{ "type": "text", "text": format!("{kind}: {m}") }], "isError": true })
                    }
                })
            }
            _ if id.is_none() => return None,
            other => Err((-32601, format!("method not found: {other}"))),
        };
        let id = id?;
        Some(match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err((code, message)) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
            }
        })
    }

    /// Serves newline-delimited JSON-RPC on `input`/`output` until EOF.
    ///
    /// # Errors
    /// Reading or writing failed.
    pub async fn serve<R, W>(&self, input: R, mut output: W) -> Result<(), Error>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut lines = input.lines();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let reply = if line.len() > MAX_LINE {
                Some(
                    json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32600, "message": "request too large" } }),
                )
            } else {
                match serde_json::from_str::<Value>(&line) {
                    Ok(msg) => self.handle(&msg).await,
                    Err(_) => Some(
                        json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "parse error" } }),
                    ),
                }
            };
            if let Some(r) = reply {
                output.write_all((r.to_string() + "\n").as_bytes()).await?;
                output.flush().await?;
            }
        }
        Ok(())
    }

    /// Serves on the process's stdin/stdout.
    ///
    /// # Errors
    /// Reading or writing failed.
    pub async fn serve_stdio(&self) -> Result<(), Error> {
        self.serve(BufReader::new(tokio::io::stdin()), tokio::io::stdout()).await
    }
}
