//! Sync commands: sync now, pause/resume, activity and focus, the outbox's failed writes,
//! and the presence keep-alive toggle.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::command::{CommandFuture, CommandSpec, always, parse};
use crate::error::{Error, ErrorKind, Result};
use crate::session::Session;

/// Longest focus string accepted (`mail`, `chat:<id>`, ...).
const MAX_FOCUS_LEN: usize = 256;
/// Longest poke key accepted.
const MAX_KEY_LEN: usize = 1024;
/// Most failed writes listed by `outbox.failed`.
const MAX_FAILED_LIST: u32 = 500;

fn spec(
    id: &'static str,
    label: &'static str,
    menu: Option<&'static str>,
    shortcut: Option<&'static str>,
    params_doc: &'static str,
    destructive: bool,
    run: crate::command::RunFn,
) -> CommandSpec {
    CommandSpec { id, label, menu, shortcut, params_doc, destructive, enabled: always, run }
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        spec(
            "sync.now",
            "Sync Now",
            Some("View/Sync Now"),
            None,
            r#"{"key"?: string} (a resource key or prefix, e.g. "mail.folder:"; omitted = everything)"#,
            false,
            sync_now,
        ),
        spec("sync.pause", "Pause Sync", Some("View/Pause Sync"), None, "{}", false, pause),
        spec("sync.resume", "Resume Sync", Some("View/Resume Sync"), None, "{}", false, resume),
        spec(
            "sync.set_activity",
            "Set Sync Activity",
            None,
            None,
            r#"{"activity": "focused" | "background" | "idle"} (sent by the shell)"#,
            false,
            set_activity,
        ),
        spec(
            "sync.focus",
            "Set Sync Focus",
            None,
            None,
            r#"{"area": string} (an area such as "mail", or finer such as "chat:<id>"; "" clears)"#,
            false,
            focus,
        ),
        spec("outbox.retry", "Retry Failed Write", None, None, r#"{"id": integer}"#, false, retry),
        spec(
            "outbox.discard",
            "Discard Failed Write",
            None,
            None,
            r#"{"id": integer} (undoes the local change and drops the write)"#,
            true,
            discard,
        ),
        spec(
            "presence.keepalive",
            "Keep Me Available",
            Some("Teams/Keep Me Available"),
            None,
            r#"{"enabled": bool} (also the setting presence.keep_alive)"#,
            false,
            set_keepalive,
        ),
    ]
}

pub(crate) fn queries() -> Vec<CommandSpec> {
    vec![
        spec("sync.status", "Sync Status", None, None, "{}", false, status),
        spec(
            "outbox.failed",
            "Failed Writes",
            None,
            None,
            r#"{"limit"?: integer (default 100, max 500)}"#,
            false,
            failed,
        ),
        spec("presence.keepalive", "Keep-Alive State", None, None, "{}", false, keepalive_state),
    ]
}

/// Maps a sync error onto the engine's error kinds.
fn sync_err(e: quorum_sync::Error) -> Error {
    let kind = match e.kind() {
        quorum_sync::ErrorKind::Store => ErrorKind::Store,
        quorum_sync::ErrorKind::Auth => ErrorKind::Auth,
        quorum_sync::ErrorKind::Invalid => ErrorKind::InvalidParams,
        quorum_sync::ErrorKind::Bug => ErrorKind::Bug,
        _ => ErrorKind::Remote,
    };
    let message = e.message().to_owned();
    Error::new(kind, message).with_source(e)
}

fn object(p: Value) -> Result<Value> {
    let p = if p.is_null() { json!({}) } else { p };
    if !p.is_object() {
        return Err(Error::params("parameters must be an object"));
    }
    Ok(p)
}

