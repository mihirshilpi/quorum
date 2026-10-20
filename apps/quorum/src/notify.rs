//! Native notifications and the Dock badge, driven by store change events.
//!
//! When the store reports changes to mail, chat or calendar tables the notifier re-reads the
//! same queries the UI uses and diffs them against what it has seen: the first read only seeds
//! the seen set, so starting Quorum never floods the user. Rules: per-area switches
//! (`notifications.mail|chat|meetings`), and quiet while presence is Do not disturb
//! (`notifications.quiet_when_dnd`). Field names are read leniently so the notifier keeps
//! working while query shapes evolve.

use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use quorum_engine::Session;
use quorum_ipc::CoreEvent;
use serde_json::{Value, json};

/// Most ids remembered per area.
const MAX_SEEN: usize = 5_000;
/// Most notifications shown for one refresh (the rest are summarised).
const MAX_BURST: usize = 3;
/// Longest notification title / body.
const MAX_TEXT: usize = 200;

/// A notification to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// Title (sender, chat or meeting).
    pub title: String,
    /// Body (subject or preview).
    pub body: String,
}

/// Remembers which items were already announced (bounded, insertion-ordered).
#[derive(Debug, Default)]
pub struct Seen {
    set: HashSet<String>,
    order: VecDeque<String>,
    seeded: bool,
}

impl Seen {
    /// Returns rows not seen before (all rows count as seen on the first call).
    pub fn fresh<'a>(&mut self, rows: impl IntoIterator<Item = (&'a str, &'a Value)>) -> Vec<&'a Value> {
        let mut out = Vec::new();
        for (id, row) in rows {
            if id.is_empty() || self.set.contains(id) {
                continue;
            }
            self.set.insert(id.to_owned());
            self.order.push_back(id.to_owned());
            if self.order.len() > MAX_SEEN
                && let Some(old) = self.order.pop_front()
            {
                self.set.remove(&old);
            }
            if self.seeded {
                out.push(row);
            }
        }
        self.seeded = true;
        out
    }
}

fn text(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| match v.get(*k) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Object(o)) => o.get("name").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    })
}

fn clip(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(MAX_TEXT).collect()
}

/// Whether a row is unread (`unread: true`, `unread: n > 0`, or `is_read: false`).
pub fn is_unread(v: &Value) -> bool {
    match v.get("unread") {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_u64().is_some_and(|n| n > 0),
        _ => v.get("is_read").and_then(Value::as_bool).is_some_and(|r| !r),
    }
}

/// Rows of a list query result (`[...]`, `{"items": [...]}` or `{"rows": [...]}`).
pub fn rows(v: &Value) -> Vec<Value> {
    v.as_array()
        .or_else(|| v.get("items").and_then(Value::as_array))
        .or_else(|| v.get("rows").and_then(Value::as_array))
        .map(|a| a.iter().take(200).cloned().collect())
        .unwrap_or_default()
}

/// New unread mail rows → notifications.
pub fn mail_notes(seen: &mut Seen, list: &[Value]) -> Vec<Note> {
    let ids: Vec<(String, &Value)> = list
        .iter()
        .filter(|r| is_unread(r))
        .filter_map(|r| r.get("id").and_then(Value::as_str).map(|id| (id.to_owned(), r)))
        .collect();
    let fresh = seen.fresh(ids.iter().map(|(id, r)| (id.as_str(), *r)));
    fresh
        .into_iter()
        .map(|r| Note {
            title: clip(&text(r, &["from_name", "from", "sender"]).unwrap_or_else(|| "New mail".into())),
            body: clip(&text(r, &["subject"]).unwrap_or_default()),
        })
        .collect()
}

/// Chats with new unread activity → notifications. A chat is "new" when its id plus last
/// activity changed while it is unread.
pub fn chat_notes(seen: &mut Seen, list: &[Value]) -> Vec<Note> {
    let keyed: Vec<(String, &Value)> = list
        .iter()
        .filter(|r| is_unread(r))
        .filter_map(|r| {
            let id = r.get("id").and_then(Value::as_str)?;
            let stamp = text(r, &["time", "last_activity", "updated"]).unwrap_or_default();
            let preview = text(r, &["last_message_preview", "preview"]).unwrap_or_default();
            Some((format!("{id}|{stamp}|{}", preview.len()), r))
        })
        .collect();
    let fresh = seen.fresh(keyed.iter().map(|(k, r)| (k.as_str(), *r)));
    fresh
        .into_iter()
        .map(|r| {
            let who = text(r, &["from_name", "last_sender"]);
            let chat = text(r, &["title", "topic", "name"]);
            let title = match (who, chat) {
                (Some(w), Some(c)) if w != c => format!("{w} in {c}"),
                (Some(w), _) => w,
                (None, Some(c)) => c,
                (None, None) => "New chat message".into(),
            };
            Note { title: clip(&title), body: clip(&text(r, &["last_message_preview", "preview"]).unwrap_or_default()) }
        })
        .collect()
}

