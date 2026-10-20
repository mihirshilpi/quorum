//! In-wasm mock backend for **browser preview mode** (no Tauri, no core).
//!
//! Answers the queries the UI uses with deterministic, synthetic data (`example.com` people,
//! made-up subjects), and a few commands with realistic side effects (events), so the UI can be
//! developed and screenshotted in any browser. Nothing here is tenant data.
//!
//! Row shapes are this UI's placeholders; feature agents replace them with the engine's real
//! query results and keep this mock in step.

use std::cell::RefCell;

use quorum_ipc::{CommandInfo, CoreEvent, Reply};
use serde_json::{Value, json};

#[derive(Default)]
struct MockState {
    presence: Option<String>,
    syncs: u32,
}

thread_local! {
    static STATE: RefCell<MockState> = RefCell::new(MockState::default());
}

fn with_state<R>(f: impl FnOnce(&mut MockState) -> R) -> Option<R> {
    STATE.with(|s| s.try_borrow_mut().ok().map(|mut s| f(&mut s)))
}

const FIRST: &[&str] = &[
    "Avery", "Blake", "Casey", "Devon", "Emery", "Finley", "Harper", "Jordan", "Kai", "Logan", "Morgan", "Parker",
    "Quinn", "Riley", "Rowan", "Sage", "Taylor", "Robin", "Skyler", "Jamie",
];
const LAST: &[&str] = &[
    "Adler", "Brook", "Castillo", "Dahl", "Ellis", "Fischer", "Garcia", "Hale", "Ito", "Jensen", "Kowalski", "Lund",
    "Moreau", "Novak", "Okafor", "Patel", "Reyes", "Sato", "Varga", "Weiss", "Young",
];
const TOPIC: &[&str] = &[
    "Quarterly planning",
    "Design review",
    "Budget draft",
    "Release notes",
    "Onboarding checklist",
    "Offsite logistics",
    "Customer feedback",
    "Roadmap sync",
    "Hiring loop",
    "Security review",
    "Launch retro",
    "Weekly status",
    "Vendor contract",
    "Team lunch",
];
const SNIPPET: &[&str] = &[
    "Thanks for pulling this together. A few comments inline before we share it more widely.",
    "Can we move this to Thursday? I have a conflict in the morning.",
    "Attached is the latest version with the changes we discussed.",
    "Quick reminder that the deadline is end of week.",
    "Looks good to me. Ship it once the checks pass.",
    "I've added the open questions to the doc; let's go through them together.",
];
const PRESENCE: &[&str] = &["available", "busy", "away", "available", "do_not_disturb", "offline", "be_right_back"];

