//! Teams chats: the chat list, members, messages, reactions, read state, hosted content and pins.
//!
//! Every wrapper has a `*_request` builder (no I/O) and an `async` call on [`Client`] that maps
//! Graph's JSON to `quorum-model` types:
//!
//! * `chatId` / `channelIdentity` become [`MessageContainer`] (falling back to the chat or channel
//!   the request addressed);
//! * Graph's `identitySet` (`user` / `application` / `device` / `conversation` / `tag`) becomes an
//!   [`Identity`];
//! * `viewpoint.lastMessageReadDateTime` and `viewpoint.isHidden` land on [`Chat`].
//!
//! Message actions that work the same on chats and channels (edit, soft delete, reactions) take a
//! [`MessageRef`]. Chats have no threaded replies: quote a message with
//! [`OutgoingMessage::quote`], which Graph sends as a `messageReference` attachment.
//!
//! This module also holds the lenient wire helpers shared by the other Teams and Files modules.

use std::fmt;
use std::fmt::Write as _;

use jiff::Timestamp;
use quorum_model::{
    BodyType, ChannelId, Chat, ChatAttachment, ChatBody, ChatId, ChatImportance, ChatMember, ChatMessage,
    ChatMessageId, ChatMessageType, ChatPreview, ChatType, DriveItem, GraphEnum, Identity, IdentityKind, MailAddress,
    Mention, MessageContainer, Reaction, Sensitive, TeamId, UserId,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};

use crate::client::Client;
use crate::error::{Error, ErrorKind, Result};
use crate::paging::Page;
use crate::request::{Request, ResourceFamily};

/// Concurrency bucket for chat and channel requests (kept apart from the mailbox bucket).
pub const TEAMS_BUCKET: &str = "teams";
/// Largest `$top` Graph accepts for chats, chat messages and channel messages.
pub const MAX_CHAT_PAGE_SIZE: u32 = 50;
/// Largest message body we send (a sanity cap; Teams enforces its own, smaller limit).
pub const MAX_OUTGOING_BODY_BYTES: usize = 256 * 1024;
/// Largest inline image (`hostedContents`) we send.
pub const MAX_HOSTED_IMAGE_BYTES: usize = 4 * 1024 * 1024;

const SELECT_CHAT: &str = "id,topic,chatType,createdDateTime,lastUpdatedDateTime,webUrl,viewpoint";

// ---------------------------------------------------------------------------------------------
// Shared wire helpers (also used by `teams`, `presence`, `files` and `sites`).
// ---------------------------------------------------------------------------------------------

/// Longest display name kept.
pub(crate) const MAX_NAME_CHARS: usize = 512;
/// Longest topic / subject / description kept.
pub(crate) const MAX_TITLE_CHARS: usize = 4096;
/// Longest message body kept (bytes).
pub(crate) const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
/// Longest URL kept; longer ones are dropped.
pub(crate) const MAX_URL_LEN: usize = 8192;
/// Most entries kept from an array inside one item (mentions, reactions, roles, ...).
pub(crate) const MAX_ITEMS: usize = 1024;

/// Percent-encodes one path segment (everything but RFC 3986 unreserved and sub-delims, `:`, `@`).
pub(crate) fn seg(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@".contains(&b) {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Validates an opaque id that has no model newtype (membership, permission, pinned ids, ...).
pub(crate) fn opaque<'a>(s: &'a str, what: &'static str) -> Result<&'a str> {
    if s.is_empty() || s.len() > quorum_model::MAX_ID_LEN || s.chars().any(char::is_control) {
        return Err(Error::new(ErrorKind::Config, format!("invalid {what}")));
    }
    Ok(s)
}

/// Deserialises a field, turning a value of the wrong shape into `None` instead of failing the item.
pub(crate) fn lenient<'de, D, T>(d: D) -> std::result::Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let v = Option::<Value>::deserialize(d)?;
    Ok(v.and_then(|v| serde_json::from_value(v).ok()))
}

/// Parses a Graph instant leniently (RFC 3339; a missing offset is read as UTC).
pub(crate) fn ts(s: Option<&str>) -> Option<Timestamp> {
    let s = s?.trim();
    if s.is_empty() || s.len() > 64 {
        return None;
    }
    s.parse::<Timestamp>().ok().or_else(|| format!("{s}Z").parse::<Timestamp>().ok())
}

/// Keeps at most `max` bytes of `s`, cut on a char boundary.
pub(crate) fn cap(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        s.truncate(end);
    }
    s
}

/// A capped [`Sensitive`] from an optional string.
pub(crate) fn sens(s: Option<String>, max: usize) -> Sensitive {
    Sensitive::new(cap(s.unwrap_or_default(), max))
}

/// An `https` URL from Graph, or `None` when missing, oversized or of another scheme.
pub(crate) fn web_url(s: Option<String>) -> Option<String> {
    s.filter(|u| u.len() <= MAX_URL_LEN && u.get(..8).is_some_and(|p| p.eq_ignore_ascii_case("https://")))
}

/// Parses a model id leniently.
pub(crate) fn id_of<T: std::str::FromStr>(s: Option<String>) -> Option<T> {
    s.and_then(|s| s.parse().ok())
}

/// Strings from an array, capped in count and length.
pub(crate) fn strings(v: Option<Vec<Value>>, max_len: usize) -> Vec<String> {
    v.unwrap_or_default()
        .into_iter()
        .filter_map(|x| x.as_str().map(|s| cap(s.to_owned(), max_len)))
        .take(MAX_ITEMS)
        .collect()
}

/// One entry of an `identitySet`.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WireIdentity {
    #[serde(default, deserialize_with = "lenient")]
    id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    display_name: Option<String>,
}

/// Graph's `identitySet` (and the chat flavours with `conversation` and `tag`).
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WireIdentitySet {
    #[serde(default, deserialize_with = "lenient")]
    user: Option<WireIdentity>,
    #[serde(default, deserialize_with = "lenient")]
    application: Option<WireIdentity>,
    #[serde(default, deserialize_with = "lenient")]
    device: Option<WireIdentity>,
    #[serde(default, deserialize_with = "lenient")]
    conversation: Option<WireIdentity>,
    #[serde(default, deserialize_with = "lenient")]
    tag: Option<WireIdentity>,
}

impl WireIdentitySet {
    /// The most specific identity: user, then application, device, conversation, tag.
    pub(crate) fn into_identity(self) -> Option<Identity> {
        let (kind, w) = if let Some(u) = self.user {
            (IdentityKind::User, u)
        } else if let Some(a) = self.application {
            (IdentityKind::Application, a)
        } else if let Some(d) = self.device {
            (IdentityKind::Device, d)
        } else if let Some(c) = self.conversation {
            (IdentityKind::from_graph("conversation"), c)
        } else {
            (IdentityKind::from_graph("tag"), self.tag?)
        };
        let id = w.id.filter(|s| opaque(s, "identity id").is_ok());
        if id.is_none() && w.display_name.as_deref().unwrap_or("").is_empty() {
            return None;
        }
        Some(Identity { kind, id, display_name: sens(w.display_name, MAX_NAME_CHARS) })
    }
}

