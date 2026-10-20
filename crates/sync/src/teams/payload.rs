//! Outbox payloads for Teams chat and channel writes.
//!
//! The engine builds these (and applies the optimistic local change in the same store
//! transaction); the handlers in [`super::outbox`] replay them against Graph. They travel as JSON
//! in the encrypted outbox, so field names are part of the on-disk format: add fields with
//! `#[serde(default)]`, never rename them.

use quorum_graph::api::chat::MessageRef;
use quorum_model::{ChannelId, ChatId, ChatMessage, ChatMessageId, MessageContainer, TeamId, UserId};
use serde::{Deserialize, Serialize};

/// Where a message lives or goes, in a form that survives JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Target {
    /// A chat (send) or a chat message (`message` set).
    Chat {
        /// The chat.
        chat: ChatId,
        /// The message, for edit/delete/react.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<ChatMessageId>,
    },
    /// A channel (new post) or a top-level post (`message` set).
    Channel {
        /// The team.
        team: TeamId,
        /// The channel.
        channel: ChannelId,
        /// The post, for reply/edit/delete/react.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<ChatMessageId>,
        /// The reply (for edit/delete/react of a reply).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply: Option<ChatMessageId>,
    },
}

impl Target {
    /// The Graph message reference, when the target names a message.
    #[must_use]
    pub fn message_ref(&self) -> Option<MessageRef> {
        match self {
            Self::Chat { chat, message: Some(m) } => Some(MessageRef::Chat { chat: chat.clone(), message: m.clone() }),
            Self::Channel { team, channel, message: Some(m), reply: None } => {
                Some(MessageRef::ChannelPost { team: team.clone(), channel: channel.clone(), message: m.clone() })
            }
            Self::Channel { team, channel, message: Some(m), reply: Some(r) } => Some(MessageRef::ChannelReply {
                team: team.clone(),
                channel: channel.clone(),
                message: m.clone(),
                reply: r.clone(),
            }),
            _ => None,
        }
    }

    /// The container the target lives in.
    #[must_use]
    pub fn container(&self) -> MessageContainer {
        match self {
            Self::Chat { chat, .. } => MessageContainer::Chat(chat.clone()),
            Self::Channel { team, channel, .. } => {
                MessageContainer::Channel { team_id: team.clone(), channel_id: channel.clone() }
            }
        }
    }

    /// The id of the message the target names (the reply when there is one).
    #[must_use]
    pub fn message_id(&self) -> Option<&ChatMessageId> {
        match self {
            Self::Chat { message, .. } => message.as_ref(),
            Self::Channel { reply: Some(r), .. } => Some(r),
            Self::Channel { message, .. } => message.as_ref(),
        }
    }

    /// The outbox ordering key and sync resource key: `chat:<id>` or `channel:<team>/<channel>`.
    #[must_use]
    pub fn resource_key(&self) -> String {
        match self {
            Self::Chat { chat, .. } => super::chat_key(chat),
            Self::Channel { team, channel, .. } => super::channel_key(team, channel),
        }
    }
}

/// Who an outgoing mention refers to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MentionKind {
    /// A user (`target` is the user id).
    User,
    /// A channel (`target` is the channel id).
    Channel,
    /// A team (`target` is the team id).
    Team,
    /// A tag (`target` is the tag id).
    Tag,
}

/// An `<at id="N">` mention in an outgoing body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MentionPayload {
    /// The index used in the body.
    pub id: u32,
    /// The text shown.
    pub text: String,
    /// What kind of thing is mentioned.
    #[serde(default = "user_kind")]
    pub kind: MentionKind,
    /// The id of the mentioned user, channel, team or tag.
    pub target: String,
}

fn user_kind() -> MentionKind {
    MentionKind::User
}

/// An inline image whose bytes wait in the blob cache.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagePayload {
    /// The temporary id referenced as `../hostedContents/{id}/$value` in the body.
    pub temp_id: String,
    /// `image/*` type.
    pub content_type: String,
    /// Blob cache key of the bytes.
    pub blob: String,
}