fn pick(list: &[&'static str], i: usize) -> &'static str {
    list.get(i % list.len().max(1)).copied().unwrap_or("")
}

/// A synthetic person for seed `i`: (display name, address).
pub fn person(i: usize) -> (String, String) {
    let first = pick(FIRST, i);
    let last = pick(LAST, i / FIRST.len().max(1) + i);
    (format!("{first} {last}"), format!("{}.{}@example.com", first.to_lowercase(), last.to_lowercase()))
}

fn time_label(i: usize, spacing_min: usize) -> String {
    let minutes = i.saturating_mul(spacing_min);
    let days = minutes / 1440;
    let at = 1440 - 1 - minutes % 1440;
    let (h, m) = (at / 60, at % 60);
    match days {
        0 => format!("{h:02}:{m:02}"),
        1 => "Yesterday".to_owned(),
        2..=6 => pick(&["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"], 6 - days).to_owned(),
        _ => format!("{days} days ago"),
    }
}

fn presence_for(i: usize) -> String {
    pick(PRESENCE, i).to_owned()
}

fn chats() -> Value {
    Value::Array(
        (0..400)
            .map(|i| {
                let (name, _) = person(i * 5 + 2);
                let group = i % 5 == 0;
                json!({
                    "id": format!("chat-{i}"),
                    "topic": if group { format!("{} group", pick(TOPIC, i)) } else { name.clone() },
                    "last_from": name,
                    "last_message": pick(SNIPPET, i + 1),
                    "updated": time_label(i, 41),
                    "unread": i % 3 == 0,
                    "presence": if group { "unknown".to_owned() } else { presence_for(i) },
                })
            })
            .collect(),
    )
}

fn channels() -> Value {
    let teams = ["Design", "Engineering", "Marketing", "Operations", "All Company"];
    let chans = ["General", "Announcements", "Planning", "Random", "Releases", "Support"];
    Value::Array(
        (0..teams.len() * chans.len())
            .map(|i| {
                let team = teams.get(i / chans.len()).copied().unwrap_or("Team");
                let chan = chans.get(i % chans.len()).copied().unwrap_or("General");
                json!({
                    "id": format!("chan-{i}"),
                    "team": team,
                    "name": chan,
                    "last_post": pick(SNIPPET, i),
                    "updated": time_label(i, 97),
                    "unread": i % 4 == 1,
                })
            })
            .collect(),
    )
}

fn drive_items() -> Value {
    let ext = ["docx", "xlsx", "pptx", "pdf", "png", "txt"];
    Value::Array(
        (0..600)
            .map(|i| {
                let folder = i % 9 == 0;
                let (by, _) = person(i * 11);
                let topic = pick(TOPIC, i);
                let name = if folder {
                    topic.to_owned()
                } else {
                    format!("{topic} {}.{}", i / TOPIC.len() + 1, ext.get(i % ext.len()).copied().unwrap_or("txt"))
                };
                json!({
                    "id": format!("item-{i}"),
                    "name": name,
                    "folder": folder,
                    "size": if folder { 0 } else { 1_024 * (i % 900 + 12) },
                    "modified": time_label(i, 173),
                    "modified_by": by,
                    "location": pick(&["mine", "shared", "recent", "mine"], i),
                })
            })
            .collect(),
    )
}

fn drive_locations() -> Value {
    json!([
        { "id": "mine", "name": "My files", "kind": "folder" },
        { "id": "recent", "name": "Recent", "kind": "clock" },
        { "id": "shared", "name": "Shared with me", "kind": "people" },
        { "id": "sites", "name": "Sites", "kind": "teams" },
    ])
}

fn info(id: &str, label: &str, menu: Option<&str>, shortcut: Option<&str>, destructive: bool) -> CommandInfo {
    CommandInfo {
        id: id.into(),
        label: label.into(),
        menu: menu.map(Into::into),
        shortcut: shortcut.map(Into::into),
        destructive,
        enabled: true,
    }
}

/// The mock engine registry (what `app.commands` returns in preview).
pub fn commands() -> Vec<CommandInfo> {
    vec![
        info("app.about", "About Quorum", Some("Quorum/About Quorum"), None, false),
        info("app.commands", "List Commands", None, None, false),
        info("palette.open", "Command Palette", Some("View/Command Palette"), Some("Cmd+K"), false),
        info("sync.now", "Sync Now", Some("Quorum/Sync Now"), Some("Cmd+Shift+Y"), false),
        info("chat.new", "New Chat", Some("Chat/New Chat"), Some("Cmd+N"), false),
        info("chat.send", "Send Chat Message", Some("Chat/Send"), Some("Cmd+Enter"), true),
        info("channel.post", "New Post", Some("Teams/New Post"), Some("Cmd+N"), true),
        info("presence.set", "Set Presence", Some("Quorum/Set Presence"), None, false),
        info("presence.set_status_message", "Set Status Message", Some("Quorum/Set Status Message"), None, false),
        info("presence.keepalive", "Keep Me Available", Some("Quorum/Keep Me Available"), None, false),
        info("people.open", "Show Person Card", Some("People/Show Person Card"), None, false),
        info("people.chat", "Chat with Person", Some("People/Chat"), None, false),
        info("people.email", "Email Person", Some("People/Email"), None, false),
        info("people.schedule", "Schedule Meeting with Person", Some("People/Schedule Meeting"), None, false),
        info("search.open", "Search Everything", Some("Edit/Find/Search Everything"), Some("Cmd+F"), false),
        info("account.sign_out", "Sign Out", Some("Quorum/Sign Out"), None, false),
    ]
    .into_iter()
    .chain(crate::areas::files_mock::commands())
    .chain(crate::areas::mail_mock::commands())
    .chain(
        crate::areas::calendar_mock::COMMANDS
            .iter()
            .map(|(id, label, menu, shortcut, destructive)| info(id, label, *menu, *shortcut, *destructive)),
    )
    .collect()
}

fn str_param<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str)
}

fn ok(value: Value) -> Reply {
    Reply::Ok { value }
}

fn err(kind: &str, message: impl Into<String>) -> Reply {
    Reply::Err { kind: kind.into(), message: message.into() }
}

fn toast(level: &str, message: &str) -> CoreEvent {
    CoreEvent::Toast { level: level.into(), message: message.into() }
}

fn changed(tables: &[&str]) -> CoreEvent {
    CoreEvent::StoreChanged { tables: tables.iter().map(|t| (*t).to_owned()).collect() }
}