/// Maps `identitySet` JSON to an [`Identity`] (`None` when empty or malformed).
pub(crate) fn identity(v: Option<WireIdentitySet>) -> Option<Identity> {
    v.and_then(WireIdentitySet::into_identity)
}

/// Converts a page of raw items, counting items that do not map as skipped.
pub(crate) fn map_page<M>(page: Page<Value>, mut f: impl FnMut(Value) -> Option<M>) -> Page<M> {
    let mut items = Vec::with_capacity(page.items.len());
    let mut skipped = page.skipped;
    for v in page.items {
        match f(v) {
            Some(m) => items.push(m),
            None => skipped = skipped.saturating_add(1),
        }
    }
    if skipped > page.skipped {
        tracing::warn!(name: "graph.items_unmapped", skipped, "collection items did not map to model types");
    }
    Page { items, skipped, next_link: page.next_link }
}

/// Error for a single-item response that does not map to the expected model type.
pub(crate) fn unmapped(what: &'static str) -> Error {
    Error::new(ErrorKind::Malformed, format!("response is not a valid {what}"))
}

/// Builds the plain-text preview of a (possibly HTML) body: tags dropped, common entities
/// decoded, whitespace collapsed, at most `max` chars.
pub(crate) fn preview_text(content: &str, html: bool, max: usize) -> String {
    let mut out = String::new();
    let mut chars = content.chars().peekable();
    let mut count = 0usize;
    let mut last_space = true;
    let push = |out: &mut String, c: char, count: &mut usize, last_space: &mut bool| {
        if c.is_whitespace() {
            if !*last_space {
                out.push(' ');
                *count = count.saturating_add(1);
            }
            *last_space = true;
        } else if !c.is_control() {
            out.push(c);
            *count = count.saturating_add(1);
            *last_space = false;
        }
    };
    while let Some(c) = chars.next() {
        if count >= max {
            break;
        }
        if html && c == '<' {
            for n in chars.by_ref() {
                if n == '>' {
                    break;
                }
            }
            push(&mut out, ' ', &mut count, &mut last_space);
            continue;
        }
        if html && c == '&' {
            let mut entity = String::new();
            while let Some(&n) = chars.peek() {
                if n == ';' || entity.len() > 8 {
                    break;
                }
                entity.push(n);
                chars.next();
            }
            let decoded = match entity.as_str() {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "#39" | "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ => None,
            };
            if let Some(d) = decoded
                && chars.peek() == Some(&';')
            {
                chars.next();
                push(&mut out, d, &mut count, &mut last_space);
            } else {
                push(&mut out, '&', &mut count, &mut last_space);
                for e in entity.chars() {
                    push(&mut out, e, &mut count, &mut last_space);
                }
            }
            continue;
        }
        push(&mut out, c, &mut count, &mut last_space);
    }
    out.trim_end().to_owned()
}

/// Standard base64 (RFC 4648, with padding), for `hostedContents` `contentBytes`.
pub(crate) fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let sym = |i: u32| ALPHABET.get(usize::try_from(i & 63).unwrap_or(0)).copied().map_or('A', char::from);
    let mut out = String::with_capacity(data.len().div_ceil(3).saturating_mul(4));
    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk.first().copied().unwrap_or(0));
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(sym(n >> 18));
        out.push(sym(n >> 12));
        out.push(if chunk.len() > 1 { sym(n >> 6) } else { '=' });
        out.push(if chunk.len() > 2 { sym(n) } else { '=' });
    }
    out
}

/// `https://graph.microsoft.com/v1.0` style base without a trailing slash.
fn base_of(base: &str) -> &str {
    base.trim_end_matches('/')
}

fn teams(req: Request) -> Request {
    req.family(ResourceFamily::Teams).bucket(TEAMS_BUCKET)
}

fn clamp_top(top: u32) -> u32 {
    top.clamp(1, MAX_CHAT_PAGE_SIZE)
}

// ---------------------------------------------------------------------------------------------
// Wire types and mapping.
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireBody {
    #[serde(default, deserialize_with = "lenient")]
    content_type: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    content: Option<String>,
}

impl WireBody {
    fn is_html(&self) -> bool {
        self.content_type.as_deref().is_some_and(|t| t.eq_ignore_ascii_case("html"))
    }

