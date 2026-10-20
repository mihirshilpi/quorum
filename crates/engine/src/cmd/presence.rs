//! Presence: set your own status and status message, read anyone's, and tell sync whose
//! presence is on screen.
//!
//! Commands: `presence.set`, `presence.set_status_message`, `presence.clear_status_message`
//! (outbox writes with an optimistic local change), `presence.watch` (feeds the batched
//! presence poll). Queries: `presence.me`, `presence.of`.
//! The keep-alive toggle (`presence.keepalive`) belongs to the sync engine's own commands.

use std::time::Duration;

use jiff::Timestamp;
use quorum_model::{Presence, Sensitive, UserId};
use quorum_store::NewOutboxEntry;
use quorum_sync::people as sp;
use serde::Deserialize;
use serde_json::{Value, json};

use super::people::{presence_fields, presences_of, store_err, sync_err};
use crate::command::{CommandFuture, CommandSpec, always, parse};
use crate::error::{Error, Result};
use crate::session::Session;

/// Most ids one `presence.watch` or `presence.of` call accepts.
pub(crate) const MAX_IDS_PER_CALL: usize = 650;

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "presence.set",
            label: "Set Presence",
            menu: Some("Quorum/Set Presence"),
            shortcut: None,
            params_doc: r#"{"availability": "available"|"busy"|"dnd"|"brb"|"away"|"offline"|"reset", "duration_minutes"?: 1..10080}"#,
            destructive: false,
            enabled: always,
            run: set,
        },
        CommandSpec {
            id: "presence.set_status_message",
            label: "Set Status Message",
            menu: Some("Quorum/Set Status Message"),
            shortcut: None,
            params_doc: r#"{"message": string (<= 280 chars), "expires"?: RFC 3339 instant}"#,
            destructive: false,
            enabled: always,
            run: set_status_message,
        },
        CommandSpec {
            id: "presence.clear_status_message",
            label: "Clear Status Message",
            menu: Some("Quorum/Clear Status Message"),
            shortcut: None,
            params_doc: "{}",
            destructive: false,
            enabled: always,
            run: clear_status_message,
        },
        CommandSpec {
            id: "presence.watch",
            label: "Watch Presence",
            menu: None,
            shortcut: None,
            params_doc: r#"{"user_ids": [string], "ttl_secs"?: 10..3600 (default 300)} (sent by the UI for visible people)"#,
            destructive: false,
            enabled: always,
            run: watch,
        },
    ]
}

pub(crate) fn queries() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "presence.me",
            label: "My Presence",
            menu: None,
            shortcut: None,
            params_doc: "{} -> {user_id, presence, availability, activity, status_message, expiry, fetched_at}",
            destructive: false,
            enabled: always,
            run: me,
        },
        CommandSpec {
            id: "presence.of",
            label: "Presence of People",
            menu: None,
            shortcut: None,
            params_doc: r#"{"user_ids": [string]} -> {"<id>": {presence, availability, activity, status_message}}"#,
            destructive: false,
            enabled: always,
            run: of,
        },
    ]
}

#[derive(Deserialize)]
struct SetParams {
    availability: String,
    #[serde(default)]
    duration_minutes: Option<u64>,
    /// Alias of `duration_minutes`.
    #[serde(default)]
    duration: Option<u64>,
}

/// Enqueues a write to the user's own presence together with its optimistic local change.
async fn enqueue(
    s: &Session,
    kind: &'static str,
    payload: Value,
    change: impl FnOnce(Presence) -> Presence + Send + 'static,
) -> Result<()> {
    let store = s.store()?;
    let me = sp::my_id(&store).await.map_err(sync_err)?;
    let entry =
        NewOutboxEntry { kind: kind.into(), resource: sp::OWN_PRESENCE_RESOURCE.into(), local_id: None, payload };
    store
        .write(move |tx| {
            tx.outbox_enqueue(&entry)?;
            if let Some(me) = me {
                let current = tx.read().presences(std::slice::from_ref(&me))?.items.into_iter().next();
                let base = current
                    .map(|c| c.presence)
                    .unwrap_or_else(|| Presence { user_id: Some(me.clone()), ..Presence::default() });
                let mut next = change(base);
                next.user_id = Some(me);
                tx.upsert_presences(&[next], Timestamp::now())?;
            }
            Ok(())
        })
        .await
        .map_err(store_err)?;
    if let Ok(sync) = s.sync() {
        sync.outbox_changed();
    }
    Ok(())
}

fn set(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: SetParams = parse(p)?;
        let choice = sp::PresenceChoice::parse(&p.availability)
            .ok_or_else(|| Error::params("`availability` must be available, busy, dnd, brb, away, offline or reset"))?;
        let duration = match p.duration_minutes.or(p.duration) {
            None => None,
            Some(m) if (1..=sp::MAX_DURATION_MINUTES).contains(&m) => Some(Duration::from_secs(m.saturating_mul(60))),
            Some(_) => return Err(Error::params("`duration_minutes` must be 1 to 10080")),
        };
        if duration.is_some() && choice == sp::PresenceChoice::Reset {
            return Err(Error::params("`reset` takes no duration"));
        }
        let (kind, payload) = choice.outbox_payload(duration);
        let c = choice.clone();
        enqueue(s, kind, payload, move |cur| sp::with_choice(cur, &c)).await?;
        Ok(Value::Null)
    })
}

