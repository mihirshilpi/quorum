//! Teams chat commands and queries.
//!
//! Writes go through the outbox (`quorum_sync::teams::outbox`): each command validates its
//! params, then enqueues the entry and applies the optimistic local change (a pending message
//! with a `local-` id, an edited body, a reaction...) in one store transaction, and wakes the
//! outbox. Reads come from the store only.
//!
//! Message HTML is untrusted: queries return it through `quorum_sanitize::sanitize_chat` with
//! hosted images rewritten for the webview. Outgoing HTML (from the composer, the CLI or MCP)
//! goes through the same sanitiser, with links restored to their targets.

use std::collections::{HashMap, HashSet};

use base64::Engine as _;
use jiff::Timestamp;
use quorum_ipc::CoreEvent;
use quorum_model::{
    BodyType, Chat, ChatAttachment, ChatBody, ChatId, ChatImportance, ChatMessage, ChatMessageId, ChatMessageType,
    ChatType, GraphEnum, Identity, IdentityKind, Mention, MessageContainer, Sensitive, UserId,
};
use quorum_store::{Cursor, Draft, DraftKind, NewOutboxEntry, SearchScope, Tx};
use quorum_sync::teams::payload::{
    CreatePayload, DeletePayload, EditPayload, FilePayload, ImagePayload, MemberPayload, MentionKind, MentionPayload,
    PinPayload, QuotePayload, ReactPayload, ReadPayload, SendPayload, Target,
};
use quorum_sync::teams::{OPEN_CHAT_SETTING, PINNED_CHATS_PREF, chat_key, older_request_key};
use serde::Deserialize;
use serde_json::{Value, json};

use super::people::{presence_fields, presences_of, store_err};
use crate::command::{CommandFuture, CommandSpec, always, parse};
use crate::error::{Error, ErrorKind, Result};
use crate::session::Session;

/// Largest inline image accepted from the composer (Graph's hosted content limit).
pub(crate) const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
/// Largest file attachment accepted through params (base64 over IPC).
pub(crate) const MAX_FILE_BYTES: usize = 25 * 1024 * 1024;
/// Most images or files per message.
pub(crate) const MAX_ATTACHMENTS: usize = 10;
/// Largest outgoing HTML/text accepted.
pub(crate) const MAX_OUTGOING: usize = 200 * 1024;
/// Most mentions per message.
const MAX_MENTIONS: usize = 50;
/// Mock-mode identity (matches `fixtures/graph/me.json`).
const MOCK_USER: &str = "00000000-0000-0000-0000-000000000001";
const MOCK_TENANT: &str = "00000000-0000-0000-0000-0000000000f1";

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
            "chat.open",
            "Open Chat",
            None,
            None,
            r#"{"id": chat id, "message"?: message id, "mark_read"?: bool (default true)}"#,
            false,
            open,
        ),
        spec(
            "chat.new",
            "New Chat",
            Some("Chat/New Chat"),
            Some("Cmd+N"),
            r#"{"members": [user id or UPN], "emails"?: [address], "topic"?: string, "message"?: string (html)} -> {"id", "existing"}"#,
            false,
            new_chat,
        ),
        spec(
            "chat.send",
            "Send Chat Message",
            Some("Chat/Send"),
            None,
            r#"{"chat"|"id": chat id, "html"?: string, "text"?: string, "mentions"?: [{"id": n, "text": string, "user": user id}], "images"?: [{"content_type": "image/png", "data": base64}], "files"?: [{"name": string, "content_type"?: string, "data": base64}], "quote"?: message id, "importance"?: "normal"|"high"|"urgent"} -> {"local_id"}"#,
            true,
            send,
        ),
        spec(
            "chat.edit",
            "Edit Message",
            Some("Chat/Edit Message"),
            None,
            r#"{"chat": chat id, "message": message id, "html"?: string, "text"?: string, "mentions"?: [...]}"#,
            false,
            edit,
        ),
        spec(
            "chat.delete",
            "Delete Message",
            Some("Chat/Delete Message"),
            None,
            r#"{"chat": chat id, "message": message id}"#,
            true,
            delete,
        ),
        spec(
            "chat.react",
            "React to Message",
            None,
            None,
            r#"{"chat": chat id, "message": message id, "reaction": emoji}"#,
            false,
            react,
        ),
        spec(
            "chat.unreact",
            "Remove Reaction",
            None,
            None,
            r#"{"chat": chat id, "message": message id, "reaction": emoji}"#,
            false,
            unreact,
        ),
        spec(
            "chat.mark_read",
            "Mark Chat Read",
            Some("Chat/Mark as Read"),
            Some("Cmd+Shift+R"),
            r#"{"id"|"chat": chat id}"#,
            false,
            mark_read,
        ),
        spec(
            "chat.mark_unread",
            "Mark Chat Unread",
            Some("Chat/Mark as Unread"),
            Some("Cmd+Shift+U"),
            r#"{"id"|"chat": chat id}"#,
            false,
            mark_unread,
        ),
        spec("chat.load_older", "Load Older Messages", None, None, r#"{"id"|"chat": chat id}"#, false, load_older),
        spec(
            "chat.add_member",
            "Add People to Chat",
            Some("Chat/Add People…"),
            None,
            r#"{"chat": chat id, "user": user id or UPN, "share_history"?: bool}"#,
            false,
            add_member,
        ),
        spec(
            "chat.remove_member",
            "Remove from Chat",
            None,
            None,
            r#"{"chat": chat id, "user": user id or UPN}"#,
            true,
            remove_member,
        ),
        spec(
            "chat.pin",
            "Pin Chat",
            Some("Chat/Pin"),
            None,
            r#"{"id"|"chat": chat id} (pins to the top of the chat list; local)"#,
            false,
            pin_chat,
        ),
        spec("chat.unpin", "Unpin Chat", Some("Chat/Unpin"), None, r#"{"id"|"chat": chat id}"#, false, unpin_chat),
        spec(
            "chat.pin_message",
            "Pin Message",
            None,
            None,
            r#"{"chat": chat id, "message": message id}"#,
            false,
            pin_message,
        ),
        spec(
            "chat.unpin_message",
            "Unpin Message",
            None,
            None,
            r#"{"chat": chat id, "message": message id}"#,
            false,
            unpin_message,
        ),
        spec(
            "chat.save_draft",
            "Save Chat Draft",
            None,
            None,
            r#"{"chat": chat id, "html": string, "mentions"?: [...]} (empty html deletes the draft)"#,
            false,
            save_draft,
        ),
    ]
}

pub(crate) fn queries() -> Vec<CommandSpec> {
    vec![
        spec(
            "chat.list",
            "Chats",
            None,
            None,
            r#"{"limit"?: 1..500 (default 200), "filter"?: "all"|"unread"|"pinned"|"group"|"meeting"|"hidden", "query"?: string} -> [chat row]"#,
            false,
            list,
        ),
        spec(
            "chat.messages",
            "Chat Messages",
            None,
            None,
            r#"{"chat"|"id": chat id, "before"?: cursor, "limit"?: 1..200 (default 50)} -> {"items": [message, oldest first], "next": cursor|null, "has_more": bool, "me": user id}"#,
            false,
            messages,
        ),
        spec(
            "chat.search",
            "Search Chats and Channels",
            None,
            None,
            r#"{"query": string, "limit"?: 1..100 (default 30)} -> [hit]"#,
            false,
            search,
        ),
        spec(
            "chat.draft",
            "Chat Draft",
            None,
            None,
            r#"{"chat"|"id": chat id} -> {"html", "mentions", "updated_at"} | null"#,
            false,
            draft,
        ),
    ]
}