    fn into_model(self) -> ChatBody {
        let content_type = self.content_type.as_deref().map(BodyType::from_graph).unwrap_or_default();
        ChatBody { content_type, content: sens(self.content, MAX_BODY_BYTES) }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireMember {
    #[serde(default, deserialize_with = "lenient")]
    id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    display_name: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    user_id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    email: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    roles: Option<Vec<Value>>,
}

impl WireMember {
    fn member(self) -> (Option<String>, ChatMember) {
        let member = ChatMember {
            user_id: id_of(self.user_id),
            display_name: sens(self.display_name, MAX_NAME_CHARS),
            email: self.email.and_then(|e| MailAddress::new(e).ok()),
            roles: strings(self.roles, 64).into_iter().take(16).collect(),
        };
        (self.id.filter(|s| opaque(s, "membership id").is_ok()), member)
    }
}

/// A member of a chat, team or channel, with the membership id needed to remove them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationMember {
    /// Graph's `conversationMember` id (not the user id).
    pub membership_id: String,
    /// The member.
    pub member: ChatMember,
}

/// Maps a Graph `conversationMember` to a [`ConversationMember`].
#[must_use]
pub fn parse_conversation_member(value: Value) -> Option<ConversationMember> {
    let w: WireMember = serde_json::from_value(value).ok()?;
    let (id, member) = w.member();
    Some(ConversationMember { membership_id: id?, member })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WirePreview {
    #[serde(default, deserialize_with = "lenient")]
    id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    created_date_time: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    from: Option<WireIdentitySet>,
    #[serde(default, deserialize_with = "lenient")]
    body: Option<WireBody>,
    #[serde(default, deserialize_with = "lenient")]
    is_deleted: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireViewpoint {
    #[serde(default, deserialize_with = "lenient")]
    is_hidden: Option<bool>,
    #[serde(default, deserialize_with = "lenient")]
    last_message_read_date_time: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireChat {
    #[serde(default, deserialize_with = "lenient")]
    id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    topic: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    chat_type: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    members: Option<Vec<Value>>,
    #[serde(default, deserialize_with = "lenient")]
    last_message_preview: Option<WirePreview>,
    #[serde(default, deserialize_with = "lenient")]
    last_updated_date_time: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    created_date_time: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    viewpoint: Option<WireViewpoint>,
    #[serde(default, deserialize_with = "lenient")]
    web_url: Option<String>,
}

/// Maps a Graph `chat` (optionally with expanded `members` and `lastMessagePreview`) to a [`Chat`].
///
/// Returns `None` if the id or chat type is missing or invalid.
#[must_use]
pub fn parse_chat(value: Value) -> Option<Chat> {
    let w: WireChat = serde_json::from_value(value).ok()?;
    let id: ChatId = id_of(w.id)?;
    let chat_type = ChatType::from_graph(w.chat_type.as_deref()?);
    let members = w
        .members
        .unwrap_or_default()
        .into_iter()
        .take(MAX_ITEMS)
        .filter_map(|m| serde_json::from_value::<WireMember>(m).ok())
        .map(|m| m.member().1)
        .collect();
    let last_message_preview = w.last_message_preview.and_then(|p| {
        let id: ChatMessageId = id_of(p.id)?;
        let body_preview = p.body.map_or_else(String::new, |b| {
            let html = b.is_html();
            preview_text(b.content.as_deref().unwrap_or(""), html, 512)
        });
        Some(ChatPreview {
            id,
            created_date_time: ts(p.created_date_time.as_deref()),
            from: identity(p.from),
            body_preview: Sensitive::new(body_preview),
            is_deleted: p.is_deleted.unwrap_or(false),
        })
    });
    let (is_hidden, last_read) = w
        .viewpoint
        .map_or((false, None), |v| (v.is_hidden.unwrap_or(false), ts(v.last_message_read_date_time.as_deref())));
    Some(Chat {
        id,
        topic: w.topic.filter(|t| !t.is_empty()).map(|t| Sensitive::new(cap(t, MAX_TITLE_CHARS))),
        chat_type,
        members,
        last_message_preview,
        last_updated_date_time: ts(w.last_updated_date_time.as_deref()),
        created_date_time: ts(w.created_date_time.as_deref()),
        last_message_read_date_time: last_read,
        is_hidden,
        web_url: web_url(w.web_url),
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireChannelIdentity {
    #[serde(default, deserialize_with = "lenient")]
    team_id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    channel_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireMention {
    #[serde(default, deserialize_with = "lenient")]
    id: Option<u32>,
    #[serde(default, deserialize_with = "lenient")]
    mention_text: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    mentioned: Option<WireIdentitySet>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireReaction {
    #[serde(default, deserialize_with = "lenient")]
    reaction_type: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    created_date_time: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    user: Option<WireIdentitySet>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireAttachment {
    #[serde(default, deserialize_with = "lenient")]
    id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    content_type: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    content_url: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    name: Option<String>,
    #[serde(default)]
    content: Option<Value>,
    #[serde(default, deserialize_with = "lenient")]
    thumbnail_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireMessage {
    #[serde(default, deserialize_with = "lenient")]
    id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    chat_id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    channel_identity: Option<WireChannelIdentity>,
    #[serde(default, deserialize_with = "lenient")]
    reply_to_id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    from: Option<WireIdentitySet>,
    #[serde(default, deserialize_with = "lenient")]
    created_date_time: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    last_modified_date_time: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    last_edited_date_time: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    deleted_date_time: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    subject: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    body: Option<WireBody>,
    #[serde(default, deserialize_with = "lenient")]
    mentions: Option<Vec<Value>>,
    #[serde(default, deserialize_with = "lenient")]
    reactions: Option<Vec<Value>>,
    #[serde(default, deserialize_with = "lenient")]
    attachments: Option<Vec<Value>>,
    #[serde(default, deserialize_with = "lenient")]
    importance: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    message_type: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    web_url: Option<String>,
}

fn container_of(w: &WireMessage, fallback: Option<&MessageContainer>) -> Option<MessageContainer> {
    if let Some(chat) = w.chat_id.as_deref().and_then(|c| ChatId::new(c).ok()) {
        return Some(MessageContainer::Chat(chat));
    }
    if let Some(ci) = &w.channel_identity {
        let team = ci.team_id.as_deref().and_then(|t| TeamId::new(t).ok());
        let channel = ci.channel_id.as_deref().and_then(|c| ChannelId::new(c).ok());
        if let (Some(team_id), Some(channel_id)) = (team, channel) {
            return Some(MessageContainer::Channel { team_id, channel_id });
        }
    }
    fallback.cloned()
}

fn list_of<W: DeserializeOwned>(v: Option<Vec<Value>>) -> impl Iterator<Item = W> {
    v.unwrap_or_default().into_iter().take(MAX_ITEMS).filter_map(|x| serde_json::from_value::<W>(x).ok())
}

/// Maps a Graph `chatMessage` to a [`ChatMessage`].
///
/// The container comes from `chatId` or `channelIdentity`; when Graph sends neither, `fallback`
/// (the chat or channel the request addressed) is used. Returns `None` without an id or container.
#[must_use]
pub fn parse_chat_message(value: Value, fallback: Option<&MessageContainer>) -> Option<ChatMessage> {
    let w: WireMessage = serde_json::from_value(value).ok()?;
    let container = container_of(&w, fallback)?;
    let id: ChatMessageId = id_of(w.id)?;
    let mentions = list_of::<WireMention>(w.mentions)
        .filter_map(|m| {
            Some(Mention {
                id: m.id?,
                mention_text: sens(m.mention_text, MAX_NAME_CHARS),
                mentioned: identity(m.mentioned),
            })
        })
        .collect();
    let reactions = list_of::<WireReaction>(w.reactions)
        .map(|r| Reaction {
            reaction_type: cap(r.reaction_type.unwrap_or_default(), 64),
            created_date_time: ts(r.created_date_time.as_deref()),
            user: identity(r.user),
        })
        .collect();
    let attachments = list_of::<WireAttachment>(w.attachments)
        .map(|a| {
            let content = match a.content {
                Some(Value::String(s)) => Some(s),
                Some(Value::Null) | None => None,
                Some(other) => Some(other.to_string()),
            };
            ChatAttachment {
                id: a.id.map(|i| cap(i, 1024)),
                content_type: cap(a.content_type.unwrap_or_default(), 256),
                content_url: web_url(a.content_url),
                name: a.name.map(|n| Sensitive::new(cap(n, MAX_NAME_CHARS))),
                content: content.map(|c| Sensitive::new(cap(c, 64 * 1024))),
                thumbnail_url: web_url(a.thumbnail_url),
            }
        })
        .collect();
    Some(ChatMessage {
        id,
        container,
        reply_to_id: id_of(w.reply_to_id),
        from: identity(w.from),
        created_date_time: ts(w.created_date_time.as_deref()),
        last_modified_date_time: ts(w.last_modified_date_time.as_deref()),
        last_edited_date_time: ts(w.last_edited_date_time.as_deref()),
        deleted_date_time: ts(w.deleted_date_time.as_deref()),
        subject: w.subject.filter(|s| !s.is_empty()).map(|s| Sensitive::new(cap(s, MAX_TITLE_CHARS))),
        body: w.body.map(WireBody::into_model).unwrap_or_default(),
        mentions,
        reactions,
        attachments,
        importance: w.importance.as_deref().map(ChatImportance::from_graph).unwrap_or_default(),
        message_type: w.message_type.as_deref().map(ChatMessageType::from_graph).unwrap_or_default(),
        web_url: web_url(w.web_url),
    })
}

// ---------------------------------------------------------------------------------------------
// Outgoing messages.
// ---------------------------------------------------------------------------------------------

/// Who an outgoing @mention refers to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MentionTarget {
    /// A user.
    User {
        /// The user's id.
        id: UserId,
        /// Their display name.
        display_name: Sensitive,
    },
    /// A channel (channel posts only).
    Channel {
        /// The channel id.
        id: ChannelId,
        /// The channel name.
        display_name: Sensitive,
    },
    /// A whole team (channel posts only).
    Team {
        /// The team id.
        id: TeamId,
        /// The team name.
        display_name: Sensitive,
    },
    /// A team tag.
    Tag {
        /// The tag id.
        id: String,
        /// The tag name.
        display_name: Sensitive,
    },
}

/// An @mention in an outgoing message; the body must contain `<at id="{id}">text</at>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutgoingMention {
    /// Index referenced by `<at id="N">` in the body.
    pub id: u32,
    /// The text shown, e.g. the person's name.
    pub text: Sensitive,
    /// Who is mentioned.
    pub target: MentionTarget,
}

/// A file attached by reference (the file lives in OneDrive/SharePoint, as Teams does it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileReference {
    /// The attachment id: the GUID from the drive item's eTag.
    pub attachment_id: String,
    /// The item's `webUrl`.
    pub content_url: String,
    /// The file name shown.
    pub name: Sensitive,
}

impl FileReference {
    /// Builds a reference from an uploaded drive item (uses its eTag GUID and `webUrl`).
    #[must_use]
    pub fn from_drive_item(item: &DriveItem) -> Option<Self> {
        let etag = item.e_tag.as_deref()?;
        let start = etag.find('{')?;
        let rest = etag.get(start.saturating_add(1)..)?;
        let guid = rest.get(..rest.find('}')?)?;
        if guid.len() != 36 || !guid.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            return None;
        }
        Some(Self {
            attachment_id: guid.to_ascii_lowercase(),
            content_url: web_url(item.web_url.clone())?,
            name: item.name.clone(),
        })
    }
}

/// An inline image sent as `hostedContents`; reference it in the body as
/// `<img src="../hostedContents/{temporary_id}/$value">`.
#[derive(Clone, PartialEq, Eq)]
pub struct HostedImage {
    /// The temporary id used in the body (e.g. `"1"`).
    pub temporary_id: String,
    /// The MIME type, `image/*`.
    pub content_type: String,
    /// The image bytes (at most [`MAX_HOSTED_IMAGE_BYTES`]).
    pub bytes: Vec<u8>,
}

impl fmt::Debug for HostedImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostedImage")
            .field("temporary_id", &self.temporary_id)
            .field("content_type", &self.content_type)
            .field("len", &self.bytes.len())
            .finish()
    }
}

/// A message quoted in a chat (Graph's `messageReference` attachment; chats have no replies).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotedMessage {
    /// The quoted message.
    pub message_id: ChatMessageId,
    /// A short preview of its text.
    pub preview: Sensitive,
    /// Who sent it, if known.
    pub sender: Option<(UserId, Sensitive)>,
}

/// A chat message or channel post to send.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutgoingMessage {
    /// The body; HTML is required when mentions, files, images or a quote are present.
    pub body: ChatBody,
    /// Subject (channel posts only).
    pub subject: Option<Sensitive>,
    /// Importance.
    pub importance: ChatImportance,
    /// @mentions.
    pub mentions: Vec<OutgoingMention>,
    /// Files attached by reference; `<attachment id>` tags are appended to the body if missing.
    pub files: Vec<FileReference>,
    /// Inline images.
    pub images: Vec<HostedImage>,
    /// A quoted message (chats).
    pub quote: Option<QuotedMessage>,
}

impl OutgoingMessage {
    /// An HTML message.
    #[must_use]
    pub fn html(content: impl Into<String>) -> Self {
        Self { body: ChatBody { content_type: BodyType::Html, content: Sensitive::new(content) }, ..Self::default() }
    }

    /// A plain-text message.
    #[must_use]
    pub fn text(content: impl Into<String>) -> Self {
        Self { body: ChatBody { content_type: BodyType::Text, content: Sensitive::new(content) }, ..Self::default() }
    }
}

fn config(detail: &'static str) -> Error {
    Error::new(ErrorKind::Config, detail)
}

fn mentions_json(body: &str, mentions: &[OutgoingMention]) -> Result<Vec<Value>> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(mentions.len());
    for m in mentions {
        if !seen.insert(m.id) {
            return Err(config("mention ids must be unique"));
        }
        if !body.contains(&format!("<at id=\"{}\">", m.id)) {
            return Err(config("every mention needs a matching <at id=\"N\"> element in the body"));
        }
        let mentioned = match &m.target {
            MentionTarget::User { id, display_name } => json!({"user": {
                "id": id.as_str(), "displayName": display_name.as_str(), "userIdentityType": "aadUser"
            }}),
            MentionTarget::Channel { id, display_name } => json!({"conversation": {
                "id": id.as_str(), "displayName": display_name.as_str(), "conversationIdentityType": "channel"
            }}),
            MentionTarget::Team { id, display_name } => json!({"conversation": {
                "id": id.as_str(), "displayName": display_name.as_str(), "conversationIdentityType": "team"
            }}),
            MentionTarget::Tag { id, display_name } => {
                opaque(id, "tag id")?;
                json!({"tag": {"id": id, "displayName": display_name.as_str()}})
            }
        };
        out.push(json!({"id": m.id, "mentionText": m.text.as_str(), "mentioned": mentioned}));
    }
    Ok(out)
}

fn body_json(body: &ChatBody) -> Value {
    json!({"contentType": body.content_type.as_graph(), "content": body.content.as_str()})
}

/// Builds the JSON body Graph expects for sending `msg` (validating mentions, files and images).
///
/// # Errors
/// [`ErrorKind::Config`] for an empty or oversized body, rich content in a text body, duplicate or
/// unreferenced mentions, bad image ids, types or sizes, or a file reference without an https URL.
pub fn message_payload(msg: &OutgoingMessage) -> Result<Value> {
    let rich = !msg.mentions.is_empty() || !msg.files.is_empty() || !msg.images.is_empty() || msg.quote.is_some();
    if rich && msg.body.content_type != BodyType::Html {
        return Err(config("mentions, files, images and quotes need an HTML body"));
    }
    let mut content = msg.body.content.as_str().to_owned();
    let mut attachments = Vec::new();
    for f in &msg.files {
        if f.attachment_id.is_empty()
            || f.attachment_id.len() > 64
            || !f.attachment_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(config("file attachment id must be a GUID"));
        }
        if web_url(Some(f.content_url.clone())).is_none() {
            return Err(config("file attachment URL must be https"));
        }
        let tag = format!("<attachment id=\"{}\"></attachment>", f.attachment_id);
        if !content.contains(&format!("<attachment id=\"{}\">", f.attachment_id)) {
            content.push_str(&tag);
        }
        attachments.push(json!({
            "id": f.attachment_id, "contentType": "reference", "contentUrl": f.content_url, "name": f.name.as_str()
        }));
    }
    if let Some(q) = &msg.quote {
        let id = q.message_id.as_str();
        if !content.contains(&format!("<attachment id=\"{id}\">")) {
            content = format!("<attachment id=\"{id}\"></attachment>{content}");
        }
        let sender = q.sender.as_ref().map(|(uid, name)| {
            json!({"user": {"id": uid.as_str(), "displayName": name.as_str(), "userIdentityType": "aadUser"}})
        });
        let preview: String = q.preview.as_str().chars().take(200).collect();
        let reference = json!({"messageId": id, "messagePreview": preview, "messageSender": sender});
        attachments.push(json!({"id": id, "contentType": "messageReference", "content": reference.to_string()}));
    }
    let mut hosted = Vec::new();
    let mut temp_ids = std::collections::HashSet::new();
    for img in &msg.images {
        let id_ok = !img.temporary_id.is_empty()
            && img.temporary_id.len() <= 64
            && img.temporary_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !id_ok || !temp_ids.insert(img.temporary_id.as_str()) {
            return Err(config("hosted image ids must be unique, short and alphanumeric"));
        }
        if !img.content_type.starts_with("image/") || img.content_type.len() > 128 {
            return Err(config("hosted content must be an image/* type"));
        }
        if img.bytes.is_empty() || img.bytes.len() > MAX_HOSTED_IMAGE_BYTES {
            return Err(config("hosted image must be 1 byte to 4 MiB"));
        }
        if !content.contains(&format!("../hostedContents/{}/$value", img.temporary_id)) {
            return Err(config("hosted image is not referenced from the body"));
        }
        hosted.push(json!({
            "@microsoft.graph.temporaryId": img.temporary_id,
            "contentBytes": base64(&img.bytes),
            "contentType": img.content_type,
        }));
    }
    if content.trim().is_empty() {
        return Err(config("message body is empty"));
    }
    if content.len() > MAX_OUTGOING_BODY_BYTES {
        return Err(config("message body is too large"));
    }
    let mentions = mentions_json(&content, &msg.mentions)?;
    let mut obj = Map::new();
    obj.insert("body".into(), json!({"contentType": msg.body.content_type.as_graph(), "content": content}));
    if let Some(subject) = &msg.subject {
        obj.insert("subject".into(), Value::String(cap(subject.as_str().to_owned(), MAX_TITLE_CHARS)));
    }
    if msg.importance != ChatImportance::Normal {
        obj.insert("importance".into(), Value::String(msg.importance.as_graph().to_owned()));
    }
    if !mentions.is_empty() {
        obj.insert("mentions".into(), Value::Array(mentions));
    }
    if !attachments.is_empty() {
        obj.insert("attachments".into(), Value::Array(attachments));
    }
    if !hosted.is_empty() {
        obj.insert("hostedContents".into(), Value::Array(hosted));
    }
    Ok(Value::Object(obj))
}

// ---------------------------------------------------------------------------------------------
// Message references shared by chats and channels.
// ---------------------------------------------------------------------------------------------

/// A message in a chat or channel, for actions that work on both (edit, delete, reactions).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MessageRef {
    /// A chat message.
    Chat {
        /// The chat.
        chat: ChatId,
        /// The message.
        message: ChatMessageId,
    },
    /// A top-level channel post.
    ChannelPost {
        /// The team.
        team: TeamId,
        /// The channel.
        channel: ChannelId,
        /// The post.
        message: ChatMessageId,
    },
    /// A reply to a channel post.
    ChannelReply {
        /// The team.
        team: TeamId,
        /// The channel.
        channel: ChannelId,
        /// The parent post.
        message: ChatMessageId,
        /// The reply.
        reply: ChatMessageId,
    },
}

