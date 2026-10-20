//! Browser-preview answers for the people, presence and search queries and commands, with
//! synthetic `example.com` people and the same shapes the engine returns. Wired into
//! [`crate::mock`].

use std::cell::RefCell;

use quorum_ipc::{CoreEvent, Reply};
use serde_json::{Value, json};

use crate::mock::person;

/// People in the preview directory.
pub const PEOPLE: usize = 800;

const TITLES: &[&str] =
    &["Product Manager", "Software Engineer", "Designer", "Data Analyst", "Engineering Manager", "Researcher"];
const DEPTS: &[&str] = &["Design", "Engineering", "Marketing", "Operations"];
const PRESENCE: &[&str] = &["available", "busy", "away", "available", "dnd", "offline", "brb", "available"];
const STATUS: &[&str] = &["", "", "Heads down until 3pm", "", "Out at the dentist, back soon", "", "", ""];

/// Query ids answered here.
pub const QUERIES: &[&str] = &[
    "people.list",
    "people.search",
    "people.person",
    "people.photo",
    "presence.me",
    "presence.of",
    "presence.keepalive",
    "search.all",
];

/// Command ids answered here.
pub const COMMANDS: &[&str] = &[
    "presence.set",
    "presence.set_status_message",
    "presence.clear_status_message",
    "presence.watch",
    "presence.keepalive",
    "people.open",
    "people.email",
    "people.chat",
    "people.schedule",
    "search.open",
];

struct Own {
    presence: String,
    message: Option<String>,
    keepalive: bool,
}

thread_local! {
    static OWN: RefCell<Own> = RefCell::new(Own { presence: "available".into(), message: None, keepalive: true });
}

fn pick(list: &[&'static str], i: usize) -> &'static str {
    list.get(i % list.len().max(1)).copied().unwrap_or("")
}

fn id_of(i: usize) -> String {
    format!("user-{i}")
}

fn index_of(id: &str) -> Option<usize> {
    id.strip_prefix("user-").and_then(|n| n.parse().ok()).filter(|n| *n < PEOPLE)
}

fn presence_of(i: usize) -> &'static str {
    if i == 0 {
        return "available";
    }
    pick(PRESENCE, i.wrapping_mul(3).wrapping_add(1))
}

/// The preview person row for `i`.
pub fn row(i: usize) -> Value {
    let (name, email) = person(i);
    let status = pick(STATUS, i);
    json!({
        "id": id_of(i),
        "user_id": id_of(i),
        "display_name": name,
        "name": name,
        "email": email,
        "job_title": pick(TITLES, i),
        "department": pick(DEPTS, i / 3),
        "office_location": format!("Building {}", i % 5 + 1),
        "phone": format!("+1 555 01{:02}", i % 100),
        "presence": presence_of(i),
        "availability": Value::Null,
        "activity": Value::Null,
        "status_message": if status.is_empty() { Value::Null } else { json!(status) },
    })
}

fn str_param<'a>(p: &'a Value, k: &str) -> &'a str {
    p.get(k).and_then(Value::as_str).unwrap_or_default()
}

fn search_people(q: &str, limit: usize) -> Vec<Value> {
    let q = q.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    (0..PEOPLE)
        .filter(|i| {
            let (name, email) = person(*i);
            name.to_lowercase().split(' ').any(|w| w.starts_with(&q))
                || email.starts_with(&q)
                || name.to_lowercase().starts_with(&q)
        })
        .take(limit)
        .map(row)
        .collect()
}

fn manager_of(i: usize) -> Option<usize> {
    (i > 0).then(|| (i - 1) / 4)
}

/// Someone known only by address (a mail sender, an attendee): what the engine answers for a
/// `mail:<address>` or bare address that is not in the directory.
fn by_address(user: &str) -> Option<Value> {
    let address = user.strip_prefix("mail:").unwrap_or(user).trim();
    if !address.contains('@') || address.len() > 320 {
        return None;
    }
    let name = address
        .split('@')
        .next()
        .unwrap_or_default()
        .split('.')
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect::<String>()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ");
    Some(json!({
        "person": { "id": format!("mail:{}", address.to_lowercase()), "display_name": name, "email": address, "presence": "unknown" },
        "managers": [], "reports": [], "recent_mail": [], "recent_chats": []
    }))
}

