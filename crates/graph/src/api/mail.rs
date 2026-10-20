//! Outlook mail: folders, messages, delta sync, attachments, compose and send, mailbox settings,
//! rules, categories, Focused Inbox overrides, and shared or delegated mailboxes.
//!
//! Every builder (`*_request`) returns a [`Request`] without I/O; the matching `async` method on
//! [`Client`] (e.g. [`Client::list_messages`]) sends it and maps Graph's JSON to `quorum-model`
//! types. Collections come back as a [`Page`] (follow `next_link` with [`Request::follow`] on the
//! original request so the `Prefer` headers carry over, then pass it to the matching `*_page`
//! method such as [`Client::messages_page`]).
//!
//! Conventions used throughout:
//! - **Immutable ids.** Every mail request carries `Prefer: IdType="ImmutableId"`, so message ids
//!   survive moves between folders (Graph's "immutable identifiers for Outlook resources").
//! - **`$select`** names only the fields the store keeps ([`MESSAGE_SELECT`], [`FOLDER_SELECT`]).
//! - **HTML bodies.** Body reads ask for `outlook.body-content-type="html"`; HTML is untrusted and
//!   must go through `quorum-sanitize` before display.
//! - **Mailbox.** [`Mailbox::Me`] targets `/me/...`; [`Mailbox::User`] targets `/users/{id|upn}/...`
//!   for shared and delegated mailboxes (`Mail.ReadWrite.Shared`). The mailbox is also the
//!   request's concurrency bucket (Exchange allows 4 concurrent requests per mailbox).
//! - **Hostile JSON.** Items that do not map (bad id, wrong types) are skipped and counted in
//!   [`Page::skipped`]; unknown enum values parse to `Unknown(String)`.

use std::fmt;

use jiff::Timestamp;
use quorum_model::{
    Attachment, AttachmentId, AttachmentKind, ConversationId, DateTimeZone, DayOfWeek, EmailAddress, FlagStatus,
    GraphEnum, Importance, InferenceClassification, MailFolder, MailFolderId, Message, MessageBody, MessageId,
    Sensitive, WellKnownFolder,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};

use crate::client::Client;
use crate::error::{Error, ErrorKind, Result};
use crate::paging::{DeltaOutcome, DeltaPage, Page};
use crate::request::{Request, ResourceFamily};
use crate::upload::{ChunkSize, ChunkSource, UploadSession};

// ---------------------------------------------------------------------------------------------
// Shared helpers (also used by the calendar, people and search modules).
// ---------------------------------------------------------------------------------------------

/// `Prefer` value asking Graph for immutable Outlook ids.
pub const PREFER_IMMUTABLE_ID: &str = "IdType=\"ImmutableId\"";
/// `Prefer` value asking for HTML bodies.
pub const PREFER_HTML_BODY: &str = "outlook.body-content-type=\"html\"";
/// Default page size for mail collections (`$top` / `odata.maxpagesize`).
pub const DEFAULT_PAGE_SIZE: u32 = 50;
/// Largest `$top` accepted by Outlook collections.
pub const MAX_PAGE_SIZE: u32 = 1000;
/// Largest file attachment sent inline as base64 (`POST .../attachments`, `sendMail`): 3 MiB.
/// Bigger files go through [`Client::create_attachment_upload_session`].
pub const MAX_INLINE_ATTACHMENT_BYTES: usize = 3 * 1024 * 1024;
/// Largest attachment an Outlook upload session accepts (150 MiB).
pub const MAX_ATTACHMENT_BYTES: u64 = 150 * 1024 * 1024;
/// Upload-session hosts used by Outlook attachment uploads. Add these to
/// [`ClientBuilder::upload_host_suffixes`](crate::ClientBuilder::upload_host_suffixes) (together
/// with the defaults) to allow [`Client::upload_attachment`]; the core default list only covers
/// OneDrive and SharePoint.
pub const MAIL_UPLOAD_HOST_SUFFIXES: &[&str] = &["outlook.office.com", "outlook.office365.com"];
/// Outlook upload sessions take chunks under 4 MiB: 12 × 320 KiB = 3.75 MiB.
pub const MAIL_UPLOAD_CHUNK_UNITS: usize = 12;
/// Longest single id or name accepted in a path segment.
const MAX_SEGMENT_LEN: usize = 2048;

/// Fields of a message the store keeps (list, get and delta use the same set).
pub const MESSAGE_SELECT: &str = "id,conversationId,parentFolderId,subject,from,sender,toRecipients,ccRecipients,\
bccRecipients,replyTo,receivedDateTime,sentDateTime,bodyPreview,isRead,isDraft,importance,flag,categories,\
hasAttachments,inferenceClassification,webLink";
/// Fields of a mail folder the store keeps.
pub const FOLDER_SELECT: &str =
    "id,displayName,parentFolderId,childFolderCount,unreadItemCount,totalItemCount,isHidden";
/// Attachment metadata fields (never `contentBytes`).
pub const ATTACHMENT_SELECT: &str = "id,name,contentType,size,isInline,lastModifiedDateTime";

pub(crate) fn config(detail: &'static str) -> Error {
    Error::new(ErrorKind::Config, detail)
}

/// Percent-encodes one path segment with [`super::chat::seg`], first rejecting empty, dot and
/// oversized segments: ids come from Graph (hostile), and a `..` id must never change the path.
pub(crate) fn segment(raw: &str) -> Result<String> {
    if raw.is_empty() || raw == "." || raw == ".." || raw.len() > MAX_SEGMENT_LEN {
        return Err(config("path segment is empty, a dot segment, or too long"));
    }
    Ok(super::chat::seg(raw))
}

/// Adds one `Prefer` header holding all `prefs`, comma-separated (a single header so the
/// preferences survive `$batch`, whose headers are a JSON object).
pub(crate) fn with_prefer(req: Request, prefs: &[&str]) -> Request {
    if prefs.is_empty() { req } else { req.prefer(prefs.join(", ")) }
}

/// Clamps a page size to `1..=max`.
pub(crate) fn clamp_top(top: Option<u32>, default: u32, max: u32) -> u32 {
    top.unwrap_or(default).clamp(1, max)
}

/// Deserialises `null` as `T::default()`.
pub(crate) fn null_default<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// Maps raw JSON items with `f`; items `f` rejects are counted as skipped.
pub(crate) fn map_items<T>(values: Vec<Value>, mut f: impl FnMut(Value) -> Option<T>) -> (Vec<T>, usize) {
    let mut items = Vec::with_capacity(values.len());
    let mut skipped = 0usize;
    for v in values {
        match f(v) {
            Some(item) => items.push(item),
            None => skipped = skipped.saturating_add(1),
        }
    }
    if skipped > 0 {
        tracing::warn!(name: "graph.api.items_skipped", skipped, "items failed to map to model types");
    }
    (items, skipped)
}

/// Sends `req` as a collection request and maps each item with `f`.
pub(crate) async fn mapped_page<T>(
    client: &Client,
    req: Request,
    f: impl FnMut(Value) -> Option<T>,
) -> Result<Page<T>> {
    Ok(super::chat::map_page(client.page::<Value>(req).await?, f))
}

/// Sends `req` as a delta request and maps each item with `f`.
pub(crate) async fn mapped_delta<T>(
    client: &Client,
    req: Request,
    f: impl FnMut(Value) -> Option<T>,
) -> Result<DeltaPage<T>> {
    let page = client.delta_page::<Value>(req).await?;
    let (items, skipped) = map_items(page.items, f);
    Ok(DeltaPage { items, skipped: skipped.saturating_add(page.skipped), outcome: page.outcome })
}

/// Whether a delta item is an `@removed` tombstone.
pub(crate) fn is_removed(v: &Value) -> bool {
    v.get("@removed").is_some()
}

/// Parses the `id` of a tombstone.
pub(crate) fn removed_id<I: for<'de> Deserialize<'de>>(v: &Value) -> Option<I> {
    serde_json::from_value(v.get("id")?.clone()).ok()
}

/// Graph's `recipient` resource: `{"emailAddress": {"name", "address"}}`.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Recipient {
    #[serde(default, deserialize_with = "null_default")]
    pub(crate) email_address: EmailAddress,
}

/// Serialises an address as a Graph `recipient`. An address is required.
pub(crate) fn recipient_json(addr: &EmailAddress) -> Result<Value> {
    let Some(address) = &addr.address else {
        return Err(config("recipient has no email address"));
    };
    let mut email = Map::new();
    if !addr.name.is_empty() {
        email.insert("name".into(), Value::String(addr.name.as_str().to_owned()));
    }
    email.insert("address".into(), Value::String(address.as_str().to_owned()));
    Ok(json!({ "emailAddress": Value::Object(email) }))
}