// ---------------------------------------------------------------------------------------------
// Shared helpers (also used by `channels`).
// ---------------------------------------------------------------------------------------------

/// The signed-in user's id and tenant (mock mode: the fixture identity).
pub(crate) async fn identity(s: &Session) -> (Option<UserId>, Option<String>) {
    if s.accounts().is_mock() {
        return (UserId::new(MOCK_USER).ok(), Some(MOCK_TENANT.to_owned()));
    }
    let mut user = None;
    let mut tenant = None;
    if let Some(p) = s.accounts().provider().await {
        let a = p.account();
        user = a.id().split('.').next().and_then(|oid| UserId::new(oid).ok());
        tenant = Some(a.tenant_id().to_owned()).filter(|t| !t.is_empty());
    }
    if user.is_none()
        && let Ok(store) = s.store()
        && let Ok(Some(id)) = store.setting::<String>(quorum_sync::people::ME_ID_SETTING.to_owned()).await
    {
        user = UserId::new(id).ok();
    }
    (user, tenant)
}

/// A fresh id for an optimistic local item: `local-<prefix>-<nanos>-<n>`.
pub(crate) fn local_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    format!("local-{prefix}-{}-{n}", Timestamp::now().as_nanosecond())
}

/// Whether a message id is an optimistic local one.
pub(crate) fn is_local(id: &str) -> bool {
    id.starts_with("local-")
}

pub(crate) fn platform() -> quorum_sanitize::WebviewPlatform {
    if cfg!(target_os = "windows") {
        quorum_sanitize::WebviewPlatform::Windows
    } else {
        quorum_sanitize::WebviewPlatform::MacOsLinux
    }
}

pub(crate) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Plain text to HTML paragraphs (newlines become `<br>`).
pub(crate) fn text_to_html(text: &str) -> String {
    let lines: Vec<String> = text.trim_end().split('\n').map(|l| escape(l.trim_end_matches('\r'))).collect();
    format!("<p>{}</p>", lines.join("<br>"))
}

