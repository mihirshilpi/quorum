//! Browser-preview answers for the chat and channel queries and commands: synthetic
//! `example.com` people, in the same shapes the engine returns, with sends, edits, reactions and
//! drafts applied to an in-memory copy. Wired into [`crate::mock`].

use std::cell::RefCell;
use std::collections::HashMap;

use quorum_ipc::{CoreEvent, Reply};
use serde_json::{Value, json};

use super::chat_logic::civil_from_days;
use crate::mock::person;

/// Query ids answered here.
pub const QUERIES: &[&str] = &[
    "chat.list",
    "chat.messages",
    "chat.draft",
    "chat.search",
    "teams.list",
    "channel.posts",
    "channel.replies",
    "channel.draft",
];

/// Command ids answered here.
pub const COMMANDS: &[&str] = &[
    "chat.open",
    "chat.new",
    "chat.send",
    "chat.edit",
    "chat.delete",
    "chat.react",
    "chat.unreact",
    "chat.mark_read",
    "chat.mark_unread",
    "chat.load_older",
    "chat.pin",
    "chat.unpin",
    "chat.save_draft",
    "channel.open",
    "channel.post",
    "channel.reply",
    "channel.edit",
    "channel.delete",
    "channel.react",
    "channel.save_draft",
];

const ME: &str = "user-0";
const MINUTE: i64 = 60_000;

fn now_ms() -> i64 {
    #[cfg(target_arch = "wasm32")]
    {
        let n = js_sys::Date::now();
        if n.is_finite() { n as i64 } else { 0 }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(0))
            .unwrap_or(0)
    }
}