pub(crate) fn recipients_json(list: &[EmailAddress]) -> Result<Value> {
    list.iter().map(recipient_json).collect::<Result<Vec<_>>>().map(Value::Array)
}

/// Defines a Graph string enum whose unknown values parse to `Unknown(String)` (like the
/// enums in `quorum-model`).
macro_rules! api_enum {
    (
        $(#[$meta:meta])*
        $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident = $wire:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        ///
        /// Unknown values from Graph are kept in `Unknown` instead of failing the parse.
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum $name {
            $( $(#[$vmeta])* $variant, )+
            /// A value this version does not know (kept verbatim, truncated to 256 bytes).
            Unknown(String),
        }

        impl quorum_model::GraphEnum for $name {
            fn from_graph(value: &str) -> Self {
                $( if value.eq_ignore_ascii_case($wire) { return Self::$variant; } )+
                let mut end = value.len().min(256);
                while end > 0 && !value.is_char_boundary(end) {
                    end -= 1;
                }
                Self::Unknown(value.get(..end).unwrap_or("").to_owned())
            }

            fn as_graph(&self) -> &str {
                match self {
                    $( Self::$variant => $wire, )+
                    Self::Unknown(raw) => raw,
                }
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                s.serialize_str(quorum_model::GraphEnum::as_graph(self))
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                let raw = <std::borrow::Cow<'de, str> as serde::Deserialize>::deserialize(d)?;
                Ok(<Self as quorum_model::GraphEnum>::from_graph(&raw))
            }
        }
    };
}

/// A JSON value from Graph that may hold personal data (rule conditions, search resources).
/// `Debug` prints only its kind.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SensitiveJson(pub Value);

impl fmt::Debug for SensitiveJson {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match &self.0 {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        write!(f, "SensitiveJson(<{kind}>)")
    }
}

/// One change from a delta query: an added or updated item, or a removed id.
#[derive(Debug, Clone, PartialEq)]
pub enum Change<T, I> {
    /// The item was created or changed (full item as selected).
    Upsert(T),
    /// The item was deleted or moved out of the synced scope (`@removed`).
    Removed(I),
}

// ---------------------------------------------------------------------------------------------
// Mailbox and folder addressing.
// ---------------------------------------------------------------------------------------------

/// Whose mailbox (or calendar, or contacts) a request targets.
#[derive(Clone, PartialEq, Eq, Hash, Default)]
pub enum Mailbox {
    /// The signed-in user (`/me`).
    #[default]
    Me,
    /// Another user's or a shared mailbox, by object id or UPN (`/users/{id|upn}`); needs the
    /// `*.Shared` scopes and the owner's delegation.
    User(String),
}

impl fmt::Debug for Mailbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Me => f.write_str("Me"),
            Self::User(u) => write!(f, "User({} bytes)", u.len()),
        }
    }
}

impl Mailbox {
    /// A shared or delegated mailbox by object id or UPN.
    #[must_use]
    pub fn user(id_or_upn: impl Into<String>) -> Self {
        Self::User(id_or_upn.into())
    }

    /// The path prefix: `/me` or `/users/{encoded id}`.
    ///
    /// # Errors
    /// [`ErrorKind::Config`] for an empty or dot user id.
    pub fn prefix(&self) -> Result<String> {
        match self {
            Self::Me => Ok("/me".to_owned()),
            Self::User(u) => Ok(format!("/users/{}", segment(u.trim())?)),
        }
    }

    /// The concurrency bucket: `"me"` or the lower-cased user id.
    #[must_use]
    pub fn bucket(&self) -> String {
        match self {
            Self::Me => "me".to_owned(),
            Self::User(u) => u.trim().to_ascii_lowercase(),
        }
    }

    /// `prefix + rest` as a request with family and bucket set.
    pub(crate) fn request(
        &self,
        method: crate::request::Method,
        rest: &str,
        family: ResourceFamily,
    ) -> Result<Request> {
        Ok(Request::new(method, format!("{}{rest}", self.prefix()?)).family(family).bucket(self.bucket()))
    }
}

/// A mail folder, by well-known name (`inbox`, `sentitems`, ...) or by id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderRef {
    /// A well-known folder; Graph accepts the name in place of the id.
    WellKnown(WellKnownFolder),
    /// A folder id.
    Id(MailFolderId),
}

impl FolderRef {
    /// The inbox.
    #[must_use]
    pub fn inbox() -> Self {
        Self::WellKnown(WellKnownFolder::Inbox)
    }

    fn encoded(&self) -> Result<String> {
        match self {
            Self::WellKnown(w) => segment(w.as_graph()),
            Self::Id(id) => segment(id.as_str()),
        }
    }
}

impl From<MailFolderId> for FolderRef {
    fn from(id: MailFolderId) -> Self {
        Self::Id(id)
    }
}

impl From<WellKnownFolder> for FolderRef {
    fn from(w: WellKnownFolder) -> Self {
        Self::WellKnown(w)
    }
}

fn mail_get(mailbox: &Mailbox, rest: &str) -> Result<Request> {
    Ok(with_prefer(mailbox.request(crate::request::Method::Get, rest, ResourceFamily::Mail)?, &[PREFER_IMMUTABLE_ID]))
}

fn mail_req(mailbox: &Mailbox, method: crate::request::Method, rest: &str) -> Result<Request> {
    Ok(with_prefer(mailbox.request(method, rest, ResourceFamily::Mail)?, &[PREFER_IMMUTABLE_ID]))
}

fn msg_path(id: &MessageId) -> Result<String> {
    Ok(format!("/messages/{}", segment(id.as_str())?))
}

// ---------------------------------------------------------------------------------------------
// Folders.
// ---------------------------------------------------------------------------------------------

/// Builds `GET {mailbox}/mailFolders` (top-level folders, hidden ones included).
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox.
pub fn list_folders_request(mailbox: &Mailbox) -> Result<Request> {
    Ok(mail_get(mailbox, "/mailFolders")?
        .query("$select", FOLDER_SELECT)
        .query("$top", MAX_PAGE_SIZE.to_string())
        .query("includeHiddenFolders", "true"))
}

/// Builds `GET {mailbox}/mailFolders/{folder}/childFolders`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox or folder id.
pub fn child_folders_request(mailbox: &Mailbox, folder: &FolderRef) -> Result<Request> {
    Ok(mail_get(mailbox, &format!("/mailFolders/{}/childFolders", folder.encoded()?))?
        .query("$select", FOLDER_SELECT)
        .query("$top", MAX_PAGE_SIZE.to_string())
        .query("includeHiddenFolders", "true"))
}

/// Builds `GET {mailbox}/mailFolders/{folder}`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox or folder id.
pub fn get_folder_request(mailbox: &Mailbox, folder: &FolderRef) -> Result<Request> {
    Ok(mail_get(mailbox, &format!("/mailFolders/{}", folder.encoded()?))?.query("$select", FOLDER_SELECT))
}

/// Builds the first `GET {mailbox}/mailFolders/delta` request (the whole folder tree).
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox.
pub fn folders_delta_request(mailbox: &Mailbox) -> Result<Request> {
    Ok(mail_get(mailbox, "/mailFolders/delta")?.query("$select", FOLDER_SELECT))
}

fn map_folder(v: Value) -> Option<MailFolder> {
    serde_json::from_value(v).ok()
}

fn map_folder_change(v: Value) -> Option<Change<MailFolder, MailFolderId>> {
    if is_removed(&v) { removed_id(&v).map(Change::Removed) } else { map_folder(v).map(Change::Upsert) }
}

/// Rebuilds a request for a stored `@odata.nextLink` / `@odata.deltaLink` of a mail delta query
/// with the same `Prefer` headers, family and bucket as the initial request (`maxpagesize` is
/// carried when given).
#[must_use]
pub fn delta_link_request(mailbox: &Mailbox, link: impl Into<String>, page_size: Option<u32>) -> Request {
    let mut prefs = vec![PREFER_IMMUTABLE_ID.to_owned()];
    if let Some(n) = page_size {
        prefs.push(format!("odata.maxpagesize={}", n.clamp(1, MAX_PAGE_SIZE)));
    }
    let refs: Vec<&str> = prefs.iter().map(String::as_str).collect();
    with_prefer(Request::link(link).family(ResourceFamily::Mail).bucket(mailbox.bucket()), &refs)
}