/// Restores `quorum-link:` hrefs written by the sanitiser to their real targets (outgoing HTML
/// must carry normal links).
fn restore_links(html: &str) -> String {
    let marker = "href=\"quorum-link:";
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(pos) = rest.find(marker) {
        let (before, at) = rest.split_at(pos);
        out.push_str(before);
        let value_start = "href=\"".len();
        let Some(tail) = at.get(value_start..) else { break };
        let Some(end) = tail.find('"') else {
            out.push_str(at);
            rest = "";
            break;
        };
        let (url, after) = tail.split_at(end);
        let target = quorum_sanitize::QuorumUrl::parse(url).map(|u| u.target().to_owned()).unwrap_or_default();
        out.push_str("href=\"");
        out.push_str(&escape(&target));
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Outgoing body: sanitised HTML, or plain text turned into HTML. Rejects empty bodies.
pub(crate) fn outgoing_html(html: Option<&str>, text: Option<&str>) -> Result<String> {
    let raw_len = html.map_or(0, str::len).max(text.map_or(0, str::len));
    if raw_len > MAX_OUTGOING {
        return Err(Error::params("the message is too long"));
    }
    let out = match (html.filter(|h| !h.trim().is_empty()), text.filter(|t| !t.trim().is_empty())) {
        (Some(h), _) => restore_links(&quorum_sanitize::sanitize_chat(h).html),
        (None, Some(t)) => text_to_html(t),
        (None, None) => String::new(),
    };
    Ok(out)
}

/// Whether HTML has visible content (text, a mention, or an image).
pub(crate) fn has_content(html: &str) -> bool {
    html.contains("<img") || html.contains("<at ") || !quorum_sanitize::to_plain_text(html).trim().is_empty()
}

/// A mention as sent by callers.
#[derive(Deserialize, Clone)]
pub(crate) struct MentionParam {
    id: u32,
    #[serde(default)]
    text: String,
    #[serde(default, alias = "user_id")]
    user: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    target: Option<String>,
}

pub(crate) fn mentions(html: &str, list: &[MentionParam]) -> Result<Vec<MentionPayload>> {
    if list.len() > MAX_MENTIONS {
        return Err(Error::params("too many mentions"));
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for m in list {
        if !seen.insert(m.id) {
            return Err(Error::params("mention ids must be unique"));
        }
        if !html.contains(&format!("<at id=\"{}\">", m.id)) {
            return Err(Error::params("each mention needs <at id=\"N\">name</at> in the body"));
        }
        let kind = match m.kind.as_deref().unwrap_or("user") {
            "user" => MentionKind::User,
            "channel" => MentionKind::Channel,
            "team" => MentionKind::Team,
            "tag" => MentionKind::Tag,
            _ => return Err(Error::params("mention kind must be user, channel, team or tag")),
        };
        let target = m.target.clone().or_else(|| m.user.clone()).unwrap_or_default();
        if target.is_empty() || target.len() > quorum_model::MAX_ID_LEN || target.chars().any(char::is_control) {
            return Err(Error::params("each mention needs the mentioned user id"));
        }
        let text: String = m.text.chars().filter(|c| !c.is_control()).take(256).collect();
        out.push(MentionPayload { id: m.id, text, kind, target });
    }
    Ok(out)
}

fn model_mentions(list: &[MentionPayload]) -> Vec<Mention> {
    list.iter()
        .map(|m| Mention {
            id: m.id,
            mention_text: Sensitive::new(m.text.clone()),
            mentioned: Some(Identity {
                kind: IdentityKind::User,
                id: Some(m.target.clone()),
                display_name: Sensitive::new(m.text.clone()),
            }),
        })
        .collect()
}

/// An attachment as sent by callers (base64 bytes).
#[derive(Deserialize, Clone)]
pub(crate) struct DataParam {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    content_type: Option<String>,
    data: String,
}

fn decode(data: &str, max: usize) -> Result<Vec<u8>> {
    let clean = data.split_once(',').filter(|(h, _)| h.starts_with("data:")).map_or(data, |(_, b)| b);
    // Base64 is 4/3 the size; reject before decoding huge input.
    if clean.len() / 4 * 3 > max.saturating_add(3) {
        return Err(Error::params("attachment is too large"));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(clean.trim())
        .map_err(|_| Error::params("attachment data must be base64"))?;
    if bytes.is_empty() || bytes.len() > max {
        return Err(Error::params("attachment is empty or too large"));
    }
    Ok(bytes)
}

fn safe_name(name: &str) -> Result<String> {
    let n: String = name.chars().filter(|c| !c.is_control() && !"/\\:*?\"<>|".contains(*c)).collect();
    let n = n.trim().trim_matches('.').to_owned();
    if n.is_empty() || n.len() > 200 {
        return Err(Error::params("file name must be 1 to 200 characters"));
    }
    Ok(n)
}

/// Prepared attachments: payload entries, the extra HTML for images, and blobs to store.
pub(crate) struct Prepared {
    pub(crate) images: Vec<ImagePayload>,
    pub(crate) files: Vec<FilePayload>,
    pub(crate) image_html: String,
    pub(crate) blobs: Vec<(String, Vec<u8>)>,
}

pub(crate) fn prepare_attachments(local: &str, images: &[DataParam], files: &[DataParam]) -> Result<Prepared> {
    if images.len() > MAX_ATTACHMENTS || files.len() > MAX_ATTACHMENTS {
        return Err(Error::params("too many attachments"));
    }
    let mut p = Prepared { images: Vec::new(), files: Vec::new(), image_html: String::new(), blobs: Vec::new() };
    for (i, img) in images.iter().enumerate() {
        let ct = img.content_type.clone().unwrap_or_else(|| "image/png".into());
        if !matches!(ct.as_str(), "image/png" | "image/jpeg" | "image/gif" | "image/webp") {
            return Err(Error::params("images must be PNG, JPEG, GIF or WebP"));
        }
        let bytes = decode(&img.data, MAX_IMAGE_BYTES)?;
        let temp = format!("{}", i.saturating_add(1));
        let key = format!("chat-out:{local}:img{temp}");
        p.image_html.push_str(&format!("<img src=\"../hostedContents/{temp}/$value\">"));
        p.images.push(ImagePayload { temp_id: temp, content_type: ct, blob: key.clone() });
        p.blobs.push((key, bytes));
    }
    for (i, f) in files.iter().enumerate() {
        let name = safe_name(f.name.as_deref().unwrap_or(""))?;
        let bytes = decode(&f.data, MAX_FILE_BYTES)?;
        let key = format!("chat-out:{local}:file{i}");
        let ct =
            f.content_type.clone().filter(|c| c.len() <= 128 && !c.chars().any(char::is_control)).unwrap_or_default();
        p.files.push(FilePayload { name, content_type: ct, blob: key.clone() });
        p.blobs.push((key, bytes));
    }
    Ok(p)
}

/// Stores attachment bytes in the blob cache (before the outbox entry referencing them).
pub(crate) async fn store_blobs(s: &Session, blobs: Vec<(String, Vec<u8>)>) -> Result<()> {
    let store = s.store()?;
    for (k, b) in blobs {
        store.put_blob(k, b).await.map_err(store_err)?;
    }
    Ok(())
}

/// The optimistic message for a send.
pub(crate) fn pending_message(
    id: ChatMessageId,
    container: MessageContainer,
    reply_to: Option<ChatMessageId>,
    me: Option<&UserId>,
    html: String,
    subject: Option<String>,
    mentions: &[MentionPayload],
    files: &[FilePayload],
    importance: ChatImportance,
) -> ChatMessage {
    let now = Timestamp::now();
    ChatMessage {
        id,
        container,
        reply_to_id: reply_to,
        from: Some(Identity {
            kind: IdentityKind::User,
            id: me.map(|u| u.as_str().to_owned()),
            display_name: Sensitive::new("You"),
        }),
        created_date_time: Some(now),
        last_modified_date_time: Some(now),
        last_edited_date_time: None,
        deleted_date_time: None,
        subject: subject.map(Sensitive::new),
        body: ChatBody { content_type: BodyType::Html, content: Sensitive::new(html) },
        mentions: model_mentions(mentions),
        reactions: Vec::new(),
        attachments: files
            .iter()
            .map(|f| ChatAttachment {
                id: None,
                content_type: "reference".into(),
                content_url: None,
                name: Some(Sensitive::new(f.name.clone())),
                content: None,
                thumbnail_url: None,
            })
            .collect(),
        importance,
        message_type: ChatMessageType::Message,
        web_url: None,
    }
}

/// Enqueues an outbox entry and applies `local` in the same transaction, then wakes the outbox.
pub(crate) async fn enqueue<F>(
    s: &Session,
    kind: &str,
    resource: String,
    local_id: Option<String>,
    payload: Value,
    local: F,
) -> Result<()>
where
    F: FnOnce(&mut Tx<'_>) -> quorum_store::Result<()> + Send + 'static,
{
    let store = s.store()?;
    let entry = NewOutboxEntry { kind: kind.to_owned(), resource, local_id, payload };
    store
        .write(move |tx| {
            local(tx)?;
            tx.outbox_enqueue(&entry)?;
            Ok(())
        })
        .await
        .map_err(store_err)?;
    if let Ok(sync) = s.sync() {
        sync.outbox_changed();
    }
    Ok(())
}

fn to_payload<T: serde::Serialize>(p: &T) -> Result<Value> {
    serde_json::to_value(p).map_err(|e| Error::new(ErrorKind::Bug, "payload encode failed").with_source(e))
}

pub(crate) fn chat_id(s: Option<String>) -> Result<ChatId> {
    ChatId::new(s.unwrap_or_default()).map_err(|_| Error::params("`chat` (or `id`) must be a chat id"))
}

pub(crate) fn message_id(s: &str) -> Result<ChatMessageId> {
    ChatMessageId::new(s).map_err(|_| Error::params("`message` must be a message id"))
}

/// Reactions folded for display: legacy names mapped to emoji, counted, with the user's own.
pub(crate) fn reaction_summary(m: &ChatMessage, me: Option<&UserId>) -> Vec<Value> {
    let mut order: Vec<String> = Vec::new();
    let mut map: HashMap<String, (u32, bool, Vec<String>)> = HashMap::new();
    for r in m.reactions.iter().take(500) {
        let emoji = emoji_for(&r.reaction_type);
        if emoji.is_empty() {
            continue;
        }
        let entry = map.entry(emoji.clone()).or_insert_with(|| {
            order.push(emoji.clone());
            (0, false, Vec::new())
        });
        entry.0 = entry.0.saturating_add(1);
        let uid = r.user.as_ref().and_then(|u| u.id.as_deref());
        if me.is_some_and(|me| uid == Some(me.as_str())) {
            entry.1 = true;
        }
        if let Some(name) = r.user.as_ref().map(|u| u.display_name.as_str()).filter(|n| !n.is_empty())
            && entry.2.len() < 10
        {
            entry.2.push(name.to_owned());
        }
    }
    order
        .into_iter()
        .filter_map(|e| {
            let (count, mine, names) = map.remove(&e)?;
            Some(json!({ "emoji": e, "count": count, "mine": mine, "names": names }))
        })
        .collect()
}

/// Graph's legacy reaction names to emoji; anything else is shown as sent (short, no controls).
pub(crate) fn emoji_for(reaction: &str) -> String {
    match reaction {
        "like" => "👍".into(),
        "heart" => "❤️".into(),
        "laugh" => "😆".into(),
        "surprised" => "😮".into(),
        "sad" => "😢".into(),
        "angry" => "😡".into(),
        other => other.chars().filter(|c| !c.is_control()).take(16).collect(),
    }
}

fn quote_of(m: &ChatMessage) -> Option<Value> {
    let att = m.attachments.iter().find(|a| a.content_type == "messageReference")?;
    let content = att.content.as_ref()?.as_str();
    if content.len() > 16 * 1024 {
        return None;
    }
    let v: Value = serde_json::from_str(content).ok()?;
    let text = |p: &str| v.pointer(p).and_then(Value::as_str).map(|s| s.chars().take(300).collect::<String>());
    Some(json!({
        "id": text("/messageId"),
        "preview": text("/messagePreview").unwrap_or_default(),
        "from": text("/messageSender/user/displayName").or_else(|| text("/messageSender/application/displayName")),
    }))
}

/// Display HTML: sanitised, hosted images pointed at the webview scheme.
pub(crate) fn display_html(m: &ChatMessage) -> String {
    if m.is_deleted() {
        return String::new();
    }
    match m.body.content_type {
        BodyType::Text => text_to_html(m.body.content.as_str()),
        _ => quorum_sanitize::webview_urls(&quorum_sanitize::sanitize_chat(m.body.content.as_str()).html, platform()),
    }
}

/// A message row for the UI (chat messages, channel posts and replies).
pub(crate) fn message_row(m: &ChatMessage, me: Option<&UserId>) -> Value {
    let from_id = m.from.as_ref().and_then(|f| f.id.clone());
    let mine = match (&from_id, me) {
        (Some(f), Some(me)) => f == me.as_str(),
        _ => false,
    } || (is_local(m.id.as_str()));
    let system = !matches!(m.message_type, ChatMessageType::Message);
    let files: Vec<Value> = m
        .attachments
        .iter()
        .filter(|a| a.content_type == "reference")
        .take(MAX_ATTACHMENTS * 2)
        .map(|a| {
            json!({
                "name": a.name.as_ref().map(|n| n.as_str().to_owned()).unwrap_or_else(|| "File".into()),
                "url": a.content_url.as_deref().filter(|u| u.starts_with("https://")),
            })
        })
        .collect();
    let cards = m.attachments.iter().filter(|a| a.content_type.starts_with("application/vnd.microsoft.card")).count();
    let mentions_me = me.is_some_and(|me| {
        m.mentions.iter().any(|x| x.mentioned.as_ref().and_then(|i| i.id.as_deref()) == Some(me.as_str()))
    });
    json!({
        "id": m.id.as_str(),
        "reply_to": m.reply_to_id.as_ref().map(ChatMessageId::as_str),
        "from_id": from_id,
        "from_name": m.from.as_ref().map(|f| f.display_name.as_str().to_owned()).filter(|n| !n.is_empty()).unwrap_or_else(|| if system { String::new() } else { "Unknown".into() }),
        "mine": mine,
        "time": m.created_date_time,
        "edited": m.is_edited() && !m.is_deleted(),
        "deleted": m.is_deleted(),
        "system": system,
        "pending": is_local(m.id.as_str()),
        "subject": m.subject.as_ref().map(|s| s.as_str().to_owned()).filter(|s| !s.is_empty()),
        "importance": m.importance.as_graph(),
        "html": display_html(m),
        "reactions": reaction_summary(m, me),
        "files": files,
        "cards": cards,
        "quote": quote_of(m),
        "mentions_me": mentions_me,
    })
}

fn member_name(m: &quorum_model::ChatMember) -> String {
    let n = m.display_name.as_str();
    if !n.trim().is_empty() {
        return n.to_owned();
    }
    m.email.as_ref().map(|e| e.as_str().to_owned()).unwrap_or_else(|| "Someone".into())
}

/// Display title of a chat: its topic, else the other members' names.
pub(crate) fn chat_title(c: &Chat, me: Option<&UserId>) -> String {
    if let Some(t) = c.topic.as_ref().map(Sensitive::as_str).filter(|t| !t.trim().is_empty()) {
        return t.to_owned();
    }
    let others: Vec<String> =
        c.members.iter().filter(|m| me.is_none() || m.user_id.as_ref() != me).map(member_name).take(5).collect();
    if others.is_empty() {
        return match c.members.first() {
            Some(m) => member_name(m),
            None => "Chat".into(),
        };
    }
    let extra = c.members.len().saturating_sub(others.len()).saturating_sub(1);
    if extra > 0 && c.chat_type != ChatType::OneOnOne {
        format!("{} +{extra}", others.join(", "))
    } else {
        others.join(", ")
    }
}

// ---------------------------------------------------------------------------------------------
// Commands.
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct ChatParam {
    #[serde(default, alias = "id")]
    chat: Option<String>,
}

#[derive(Deserialize)]
struct OpenParams {
    #[serde(default, alias = "chat")]
    id: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default = "yes")]
    mark_read: bool,
}

fn yes() -> bool {
    true
}

fn open(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: OpenParams = parse(p)?;
        let chat = chat_id(p.id)?;
        let store = s.store()?;
        // A chat created locally may have its real id by now.
        let chat = match store
            .setting::<String>(format!("{}{}", quorum_sync::teams::outbox::CREATED_PREFIX, chat.as_str()))
            .await
        {
            Ok(Some(real)) => ChatId::new(real).unwrap_or(chat),
            _ => chat,
        };
        store.set_setting(OPEN_CHAT_SETTING.to_owned(), chat.as_str().to_owned()).await.map_err(store_err)?;
        if let Ok(sync) = s.sync() {
            sync.set_focus(&chat_key(&chat));
            sync.poke(&chat_key(&chat));
        }
        let cached = store
            .read({
                let c = chat.clone();
                move |r| r.chat(&c)
            })
            .await
            .map_err(store_err)?;
        if p.mark_read && cached.as_ref().is_some_and(Chat::has_unread) {
            mark_read_inner(s, chat.clone(), true).await?;
        }
        let mut params = json!({ "area": "chat", "id": chat.as_str() });
        if let (Some(m), Some(obj)) = (p.message, params.as_object_mut()) {
            obj.insert("message".into(), Value::String(m));
        }
        s.emit(CoreEvent::Ui { command: "view.open".into(), params });
        Ok(json!({ "id": chat.as_str() }))
    })
}

#[derive(Deserialize)]
struct NewParams {
    #[serde(default)]
    members: Vec<String>,
    #[serde(default)]
    emails: Vec<String>,
    #[serde(default)]
    topic: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

fn new_chat(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: NewParams = parse(p)?;
        let (me, _) = identity(s).await;
        let me = me.ok_or_else(|| {
            Error::new(ErrorKind::NotAvailable, "your user id is not known yet; try again after sync")
        })?;
        let mut members: Vec<UserId> = Vec::new();
        let mut seen = HashSet::new();
        for m in p.members.iter().chain(p.emails.iter()) {
            let m = m.trim();
            let id = UserId::new(m).map_err(|_| Error::params("members must be user ids or addresses"))?;
            if m.eq_ignore_ascii_case(me.as_str()) || !seen.insert(m.to_ascii_lowercase()) {
                continue;
            }
            members.push(id);
        }
        if members.is_empty() {
            return Err(Error::params("add at least one person to chat with (`members`)"));
        }
        if members.len() > 249 {
            return Err(Error::params("a group chat has at most 250 members"));
        }
        let topic = p.topic.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty());
        if topic.as_ref().is_some_and(|t| t.chars().count() > 250 || t.chars().any(char::is_control)) {
            return Err(Error::params("topic must be at most 250 characters"));
        }
        let message = match p.message.as_deref() {
            Some(m) => Some(outgoing_html(Some(m), None)?).filter(|h| has_content(h)),
            None => None,
        };
        let store = s.store()?;
        // An existing one-on-one chat with that person is reused.
        if members.len() == 1 && topic.is_none() {
            let other = members.first().cloned();
            let existing = store
                .read(move |r| {
                    let Some(other) = other else { return Ok(None) };
                    for id in r.chats_with_member(&other)? {
                        if let Some(c) = r.chat(&id)?
                            && c.chat_type == ChatType::OneOnOne
                        {
                            return Ok(Some(c.id));
                        }
                    }
                    Ok(None)
                })
                .await
                .map_err(store_err)?;
            if let Some(id) = existing {
                if let Some(html) = message {
                    send_inner(
                        s,
                        id.clone(),
                        html,
                        Vec::new(),
                        None,
                        ChatImportance::Normal,
                        Prepared { images: vec![], files: vec![], image_html: String::new(), blobs: vec![] },
                    )
                    .await?;
                }
                s.execute("chat.open", json!({ "id": id.as_str(), "mark_read": false })).await?;
                return Ok(json!({ "id": id.as_str(), "existing": true }));
            }
        }
        let local = ChatId::new(local_id("chat")).map_err(|_| Error::new(ErrorKind::Bug, "local id"))?;
        let chat_type = if members.len() == 1 && topic.is_none() { ChatType::OneOnOne } else { ChatType::Group };
        let mut all = vec![me.clone()];
        all.extend(members.iter().cloned());
        let now = Timestamp::now();
        let optimistic = Chat {
            id: local.clone(),
            topic: topic.clone().map(Sensitive::new),
            chat_type: chat_type.clone(),
            members: all
                .iter()
                .map(|u| quorum_model::ChatMember {
                    user_id: Some(u.clone()),
                    display_name: Sensitive::new(if u == &me { "You".to_owned() } else { u.as_str().to_owned() }),
                    email: None,
                    roles: vec!["owner".into()],
                })
                .collect(),
            last_message_preview: None,
            last_updated_date_time: Some(now),
            created_date_time: Some(now),
            last_message_read_date_time: Some(now),
            is_hidden: false,
            web_url: None,
        };
        let payload = CreatePayload {
            local_id: local.clone(),
            chat_type: chat_type.as_graph().to_owned(),
            members: all,
            topic,
            message: message.clone(),
        };
        let first_id = ChatMessageId::new(local_id("msg")).map_err(|_| Error::new(ErrorKind::Bug, "local id"))?;
        let first = message.map(|html| {
            pending_message(
                first_id,
                MessageContainer::Chat(local.clone()),
                None,
                Some(&me),
                html,
                None,
                &[],
                &[],
                ChatImportance::Normal,
            )
        });
        enqueue(
            s,
            "chat.create",
            chat_key(&local),
            Some(local.as_str().to_owned()),
            to_payload(&payload)?,
            move |tx| {
                tx.upsert_chats(std::slice::from_ref(&optimistic))?;
                if let Some(m) = first {
                    tx.upsert_chat_messages(std::slice::from_ref(&m))?;
                }
                Ok(())
            },
        )
        .await?;
        s.execute("chat.open", json!({ "id": local.as_str(), "mark_read": false })).await?;
        Ok(json!({ "id": local.as_str(), "existing": false }))
    })
}