impl MessageRef {
    /// The message's path, e.g. `/chats/{id}/messages/{id}`.
    #[must_use]
    pub fn path(&self) -> String {
        match self {
            Self::Chat { chat, message } => format!("/chats/{}/messages/{}", seg(chat.as_str()), seg(message.as_str())),
            Self::ChannelPost { team, channel, message } => format!(
                "/teams/{}/channels/{}/messages/{}",
                seg(team.as_str()),
                seg(channel.as_str()),
                seg(message.as_str())
            ),
            Self::ChannelReply { team, channel, message, reply } => format!(
                "/teams/{}/channels/{}/messages/{}/replies/{}",
                seg(team.as_str()),
                seg(channel.as_str()),
                seg(message.as_str()),
                seg(reply.as_str())
            ),
        }
    }

    /// The container the message lives in.
    #[must_use]
    pub fn container(&self) -> MessageContainer {
        match self {
            Self::Chat { chat, .. } => MessageContainer::Chat(chat.clone()),
            Self::ChannelPost { team, channel, .. } | Self::ChannelReply { team, channel, .. } => {
                MessageContainer::Channel { team_id: team.clone(), channel_id: channel.clone() }
            }
        }
    }

    /// Soft delete lives under `/users/{id}/chats/...` for chats, so `/me` is prefixed there.
    fn action_path(&self, action: &str) -> String {
        match self {
            Self::Chat { .. } => format!("/me{}/{action}", self.path()),
            _ => format!("{}/{action}", self.path()),
        }
    }
}