fn details(id: &str) -> Option<Value> {
    let i = index_of(id)?;
    let mut managers = Vec::new();
    let mut m = manager_of(i);
    while let Some(x) = m {
        managers.push(row(x));
        if managers.len() >= 6 {
            break;
        }
        m = manager_of(x);
    }
    let reports: Vec<Value> = (1..=4).map(|k| i * 4 + k).filter(|r| *r < PEOPLE).map(row).collect();
    let (name, _) = person(i);
    let first = name.split(' ').next().unwrap_or_default().to_owned();
    let recent_mail: Vec<Value> = ["Quarterly planning", "Design review", "Budget draft"]
        .iter()
        .enumerate()
        .map(|(k, s)| {
            json!({
                "key": format!("mail:{id}-{k}"),
                "scope": "mail",
                "title": format!("Re: {s}"),
                "snippet": { "text": format!("{first} wrote: thanks for the notes on {s}."), "highlights": [] },
                "when": format!("2026-10-{:02}T09:00:00Z", 16 - k),
                "open": { "command": "view.open", "params": { "area": "mail", "id": format!("inbox-{k}") } },
            })
        })
        .collect();
    let recent_chats = vec![json!({
        "id": format!("chat-{i}"),
        "topic": name,
        "preview": "Sounds good, talk tomorrow.",
        "when": "2026-10-17T08:30:00Z",
        "open": { "command": "chat.open", "params": { "id": format!("chat-{i}") } },
    })];
    let mut person_row = row(i);
    if i == 0
        && let Some(obj) = person_row.as_object_mut()
    {
        OWN.with(|o| {
            if let Ok(o) = o.try_borrow() {
                obj.insert("presence".into(), json!(o.presence));
                obj.insert("status_message".into(), json!(o.message));
            }
        });
    }
    Some(json!({
        "person": person_row,
        "managers": managers,
        "reports": reports,
        "recent_mail": recent_mail,
        "recent_chats": recent_chats,
    }))
}

fn highlight(text: &str, q: &str) -> Value {
    let lower = text.to_lowercase();
    let ranges: Vec<Value> = match lower.find(&q.to_lowercase()) {
        Some(start) if !q.is_empty() && text.is_char_boundary(start) && text.is_char_boundary(start + q.len()) => {
            vec![json!([start, start + q.len()])]
        }
        _ => Vec::new(),
    };
    json!({ "text": text, "highlights": ranges })
}

fn search_all(params: &Value) -> Value {
    let q: String = str_param(params, "query").chars().filter(|c| !c.is_control()).take(256).collect();
    let q = q.trim().to_owned();
    if q.is_empty() {
        return json!({ "query": q, "remote": false, "groups": [] });
    }
    let mk = |scope: &str, k: usize, title: String, sub: &str, text: String, open: Value| {
        json!({
            "key": format!("{scope}:{k}"),
            "scope": scope,
            "title": title,
            "subtitle": sub,
            "snippet": highlight(&text, &q),
            "when": format!("2026-10-{:02}T10:00:00Z", 17 - (k % 9)),
            "source": "local",
            "open": open,
        })
    };
    let mail: Vec<Value> = (0..5)
        .map(|k| {
            let (from, _) = person(k * 7 + 3);
            mk(
                "mail",
                k,
                format!("{q} follow-up #{}", k + 1),
                &from,
                format!("Notes about {q} and next steps for the team."),
                json!({ "command": "view.open", "params": { "area": "mail", "id": format!("inbox-{k}") } }),
            )
        })
        .collect();
    let chats: Vec<Value> = (0..3)
        .map(|k| {
            let (from, _) = person(k * 5 + 2);
            mk(
                "chats",
                k,
                from,
                "",
                format!("Did you see the {q} update?"),
                json!({ "command": "chat.open", "params": { "id": format!("chat-{k}") } }),
            )
        })
        .collect();
    let channels: Vec<Value> = (0..2)
        .map(|k| {
            mk(
                "channels",
                k,
                "Engineering › General".to_owned(),
                "",
                format!("Posted: {q} rollout plan"),
                json!({ "command": "channel.open", "params": { "team": "team-1", "channel": format!("chan-{k}") } }),
            )
        })
        .collect();
    let files: Vec<Value> = (0..3)
        .map(|k| {
            mk(
                "files",
                k,
                format!("{q} plan v{}.docx", k + 1),
                "My files",
                format!("{q} plan v{}.docx", k + 1),
                json!({ "command": "view.open", "params": { "area": "files", "id": format!("item-{k}") } }),
            )
        })
        .collect();
    let people: Vec<Value> = search_people(&q, 4)
        .into_iter()
        .enumerate()
        .map(|(k, p)| {
            let name = str_param(&p, "display_name").to_owned();
            let id = str_param(&p, "id").to_owned();
            mk(
                "people",
                k,
                name.clone(),
                str_param(&p, "job_title"),
                name,
                json!({ "command": "people.open", "params": { "user": id } }),
            )
        })
        .collect();
    let events: Vec<Value> = (0..2)
        .map(|k| {
            mk(
                "events",
                k,
                format!("{q} sync"),
                "Room 4.01",
                format!("{q} sync"),
                json!({ "command": "view.open", "params": { "area": "calendar", "id": format!("evt-{k}") } }),
            )
        })
        .collect();
    let groups: Vec<Value> = [
        ("mail", "Mail", mail),
        ("chats", "Chats", chats),
        ("channels", "Channels", channels),
        ("files", "Files", files),
        ("people", "People", people),
        ("events", "Events", events),
    ]
    .into_iter()
    .filter(|(_, _, h)| !h.is_empty())
    .map(|(s, l, h)| json!({ "scope": s, "label": l, "hits": h }))
    .collect();
    json!({ "query": q, "remote": true, "groups": groups })
}