// ---------------------------------------------------------------------------------------------
// Messages.
// ---------------------------------------------------------------------------------------------

/// Options for listing messages in a folder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListOptions {
    /// Page size (`$top`), default [`DEFAULT_PAGE_SIZE`], clamped to 1..=1000.
    pub top: Option<u32>,
    /// An OData `$filter` (e.g. `isRead eq false`). Passed through verbatim.
    pub filter: Option<String>,
    /// `$orderby`, default `receivedDateTime desc`.
    pub order_by: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireFlag {
    #[serde(default, deserialize_with = "null_default")]
    flag_status: FlagStatus,
}

/// Graph's message JSON, before mapping to [`Message`].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireMessage {
    id: MessageId,
    #[serde(default)]
    conversation_id: Option<ConversationId>,
    #[serde(default)]
    parent_folder_id: Option<MailFolderId>,
    #[serde(default)]
    subject: Sensitive,
    #[serde(default)]
    from: Option<Recipient>,
    #[serde(default)]
    sender: Option<Recipient>,
    #[serde(default, deserialize_with = "null_default")]
    to_recipients: Vec<Recipient>,
    #[serde(default, deserialize_with = "null_default")]
    cc_recipients: Vec<Recipient>,
    #[serde(default, deserialize_with = "null_default")]
    bcc_recipients: Vec<Recipient>,
    #[serde(default, deserialize_with = "null_default")]
    reply_to: Vec<Recipient>,
    #[serde(default)]
    received_date_time: Option<Timestamp>,
    #[serde(default)]
    sent_date_time: Option<Timestamp>,
    #[serde(default)]
    body_preview: Sensitive,
    #[serde(default, deserialize_with = "null_default")]
    is_read: bool,
    #[serde(default, deserialize_with = "null_default")]
    is_draft: bool,
    #[serde(default, deserialize_with = "null_default")]
    importance: Importance,
    #[serde(default)]
    flag: Option<WireFlag>,
    #[serde(default, deserialize_with = "null_default")]
    categories: Vec<String>,
    #[serde(default, deserialize_with = "null_default")]
    has_attachments: bool,
    #[serde(default, deserialize_with = "null_default")]
    inference_classification: InferenceClassification,
    #[serde(default)]
    web_link: Option<String>,
    #[serde(default)]
    body: Option<MessageBody>,
}

fn addresses(list: Vec<Recipient>) -> Vec<EmailAddress> {
    list.into_iter().map(|r| r.email_address).collect()
}

impl WireMessage {
    fn into_model(self) -> (Message, Option<MessageBody>) {
        let message = Message {
            id: self.id,
            conversation_id: self.conversation_id,
            parent_folder_id: self.parent_folder_id,
            subject: self.subject,
            from: self.from.map(|r| r.email_address),
            sender: self.sender.map(|r| r.email_address),
            to_recipients: addresses(self.to_recipients),
            cc_recipients: addresses(self.cc_recipients),
            bcc_recipients: addresses(self.bcc_recipients),
            reply_to: addresses(self.reply_to),
            received_date_time: self.received_date_time,
            sent_date_time: self.sent_date_time,
            body_preview: self.body_preview,
            is_read: self.is_read,
            is_draft: self.is_draft,
            importance: self.importance,
            flag_status: self.flag.map(|f| f.flag_status).unwrap_or_default(),
            categories: self.categories,
            has_attachments: self.has_attachments,
            inference_classification: self.inference_classification,
            web_link: self.web_link,
        };
        (message, self.body)
    }
}

/// Maps Graph message JSON (recipients as `{emailAddress}`, `flag.flagStatus`) to a [`Message`].
/// Returns `None` if it does not parse.
#[must_use]
pub fn message_from_json(v: Value) -> Option<Message> {
    serde_json::from_value::<WireMessage>(v).ok().map(|w| w.into_model().0)
}

fn map_message_change(v: Value) -> Option<Change<Message, MessageId>> {
    if is_removed(&v) { removed_id(&v).map(Change::Removed) } else { message_from_json(v).map(Change::Upsert) }
}

/// A message with its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageWithBody {
    /// The message metadata.
    pub message: Message,
    /// The body (HTML is untrusted; sanitise before display).
    pub body: MessageBody,
}

/// Builds `GET {mailbox}/mailFolders/{folder}/messages` with `$select`, `$top` and `$orderby`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox or folder id.
pub fn list_messages_request(mailbox: &Mailbox, folder: &FolderRef, opts: &ListOptions) -> Result<Request> {
    let mut req = mail_get(mailbox, &format!("/mailFolders/{}/messages", folder.encoded()?))?
        .query("$select", MESSAGE_SELECT)
        .query("$top", clamp_top(opts.top, DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE).to_string())
        .query("$orderby", opts.order_by.clone().unwrap_or_else(|| "receivedDateTime desc".to_owned()));
    if let Some(filter) = &opts.filter {
        req = req.query("$filter", filter.clone());
    }
    Ok(req)
}

/// One page of a backfill (older messages).
#[derive(Debug, Clone)]
pub struct Backfill {
    /// The messages, newest first.
    pub page: Page<Message>,
    /// Cursor for the next (older) page after a restart: the oldest `receivedDateTime` seen.
    /// `None` when the page was empty (backfill done).
    pub older_than: Option<Timestamp>,
}

/// Builds the backfill request: messages received strictly before `before`, newest first.
///
/// Within one run, follow `page.next_link` (lossless). The `older_than` cursor is for resuming
/// after a restart; because it uses `lt`, messages sharing the boundary timestamp exactly may be
/// skipped by the backfill, but the folder's delta round still delivers them.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox or folder id.
pub fn backfill_request(mailbox: &Mailbox, folder: &FolderRef, before: Timestamp, top: Option<u32>) -> Result<Request> {
    list_messages_request(
        mailbox,
        folder,
        &ListOptions {
            top,
            filter: Some(format!("receivedDateTime lt {before}")),
            order_by: Some("receivedDateTime desc".to_owned()),
        },
    )
}

/// Builds `GET {mailbox}/messages/{id}` with the body (HTML).
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox or id.
pub fn get_message_request(mailbox: &Mailbox, id: &MessageId) -> Result<Request> {
    Ok(with_prefer(
        mailbox.request(crate::request::Method::Get, &msg_path(id)?, ResourceFamily::Mail)?,
        &[PREFER_IMMUTABLE_ID, PREFER_HTML_BODY],
    )
    .query("$select", format!("{MESSAGE_SELECT},body")))
}

/// Options for a per-folder message delta.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeltaOptions {
    /// `Prefer: odata.maxpagesize`, default [`DEFAULT_PAGE_SIZE`].
    pub page_size: Option<u32>,
    /// Only messages received at or after this instant (`$filter=receivedDateTime ge ...`), to
    /// bound the first round on huge folders.
    pub received_since: Option<Timestamp>,
}

/// Builds the first `GET {mailbox}/mailFolders/{folder}/messages/delta` request.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox or folder id.
pub fn messages_delta_request(mailbox: &Mailbox, folder: &FolderRef, opts: &DeltaOptions) -> Result<Request> {
    let size = clamp_top(opts.page_size, DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE);
    let mut req = with_prefer(
        mailbox.request(
            crate::request::Method::Get,
            &format!("/mailFolders/{}/messages/delta", folder.encoded()?),
            ResourceFamily::Mail,
        )?,
        &[PREFER_IMMUTABLE_ID, &format!("odata.maxpagesize={size}")],
    )
    .query("$select", MESSAGE_SELECT);
    if let Some(since) = opts.received_since {
        req = req.query("$filter", format!("receivedDateTime ge {since}"));
    }
    Ok(req)
}

/// A whole message delta round (all pages up to `max_pages`).
#[derive(Debug, Clone)]
pub struct MessageDeltaRound {
    /// Changes in page order.
    pub changes: Vec<Change<Message, MessageId>>,
    /// Items skipped as unparseable.
    pub skipped: usize,
    /// The `deltaLink` to store for the next poll.
    pub delta_link: String,
    /// Pages fetched.
    pub pages: usize,
}

// ---------------------------------------------------------------------------------------------
// Updates, move, copy, delete.
// ---------------------------------------------------------------------------------------------

