//! Mail folders, messages, bodies and attachments.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::address::EmailAddress;
use crate::de::null_default;
use crate::enums::graph_enum;
use crate::id::{AttachmentId, ConversationId, MailFolderId, MessageId};
use crate::sensitive::Sensitive;

graph_enum!(
    /// A well-known mail folder name (Graph's `wellKnownName`).
    WellKnownFolder {
        /// The inbox.
        Inbox = "inbox",
        /// Drafts.
        Drafts = "drafts",
        /// Sent items.
        SentItems = "sentitems",
        /// Deleted items.
        DeletedItems = "deleteditems",
        /// The archive folder.
        Archive = "archive",
        /// Junk email.
        JunkEmail = "junkemail",
        /// The outbox.
        Outbox = "outbox",
        /// Clutter (legacy).
        Clutter = "clutter",
        /// Conversation history (Skype/Teams).
        ConversationHistory = "conversationhistory",
        /// Scheduled (send later).
        Scheduled = "scheduled",
        /// The root of search folders.
        SearchFolders = "searchfolders",
        /// Sync issues.
        SyncIssues = "syncissues",
        /// The top of the folder tree.
        MsgFolderRoot = "msgfolderroot",
        /// Recoverable items (soft-deleted).
        RecoverableItemsDeletions = "recoverableitemsdeletions",
    }
);

graph_enum!(
    /// Message importance.
    #[derive(Default)]
    Importance {
        /// Low.
        Low = "low",
        /// Normal (the default).
        #[default]
        Normal = "normal",
        /// High.
        High = "high",
    }
);

graph_enum!(
    /// Follow-up flag state of a message.
    #[derive(Default)]
    FlagStatus {
        /// Not flagged.
        #[default]
        NotFlagged = "notFlagged",
        /// Flagged for follow-up.
        Flagged = "flagged",
        /// Follow-up complete.
        Complete = "complete",
    }
);

graph_enum!(
    /// Focused Inbox classification.
    #[derive(Default)]
    InferenceClassification {
        /// The Focused tab.
        #[default]
        Focused = "focused",
        /// The Other tab.
        Other = "other",
    }
);

graph_enum!(
    /// The content type of a message or chat body.
    #[derive(Default)]
    BodyType {
        /// HTML (must be sanitised before display).
        Html = "html",
        /// Plain text.
        #[default]
        Text = "text",
    }
);

graph_enum!(
    /// The kind of a mail attachment (from Graph's `@odata.type`).
    AttachmentKind {
        /// A file (`#microsoft.graph.fileAttachment`).
        File = "file",
        /// An attached message, event or contact (`#microsoft.graph.itemAttachment`).
        Item = "item",
        /// A link to a cloud file (`#microsoft.graph.referenceAttachment`).
        Reference = "reference",
    }
);

impl AttachmentKind {
    /// Maps a Graph `@odata.type` such as `#microsoft.graph.fileAttachment` to a kind.
    #[must_use]
    pub fn from_odata_type(odata_type: &str) -> Self {
        use crate::enums::GraphEnum;
        let short =
            odata_type.trim_start_matches('#').trim_start_matches("microsoft.graph.").trim_end_matches("Attachment");
        Self::from_graph(short)
    }
}

/// A mail folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailFolder {
    /// The folder id.
    pub id: MailFolderId,
    /// The parent folder, if any.
    #[serde(default)]
    pub parent_folder_id: Option<MailFolderId>,
    /// The folder's name (user-chosen, so treated as personal).
    #[serde(default)]
    pub display_name: Sensitive,
    /// The well-known role of the folder, if it has one.
    #[serde(default)]
    pub well_known_name: Option<WellKnownFolder>,
    /// Number of unread items.
    #[serde(default, deserialize_with = "null_default")]
    pub unread_item_count: u32,
    /// Number of items.
    #[serde(default, deserialize_with = "null_default")]
    pub total_item_count: u32,
    /// Number of child folders.
    #[serde(default, deserialize_with = "null_default")]
    pub child_folder_count: u32,
    /// Whether the folder is hidden.
    #[serde(default, deserialize_with = "null_default")]
    pub is_hidden: bool,
}

