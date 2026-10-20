//! Browser-preview backend for Mail: synthetic `example.com` mail with the engine's real query
//! shapes (`mail.folders`, `mail.list`, `mail.message`, `mail.draft`, ...) and the side effects
//! of the mail commands (read, flag, move, delete, compose, send with undo). Nothing here is
//! tenant data. Pure Rust (no browser APIs), so it is unit-tested natively.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use quorum_ipc::{CommandInfo, CoreEvent, Reply};
use serde_json::{Value, json};

/// Messages in the mock Inbox (exercises the virtual list).
pub const INBOX_COUNT: usize = 3000;
/// The preview's "now": 2026-10-17 12:00 UTC, in minutes since the Unix epoch.
const NOW_MIN: i64 = 29_870_640;

const FOLDERS: &[(&str, &str, &str, usize, Option<&str>)] = &[
    ("f-inbox", "Inbox", "inbox", INBOX_COUNT, None),
    ("f-drafts", "Drafts", "drafts", 0, None),
    ("f-sent", "Sent Items", "sentitems", 240, None),
    ("f-deleted", "Deleted Items", "deleteditems", 30, None),
    ("f-archive", "Archive", "archive", 1200, None),
    ("f-junk", "Junk Email", "junkemail", 18, None),
    ("f-projects", "Projects", "", 64, Some("f-inbox")),
    ("f-launch", "Launch", "", 22, Some("f-projects")),
    ("f-receipts", "Receipts", "", 40, None),
];

const PEOPLE: &[(&str, &str)] = &[
    ("Avery Adler", "avery.adler"),
    ("Blake Brook", "blake.brook"),
    ("Casey Castillo", "casey.castillo"),
    ("Devon Dahl", "devon.dahl"),
    ("Emery Ellis", "emery.ellis"),
    ("Finley Fischer", "finley.fischer"),
    ("Harper Hale", "harper.hale"),
    ("Jordan Ito", "jordan.ito"),
    ("Kai Jensen", "kai.jensen"),
    ("Logan Lund", "logan.lund"),
    ("Morgan Moreau", "morgan.moreau"),
    ("Parker Patel", "parker.patel"),
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
const VERB: &[&str] = &["RE:", "FW:", "", "", "", "Updated:", "", "Question about", "Notes from"];
const SNIPPET: &[&str] = &[
    "Thanks for pulling this together. A few comments inline before we share it more widely.",
    "Can we move this to Thursday? I have a conflict in the morning.",
    "Attached is the latest version with the changes we discussed.",
    "Quick reminder that the deadline is end of week.",
    "Looks good to me. Ship it once the checks pass.",
    "I've added the open questions to the doc; let's go through them together.",
];
const CATEGORIES: &[(&str, &str)] = &[
    ("Blue category", "preset7"),
    ("Green category", "preset4"),
    ("Orange category", "preset1"),
    ("Purple category", "preset8"),
];

#[derive(Default)]
struct Draft {
    mode: String,
    source: Option<String>,
    to: Vec<Value>,
    cc: Vec<Value>,
    bcc: Vec<Value>,
    subject: String,
    body: String,
    attachments: Vec<Value>,
    sending: bool,
}

#[derive(Default)]
struct State {
    read: BTreeMap<String, bool>,
    flag: BTreeMap<String, String>,
    folder: BTreeMap<String, String>,
    focused: BTreeMap<String, bool>,
    categories: BTreeMap<String, Vec<String>>,
    deleted: BTreeSet<String>,
    allowed: BTreeSet<String>,
    drafts: BTreeMap<String, Draft>,
    sent: Vec<String>,
    next: u64,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.try_borrow_mut().ok().map(|mut s| f(&mut s)))
}

fn pick<'a>(list: &[&'a str], i: usize) -> &'a str {
    list.get(i % list.len().max(1)).copied().unwrap_or("")
}

/// Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// RFC 3339 for minutes since the epoch.
pub fn iso(minutes: i64) -> String {
    let (y, m, d) = civil(minutes.div_euclid(1440));
    let rem = minutes.rem_euclid(1440);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:00Z", rem / 60, rem % 60)
}

fn folder_of(id: &str) -> Option<&'static (&'static str, &'static str, &'static str, usize, Option<&'static str>)> {
    FOLDERS.iter().find(|f| f.0 == id || (!f.2.is_empty() && f.2 == id) || alias(id) == Some(f.2))
}

fn alias(name: &str) -> Option<&'static str> {
    Some(match name {
        "sent" => "sentitems",
        "deleted" | "trash" => "deleteditems",
        "junk" => "junkemail",
        _ => return None,
    })
}

fn kind_of(role: &str) -> &'static str {
    match role {
        "inbox" => "inbox",
        "drafts" => "drafts",
        "sentitems" => "sent",
        "deleteditems" => "deleted",
        "archive" => "archive",
        "junkemail" => "junk",
        _ => "folder",
    }
}