/// Fields to change on a message (`PATCH`); `None` leaves a field as is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageUpdate {
    /// Read state.
    pub is_read: Option<bool>,
    /// Follow-up flag.
    pub flag: Option<FlagStatus>,
    /// Replaces the category list.
    pub categories: Option<Vec<String>>,
    /// Importance.
    pub importance: Option<Importance>,
    /// Focused or Other (moves the message between the Focused Inbox tabs).
    pub inference_classification: Option<InferenceClassification>,
}

impl MessageUpdate {
    /// The PATCH body.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        if let Some(r) = self.is_read {
            m.insert("isRead".into(), Value::Bool(r));
        }
        if let Some(f) = &self.flag {
            m.insert("flag".into(), json!({ "flagStatus": f.as_graph() }));
        }
        if let Some(c) = &self.categories {
            m.insert("categories".into(), json!(c));
        }
        if let Some(i) = &self.importance {
            m.insert("importance".into(), Value::String(i.as_graph().to_owned()));
        }
        if let Some(i) = &self.inference_classification {
            m.insert("inferenceClassification".into(), Value::String(i.as_graph().to_owned()));
        }
        Value::Object(m)
    }
}

/// Builds `PATCH {mailbox}/messages/{id}`. It sets absolute values, so it is marked idempotent
/// (safe to retry).
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox or id, or an empty update.
pub fn update_message_request(mailbox: &Mailbox, id: &MessageId, update: &MessageUpdate) -> Result<Request> {
    let body = update.to_json();
    if body.as_object().is_none_or(Map::is_empty) {
        return Err(config("message update changes nothing"));
    }
    Ok(mail_req(mailbox, crate::request::Method::Patch, &msg_path(id)?)?.json_value(body).idempotent(true))
}

/// Builds `POST {mailbox}/messages/{id}/move` (`copy` when `copy` is true).
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox, id or destination.
pub fn move_message_request(mailbox: &Mailbox, id: &MessageId, destination: &FolderRef, copy: bool) -> Result<Request> {
    let dest = match destination {
        FolderRef::WellKnown(w) => w.as_graph().to_owned(),
        FolderRef::Id(id) => id.as_str().to_owned(),
    };
    let action = if copy { "copy" } else { "move" };
    Ok(mail_req(mailbox, crate::request::Method::Post, &format!("{}/{action}", msg_path(id)?))?
        .json_value(json!({ "destinationId": dest })))
}

/// Builds `DELETE {mailbox}/messages/{id}` (Exchange moves it to Recoverable Items; use
/// [`Client::move_message`] to Deleted Items for the usual "delete").
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox or id.
pub fn delete_message_request(mailbox: &Mailbox, id: &MessageId) -> Result<Request> {
    mail_req(mailbox, crate::request::Method::Delete, &msg_path(id)?)
}

// ---------------------------------------------------------------------------------------------
// Compose, drafts, send.
// ---------------------------------------------------------------------------------------------

/// A small file attachment sent inline as base64 (at most [`MAX_INLINE_ATTACHMENT_BYTES`]).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct FileAttachmentUpload {
    /// File name (shown to recipients).
    pub name: Sensitive,
    /// MIME type.
    pub content_type: Option<String>,
    /// The bytes.
    pub bytes: Vec<u8>,
    /// Whether it is an inline (`cid:`) image.
    pub is_inline: bool,
    /// The Content-ID for inline images.
    pub content_id: Option<String>,
}

impl fmt::Debug for FileAttachmentUpload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileAttachmentUpload")
            .field("name", &self.name)
            .field("content_type", &self.content_type)
            .field("len", &self.bytes.len())
            .field("is_inline", &self.is_inline)
            .finish_non_exhaustive()
    }
}

impl FileAttachmentUpload {
    /// The `#microsoft.graph.fileAttachment` JSON.
    ///
    /// # Errors
    /// [`ErrorKind::TooLarge`] above [`MAX_INLINE_ATTACHMENT_BYTES`] (use an upload session).
    pub fn to_json(&self) -> Result<Value> {
        if self.bytes.len() > MAX_INLINE_ATTACHMENT_BYTES {
            return Err(Error::new(ErrorKind::TooLarge, "attachment too large to send inline; use an upload session"));
        }
        let mut m = Map::new();
        m.insert("@odata.type".into(), Value::String("#microsoft.graph.fileAttachment".into()));
        m.insert("name".into(), Value::String(self.name.as_str().to_owned()));
        if let Some(ct) = &self.content_type {
            m.insert("contentType".into(), Value::String(ct.clone()));
        }
        m.insert("contentBytes".into(), Value::String(super::chat::base64(&self.bytes)));
        m.insert("isInline".into(), Value::Bool(self.is_inline));
        if let Some(cid) = &self.content_id {
            m.insert("contentId".into(), Value::String(cid.clone()));
        }
        Ok(Value::Object(m))
    }
}

/// A message to send or save as a draft.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutgoingMessage {
    /// Subject.
    pub subject: Sensitive,
    /// Body (HTML or text).
    pub body: MessageBody,
    /// To.
    pub to: Vec<EmailAddress>,
    /// Cc.
    pub cc: Vec<EmailAddress>,
    /// Bcc.
    pub bcc: Vec<EmailAddress>,
    /// Reply-To.
    pub reply_to: Vec<EmailAddress>,
    /// Send as / on behalf of (shared mailboxes, `Mail.Send.Shared`).
    pub from: Option<EmailAddress>,
    /// Importance.
    pub importance: Option<Importance>,
    /// Categories.
    pub categories: Vec<String>,
    /// Ask for a read receipt.
    pub request_read_receipt: bool,
    /// Small file attachments (larger ones: save a draft, then upload).
    pub attachments: Vec<FileAttachmentUpload>,
}

impl OutgoingMessage {
    /// The Graph `message` JSON. Empty lists and unset fields are left out, so the same body
    /// works for `POST /messages` and as the `message` of `createReply` (where it only overrides).
    ///
    /// # Errors
    /// [`ErrorKind::Config`] for a recipient without an address; `TooLarge` for an attachment
    /// over the inline limit.
    pub fn to_json(&self) -> Result<Value> {
        let mut m = Map::new();
        if !self.subject.is_empty() {
            m.insert("subject".into(), Value::String(self.subject.as_str().to_owned()));
        }
        if !self.body.content.is_empty() {
            m.insert(
                "body".into(),
                json!({ "contentType": self.body.content_type.as_graph(), "content": self.body.content.as_str() }),
            );
        }
        for (key, list) in [
            ("toRecipients", &self.to),
            ("ccRecipients", &self.cc),
            ("bccRecipients", &self.bcc),
            ("replyTo", &self.reply_to),
        ] {
            if !list.is_empty() {
                m.insert(key.into(), recipients_json(list)?);
            }
        }
        if let Some(from) = &self.from {
            m.insert("from".into(), recipient_json(from)?);
        }
        if let Some(i) = &self.importance {
            m.insert("importance".into(), Value::String(i.as_graph().to_owned()));
        }
        if !self.categories.is_empty() {
            m.insert("categories".into(), json!(self.categories));
        }
        if self.request_read_receipt {
            m.insert("isReadReceiptRequested".into(), Value::Bool(true));
        }
        if !self.attachments.is_empty() {
            let atts = self.attachments.iter().map(FileAttachmentUpload::to_json).collect::<Result<Vec<_>>>()?;
            m.insert("attachments".into(), Value::Array(atts));
        }
        Ok(Value::Object(m))
    }
}

/// Builds `POST {mailbox}/sendMail` (not idempotent: never retried automatically).
///
/// # Errors
/// As [`OutgoingMessage::to_json`]; [`ErrorKind::Config`] for an invalid mailbox or no recipients.
pub fn send_mail_request(mailbox: &Mailbox, message: &OutgoingMessage, save_to_sent: bool) -> Result<Request> {
    if message.to.is_empty() && message.cc.is_empty() && message.bcc.is_empty() {
        return Err(config("a message needs at least one recipient"));
    }
    let body = json!({ "message": message.to_json()?, "saveToSentItems": save_to_sent });
    Ok(mail_req(mailbox, crate::request::Method::Post, "/sendMail")?.json_value(body))
}

/// Builds `POST {mailbox}/messages` (create a draft in Drafts).
///
/// # Errors
/// As [`OutgoingMessage::to_json`].
pub fn create_draft_request(mailbox: &Mailbox, message: &OutgoingMessage) -> Result<Request> {
    Ok(mail_req(mailbox, crate::request::Method::Post, "/messages")?.json_value(message.to_json()?))
}