/// `PATCH` a message's body (and mentions). Returns 204.
///
/// # Errors
/// [`ErrorKind::Config`] for an empty body or invalid mentions.
pub fn edit_message_request(target: &MessageRef, body: &ChatBody, mentions: &[OutgoingMention]) -> Result<Request> {
    if body.content.as_str().trim().is_empty() {
        return Err(config("message body is empty"));
    }
    if body.content.len() > MAX_OUTGOING_BODY_BYTES {
        return Err(config("message body is too large"));
    }
    if !mentions.is_empty() && body.content_type != BodyType::Html {
        return Err(config("mentions need an HTML body"));
    }
    let mut obj = Map::new();
    obj.insert("body".into(), body_json(body));
    let m = mentions_json(body.content.as_str(), mentions)?;
    if !m.is_empty() {
        obj.insert("mentions".into(), Value::Array(m));
    }
    Ok(teams(Request::patch(target.path()).json_value(Value::Object(obj))))
}

/// `POST .../softDelete`: deletes a message the user sent (it can be restored).
#[must_use]
pub fn soft_delete_message_request(target: &MessageRef) -> Request {
    teams(Request::post(target.action_path("softDelete")))
}

/// `POST .../undoSoftDelete`: restores a soft-deleted message.
#[must_use]
pub fn undo_soft_delete_message_request(target: &MessageRef) -> Request {
    teams(Request::post(target.action_path("undoSoftDelete")))
}