/// RFC 3339 (UTC) for epoch milliseconds.
fn iso(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (rem / 3_600_000, rem / 60_000 % 60, rem / 1000 % 60);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn name(i: usize) -> String {
    person(i).0
}

fn uid(i: usize) -> String {
    format!("user-{i}")
}

struct Chat {
    id: String,
    kind: &'static str,
    topic: Option<String>,
    members: Vec<usize>,
    unread: bool,
    pinned: bool,
}

struct Post {
    team: String,
    channel: String,
    row: Value,
    replies: Vec<Value>,
}

#[derive(Default)]
struct Mock {
    ready: bool,
    chats: Vec<Chat>,
    messages: HashMap<String, Vec<Value>>,
    posts: Vec<Post>,
    drafts: HashMap<String, Value>,
    seq: u64,
}

thread_local! {
    static MOCK: RefCell<Mock> = RefCell::new(Mock::default());
}

fn with<R>(f: impl FnOnce(&mut Mock) -> R) -> Option<R> {
    MOCK.with(|m| {
        let mut m = m.try_borrow_mut().ok()?;
        if !m.ready {
            seed(&mut m);
        }
        Some(f(&mut m))
    })
}

fn msg(id: String, from: usize, at: i64, html: &str) -> Value {
    json!({
        "id": id,
        "from_id": uid(from),
        "from_name": name(from),
        "mine": from == 0,
        "time": iso(at),
        "edited": false,
        "deleted": false,
        "system": false,
        "pending": false,
        "importance": "normal",
        "html": html,
        "reactions": [],
        "files": [],
        "cards": 0,
        "quote": null,
        "mentions_me": false,
    })
}

const LINES: &[&str] = &[
    "Morning! Did you get a chance to look at the draft?",
    "Yes, left a few comments on the second section.",
    "Thanks, I'll go through them after standup.",
    "Can we move the review to Thursday?",
    "Thursday works for me 👍",
    "I pushed the new build, it should be on staging in a few minutes.",
    "Looks great on my side. The <strong>search</strong> is much faster now.",
    "Agreed, nice work!",
    "One thing: the empty state copy still says \"coming soon\".",
    "Good catch, fixing it now.",
    "Lunch?",
    "Sure, 12:30 at the usual place?",
];

fn seed(m: &mut Mock) {
    m.ready = true;
    let now = now_ms();
    let specs: &[(&'static str, Option<&str>, &[usize], bool, bool)] = &[
        ("oneOnOne", None, &[0, 1], true, true),
        ("group", Some("Launch crew"), &[0, 2, 3, 4, 5], true, false),
        ("oneOnOne", None, &[0, 2], false, false),
        ("meeting", Some("Weekly sync"), &[0, 1, 3, 6], false, false),
        ("oneOnOne", None, &[0, 3], false, false),
        ("group", Some("Design reviews"), &[0, 4, 7, 8], false, false),
        ("oneOnOne", None, &[0, 5], false, false),
        ("oneOnOne", None, &[0, 6], false, false),
        ("group", None, &[0, 7, 9], false, false),
        ("oneOnOne", None, &[0, 8], false, false),
        ("oneOnOne", None, &[0, 9], false, false),
        ("oneOnOne", None, &[0, 10], false, false),
    ];
    for (n, (kind, topic, members, unread, pinned)) in specs.iter().enumerate() {
        let id = format!("19:mock-chat-{n}@thread.v2");
        let others: Vec<usize> = members.iter().copied().filter(|i| *i != 0).collect();
        let count = 14usize.saturating_sub(n);
        let mut list = Vec::new();
        // Oldest first; the newest message is `n` hours ago (a few days back for later chats).
        let newest = now - i64::try_from(n).unwrap_or(0) * 5 * 3_600_000 - 4 * MINUTE;
        for k in 0..count {
            let back = i64::try_from(count - 1 - k).unwrap_or(0);
            let at = newest - back * 7 * MINUTE - if back > 6 { 20 * 3_600_000 } else { 0 };
            let from = if k % 3 == 1 { 0 } else { others.get(k % others.len().max(1)).copied().unwrap_or(1) };
            let line = LINES.get((k + n * 5) % LINES.len()).copied().unwrap_or("Hi");
            let mut row = msg(
                format!("{}{:03}", 1_760_000_000_000_u64 + n as u64 * 1000, k),
                from,
                at,
                &format!("<p>{line}</p>"),
            );
            if let Some(o) = row.as_object_mut() {
                if k == 2 {
                    o.insert("reactions".into(), json!([{ "emoji": "👍", "count": 2, "mine": true, "names": ["You", name(others.first().copied().unwrap_or(1))] }]));
                }
                if k == 4 && n == 0 {
                    o.insert(
                        "files".into(),
                        json!([{ "name": "Launch plan.docx", "url": "https://example.com/files/launch-plan" }]),
                    );
                    o.insert("edited".into(), json!(true));
                }
                if k == 5 && n == 1 {
                    o.insert(
                        "html".into(),
                        json!(format!("<p><at id=\"0\">{}</at> can you confirm the date?</p>", name(0))),
                    );
                    o.insert("mentions_me".into(), json!(true));
                }
            }
            list.push(row);
        }
        m.messages.insert(id.clone(), list);
        m.chats.push(Chat {
            id,
            kind,
            topic: topic.map(str::to_owned),
            members: members.to_vec(),
            unread: *unread,
            pinned: *pinned,
        });
    }
    // Teams: two teams, a few channels, posts with replies.
    let channels: &[(&str, &str, &str)] = &[
        ("team-product", "chan-general", "General"),
        ("team-product", "chan-roadmap", "Roadmap"),
        ("team-product", "chan-design", "Design"),
        ("team-eng", "chan-eng-general", "General"),
        ("team-eng", "chan-releases", "Releases"),
    ];
    let subjects = ["Q3 planning kickoff", "", "New onboarding flow", "Release notes for 2.4", ""];
    for (ci, (team, channel, _)) in channels.iter().enumerate() {
        for p in 0..4usize {
            let at = now - i64::try_from((4 - p) * 9 + ci).unwrap_or(0) * 3_600_000;
            let from = (ci + p) % 9 + 1;
            let line = LINES.get((p * 3 + ci) % LINES.len()).copied().unwrap_or("Hello team");
            let mut row =
                msg(format!("{}", 1_761_000_000_000_u64 + (ci * 10 + p) as u64), from, at, &format!("<p>{line}</p>"));
            if let (Some(o), Some(s)) = (row.as_object_mut(), subjects.get((ci + p) % subjects.len())) {
                o.insert("subject".into(), if s.is_empty() { Value::Null } else { json!(s) });
            }
            let replies = (0..(p * 2 + ci) % 5)
                .map(|r| {
                    let rf = (from + r + 1) % 10;
                    let l = LINES.get((r + p + 1) % LINES.len()).copied().unwrap_or("+1");
                    let mut v = msg(
                        format!("{}", 1_762_000_000_000_u64 + (ci * 100 + p * 10 + r) as u64),
                        rf,
                        at + i64::try_from(r + 1).unwrap_or(1) * 11 * MINUTE,
                        &format!("<p>{l}</p>"),
                    );
                    if let (Some(o), Some(pid)) = (v.as_object_mut(), row.get("id").cloned()) {
                        o.insert("reply_to".into(), pid);
                    }
                    v
                })
                .collect();
            m.posts.push(Post { team: (*team).to_owned(), channel: (*channel).to_owned(), row, replies });
        }
    }
}

fn str_param<'a>(p: &'a Value, k: &str) -> &'a str {
    p.get(k).and_then(Value::as_str).unwrap_or_default()
}