/// Which response draft to create from an existing message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyKind {
    /// `createReply`.
    Reply,
    /// `createReplyAll`.
    ReplyAll,
    /// `createForward` (needs recipients).
    Forward,
}

impl ReplyKind {
    fn action(self) -> &'static str {
        match self {
            Self::Reply => "createReply",
            Self::ReplyAll => "createReplyAll",
            Self::Forward => "createForward",
        }
    }
}

/// Builds `POST {mailbox}/messages/{id}/createReply|createReplyAll|createForward`.
///
/// `overrides` (optional) sets fields of the new draft (body, extra recipients, attachments);
/// for a forward, its `to` list is the forward's recipients.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid id or recipient.
pub fn create_response_draft_request(
    mailbox: &Mailbox,
    id: &MessageId,
    kind: ReplyKind,
    overrides: Option<&OutgoingMessage>,
) -> Result<Request> {
    let mut body = Map::new();
    if let Some(o) = overrides {
        body.insert("message".into(), o.to_json()?);
    }
    Ok(mail_req(mailbox, crate::request::Method::Post, &format!("{}/{}", msg_path(id)?, kind.action()))?
        .json_value(Value::Object(body)))
}

/// Builds `PATCH {mailbox}/messages/{id}` replacing a draft's fields (autosave).
///
/// # Errors
/// As [`OutgoingMessage::to_json`].
pub fn update_draft_request(mailbox: &Mailbox, id: &MessageId, message: &OutgoingMessage) -> Result<Request> {
    let mut body = message.to_json()?;
    if let Some(m) = body.as_object_mut() {
        // Attachments are added separately; a PATCH cannot carry them.
        m.remove("attachments");
    }
    Ok(mail_req(mailbox, crate::request::Method::Patch, &msg_path(id)?)?.json_value(body).idempotent(true))
}

/// Builds `POST {mailbox}/messages/{id}/send`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid id.
pub fn send_draft_request(mailbox: &Mailbox, id: &MessageId) -> Result<Request> {
    mail_req(mailbox, crate::request::Method::Post, &format!("{}/send", msg_path(id)?))
}

// ---------------------------------------------------------------------------------------------
// Attachments.
// ---------------------------------------------------------------------------------------------

/// Maps Graph attachment JSON to an [`Attachment`]; `kind` comes from `@odata.type`.
#[must_use]
pub fn attachment_from_json(v: Value) -> Option<Attachment> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Wire {
        id: AttachmentId,
        #[serde(default, rename = "@odata.type")]
        odata_type: Option<String>,
        #[serde(default)]
        name: Sensitive,
        #[serde(default, deserialize_with = "null_default")]
        size: u64,
        #[serde(default)]
        content_type: Option<String>,
        #[serde(default, deserialize_with = "null_default")]
        is_inline: bool,
        #[serde(default)]
        content_id: Option<String>,
        #[serde(default)]
        last_modified_date_time: Option<Timestamp>,
    }
    let w: Wire = serde_json::from_value(v).ok()?;
    let kind = match w.odata_type.as_deref() {
        Some(t) => AttachmentKind::from_odata_type(t),
        None => AttachmentKind::Unknown(String::new()),
    };
    Some(Attachment {
        id: w.id,
        kind,
        name: w.name,
        size: w.size,
        content_type: w.content_type,
        is_inline: w.is_inline,
        content_id: w.content_id,
        last_modified_date_time: w.last_modified_date_time,
    })
}

fn attachment_path(id: &MessageId, attachment: &AttachmentId) -> Result<String> {
    Ok(format!("{}/attachments/{}", msg_path(id)?, segment(attachment.as_str())?))
}

/// Builds `GET {mailbox}/messages/{id}/attachments` (metadata only, never `contentBytes`).
///
/// `contentId` is not in the base `attachment` type, so the list cannot `$select` it; fetch an
/// inline image's metadata with [`Client::get_attachment`] when its `cid:` is needed.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid id.
pub fn list_attachments_request(mailbox: &Mailbox, id: &MessageId) -> Result<Request> {
    Ok(mail_get(mailbox, &format!("{}/attachments", msg_path(id)?))?.query("$select", ATTACHMENT_SELECT))
}

/// Builds `GET {mailbox}/messages/{id}/attachments/{aid}` (full attachment incl. `contentId`).
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid id.
pub fn get_attachment_request(mailbox: &Mailbox, id: &MessageId, attachment: &AttachmentId) -> Result<Request> {
    mail_get(mailbox, &attachment_path(id, attachment)?)
}

/// Builds `GET {mailbox}/messages/{id}/attachments/{aid}/$value` (raw content).
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid id.
pub fn attachment_content_request(mailbox: &Mailbox, id: &MessageId, attachment: &AttachmentId) -> Result<Request> {
    Ok(mail_get(mailbox, &format!("{}/$value", attachment_path(id, attachment)?))?.header("Accept", "*/*"))
}

/// Builds `POST {mailbox}/messages/{id}/attachments` for a small file attachment.
///
/// # Errors
/// As [`FileAttachmentUpload::to_json`].
pub fn add_attachment_request(mailbox: &Mailbox, id: &MessageId, attachment: &FileAttachmentUpload) -> Result<Request> {
    Ok(mail_req(mailbox, crate::request::Method::Post, &format!("{}/attachments", msg_path(id)?))?
        .json_value(attachment.to_json()?))
}

/// Builds `POST {mailbox}/messages/{id}/attachments/createUploadSession` for a large file.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid id or a zero size; `TooLarge` above
/// [`MAX_ATTACHMENT_BYTES`].
pub fn attachment_upload_session_request(
    mailbox: &Mailbox,
    id: &MessageId,
    name: &str,
    size: u64,
    is_inline: bool,
    content_id: Option<&str>,
) -> Result<Request> {
    if size == 0 {
        return Err(config("attachment upload needs a non-empty file"));
    }
    if size > MAX_ATTACHMENT_BYTES {
        return Err(Error::new(ErrorKind::TooLarge, "attachment exceeds the 150 MiB Outlook limit"));
    }
    let mut item = json!({ "attachmentType": "file", "name": name, "size": size, "isInline": is_inline });
    if let (Some(cid), Some(m)) = (content_id, item.as_object_mut()) {
        m.insert("contentId".into(), Value::String(cid.to_owned()));
    }
    Ok(mail_req(mailbox, crate::request::Method::Post, &format!("{}/attachments/createUploadSession", msg_path(id)?))?
        .json_value(json!({ "AttachmentItem": item })))
}

// ---------------------------------------------------------------------------------------------
// Mailbox settings.
// ---------------------------------------------------------------------------------------------

api_enum!(
    /// Automatic replies state.
    AutomaticRepliesStatus {
        /// Off.
        Disabled = "disabled",
        /// On until turned off.
        AlwaysEnabled = "alwaysEnabled",
        /// On between the scheduled times.
        Scheduled = "scheduled",
    }
);

api_enum!(
    /// Who outside the organisation gets automatic replies.
    ExternalAudience {
        /// Nobody outside.
        None = "none",
        /// Only the user's contacts.
        ContactsOnly = "contactsOnly",
        /// Everyone.
        All = "all",
    }
);

/// Automatic replies (out of office).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomaticRepliesSetting {
    /// On, off or scheduled.
    pub status: AutomaticRepliesStatus,
    /// External audience.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_audience: Option<ExternalAudience>,
    /// Scheduled start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_start_date_time: Option<DateTimeZone>,
    /// Scheduled end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_end_date_time: Option<DateTimeZone>,
    /// Reply to people inside the organisation (HTML).
    #[serde(default)]
    pub internal_reply_message: Sensitive,
    /// Reply to people outside (HTML).
    #[serde(default)]
    pub external_reply_message: Sensitive,
}

/// A working-hours time zone (`{name}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkingHoursTimeZone {
    /// The zone name (Windows or IANA).
    #[serde(default)]
    pub name: String,
}

/// Working hours.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkingHours {
    /// Working days.
    #[serde(default, deserialize_with = "null_default")]
    pub days_of_week: Vec<DayOfWeek>,
    /// Start of day, as Graph sends it (`08:00:00.0000000`).
    #[serde(default)]
    pub start_time: String,
    /// End of day.
    #[serde(default)]
    pub end_time: String,
    /// The zone the times are in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<WorkingHoursTimeZone>,
}