#[derive(Deserialize)]
struct SendParams {
    #[serde(default, alias = "id")]
    chat: Option<String>,
    #[serde(default)]
    html: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    mentions: Vec<MentionParam>,
    #[serde(default)]
    images: Vec<DataParam>,
    #[serde(default)]
    files: Vec<DataParam>,
    #[serde(default)]
    quote: Option<String>,
    #[serde(default)]
    importance: Option<String>,
}

pub(crate) fn importance(s: Option<&str>) -> Result<ChatImportance> {
    match s.unwrap_or("normal") {
        "normal" => Ok(ChatImportance::Normal),
        "high" => Ok(ChatImportance::High),
        "urgent" => Ok(ChatImportance::Urgent),
        _ => Err(Error::params("importance must be normal, high or urgent")),
    }
}

fn send(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: SendParams = parse(p)?;
        let chat = chat_id(p.chat)?;
        let html = outgoing_html(p.html.as_deref(), p.text.as_deref())?;
        let mentions = mentions(&html, &p.mentions)?;
        let local = local_id("msg");
        let prepared = prepare_attachments(&local, &p.images, &p.files)?;
        if !has_content(&html) && prepared.images.is_empty() && prepared.files.is_empty() {
            return Err(Error::params("write a message first (`html` or `text`)"));
        }
        let quote = match p.quote {
            Some(q) => {
                let qid = message_id(&q)?;
                let store = s.store()?;
                let container = MessageContainer::Chat(chat.clone());
                let found = store.read(move |r| r.chat_message(&container, &qid)).await.map_err(store_err)?;
                let m = found.ok_or_else(|| Error::new(ErrorKind::NotFound, "the quoted message is not cached"))?;
                Some(QuotePayload {
                    message_id: m.id.clone(),
                    preview: quorum_sanitize::to_plain_text(m.body.content.as_str()).chars().take(200).collect(),
                    sender_id: m.from.as_ref().and_then(|f| f.id.clone()).and_then(|i| UserId::new(i).ok()),
                    sender_name: m.from.as_ref().map(|f| f.display_name.as_str().to_owned()),
                })
            }
            None => None,
        };
        let imp = importance(p.importance.as_deref())?;
        let id = send_with(s, chat, local, html, mentions, quote, imp, prepared).await?;
        Ok(json!({ "local_id": id }))
    })
}