/// Answers a people/presence/search query, or `None` for other ids.
pub fn query(id: &str, params: &Value) -> Option<Reply> {
    let ok = |value: Value| Some(Reply::Ok { value });
    match id {
        "people.list" => ok(Value::Array((1..60).map(row).collect())),
        "people.search" => {
            let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(10).clamp(1, 50) as usize;
            ok(Value::Array(search_people(str_param(params, "query"), limit)))
        }
        "people.person" => match details(str_param(params, "user")).or_else(|| by_address(str_param(params, "user"))) {
            Some(d) => ok(d),
            None => Some(Reply::Err { kind: "not_found".into(), message: "No such person.".into() }),
        },
        "people.photo" => ok(Value::Null),
        "presence.me" => OWN.with(|o| {
            let o = o.try_borrow().ok()?;
            ok(json!({ "user_id": "user-0", "presence": o.presence, "status_message": o.message, "availability": null, "activity": null, "expiry": null, "fetched_at": null }))
        }),
        "presence.of" => {
            let ids = params.get("user_ids").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut map = serde_json::Map::new();
            for id in ids.iter().filter_map(Value::as_str).take(650) {
                if let Some(i) = index_of(id) {
                    let presence = if i == 0 {
                        OWN.with(|o| o.try_borrow().map(|o| o.presence.clone()).unwrap_or_default())
                    } else {
                        presence_of(i).to_owned()
                    };
                    map.insert(id.to_owned(), json!({ "presence": presence, "status_message": pick(STATUS, i) }));
                }
            }
            ok(Value::Object(map))
        }
        "presence.keepalive" => {
            let on = OWN.with(|o| o.try_borrow().map(|o| o.keepalive).unwrap_or(false));
            ok(json!({ "enabled": on, "active": on }))
        }
        "search.all" => ok(search_all(params)),
        _ => None,
    }
}

fn changed(tables: &[&str]) -> CoreEvent {
    CoreEvent::StoreChanged { tables: tables.iter().map(|t| (*t).to_owned()).collect() }
}