/// The generated message `i` of folder `f` (before user changes).
fn base(folder: &str, i: usize) -> Value {
    let seed = folder.bytes().map(usize::from).sum::<usize>();
    let (name, user) = PEOPLE.get((i * 7 + seed) % PEOPLE.len()).copied().unwrap_or(("Avery Adler", "avery.adler"));
    let sent = folder == "f-sent";
    let verb = pick(VERB, i / 2);
    let topic = pick(TOPIC, i / 3 + seed);
    let subject = if verb.is_empty() { topic.to_owned() } else { format!("{verb} {topic}") };
    let spacing = if folder == "f-inbox" { 23 } else { 140 };
    let minutes = NOW_MIN - i64::try_from(i * spacing + 7).unwrap_or(0);
    let id = format!("{folder}-{i}");
    json!({
        "id": id,
        "cursor": format!("{}:{id}", minutes * 60_000),
        "folder_id": folder,
        "conversation_id": format!("conv-{folder}-{}", i / 2),
        "from_name": if sent { "Alex Example" } else { name },
        "from_email": if sent { "you@example.com".to_owned() } else { format!("{user}@example.com") },
        "from": if sent { "Alex Example" } else { name },
        "to": if sent { name } else { "Alex Example" },
        "subject": subject,
        "preview": pick(SNIPPET, i + seed),
        "received": iso(minutes),
        "unread": folder == "f-inbox" && i.is_multiple_of(4),
        "is_read": !(folder == "f-inbox" && i.is_multiple_of(4)),
        "flagged": i.is_multiple_of(13),
        "flag": if i.is_multiple_of(13) { "flagged" } else { "notFlagged" },
        "has_attachments": i % 6 == 1,
        "importance": if i % 17 == 3 { "high" } else { "normal" },
        "categories": if i % 9 == 4 { json!(["Blue category"]) } else { json!([]) },
        "is_draft": false,
        "focused": i % 7 != 5,
        "count": 1,
    })
}

fn apply(state: &State, mut row: Value) -> Option<Value> {
    let id = row.get("id").and_then(Value::as_str)?.to_owned();
    if state.deleted.contains(&id) {
        return None;
    }
    let o = row.as_object_mut()?;
    if let Some(r) = state.read.get(&id) {
        o.insert("is_read".into(), json!(r));
        o.insert("unread".into(), json!(!r));
    }
    if let Some(f) = state.flag.get(&id) {
        o.insert("flag".into(), json!(f));
        o.insert("flagged".into(), json!(f == "flagged"));
    }
    if let Some(f) = state.folder.get(&id) {
        o.insert("folder_id".into(), json!(f));
    }
    if let Some(f) = state.focused.get(&id) {
        o.insert("focused".into(), json!(f));
    }
    if let Some(c) = state.categories.get(&id) {
        o.insert("categories".into(), json!(c));
    }
    Some(row)
}

/// Generated id → (folder, index).
fn parse_id(id: &str) -> Option<(&'static str, usize)> {
    let (folder, i) = id.rsplit_once('-')?;
    let f = FOLDERS.iter().find(|f| f.0 == folder)?;
    let i: usize = i.parse().ok()?;
    (i < f.3).then_some((f.0, i))
}

fn message_row(state: &State, id: &str) -> Option<Value> {
    let (folder, i) = parse_id(id)?;
    apply(state, base(folder, i))
}

fn rows(state: &State, folder: &str) -> Vec<Value> {
    let Some(f) = folder_of(folder) else { return Vec::new() };
    let mut out: Vec<Value> = (0..f.3)
        .map(|i| base(f.0, i))
        .filter_map(|r| apply(state, r))
        .filter(|r| r.get("folder_id").and_then(Value::as_str) == Some(f.0))
        .collect();
    // Messages moved in from other folders.
    for (id, to) in &state.folder {
        if to == f.0
            && parse_id(id).is_some_and(|(from, _)| from != f.0)
            && let Some(r) = message_row(state, id)
        {
            out.push(r);
        }
    }
    out.sort_by(|a, b| {
        let k = |r: &Value| r.get("received").and_then(Value::as_str).map(str::to_owned).unwrap_or_default();
        k(b).cmp(&k(a))
    });
    out
}

fn folders(state: &State) -> Value {
    let mut depth = BTreeMap::new();
    let mut out = Vec::new();
    for (id, name, role, _, parent) in FOLDERS {
        let d = parent.and_then(|p| depth.get(p).copied()).map_or(0, |d: usize| d + 1);
        depth.insert(*id, d);
        let list = rows(state, id);
        let unread = list.iter().filter(|r| r.get("unread") == Some(&json!(true))).count();
        let total = if *role == "drafts" { state.drafts.len() } else { list.len() };
        out.push(json!({
            "id": id,
            "name": name,
            "kind": kind_of(role),
            "role": if role.is_empty() { Value::Null } else { json!(role) },
            "parent_id": parent,
            "depth": d,
            "unread": unread,
            "total": total,
            "favourite": matches!(*role, "inbox" | "sentitems" | "drafts" | "archive"),
        }));
    }
    Value::Array(out)
}