async fn send_inner(
    s: &Session,
    chat: ChatId,
    html: String,
    mentions: Vec<MentionPayload>,
    quote: Option<QuotePayload>,
    imp: ChatImportance,
    prepared: Prepared,
) -> Result<String> {
    send_with(s, chat, local_id("msg"), html, mentions, quote, imp, prepared).await
}

async fn send_with(
    s: &Session,
    chat: ChatId,
    local: String,
    html: String,
    mentions: Vec<MentionPayload>,
    quote: Option<QuotePayload>,
    imp: ChatImportance,
    prepared: Prepared,
) -> Result<String> {
    let (me, _) = identity(s).await;
    let local_msg = ChatMessageId::new(local.clone()).map_err(|_| Error::new(ErrorKind::Bug, "local id"))?;
    let Prepared { images, files, image_html, blobs } = prepared;
    store_blobs(s, blobs).await?;
    let full = format!("{html}{image_html}");
    let optimistic = pending_message(
        local_msg.clone(),
        MessageContainer::Chat(chat.clone()),
        None,
        me.as_ref(),
        full.clone(),
        None,
        &mentions,
        &files,
        imp.clone(),
    );
    let payload = SendPayload {
        target: Target::Chat { chat: chat.clone(), message: None },
        local_id: local_msg,
        html: full,
        subject: None,
        importance: Some(imp.as_graph().to_owned()),
        mentions,
        images,
        files,
        quote,
    };
    let draft_id = draft_id_for(&chat);
    let preview_chat = chat.clone();
    let preview_text: String =
        quorum_sanitize::to_plain_text(optimistic.body.content.as_str()).chars().take(200).collect();
    let now = optimistic.created_date_time;
    let me_c = me.clone();
    enqueue(s, "chat.send", chat_key(&chat), Some(local.clone()), to_payload(&payload)?, move |tx| {
        tx.upsert_chat_messages(std::slice::from_ref(&optimistic))?;
        tx.delete_draft(&draft_id)?;
        // Move the chat to the top of the list with the new preview.
        if let Some(mut c) = tx.read().chat(&preview_chat)? {
            c.last_message_preview = Some(quorum_model::ChatPreview {
                id: optimistic.id.clone(),
                created_date_time: now,
                from: Some(Identity {
                    kind: IdentityKind::User,
                    id: me_c.map(|m| m.as_str().to_owned()),
                    display_name: Sensitive::new("You"),
                }),
                body_preview: Sensitive::new(preview_text),
                is_deleted: false,
            });
            c.last_message_read_date_time = now;
            tx.upsert_chats(std::slice::from_ref(&c))?;
        }
        Ok(())
    })
    .await?;
    Ok(local)
}

#[derive(Deserialize)]
struct MessageParams {
    #[serde(default, alias = "id")]
    chat: Option<String>,
    message: String,
    #[serde(default)]
    html: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    mentions: Vec<MentionParam>,
    #[serde(default)]
    reaction: Option<String>,
}

async fn cached_message(s: &Session, container: MessageContainer, id: ChatMessageId) -> Result<ChatMessage> {
    let store = s.store()?;
    store
        .read(move |r| r.chat_message(&container, &id))
        .await
        .map_err(store_err)?
        .ok_or_else(|| Error::new(ErrorKind::NotFound, "that message is not cached"))
}

pub(crate) fn ensure_mine(m: &ChatMessage, me: Option<&UserId>, what: &str) -> Result<()> {
    let from = m.from.as_ref().and_then(|f| f.id.as_deref());
    if is_local(m.id.as_str()) {
        return Err(Error::new(ErrorKind::NotAvailable, "this message is still being sent"));
    }
    match (from, me) {
        (Some(f), Some(me)) if f == me.as_str() => Ok(()),
        _ => Err(Error::new(ErrorKind::NotAvailable, format!("you can only {what} your own messages"))),
    }
}

