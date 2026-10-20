//! Mail commands and queries (architecture §8 Mail).
//!
//! Every write is optimistic: the local change and its outbox entry commit in one
//! `Store::write`, then the outbox replayer is woken; `quorum_sync::mail`'s handlers replay
//! them against Graph. Programmatic calls never open anything: `mail.compose`, `mail.reply`,
//! `mail.reply_all` and `mail.forward` create a local draft and return its id; the UI decides
//! whether to show a compose pane for it.
//!
//! Shortcuts follow Outlook on the web's single-key set (inactive while typing): `R` reply,
//! `Shift+R` reply all, `Shift+F` forward, `E` archive, `Q` mark read, `U` mark unread, `V`
//! move, `C` categorize, `Backspace` delete; plus `Cmd+N` new, `Cmd+Enter` send, `Cmd+S` save
//! draft, `Cmd+Shift+G` flag.
//!
//! Bodies are stored as Graph sent them and sanitised here, at read time (`mail.message`,
//! `mail.conversation`, `mail.draft`), so a sanitiser policy change applies to cached mail.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use base64::Engine as _;
use jiff::{SignedDuration, Timestamp};
use quorum_graph::api::mail::{AutomaticRepliesSetting, AutomaticRepliesStatus, ExternalAudience, MailboxSettings};
use quorum_model::{
    AttachmentId, BodyType, ConversationId, DateTimeZone, EmailAddress, FlagStatus, GraphEnum, Importance,
    InferenceClassification, MailAddress, MailFolderId, Message, MessageId, Person, Sensitive,
};
use quorum_store::{Cursor, Draft, DraftKind, MessageQuery, NewOutboxEntry, OutboxId, Store, Tx};
use quorum_sync::mail::payload::{
    AutoReplyPayload, DeletePayload, Destination, DraftDeletePayload, DraftSyncPayload, KIND_AUTO_REPLY,
    KIND_DELETE_PERMANENT, KIND_DRAFT_DELETE, KIND_DRAFT_SYNC, KIND_MOVE, KIND_SEND, KIND_UPDATE, MovePayload,
    Previous, SETTINGS_RESOURCE, SendPayload, UpdatePayload,
};
use quorum_sync::mail::{
    CATEGORIES_SETTING, Compose, ComposeAttachment, ComposeMode, MAILBOX_SETTINGS_SETTING, draft_key, message_key,
};
use serde_json::{Map, Value, json};

use super::people::{store_err, sync_err};
use crate::command::{CommandFuture, CommandSpec, always};
use crate::error::{Error, ErrorKind, Result};
use crate::session::Session;

/// Most messages one command acts on.
pub const MAX_TARGETS: usize = 500;
/// Most rows `mail.list` returns.
pub const MAX_LIST: usize = 5000;
/// Default `mail.list` page.
pub const DEFAULT_LIST: usize = 200;
/// Largest file attached to a draft (Graph's inline attachment limit).
pub const MAX_ATTACHMENT_BYTES: usize = quorum_graph::api::mail::MAX_INLINE_ATTACHMENT_BYTES;
/// Longest undo-send delay.
pub const MAX_UNDO_SECONDS: u64 = 30;
/// Settings key: lower-cased sender addresses whose remote images load automatically.
pub const ALLOWED_SENDERS_SETTING: &str = "mail.remote_images.allowed_senders";
/// Settings key: the most recent queued send (`{draft_id, outbox_id}`), for `mail.undo_send`.
pub const LAST_SEND_SETTING: &str = "mail.last_send";
/// Most allow-listed senders kept.
const MAX_ALLOWED_SENDERS: usize = 5000;
/// Most categories on one message.
const MAX_CATEGORIES: usize = 25;
/// Longest category name.
const MAX_CATEGORY_LEN: usize = 255;
/// Longest attachment name.
const MAX_NAME_LEN: usize = 255;
/// Longest search text.
const MAX_QUERY_CHARS: usize = 256;
/// Most bodies sanitised by `mail.conversation`.
const MAX_CONVERSATION_BODIES: usize = 50;
/// Folder tree depth shown.
const MAX_FOLDER_DEPTH: usize = 32;

const fn cmd(
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

const TARGET_DOC: &str = r#"{"id": message id} or {"ids": [message id, ...]} (at most 500)"#;

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        cmd(
            "mail.compose",
            "New Message",
            Some("Mail/New Message"),
            Some("Cmd+N"),
            r#"{"to"?, "cc"?, "bcc"?, "subject"?, "body_html"?, "importance"?} → {"draft_id"} (creates a local draft; opens nothing)"#,
            false,
            compose,
        ),
        cmd(
            "mail.reply",
            "Reply",
            Some("Mail/Reply"),
            Some("R"),
            r#"{"id": message id, "body_html"?} → {"draft_id"}"#,
            false,
            reply,
        ),
        cmd(
            "mail.reply_all",
            "Reply All",
            Some("Mail/Reply All"),
            Some("Shift+R"),
            r#"{"id": message id, "body_html"?} → {"draft_id"}"#,
            false,
            reply_all,
        ),
        cmd(
            "mail.forward",
            "Forward",
            Some("Mail/Forward"),
            Some("Shift+F"),
            r#"{"id": message id, "to"?, "body_html"?} → {"draft_id"}"#,
            false,
            forward,
        ),
        cmd(
            "mail.save_draft",
            "Save Draft",
            Some("Mail/Save Draft"),
            Some("Cmd+S"),
            r#"{"draft_id": string, ...fields, "sync"?: bool (default true: mirror to the server's Drafts)}"#,
            false,
            save_draft,
        ),
        cmd(
            "mail.send",
            "Send",
            Some("Mail/Send"),
            Some("Cmd+Enter"),
            r#"{"draft_id": string, ...fields, "delay_seconds"?: 0..30 (default: setting mail.undo_send_seconds)} → {"draft_id", "outbox_id", "undo_until"}"#,
            true,
            send,
        ),
        cmd(
            "mail.undo_send",
            "Undo Send",
            Some("Mail/Undo Send"),
            None,
            r#"{"draft_id"?: string (default: the last message sent)} → {"undone": true, "draft_id"}"#,
            false,
            undo_send,
        ),
        cmd(
            "mail.discard_draft",
            "Discard Draft",
            Some("Mail/Discard Draft"),
            None,
            r#"{"draft_id": string}"#,
            true,
            discard_draft,
        ),
        cmd(
            "mail.attach",
            "Attach File",
            Some("Mail/Attach File"),
            None,
            r#"{"draft_id": string, "name": string, "content_type"?: string, "data_base64": string} (at most 3 MB) → {"attachment"}"#,
            false,
            attach,
        ),
        cmd(
            "mail.remove_attachment",
            "Remove Attachment",
            None,
            None,
            r#"{"draft_id": string, "attachment": string}"#,
            false,
            remove_attachment,
        ),
        cmd("mail.mark_read", "Mark as Read", Some("Mail/Mark as Read"), Some("Q"), TARGET_DOC, false, mark_read),
        cmd(
            "mail.mark_unread",
            "Mark as Unread",
            Some("Mail/Mark as Unread"),
            Some("U"),
            TARGET_DOC,
            false,
            mark_unread,
        ),
        cmd("mail.toggle_read", "Toggle Read", None, None, TARGET_DOC, false, toggle_read),
        cmd(
            "mail.flag",
            "Flag",
            Some("Mail/Flag"),
            Some("Cmd+Shift+G"),
            r#"{"id"|"ids", "toggle"?: bool (default true: unflags when every target is flagged)}"#,
            false,
            flag,
        ),
        cmd("mail.unflag", "Clear Flag", Some("Mail/Clear Flag"), None, TARGET_DOC, false, unflag),
        cmd(
            "mail.complete_flag",
            "Mark Flag Complete",
            Some("Mail/Mark Complete"),
            None,
            TARGET_DOC,
            false,
            complete_flag,
        ),
        cmd(
            "mail.categorize",
            "Categorize",
            Some("Mail/Categorize"),
            Some("C"),
            r#"{"id"|"ids", "categories": [string], "mode"?: "toggle"|"add"|"remove"|"set" (default toggle)}"#,
            false,
            categorize,
        ),
        cmd(
            "mail.move",
            "Move to Folder",
            Some("Mail/Move"),
            Some("V"),
            r#"{"id"|"ids", "folder": folder id or well-known name (inbox, archive, deleteditems, junkemail, ...)}"#,
            false,
            move_to,
        ),
        cmd("mail.archive", "Archive", Some("Mail/Archive"), Some("E"), TARGET_DOC, false, archive),
        cmd(
            "mail.delete",
            "Delete",
            Some("Mail/Delete"),
            Some("Backspace"),
            r#"{"id"|"ids", "permanent"?: bool} (moves to Deleted Items; deletes permanently from Deleted Items or with permanent)"#,
            true,
            delete,
        ),
        cmd(
            "mail.mark_focused",
            "Move to Focused",
            Some("Mail/Move to Focused"),
            None,
            TARGET_DOC,
            false,
            mark_focused,
        ),
        cmd("mail.mark_other", "Move to Other", Some("Mail/Move to Other"), None, TARGET_DOC, false, mark_other),
        cmd(
            "mail.allow_remote_images",
            "Always Show Images from Sender",
            None,
            None,
            r#"{"sender": address} or {"id": message id}"#,
            false,
            allow_remote_images,
        ),
        cmd(
            "mail.download_attachment",
            "Download Attachment",
            None,
            None,
            r#"{"id": message id, "attachment": attachment id} → {"blob_key", "size", "name", "content_type"}"#,
            false,
            download_attachment,
        ),
        cmd("mail.fetch_body", "Download Message", None, None, r#"{"id": message id}"#, false, fetch_body),
        cmd(
            "mail.set_auto_reply",
            "Automatic Replies",
            Some("Mail/Automatic Replies…"),
            None,
            r#"{"status": "disabled"|"alwaysEnabled"|"scheduled", "internal_message"?: html, "external_message"?: html, "external_audience"?: "none"|"contactsOnly"|"all", "start"?: {"dateTime","timeZone"}, "end"?: {...}}"#,
            false,
            set_auto_reply,
        ),
    ]
}