fn body_for(row: &Value, allow: bool) -> (String, bool) {
    let first = row.get("from_name").and_then(Value::as_str).and_then(|n| n.split(' ').next()).unwrap_or("there");
    let i = row.get("id").and_then(Value::as_str).and_then(parse_id).map_or(0, |(_, i)| i);
    let image = if allow {
        "<p><img src=\"quorum-remote:https%3A%2F%2Fimages.example.com%2Fbanner.png\" alt=\"Banner\" width=\"480\" height=\"120\" style=\"background:#e8eefc\"></p>"
    } else {
        "<p><img alt=\"Banner\" width=\"480\" height=\"120\" style=\"background:#eef0f4\"></p>"
    };
    let has_image = i.is_multiple_of(3);
    let html = format!(
        "<div style=\"font-family: -apple-system, sans-serif; font-size: 14px; line-height: 1.5\">\
         <p>Hi Alex,</p><p>{}</p>{}\
         <ul><li>The timeline in section 2 looks tight.</li><li>Can we add the open risks?</li></ul>\
         <p>More context is in <a href=\"quorum-link:https%3A%2F%2Fexample.com%2Fdocs%2Fplan\">the plan</a>.</p>\
         <p>Best,<br>{first}</p></div>",
        pick(SNIPPET, i),
        if has_image { image } else { "" },
    );
    (html, has_image && !allow)
}

/// The preview's conversation of `row`: the folder's other message of the same conversation,
/// plus your reply in between (Sent Items), oldest first, each with quoted history.
fn conversation(s: &State, row: &Value, p: &Value) -> Vec<Value> {
    let conv = str_param(row, "conversation_id").unwrap_or_default().to_owned();
    let folder = str_param(row, "folder_id").unwrap_or_default().to_owned();
    let mut members: Vec<Value> = (0..400)
        .filter_map(|i| message_row(s, &format!("{folder}-{i}")))
        .filter(|r| str_param(r, "conversation_id") == Some(conv.as_str()))
        .take(6)
        .collect();
    members.sort_by(|a, b| str_param(a, "received").cmp(&str_param(b, "received")));
    // Your reply between the first two.
    if let (Some(first), true) = (members.first().cloned(), members.len() >= 2) {
        let mut mine = first.clone();
        if let Some(o) = mine.as_object_mut() {
            let when = str_param(&first, "received").and_then(super::mail_util::parse_minutes).unwrap_or(NOW_MIN);
            o.insert("id".into(), json!(format!("{}-reply", str_param(&first, "id").unwrap_or("x"))));
            o.insert("from_name".into(), json!("Alex Example"));
            o.insert("from_email".into(), json!("you@example.com"));
            o.insert("received".into(), json!(iso(when + 9)));
            o.insert("unread".into(), json!(false));
            o.insert("has_attachments".into(), json!(false));
            o.insert(
                "preview".into(),
                json!("Thanks, that works for me. I'll update the doc and circle back by Friday."),
            );
        }
        members.insert(1, mine);
    }
    let allow_ids: Vec<String> = p
        .get("allow_remote_ids")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default();
    let mut history = String::new();
    let mut out = Vec::new();
    for mut m in members {
        let id = str_param(&m, "id").unwrap_or_default().to_owned();
        let mine = id.ends_with("-reply");
        let sender = str_param(&m, "from_email").unwrap_or("").to_owned();
        let allow = allow_ids.contains(&id) || s.allowed.contains(&sender);
        let (new_html, blocked) = if mine {
            ("<div>Thanks, that works for me. I'll update the doc and circle back by Friday.</div><div><br>Alex</div>".to_owned(), false)
        } else {
            body_for(&m, allow)
        };
        let full = if history.is_empty() {
            new_html.clone()
        } else {
            format!("{new_html}<hr><div><b>From:</b> Earlier message</div>{history}")
        };
        if let Some(o) = m.as_object_mut() {
            let name = o.get("from_name").cloned().unwrap_or(Value::Null);
            o.insert("from_address".into(), json!({ "name": name, "address": sender }));
            o.insert("to_recipients".into(), json!([{ "name": "Alex Example", "address": "you@example.com" }]));
            o.insert(
                "cc_recipients".into(),
                json!([{ "name": "Casey Castillo", "address": "casey.castillo@example.com" }]),
            );
            o.insert("body_html".into(), json!(full));
            o.insert("body_new_html".into(), if history.is_empty() { Value::Null } else { json!(new_html) });
            o.insert("has_quoted".into(), json!(!history.is_empty()));
            o.insert("own_background".into(), json!(false));
            o.insert("body_loading".into(), json!(false));
            o.insert("remote_images_blocked".into(), json!(blocked));
            o.insert("folder_kind".into(), json!(if mine { "sent" } else { "inbox" }));
            o.insert("from_me".into(), json!(mine));
            let has = o.get("has_attachments") == Some(&json!(true));
            o.insert(
                "attachments".into(),
                if has { json!([{ "id": "att-1", "name": "Synthetic plan.pdf", "size": 482_133, "content_type": "application/pdf" }]) } else { json!([]) },
            );
        }
        history = full;
        out.push(m);
    }
    out
}