fn edit(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: MessageParams = parse(p)?;
        let chat = chat_id(p.chat)?;
        let mid = message_id(&p.message)?;
        let html = outgoing_html(p.html.as_deref(), p.text.as_deref())?;
        if !has_content(&html) {
            return Err(Error::params("the new text is empty; delete the message instead"));
        }
        let mentions = mentions(&html, &p.mentions)?;
        let (me, _) = identity(s).await;
        let before = cached_message(s, MessageContainer::Chat(chat.clone()), mid.clone()).await?;
        ensure_mine(&before, me.as_ref(), "edit")?;
        let target = Target::Chat { chat: chat.clone(), message: Some(mid) };
        edit_target(s, "chat.edit", target, html, mentions, before).await
    })
}

pub(crate) async fn edit_target(
    s: &Session,
    kind: &str,
    target: Target,
    html: String,
    mentions: Vec<MentionPayload>,
    before: ChatMessage,
) -> Result<Value> {
    let mut after = before.clone();
    after.body = ChatBody { content_type: BodyType::Html, content: Sensitive::new(html.clone()) };
    after.mentions = model_mentions(&mentions);
    after.last_edited_date_time = Some(Timestamp::now());
    let payload = EditPayload { target: target.clone(), html, mentions, before: Some(before) };
    enqueue(s, kind, target.resource_key(), None, to_payload(&payload)?, move |tx| {
        tx.upsert_chat_messages(std::slice::from_ref(&after))
    })
    .await?;
    Ok(Value::Null)
}

fn delete(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: MessageParams = parse(p)?;
        let chat = chat_id(p.chat)?;
        let mid = message_id(&p.message)?;
        let (me, _) = identity(s).await;
        let before = cached_message(s, MessageContainer::Chat(chat.clone()), mid.clone()).await?;
        ensure_mine(&before, me.as_ref(), "delete")?;
        delete_target(s, "chat.delete", Target::Chat { chat, message: Some(mid) }, before).await
    })
}

pub(crate) async fn delete_target(s: &Session, kind: &str, target: Target, before: ChatMessage) -> Result<Value> {
    if before.is_deleted() {
        return Ok(Value::Null);
    }
    let mut after = before.clone();
    after.deleted_date_time = Some(Timestamp::now());
    let payload = DeletePayload { target: target.clone(), before: Some(before) };
    enqueue(s, kind, target.resource_key(), None, to_payload(&payload)?, move |tx| {
        tx.upsert_chat_messages(std::slice::from_ref(&after))
    })
    .await?;
    Ok(Value::Null)
}

pub(crate) fn check_reaction(r: Option<&str>) -> Result<String> {
    let r = r.unwrap_or("").trim();
    if r.is_empty() || r.len() > 64 || r.chars().any(char::is_control) || r.chars().count() > 16 {
        return Err(Error::params("`reaction` must be an emoji such as 👍"));
    }
    Ok(r.to_owned())
}

pub(crate) async fn react_target(
    s: &Session,
    kind: &str,
    target: Target,
    reaction: String,
    remove: bool,
    before: ChatMessage,
) -> Result<Value> {
    let (me, _) = identity(s).await;
    let mut after = before.clone();
    let mine = |r: &quorum_model::Reaction| {
        me.as_ref().is_some_and(|me| r.user.as_ref().and_then(|u| u.id.as_deref()) == Some(me.as_str()))
    };
    let shown = emoji_for(&reaction);
    after.reactions.retain(|r| !(mine(r) && emoji_for(&r.reaction_type) == shown));
    if !remove {
        after.reactions.push(quorum_model::Reaction {
            reaction_type: reaction.clone(),
            created_date_time: Some(Timestamp::now()),
            user: Some(Identity {
                kind: IdentityKind::User,
                id: me.as_ref().map(|m| m.as_str().to_owned()),
                display_name: Sensitive::new("You"),
            }),
        });
    }
    let payload = ReactPayload { target: target.clone(), reaction, remove, before: Some(before) };
    enqueue(s, kind, target.resource_key(), None, to_payload(&payload)?, move |tx| {
        tx.upsert_chat_messages(std::slice::from_ref(&after))
    })
    .await?;
    Ok(Value::Null)
}

fn react_impl(s: &Session, p: Value, remove: bool) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: MessageParams = parse(p)?;
        let chat = chat_id(p.chat)?;
        let mid = message_id(&p.message)?;
        let reaction = check_reaction(p.reaction.as_deref())?;
        if is_local(mid.as_str()) {
            return Err(Error::new(ErrorKind::NotAvailable, "this message is still being sent"));
        }
        let before = cached_message(s, MessageContainer::Chat(chat.clone()), mid.clone()).await?;
        let kind = if remove { "chat.unreact" } else { "chat.react" };
        react_target(s, kind, Target::Chat { chat, message: Some(mid) }, reaction, remove, before).await
    })
}

fn react(s: &Session, p: Value) -> CommandFuture<'_> {
    react_impl(s, p, false)
}

fn unreact(s: &Session, p: Value) -> CommandFuture<'_> {
    react_impl(s, p, true)
}

async fn mark_read_inner(s: &Session, chat: ChatId, read: bool) -> Result<()> {
    if is_local(chat.as_str()) {
        return Ok(());
    }
    let (user, tenant) = identity(s).await;
    let (Some(user), Some(tenant)) = (user, tenant) else {
        return Err(Error::new(ErrorKind::NotAvailable, "your account is not known yet; try again after sync"));
    };
    let now = Timestamp::now();
    let store = s.store()?;
    let cached = store
        .read({
            let c = chat.clone();
            move |r| r.chat(&c)
        })
        .await
        .map_err(store_err)?;
    let cached = cached.ok_or_else(|| Error::new(ErrorKind::NotFound, "that chat is not cached"))?;
    // Unread from just before the latest message, so it counts as unread.
    let last_read = cached
        .last_message_preview
        .as_ref()
        .and_then(|p| p.created_date_time)
        .and_then(|t| t.checked_sub(jiff::SignedDuration::from_secs(1)).ok())
        .unwrap_or(now);
    let payload = ReadPayload { chat: chat.clone(), user, tenant, last_read: (!read).then_some(last_read) };
    let kind = if read { "chat.mark_read" } else { "chat.mark_unread" };
    enqueue(s, kind, chat_key(&chat), None, to_payload(&payload)?, move |tx| {
        let mut c = cached;
        c.last_message_read_date_time = Some(if read { now } else { last_read });
        tx.upsert_chats(std::slice::from_ref(&c))
    })
    .await
}

fn mark_read(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: ChatParam = parse(p)?;
        mark_read_inner(s, chat_id(p.chat)?, true).await?;
        Ok(Value::Null)
    })
}

fn mark_unread(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: ChatParam = parse(p)?;
        mark_read_inner(s, chat_id(p.chat)?, false).await?;
        Ok(Value::Null)
    })
}

fn load_older(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: ChatParam = parse(p)?;
        let chat = chat_id(p.chat)?;
        let store = s.store()?;
        store.set_setting(older_request_key(&chat), true).await.map_err(store_err)?;
        if let Ok(sync) = s.sync() {
            sync.poke(&chat_key(&chat));
        }
        Ok(Value::Null)
    })
}

#[derive(Deserialize)]
struct MemberParams {
    chat: String,
    user: String,
    #[serde(default)]
    share_history: bool,
}