/// Meetings starting within `minutes` that were not announced yet.
pub fn meeting_notes(seen: &mut Seen, list: &[Value], now: jiff::Timestamp, minutes: i64) -> Vec<Note> {
    let window = jiff::SignedDuration::from_mins(minutes.clamp(0, 240));
    let due: Vec<(String, &Value)> = list
        .iter()
        .filter(|r| {
            let flag = |k: &str| r.get(k).and_then(Value::as_bool);
            let ts = |k: &str| r.get(k).and_then(Value::as_str).and_then(|s| s.parse::<jiff::Timestamp>().ok());
            let all_day = flag("all_day") == Some(true) || flag("is_all_day") == Some(true);
            let skip = flag("is_cancelled") == Some(true)
                || flag("is_reminder_on") == Some(false)
                || r.get("response").and_then(Value::as_str) == Some("declined");
            let Some(start) = ts("start") else { return false };
            let until = now.duration_until(start);
            let due = match ts("reminder_at") {
                Some(at) => at <= now,
                None => until <= window,
            };
            !all_day && !skip && until >= jiff::SignedDuration::ZERO && due
        })
        .filter_map(|r| {
            let id = r.get("id").and_then(Value::as_str)?;
            let start = r.get("start").and_then(Value::as_str).unwrap_or_default();
            Some((format!("{id}|{start}"), r))
        })
        .collect();
    // Meetings use their own seen set without seeding: a reminder is due regardless of start-up.
    seen.seeded = true;
    seen.fresh(due.iter().map(|(k, r)| (k.as_str(), *r)))
        .into_iter()
        .map(|r| Note {
            title: clip(&text(r, &["subject", "title"]).unwrap_or_else(|| "Meeting".into())),
            body: clip(&match text(r, &["location"]) {
                Some(l) => format!("Starts soon · {l}"),
                None => "Starts soon".to_owned(),
            }),
        })
        .collect()
}

/// Caps a burst: at most [`MAX_BURST`] notes, then a summary.
pub fn burst(mut notes: Vec<Note>, what: &str) -> Vec<Note> {
    if notes.len() > MAX_BURST {
        let extra = notes.len() - MAX_BURST;
        notes.truncate(MAX_BURST);
        notes.push(Note { title: "Quorum".into(), body: format!("{extra} more {what}") });
    }
    notes
}

/// Unread count of the inbox in a `mail.folders` result.
pub fn inbox_unread(folders: &Value) -> Option<i64> {
    rows(folders).iter().find_map(|f| {
        let is_inbox = ["kind", "id", "well_known_name", "wellKnownName"]
            .iter()
            .any(|k| f.get(*k).and_then(Value::as_str).is_some_and(|v| v.eq_ignore_ascii_case("inbox")));
        is_inbox.then(|| f.get("unread").or_else(|| f.get("unread_count")).and_then(Value::as_i64)).flatten()
    })
}

async fn setting_bool(s: &Session, key: &str) -> bool {
    quorum_engine::settings::get(s, key).await.ok().and_then(|v| v.as_bool()).unwrap_or(false)
}

async fn query_rows(s: &Session, id: &str, params: Value) -> Option<Vec<Value>> {
    s.query(id, params).await.ok().map(|v| rows(&v))
}

/// Own presence short id (`presence.me`), if known.
pub async fn own_presence(s: &Session) -> Option<String> {
    let v = s.query("presence.me", json!({})).await.ok()?;
    v.get("presence").or_else(|| v.get("availability")).and_then(Value::as_str).map(str::to_ascii_lowercase)
}

/// Platform hooks the notifier drives.
pub trait Sink: Send + Sync + 'static {
    /// Shows a notification.
    fn show(&self, note: &Note);
    /// Sets (or clears, with `None`) the Dock badge.
    fn badge(&self, count: Option<i64>);
}