/// Runs a people/presence/search command, or `None` for other ids.
pub fn run(id: &str, params: &Value) -> Option<(Reply, Vec<CoreEvent>)> {
    let ok = Reply::Ok { value: Value::Null };
    let bad = |m: &str| Reply::Err { kind: "invalid_params".into(), message: m.into() };
    let user = params.get("user").or_else(|| params.get("id")).and_then(Value::as_str).unwrap_or_default().to_owned();
    Some(match id {
        "presence.set" => {
            let a = str_param(params, "availability").to_lowercase();
            let short = match a.replace('_', "").as_str() {
                "available" => "available",
                "busy" => "busy",
                "dnd" | "donotdisturb" => "dnd",
                "brb" | "berightback" => "brb",
                "away" => "away",
                "offline" => "offline",
                "reset" => "available",
                _ => return Some((bad("Unknown availability."), vec![])),
            };
            OWN.with(|o| {
                if let Ok(mut o) = o.try_borrow_mut() {
                    o.presence = short.to_owned();
                }
            });
            (ok, vec![changed(&["presence", "outbox"])])
        }
        "presence.set_status_message" | "presence.clear_status_message" => {
            let msg: String = str_param(params, "message").trim().chars().take(280).collect();
            if id == "presence.set_status_message" && msg.is_empty() {
                return Some((bad("The message is empty."), vec![]));
            }
            OWN.with(|o| {
                if let Ok(mut o) = o.try_borrow_mut() {
                    o.message = (!msg.is_empty()).then_some(msg);
                }
            });
            (ok, vec![changed(&["presence"])])
        }
        "presence.watch" => {
            let n = params.get("user_ids").and_then(Value::as_array).map_or(0, Vec::len);
            (Reply::Ok { value: json!({ "watching": n, "added": n }) }, vec![])
        }
        "presence.keepalive" => {
            let on = params.get("enabled").and_then(Value::as_bool).unwrap_or(true);
            OWN.with(|o| {
                if let Ok(mut o) = o.try_borrow_mut() {
                    o.keepalive = on;
                }
            });
            (ok, vec![changed(&["setting"])])
        }
        "people.open" => {
            if user.is_empty() {
                return Some((bad("Choose a person first."), vec![]));
            }
            (ok, vec![CoreEvent::Ui { command: "people.card.show".into(), params: json!({ "user": user }) }])
        }
        "people.email" | "people.chat" | "people.schedule" => {
            if user.is_empty() {
                return Some((bad("Choose a person first."), vec![]));
            }
            let what = match id {
                "people.email" => "New message",
                "people.chat" => "New chat",
                _ => "New meeting",
            };
            (ok, vec![CoreEvent::Toast { level: "info".into(), message: format!("{what} (preview)") }])
        }
        "search.open" => {
            let q = str_param(params, "query").to_owned();
            (ok, vec![CoreEvent::Ui { command: "search.show".into(), params: json!({ "query": q }) }])
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn unknown_addresses_get_a_minimal_card() {
        let r = super::query("people.person", &serde_json::json!({ "user": "mail:logan.lund@example.com" }));
        assert!(matches!(r, Some(quorum_ipc::Reply::Ok { .. })), "{r:?}");
        let r = super::query("people.person", &serde_json::json!({ "user": "nobody" }));
        assert!(matches!(r, Some(quorum_ipc::Reply::Err { .. })));
    }

    use super::*;

    #[test]
    fn shapes_match_the_engine() {
        let Some(Reply::Ok { value }) = query("people.search", &json!({ "query": "av", "limit": 3 })) else {
            panic!("search failed")
        };
        let rows = value.as_array().cloned().unwrap_or_default();
        assert!(!rows.is_empty() && rows.len() <= 3);
        for r in &rows {
            for k in ["id", "display_name", "email", "job_title", "department", "presence"] {
                assert!(r.get(k).is_some(), "{k}");
            }
        }
        assert!(matches!(query("people.person", &json!({ "user": "user-9" })), Some(Reply::Ok { .. })));
        assert!(matches!(query("people.person", &json!({ "user": "nope" })), Some(Reply::Err { .. })));
        assert!(query("mail.list", &Value::Null).is_none());
    }

    #[test]
    fn hostile_params_are_soft() {
        for v in
            [Value::Null, json!(5), json!({ "query": 5, "user_ids": "x" }), json!({ "query": "\u{0}é".repeat(999) })]
        {
            for q in QUERIES {
                let _ = query(q, &v);
            }
            for c in COMMANDS {
                let _ = run(c, &v);
            }
        }
    }

    #[test]
    fn set_presence_round_trips() {
        let _ = run("presence.set", &json!({ "availability": "dnd" }));
        let Some(Reply::Ok { value }) = query("presence.me", &Value::Null) else { panic!("me failed") };
        assert_eq!(value.get("presence").and_then(Value::as_str), Some("dnd"));
    }
}