/// A file attachment whose bytes wait in the blob cache; uploaded to OneDrive before sending.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilePayload {
    /// File name.
    pub name: String,
    /// MIME type.
    #[serde(default)]
    pub content_type: String,
    /// Blob cache key of the bytes.
    pub blob: String,
}

/// A quoted chat message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotePayload {
    /// The quoted message.
    pub message_id: ChatMessageId,
    /// Short preview text.
    #[serde(default)]
    pub preview: String,
    /// Sender user id.
    #[serde(default)]
    pub sender_id: Option<UserId>,
    /// Sender name.
    #[serde(default)]
    pub sender_name: Option<String>,
}

/// `chat.send`, `channel.post`, `channel.reply`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendPayload {
    /// Chat, channel, or post (reply).
    pub target: Target,
    /// Id of the optimistic local message, replaced by the real one on success.
    pub local_id: ChatMessageId,
    /// Sanitised HTML body (without image tags; those are appended from `images`).
    pub html: String,
    /// Subject (channel posts).
    #[serde(default)]
    pub subject: Option<String>,
    /// `normal`, `high` or `urgent`.
    #[serde(default)]
    pub importance: Option<String>,
    /// Mentions referenced from the body.
    #[serde(default)]
    pub mentions: Vec<MentionPayload>,
    /// Inline images.
    #[serde(default)]
    pub images: Vec<ImagePayload>,
    /// File attachments.
    #[serde(default)]
    pub files: Vec<FilePayload>,
    /// Quoted message (chats).
    #[serde(default)]
    pub quote: Option<QuotePayload>,
}

/// `chat.edit`, `channel.edit`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditPayload {
    /// The message.
    pub target: Target,
    /// New sanitised HTML body.
    pub html: String,
    /// Mentions in the new body.
    #[serde(default)]
    pub mentions: Vec<MentionPayload>,
    /// The message as it was, restored if the user discards a failed edit.
    #[serde(default)]
    pub before: Option<ChatMessage>,
}

/// `chat.delete`, `channel.delete`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletePayload {
    /// The message.
    pub target: Target,
    /// The message as it was, restored if the user discards a failed delete.
    #[serde(default)]
    pub before: Option<ChatMessage>,
}

/// `chat.react`, `chat.unreact`, `channel.react`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactPayload {
    /// The message.
    pub target: Target,
    /// Unicode emoji (or a legacy name such as `like`).
    pub reaction: String,
    /// Remove instead of add.
    #[serde(default)]
    pub remove: bool,
    /// The message as it was, restored on discard.
    #[serde(default)]
    pub before: Option<ChatMessage>,
}

/// `chat.mark_read`, `chat.mark_unread`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadPayload {
    /// The chat.
    pub chat: ChatId,
    /// The signed-in user.
    pub user: UserId,
    /// The user's tenant.
    pub tenant: String,
    /// For unread: the instant from which messages count as unread.
    #[serde(default)]
    pub last_read: Option<jiff::Timestamp>,
}

/// `chat.create`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatePayload {
    /// Id of the optimistic local chat.
    pub local_id: ChatId,
    /// `oneOnOne` or `group`.
    pub chat_type: String,
    /// Every member including the signed-in user (ids or UPNs).
    pub members: Vec<UserId>,
    /// Topic (group chats).
    #[serde(default)]
    pub topic: Option<String>,
    /// First message (sanitised HTML), sent after the chat exists.
    #[serde(default)]
    pub message: Option<String>,
}

/// `chat.add_member`, `chat.remove_member`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberPayload {
    /// The chat.
    pub chat: ChatId,
    /// The user (id or UPN).
    pub user: UserId,
    /// Share all history with an added member.
    #[serde(default)]
    pub share_history: bool,
}

/// `chat.pin_message`, `chat.unpin_message`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinPayload {
    /// The chat.
    pub chat: ChatId,
    /// The message.
    pub message: ChatMessageId,
}