pub(crate) fn queries() -> Vec<CommandSpec> {
    vec![
        cmd(
            "mail.folders",
            "Mail Folders",
            None,
            None,
            "{} → [{id, name, kind, role, parent_id, depth, unread, total, favourite}]",
            false,
            q_folders,
        ),
        cmd(
            "mail.list",
            "Messages",
            None,
            None,
            r#"{"folder"?: id or well-known name (default inbox), "focused"?: bool, "unread_only"?: bool, "flagged_only"?: bool, "after"?: row cursor, "limit"?: 1..5000 (default 200), "group"?: "conversation"} → [row]"#,
            false,
            q_list,
        ),
        cmd(
            "mail.message",
            "Message",
            None,
            None,
            r#"{"id": message id, "allow_remote_images"?: bool} → headers, body_html (sanitised), remote_images_blocked, attachments"#,
            false,
            q_message,
        ),
        cmd(
            "mail.conversation",
            "Conversation",
            None,
            None,
            r#"{"id": conversation id} or {"message": message id}, "order"?: "oldest_first" (default) | "newest_first", "allow_remote_images"?: bool, "allow_remote_ids"?: [message id], "webview"?: bool (image URLs in the desktop webview's form) → [message: row fields + body_html (full), body_new_html (without quoted history, when there is some), has_quoted, own_background, folder_kind, from_me, body_loading]"#,
            false,
            q_conversation,
        ),
        cmd(
            "mail.draft",
            "Draft",
            None,
            None,
            r#"{"draft_id": string} → fields, body_html, quote_html, attachments"#,
            false,
            q_draft,
        ),
        cmd(
            "mail.drafts",
            "Local Drafts",
            None,
            None,
            "{} → [{draft_id, mode, subject, to, sending, send_at, updated_at}]",
            false,
            q_drafts,
        ),
        cmd(
            "mail.search",
            "Search Mail",
            None,
            None,
            r#"{"query": string, "folder"?: id, "limit"?: 1..200, "remote"?: bool (default true)} → [row with snippet]"#,
            false,
            q_search,
        ),
        cmd("mail.categories", "Mail Categories", None, None, "{} → [{name, color}]", false, q_categories),
        cmd(
            "mail.auto_reply",
            "Automatic Replies Setting",
            None,
            None,
            "{} → the automatic replies setting, or null",
            false,
            q_auto_reply,
        ),
    ]
}

// ---------------------------------------------------------------------------------------------
// Parameter helpers (hostile input: never panic, cap everything).
// ---------------------------------------------------------------------------------------------

fn obj(p: &Value) -> Result<&Map<String, Value>> {
    p.as_object().ok_or_else(|| Error::params("parameters must be an object"))
}

fn opt_str<'a>(p: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(Error::params(format!("`{key}` must be a string"))),
    }
}

fn req_str<'a>(p: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    opt_str(p, key)?.filter(|s| !s.is_empty()).ok_or_else(|| Error::params(format!("`{key}` is required")))
}

fn opt_bool(p: &Map<String, Value>, key: &str) -> Result<Option<bool>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(Error::params(format!("`{key}` must be true or false"))),
    }
}

fn opt_u64(p: &Map<String, Value>, key: &str) -> Result<Option<u64>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| Error::params(format!("`{key}` must be a non-negative integer"))),
    }
}

fn message_id(s: &str) -> Result<MessageId> {
    MessageId::new(s).map_err(|_| Error::params("not a valid message id"))
}

/// `{"id"}` or `{"ids": [...]}`, deduplicated, at most [`MAX_TARGETS`].
fn targets(p: &Map<String, Value>) -> Result<Vec<MessageId>> {
    let mut out = Vec::new();
    if let Some(id) = opt_str(p, "id")? {
        out.push(message_id(id)?);
    }
    match p.get("ids") {
        None | Some(Value::Null) => {}
        Some(Value::Array(ids)) => {
            if ids.len() > MAX_TARGETS {
                return Err(Error::params(format!("at most {MAX_TARGETS} messages at a time")));
            }
            for v in ids {
                let s = v.as_str().ok_or_else(|| Error::params("`ids` must be strings"))?;
                out.push(message_id(s)?);
            }
        }
        Some(_) => return Err(Error::params("`ids` must be a list")),
    }
    let mut seen = HashSet::new();
    out.retain(|id| seen.insert(id.clone()));
    if out.is_empty() {
        return Err(Error::params("select a message first (`id` or `ids`)"));
    }
    out.truncate(MAX_TARGETS);
    Ok(out)
}

fn one_target(p: &Map<String, Value>) -> Result<MessageId> {
    message_id(req_str(p, "id")?)
}

fn parse_address(s: &str) -> Result<EmailAddress> {
    let s = s.trim();
    // "Name <addr>" is accepted too.
    let (name, addr) = match (s.rfind('<'), s.ends_with('>')) {
        (Some(i), true) => (s.get(..i).unwrap_or("").trim().trim_matches('"'), s.get(i + 1..s.len() - 1).unwrap_or("")),
        _ => ("", s),
    };
    let address = MailAddress::new(addr.trim()).map_err(|_| Error::params("not a valid email address"))?;
    Ok(EmailAddress { name: Sensitive::new(name), address: Some(address) })
}

/// A recipient list: strings (`a@x`, `Name <a@x>`, or `;`/`,`-separated) or `{"name","address"}`.
fn recipients(p: &Map<String, Value>, key: &str) -> Result<Option<Vec<EmailAddress>>> {
    let list: Vec<Value> = match p.get(key) {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(s)) => {
            s.split([';', ',']).filter(|x| !x.trim().is_empty()).map(|x| Value::String(x.to_owned())).collect()
        }
        Some(Value::Array(a)) => a.clone(),
        Some(_) => return Err(Error::params(format!("`{key}` must be a list of addresses"))),
    };
    if list.len() > quorum_sync::mail::compose::MAX_RECIPIENTS {
        return Err(Error::params(format!("too many recipients in `{key}`")));
    }
    let mut out: Vec<EmailAddress> = Vec::new();
    for v in list {
        let a = match v {
            Value::String(s) => parse_address(&s)?,
            Value::Object(o) => {
                let addr = o.get("address").or_else(|| o.get("email")).and_then(Value::as_str).unwrap_or("");
                let mut a = parse_address(addr)?;
                let name = o.get("name").and_then(Value::as_str).unwrap_or("");
                a.name = Sensitive::new(name.chars().take(256).collect::<String>());
                a
            }
            _ => return Err(Error::params(format!("`{key}` must be a list of addresses"))),
        };
        if !out.iter().any(|x| same_address(x, &a)) {
            out.push(a);
        }
    }
    Ok(Some(out))
}

fn same_address(a: &EmailAddress, b: &EmailAddress) -> bool {
    match (&a.address, &b.address) {
        (Some(x), Some(y)) => x.eq_ignore_case(y),
        _ => false,
    }
}

fn importance(p: &Map<String, Value>) -> Result<Option<Importance>> {
    match opt_str(p, "importance")? {
        None => Ok(None),
        Some(s) => match Importance::from_graph(s) {
            Importance::Unknown(_) => Err(Error::params("`importance` must be low, normal or high")),
            i => Ok(Some(i)),
        },
    }
}

/// Undoes the sanitiser's `quorum-link:` / `quorum-remote:` rewriting in HTML coming back from
/// the compose editor, so the sent message carries the real URLs.
fn restore_urls(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    // Bounded by the input length: every iteration consumes at least one byte.
    while let Some(i) = rest.find("quorum-") {
        let (head, tail) = rest.split_at(i);
        out.push_str(head);
        let end = tail.find(['"', '\'', ' ', '>', '<']).unwrap_or(tail.len());
        let (url, after) = tail.split_at(end);
        match quorum_sanitize::QuorumUrl::parse(url) {
            Some(u @ (quorum_sanitize::QuorumUrl::Link(_) | quorum_sanitize::QuorumUrl::Remote(_))) => {
                out.push_str(&u.target().replace('"', "%22").replace('\'', "%27"));
                rest = after;
            }
            _ => {
                out.push_str("quorum-");
                rest = tail.get("quorum-".len()..).unwrap_or("");
            }
        }
    }
    out.push_str(rest);
    out
}

fn body_param(p: &Map<String, Value>) -> Result<Option<String>> {
    match opt_str(p, "body_html")? {
        None => Ok(None),
        Some(s) if s.len() > quorum_store::MAX_DRAFT_LEN / 2 => Err(Error::params("the message is too long")),
        Some(s) => Ok(Some(restore_urls(s))),
    }
}

fn subject_param(p: &Map<String, Value>) -> Result<Option<Sensitive>> {
    Ok(opt_str(p, "subject")?.map(|s| {
        let clean: String =
            s.chars().filter(|c| !c.is_control()).take(quorum_sync::mail::compose::MAX_SUBJECT_LEN).collect();
        Sensitive::new(clean)
    }))
}

// ---------------------------------------------------------------------------------------------
// Session helpers.
// ---------------------------------------------------------------------------------------------

fn store(s: &Session) -> Result<Store> {
    s.store()
}

fn sync_ctx(s: &Session) -> Result<quorum_sync::Ctx> {
    Ok(s.sync()?.ctx().clone())
}

fn kick(s: &Session) {
    if let Ok(h) = s.sync() {
        h.outbox_changed();
    }
}

fn entry(kind: &str, resource: String, payload: &impl serde::Serialize) -> Result<NewOutboxEntry> {
    Ok(NewOutboxEntry {
        kind: kind.to_owned(),
        resource,
        local_id: None,
        payload: serde_json::to_value(payload).map_err(|e| Error::new(ErrorKind::Bug, "payload").with_source(e))?,
    })
}