fn reaction_body(reaction: &str) -> Result<Value> {
    let r = reaction.trim();
    if r.is_empty() || r.len() > 64 || r.chars().any(char::is_control) {
        return Err(config("reaction must be a short emoji or reaction name"));
    }
    Ok(json!({"reactionType": r}))
}

/// `POST .../setReaction` with a unicode emoji (or a legacy name such as `like`).
///
/// # Errors
/// [`ErrorKind::Config`] for an empty, long or control-character reaction.
pub fn set_reaction_request(target: &MessageRef, reaction: &str) -> Result<Request> {
    Ok(teams(Request::post(format!("{}/setReaction", target.path())).json_value(reaction_body(reaction)?)))
}

/// `POST .../unsetReaction`.
///
/// # Errors
/// As [`set_reaction_request`].
pub fn unset_reaction_request(target: &MessageRef, reaction: &str) -> Result<Request> {
    Ok(teams(Request::post(format!("{}/unsetReaction", target.path())).json_value(reaction_body(reaction)?)))
}

// ---------------------------------------------------------------------------------------------
// Chat request builders.
// ---------------------------------------------------------------------------------------------

fn chat_path(chat: &ChatId) -> String {
    format!("/chats/{}", seg(chat.as_str()))
}

/// `GET /me/chats` with members and the last message preview expanded, newest activity first.
#[must_use]
pub fn list_chats_request(top: u32) -> Request {
    teams(
        Request::get("/me/chats")
            .query("$expand", "members,lastMessagePreview")
            .query("$orderby", "lastMessagePreview/createdDateTime desc")
            .query("$select", SELECT_CHAT)
            .query("$top", clamp_top(top).to_string()),
    )
}

/// `GET /chats/{id}` with members and the last message preview.
#[must_use]
pub fn get_chat_request(chat: &ChatId) -> Request {
    teams(Request::get(chat_path(chat)).query("$expand", "members,lastMessagePreview").query("$select", SELECT_CHAT))
}

fn member_binding(base: &str, user: &UserId, roles: &[&str]) -> Value {
    json!({
        "@odata.type": "#microsoft.graph.aadUserConversationMember",
        "roles": roles,
        "user@odata.bind": format!("{}/users('{}')", base_of(base), seg(user.as_str())),
    })
}

/// `POST /chats` creating a one-on-one (exactly two members, including the signed-in user) or
/// group chat (two or more members). `base` is the client's Graph base URL, used for the
/// `user@odata.bind` links.
///
/// # Errors
/// [`ErrorKind::Config`] for another chat type, a wrong member count, duplicate members or a topic
/// on a one-on-one chat.
pub fn create_chat_request(
    base: &str,
    chat_type: &ChatType,
    members: &[UserId],
    topic: Option<&str>,
) -> Result<Request> {
    let unique: std::collections::HashSet<&str> = members.iter().map(UserId::as_str).collect();
    if unique.len() != members.len() {
        return Err(config("chat members must be unique"));
    }
    match chat_type {
        ChatType::OneOnOne if members.len() != 2 => return Err(config("a one-on-one chat has exactly two members")),
        ChatType::OneOnOne if topic.is_some() => return Err(config("one-on-one chats have no topic")),
        ChatType::OneOnOne => {}
        ChatType::Group if members.len() < 2 || members.len() > 250 => {
            return Err(config("a group chat has 2 to 250 members"));
        }
        ChatType::Group => {}
        _ => return Err(config("only one-on-one and group chats can be created")),
    }
    let members: Vec<Value> = members.iter().map(|m| member_binding(base, m, &["owner"])).collect();
    let mut body = json!({"chatType": chat_type.as_graph(), "members": members});
    if let (Some(t), Some(obj)) = (topic, body.as_object_mut()) {
        let t = t.trim();
        if t.is_empty() || t.chars().any(char::is_control) {
            return Err(config("chat topic must be non-empty text"));
        }
        obj.insert("topic".into(), Value::String(cap(t.to_owned(), 250)));
    }
    Ok(teams(Request::post("/chats").json_value(body)))
}

/// `GET /chats/{id}/members`.
#[must_use]
pub fn list_chat_members_request(chat: &ChatId) -> Request {
    teams(Request::get(format!("{}/members", chat_path(chat))))
}

/// `POST /chats/{id}/members` adding `user`; `history_from` is the oldest message they can see
/// (`None`: no history; a very old instant shares all of it).
#[must_use]
pub fn add_chat_member_request(base: &str, chat: &ChatId, user: &UserId, history_from: Option<Timestamp>) -> Request {
    let mut body = member_binding(base, user, &["owner"]);
    if let (Some(t), Some(obj)) = (history_from, body.as_object_mut()) {
        obj.insert("visibleHistoryStartDateTime".into(), Value::String(t.to_string()));
    }
    teams(Request::post(format!("{}/members", chat_path(chat))).json_value(body))
}

/// `DELETE /chats/{id}/members/{membershipId}`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid membership id.
pub fn remove_chat_member_request(chat: &ChatId, membership_id: &str) -> Result<Request> {
    let id = opaque(membership_id, "membership id")?;
    Ok(teams(Request::delete(format!("{}/members/{}", chat_path(chat), seg(id)))))
}

/// `GET /chats/{id}/messages`, most recently modified first; with `modified_after`, only messages
/// changed since then (`$filter=lastModifiedDateTime gt ...`), for incremental polling.
#[must_use]
pub fn list_chat_messages_request(chat: &ChatId, top: u32, modified_after: Option<Timestamp>) -> Request {
    let mut req = Request::get(format!("{}/messages", chat_path(chat)))
        .query("$top", clamp_top(top).to_string())
        .query("$orderby", "lastModifiedDateTime desc");
    if let Some(t) = modified_after {
        req = req.query("$filter", format!("lastModifiedDateTime gt {t}"));
    }
    teams(req)
}

/// `GET /chats/{id}/messages/{id}`.
#[must_use]
pub fn get_chat_message_request(chat: &ChatId, message: &ChatMessageId) -> Request {
    teams(Request::get(MessageRef::Chat { chat: chat.clone(), message: message.clone() }.path()))
}

/// `POST /chats/{id}/messages`.
///
/// # Errors
/// As [`message_payload`].
pub fn send_chat_message_request(chat: &ChatId, msg: &OutgoingMessage) -> Result<Request> {
    Ok(teams(Request::post(format!("{}/messages", chat_path(chat))).json_value(message_payload(msg)?)))
}

fn tenant_user(user: &UserId, tenant_id: &str) -> Result<Value> {
    let tenant = opaque(tenant_id, "tenant id")?;
    Ok(json!({"id": user.as_str(), "tenantId": tenant}))
}

/// `POST /chats/{id}/markChatReadForUser`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid tenant id.
pub fn mark_chat_read_request(chat: &ChatId, user: &UserId, tenant_id: &str) -> Result<Request> {
    let body = json!({"user": tenant_user(user, tenant_id)?});
    Ok(teams(Request::post(format!("{}/markChatReadForUser", chat_path(chat))).json_value(body)))
}