fn draft_json(id: &str, d: &Draft) -> Value {
    json!({
        "draft_id": id,
        "mode": d.mode,
        "source": d.source,
        "to": d.to,
        "cc": d.cc,
        "bcc": d.bcc,
        "subject": d.subject,
        "importance": Value::Null,
        "body_html": d.body,
        "quote_html": d.source.as_ref().map(|_| "<p><b>From:</b> Synthetic sender<br><b>Subject:</b> Synthetic</p><p>Quoted original text.</p>"),
        "attachments": d.attachments,
        "sending": d.sending,
        "send_at": Value::Null,
        "synced": false,
        "updated_at": iso(NOW_MIN),
    })
}

fn ok(value: Value) -> Reply {
    Reply::Ok { value }
}

fn err(kind: &str, message: &str) -> Reply {
    Reply::Err { kind: kind.into(), message: message.into() }
}

fn changed(tables: &[&str]) -> CoreEvent {
    CoreEvent::StoreChanged { tables: tables.iter().map(|t| (*t).to_owned()).collect() }
}

fn toast(level: &str, message: &str) -> CoreEvent {
    CoreEvent::Toast { level: level.into(), message: message.into() }
}

fn str_param<'a>(p: &'a Value, key: &str) -> Option<&'a str> {
    p.get(key).and_then(Value::as_str)
}

fn targets(p: &Value) -> Vec<String> {
    let mut out: Vec<String> = str_param(p, "id").map(str::to_owned).into_iter().collect();
    if let Some(ids) = p.get("ids").and_then(Value::as_array) {
        out.extend(ids.iter().filter_map(Value::as_str).take(500).map(str::to_owned));
    }
    out
}

fn recipients(p: &Value, key: &str) -> Option<Vec<Value>> {
    let list = p.get(key)?;
    let items: Vec<Value> = match list {
        Value::String(s) => s.split([';', ',']).map(|x| Value::String(x.trim().to_owned())).collect(),
        Value::Array(a) => a.clone(),
        _ => return Some(Vec::new()),
    };
    Some(
        items
            .into_iter()
            .filter_map(|v| match v {
                Value::String(s) if s.contains('@') => Some(json!({ "name": "", "address": s })),
                Value::Object(o) => {
                    let a = o.get("address").or_else(|| o.get("email")).and_then(Value::as_str)?.to_owned();
                    Some(json!({ "name": o.get("name").and_then(Value::as_str).unwrap_or(""), "address": a }))
                }
                _ => None,
            })
            .collect(),
    )
}