fn new_draft_id() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let now = Timestamp::now();
    format!("d-{}-{}-{n}", now.as_millisecond(), now.subsec_nanosecond())
}

/// Folder roles: well-known name → id.
async fn roles(store: &Store) -> Result<BTreeMap<String, MailFolderId>> {
    quorum_sync::mail::well_known(store).await.map_err(sync_err)
}

/// Accepted aliases for well-known folder names.
fn canonical_role(name: &str) -> Option<&'static str> {
    Some(match name.to_ascii_lowercase().as_str() {
        "inbox" => "inbox",
        "drafts" => "drafts",
        "sent" | "sentitems" | "sent_items" => "sentitems",
        "deleted" | "deleteditems" | "deleted_items" | "trash" => "deleteditems",
        "archive" => "archive",
        "junk" | "junkemail" | "spam" => "junkemail",
        "outbox" => "outbox",
        _ => return None,
    })
}

/// A folder parameter: a well-known name or a cached folder id.
async fn resolve_folder(store: &Store, name: &str) -> Result<MailFolderId> {
    if let Some(role) = canonical_role(name) {
        if let Some(id) = roles(store).await?.get(role) {
            return Ok(id.clone());
        }
        // Not resolved yet (first sync running): fall back to a folder with that role.
        let folders = store.mail_folders().await.map_err(store_err)?;
        if let Some(f) = folders.items.iter().find(|f| f.well_known_name.as_ref().is_some_and(|w| w.as_graph() == role))
        {
            return Ok(f.id.clone());
        }
        return MailFolderId::new(role).map_err(|_| Error::params("not a valid folder"));
    }
    MailFolderId::new(name).map_err(|_| Error::params("not a valid folder id"))
}

fn kind_of(role: Option<&str>) -> &'static str {
    match role {
        Some("inbox") => "inbox",
        Some("drafts") => "drafts",
        Some("sentitems") => "sent",
        Some("deleteditems") => "deleted",
        Some("archive") => "archive",
        Some("junkemail") => "junk",
        Some("outbox") => "outbox",
        _ => "folder",
    }
}

fn rfc3339(t: Option<Timestamp>) -> Value {
    t.map_or(Value::Null, |t| Value::String(t.to_string()))
}

fn address_json(a: &EmailAddress) -> Value {
    json!({ "name": a.name.as_str(), "address": a.address.as_ref().map(MailAddress::as_str) })
}

fn addresses_json(list: &[EmailAddress]) -> Value {
    Value::Array(list.iter().map(address_json).collect())
}

/// The list row the UI and notifications read.
fn row(m: &Message) -> Value {
    let from = m.from.as_ref().or(m.sender.as_ref());
    let sort = m.received_date_time.or(m.sent_date_time).map_or(0, Timestamp::as_millisecond);
    json!({
        "id": m.id.as_str(),
        "cursor": format!("{sort}:{}", m.id.as_str()),
        "folder_id": m.parent_folder_id.as_ref().map(MailFolderId::as_str),
        "conversation_id": m.conversation_id.as_ref().map(ConversationId::as_str),
        "from_name": from.map(EmailAddress::display).unwrap_or(""),
        "from_email": from.and_then(|a| a.address.as_ref()).map(MailAddress::as_str).unwrap_or(""),
        "from": from.map(EmailAddress::display).unwrap_or(""),
        "to": m.to_recipients.iter().map(EmailAddress::display).collect::<Vec<_>>().join(", "),
        "subject": m.subject.as_str(),
        "preview": m.body_preview.as_str(),
        "received": rfc3339(m.received_date_time.or(m.sent_date_time)),
        "unread": !m.is_read,
        "is_read": m.is_read,
        "flagged": m.flag_status == FlagStatus::Flagged,
        "flag": m.flag_status.as_graph(),
        "has_attachments": m.has_attachments,
        "importance": m.importance.as_graph(),
        "categories": m.categories,
        "is_draft": m.is_draft,
        "focused": m.inference_classification != InferenceClassification::Other,
        "count": 1,
    })
}

// ---------------------------------------------------------------------------------------------
// Message writes.
// ---------------------------------------------------------------------------------------------

fn previous(m: &Message) -> Previous {
    Previous {
        is_read: m.is_read,
        flag: m.flag_status.clone(),
        categories: m.categories.clone(),
        focused: m.inference_classification != InferenceClassification::Other,
    }
}

fn apply_update(m: &mut Message, u: &UpdatePayload) {
    if let Some(r) = u.is_read {
        m.is_read = r;
    }
    if let Some(f) = &u.flag {
        m.flag_status = f.clone();
    }
    if let Some(c) = &u.categories {
        m.categories = c.clone();
    }
    if let Some(f) = u.focused {
        m.inference_classification = if f { InferenceClassification::Focused } else { InferenceClassification::Other };
    }
}

/// Applies `change` to every cached target and queues the Graph update, atomically.
/// `change` returns `None` to leave a message alone. Returns how many changed.
async fn update_messages(
    s: &Session,
    ids: Vec<MessageId>,
    change: impl Fn(&Message, &[Message]) -> Option<UpdatePayload> + Send + 'static,
) -> Result<Value> {
    let store = store(s)?;
    let n = store
        .write(move |tx| {
            let all: Vec<Message> = ids.iter().filter_map(|id| tx.read().message(id).ok().flatten()).collect();
            let mut n = 0u32;
            for m in &all {
                let Some(u) = change(m, &all) else { continue };
                let mut updated = m.clone();
                apply_update(&mut updated, &u);
                if updated == *m {
                    continue;
                }
                tx.upsert_messages(std::slice::from_ref(&updated))?;
                tx.outbox_enqueue(&to_entry(KIND_UPDATE, message_key(&m.id), &u)?)?;
                n = n.saturating_add(1);
            }
            Ok(n)
        })
        .await
        .map_err(store_err)?;
    kick(s);
    Ok(json!({ "changed": n }))
}

fn to_entry(kind: &str, resource: String, payload: &impl serde::Serialize) -> quorum_store::Result<NewOutboxEntry> {
    entry(kind, resource, payload)
        .map_err(|_| quorum_store::Error::new(quorum_store::ErrorKind::InvalidInput, "payload could not be encoded"))
}

fn update(m: &Message) -> UpdatePayload {
    UpdatePayload {
        id: m.id.clone(),
        is_read: None,
        flag: None,
        categories: None,
        focused: None,
        previous: previous(m),
    }
}

fn mark_read(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let ids = targets(obj(&p)?)?;
        update_messages(s, ids, |m, _| Some(UpdatePayload { is_read: Some(true), ..update(m) })).await
    })
}

fn mark_unread(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let ids = targets(obj(&p)?)?;
        update_messages(s, ids, |m, _| Some(UpdatePayload { is_read: Some(false), ..update(m) })).await
    })
}

fn toggle_read(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let ids = targets(obj(&p)?)?;
        update_messages(s, ids, |m, all| {
            let to = all.iter().any(|x| !x.is_read);
            Some(UpdatePayload { is_read: Some(to), ..update(m) })
        })
        .await
    })
}

fn flag(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p = obj(&p)?;
        let toggle = opt_bool(p, "toggle")?.unwrap_or(true);
        let ids = targets(p)?;
        update_messages(s, ids, move |m, all| {
            let all_flagged = all.iter().all(|x| x.flag_status == FlagStatus::Flagged);
            let to = if toggle && all_flagged { FlagStatus::NotFlagged } else { FlagStatus::Flagged };
            Some(UpdatePayload { flag: Some(to), ..update(m) })
        })
        .await
    })
}

fn unflag(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let ids = targets(obj(&p)?)?;
        update_messages(s, ids, |m, _| Some(UpdatePayload { flag: Some(FlagStatus::NotFlagged), ..update(m) })).await
    })
}

fn complete_flag(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let ids = targets(obj(&p)?)?;
        update_messages(s, ids, |m, _| Some(UpdatePayload { flag: Some(FlagStatus::Complete), ..update(m) })).await
    })
}

fn mark_focused(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let ids = targets(obj(&p)?)?;
        update_messages(s, ids, |m, _| Some(UpdatePayload { focused: Some(true), ..update(m) })).await
    })
}

fn mark_other(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let ids = targets(obj(&p)?)?;
        update_messages(s, ids, |m, _| Some(UpdatePayload { focused: Some(false), ..update(m) })).await
    })
}

fn categorize(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p = obj(&p)?;
        let ids = targets(p)?;
        let cats: Vec<String> = match p.get("categories").or_else(|| p.get("category")) {
            Some(Value::String(c)) => vec![c.clone()],
            Some(Value::Array(list)) => list.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
            _ => return Err(Error::params("`categories` must be a list of category names")),
        };
        if cats.len() > MAX_CATEGORIES || cats.iter().any(|c| c.trim().is_empty() || c.len() > MAX_CATEGORY_LEN) {
            return Err(Error::params("invalid category names"));
        }
        let cats: Vec<String> = cats.iter().map(|c| c.trim().chars().filter(|ch| !ch.is_control()).collect()).collect();
        let mode = opt_str(p, "mode")?.unwrap_or("toggle").to_owned();
        if !matches!(mode.as_str(), "toggle" | "add" | "remove" | "set") {
            return Err(Error::params("`mode` must be toggle, add, remove or set"));
        }
        update_messages(s, ids, move |m, all| {
            let mut next = m.categories.clone();
            let toggle_off = mode == "toggle" && cats.iter().all(|c| all.iter().all(|x| x.categories.contains(c)));
            match mode.as_str() {
                "set" => next = cats.clone(),
                "remove" => next.retain(|c| !cats.contains(c)),
                _ if toggle_off => next.retain(|c| !cats.contains(c)),
                _ => {
                    for c in &cats {
                        if !next.contains(c) {
                            next.push(c.clone());
                        }
                    }
                }
            }
            next.truncate(MAX_CATEGORIES);
            Some(UpdatePayload { categories: Some(next), ..update(m) })
        })
        .await
    })
}