/// `POST /chats/{id}/markChatUnreadForUser`, unread from `last_read` on.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid tenant id.
pub fn mark_chat_unread_request(
    chat: &ChatId,
    user: &UserId,
    tenant_id: &str,
    last_read: Timestamp,
) -> Result<Request> {
    let body = json!({"user": tenant_user(user, tenant_id)?, "lastMessageReadDateTime": last_read.to_string()});
    Ok(teams(Request::post(format!("{}/markChatUnreadForUser", chat_path(chat))).json_value(body)))
}

/// `GET /chats/{id}/messages/{id}/hostedContents/{id}/$value` (inline image bytes).
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid hosted content id.
pub fn hosted_content_request(chat: &ChatId, message: &ChatMessageId, hosted_content_id: &str) -> Result<Request> {
    let id = opaque(hosted_content_id, "hosted content id")?;
    let path = format!(
        "{}/hostedContents/{}/$value",
        MessageRef::Chat { chat: chat.clone(), message: message.clone() }.path(),
        seg(id)
    );
    Ok(teams(Request::get(path).header("Accept", "*/*")))
}

/// `GET /chats/{id}/pinnedMessages?$expand=message`.
#[must_use]
pub fn list_pinned_messages_request(chat: &ChatId) -> Request {
    teams(Request::get(format!("{}/pinnedMessages", chat_path(chat))).query("$expand", "message"))
}

/// `POST /chats/{id}/pinnedMessages` binding the message by URL (`base` is the Graph base URL).
#[must_use]
pub fn pin_message_request(base: &str, chat: &ChatId, message: &ChatMessageId) -> Request {
    let bind = format!("{}{}", base_of(base), MessageRef::Chat { chat: chat.clone(), message: message.clone() }.path());
    teams(Request::post(format!("{}/pinnedMessages", chat_path(chat))).json_value(json!({"message@odata.bind": bind})))
}

/// `DELETE /chats/{id}/pinnedMessages/{pinnedId}`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid pinned id.
pub fn unpin_message_request(chat: &ChatId, pinned_id: &str) -> Result<Request> {
    let id = opaque(pinned_id, "pinned message id")?;
    Ok(teams(Request::delete(format!("{}/pinnedMessages/{}", chat_path(chat), seg(id)))))
}

/// A pinned chat message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedMessage {
    /// The pin's id (used to unpin).
    pub pinned_id: String,
    /// The message, when expanded and valid.
    pub message: Option<ChatMessage>,
}

/// Inline image or other hosted content bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct HostedContent {
    /// The `Content-Type` Graph sent (untrusted).
    pub content_type: Option<String>,
    /// The bytes.
    pub bytes: Vec<u8>,
}