fn member_payload(p: MemberParams) -> Result<MemberPayload> {
    let chat = chat_id(Some(p.chat))?;
    if is_local(chat.as_str()) {
        return Err(Error::new(ErrorKind::NotAvailable, "the chat is still being created"));
    }
    let user = UserId::new(p.user.trim()).map_err(|_| Error::params("`user` must be a user id or address"))?;
    Ok(MemberPayload { chat, user, share_history: p.share_history })
}

fn add_member(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let payload = member_payload(parse(p)?)?;
        let key = chat_key(&payload.chat);
        enqueue(s, "chat.add_member", key, None, to_payload(&payload)?, |_| Ok(())).await?;
        Ok(Value::Null)
    })
}

fn remove_member(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let payload = member_payload(parse(p)?)?;
        let key = chat_key(&payload.chat);
        let (chat, user) = (payload.chat.clone(), payload.user.clone());
        enqueue(s, "chat.remove_member", key, None, to_payload(&payload)?, move |tx| {
            if let Some(mut c) = tx.read().chat(&chat)? {
                c.members.retain(|m| m.user_id.as_ref() != Some(&user));
                if !c.members.is_empty() {
                    tx.upsert_chats(std::slice::from_ref(&c))?;
                }
            }
            Ok(())
        })
        .await?;
        Ok(Value::Null)
    })
}

async fn pinned(s: &Session) -> Result<Vec<String>> {
    Ok(s.store()?.ui_pref::<Vec<String>>(PINNED_CHATS_PREF.to_owned()).await.map_err(store_err)?.unwrap_or_default())
}

fn pin_chat(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let chat = chat_id(parse::<ChatParam>(p)?.chat)?;
        let mut list = pinned(s).await?;
        if !list.iter().any(|c| c == chat.as_str()) {
            if list.len() >= 50 {
                return Err(Error::params("at most 50 chats can be pinned"));
            }
            list.push(chat.as_str().to_owned());
        }
        s.store()?.set_ui_pref(PINNED_CHATS_PREF.to_owned(), list).await.map_err(store_err)?;
        s.emit(CoreEvent::StoreChanged { tables: vec!["chat".into()] });
        Ok(Value::Null)
    })
}

fn unpin_chat(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let chat = chat_id(parse::<ChatParam>(p)?.chat)?;
        let mut list = pinned(s).await?;
        list.retain(|c| c != chat.as_str());
        s.store()?.set_ui_pref(PINNED_CHATS_PREF.to_owned(), list).await.map_err(store_err)?;
        s.emit(CoreEvent::StoreChanged { tables: vec!["chat".into()] });
        Ok(Value::Null)
    })
}

fn pin_impl<'a>(s: &'a Session, p: Value, kind: &'static str) -> CommandFuture<'a> {
    Box::pin(async move {
        let p: MessageParams = parse(p)?;
        let chat = chat_id(p.chat)?;
        let message = message_id(&p.message)?;
        if is_local(message.as_str()) || is_local(chat.as_str()) {
            return Err(Error::new(ErrorKind::NotAvailable, "this message is still being sent"));
        }
        let payload = PinPayload { chat: chat.clone(), message };
        enqueue(s, kind, chat_key(&chat), None, to_payload(&payload)?, |_| Ok(())).await?;
        Ok(Value::Null)
    })
}

fn pin_message(s: &Session, p: Value) -> CommandFuture<'_> {
    pin_impl(s, p, "chat.pin_message")
}

fn unpin_message(s: &Session, p: Value) -> CommandFuture<'_> {
    pin_impl(s, p, "chat.unpin_message")
}

pub(crate) fn draft_id_for(chat: &ChatId) -> String {
    format!("chat:{}", chat.as_str())
}

#[derive(Deserialize)]
struct DraftParams {
    #[serde(default, alias = "id")]
    chat: Option<String>,
    #[serde(default)]
    html: String,
    #[serde(default)]
    mentions: Value,
}

/// Saves (or, when empty, deletes) a draft with `id`, `kind` and `context`.
pub(crate) async fn save_draft_inner(
    s: &Session,
    id: String,
    kind: DraftKind,
    context: String,
    html: String,
    mentions: Value,
) -> Result<Value> {
    if html.len() > MAX_OUTGOING {
        return Err(Error::params("the draft is too long"));
    }
    let store = s.store()?;
    if !has_content(&html) {
        store.delete_draft(id).await.map_err(store_err)?;
        return Ok(Value::Null);
    }
    let mut d = Draft::new(id, kind);
    d.context = Some(context);
    d.body = Sensitive::new(html);
    d.fields = json!({ "mentions": if mentions.is_array() { mentions } else { json!([]) } });
    let at = store.save_draft(d).await.map_err(store_err)?;
    Ok(json!({ "updated_at": at }))
}

fn save_draft(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: DraftParams = parse(p)?;
        let chat = chat_id(p.chat)?;
        save_draft_inner(s, draft_id_for(&chat), DraftKind::Chat, chat.as_str().to_owned(), p.html, p.mentions).await
    })
}

// ---------------------------------------------------------------------------------------------
// Queries.
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct ListParams {
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    filter: Option<String>,
    #[serde(default)]
    query: Option<String>,
}