#[derive(Deserialize)]
struct StatusParams {
    message: String,
    #[serde(default)]
    expires: Option<String>,
}

async fn status(s: &Session, message: String, expiry: Option<Timestamp>) -> Result<()> {
    sp::validate_status_message(&message).map_err(|e| Error::params(e.message().to_owned()))?;
    let payload = json!({ "message": message, "expiry": expiry });
    enqueue(s, sp::KIND_STATUS_MESSAGE, payload, move |mut cur| {
        cur.status_message = if message.is_empty() { None } else { Some(Sensitive::new(message)) };
        cur.expiry = expiry;
        cur
    })
    .await
}

fn set_status_message(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: StatusParams = parse(p)?;
        let expiry = match p.expires.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
            None => None,
            Some(e) => {
                let t: Timestamp = e.parse().map_err(|_| Error::params("`expires` must be an RFC 3339 instant"))?;
                if t <= Timestamp::now() {
                    return Err(Error::params("`expires` must be in the future"));
                }
                Some(t)
            }
        };
        let message = p.message.trim().to_owned();
        if message.is_empty() {
            return Err(Error::params("`message` is empty; use presence.clear_status_message"));
        }
        status(s, message, expiry).await?;
        Ok(Value::Null)
    })
}

fn clear_status_message(s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        status(s, String::new(), None).await?;
        Ok(Value::Null)
    })
}

#[derive(Deserialize)]
struct IdsParams {
    user_ids: Vec<Value>,
    #[serde(default)]
    ttl_secs: Option<u64>,
}

/// Valid, distinct user ids from `raw` (non-strings and invalid ids are skipped).
pub(crate) fn user_ids(raw: &[Value]) -> Result<Vec<UserId>> {
    if raw.len() > MAX_IDS_PER_CALL {
        return Err(Error::params("at most 650 user ids per call"));
    }
    let mut out: Vec<UserId> =
        raw.iter().filter_map(Value::as_str).filter_map(|s| UserId::new(s.trim()).ok()).collect();
    out.sort();
    out.dedup();
    Ok(out)
}

fn watch(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: IdsParams = parse(p)?;
        let ids = user_ids(&p.user_ids)?;
        let ttl = p.ttl_secs.map_or(sp::DEFAULT_WATCH_TTL, Duration::from_secs);
        let set = sp::PresenceWatch::global();
        let added = set.watch(&ids, ttl);
        if added > 0
            && let Ok(sync) = s.sync()
        {
            sync.poke("presence.watch");
        }
        Ok(json!({ "watching": set.len(), "added": added }))
    })
}

fn me(s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let store = s.store()?;
        let Some(id) = sp::my_id(&store).await.map_err(sync_err)? else {
            return Ok(json!({ "user_id": null, "presence": "unknown", "availability": null, "activity": null,
                "status_message": null, "expiry": null, "fetched_at": null }));
        };
        let map = presences_of(s, vec![id.clone()]).await?;
        let cached = map.get(&id);
        let mut out = json!({
            "user_id": id.as_str(),
            "expiry": cached.and_then(|c| c.presence.expiry),
            "fetched_at": cached.map(|c| c.fetched_at),
        });
        if let Some(obj) = out.as_object_mut() {
            for (k, v) in presence_fields(cached) {
                obj.insert(k.to_owned(), v);
            }
        }
        Ok(out)
    })
}

fn of(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: IdsParams = parse(p)?;
        let ids = user_ids(&p.user_ids)?;
        let map = presences_of(s, ids).await?;
        let mut out = serde_json::Map::new();
        for (id, c) in &map {
            out.insert(
                id.as_str().to_owned(),
                Value::Object(presence_fields(Some(c)).into_iter().map(|(k, v)| (k.to_owned(), v)).collect()),
            );
        }
        Ok(Value::Object(out))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;

    #[test]
    fn user_ids_skip_garbage_and_dedupe() {
        let raw = vec![json!("a"), json!("a"), json!(5), json!(""), json!("bad\u{7}id"), json!(" b ")];
        let ids = user_ids(&raw).unwrap_or_default();
        assert_eq!(ids.iter().map(UserId::as_str).collect::<Vec<_>>(), vec!["a", "b"]);
        assert!(user_ids(&vec![json!("x"); 651]).is_err());
    }

    #[test]
    fn errors_map_kinds() {
        let e = sync_err(quorum_sync::Error::new(quorum_sync::ErrorKind::Store, "x"));
        assert_eq!(e.kind(), ErrorKind::Store);
    }
}