fn apply_fields(d: &mut Draft, p: &Value) {
    if let Some(v) = recipients(p, "to") {
        d.to = v;
    }
    if let Some(v) = recipients(p, "cc") {
        d.cc = v;
    }
    if let Some(v) = recipients(p, "bcc") {
        d.bcc = v;
    }
    if let Some(s) = str_param(p, "subject") {
        d.subject = s.chars().take(4096).collect();
    }
    if let Some(b) = str_param(p, "body_html") {
        d.body = b.chars().take(1_000_000).collect();
    }
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

/// The mail commands of the engine registry, as the preview's `app.commands` lists them.
pub fn commands() -> Vec<CommandInfo> {
    vec![
        info("mail.compose", "New Message", Some("Mail/New Message"), Some("Cmd+N"), false),
        info("mail.reply", "Reply", Some("Mail/Reply"), Some("R"), false),
        info("mail.reply_all", "Reply All", Some("Mail/Reply All"), Some("Shift+R"), false),
        info("mail.forward", "Forward", Some("Mail/Forward"), Some("Shift+F"), false),
        info("mail.save_draft", "Save Draft", Some("Mail/Save Draft"), Some("Cmd+S"), false),
        info("mail.send", "Send", Some("Mail/Send"), Some("Cmd+Enter"), true),
        info("mail.undo_send", "Undo Send", Some("Mail/Undo Send"), None, false),
        info("mail.discard_draft", "Discard Draft", Some("Mail/Discard Draft"), None, true),
        info("mail.attach", "Attach File", Some("Mail/Attach File"), None, false),
        info("mail.remove_attachment", "Remove Attachment", None, None, false),
        info("mail.mark_read", "Mark as Read", Some("Mail/Mark as Read"), Some("Q"), false),
        info("mail.mark_unread", "Mark as Unread", Some("Mail/Mark as Unread"), Some("U"), false),
        info("mail.toggle_read", "Toggle Read", None, None, false),
        info("mail.flag", "Flag", Some("Mail/Flag"), Some("Cmd+Shift+G"), false),
        info("mail.unflag", "Clear Flag", Some("Mail/Clear Flag"), None, false),
        info("mail.complete_flag", "Mark Flag Complete", Some("Mail/Mark Complete"), None, false),
        info("mail.categorize", "Categorize", Some("Mail/Categorize"), Some("C"), false),
        info("mail.move", "Move to Folder", Some("Mail/Move"), Some("V"), false),
        info("mail.archive", "Archive", Some("Mail/Archive"), Some("E"), false),
        info("mail.delete", "Delete", Some("Mail/Delete"), Some("Backspace"), true),
        info("mail.mark_focused", "Move to Focused", Some("Mail/Move to Focused"), None, false),
        info("mail.mark_other", "Move to Other", Some("Mail/Move to Other"), None, false),
        info("mail.allow_remote_images", "Always Show Images from Sender", None, None, false),
        info("mail.download_attachment", "Download Attachment", None, None, false),
        info("mail.fetch_body", "Download Message", None, None, false),
        info("mail.set_auto_reply", "Automatic Replies", Some("Mail/Automatic Replies…"), None, false),
    ]
}

/// Answers a mail query; `None` if `id` is not one.
pub fn query(id: &str, p: &Value) -> Option<Reply> {
    if !id.starts_with("mail.") {
        return None;
    }
    let reply = with(|s| match id {
        "mail.folders" => ok(folders(s)),
        "mail.list" => {
            let folder = str_param(p, "folder").unwrap_or("inbox");
            let limit = p.get("limit").and_then(Value::as_u64).map_or(200, |n| n.clamp(1, 5000));
            let limit = usize::try_from(limit).unwrap_or(200);
            let focused = p.get("focused").and_then(Value::as_bool);
            let unread_only = p.get("unread_only").and_then(Value::as_bool).unwrap_or(false);
            let flagged_only = p.get("flagged_only").and_then(Value::as_bool).unwrap_or(false);
            let group = str_param(p, "group") == Some("conversation");
            let after = str_param(p, "after");
            let mut list = rows(s, folder);
            if folder_of(folder).is_some_and(|f| f.2 == "drafts") {
                list = s
                    .drafts
                    .iter()
                    .filter(|(_, d)| !d.sending)
                    .map(|(id, d)| {
                        json!({ "id": id, "draft_id": id, "cursor": id, "from_name": "Draft", "from_email": "", "subject": d.subject,
                            "preview": "", "received": iso(NOW_MIN), "unread": false, "is_read": true, "flagged": false, "flag": "notFlagged",
                            "has_attachments": !d.attachments.is_empty(), "importance": "normal", "categories": [], "is_draft": true,
                            "focused": true, "count": 1, "conversation_id": Value::Null, "folder_id": "f-drafts",
                            "to": d.to.iter().filter_map(|t| t.get("address").and_then(Value::as_str)).collect::<Vec<_>>().join(", ") })
                    })
                    .collect();
            }
            let mut skipping = after.is_some();
            let mut out: Vec<Value> = Vec::new();
            let mut convs: BTreeMap<String, usize> = BTreeMap::new();
            for r in list {
                if skipping {
                    if str_param(&r, "cursor") == after {
                        skipping = false;
                    }
                    continue;
                }
                if focused.is_some_and(|f| r.get("focused").and_then(Value::as_bool) != Some(f))
                    || (unread_only && r.get("unread") != Some(&json!(true)))
                    || (flagged_only && r.get("flagged") != Some(&json!(true)))
                {
                    continue;
                }
                if group && let Some(c) = str_param(&r, "conversation_id").map(str::to_owned) {
                    if let Some(&i) = convs.get(&c) {
                        if let Some(Value::Object(o)) = out.get_mut(i) {
                            let n = o.get("count").and_then(Value::as_u64).unwrap_or(1) + 1;
                            o.insert("count".into(), json!(n));
                        }
                        continue;
                    }
                    convs.insert(c, out.len());
                }
                out.push(r);
                if out.len() >= limit {
                    break;
                }
            }
            ok(Value::Array(out))
        }
        "mail.message" => {
            let Some(id) = str_param(p, "id") else { return err("invalid_params", "`id` is required") };
            let Some(mut row) = message_row(s, id) else { return err("not_found", "no such message") };
            let sender = str_param(&row, "from_email").unwrap_or("").to_owned();
            let allow =
                p.get("allow_remote_images").and_then(Value::as_bool).unwrap_or(false) || s.allowed.contains(&sender);
            let (html, blocked) = body_for(&row, allow);
            if let Some(o) = row.as_object_mut() {
                let name = o.get("from_name").cloned().unwrap_or(Value::Null);
                o.insert("from_address".into(), json!({ "name": name, "address": sender }));
                o.insert("to_recipients".into(), json!([{ "name": "Alex Example", "address": "you@example.com" }]));
                o.insert(
                    "cc_recipients".into(),
                    json!([{ "name": "Casey Castillo", "address": "casey.castillo@example.com" }]),
                );
                o.insert("bcc_recipients".into(), json!([]));
                o.insert("reply_to".into(), json!([]));
                o.insert("body_html".into(), json!(html));
                o.insert("body_loading".into(), json!(false));
                o.insert("remote_images_blocked".into(), json!(blocked));
                o.insert("remote_images_allowed".into(), json!(allow));
                let has = o.get("has_attachments") == Some(&json!(true));
                o.insert(
                    "attachments".into(),
                    if has {
                        json!([
                            { "id": "att-1", "name": "Synthetic plan.pdf", "size": 482_133, "content_type": "application/pdf", "kind": "file", "is_inline": false, "downloaded": false },
                            { "id": "att-2", "name": "Budget.xlsx", "size": 38_012, "content_type": "application/vnd.ms-excel", "kind": "file", "is_inline": false, "downloaded": false },
                        ])
                    } else {
                        json!([])
                    },
                );
            }
            ok(row)
        }
        "mail.conversation" => {
            let Some(id) = str_param(p, "message").or_else(|| str_param(p, "id")) else {
                return err("invalid_params", "`message` is required");
            };
            let Some(row) = message_row(s, id) else { return err("not_found", "no such message") };
            ok(Value::Array(conversation(s, &row, p)))
        }
        "mail.draft" => match str_param(p, "draft_id").and_then(|id| s.drafts.get(id).map(|d| draft_json(id, d))) {
            Some(d) => ok(d),
            None => err("not_found", "no such draft"),
        },
        "mail.drafts" => ok(Value::Array(s.drafts.iter().map(|(id, d)| draft_json(id, d)).collect())),
        "mail.search" => {
            let q = str_param(p, "query").unwrap_or("").to_lowercase();
            if q.trim().is_empty() {
                return err("invalid_params", "`query` is required");
            }
            let hits: Vec<Value> = rows(s, "f-inbox")
                .into_iter()
                .filter(|r| {
                    ["subject", "from_name", "preview"]
                        .iter()
                        .any(|k| str_param(r, k).is_some_and(|v| v.to_lowercase().contains(&q)))
                })
                .take(50)
                .map(|mut r| {
                    if let Some(o) = r.as_object_mut() {
                        let snip = o.get("preview").cloned().unwrap_or(Value::Null);
                        o.insert("snippet".into(), snip);
                        o.insert("remote".into(), json!(false));
                    }
                    r
                })
                .collect();
            ok(Value::Array(hits))
        }
        "mail.categories" => {
            ok(Value::Array(CATEGORIES.iter().map(|(n, c)| json!({ "name": n, "color": c })).collect()))
        }
        "mail.auto_reply" => {
            ok(json!({ "status": "disabled", "internalReplyMessage": "", "externalReplyMessage": "" }))
        }
        _ => err("unknown_command", "no such mail query"),
    });
    Some(reply.unwrap_or_else(|| err("bug", "mock state busy")))
}

fn new_draft(s: &mut State, d: Draft) -> String {
    s.next = s.next.wrapping_add(1);
    let id = format!("d-preview-{}", s.next);
    s.drafts.insert(id.clone(), d);
    id
}

fn respond(s: &mut State, p: &Value, mode: &str) -> Reply {
    let Some(id) = str_param(p, "id") else { return err("invalid_params", "Select a message first.") };
    let Some(row) = message_row(s, id) else { return err("not_found", "no such message") };
    let existing = s.drafts.iter().find(|(_, d)| d.mode == mode && d.source.as_deref() == Some(id) && !d.sending);
    if let Some((did, _)) = existing {
        return ok(json!({ "draft_id": did, "existing": true }));
    }
    let subject = str_param(&row, "subject").unwrap_or("");
    let prefix = if mode == "forward" { "FW: " } else { "RE: " };
    let subject = if subject.to_lowercase().starts_with(&prefix.to_lowercase()) {
        subject.to_owned()
    } else {
        format!("{prefix}{subject}")
    };
    let author = json!({ "name": row.get("from_name"), "address": row.get("from_email") });
    let to = match mode {
        "forward" => Vec::new(),
        _ => vec![author],
    };
    let cc = if mode == "reply_all" {
        vec![json!({ "name": "Casey Castillo", "address": "casey.castillo@example.com" })]
    } else {
        Vec::new()
    };
    let mut d = Draft { mode: mode.into(), source: Some(id.to_owned()), to, cc, subject, ..Draft::default() };
    apply_fields(&mut d, p);
    ok(json!({ "draft_id": new_draft(s, d) }))
}

/// Runs a mail command; `None` if `id` is not one.
pub fn run(id: &str, p: &Value) -> Option<(Reply, Vec<CoreEvent>)> {
    if !id.starts_with("mail.") {
        return None;
    }
    let r = with(|s| run_in(s, id, p));
    Some(r.unwrap_or_else(|| (err("bug", "mock state busy"), vec![])))
}

const MSG_TABLES: &[&str] = &["message", "mail_folder", "outbox"];

fn run_in(s: &mut State, id: &str, p: &Value) -> (Reply, Vec<CoreEvent>) {
    let ids: Vec<String> = targets(p).into_iter().filter(|t| message_row(s, t).is_some()).collect();
    let needs_target = matches!(
        id,
        "mail.mark_read"
            | "mail.mark_unread"
            | "mail.toggle_read"
            | "mail.flag"
            | "mail.unflag"
            | "mail.complete_flag"
            | "mail.categorize"
            | "mail.move"
            | "mail.archive"
            | "mail.delete"
            | "mail.mark_focused"
            | "mail.mark_other"
    );
    if needs_target && ids.is_empty() {
        return (err("invalid_params", "Select a message first."), vec![]);
    }
    let rows: Vec<Value> = ids.iter().filter_map(|i| message_row(s, i)).collect();
    let all = |key: &str, v: Value| rows.iter().all(|r| r.get(key) == Some(&v));
    let done = |n: usize| (ok(json!({ "changed": n })), vec![changed(MSG_TABLES)]);
    match id {
        "mail.mark_read" | "mail.mark_unread" | "mail.toggle_read" => {
            let to = match id {
                "mail.mark_read" => true,
                "mail.mark_unread" => false,
                _ => !all("is_read", json!(true)),
            };
            for i in &ids {
                s.read.insert(i.clone(), to);
            }
            done(ids.len())
        }
        "mail.flag" | "mail.unflag" | "mail.complete_flag" => {
            let toggle = p.get("toggle").and_then(Value::as_bool).unwrap_or(true);
            let to = match id {
                "mail.unflag" => "notFlagged",
                "mail.complete_flag" => "complete",
                _ if toggle && all("flagged", json!(true)) => "notFlagged",
                _ => "flagged",
            };
            for i in &ids {
                s.flag.insert(i.clone(), to.to_owned());
            }
            done(ids.len())
        }
        "mail.mark_focused" | "mail.mark_other" => {
            for i in &ids {
                s.focused.insert(i.clone(), id == "mail.mark_focused");
            }
            done(ids.len())
        }
        "mail.categorize" => {
            let cats: Vec<String> = match p.get("categories") {
                Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
                Some(Value::String(c)) => vec![c.clone()],
                _ => return (err("invalid_params", "`categories` must be a list"), vec![]),
            };
            for (i, row) in ids.iter().zip(&rows) {
                let mut current: Vec<String> = row
                    .get("categories")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                    .unwrap_or_default();
                for c in &cats {
                    if let Some(pos) = current.iter().position(|x| x == c) {
                        current.remove(pos);
                    } else {
                        current.push(c.clone());
                    }
                }
                s.categories.insert(i.clone(), current);
            }
            done(ids.len())
        }
        "mail.move" | "mail.archive" | "mail.delete" => {
            let dest = match id {
                "mail.archive" => Some("f-archive"),
                "mail.delete" => Some("f-deleted"),
                _ => str_param(p, "folder").and_then(folder_of).map(|f| f.0),
            };
            let Some(dest) = dest else { return (err("not_found", "no such folder"), vec![]) };
            let permanent = p.get("permanent").and_then(Value::as_bool).unwrap_or(false);
            for (i, row) in ids.iter().zip(&rows) {
                if id == "mail.delete" && (permanent || str_param(row, "folder_id") == Some("f-deleted")) {
                    s.deleted.insert(i.clone());
                } else {
                    s.folder.insert(i.clone(), dest.to_owned());
                }
            }
            let what = match id {
                "mail.archive" => "Archived",
                "mail.delete" => "Deleted",
                _ => "Moved",
            };
            let (reply, mut ev) = done(ids.len());
            ev.push(toast("info", what));
            (reply, ev)
        }
        "mail.compose" => {
            let mut d = Draft { mode: "new".into(), ..Draft::default() };
            apply_fields(&mut d, p);
            (ok(json!({ "draft_id": new_draft(s, d) })), vec![changed(&["draft"])])
        }
        "mail.reply" => (respond(s, p, "reply"), vec![changed(&["draft"])]),
        "mail.reply_all" => (respond(s, p, "reply_all"), vec![changed(&["draft"])]),
        "mail.forward" => (respond(s, p, "forward"), vec![changed(&["draft"])]),
        "mail.save_draft" | "mail.send" => {
            let Some(did) = str_param(p, "draft_id") else {
                return (err("invalid_params", "`draft_id` is required"), vec![]);
            };
            let Some(d) = s.drafts.get_mut(did) else { return (err("not_found", "no such draft"), vec![]) };
            if d.sending {
                return (err("not_available", "This message is being sent; undo the send to edit it."), vec![]);
            }
            apply_fields(d, p);
            if id == "mail.save_draft" {
                return (ok(json!({ "draft_id": did, "saved_at": iso(NOW_MIN) })), vec![changed(&["draft"])]);
            }
            if d.to.is_empty() && d.cc.is_empty() && d.bcc.is_empty() {
                return (err("invalid_params", "Add at least one recipient."), vec![]);
            }
            d.sending = true;
            s.sent.push(did.to_owned());
            let delay = p.get("delay_seconds").and_then(Value::as_u64).unwrap_or(5).min(30);
            (
                ok(
                    json!({ "draft_id": did, "outbox_id": s.sent.len(), "undo_until": iso(NOW_MIN), "delay_seconds": delay }),
                ),
                vec![changed(&["draft", "outbox"])],
            )
        }
        "mail.undo_send" => {
            let did = str_param(p, "draft_id").map(str::to_owned).or_else(|| s.sent.last().cloned());
            match did.and_then(|d| s.drafts.get_mut(&d).map(|x| (d, x))) {
                Some((d, x)) if x.sending => {
                    x.sending = false;
                    s.sent.retain(|y| *y != d);
                    (ok(json!({ "undone": true, "draft_id": d })), vec![changed(&["draft", "outbox"])])
                }
                _ => (err("not_available", "Too late: the message is already on its way."), vec![]),
            }
        }
        "mail.discard_draft" => match str_param(p, "draft_id").and_then(|d| s.drafts.remove(d)) {
            Some(_) => (ok(json!({ "discarded": true })), vec![changed(&["draft"])]),
            None => (err("not_found", "no such draft"), vec![]),
        },
        "mail.attach" => {
            let Some(d) = str_param(p, "draft_id").and_then(|d| s.drafts.get_mut(d)) else {
                return (err("not_found", "no such draft"), vec![]);
            };
            let name = str_param(p, "name").unwrap_or("attachment").chars().take(255).collect::<String>();
            let size = str_param(p, "data_base64").map_or(0, |b| b.len() * 3 / 4);
            let att = json!({ "id": format!("a{}", d.attachments.len() + 1), "name": name, "size": size, "content_type": str_param(p, "content_type") });
            d.attachments.push(att.clone());
            (ok(json!({ "attachment": att })), vec![changed(&["draft"])])
        }
        "mail.remove_attachment" => {
            let att = str_param(p, "attachment").unwrap_or("");
            if let Some(d) = str_param(p, "draft_id").and_then(|d| s.drafts.get_mut(d)) {
                d.attachments.retain(|a| str_param(a, "id") != Some(att));
            }
            (ok(json!({ "removed": true })), vec![changed(&["draft"])])
        }
        "mail.allow_remote_images" => {
            let sender = str_param(p, "sender").map(str::to_owned).or_else(|| {
                str_param(p, "id")
                    .and_then(|i| message_row(s, i))
                    .and_then(|r| str_param(&r, "from_email").map(str::to_owned))
            });
            match sender {
                Some(a) => {
                    s.allowed.insert(a.clone());
                    (ok(json!({ "sender": a })), vec![changed(&["message_body"])])
                }
                None => (err("invalid_params", "`sender` or `id` is required"), vec![]),
            }
        }
        "mail.download_attachment" => (
            ok(
                json!({ "blob_key": "preview", "size": 482_133, "name": "Synthetic plan.pdf", "content_type": "application/pdf" }),
            ),
            vec![toast("success", "Downloaded (preview)")],
        ),
        "mail.fetch_body" => (ok(json!({ "fetched": true })), vec![]),
        "mail.set_auto_reply" => (ok(json!({ "status": str_param(p, "status") })), vec![changed(&["setting"])]),
        _ => (err("unknown_command", "no such mail command"), vec![]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(r: Option<Reply>) -> Value {
        match r {
            Some(Reply::Ok { value }) => value,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn dates_are_valid_rfc3339() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(NOW_MIN), "2026-10-17T12:00:00Z");
        assert_eq!(iso(NOW_MIN - 1440 * 17), "2026-09-30T12:00:00Z");
    }

    #[test]
    fn list_shapes_match_the_engine() {
        let rows = value(query("mail.list", &json!({ "folder": "inbox", "limit": 50 })));
        let rows = rows.as_array().unwrap();
        assert_eq!(rows.len(), 50);
        for key in [
            "id",
            "cursor",
            "from_name",
            "from_email",
            "subject",
            "preview",
            "received",
            "unread",
            "flagged",
            "has_attachments",
            "importance",
            "conversation_id",
        ] {
            assert!(rows[0].get(key).is_some(), "{key}");
        }
        let all = value(query("mail.list", &json!({ "folder": "inbox", "limit": 5000 })));
        assert_eq!(all.as_array().unwrap().len(), INBOX_COUNT);
        let folders = value(query("mail.folders", &Value::Null));
        assert!(folders.as_array().unwrap().iter().any(|f| f["kind"] == "inbox" && f["unread"].as_u64().unwrap() > 0));
        let m = value(query("mail.message", &json!({ "id": "f-inbox-0" })));
        assert!(m["body_html"].as_str().unwrap().contains("Hi Alex"));
        assert_eq!(m["remote_images_blocked"], true);
    }

    #[test]
    fn writes_change_what_queries_return() {
        run("mail.mark_read", &json!({ "id": "f-inbox-4" }));
        let m = value(query("mail.message", &json!({ "id": "f-inbox-4" })));
        assert_eq!(m["unread"], false);
        run("mail.archive", &json!({ "id": "f-inbox-8" }));
        let inbox = value(query("mail.list", &json!({ "folder": "inbox", "limit": 5000 })));
        assert!(!inbox.as_array().unwrap().iter().any(|r| r["id"] == "f-inbox-8"));
        let archive = value(query("mail.list", &json!({ "folder": "archive", "limit": 5000 })));
        assert!(archive.as_array().unwrap().iter().any(|r| r["id"] == "f-inbox-8"));
        let (reply, _) = run("mail.reply", &json!({ "id": "f-inbox-2" })).unwrap();
        let Reply::Ok { value: d } = reply else { panic!() };
        let draft = value(query("mail.draft", &json!({ "draft_id": d["draft_id"] })));
        assert!(
            draft["subject"].as_str().unwrap().starts_with("RE:")
                || draft["subject"].as_str().unwrap().starts_with("Re")
        );
        let (sent, _) = run("mail.send", &json!({ "draft_id": d["draft_id"] })).unwrap();
        assert!(matches!(sent, Reply::Ok { .. }));
        let (undone, _) = run("mail.undo_send", &Value::Null).unwrap();
        assert!(matches!(undone, Reply::Ok { .. }));
        assert!(matches!(run("mail.delete", &Value::Null).unwrap().0, Reply::Err { .. }));
    }
}