fn ok(value: Value) -> Reply {
    Reply::Ok { value }
}

fn bad(message: &str) -> Reply {
    Reply::Err { kind: "invalid_params".into(), message: message.into() }
}

fn changed(tables: &[&str]) -> CoreEvent {
    CoreEvent::StoreChanged { tables: tables.iter().map(|t| (*t).to_owned()).collect() }
}

fn strip(html: &str) -> String {
    let mut out = String::new();
    let mut tag = false;
    for c in html.chars() {
        match c {
            '<' => tag = true,
            '>' => tag = false,
            c if !tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"")
}

fn chat_row(m: &Mock, c: &Chat) -> Value {
    let others: Vec<usize> = c.members.iter().copied().filter(|i| *i != 0).collect();
    let title = c.topic.clone().unwrap_or_else(|| {
        if c.kind == "oneOnOne" {
            others.first().map(|i| name(*i)).unwrap_or_else(|| "Chat".into())
        } else {
            others
                .iter()
                .map(|i| name(*i).split_whitespace().next().unwrap_or("").to_owned())
                .collect::<Vec<_>>()
                .join(", ")
        }
    });
    let last = m
        .messages
        .get(&c.id)
        .and_then(|l| l.iter().rev().find(|v| !v.get("deleted").and_then(Value::as_bool).unwrap_or(false)));
    let presence = (c.kind == "oneOnOne").then(|| crate::areas::people_mock::row(others.first().copied().unwrap_or(1)));
    json!({
        "id": c.id,
        "kind": c.kind,
        "title": title,
        "topic": c.topic,
        "members": c.members.iter().map(|i| json!({ "id": uid(*i), "name": if *i == 0 { "You".to_owned() } else { name(*i) }, "email": person(*i).1, "me": *i == 0 })).collect::<Vec<_>>(),
        "member_ids": c.members.iter().map(|i| uid(*i)).collect::<Vec<_>>(),
        "preview": last.map(|v| strip(v.get("html").and_then(Value::as_str).unwrap_or(""))).unwrap_or_default(),
        "last_message_preview": last.map(|v| strip(v.get("html").and_then(Value::as_str).unwrap_or(""))).unwrap_or_default(),
        "from_name": last.and_then(|v| v.get("from_name").cloned()).unwrap_or(Value::Null),
        "from_me": last.and_then(|v| v.get("mine").and_then(Value::as_bool)).unwrap_or(false),
        "time": last.and_then(|v| v.get("time").cloned()).unwrap_or(Value::Null),
        "unread": c.unread,
        "pinned": c.pinned,
        "hidden": false,
        "pending": false,
        "draft": m.drafts.get(&format!("chat:{}", c.id)).is_some_and(|d| !d.get("html").and_then(Value::as_str).unwrap_or("").is_empty()),
        "presence": presence.as_ref().and_then(|p| p.get("presence").cloned()),
        "presence_id": presence.as_ref().and_then(|p| p.get("user_id").cloned()),
        "web_url": Value::Null,
    })
}

fn chat_list(m: &Mock) -> Value {
    let mut rows: Vec<Value> = m.chats.iter().map(|c| chat_row(m, c)).collect();
    rows.sort_by(|a, b| {
        let pa = a.get("pinned").and_then(Value::as_bool).unwrap_or(false);
        let pb = b.get("pinned").and_then(Value::as_bool).unwrap_or(false);
        pb.cmp(&pa).then_with(|| {
            let ta = a.get("time").and_then(Value::as_str).unwrap_or("");
            let tb = b.get("time").and_then(Value::as_str).unwrap_or("");
            tb.cmp(ta)
        })
    });
    Value::Array(rows)
}

fn messages(m: &Mock, p: &Value) -> Reply {
    let id = str_param(p, "chat");
    let Some(list) = m.messages.get(id) else { return bad("Unknown chat.") };
    let limit = p.get("limit").and_then(Value::as_u64).map_or(50, |l| usize::try_from(l).unwrap_or(50)).clamp(1, 500);
    let start = list.len().saturating_sub(limit);
    ok(json!({
        "chat": id,
        "items": list.get(start..).unwrap_or(&[]),
        "next": Value::Null,
        "has_more": start > 0,
        "server_has_more": false,
        "synced": true,
        "me": ME,
    }))
}

fn teams() -> Value {
    json!([
        { "id": "team-product", "name": "Product", "description": "Product planning", "archived": false, "channels": [
            { "id": "chan-general", "team": "team-product", "name": "General", "description": null, "membership": "standard", "general": true, "web_url": null },
            { "id": "chan-roadmap", "team": "team-product", "name": "Roadmap", "description": null, "membership": "standard", "general": false, "web_url": null },
            { "id": "chan-design", "team": "team-product", "name": "Design", "description": null, "membership": "private", "general": false, "web_url": null },
        ]},
        { "id": "team-eng", "name": "Engineering", "description": "Builders", "archived": false, "channels": [
            { "id": "chan-eng-general", "team": "team-eng", "name": "General", "description": null, "membership": "standard", "general": true, "web_url": null },
            { "id": "chan-releases", "team": "team-eng", "name": "Releases", "description": null, "membership": "standard", "general": false, "web_url": null },
        ]},
    ])
}

fn channel_of(p: &Value) -> String {
    let c = str_param(p, "channel");
    if c.is_empty() { str_param(p, "id").to_owned() } else { c.to_owned() }
}

fn posts(m: &Mock, p: &Value) -> Reply {
    let channel = channel_of(p);
    let items: Vec<Value> = m
        .posts
        .iter()
        .filter(|x| x.channel == channel)
        .map(|x| {
            let mut row = x.row.clone();
            if let Some(o) = row.as_object_mut() {
                o.insert("reply_count".into(), json!(x.replies.len()));
                o.insert(
                    "last_reply_at".into(),
                    x.replies.last().and_then(|r| r.get("time").cloned()).unwrap_or(Value::Null),
                );
                o.insert("replies".into(), json!(x.replies.iter().rev().take(3).rev().collect::<Vec<_>>()));
            }
            row
        })
        .collect();
    let team = m.posts.iter().find(|x| x.channel == channel).map(|x| x.team.clone()).unwrap_or_default();
    ok(json!({ "team": team, "channel": channel, "items": items, "next": null, "synced": true, "me": ME }))
}

/// Answers a chat/channel query, or `None` for other ids.
pub fn query(id: &str, params: &Value) -> Option<Reply> {
    with(|m| match id {
        "chat.list" => {
            let mut rows = chat_list(m);
            let filter = str_param(params, "filter");
            if let (Value::Array(list), false) = (&mut rows, filter.is_empty() || filter == "all") {
                list.retain(|r| match filter {
                    "unread" => r.get("unread").and_then(Value::as_bool).unwrap_or(false),
                    "pinned" => r.get("pinned").and_then(Value::as_bool).unwrap_or(false),
                    k => r.get("kind").and_then(Value::as_str) == Some(k),
                });
            }
            ok(rows)
        }
        "chat.messages" => messages(m, params),
        "chat.draft" => ok(m.drafts.get(&format!("chat:{}", str_param(params, "chat"))).cloned().unwrap_or(json!({ "html": "", "mentions": [] }))),
        "channel.draft" => {
            let key = format!("channel:{}/{}", channel_of(params), str_param(params, "post"));
            ok(m.drafts.get(&key).cloned().unwrap_or(json!({ "html": "", "mentions": [] })))
        }
        "chat.search" => {
            let q = str_param(params, "query").to_lowercase();
            let hits: Vec<Value> = m
                .messages
                .iter()
                .flat_map(|(chat, list)| list.iter().map(move |v| (chat, v)))
                .filter(|(_, v)| !q.is_empty() && strip(v.get("html").and_then(Value::as_str).unwrap_or("")).to_lowercase().contains(&q))
                .take(25)
                .map(|(chat, v)| json!({ "kind": "chat", "chat": chat, "message": v.get("id"), "from_name": v.get("from_name"), "time": v.get("time"), "preview": strip(v.get("html").and_then(Value::as_str).unwrap_or("")) }))
                .collect();
            ok(Value::Array(hits))
        }
        "teams.list" => ok(teams()),
        "channel.posts" => posts(m, params),
        "channel.replies" => {
            let post = str_param(params, "post");
            ok(m.posts.iter().find(|x| x.row.get("id").and_then(Value::as_str) == Some(post)).map(|x| Value::Array(x.replies.clone())).unwrap_or(json!([])))
        }
        _ => Reply::Err { kind: "unknown_command".into(), message: format!("no query `{id}`") },
    })
    .filter(|_| QUERIES.contains(&id))
}

/// Sets a field of a JSON object (no-op for other values).
fn set(v: &mut Value, key: &str, value: Value) {
    if let Some(o) = v.as_object_mut() {
        o.insert(key.to_owned(), value);
    }
}

fn next_id(m: &mut Mock) -> String {
    m.seq = m.seq.wrapping_add(1);
    format!("{}", 1_770_000_000_000_u64.wrapping_add(m.seq))
}

fn find_msg<'a>(m: &'a mut Mock, p: &Value) -> Option<&'a mut Value> {
    let id = str_param(p, "message");
    let chat = str_param(p, "chat");
    if !chat.is_empty() {
        return m.messages.get_mut(chat)?.iter_mut().find(|v| v.get("id").and_then(Value::as_str) == Some(id));
    }
    let id = if id.is_empty() { str_param(p, "post") } else { id };
    for post in &mut m.posts {
        if post.row.get("id").and_then(Value::as_str) == Some(id) {
            return Some(&mut post.row);
        }
        if let Some(r) = post.replies.iter_mut().find(|v| v.get("id").and_then(Value::as_str) == Some(id)) {
            return Some(r);
        }
    }
    None
}