/// The mailbox settings Quorum reads and writes.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailboxSettings {
    /// Automatic replies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automatic_replies_setting: Option<AutomaticRepliesSetting>,
    /// The user's time zone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
    /// Working hours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_hours: Option<WorkingHours>,
    /// Date format, e.g. `dd/MM/yyyy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_format: Option<String>,
    /// Time format, e.g. `HH:mm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_format: Option<String>,
}

/// Builds `GET {mailbox}/mailboxSettings`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox.
pub fn get_mailbox_settings_request(mailbox: &Mailbox) -> Result<Request> {
    Ok(mailbox
        .request(crate::request::Method::Get, "/mailboxSettings", ResourceFamily::Mail)?
        .query("$select", "automaticRepliesSetting,timeZone,workingHours,dateFormat,timeFormat"))
}

/// Builds `PATCH {mailbox}/mailboxSettings` with only the fields set in `settings`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox or an empty update.
pub fn update_mailbox_settings_request(mailbox: &Mailbox, settings: &MailboxSettings) -> Result<Request> {
    if settings == &MailboxSettings::default() {
        return Err(config("mailbox settings update changes nothing"));
    }
    Ok(mailbox
        .request(crate::request::Method::Patch, "/mailboxSettings", ResourceFamily::Mail)?
        .json(settings)?
        .idempotent(true))
}

// ---------------------------------------------------------------------------------------------
// Inbox rules.
// ---------------------------------------------------------------------------------------------

/// An inbox rule (`messageRule`). Conditions, actions and exceptions are kept as Graph JSON
/// (they hold addresses and words, so `Debug` redacts them).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRule {
    /// Rule id (absent when creating).
    #[serde(default, skip_serializing)]
    pub id: Option<String>,
    /// Name.
    #[serde(default)]
    pub display_name: Sensitive,
    /// Evaluation order (1 = first).
    #[serde(default, deserialize_with = "null_default")]
    pub sequence: u32,
    /// Whether it runs.
    #[serde(default, deserialize_with = "null_default")]
    pub is_enabled: bool,
    /// Set by Exchange when the rule is broken.
    #[serde(default, deserialize_with = "null_default", skip_serializing)]
    pub has_error: bool,
    /// Created by another client and not editable here.
    #[serde(default, deserialize_with = "null_default", skip_serializing)]
    pub is_read_only: bool,
    /// `messageRulePredicates` for when the rule applies.
    #[serde(default, skip_serializing_if = "is_null")]
    pub conditions: SensitiveJson,
    /// `messageRuleActions`.
    #[serde(default, skip_serializing_if = "is_null")]
    pub actions: SensitiveJson,
    /// `messageRulePredicates` for exceptions.
    #[serde(default, skip_serializing_if = "is_null")]
    pub exceptions: SensitiveJson,
}

fn is_null(v: &SensitiveJson) -> bool {
    v.0.is_null()
}

const RULES_PATH: &str = "/mailFolders/inbox/messageRules";

fn rule_path(id: &str) -> Result<String> {
    Ok(format!("{RULES_PATH}/{}", segment(id)?))
}

/// Builds `GET {mailbox}/mailFolders/inbox/messageRules`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox.
pub fn list_rules_request(mailbox: &Mailbox) -> Result<Request> {
    mailbox.request(crate::request::Method::Get, RULES_PATH, ResourceFamily::Mail)
}

/// Builds `POST {mailbox}/mailFolders/inbox/messageRules`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox.
pub fn create_rule_request(mailbox: &Mailbox, rule: &MessageRule) -> Result<Request> {
    mailbox.request(crate::request::Method::Post, RULES_PATH, ResourceFamily::Mail)?.json(rule)
}

/// Builds `PATCH {mailbox}/mailFolders/inbox/messageRules/{id}`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid id.
pub fn update_rule_request(mailbox: &Mailbox, id: &str, rule: &MessageRule) -> Result<Request> {
    Ok(mailbox
        .request(crate::request::Method::Patch, &rule_path(id)?, ResourceFamily::Mail)?
        .json(rule)?
        .idempotent(true))
}

/// Builds `DELETE {mailbox}/mailFolders/inbox/messageRules/{id}`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid id.
pub fn delete_rule_request(mailbox: &Mailbox, id: &str) -> Result<Request> {
    mailbox.request(crate::request::Method::Delete, &rule_path(id)?, ResourceFamily::Mail)
}

// ---------------------------------------------------------------------------------------------
// Master categories.
// ---------------------------------------------------------------------------------------------

/// An entry of the master category list (`outlookCategory`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Category {
    /// Category id (absent when creating).
    #[serde(default, skip_serializing)]
    pub id: Option<String>,
    /// The name, which is also what messages store in `categories`.
    #[serde(default)]
    pub display_name: String,
    /// Colour preset (`none`, `preset0` … `preset24`), kept verbatim.
    #[serde(default)]
    pub color: String,
}

/// Builds `GET {mailbox}/outlook/masterCategories`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox.
pub fn list_categories_request(mailbox: &Mailbox) -> Result<Request> {
    mailbox.request(crate::request::Method::Get, "/outlook/masterCategories", ResourceFamily::Mail)
}

/// Builds `POST {mailbox}/outlook/masterCategories`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox or an empty name.
pub fn create_category_request(mailbox: &Mailbox, category: &Category) -> Result<Request> {
    if category.display_name.trim().is_empty() {
        return Err(config("category needs a name"));
    }
    mailbox.request(crate::request::Method::Post, "/outlook/masterCategories", ResourceFamily::Mail)?.json(category)
}

/// Builds `DELETE {mailbox}/outlook/masterCategories/{id}`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid id.
pub fn delete_category_request(mailbox: &Mailbox, id: &str) -> Result<Request> {
    mailbox.request(
        crate::request::Method::Delete,
        &format!("/outlook/masterCategories/{}", segment(id)?),
        ResourceFamily::Mail,
    )
}

// ---------------------------------------------------------------------------------------------
// Focused Inbox overrides.
// ---------------------------------------------------------------------------------------------

/// A Focused Inbox override: always classify mail from a sender as Focused or Other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceOverride {
    /// Override id.
    pub id: String,
    /// Focused or Other.
    pub classify_as: InferenceClassification,
    /// The sender.
    pub sender: EmailAddress,
}

fn override_from_json(v: Value) -> Option<InferenceOverride> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Wire {
        id: String,
        #[serde(default, deserialize_with = "null_default")]
        classify_as: InferenceClassification,
        #[serde(default, deserialize_with = "null_default")]
        sender_email_address: EmailAddress,
    }
    let w: Wire = serde_json::from_value(v).ok()?;
    if w.id.is_empty() {
        return None;
    }
    Some(InferenceOverride { id: w.id, classify_as: w.classify_as, sender: w.sender_email_address })
}

const OVERRIDES_PATH: &str = "/inferenceClassification/overrides";

/// Builds `GET {mailbox}/inferenceClassification/overrides`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox.
pub fn list_overrides_request(mailbox: &Mailbox) -> Result<Request> {
    mailbox.request(crate::request::Method::Get, OVERRIDES_PATH, ResourceFamily::Mail)
}

/// Builds `POST {mailbox}/inferenceClassification/overrides`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox or a sender without an address.
pub fn create_override_request(
    mailbox: &Mailbox,
    sender: &EmailAddress,
    classify_as: &InferenceClassification,
) -> Result<Request> {
    let email = recipient_json(sender)?.get("emailAddress").cloned().unwrap_or(Value::Null);
    Ok(mailbox
        .request(crate::request::Method::Post, OVERRIDES_PATH, ResourceFamily::Mail)?
        .json_value(json!({ "classifyAs": classify_as.as_graph(), "senderEmailAddress": email })))
}

/// Builds `PATCH {mailbox}/inferenceClassification/overrides/{id}`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid id.
pub fn update_override_request(mailbox: &Mailbox, id: &str, classify_as: &InferenceClassification) -> Result<Request> {
    Ok(mailbox
        .request(crate::request::Method::Patch, &format!("{OVERRIDES_PATH}/{}", segment(id)?), ResourceFamily::Mail)?
        .json_value(json!({ "classifyAs": classify_as.as_graph() }))
        .idempotent(true))
}

/// Builds `DELETE {mailbox}/inferenceClassification/overrides/{id}`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid id.
pub fn delete_override_request(mailbox: &Mailbox, id: &str) -> Result<Request> {
    mailbox.request(crate::request::Method::Delete, &format!("{OVERRIDES_PATH}/{}", segment(id)?), ResourceFamily::Mail)
}

