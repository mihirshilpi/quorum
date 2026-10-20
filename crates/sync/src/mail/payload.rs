//! Outbox entry kinds and payloads for mail writes (built by the engine's `mail.*` commands,
//! replayed by this module's handlers).

use quorum_graph::api::mail::{AutomaticRepliesSetting, FolderRef};
use quorum_model::{FlagStatus, GraphEnum, MailFolderId, Message, MessageId, WellKnownFolder};
use serde::{Deserialize, Serialize};

use super::compose::Compose;

/// Read state, flag, categories or Focused/Other of one message.
pub const KIND_UPDATE: &str = "mail.update";
/// Move one message.
pub const KIND_MOVE: &str = "mail.move";
/// Delete one message permanently.
pub const KIND_DELETE_PERMANENT: &str = "mail.delete_permanent";
/// Send a draft (queued with a delay so it can be undone).
pub const KIND_SEND: &str = "mail.send";
/// Save a local draft to the server's Drafts folder.
pub const KIND_DRAFT_SYNC: &str = "mail.draft_sync";
/// Delete a server draft.
pub const KIND_DRAFT_DELETE: &str = "mail.draft_delete";
/// Change automatic replies.
pub const KIND_AUTO_REPLY: &str = "mail.auto_reply";
/// The outbox ordering key of mailbox settings writes.
pub const SETTINGS_RESOURCE: &str = "mail.settings";

/// The values of a message before an optimistic change (for revert).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Previous {
    /// Read state.
    pub is_read: bool,
    /// Flag.
    pub flag: FlagStatus,
    /// Categories.
    pub categories: Vec<String>,
    /// Focused (vs Other).
    pub focused: bool,
}

/// [`KIND_UPDATE`]: fields to set (`None` = leave).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatePayload {
    /// The message.
    pub id: MessageId,
    /// Read state.
    #[serde(default)]
    pub is_read: Option<bool>,
    /// Flag.
    #[serde(default)]
    pub flag: Option<FlagStatus>,
    /// Categories (replaces the list).
    #[serde(default)]
    pub categories: Option<Vec<String>>,
    /// Focused (`true`) or Other (`false`).
    #[serde(default)]
    pub focused: Option<bool>,
    /// The values before the change.
    #[serde(default)]
    pub previous: Previous,
}

/// Where a message moves: a well-known folder by name, or a folder id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Destination {
    /// A well-known folder (`archive`, `deleteditems`, ...).
    WellKnown(String),
    /// A folder id.
    Id(MailFolderId),
}

impl Destination {
    /// The Graph folder reference.
    #[must_use]
    pub fn folder_ref(&self) -> FolderRef {
        match self {
            Self::WellKnown(name) => FolderRef::WellKnown(WellKnownFolder::from_graph(name)),
            Self::Id(id) => FolderRef::Id(id.clone()),
        }
    }
}

/// [`KIND_MOVE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovePayload {
    /// The message.
    pub id: MessageId,
    /// Where to.
    pub destination: Destination,
    /// The folder it was in (for revert).
    #[serde(default)]
    pub previous_folder: Option<MailFolderId>,
}

/// [`KIND_DELETE_PERMANENT`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletePayload {
    /// The message.
    pub id: MessageId,
    /// The header as it was (restored if the user discards a failed delete).
    #[serde(default)]
    pub message: Option<Message>,
}

/// [`KIND_SEND`]: a snapshot of the draft at the moment Send was pressed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendPayload {
    /// The local draft id.
    pub draft_id: String,
    /// The draft's fields.
    pub compose: Compose,
    /// The draft's HTML body (the user's text; the quote is in `compose`).
    pub body_html: quorum_model::Sensitive,
}

/// [`KIND_DRAFT_SYNC`]: the handler reads the draft's latest state from the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftSyncPayload {
    /// The local draft id.
    pub draft_id: String,
}

/// [`KIND_DRAFT_DELETE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftDeletePayload {
    /// The server draft.
    pub server_id: MessageId,
}

/// [`KIND_AUTO_REPLY`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoReplyPayload {
    /// The new automatic replies setting.
    pub setting: AutomaticRepliesSetting,
}