/// Moves cached targets to `dest` (local id for the optimistic move, if known).
async fn move_messages(
    s: &Session,
    ids: Vec<MessageId>,
    dest: Destination,
    local: MailFolderId,
    permanent_if_in: Option<MailFolderId>,
    permanent: bool,
) -> Result<Value> {
    let store = store(s)?;
    let (moved, deleted) = store
        .write(move |tx| {
            let (mut moved, mut deleted) = (0u32, 0u32);
            for id in &ids {
                let Some(m) = tx.read().message(id)? else { continue };
                let in_trash = permanent_if_in.as_ref().is_some_and(|t| m.parent_folder_id.as_ref() == Some(t));
                if permanent || in_trash {
                    tx.delete_messages(std::slice::from_ref(id))?;
                    let payload = DeletePayload { id: id.clone(), message: Some(m) };
                    tx.outbox_enqueue(&to_entry(KIND_DELETE_PERMANENT, message_key(id), &payload)?)?;
                    deleted = deleted.saturating_add(1);
                    continue;
                }
                if m.parent_folder_id.as_ref() == Some(&local) {
                    continue;
                }
                tx.move_message(id, &local)?;
                let payload = MovePayload {
                    id: id.clone(),
                    destination: dest.clone(),
                    previous_folder: m.parent_folder_id.clone(),
                };
                tx.outbox_enqueue(&to_entry(KIND_MOVE, message_key(id), &payload)?)?;
                moved = moved.saturating_add(1);
            }
            Ok((moved, deleted))
        })
        .await
        .map_err(store_err)?;
    kick(s);
    Ok(json!({ "moved": moved, "deleted": deleted }))
}

async fn destination(store: &Store, folder: &str) -> Result<(Destination, MailFolderId)> {
    if let Some(role) = canonical_role(folder) {
        let local = resolve_folder(store, role).await?;
        return Ok((Destination::WellKnown(role.to_owned()), local));
    }
    let id = MailFolderId::new(folder).map_err(|_| Error::params("not a valid folder id"))?;
    let known = store.mail_folders().await.map_err(store_err)?.items.iter().any(|f| f.id == id);
    if !known {
        return Err(Error::new(ErrorKind::NotFound, "no such folder"));
    }
    Ok((Destination::Id(id.clone()), id))
}

fn move_to(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p = obj(&p)?;
        let ids = targets(p)?;
        let folder = req_str(p, "folder").or_else(|_| req_str(p, "destination"))?;
        let (dest, local) = destination(&store(s)?, folder).await?;
        move_messages(s, ids, dest, local, None, false).await
    })
}

fn archive(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let ids = targets(obj(&p)?)?;
        let (dest, local) = destination(&store(s)?, "archive").await?;
        move_messages(s, ids, dest, local, None, false).await
    })
}

fn delete(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p = obj(&p)?;
        let permanent = opt_bool(p, "permanent")?.unwrap_or(false);
        let ids = targets(p)?;
        let store = store(s)?;
        let (dest, local) = destination(&store, "deleteditems").await?;
        let trash = roles(&store).await?.get("deleteditems").cloned().or_else(|| Some(local.clone()));
        move_messages(s, ids, dest, local, trash, permanent).await
    })
}

// ---------------------------------------------------------------------------------------------
// Compose.
// ---------------------------------------------------------------------------------------------

fn read_compose(d: &Draft) -> Compose {
    serde_json::from_value(d.fields.clone()).unwrap_or_default()
}

fn write_compose(d: &mut Draft, c: &Compose) -> quorum_store::Result<()> {
    d.fields = serde_json::to_value(c)
        .map_err(|_| quorum_store::Error::new(quorum_store::ErrorKind::InvalidInput, "draft fields"))?;
    Ok(())
}

/// Applies the editable fields present in `p` to a draft.
fn apply_fields(p: &Map<String, Value>, c: &mut Compose, body: &mut Sensitive) -> Result<()> {
    if let Some(to) = recipients(p, "to")? {
        c.to = to;
    }
    if let Some(cc) = recipients(p, "cc")? {
        c.cc = cc;
    }
    if let Some(bcc) = recipients(p, "bcc")? {
        c.bcc = bcc;
    }
    if let Some(sub) = subject_param(p)? {
        c.subject = sub;
    }
    if let Some(i) = importance(p)? {
        c.importance = if i == Importance::Normal { None } else { Some(i) };
    }
    if let Some(r) = opt_bool(p, "request_read_receipt")? {
        c.request_read_receipt = r;
    }
    if let Some(b) = body_param(p)? {
        *body = Sensitive::new(b);
    }
    Ok(())
}

async fn load_draft(store: &Store, id: &str) -> Result<Draft> {
    if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
        return Err(Error::params("not a valid draft id"));
    }
    match store.draft(id.to_owned()).await.map_err(store_err)? {
        Some(d) if d.kind == DraftKind::Mail => Ok(d),
        _ => Err(Error::new(ErrorKind::NotFound, "no such draft")),
    }
}

fn compose(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p = if p.is_null() { Map::new() } else { obj(&p)?.clone() };
        let store = store(s)?;
        let mut c = Compose::default();
        let mut body = Sensitive::default();
        apply_fields(&p, &mut c, &mut body)?;
        let id = new_draft_id();
        let mut d = Draft::new(id.clone(), DraftKind::Mail);
        d.body = body;
        write_compose(&mut d, &c).map_err(store_err)?;
        store.save_draft(d).await.map_err(store_err)?;
        Ok(json!({ "draft_id": id }))
    })
}

fn has_prefix(subject: &str, prefixes: &[&str]) -> bool {
    let lower = subject.trim_start().to_lowercase();
    prefixes.iter().any(|p| lower.starts_with(p))
}

async fn own_addresses(store: &Store) -> Vec<MailAddress> {
    let me: Option<Person> = store.setting("people.me".to_owned()).await.ok().flatten();
    me.and_then(|p| p.email).into_iter().collect()
}

async fn respond(s: &Session, p: Value, mode: ComposeMode) -> Result<Value> {
    let p = obj(&p)?;
    let id = one_target(p)?;
    let store = store(s)?;
    let m = store
        .message(id.clone())
        .await
        .map_err(store_err)?
        .ok_or_else(|| Error::new(ErrorKind::NotFound, "no such message"))?;
    let tag = match mode {
        ComposeMode::Reply => "reply",
        ComposeMode::ReplyAll => "reply_all",
        ComposeMode::Forward => "forward",
        ComposeMode::New => "new",
    };
    let context = format!("{tag}:{}", id.as_str());
    if context.len() <= 1024
        && let Some(existing) = store
            .read({
                let c = context.clone();
                move |r| r.draft_for_context(DraftKind::Mail, &c)
            })
            .await
            .map_err(store_err)?
        && !read_compose(&existing).sending
    {
        return Ok(json!({ "draft_id": existing.id, "existing": true }));
    }
    let me = own_addresses(&store).await;
    let is_me = |a: &EmailAddress| a.address.as_ref().is_some_and(|x| me.iter().any(|y| y.eq_ignore_case(x)));
    let mut c = Compose { mode, source: Some(id.clone()), ..Compose::default() };
    let author: Vec<EmailAddress> = if m.reply_to.is_empty() {
        m.from.clone().or(m.sender.clone()).into_iter().collect()
    } else {
        m.reply_to.clone()
    };
    match mode {
        ComposeMode::Reply => c.to = author,
        ComposeMode::ReplyAll => {
            let mut to: Vec<EmailAddress> = Vec::new();
            for a in author.into_iter().chain(m.to_recipients.iter().cloned()) {
                if !to.iter().any(|x| same_address(x, &a))
                    && (!is_me(&a) || to.is_empty() && m.to_recipients.len() <= 1)
                {
                    to.push(a);
                }
            }
            to.retain(|a| a.address.is_some());
            let mut cc: Vec<EmailAddress> = Vec::new();
            for a in m.cc_recipients.iter().cloned() {
                if !is_me(&a) && !to.iter().chain(cc.iter()).any(|x| same_address(x, &a)) && a.address.is_some() {
                    cc.push(a);
                }
            }
            c.to = to;
            c.cc = cc;
        }
        ComposeMode::Forward | ComposeMode::New => {}
    }
    c.to.retain(|a| a.address.is_some());
    let subject = m.subject.as_str();
    c.subject = Sensitive::new(match mode {
        ComposeMode::Forward if !has_prefix(subject, &["fw:", "fwd:"]) => format!("FW: {subject}"),
        ComposeMode::Reply | ComposeMode::ReplyAll if !has_prefix(subject, &["re:"]) => format!("RE: {subject}"),
        _ => subject.to_owned(),
    });
    let mut body = Sensitive::default();
    apply_fields(p, &mut c, &mut body)?;
    let draft_id = new_draft_id();
    let mut d = Draft::new(draft_id.clone(), DraftKind::Mail);
    d.context = Some(context).filter(|c| c.len() <= 1024);
    d.body = body;
    write_compose(&mut d, &c).map_err(store_err)?;
    store.save_draft(d).await.map_err(store_err)?;
    Ok(json!({ "draft_id": draft_id }))
}

fn reply(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(respond(s, p, ComposeMode::Reply))
}

fn reply_all(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(respond(s, p, ComposeMode::ReplyAll))
}

fn forward(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(respond(s, p, ComposeMode::Forward))
}