impl Client {
    /// Parses any page of folders (first page or a [`Request::follow`] of its `next_link`).
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn mail_folders_page(&self, req: Request) -> Result<Page<MailFolder>> {
        mapped_page(self, req, map_folder).await
    }

    /// Top-level folders of `mailbox` (first page).
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_mail_folders(&self, mailbox: &Mailbox) -> Result<Page<MailFolder>> {
        self.mail_folders_page(list_folders_request(mailbox)?).await
    }

    /// Child folders of `folder` (first page).
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_child_mail_folders(&self, mailbox: &Mailbox, folder: &FolderRef) -> Result<Page<MailFolder>> {
        self.mail_folders_page(child_folders_request(mailbox, folder)?).await
    }

    /// One folder. A well-known reference fills in [`MailFolder::well_known_name`] (Graph v1.0 does
    /// not return it, so this is how the sync engine learns which id is the inbox, drafts, ...).
    ///
    /// # Errors
    /// As [`Client::send_json`]; [`ErrorKind::Malformed`] if the folder does not parse.
    pub async fn get_mail_folder(&self, mailbox: &Mailbox, folder: &FolderRef) -> Result<MailFolder> {
        let v: Value = self.send_json(get_folder_request(mailbox, folder)?).await?;
        let mut f = map_folder(v).ok_or_else(|| Error::new(ErrorKind::Malformed, "mail folder did not parse"))?;
        if let FolderRef::WellKnown(w) = folder {
            f.well_known_name = Some(w.clone());
        }
        Ok(f)
    }

    /// One page of the folder delta. Pass [`folders_delta_request`] first, then
    /// [`delta_link_request`] for each `nextLink` or stored `deltaLink`.
    ///
    /// # Errors
    /// As [`Client::delta_page`] ([`ErrorKind::DeltaExpired`] means: drop the link, resync).
    pub async fn mail_folders_delta_page(&self, req: Request) -> Result<DeltaPage<Change<MailFolder, MailFolderId>>> {
        mapped_delta(self, req, map_folder_change).await
    }

    /// Looks up the ids of well-known folders in one `$batch` (`GET {mailbox}/mailFolders/{name}`
    /// each). Folders the mailbox lacks (e.g. no `archive`) are left out.
    ///
    /// # Errors
    /// Errors of the `$batch` call itself.
    pub async fn resolve_well_known_folders(
        &self,
        mailbox: &Mailbox,
        names: &[WellKnownFolder],
    ) -> Result<Vec<(WellKnownFolder, MailFolderId)>> {
        let mut items = Vec::with_capacity(names.len());
        for (i, name) in names.iter().enumerate() {
            let req = mail_get(mailbox, &format!("/mailFolders/{}", segment(name.as_graph())?))?.query("$select", "id");
            items.push(crate::batch::BatchItem::new(format!("f{i}"), req));
        }
        let responses = self.batch(items).await?;
        let mut out = Vec::new();
        for (name, resp) in names.iter().zip(responses) {
            if let Ok(v) = resp.json::<Value>()
                && let Some(id) = v.get("id").and_then(|id| serde_json::from_value::<MailFolderId>(id.clone()).ok())
            {
                out.push((name.clone(), id));
            }
        }
        Ok(out)
    }

    /// Parses any page of messages.
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn messages_page(&self, req: Request) -> Result<Page<Message>> {
        mapped_page(self, req, message_from_json).await
    }

    /// The first page of messages in `folder`, newest first.
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_messages(
        &self,
        mailbox: &Mailbox,
        folder: &FolderRef,
        opts: &ListOptions,
    ) -> Result<Page<Message>> {
        self.messages_page(list_messages_request(mailbox, folder, opts)?).await
    }

    /// Fetches messages received before `before` (one page) and the cursor for the next one.
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn backfill_messages(
        &self,
        mailbox: &Mailbox,
        folder: &FolderRef,
        before: Timestamp,
        top: Option<u32>,
    ) -> Result<Backfill> {
        let page = self.messages_page(backfill_request(mailbox, folder, before, top)?).await?;
        let older_than = page.items.iter().filter_map(|m| m.received_date_time).min();
        Ok(Backfill { page, older_than })
    }

    /// One message with its body.
    ///
    /// # Errors
    /// As [`Client::send_json`]; [`ErrorKind::NotFound`] if it no longer exists;
    /// [`ErrorKind::Malformed`] if it does not parse.
    pub async fn get_message(&self, mailbox: &Mailbox, id: &MessageId) -> Result<MessageWithBody> {
        let v: Value = self.send_json(get_message_request(mailbox, id)?).await?;
        let wire: WireMessage = serde_json::from_value(v)?;
        let (message, body) = wire.into_model();
        Ok(MessageWithBody { message, body: body.unwrap_or_default() })
    }

    /// One page of a message delta: pass [`messages_delta_request`] first, then
    /// [`delta_link_request`] for each `nextLink` or the stored `deltaLink`.
    ///
    /// # Errors
    /// As [`Client::delta_page`] ([`ErrorKind::DeltaExpired`]: drop the link and resync the folder).
    pub async fn messages_delta_page(&self, req: Request) -> Result<DeltaPage<Change<Message, MessageId>>> {
        mapped_delta(self, req, map_message_change).await
    }

    /// Runs a message delta round from `start` (initial request or stored link), following
    /// `nextLink`s with the same headers until a `deltaLink`.
    ///
    /// # Errors
    /// As [`Client::messages_delta_page`]; [`ErrorKind::TooLarge`] after `max_pages` pages;
    /// [`ErrorKind::Malformed`] for a `nextLink` loop.
    pub async fn messages_delta_round(&self, start: Request, max_pages: usize) -> Result<MessageDeltaRound> {
        let mut changes = Vec::new();
        let mut skipped = 0usize;
        let mut seen = std::collections::HashSet::new();
        let mut next = start.clone();
        for pages in 1..=max_pages.max(1) {
            let page = self.messages_delta_page(next).await?;
            changes.extend(page.items);
            skipped = skipped.saturating_add(page.skipped);
            match page.outcome {
                DeltaOutcome::Done(delta_link) => return Ok(MessageDeltaRound { changes, skipped, delta_link, pages }),
                DeltaOutcome::Next(link) => {
                    if !seen.insert(link.clone()) {
                        return Err(Error::new(ErrorKind::Malformed, "nextLink repeats an earlier page"));
                    }
                    next = start.follow(link);
                }
            }
        }
        Err(Error::new(ErrorKind::TooLarge, "page cap reached before the last delta page"))
    }

    /// Updates a message and returns it as Graph now has it.
    ///
    /// # Errors
    /// As [`Client::send_json`]; [`ErrorKind::Malformed`] if the response does not parse.
    pub async fn update_message(&self, mailbox: &Mailbox, id: &MessageId, update: &MessageUpdate) -> Result<Message> {
        let v: Value = self.send_json(update_message_request(mailbox, id, update)?).await?;
        message_from_json(v).ok_or_else(|| Error::new(ErrorKind::Malformed, "message did not parse"))
    }