/// Answers a query.
pub fn query(id: &str, params: &Value) -> Reply {
    match id {
        "app.commands" => match serde_json::to_value(commands()) {
            Ok(v) => ok(v),
            Err(e) => err("bug", e.to_string()),
        },
        "account.current" => {
            let presence = with_state(|s| s.presence.clone()).flatten().unwrap_or_else(|| "available".into());
            ok(
                json!({ "id": "mock.account", "display_name": "Avery Example", "username": "avery@example.com", "presence": presence, "state": "signed_in" }),
            )
        }
        id if id.starts_with("mail.") => {
            crate::areas::mail_mock::query(id, params).unwrap_or_else(|| err("bug", "mock"))
        }
        id if crate::areas::calendar_mock::QUERIES.contains(&id) => crate::areas::calendar_mock::query(id, params),
        id if crate::areas::chat_mock::QUERIES.contains(&id) => {
            crate::areas::chat_mock::query(id, params).unwrap_or_else(|| err("bug", "mock"))
        }
        "chat.list" => ok(chats()),
        "teams.channels" => ok(channels()),
        "drive.locations" => ok(drive_locations()),
        "drive.list" => ok(drive_items()),
        id if crate::areas::files_mock::QUERIES.contains(&id) => {
            crate::areas::files_mock::query(id, params).unwrap_or_else(|| err("bug", "mock"))
        }
        id if crate::areas::people_mock::QUERIES.contains(&id) => {
            crate::areas::people_mock::query(id, params).unwrap_or_else(|| err("bug", "mock"))
        }
        "settings.get" | "settings.schema" | "account.permissions" | "app.about" => {
            crate::areas::settings::mock_query(id, params)
        }
        _ => err("unknown_command", format!("no query `{id}`")),
    }
}

/// Runs a command; returns the reply and the events the core would emit.
pub fn run(id: &str, params: Value) -> (Reply, Vec<CoreEvent>) {
    match id {
        "app.about" => (ok(json!({ "name": "Quorum", "version": "preview" })), vec![]),
        "app.commands" => (query(id, &params), vec![]),
        "palette.open" => {
            let q = str_param(&params, "query").unwrap_or_default().to_owned();
            (ok(Value::Null), vec![CoreEvent::Ui { command: "palette.open".into(), params: json!({ "query": q }) }])
        }
        "sync.now" => {
            let n = with_state(|s| {
                s.syncs = s.syncs.wrapping_add(1);
                s.syncs
            })
            .unwrap_or(0);
            (ok(json!({ "syncs": n })), vec![changed(&["*"]), toast("success", "Synced (preview data)")])
        }
        id if id.starts_with("mail.") => {
            crate::areas::mail_mock::run(id, &params).unwrap_or_else(|| (err("bug", "mock"), vec![]))
        }
        id if id.starts_with("calendar.") && crate::areas::calendar_mock::COMMANDS.iter().any(|c| c.0 == id) => {
            crate::areas::calendar_mock::run(id, &params).unwrap_or_else(|| (err("bug", "mock"), vec![]))
        }
        id if crate::areas::files_mock::COMMANDS.contains(&id) => {
            crate::areas::files_mock::run(id, &params).unwrap_or_else(|| (err("bug", "mock"), vec![]))
        }
        id if crate::areas::chat_mock::COMMANDS.contains(&id) => {
            crate::areas::chat_mock::run(id, &params).unwrap_or_else(|| (err("bug", "mock"), vec![]))
        }
        id if crate::areas::people_mock::COMMANDS.contains(&id) => {
            crate::areas::people_mock::run(id, &params).unwrap_or_else(|| (err("bug", "mock"), vec![]))
        }
        "settings.set" | "app.open_url" | "link.confirm" | "account.configure" | "account.sign_in"
        | "account.sign_out" => crate::areas::settings::mock_run(id, &params),
        _ if commands().iter().any(|c| c.id == id) => {
            (err("not_available", "This command isn't available in the browser preview."), vec![])
        }
        _ => (err("unknown_command", format!("no command `{id}`")), vec![]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(reply: &Reply) -> usize {
        match reply {
            Reply::Ok { value: Value::Array(a) } => a.len(),
            _ => 0,
        }
    }

    #[test]
    fn data_is_synthetic() {
        for i in 0..200 {
            let (_, email) = person(i);
            assert!(email.ends_with("@example.com"), "{email}");
        }
    }

    #[test]
    fn inbox_is_large_and_limit_applies() {
        assert_eq!(rows(&query("mail.list", &json!({ "folder": "inbox", "limit": 50 }))), 50);
        assert!(rows(&query("mail.list", &json!({ "folder": "inbox", "limit": 5000 }))) > 1000);
        assert_eq!(rows(&query("mail.list", &json!({ "folder": "nope" }))), 0);
    }

    #[test]
    fn every_ui_query_answers() {
        for q in [
            "app.commands",
            "mail.folders",
            "calendar.sources",
            "chat.list",
            "teams.channels",
            "drive.locations",
            "drive.list",
            "people.list",
        ] {
            assert!(rows(&query(q, &Value::Null)) > 0, "{q}");
        }
        assert!(matches!(query("account.current", &Value::Null), Reply::Ok { .. }));
        assert!(matches!(query("nope", &Value::Null), Reply::Err { .. }));
    }

    #[test]
    fn unknown_and_unavailable_commands_fail_softly() {
        assert!(matches!(run("calendar.respond", Value::Null).0, Reply::Err { .. }));
        assert!(matches!(run("nope.nope", Value::Null).0, Reply::Err { kind, .. } if kind == "unknown_command"));
        assert!(matches!(run("palette.open", Value::Null).1.first(), Some(CoreEvent::Ui { .. })));
    }
}