/// Queues a draft sync unless one is already waiting for this draft.
fn enqueue_draft_sync(tx: &mut Tx<'_>, draft_id: &str) -> quorum_store::Result<()> {
    let key = draft_key(draft_id);
    let pending = tx.read().outbox_entries(500)?;
    let waiting = pending.items.iter().any(|e| {
        e.kind == KIND_DRAFT_SYNC
            && e.resource == key
            && e.state == quorum_store::OutboxState::Pending
            && e.attempts == 0
    });
    if !waiting {
        tx.outbox_enqueue(&to_entry(KIND_DRAFT_SYNC, key, &DraftSyncPayload { draft_id: draft_id.to_owned() })?)?;
    }
    Ok(())
}

fn save_draft(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p = obj(&p)?.clone();
        let store = store(s)?;
        let id = req_str(&p, "draft_id")?.to_owned();
        let sync = opt_bool(&p, "sync")?.unwrap_or(true);
        let mut d = load_draft(&store, &id).await?;
        let mut c = read_compose(&d);
        if c.sending {
            return Err(Error::new(ErrorKind::NotAvailable, "this message is being sent; undo the send to edit it"));
        }
        apply_fields(&p, &mut c, &mut d.body)?;
        write_compose(&mut d, &c).map_err(store_err)?;
        let saved = store
            .write(move |tx| {
                let at = tx.save_draft(&d)?;
                if sync {
                    enqueue_draft_sync(tx, &d.id)?;
                }
                Ok(at)
            })
            .await
            .map_err(store_err)?;
        if sync {
            kick(s);
        }
        Ok(json!({ "draft_id": id, "saved_at": saved.to_string() }))
    })
}

async fn undo_seconds(s: &Session, p: &Map<String, Value>) -> Result<u64> {
    if let Some(n) = opt_u64(p, "delay_seconds")? {
        return Ok(n.min(MAX_UNDO_SECONDS));
    }
    let v = super::settings::get(s, "mail.undo_send_seconds").await.unwrap_or(Value::Null);
    Ok(v.as_u64().unwrap_or(5).min(MAX_UNDO_SECONDS))
}

fn send(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p = obj(&p)?.clone();
        let store = store(s)?;
        let id = req_str(&p, "draft_id")?.to_owned();
        let mut d = load_draft(&store, &id).await?;
        let mut c = read_compose(&d);
        if c.sending {
            return Err(Error::new(ErrorKind::NotAvailable, "this message is already being sent"));
        }
        apply_fields(&p, &mut c, &mut d.body)?;
        if !c.has_recipients() {
            return Err(Error::params("add at least one recipient"));
        }
        let delay = undo_seconds(s, &p).await?;
        let not_before = Timestamp::now()
            .checked_add(SignedDuration::from_secs(i64::try_from(delay).unwrap_or(0)))
            .unwrap_or_else(|_| Timestamp::now());
        c.sending = true;
        c.send_at = Some(not_before);
        let payload = SendPayload { draft_id: id.clone(), compose: c.clone(), body_html: d.body.clone() };
        let draft_id = id.clone();
        let outbox = store
            .write(move |tx| {
                let oid = tx.outbox_enqueue_at(&to_entry(KIND_SEND, draft_key(&draft_id), &payload)?, not_before)?;
                c.outbox_id = Some(oid.0);
                write_compose(&mut d, &c)?;
                tx.save_draft(&d)?;
                tx.set_setting(LAST_SEND_SETTING, &json!({ "draft_id": draft_id, "outbox_id": oid.0 }))?;
                Ok(oid)
            })
            .await
            .map_err(store_err)?;
        kick(s);
        Ok(
            json!({ "draft_id": id, "outbox_id": outbox.0, "undo_until": not_before.to_string(), "delay_seconds": delay }),
        )
    })
}

fn undo_send(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p = if p.is_null() { Map::new() } else { obj(&p)?.clone() };
        let store = store(s)?;
        let id = match opt_str(&p, "draft_id")? {
            Some(id) => id.to_owned(),
            None => store
                .setting::<Value>(LAST_SEND_SETTING.to_owned())
                .await
                .map_err(store_err)?
                .and_then(|v| v.get("draft_id").and_then(Value::as_str).map(str::to_owned))
                .ok_or_else(|| Error::new(ErrorKind::NotAvailable, "nothing to undo"))?,
        };
        let mut d = load_draft(&store, &id)
            .await
            .map_err(|_| Error::new(ErrorKind::NotAvailable, "the message was already sent"))?;
        let mut c = read_compose(&d);
        let Some(oid) = c.outbox_id.filter(|_| c.sending) else {
            return Err(Error::new(ErrorKind::NotAvailable, "this draft is not being sent"));
        };
        let undone = store
            .write(move |tx| {
                if tx.outbox_discard_pending(OutboxId(oid))?.is_none() {
                    return Ok(false);
                }
                c.sending = false;
                c.send_at = None;
                c.outbox_id = None;
                write_compose(&mut d, &c)?;
                tx.save_draft(&d)?;
                Ok(true)
            })
            .await
            .map_err(store_err)?;
        if !undone {
            return Err(Error::new(ErrorKind::NotAvailable, "too late: the message is already on its way"));
        }
        Ok(json!({ "undone": true, "draft_id": id }))
    })
}

fn discard_draft(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p = obj(&p)?;
        let store = store(s)?;
        let id = req_str(p, "draft_id")?.to_owned();
        let d = load_draft(&store, &id).await?;
        let c = read_compose(&d);
        if c.sending {
            return Err(Error::new(ErrorKind::NotAvailable, "this message is being sent; undo the send first"));
        }
        let keys: Vec<String> = c.attachments.iter().map(|a| a.blob_key.clone()).collect();
        let server = c.server_id.clone();
        let draft_id = id.clone();
        store
            .write(move |tx| {
                tx.delete_draft(&draft_id)?;
                if let Some(server_id) = server {
                    let payload = DraftDeletePayload { server_id: server_id.clone() };
                    tx.outbox_enqueue(&to_entry(KIND_DRAFT_DELETE, draft_key(&draft_id), &payload)?)?;
                    // The server copy is cached as a message in Drafts: drop it now.
                    tx.delete_messages(std::slice::from_ref(&server_id))?;
                }
                Ok(())
            })
            .await
            .map_err(store_err)?;
        for k in keys {
            let _ = store.delete_blob(k).await;
        }
        kick(s);
        Ok(json!({ "discarded": true, "draft_id": id }))
    })
}

fn clean_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let clean: String = base.chars().filter(|c| !c.is_control()).take(MAX_NAME_LEN).collect();
    let clean = clean.trim().trim_start_matches('.').to_owned();
    if clean.is_empty() { "attachment".to_owned() } else { clean }
}

fn attach(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p = obj(&p)?;
        let store = store(s)?;
        let id = req_str(p, "draft_id")?.to_owned();
        let name = clean_name(req_str(p, "name")?);
        let content_type = opt_str(p, "content_type")?
            .filter(|t| t.len() <= 255 && t.chars().all(|c| c.is_ascii_graphic()))
            .map(str::to_owned);
        let data = req_str(p, "data_base64")?;
        if data.len() > MAX_ATTACHMENT_BYTES.saturating_mul(4) / 3 + 8 {
            return Err(Error::params("attachments are limited to 3 MB for now"));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data.trim())
            .map_err(|_| Error::params("`data_base64` is not valid base64"))?;
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err(Error::params("attachments are limited to 3 MB for now"));
        }
        let mut d = load_draft(&store, &id).await?;
        let mut c = read_compose(&d);
        if c.sending {
            return Err(Error::new(ErrorKind::NotAvailable, "this message is being sent"));
        }
        if c.attachments.len() >= quorum_sync::mail::compose::MAX_ATTACHMENTS {
            return Err(Error::params("too many attachments"));
        }
        let att_id = format!("a{}", Timestamp::now().as_microsecond());
        let blob_key = format!("mail-compose:{id}:{att_id}");
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        store.put_blob(blob_key.clone(), bytes).await.map_err(store_err)?;
        c.attachments.push(ComposeAttachment {
            id: att_id.clone(),
            name: Sensitive::new(name.clone()),
            content_type: content_type.clone(),
            size,
            blob_key,
        });
        write_compose(&mut d, &c).map_err(store_err)?;
        store.save_draft(d).await.map_err(store_err)?;
        Ok(json!({ "attachment": { "id": att_id, "name": name, "content_type": content_type, "size": size } }))
    })
}

fn remove_attachment(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p = obj(&p)?;
        let store = store(s)?;
        let id = req_str(p, "draft_id")?;
        let att = req_str(p, "attachment")?;
        let mut d = load_draft(&store, id).await?;
        let mut c = read_compose(&d);
        if c.sending {
            return Err(Error::new(ErrorKind::NotAvailable, "this message is being sent"));
        }
        let Some(pos) = c.attachments.iter().position(|a| a.id == att) else {
            return Err(Error::new(ErrorKind::NotFound, "no such attachment"));
        };
        let removed = c.attachments.remove(pos);
        c.uploaded.retain(|u| *u != removed.id);
        write_compose(&mut d, &c).map_err(store_err)?;
        store.save_draft(d).await.map_err(store_err)?;
        let _ = store.delete_blob(removed.blob_key).await;
        Ok(json!({ "removed": true }))
    })
}

// ---------------------------------------------------------------------------------------------
// Other commands.
// ---------------------------------------------------------------------------------------------

async fn allowed_senders(store: &Store) -> Vec<String> {
    store.setting::<Vec<String>>(ALLOWED_SENDERS_SETTING.to_owned()).await.ok().flatten().unwrap_or_default()
}

fn sender_of(m: &Message) -> Option<String> {
    m.from.as_ref().or(m.sender.as_ref()).and_then(|a| a.address.as_ref()).map(|a| a.as_str().to_ascii_lowercase())
}