/// The body of a message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageBody {
    /// HTML or text.
    #[serde(default)]
    pub content_type: BodyType,
    /// The body content. HTML is untrusted: sanitise with `quorum-sanitize` before display.
    #[serde(default)]
    pub content: Sensitive,
}

/// A mail message (metadata; the body is loaded separately as [`MessageBody`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    /// The message id.
    pub id: MessageId,
    /// The conversation (thread) it belongs to.
    #[serde(default)]
    pub conversation_id: Option<ConversationId>,
    /// The folder it is in.
    #[serde(default)]
    pub parent_folder_id: Option<MailFolderId>,
    /// The subject.
    #[serde(default)]
    pub subject: Sensitive,
    /// The sender as shown (may differ from `sender` for delegated mail).
    #[serde(default)]
    pub from: Option<EmailAddress>,
    /// The account that actually sent it.
    #[serde(default)]
    pub sender: Option<EmailAddress>,
    /// To recipients.
    #[serde(default, deserialize_with = "null_default")]
    pub to_recipients: Vec<EmailAddress>,
    /// Cc recipients.
    #[serde(default, deserialize_with = "null_default")]
    pub cc_recipients: Vec<EmailAddress>,
    /// Bcc recipients (only visible on sent mail and drafts).
    #[serde(default, deserialize_with = "null_default")]
    pub bcc_recipients: Vec<EmailAddress>,
    /// Reply-To addresses.
    #[serde(default, deserialize_with = "null_default")]
    pub reply_to: Vec<EmailAddress>,
    /// When it was received.
    #[serde(default)]
    pub received_date_time: Option<Timestamp>,
    /// When it was sent.
    #[serde(default)]
    pub sent_date_time: Option<Timestamp>,
    /// The first ~255 characters of the body as plain text.
    #[serde(default)]
    pub body_preview: Sensitive,
    /// Whether it has been read.
    #[serde(default, deserialize_with = "null_default")]
    pub is_read: bool,
    /// Whether it is a draft.
    #[serde(default, deserialize_with = "null_default")]
    pub is_draft: bool,
    /// Importance.
    #[serde(default, deserialize_with = "null_default")]
    pub importance: Importance,
    /// Follow-up flag state.
    #[serde(default, deserialize_with = "null_default")]
    pub flag_status: FlagStatus,
    /// Category names.
    #[serde(default, deserialize_with = "null_default")]
    pub categories: Vec<String>,
    /// Whether it has non-inline attachments.
    #[serde(default, deserialize_with = "null_default")]
    pub has_attachments: bool,
    /// Focused or Other.
    #[serde(default, deserialize_with = "null_default")]
    pub inference_classification: InferenceClassification,
    /// The Outlook on the web link.
    #[serde(default)]
    pub web_link: Option<String>,
}

impl Message {
    /// Returns the subject text.
    #[must_use]
    pub fn subject(&self) -> &str {
        self.subject.as_str()
    }

    /// Returns `true` if the message is flagged for follow-up.
    #[must_use]
    pub fn is_flagged(&self) -> bool {
        self.flag_status == FlagStatus::Flagged
    }

    /// Returns all recipients (To, then Cc, then Bcc).
    pub fn recipients(&self) -> impl Iterator<Item = &EmailAddress> {
        self.to_recipients.iter().chain(&self.cc_recipients).chain(&self.bcc_recipients)
    }
}

/// Attachment metadata (the content is downloaded separately into the blob cache).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    /// The attachment id.
    pub id: AttachmentId,
    /// File, item or reference.
    pub kind: AttachmentKind,
    /// The file name (untrusted: never use it as a path without sanitising).
    #[serde(default)]
    pub name: Sensitive,
    /// Size in bytes as reported by Graph (untrusted).
    #[serde(default, deserialize_with = "null_default")]
    pub size: u64,
    /// MIME type, if known.
    #[serde(default)]
    pub content_type: Option<String>,
    /// Whether it is shown inline in the body (a `cid:` image).
    #[serde(default, deserialize_with = "null_default")]
    pub is_inline: bool,
    /// The Content-ID referenced by `cid:` URLs in the body.
    #[serde(default)]
    pub content_id: Option<String>,
    /// When it was last modified.
    #[serde(default)]
    pub last_modified_date_time: Option<Timestamp>,
}