/// Runs the notifier until the session's event channel closes.
pub async fn run(session: Session, sink: impl Sink) {
    let mut events = session.subscribe();
    let (mut mail, mut chat, mut meetings) = (Seen::default(), Seen::default(), Seen::default());
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let refresh = tokio::select! {
            ev = events.recv() => match ev {
                Ok(CoreEvent::StoreChanged { tables }) => Some(tables),
                Ok(CoreEvent::Account { .. }) => Some(vec!["*".to_owned()]),
                Ok(_) => None,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Some(vec!["*".to_owned()]),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            _ = tick.tick() => Some(vec!["calendar".to_owned()]),
        };
        let Some(tables) = refresh else { continue };
        // Coalesce bursts of change events.
        tokio::time::sleep(Duration::from_millis(400)).await;
        while let Ok(ev) = events.try_recv() {
            let _ = ev;
        }
        let touches = |prefixes: &[&str]| tables.iter().any(|t| t == "*" || prefixes.iter().any(|p| t.starts_with(p)));
        let quiet = setting_bool(&session, "notifications.quiet_when_dnd").await
            && own_presence(&session).await.is_some_and(|p| p == "dnd" || p == "donotdisturb");
        let mut notes = Vec::new();
        if touches(&["message", "mail", "folder"]) {
            if let Some(list) = query_rows(&session, "mail.list", json!({ "folder": "inbox", "limit": 20 })).await {
                let n = mail_notes(&mut mail, &list);
                if setting_bool(&session, "notifications.mail").await {
                    notes.extend(burst(n, "new messages"));
                }
            }
            if let Ok(folders) = session.query("mail.folders", json!({})).await {
                let on = setting_bool(&session, "app.dock_badge").await;
                sink.badge(if on { inbox_unread(&folders).filter(|n| *n > 0) } else { None });
            }
        }
        if touches(&["chat"])
            && let Some(list) = query_rows(&session, "chat.list", json!({ "limit": 20 })).await
        {
            let n = chat_notes(&mut chat, &list);
            if setting_bool(&session, "notifications.chat").await {
                notes.extend(burst(n, "chats with new messages"));
            }
        }
        if touches(&["calendar", "event"]) && setting_bool(&session, "notifications.meetings").await {
            let now = jiff::Timestamp::now();
            let minutes = quorum_engine::settings::get(&session, "notifications.reminder_minutes")
                .await
                .ok()
                .and_then(|v| v.as_i64())
                .unwrap_or(5);
            let end = now.checked_add(jiff::SignedDuration::from_hours(2)).unwrap_or(now);
            let params = json!({ "from": now.to_string(), "to": end.to_string() });
            if let Some(list) = query_rows(&session, "calendar.upcoming", params).await {
                notes.extend(burst(meeting_notes(&mut meetings, &list, now, minutes), "meetings"));
            }
        }
        if !quiet {
            for n in &notes {
                sink.show(n);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_read_seeds_then_only_new_unread_mail_notifies() {
        let mut seen = Seen::default();
        let first = vec![json!({"id": "a", "unread": true, "from_name": "Avery", "subject": "Hi"})];
        assert!(mail_notes(&mut seen, &first).is_empty());
        let second = vec![
            json!({"id": "b", "unread": true, "from_name": "Blake", "subject": "Lunch"}),
            json!({"id": "c", "unread": false, "subject": "Read"}),
            json!({"id": "a", "unread": true}),
        ];
        let notes = mail_notes(&mut seen, &second);
        assert_eq!(notes, vec![Note { title: "Blake".into(), body: "Lunch".into() }]);
        assert!(mail_notes(&mut seen, &second).is_empty());
    }

    #[test]
    fn chats_notify_on_new_activity() {
        let mut seen = Seen::default();
        let r = |t: &str| json!({"id": "c1", "unread": true, "from_name": "Kai", "title": "Design", "time": t, "preview": "ok"});
        assert!(chat_notes(&mut seen, &[r("1")]).is_empty());
        let n = chat_notes(&mut seen, &[r("2")]);
        assert_eq!(n.first().map(|n| n.title.as_str()), Some("Kai in Design"));
    }

    #[test]
    fn meetings_remind_once_inside_the_window() {
        let now: jiff::Timestamp = "2026-10-17T10:00:00Z".parse().unwrap_or(jiff::Timestamp::UNIX_EPOCH);
        let list = vec![
            json!({"id": "m1", "start": "2026-10-17T10:04:00Z", "subject": "Standup", "location": "Room 1"}),
            json!({"id": "m2", "start": "2026-10-17T11:00:00Z", "subject": "Later"}),
            json!({"id": "m3", "start": "2026-10-17T09:00:00Z", "subject": "Past"}),
            json!({"id": "m4", "start": "2026-10-17T10:01:00Z", "subject": "All day", "all_day": true}),
            json!({"id": "m6", "start": "2026-10-17T10:30:00Z", "subject": "Early", "reminder_at": "2026-10-17T09:59:00Z"}),
            json!({"id": "m7", "start": "2026-10-17T10:02:00Z", "subject": "Declined", "response": "declined"}),
            json!({"id": "m5", "start": "garbage"}),
        ];
        let mut seen = Seen::default();
        let n = meeting_notes(&mut seen, &list, now, 5);
        let titles: Vec<&str> = n.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(titles, ["Standup", "Early"]);
        assert!(meeting_notes(&mut seen, &list, now, 5).is_empty());
    }

    #[test]
    fn bursts_are_summarised_and_badges_found() {
        let notes = (0..10).map(|i| Note { title: i.to_string(), body: String::new() }).collect();
        let b = burst(notes, "new messages");
        assert_eq!(b.len(), MAX_BURST + 1);
        assert_eq!(b.last().map(|n| n.body.as_str()), Some("7 more new messages"));
        assert_eq!(inbox_unread(&json!([{"id": "x", "kind": "inbox", "unread": 4}])), Some(4));
        assert_eq!(inbox_unread(&json!({"items": [{"id": "inbox", "unread_count": 2}]})), Some(2));
        assert_eq!(inbox_unread(&json!("junk")), None);
        assert!(rows(&json!(5)).is_empty());
    }

    #[test]
    fn seen_is_bounded() {
        let mut seen = Seen::default();
        let v = json!({});
        let ids: Vec<String> = (0..MAX_SEEN + 10).map(|i| i.to_string()).collect();
        let _ = seen.fresh(ids.iter().map(|i| (i.as_str(), &v)));
        assert_eq!(seen.set.len(), MAX_SEEN);
    }
}