fn allow_remote_images(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p = obj(&p)?;
        let store = store(s)?;
        let sender = match opt_str(p, "sender")? {
            Some(a) => MailAddress::new(a.trim())
                .map_err(|_| Error::params("not a valid email address"))?
                .as_str()
                .to_ascii_lowercase(),
            None => {
                let id = one_target(p)?;
                let m = store
                    .message(id)
                    .await
                    .map_err(store_err)?
                    .ok_or_else(|| Error::new(ErrorKind::NotFound, "no such message"))?;
                sender_of(&m).ok_or_else(|| Error::params("this message has no sender address"))?
            }
        };
        let mut list = allowed_senders(&store).await;
        if !list.contains(&sender) {
            list.push(sender.clone());
            let excess = list.len().saturating_sub(MAX_ALLOWED_SENDERS);
            list.drain(..excess);
            store.set_setting(ALLOWED_SENDERS_SETTING.to_owned(), list).await.map_err(store_err)?;
        }
        s.emit(quorum_ipc::CoreEvent::StoreChanged { tables: vec!["message_body".into()] });
        Ok(json!({ "sender": sender }))
    })
}

fn download_attachment(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p = obj(&p)?;
        let id = one_target(p)?;
        let att =
            AttachmentId::new(req_str(p, "attachment")?).map_err(|_| Error::params("not a valid attachment id"))?;
        let store = store(s)?;
        let meta = store
            .attachments(id.clone())
            .await
            .map_err(store_err)?
            .items
            .into_iter()
            .find(|a| a.attachment.id == att)
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "no such attachment"))?;
        let ctx = sync_ctx(s)?;
        let (key, size) = quorum_sync::mail::download_attachment(&ctx, &id, &att).await.map_err(sync_err)?;
        Ok(json!({
            "blob_key": key,
            "size": size,
            "name": clean_name(meta.attachment.name.as_str()),
            "content_type": meta.attachment.content_type,
        }))
    })
}

fn fetch_body(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let id = one_target(obj(&p)?)?;
        let ctx = sync_ctx(s)?;
        quorum_sync::mail::fetch_body(&ctx, &id).await.map_err(sync_err)?;
        Ok(json!({ "fetched": true }))
    })
}

fn date_time_zone(p: &Map<String, Value>, key: &str) -> Result<Option<DateTimeZone>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => serde_json::from_value(v.clone())
            .map(Some)
            .map_err(|_| Error::params(format!("`{key}` must be {{\"dateTime\", \"timeZone\"}}"))),
    }
}

fn set_auto_reply(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p = obj(&p)?;
        let status = match req_str(p, "status")? {
            "disabled" => AutomaticRepliesStatus::Disabled,
            "alwaysEnabled" | "enabled" => AutomaticRepliesStatus::AlwaysEnabled,
            "scheduled" => AutomaticRepliesStatus::Scheduled,
            _ => return Err(Error::params("`status` must be disabled, alwaysEnabled or scheduled")),
        };
        let external_audience = match opt_str(p, "external_audience")? {
            None => None,
            Some("none") => Some(ExternalAudience::None),
            Some("contactsOnly") => Some(ExternalAudience::ContactsOnly),
            Some("all") => Some(ExternalAudience::All),
            Some(_) => return Err(Error::params("`external_audience` must be none, contactsOnly or all")),
        };
        let text = |k: &str| -> Result<Sensitive> {
            let t = opt_str(p, k)?.unwrap_or("");
            if t.len() > 64 * 1024 {
                return Err(Error::params(format!("`{k}` is too long")));
            }
            Ok(Sensitive::new(t))
        };
        let setting = AutomaticRepliesSetting {
            status: status.clone(),
            external_audience,
            scheduled_start_date_time: date_time_zone(p, "start")?,
            scheduled_end_date_time: date_time_zone(p, "end")?,
            internal_reply_message: text("internal_message")?,
            external_reply_message: text("external_message")?,
        };
        if status == AutomaticRepliesStatus::Scheduled
            && (setting.scheduled_start_date_time.is_none() || setting.scheduled_end_date_time.is_none())
        {
            return Err(Error::params("a scheduled reply needs `start` and `end`"));
        }
        let store = store(s)?;
        let payload = AutoReplyPayload { setting: setting.clone() };
        store
            .write(move |tx| {
                let mut current: MailboxSettings = tx.read().setting(MAILBOX_SETTINGS_SETTING)?.unwrap_or_default();
                current.automatic_replies_setting = Some(setting);
                tx.set_setting(MAILBOX_SETTINGS_SETTING, &current)?;
                tx.outbox_enqueue(&to_entry(KIND_AUTO_REPLY, SETTINGS_RESOURCE.to_owned(), &payload)?)?;
                Ok(())
            })
            .await
            .map_err(store_err)?;
        kick(s);
        Ok(json!({ "status": status.as_graph() }))
    })
}

// ---------------------------------------------------------------------------------------------
// Queries.
// ---------------------------------------------------------------------------------------------

fn q_folders(s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let store = store(s)?;
        let folders = store.mail_folders().await.map_err(store_err)?.items;
        let counts = store.unread_counts().await.map_err(store_err)?;
        let roles = roles(&store).await?;
        let role_of = |f: &quorum_model::MailFolder| -> Option<String> {
            roles
                .iter()
                .find(|(_, id)| **id == f.id)
                .map(|(n, _)| n.clone())
                .or_else(|| f.well_known_name.as_ref().map(|w| w.as_graph().to_owned()))
        };
        let visible: Vec<&quorum_model::MailFolder> =
            folders.iter().filter(|f| !f.is_hidden || role_of(f).is_some()).collect();
        let ids: HashSet<&MailFolderId> = visible.iter().map(|f| &f.id).collect();
        let order = ["inbox", "drafts", "sentitems", "deleteditems", "archive", "junkemail", "outbox"];
        let rank = |f: &quorum_model::MailFolder| -> (usize, String) {
            let r = role_of(f);
            let pos = r.as_deref().and_then(|r| order.iter().position(|o| *o == r)).unwrap_or(order.len());
            (pos, f.display_name.as_str().to_lowercase())
        };
        let mut roots: Vec<&quorum_model::MailFolder> =
            visible.iter().copied().filter(|f| f.parent_folder_id.as_ref().is_none_or(|p| !ids.contains(p))).collect();
        roots.sort_by_key(|f| rank(f));
        let mut out = Vec::new();
        let mut stack: Vec<(&quorum_model::MailFolder, usize)> = roots.into_iter().rev().map(|f| (f, 0)).collect();
        let mut emitted = HashSet::new();
        while let Some((f, depth)) = stack.pop() {
            if !emitted.insert(&f.id) || out.len() >= 2000 {
                continue;
            }
            let role = role_of(f);
            let unread = counts.iter().find(|c| c.folder == f.id).map_or(0, |c| c.unread);
            let name = match (f.display_name.is_empty(), role.as_deref()) {
                (true, Some(r)) => kind_of(Some(r)).to_owned(),
                _ => f.display_name.as_str().to_owned(),
            };
            out.push(json!({
                "id": f.id.as_str(),
                "name": name,
                "kind": kind_of(role.as_deref()),
                "role": role,
                "parent_id": f.parent_folder_id.as_ref().map(MailFolderId::as_str),
                "depth": depth,
                "unread": unread,
                "total": f.total_item_count,
                "child_count": f.child_folder_count,
                "favourite": matches!(role.as_deref(), Some("inbox" | "sentitems" | "drafts" | "archive")),
            }));
            if depth < MAX_FOLDER_DEPTH {
                let mut children: Vec<&quorum_model::MailFolder> =
                    visible.iter().copied().filter(|c| c.parent_folder_id.as_ref() == Some(&f.id)).collect();
                children.sort_by_key(|c| rank(c));
                stack.extend(children.into_iter().rev().map(|c| (c, depth.saturating_add(1))));
            }
        }
        Ok(Value::Array(out))
    })
}

fn q_list(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p = if p.is_null() { Map::new() } else { obj(&p)?.clone() };
        let store = store(s)?;
        let folder = resolve_folder(&store, opt_str(&p, "folder")?.unwrap_or("inbox")).await?;
        let limit =
            opt_u64(&p, "limit")?.map_or(DEFAULT_LIST, |n| usize::try_from(n).unwrap_or(MAX_LIST)).clamp(1, MAX_LIST);
        let flagged_only = opt_bool(&p, "flagged_only")?.unwrap_or(false);
        let group = match opt_str(&p, "group")? {
            None | Some("none") => false,
            Some("conversation") => true,
            Some(_) => return Err(Error::params("`group` must be conversation or none")),
        };
        let mut q = MessageQuery::folder(folder.clone());
        q.focused = opt_bool(&p, "focused")?;
        q.unread_only = opt_bool(&p, "unread_only")?.unwrap_or(false);
        q.after = opt_str(&p, "after")?.map(Cursor::from_string);
        q.limit = 500;
        let mut rows: Vec<Value> = Vec::new();
        let mut seen_conv: BTreeMap<String, usize> = BTreeMap::new();
        // Bounded: at most MAX_LIST / 1 rows over pages of 500.
        'pages: for _ in 0..(MAX_LIST / 500 + 2) {
            let page = store.list_messages(q.clone()).await.map_err(|e| match e.kind() {
                quorum_store::ErrorKind::InvalidInput => Error::params("`after` is not a valid cursor"),
                _ => store_err(e),
            })?;
            for m in &page.items {
                if flagged_only && m.flag_status != FlagStatus::Flagged {
                    continue;
                }
                if group && let Some(conv) = &m.conversation_id {
                    if let Some(&i) = seen_conv.get(conv.as_str()) {
                        if let Some(Value::Object(r)) = rows.get_mut(i) {
                            let count = r.get("count").and_then(Value::as_u64).unwrap_or(1).saturating_add(1);
                            r.insert("count".into(), json!(count));
                            if !m.is_read {
                                r.insert("unread".into(), json!(true));
                            }
                            if m.flag_status == FlagStatus::Flagged {
                                r.insert("flagged".into(), json!(true));
                            }
                            if m.has_attachments {
                                r.insert("has_attachments".into(), json!(true));
                            }
                        }
                        continue;
                    }
                    seen_conv.insert(conv.as_str().to_owned(), rows.len());
                }
                rows.push(row(m));
                if rows.len() >= limit {
                    break 'pages;
                }
            }
            match page.next {
                Some(next) => q.after = Some(next),
                None => break,
            }
        }
        if group {
            conversation_summaries(&store, &folder, &mut rows).await?;
        }
        Ok(Value::Array(rows))
    })
}