fn list(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: ListParams = parse(p)?;
        let limit = usize::try_from(p.limit.unwrap_or(200).clamp(1, 500)).unwrap_or(200);
        let filter = p.filter.unwrap_or_else(|| "all".into());
        if !matches!(filter.as_str(), "all" | "unread" | "pinned" | "group" | "meeting" | "hidden") {
            return Err(Error::params("filter must be all, unread, pinned, group, meeting or hidden"));
        }
        let q = p.query.map(|q| q.trim().to_lowercase()).filter(|q| !q.is_empty());
        if q.as_ref().is_some_and(|q| q.len() > 256) {
            return Err(Error::params("query is too long"));
        }
        let (me, _) = identity(s).await;
        let store = s.store()?;
        let hidden = filter == "hidden";
        let chats: Vec<Chat> = store
            .read(move |r| {
                let mut out = Vec::new();
                let mut after: Option<Cursor> = None;
                for _ in 0..20 {
                    let page = r.chats(after.as_ref(), 500, hidden)?;
                    out.extend(page.items);
                    match page.next {
                        Some(n) if out.len() < 5000 => after = Some(n),
                        _ => break,
                    }
                }
                Ok(out)
            })
            .await
            .map_err(store_err)?;
        let pins = pinned(s).await?;
        let drafts: HashSet<String> = store
            .drafts(Some(DraftKind::Chat), 500)
            .await
            .map(|p| p.items.into_iter().filter_map(|d| d.context).collect())
            .unwrap_or_default();
        let mut rows: Vec<(usize, Value, Option<UserId>)> = Vec::new();
        for c in &chats {
            if hidden && !c.is_hidden {
                continue;
            }
            let pinned_at = pins.iter().position(|x| x == c.id.as_str());
            let unread = c.has_unread();
            let keep = match filter.as_str() {
                "unread" => unread,
                "pinned" => pinned_at.is_some(),
                "group" => c.chat_type == ChatType::Group,
                "meeting" => c.chat_type == ChatType::Meeting,
                _ => true,
            };
            if !keep {
                continue;
            }
            let title = chat_title(c, me.as_ref());
            let preview = c.last_message_preview.as_ref();
            let preview_text = preview
                .map(|p| {
                    if p.is_deleted {
                        "This message was deleted.".to_owned()
                    } else {
                        p.body_preview.as_str().to_owned()
                    }
                })
                .unwrap_or_default();
            if let Some(q) = &q
                && !title.to_lowercase().contains(q.as_str())
                && !preview_text.to_lowercase().contains(q.as_str())
            {
                continue;
            }
            let from_id = preview.and_then(|p| p.from.as_ref()).and_then(|f| f.id.clone());
            let from_me = matches!((&from_id, &me), (Some(f), Some(m)) if f == m.as_str());
            let from_name =
                preview.and_then(|p| p.from.as_ref()).map(|f| f.display_name.as_str().to_owned()).unwrap_or_default();
            let others: Vec<&quorum_model::ChatMember> =
                c.members.iter().filter(|m| me.is_none() || m.user_id.as_ref() != me.as_ref()).collect();
            let other =
                if c.chat_type == ChatType::OneOnOne { others.first().and_then(|m| m.user_id.clone()) } else { None };
            let members: Vec<Value> = c
                .members
                .iter()
                .take(50)
                .map(|m| json!({ "id": m.user_id.as_ref().map(UserId::as_str), "name": member_name(m), "email": m.email.as_ref().map(|e| e.as_str()), "me": me.is_some() && m.user_id.as_ref() == me.as_ref() }))
                .collect();
            let time = preview.and_then(|p| p.created_date_time).or(c.last_updated_date_time).or(c.created_date_time);
            let row = json!({
                "id": c.id.as_str(),
                "kind": c.chat_type.as_graph(),
                "title": title,
                "topic": c.topic.as_ref().map(|t| t.as_str().to_owned()),
                "members": members,
                "member_ids": others.iter().filter_map(|m| m.user_id.as_ref().map(UserId::as_str)).take(50).collect::<Vec<_>>(),
                "preview": preview_text,
                "last_message_preview": preview_text,
                "from_name": if from_me { "You".to_owned() } else { from_name },
                "from_me": from_me,
                "time": time,
                "unread": unread,
                "pinned": pinned_at.is_some(),
                "hidden": c.is_hidden,
                "pending": is_local(c.id.as_str()),
                "draft": drafts.contains(c.id.as_str()),
                "presence": Value::Null,
                "web_url": c.web_url,
            });
            rows.push((pinned_at.unwrap_or(usize::MAX), row, other));
        }
        // Pinned first in pin order; the rest keep activity order (stable sort).
        rows.sort_by_key(|(p, _, _)| *p);
        rows.truncate(limit);
        let ids: Vec<UserId> = rows.iter().filter_map(|(_, _, o)| o.clone()).collect();
        let presence = presences_of(s, ids).await.unwrap_or_default();
        let out: Vec<Value> = rows
            .into_iter()
            .map(|(_, mut row, other)| {
                if let (Some(o), Some(obj)) = (other, row.as_object_mut()) {
                    let fields = presence_fields(presence.get(&o));
                    if let Some((_, v)) = fields.into_iter().find(|(k, _)| *k == "presence") {
                        obj.insert("presence".into(), v);
                    }
                    obj.insert("presence_id".into(), json!(o.as_str()));
                }
                row
            })
            .collect();
        Ok(Value::Array(out))
    })
}

#[derive(Deserialize)]
struct MessagesParams {
    #[serde(default, alias = "id")]
    chat: Option<String>,
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

fn messages(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: MessagesParams = parse(p)?;
        let chat = chat_id(p.chat)?;
        let limit = p.limit.unwrap_or(50).clamp(1, 200);
        if p.before.as_ref().is_some_and(|b| b.len() > 2048) {
            return Err(Error::params("`before` is not a valid cursor"));
        }
        let before = p.before.map(Cursor::from_string);
        let (me, _) = identity(s).await;
        let store = s.store()?;
        let key = chat_key(&chat);
        let c = chat.clone();
        let (page, state) = store
            .read(move |r| Ok((r.chat_messages(&c, before.as_ref(), limit)?, r.delta_state(&key)?)))
            .await
            .map_err(|e| match e.kind() {
                quorum_store::ErrorKind::InvalidInput => Error::params("`before` is not a valid cursor"),
                _ => store_err(e),
            })?;
        let complete = state.as_ref().is_some_and(|s| s.backfill_complete);
        let mut items: Vec<Value> = page
            .items
            .iter()
            .filter(|m| m.message_type != ChatMessageType::Typing)
            .map(|m| message_row(m, me.as_ref()))
            .collect();
        items.reverse();
        Ok(json!({
            "chat": chat.as_str(),
            "items": items,
            "next": page.next.as_ref().map(Cursor::as_str),
            "has_more": page.next.is_some() || !complete,
            "server_has_more": !complete,
            "synced": state.is_some(),
            "me": me.as_ref().map(UserId::as_str),
        }))
    })
}

#[derive(Deserialize)]
struct SearchParams {
    query: String,
    #[serde(default)]
    limit: Option<u32>,
}

fn search(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: SearchParams = parse(p)?;
        let q = p.query.trim().to_owned();
        if q.is_empty() || q.len() > 256 {
            return Err(Error::params("`query` must be 1 to 256 characters"));
        }
        let limit = p.limit.unwrap_or(30).clamp(1, 100);
        let res =
            s.store()?.search(q, vec![SearchScope::Chat, SearchScope::Channel], limit).await.map_err(store_err)?;
        serde_json::to_value(res.hits).map_err(|e| Error::new(ErrorKind::Bug, "encode failed").with_source(e))
    })
}

/// A stored draft as `{html, mentions, updated_at}`.
pub(crate) async fn draft_inner(s: &Session, id: String) -> Result<Value> {
    let d = s.store()?.draft(id).await.map_err(store_err)?;
    Ok(match d {
        Some(d) => json!({
            "html": d.body.as_str(),
            "mentions": d.fields.get("mentions").cloned().unwrap_or_else(|| json!([])),
            "updated_at": d.updated_at,
        }),
        None => Value::Null,
    })
}

fn draft(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: ChatParam = parse(p)?;
        let chat = chat_id(p.chat)?;
        draft_inner(s, draft_id_for(&chat)).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outgoing_html_sanitises_and_restores_links() {
        let h = outgoing_html(
            Some(r#"<p onclick="x">Hi <a href="https://example.com/a?b=1">x</a><script>bad()</script></p>"#),
            None,
        )
        .unwrap();
        assert!(h.contains(r#"href="https://example.com/a?b=1""#), "{h}");
        assert!(!h.contains("script") && !h.contains("onclick"));
        assert_eq!(outgoing_html(None, Some("a <b>\nc")).unwrap(), "<p>a &lt;b&gt;<br>c</p>");
        assert!(!has_content(&outgoing_html(Some("<p> </p>"), None).unwrap()));
        assert!(has_content(r#"<at id="0">A</at>"#));
    }

    #[test]
    fn mentions_need_matching_at_elements() {
        let m = |id| MentionParam { id, text: "Al".into(), user: Some("u1".into()), kind: None, target: None };
        assert!(mentions(r#"<at id="0">Al</at>"#, &[m(0)]).is_ok());
        assert!(mentions("<p>Al</p>", &[m(0)]).is_err());
        assert!(mentions(r#"<at id="0">Al</at>"#, &[m(0), m(0)]).is_err());
    }

    #[test]
    fn reactions_fold_legacy_names() {
        assert_eq!(emoji_for("like"), "👍");
        assert_eq!(emoji_for("🎉"), "🎉");
        assert_eq!(emoji_for("\u{7}x"), "x");
    }

    #[test]
    fn decode_rejects_bad_and_huge() {
        assert!(decode("!!!", 10).is_err());
        assert!(decode(&"A".repeat(100), 10).is_err());
        assert_eq!(decode("data:image/png;base64,aGk=", 10).unwrap(), b"hi");
        assert!(safe_name("../..").is_err());
        assert_eq!(safe_name("a/b.txt").unwrap(), "ab.txt");
    }
}