/// Makes the sync engine's focus follow a `ui.report` (`{"area": ...}`); a no-op before
/// sign-in or without an area.
pub(crate) fn follow_focus(s: &Session, report: &Value) {
    if let (Ok(sync), Some(area)) = (s.sync(), report.get("area").and_then(Value::as_str))
        && area.len() <= MAX_FOCUS_LEN
    {
        sync.set_focus(area);
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NowParams {
    #[serde(default)]
    key: Option<String>,
}

fn sync_now(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: NowParams = parse(object(p)?)?;
        let key = p.key.unwrap_or_default();
        if key.len() > MAX_KEY_LEN || key.chars().any(char::is_control) {
            return Err(Error::params("key is too long or has control characters"));
        }
        let sync = s.sync()?;
        sync.poke(&key);
        sync.outbox_changed();
        Ok(json!({ "poked": key }))
    })
}

fn pause(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        object(p)?;
        s.sync()?.pause();
        Ok(Value::Null)
    })
}

fn resume(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        object(p)?;
        s.sync()?.resume();
        Ok(Value::Null)
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivityParams {
    activity: String,
}

fn set_activity(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: ActivityParams = parse(object(p)?)?;
        let activity = quorum_sync::Activity::parse(&p.activity)
            .ok_or_else(|| Error::params("activity must be focused, background or idle"))?;
        s.sync()?.set_activity(activity);
        Ok(Value::Null)
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FocusParams {
    area: String,
}

fn focus(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: FocusParams = parse(object(p)?)?;
        if p.area.len() > MAX_FOCUS_LEN || p.area.chars().any(char::is_control) {
            return Err(Error::params("area is too long or has control characters"));
        }
        s.sync()?.set_focus(&p.area);
        Ok(Value::Null)
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdParams {
    id: i64,
}

fn id_param(p: Value) -> Result<quorum_store::OutboxId> {
    let p: IdParams = parse(object(p)?)?;
    if p.id <= 0 {
        return Err(Error::params("id must be a positive integer"));
    }
    Ok(quorum_store::OutboxId(p.id))
}

fn retry(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let id = id_param(p)?;
        let sync = s.sync()?;
        sync.retry_failed(id).await.map_err(|e| {
            if e.kind() == quorum_sync::ErrorKind::Store {
                Error::new(ErrorKind::NotFound, "no queued write with that id").with_source(e)
            } else {
                sync_err(e)
            }
        })?;
        Ok(Value::Null)
    })
}

fn discard(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let id = id_param(p)?;
        let out = s.sync()?.discard_failed(id).await.map_err(sync_err)?;
        if !out.discarded {
            return Err(Error::new(ErrorKind::NotFound, "no failed write with that id"));
        }
        Ok(serde_json::to_value(out)?)
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeepaliveParams {
    enabled: bool,
}

fn set_keepalive(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: KeepaliveParams = parse(object(p)?)?;
        let sync = s.sync()?;
        super::settings::set(s, "presence.keep_alive", &Value::Bool(p.enabled)).await?;
        sync.set_keepalive(p.enabled);
        Ok(json!({ "enabled": p.enabled }))
    })
}

fn status(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        object(p)?;
        let st = s.sync()?.refresh_status().await;
        Ok(serde_json::to_value(st)?)
    })
}

fn keepalive_state(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        object(p)?;
        let st = s.sync()?.status();
        Ok(json!({ "enabled": st.keepalive, "active": st.keepalive_active }))
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FailedParams {
    #[serde(default)]
    limit: Option<u32>,
}

fn failed(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: FailedParams = parse(object(p)?)?;
        let limit = p.limit.unwrap_or(100).clamp(1, MAX_FAILED_LIST);
        let page = s
            .store()?
            .outbox_failed(limit)
            .await
            .map_err(|e| Error::new(ErrorKind::Store, "cannot read failed writes").with_source(e))?;
        // Payloads can hold message text: list metadata only.
        let items: Vec<Value> = page
            .items
            .iter()
            .map(|e| {
                json!({
                    "id": e.id,
                    "kind": e.kind,
                    "resource": e.resource,
                    "local_id": e.local_id,
                    "attempts": e.attempts,
                    "error": e.last_error,
                    "created_at": e.created_at,
                })
            })
            .collect();
        Ok(json!({ "items": items, "skipped": page.skipped }))
    })
}