impl fmt::Debug for HostedContent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostedContent")
            .field("content_type", &self.content_type)
            .field("len", &self.bytes.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------------------------
// Typed calls.
// ---------------------------------------------------------------------------------------------

impl Client {
    /// Sends `req` and maps the single JSON object with `f`.
    pub(crate) async fn send_mapped<M>(
        &self,
        req: Request,
        what: &'static str,
        f: impl FnOnce(Value) -> Option<M>,
    ) -> Result<M> {
        let v: Value = self.send_json(req).await?;
        f(v).ok_or_else(|| unmapped(what))
    }

    /// Sends `req` and discards the (usually empty) response.
    pub(crate) async fn send_unit(&self, req: Request) -> Result<()> {
        self.send_raw(req).await.map(|_| ())
    }

    /// The first page of the user's chats (see [`list_chats_request`]).
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_chats(&self, top: u32) -> Result<Page<Chat>> {
        Ok(map_page(self.page(list_chats_request(top)).await?, parse_chat))
    }

    /// The next page of chats from a `nextLink`.
    ///
    /// # Errors
    /// As [`Client::page`]; `Malformed` for a link off the Graph host.
    pub async fn list_chats_next(&self, next_link: &str) -> Result<Page<Chat>> {
        Ok(map_page(self.page(teams(Request::link(next_link))).await?, parse_chat))
    }

    /// One chat.
    ///
    /// # Errors
    /// As [`Client::send_json`]; `Malformed` if the chat does not map.
    pub async fn get_chat(&self, chat: &ChatId) -> Result<Chat> {
        self.send_mapped(get_chat_request(chat), "chat", parse_chat).await
    }

    /// Creates a chat (see [`create_chat_request`]).
    ///
    /// # Errors
    /// As [`create_chat_request`] and [`Client::send_json`].
    pub async fn create_chat(&self, chat_type: &ChatType, members: &[UserId], topic: Option<&str>) -> Result<Chat> {
        let req = create_chat_request(self.base_url().as_str(), chat_type, members, topic)?;
        self.send_mapped(req, "chat", parse_chat).await
    }

    /// The members of a chat.
    ///
    /// # Errors
    /// As [`Client::collect_all`].
    pub async fn list_chat_members(&self, chat: &ChatId) -> Result<Vec<ConversationMember>> {
        let raw: Vec<Value> = self.collect_all(list_chat_members_request(chat), 20).await?;
        Ok(raw.into_iter().filter_map(parse_conversation_member).collect())
    }

    /// Adds a member to a group chat.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn add_chat_member(
        &self,
        chat: &ChatId,
        user: &UserId,
        history_from: Option<Timestamp>,
    ) -> Result<ConversationMember> {
        let req = add_chat_member_request(self.base_url().as_str(), chat, user, history_from);
        self.send_mapped(req, "conversation member", parse_conversation_member).await
    }

    /// Removes a member from a group chat.
    ///
    /// # Errors
    /// As [`remove_chat_member_request`] and [`Client::send_raw`].
    pub async fn remove_chat_member(&self, chat: &ChatId, membership_id: &str) -> Result<()> {
        self.send_unit(remove_chat_member_request(chat, membership_id)?).await
    }

    /// The first page of a chat's messages (see [`list_chat_messages_request`]).
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_chat_messages(
        &self,
        chat: &ChatId,
        top: u32,
        modified_after: Option<Timestamp>,
    ) -> Result<Page<ChatMessage>> {
        let container = MessageContainer::Chat(chat.clone());
        let page = self.page(list_chat_messages_request(chat, top, modified_after)).await?;
        Ok(map_page(page, |v| parse_chat_message(v, Some(&container))))
    }

    /// The next page of a chat's messages from a `nextLink`.
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_chat_messages_next(&self, chat: &ChatId, next_link: &str) -> Result<Page<ChatMessage>> {
        let container = MessageContainer::Chat(chat.clone());
        let page = self.page(teams(Request::link(next_link))).await?;
        Ok(map_page(page, |v| parse_chat_message(v, Some(&container))))
    }

    /// Every message of a chat modified after `since`, following pages up to `max_pages`
    /// (incremental poll for chats whose preview changed).
    ///
    /// # Errors
    /// As [`Client::collect_all`].
    pub async fn chat_messages_since(
        &self,
        chat: &ChatId,
        since: Timestamp,
        max_pages: usize,
    ) -> Result<Vec<ChatMessage>> {
        let container = MessageContainer::Chat(chat.clone());
        let req = list_chat_messages_request(chat, MAX_CHAT_PAGE_SIZE, Some(since));
        let raw: Vec<Value> = self.collect_all(req, max_pages).await?;
        Ok(raw.into_iter().filter_map(|v| parse_chat_message(v, Some(&container))).collect())
    }

    /// One chat message.
    ///
    /// # Errors
    /// As [`Client::send_json`]; `Malformed` if the message does not map.
    pub async fn get_chat_message(&self, chat: &ChatId, message: &ChatMessageId) -> Result<ChatMessage> {
        let container = MessageContainer::Chat(chat.clone());
        self.send_mapped(get_chat_message_request(chat, message), "chat message", |v| {
            parse_chat_message(v, Some(&container))
        })
        .await
    }

    /// Sends a chat message and returns it as created.
    ///
    /// # Errors
    /// As [`message_payload`] and [`Client::send_json`].
    pub async fn send_chat_message(&self, chat: &ChatId, msg: &OutgoingMessage) -> Result<ChatMessage> {
        let container = MessageContainer::Chat(chat.clone());
        self.send_mapped(send_chat_message_request(chat, msg)?, "chat message", |v| {
            parse_chat_message(v, Some(&container))
        })
        .await
    }

    /// Edits a chat message, channel post or reply.
    ///
    /// # Errors
    /// As [`edit_message_request`] and [`Client::send_raw`].
    pub async fn edit_message(&self, target: &MessageRef, body: &ChatBody, mentions: &[OutgoingMention]) -> Result<()> {
        self.send_unit(edit_message_request(target, body, mentions)?).await
    }

    /// Soft-deletes a message the user sent.
    ///
    /// # Errors
    /// As [`Client::send_raw`].
    pub async fn soft_delete_message(&self, target: &MessageRef) -> Result<()> {
        self.send_unit(soft_delete_message_request(target)).await
    }

    /// Restores a soft-deleted message.
    ///
    /// # Errors
    /// As [`Client::send_raw`].
    pub async fn undo_soft_delete_message(&self, target: &MessageRef) -> Result<()> {
        self.send_unit(undo_soft_delete_message_request(target)).await
    }

    /// Adds the user's reaction.
    ///
    /// # Errors
    /// As [`set_reaction_request`] and [`Client::send_raw`].
    pub async fn set_reaction(&self, target: &MessageRef, reaction: &str) -> Result<()> {
        self.send_unit(set_reaction_request(target, reaction)?).await
    }

    /// Removes the user's reaction.
    ///
    /// # Errors
    /// As [`unset_reaction_request`] and [`Client::send_raw`].
    pub async fn unset_reaction(&self, target: &MessageRef, reaction: &str) -> Result<()> {
        self.send_unit(unset_reaction_request(target, reaction)?).await
    }

    /// Marks a chat read for the user.
    ///
    /// # Errors
    /// As [`mark_chat_read_request`] and [`Client::send_raw`].
    pub async fn mark_chat_read(&self, chat: &ChatId, user: &UserId, tenant_id: &str) -> Result<()> {
        self.send_unit(mark_chat_read_request(chat, user, tenant_id)?).await
    }

    /// Marks a chat unread from `last_read` on.
    ///
    /// # Errors
    /// As [`mark_chat_unread_request`] and [`Client::send_raw`].
    pub async fn mark_chat_unread(
        &self,
        chat: &ChatId,
        user: &UserId,
        tenant_id: &str,
        last_read: Timestamp,
    ) -> Result<()> {
        self.send_unit(mark_chat_unread_request(chat, user, tenant_id, last_read)?).await
    }

    /// The bytes of a hosted content (inline image).
    ///
    /// # Errors
    /// As [`hosted_content_request`] and [`Client::send_raw`].
    pub async fn get_hosted_content(
        &self,
        chat: &ChatId,
        message: &ChatMessageId,
        hosted_content_id: &str,
    ) -> Result<HostedContent> {
        let resp = self.send_raw(hosted_content_request(chat, message, hosted_content_id)?).await?;
        Ok(HostedContent { content_type: resp.header("content-type").map(str::to_owned), bytes: resp.body })
    }

    /// The bytes behind a hosted-content URL found in a message body (`.../hostedContents/{id}/$value`).
    /// The URL must be on the Graph host; anything else is rejected before any I/O.
    ///
    /// # Errors
    /// `Malformed` for a URL off the Graph host or not naming hosted content; as [`Client::send_raw`].
    pub async fn get_hosted_content_by_url(&self, url: &str) -> Result<HostedContent> {
        let path = url.split(['?', '#']).next().unwrap_or("");
        if !path.contains("/hostedContents/") || !path.ends_with("/$value") {
            return Err(Error::new(ErrorKind::Malformed, "not a hosted content URL"));
        }
        let resp = self.send_raw(teams(Request::link(url).header("Accept", "*/*"))).await?;
        Ok(HostedContent { content_type: resp.header("content-type").map(str::to_owned), bytes: resp.body })
    }

    /// The pinned messages of a chat.
    ///
    /// # Errors
    /// As [`Client::collect_all`].
    pub async fn list_pinned_messages(&self, chat: &ChatId) -> Result<Vec<PinnedMessage>> {
        let container = MessageContainer::Chat(chat.clone());
        let raw: Vec<Value> = self.collect_all(list_pinned_messages_request(chat), 5).await?;
        Ok(raw
            .into_iter()
            .filter_map(|mut v| {
                let pinned_id = v.get("id").and_then(Value::as_str).filter(|s| opaque(s, "pin").is_ok())?.to_owned();
                let message =
                    v.get_mut("message").map(Value::take).and_then(|m| parse_chat_message(m, Some(&container)));
                Some(PinnedMessage { pinned_id, message })
            })
            .collect())
    }

    /// Pins a message; returns the pin id.
    ///
    /// # Errors
    /// As [`Client::send_json`]; `Malformed` without a pin id.
    pub async fn pin_message(&self, chat: &ChatId, message: &ChatMessageId) -> Result<String> {
        let req = pin_message_request(self.base_url().as_str(), chat, message);
        self.send_mapped(req, "pinned message", |v| {
            v.get("id").and_then(Value::as_str).filter(|s| opaque(s, "pin").is_ok()).map(str::to_owned)
        })
        .await
    }

    /// Unpins a message.
    ///
    /// # Errors
    /// As [`unpin_message_request`] and [`Client::send_raw`].
    pub async fn unpin_message(&self, chat: &ChatId, pinned_id: &str) -> Result<()> {
        self.send_unit(unpin_message_request(chat, pinned_id)?).await
    }
}