    /// Moves a message; with immutable ids the returned message keeps its id.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn move_message(&self, mailbox: &Mailbox, id: &MessageId, destination: &FolderRef) -> Result<Message> {
        let v: Value = self.send_json(move_message_request(mailbox, id, destination, false)?).await?;
        message_from_json(v).ok_or_else(|| Error::new(ErrorKind::Malformed, "message did not parse"))
    }

    /// Copies a message; returns the copy.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn copy_message(&self, mailbox: &Mailbox, id: &MessageId, destination: &FolderRef) -> Result<Message> {
        let v: Value = self.send_json(move_message_request(mailbox, id, destination, true)?).await?;
        message_from_json(v).ok_or_else(|| Error::new(ErrorKind::Malformed, "message did not parse"))
    }

    /// Deletes a message.
    ///
    /// # Errors
    /// As [`Client::send_raw`].
    pub async fn delete_message(&self, mailbox: &Mailbox, id: &MessageId) -> Result<()> {
        self.send_raw(delete_message_request(mailbox, id)?).await.map(|_| ())
    }

    /// Sends a message directly (Graph answers 202 Accepted).
    ///
    /// # Errors
    /// As [`Client::send_raw`].
    pub async fn send_mail(&self, mailbox: &Mailbox, message: &OutgoingMessage, save_to_sent: bool) -> Result<()> {
        self.send_raw(send_mail_request(mailbox, message, save_to_sent)?).await.map(|_| ())
    }

    /// Creates a draft and returns it.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn create_draft(&self, mailbox: &Mailbox, message: &OutgoingMessage) -> Result<Message> {
        let v: Value = self.send_json(create_draft_request(mailbox, message)?).await?;
        message_from_json(v).ok_or_else(|| Error::new(ErrorKind::Malformed, "draft did not parse"))
    }

    /// Creates a reply, reply-all or forward draft and returns it (send it with [`Client::send_draft`]).
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn create_response_draft(
        &self,
        mailbox: &Mailbox,
        id: &MessageId,
        kind: ReplyKind,
        overrides: Option<&OutgoingMessage>,
    ) -> Result<Message> {
        let v: Value = self.send_json(create_response_draft_request(mailbox, id, kind, overrides)?).await?;
        message_from_json(v).ok_or_else(|| Error::new(ErrorKind::Malformed, "draft did not parse"))
    }

    /// Updates a draft.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn update_draft(&self, mailbox: &Mailbox, id: &MessageId, message: &OutgoingMessage) -> Result<Message> {
        let v: Value = self.send_json(update_draft_request(mailbox, id, message)?).await?;
        message_from_json(v).ok_or_else(|| Error::new(ErrorKind::Malformed, "draft did not parse"))
    }

    /// Sends a draft.
    ///
    /// # Errors
    /// As [`Client::send_raw`].
    pub async fn send_draft(&self, mailbox: &Mailbox, id: &MessageId) -> Result<()> {
        self.send_raw(send_draft_request(mailbox, id)?).await.map(|_| ())
    }

    /// Attachment metadata of a message.
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_attachments(&self, mailbox: &Mailbox, id: &MessageId) -> Result<Page<Attachment>> {
        mapped_page(self, list_attachments_request(mailbox, id)?, attachment_from_json).await
    }

    /// One attachment's metadata (the content bytes in the response are dropped).
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn get_attachment(
        &self,
        mailbox: &Mailbox,
        id: &MessageId,
        attachment: &AttachmentId,
    ) -> Result<Attachment> {
        let v: Value = self.send_json(get_attachment_request(mailbox, id, attachment)?).await?;
        attachment_from_json(v).ok_or_else(|| Error::new(ErrorKind::Malformed, "attachment did not parse"))
    }

    /// Downloads an attachment's bytes (file attachments; item attachments come back as MIME).
    /// The client's response cap bounds the size.
    ///
    /// # Errors
    /// As [`Client::send_raw`].
    pub async fn attachment_content(
        &self,
        mailbox: &Mailbox,
        id: &MessageId,
        attachment: &AttachmentId,
    ) -> Result<Vec<u8>> {
        Ok(self.send_raw(attachment_content_request(mailbox, id, attachment)?).await?.body)
    }

    /// Adds a small file attachment to a draft.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn add_attachment(
        &self,
        mailbox: &Mailbox,
        id: &MessageId,
        attachment: &FileAttachmentUpload,
    ) -> Result<Attachment> {
        let v: Value = self.send_json(add_attachment_request(mailbox, id, attachment)?).await?;
        attachment_from_json(v).ok_or_else(|| Error::new(ErrorKind::Malformed, "attachment did not parse"))
    }

    /// Creates an upload session for a large attachment on a draft.
    ///
    /// The returned URL is on `outlook.office.com`: build the client with
    /// [`MAIL_UPLOAD_HOST_SUFFIXES`] allowed, or this fails with [`ErrorKind::Malformed`].
    ///
    /// # Errors
    /// As [`Client::create_upload_session`].
    pub async fn create_attachment_upload_session(
        &self,
        mailbox: &Mailbox,
        id: &MessageId,
        name: &str,
        size: u64,
        is_inline: bool,
        content_id: Option<&str>,
    ) -> Result<UploadSession> {
        self.create_upload_session(attachment_upload_session_request(mailbox, id, name, size, is_inline, content_id)?)
            .await
    }

    /// Uploads `source` to an attachment upload session in 3.75 MiB chunks (Outlook's limit is
    /// 4 MiB per request).
    ///
    /// # Errors
    /// As [`Client::upload`].
    pub async fn upload_attachment<S: ChunkSource + ?Sized>(&self, session: &UploadSession, source: &S) -> Result<()> {
        let chunk = ChunkSize::from_units(MAIL_UPLOAD_CHUNK_UNITS)?;
        self.upload(session, source, chunk).await.map(|_| ())
    }

    /// Reads mailbox settings.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn get_mailbox_settings(&self, mailbox: &Mailbox) -> Result<MailboxSettings> {
        self.send_json(get_mailbox_settings_request(mailbox)?).await
    }

    /// Updates mailbox settings; returns the settings Graph echoes back.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn update_mailbox_settings(
        &self,
        mailbox: &Mailbox,
        settings: &MailboxSettings,
    ) -> Result<MailboxSettings> {
        self.send_json(update_mailbox_settings_request(mailbox, settings)?).await
    }

    /// Inbox rules.
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_message_rules(&self, mailbox: &Mailbox) -> Result<Page<MessageRule>> {
        mapped_page(self, list_rules_request(mailbox)?, |v| serde_json::from_value(v).ok()).await
    }

    /// Creates a rule.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn create_message_rule(&self, mailbox: &Mailbox, rule: &MessageRule) -> Result<MessageRule> {
        self.send_json(create_rule_request(mailbox, rule)?).await
    }

    /// Replaces a rule's fields.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn update_message_rule(&self, mailbox: &Mailbox, id: &str, rule: &MessageRule) -> Result<MessageRule> {
        self.send_json(update_rule_request(mailbox, id, rule)?).await
    }

    /// Deletes a rule.
    ///
    /// # Errors
    /// As [`Client::send_raw`].
    pub async fn delete_message_rule(&self, mailbox: &Mailbox, id: &str) -> Result<()> {
        self.send_raw(delete_rule_request(mailbox, id)?).await.map(|_| ())
    }

    /// The master category list.
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_master_categories(&self, mailbox: &Mailbox) -> Result<Page<Category>> {
        mapped_page(self, list_categories_request(mailbox)?, |v| serde_json::from_value(v).ok()).await
    }

    /// Creates a category.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn create_master_category(&self, mailbox: &Mailbox, category: &Category) -> Result<Category> {
        self.send_json(create_category_request(mailbox, category)?).await
    }

    /// Deletes a category.
    ///
    /// # Errors
    /// As [`Client::send_raw`].
    pub async fn delete_master_category(&self, mailbox: &Mailbox, id: &str) -> Result<()> {
        self.send_raw(delete_category_request(mailbox, id)?).await.map(|_| ())
    }

    /// Focused Inbox overrides.
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_focused_overrides(&self, mailbox: &Mailbox) -> Result<Page<InferenceOverride>> {
        mapped_page(self, list_overrides_request(mailbox)?, override_from_json).await
    }

    /// Creates an override (Graph updates an existing one for the same sender).
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn create_focused_override(
        &self,
        mailbox: &Mailbox,
        sender: &EmailAddress,
        classify_as: &InferenceClassification,
    ) -> Result<InferenceOverride> {
        let v: Value = self.send_json(create_override_request(mailbox, sender, classify_as)?).await?;
        override_from_json(v).ok_or_else(|| Error::new(ErrorKind::Malformed, "override did not parse"))
    }

    /// Changes an override's classification.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn update_focused_override(
        &self,
        mailbox: &Mailbox,
        id: &str,
        classify_as: &InferenceClassification,
    ) -> Result<InferenceOverride> {
        let v: Value = self.send_json(update_override_request(mailbox, id, classify_as)?).await?;
        override_from_json(v).ok_or_else(|| Error::new(ErrorKind::Malformed, "override did not parse"))
    }

    /// Deletes an override.
    ///
    /// # Errors
    /// As [`Client::send_raw`].
    pub async fn delete_focused_override(&self, mailbox: &Mailbox, id: &str) -> Result<()> {
        self.send_raw(delete_override_request(mailbox, id)?).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_are_encoded_and_dot_segments_rejected() {
        assert_eq!(segment("AAMk-AD_x=").unwrap(), "AAMk-AD_x=");
        assert_eq!(segment("a/b?c#d e+f%").unwrap(), "a%2Fb%3Fc%23d%20e+f%25");
        assert_eq!(segment("ünï").unwrap(), "%C3%BCn%C3%AF");
        for bad in ["", ".", ".."] {
            assert_eq!(segment(bad).unwrap_err().kind(), ErrorKind::Config);
        }
    }
}