/// Most grouped rows given conversation-wide counts and participants.
const MAX_SUMMARIES: usize = 300;
/// Most participants named on a grouped row.
const MAX_PARTICIPANTS: usize = 4;

/// For grouped rows: `count` becomes the number of messages in the whole conversation (other
/// folders too, such as your replies in Sent Items; deleted ones only when listing Deleted
/// Items) and `participants` lists who took part, newest first ("me" for your own messages).
async fn conversation_summaries(store: &Store, folder: &MailFolderId, rows: &mut [Value]) -> Result<()> {
    let roles = roles(store).await?;
    let deleted = roles.get("deleteditems");
    let mine: Vec<&MailFolderId> = ["sentitems", "drafts", "outbox"].iter().filter_map(|r| roles.get(*r)).collect();
    let listing_deleted = deleted == Some(folder);
    for r in rows.iter_mut().take(MAX_SUMMARIES) {
        let Some(conv) = r.get("conversation_id").and_then(Value::as_str).and_then(|c| ConversationId::new(c).ok())
        else {
            continue;
        };
        let messages = store.conversation(conv).await.map_err(store_err)?.items;
        let live: Vec<&Message> = messages
            .iter()
            .filter(|m| listing_deleted || deleted.is_none() || m.parent_folder_id.as_ref() != deleted)
            .collect();
        let mut names: Vec<String> = Vec::new();
        for m in live.iter().rev() {
            let me = m.is_draft || m.parent_folder_id.as_ref().is_some_and(|f| mine.contains(&f));
            let name = if me {
                "me".to_owned()
            } else {
                let from = m.from.as_ref().or(m.sender.as_ref());
                let display = from.map(EmailAddress::display).unwrap_or("");
                display.split_whitespace().next().unwrap_or(display).to_owned()
            };
            if !name.is_empty() && !names.contains(&name) {
                names.push(name);
            }
            if names.len() >= MAX_PARTICIPANTS {
                break;
            }
        }
        if let Value::Object(o) = r {
            let current = o.get("count").and_then(Value::as_u64).unwrap_or(1);
            o.insert("count".into(), json!(current.max(u64::try_from(live.len()).unwrap_or(u64::MAX))));
            o.insert("participants".into(), json!(names));
        }
    }
    Ok(())
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

struct Rendered {
    html: String,
    remote_blocked: bool,
    /// The body without the quoted reply history, when there is history to hide.
    new_html: Option<String>,
}

/// Sanitises a stored body for display. Text bodies are escaped; HTML goes through
/// `quorum-sanitize` with inline images mapped to `quorum-cid:<message>/<attachment>`.
fn render(
    id: &MessageId,
    body: &quorum_model::MessageBody,
    atts: &[quorum_store::StoredAttachment],
    allow: bool,
) -> Rendered {
    match body.content_type {
        BodyType::Html => {
            let mut opts = quorum_sanitize::MailOptions::default().with_remote_images(allow);
            for a in atts {
                if let Some(cid) = &a.attachment.content_id {
                    opts = opts.with_cid(cid.clone(), format!("{}/{}", id.as_str(), a.attachment.id.as_str()));
                }
            }
            let raw = body.content.as_str();
            let out = quorum_sanitize::sanitize_mail(raw, &opts);
            let new_html = quorum_sanitize::quote_start(raw)
                .and_then(|at| raw.get(..at))
                .map(|head| quorum_sanitize::sanitize_mail(head, &opts).html)
                .filter(|h| !h.trim().is_empty());
            Rendered { remote_blocked: out.had_remote_images && !allow, html: out.html, new_html }
        }
        _ => {
            let text: String = body.content.as_str().chars().take(quorum_sanitize::MAX_INPUT_BYTES).collect();
            let pre = |t: &str| format!("<div style=\"white-space: pre-wrap\">{}</div>", escape(t));
            let new_html = quorum_sanitize::quote_start_text(&text).and_then(|at| text.get(..at)).map(pre);
            Rendered { html: pre(&text), remote_blocked: false, new_html }
        }
    }
}

fn in_flight() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Starts fetching a body in the background (once per message at a time).
fn fetch_in_background(s: &Session, id: &MessageId) -> bool {
    let Ok(ctx) = sync_ctx(s) else { return false };
    let key = id.as_str().to_owned();
    let started = in_flight().lock().map(|mut set| set.len() < 64 && set.insert(key.clone())).unwrap_or(false);
    if !started {
        return true;
    }
    let id = id.clone();
    tokio::spawn(async move {
        let r = tokio::time::timeout(Duration::from_secs(60), quorum_sync::mail::fetch_body(&ctx, &id)).await;
        if !matches!(r, Ok(Ok(()))) {
            tracing::debug!(name: "mail.body_fetch_failed", "body fetch failed");
        }
        if let Ok(mut set) = in_flight().lock() {
            set.remove(&key);
        }
    });
    true
}

async fn remote_allowed(s: &Session, store: &Store, m: &Message, explicit: bool) -> bool {
    if explicit {
        return true;
    }
    if super::settings::get(s, "mail.remote_images").await.ok().and_then(|v| v.as_bool()).unwrap_or(false) {
        return true;
    }
    match sender_of(m) {
        Some(a) => allowed_senders(store).await.contains(&a),
        None => false,
    }
}

fn attachment_json(a: &quorum_store::StoredAttachment) -> Value {
    json!({
        "id": a.attachment.id.as_str(),
        "name": clean_name(a.attachment.name.as_str()),
        "size": a.attachment.size,
        "content_type": a.attachment.content_type,
        "kind": a.attachment.kind.as_graph(),
        "is_inline": a.attachment.is_inline,
        "downloaded": a.blob_key.is_some(),
    })
}

async fn message_json(s: &Session, store: &Store, m: &Message, allow_param: bool, fetch: bool) -> Result<Value> {
    let body = store.message_body(m.id.clone()).await.map_err(store_err)?;
    let atts = store.attachments(m.id.clone()).await.map_err(store_err)?.items;
    let allow = remote_allowed(s, store, m, allow_param).await;
    let mut v = row(m);
    let (html, new_html, blocked, loading) = match &body {
        Some(b) => {
            let r = render(&m.id, b, &atts, allow);
            let colours = r.new_html.as_deref().map_or_else(|| has_own_background(&r.html), has_own_background);
            (Value::String(r.html), (r.new_html, colours), r.remote_blocked, false)
        }
        None => (Value::Null, (None, false), false, fetch && fetch_in_background(s, &m.id)),
    };
    if let Value::Object(o) = &mut v {
        let from = m.from.as_ref().or(m.sender.as_ref());
        o.insert("from_address".into(), from.map_or(Value::Null, address_json));
        o.insert("to_recipients".into(), addresses_json(&m.to_recipients));
        o.insert("cc_recipients".into(), addresses_json(&m.cc_recipients));
        o.insert("bcc_recipients".into(), addresses_json(&m.bcc_recipients));
        o.insert("reply_to".into(), addresses_json(&m.reply_to));
        o.insert("sent".into(), rfc3339(m.sent_date_time));
        o.insert("web_link".into(), json!(m.web_link));
        o.insert("body_html".into(), html);
        o.insert("has_quoted".into(), json!(new_html.0.is_some()));
        o.insert("body_new_html".into(), new_html.0.map_or(Value::Null, Value::String));
        o.insert("own_background".into(), json!(new_html.1));
        o.insert("body_loading".into(), json!(loading));
        o.insert("remote_images_blocked".into(), json!(blocked));
        o.insert("remote_images_allowed".into(), json!(allow));
        o.insert(
            "attachments".into(),
            Value::Array(atts.iter().filter(|a| !a.attachment.is_inline).map(attachment_json).collect()),
        );
    }
    Ok(v)
}

/// Rewrites the bodies' `quorum-cid:`/`quorum-remote:` URLs to the form this platform's webview
/// loads, for a UI that shows them in a `srcdoc` frame.
fn to_webview_urls(v: &mut Value) {
    let platform = if cfg!(windows) {
        quorum_sanitize::WebviewPlatform::Windows
    } else {
        quorum_sanitize::WebviewPlatform::MacOsLinux
    };
    for key in ["body_html", "body_new_html"] {
        if let Some(Value::String(html)) = v.get_mut(key) {
            *html = quorum_sanitize::webview_urls(html, platform);
        }
    }
}

/// Whether sanitised mail paints its own backgrounds (a designed newsletter): such mail is shown
/// on a light page even in dark mode instead of being colour-inverted.
fn has_own_background(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    lower.contains("background") || lower.contains("bgcolor")
}

fn q_message(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p = obj(&p)?;
        let id = one_target(p)?;
        let allow = opt_bool(p, "allow_remote_images")?.or(opt_bool(p, "remote_images")?).unwrap_or(false);
        let store = store(s)?;
        let m = store
            .message(id)
            .await
            .map_err(store_err)?
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "no such message"))?;
        message_json(s, &store, &m, allow, true).await
    })
}