fn toggle_reaction(v: &mut Value, emoji: &str, remove: bool) {
    let Some(list) = v.get_mut("reactions").and_then(Value::as_array_mut) else { return };
    let pos = list.iter().position(|r| r.get("emoji").and_then(Value::as_str) == Some(emoji));
    match (pos, remove) {
        (Some(i), true) => {
            let n = list.get(i).and_then(|r| r.get("count")).and_then(Value::as_u64).unwrap_or(1);
            if n <= 1 {
                list.remove(i);
            } else if let Some(r) = list.get_mut(i) {
                set(r, "count", json!(n - 1));
                set(r, "mine", json!(false));
            }
        }
        (Some(i), false) => {
            if let Some(r) = list.get_mut(i)
                && !r.get("mine").and_then(Value::as_bool).unwrap_or(false)
            {
                let n = r.get("count").and_then(Value::as_u64).unwrap_or(0);
                set(r, "count", json!(n + 1));
                set(r, "mine", json!(true));
            }
        }
        (None, false) => list.push(json!({ "emoji": emoji, "count": 1, "mine": true, "names": ["You"] })),
        (None, true) => {}
    }
}

/// Runs a chat/channel command, or `None` for other ids.
pub fn run(id: &str, params: &Value) -> Option<(Reply, Vec<CoreEvent>)> {
    if !COMMANDS.contains(&id) {
        return None;
    }
    let chat_tables = || vec![changed(&["chat", "chat_message", "draft"])];
    let chan_tables = || vec![changed(&["channel_message", "draft"])];
    with(|m| match id {
        "chat.open" | "chat.mark_read" | "chat.mark_unread" => {
            let cid = str_param(params, "id");
            let cid = if cid.is_empty() { str_param(params, "chat") } else { cid };
            let Some(c) = m.chats.iter_mut().find(|c| c.id == cid) else { return (bad("Unknown chat."), vec![]) };
            c.unread = id == "chat.mark_unread";
            (ok(json!({ "id": cid })), chat_tables())
        }
        "chat.pin" | "chat.unpin" => {
            let cid = str_param(params, "id");
            let Some(c) = m.chats.iter_mut().find(|c| c.id == cid) else { return (bad("Unknown chat."), vec![]) };
            c.pinned = id == "chat.pin";
            (ok(Value::Null), vec![changed(&["ui_prefs", "chat"])])
        }
        "chat.load_older" => (ok(json!({ "requested": false })), vec![]),
        "chat.new" => {
            let members: Vec<usize> = params
                .get("members")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str()?.strip_prefix("user-")?.parse().ok())
                        .filter(|i| *i != 0)
                        .collect()
                })
                .unwrap_or_default();
            if members.is_empty() {
                return (bad("Add at least one person."), vec![]);
            }
            if members.len() == 1
                && let Some(c) = m
                    .chats
                    .iter()
                    .find(|c| c.kind == "oneOnOne" && members.first().is_some_and(|f| c.members.contains(f)))
            {
                return (ok(json!({ "id": c.id, "existing": true })), vec![]);
            }
            let cid = format!("19:mock-new-{}@thread.v2", next_id(m));
            let mut all = vec![0];
            all.extend(members.iter().copied());
            let topic = Some(str_param(params, "topic").trim().to_owned()).filter(|t| !t.is_empty());
            let kind = if all.len() == 2 { "oneOnOne" } else { "group" };
            let mut list = Vec::new();
            let html = str_param(params, "message");
            if !html.is_empty() {
                let mid = next_id(m);
                list.push(msg(mid, 0, now_ms(), html));
            }
            m.messages.insert(cid.clone(), list);
            m.chats.insert(0, Chat { id: cid.clone(), kind, topic, members: all, unread: false, pinned: false });
            (ok(json!({ "id": cid, "existing": false })), chat_tables())
        }
        "chat.send" => {
            let cid = str_param(params, "chat").to_owned();
            let html = str_param(params, "html").to_owned();
            let files: Vec<Value> = params
                .get("files")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .map(
                            |f| json!({ "name": f.get("name").and_then(Value::as_str).unwrap_or("file"), "url": null }),
                        )
                        .collect()
                })
                .unwrap_or_default();
            if html.trim().is_empty()
                && files.is_empty()
                && params.get("images").and_then(Value::as_array).is_none_or(Vec::is_empty)
            {
                return (bad("The message is empty."), vec![]);
            }
            let quote = str_param(params, "quote").to_owned();
            let mid = next_id(m);
            let Some(list) = m.messages.get_mut(&cid) else { return (bad("Unknown chat."), vec![]) };
            let q = list.iter().find(|v| v.get("id").and_then(Value::as_str) == Some(quote.as_str())).map(|v| {
                json!({ "id": quote, "preview": strip(v.get("html").and_then(Value::as_str).unwrap_or("")), "from": v.get("from_name") })
            });
            let mut row = msg(mid, 0, now_ms(), &html);
            set(&mut row, "files", json!(files));
            if let Some(q) = q {
                set(&mut row, "quote", q);
            }
            list.push(row);
            m.drafts.remove(&format!("chat:{cid}"));
            (ok(json!({ "local_id": "mock" })), chat_tables())
        }
        "channel.post" | "channel.reply" => {
            let channel = channel_of(params);
            let html = str_param(params, "html").to_owned();
            if html.trim().is_empty() {
                return (bad("The message is empty."), vec![]);
            }
            let mid = next_id(m);
            let mut row = msg(mid, 0, now_ms(), &html);
            if id == "channel.post" {
                let subject = str_param(params, "subject").trim().to_owned();
                set(&mut row, "subject", if subject.is_empty() { Value::Null } else { json!(subject) });
                let team = m.posts.iter().find(|x| x.channel == channel).map(|x| x.team.clone()).unwrap_or_default();
                m.posts.push(Post { team, channel: channel.clone(), row, replies: Vec::new() });
                m.drafts.remove(&format!("channel:{channel}/"));
            } else {
                let post = str_param(params, "post").to_owned();
                let Some(p) =
                    m.posts.iter_mut().find(|x| x.row.get("id").and_then(Value::as_str) == Some(post.as_str()))
                else {
                    return (bad("Unknown post."), vec![]);
                };
                set(&mut row, "reply_to", json!(post));
                p.replies.push(row);
                m.drafts.remove(&format!("channel:{channel}/{post}"));
            }
            (ok(json!({ "local_id": "mock" })), chan_tables())
        }
        "chat.edit" | "channel.edit" => {
            let html = str_param(params, "html").to_owned();
            let Some(v) = find_msg(m, params) else { return (bad("Unknown message."), vec![]) };
            set(v, "html", json!(html));
            set(v, "edited", json!(true));
            (ok(Value::Null), if id == "chat.edit" { chat_tables() } else { chan_tables() })
        }
        "chat.delete" | "channel.delete" => {
            let Some(v) = find_msg(m, params) else { return (bad("Unknown message."), vec![]) };
            set(v, "deleted", json!(true));
            set(v, "html", json!(""));
            (ok(Value::Null), if id == "chat.delete" { chat_tables() } else { chan_tables() })
        }
        "chat.react" | "chat.unreact" | "channel.react" => {
            let emoji = str_param(params, "reaction").to_owned();
            let remove = id == "chat.unreact" || params.get("remove").and_then(Value::as_bool).unwrap_or(false);
            let Some(v) = find_msg(m, params) else { return (bad("Unknown message."), vec![]) };
            toggle_reaction(v, &emoji, remove);
            (ok(Value::Null), if id == "channel.react" { chan_tables() } else { chat_tables() })
        }
        "chat.save_draft" | "channel.save_draft" => {
            let key = if id == "chat.save_draft" {
                format!("chat:{}", str_param(params, "chat"))
            } else {
                format!("channel:{}/{}", channel_of(params), str_param(params, "post"))
            };
            m.drafts.insert(key, json!({ "html": str_param(params, "html"), "mentions": params.get("mentions").cloned().unwrap_or(json!([])) }));
            (ok(Value::Null), vec![changed(&["draft"])])
        }
        "channel.open" => {
            let channel = channel_of(params);
            let team = m.posts.iter().find(|x| x.channel == channel).map(|x| x.team.clone()).unwrap_or_default();
            (ok(json!({ "team": team, "channel": channel })), vec![])
        }
        _ => (Reply::Err { kind: "not_available".into(), message: "Not in the preview.".into() }, vec![]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(r: Option<Reply>) -> Value {
        match r {
            Some(Reply::Ok { value }) => value,
            _ => Value::Null,
        }
    }

    #[test]
    fn list_messages_and_send_round_trip() {
        let list = value(query("chat.list", &Value::Null));
        let first = list.get(0).and_then(|c| c.get("id")).and_then(Value::as_str).unwrap_or("").to_owned();
        assert!(!first.is_empty());
        assert_eq!(list.get(0).and_then(|c| c.get("pinned")), Some(&json!(true)));
        let before = value(query("chat.messages", &json!({ "chat": first })))
            .get("items")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let sent = run("chat.send", &json!({ "chat": first, "html": "<p>hi</p>" }));
        assert!(matches!(sent, Some((Reply::Ok { .. }, _))));
        let after = value(query("chat.messages", &json!({ "chat": first, "limit": 500 })))
            .get("items")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        assert!(after >= before);
        assert!(matches!(run("chat.send", &json!({ "chat": first, "html": " " })), Some((Reply::Err { .. }, _))));
        assert!(run("mail.send", &Value::Null).is_none());
        assert!(query("mail.list", &Value::Null).is_none());
    }

    #[test]
    fn iso_formats_epoch() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(1_760_700_000_000), "2025-10-17T11:20:00Z");
    }

    #[test]
    fn channels_have_posts_and_reactions_toggle() {
        let p = value(query("channel.posts", &json!({ "channel": "chan-general" })));
        let items = p.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        assert_eq!(items.len(), 4);
        let pid = items.first().and_then(|i| i.get("id")).and_then(Value::as_str).unwrap_or("").to_owned();
        let _ = run("channel.react", &json!({ "channel": "chan-general", "message": pid, "reaction": "🎉" }));
        let p = value(query("channel.posts", &json!({ "channel": "chan-general" })));
        let r = p.pointer("/items/0/reactions/0/emoji").cloned();
        assert_eq!(r, Some(json!("🎉")));
    }
}