fn q_conversation(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p = obj(&p)?;
        let store = store(s)?;
        let conv = match (opt_str(p, "id")?, opt_str(p, "message")?) {
            (_, Some(mid)) => {
                let m = store
                    .message(message_id(mid)?)
                    .await
                    .map_err(store_err)?
                    .ok_or_else(|| Error::new(ErrorKind::NotFound, "no such message"))?;
                match m.conversation_id {
                    Some(c) => c,
                    // Not threaded: a conversation of one.
                    None => {
                        let mut v =
                            message_json(s, &store, &m, opt_bool(p, "allow_remote_images")?.unwrap_or(false), true)
                                .await?;
                        if opt_bool(p, "webview")?.unwrap_or(false) {
                            to_webview_urls(&mut v);
                        }
                        return Ok(Value::Array(vec![v]));
                    }
                }
            }
            (Some(cid), None) => ConversationId::new(cid).map_err(|_| Error::params("not a valid conversation id"))?,
            (None, None) => return Err(Error::params("`id` (conversation) or `message` is required")),
        };
        let allow = opt_bool(p, "allow_remote_images")?.unwrap_or(false);
        // Per-message "Show pictures" in the thread view.
        let allow_ids: HashSet<String> = match p.get("allow_remote_ids") {
            Some(Value::Array(ids)) => ids.iter().filter_map(Value::as_str).take(500).map(str::to_owned).collect(),
            None | Some(Value::Null) => HashSet::new(),
            Some(_) => return Err(Error::params("`allow_remote_ids` must be a list of message ids")),
        };
        let newest_first = match opt_str(p, "order")? {
            None | Some("oldest_first") => false,
            Some("newest_first") => true,
            Some(_) => return Err(Error::params("`order` must be oldest_first or newest_first")),
        };
        let webview = opt_bool(p, "webview")?.unwrap_or(false);
        let messages = store.conversation(conv).await.map_err(store_err)?.items;
        let roles = roles(&store).await?;
        let kind_of_folder = |f: Option<&MailFolderId>| -> &'static str {
            let role = f.and_then(|f| roles.iter().find(|(_, id)| id == &f)).map(|(r, _)| r.as_str());
            kind_of(role)
        };
        // Bodies for the newest messages only; older ones come back as rows (`body_html` absent).
        let skip = messages.len().saturating_sub(MAX_CONVERSATION_BODIES);
        let mut out = Vec::with_capacity(messages.len());
        for (i, m) in messages.iter().enumerate() {
            let mut v = if i < skip {
                row(m)
            } else {
                let allow_this = allow || allow_ids.contains(m.id.as_str());
                message_json(s, &store, m, allow_this, true).await?
            };
            let kind = kind_of_folder(m.parent_folder_id.as_ref());
            if webview {
                to_webview_urls(&mut v);
            }
            if let Value::Object(o) = &mut v {
                o.insert("folder_kind".into(), json!(kind));
                o.insert("from_me".into(), json!(matches!(kind, "sent" | "drafts" | "outbox") || m.is_draft));
            }
            out.push(v);
        }
        if newest_first {
            out.reverse();
        }
        Ok(Value::Array(out))
    })
}

/// Draft HTML for the compose editor: sanitised (it is shown in the main window), links kept
/// as `quorum-link:` (restored by `mail.save_draft`).
fn editor_html(html: &str) -> String {
    if html.is_empty() {
        return String::new();
    }
    quorum_sanitize::sanitize_mail(html, &quorum_sanitize::MailOptions::default()).html
}

fn q_draft(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p = obj(&p)?;
        let store = store(s)?;
        let d = load_draft(&store, req_str(p, "draft_id")?).await?;
        let c = read_compose(&d);
        let quote = match (&c.quote_html, &c.source) {
            (Some(q), _) if !q.is_empty() => Some(editor_html(q.as_str())),
            (_, Some(src)) => match store.message_body(src.clone()).await.map_err(store_err)? {
                Some(b) => {
                    let original = store.message(src.clone()).await.map_err(store_err)?;
                    let header = original.map(|m| {
                        let from = m.from.as_ref().map(EmailAddress::display).unwrap_or("");
                        format!(
                            "<p><b>From:</b> {}<br><b>Sent:</b> {}<br><b>Subject:</b> {}</p>",
                            escape(from),
                            m.received_date_time.map(|t| t.to_string()).unwrap_or_default(),
                            escape(m.subject.as_str())
                        )
                    });
                    Some(format!("{}{}", header.unwrap_or_default(), render(src, &b, &[], false).html))
                }
                None => None,
            },
            _ => None,
        };
        let mode = serde_json::to_value(c.mode).unwrap_or(Value::Null);
        Ok(json!({
            "draft_id": d.id,
            "mode": mode,
            "source": c.source.as_ref().map(MessageId::as_str),
            "to": addresses_json(&c.to),
            "cc": addresses_json(&c.cc),
            "bcc": addresses_json(&c.bcc),
            "subject": c.subject.as_str(),
            "importance": c.importance.as_ref().map(GraphEnum::as_graph),
            "request_read_receipt": c.request_read_receipt,
            "body_html": editor_html(d.body.as_str()),
            "quote_html": quote,
            "attachments": c.attachments.iter().map(|a| json!({
                "id": a.id, "name": a.name.as_str(), "size": a.size, "content_type": a.content_type,
            })).collect::<Vec<_>>(),
            "sending": c.sending,
            "send_at": rfc3339(c.send_at),
            "synced": c.server_id.is_some(),
            "updated_at": d.updated_at.to_string(),
        }))
    })
}

fn q_drafts(s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let store = store(s)?;
        let page = store.drafts(Some(DraftKind::Mail), 200).await.map_err(store_err)?;
        let rows: Vec<Value> = page
            .items
            .iter()
            .map(|d| {
                let c = read_compose(d);
                json!({
                    "draft_id": d.id,
                    "mode": serde_json::to_value(c.mode).unwrap_or(Value::Null),
                    "subject": c.subject.as_str(),
                    "to": c.to.iter().map(EmailAddress::display).collect::<Vec<_>>().join(", "),
                    "sending": c.sending,
                    "send_at": rfc3339(c.send_at),
                    "updated_at": d.updated_at.to_string(),
                })
            })
            .collect();
        Ok(Value::Array(rows))
    })
}

fn q_search(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p = obj(&p)?;
        let query: String = req_str(p, "query")?.chars().take(MAX_QUERY_CHARS).collect();
        let limit = opt_u64(p, "limit")?.map_or(50, |n| n.clamp(1, 200));
        let limit32 = u32::try_from(limit).unwrap_or(50);
        let remote = opt_bool(p, "remote")?.unwrap_or(true);
        let store = store(s)?;
        let folder = match opt_str(p, "folder")? {
            Some(f) => Some(resolve_folder(&store, f).await?),
            None => None,
        };
        let hits =
            store.search(query.clone(), vec![quorum_store::SearchScope::Mail], limit32).await.map_err(store_err)?;
        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        for hit in hits.hits {
            let quorum_store::HitItem::Mail { id, folder: hit_folder } = hit.item else { continue };
            if folder.as_ref().is_some_and(|f| hit_folder.as_ref() != Some(f)) {
                continue;
            }
            if let Some(m) = store.message(id.clone()).await.map_err(store_err)? {
                let mut r = row(&m);
                if let Value::Object(o) = &mut r {
                    o.insert("snippet".into(), json!(hit.snippet.text.as_str()));
                    o.insert("remote".into(), json!(false));
                }
                seen.insert(id);
                rows.push(r);
            }
        }
        if remote && rows.len() < 5 && folder.is_none() && s.sync().is_ok() {
            let q = quorum_graph::api::search::SearchQuery::new(
                vec![quorum_graph::api::search::EntityType::Message],
                query,
            );
            let found = tokio::time::timeout(super::people::REMOTE_TIMEOUT, s.graph().search(&q)).await;
            if let Ok(Ok(page)) = found {
                for result in page.results {
                    if let quorum_graph::api::search::SearchHit::Message(m) = result.hit
                        && seen.insert(m.id.clone())
                    {
                        let mut r = row(&m);
                        if let Value::Object(o) = &mut r {
                            o.insert(
                                "snippet".into(),
                                json!(result.summary.as_str().replace("<c0>", "").replace("</c0>", "")),
                            );
                            o.insert("remote".into(), json!(true));
                        }
                        rows.push(r);
                    }
                }
            }
        }
        rows.truncate(usize::try_from(limit).unwrap_or(50));
        Ok(Value::Array(rows))
    })
}

fn q_categories(s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let store = store(s)?;
        let cats: Vec<quorum_graph::api::mail::Category> =
            store.setting(CATEGORIES_SETTING.to_owned()).await.map_err(store_err)?.unwrap_or_default();
        Ok(Value::Array(cats.iter().map(|c| json!({ "name": c.display_name, "color": c.color })).collect()))
    })
}

fn q_auto_reply(s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let store = store(s)?;
        let settings: Option<MailboxSettings> =
            store.setting(MAILBOX_SETTINGS_SETTING.to_owned()).await.map_err(store_err)?;
        Ok(settings
            .and_then(|s| s.automatic_replies_setting)
            .map_or(Value::Null, |a| serde_json::to_value(a).unwrap_or(Value::Null)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quorum_links_are_restored_for_sending() {
        let html = r#"<a href="quorum-link:https%3A%2F%2Fexample.com%2F">x</a> quorum-nothing"#;
        assert_eq!(restore_urls(html), r#"<a href="https://example.com/">x</a> quorum-nothing"#);
        assert_eq!(restore_urls("quorum-"), "quorum-");
        assert_eq!(restore_urls("é quorum-link:%ZZ"), "é quorum-link:%ZZ");
    }

    #[test]
    fn addresses_parse_leniently_but_safely() {
        let a = parse_address("Avery Example <avery@example.com>").unwrap();
        assert_eq!(a.name.as_str(), "Avery Example");
        assert!(parse_address("not an address").is_err());
        assert!(parse_address("<>").is_err());
        assert!(parse_address("<").is_err());
    }

    #[test]
    fn attachment_names_never_escape() {
        assert_eq!(clean_name("../../etc/passwd"), "passwd");
        assert_eq!(clean_name("..\\x"), "x");
        assert_eq!(clean_name(""), "attachment");
        assert_eq!(clean_name(".hidden"), "hidden");
    }
}
