//! Teams chats, chat and channel messages, teams and channels.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::address::MailAddress;
use crate::de::null_default;
use crate::enums::graph_enum;
use crate::id::{ChannelId, ChatId, ChatMessageId, TeamId, UserId};
use crate::identity::Identity;
use crate::mail::BodyType;
use crate::sensitive::Sensitive;

graph_enum!(
    /// The kind of a chat.
    ChatType {
        /// A one-on-one chat.
        OneOnOne = "oneOnOne",
        /// A group chat.
        Group = "group",
        /// A meeting chat.
        Meeting = "meeting",
    }
);

/// A chat member.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMember {
    /// The member's user id, if they are a user in a known tenant.
    #[serde(default)]
    pub user_id: Option<UserId>,
    /// Their display name.
    #[serde(default)]
    pub display_name: Sensitive,
    /// Their email address, if known.
    #[serde(default)]
    pub email: Option<MailAddress>,
    /// Roles such as `owner` or `guest`.
    #[serde(default, deserialize_with = "null_default")]
    pub roles: Vec<String>,
}

/// The latest message of a chat, as shown in the chat list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatPreview {
    /// The message id.
    pub id: ChatMessageId,
    /// When it was sent.
    #[serde(default)]
    pub created_date_time: Option<Timestamp>,
    /// Who sent it.
    #[serde(default)]
    pub from: Option<Identity>,
    /// A plain-text preview of the body.
    #[serde(default)]
    pub body_preview: Sensitive,
    /// Whether it was deleted.
    #[serde(default, deserialize_with = "null_default")]
    pub is_deleted: bool,
}

/// A Teams chat (one-on-one, group or meeting).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chat {
    /// The chat id.
    pub id: ChatId,
    /// The topic, for group and meeting chats.
    #[serde(default)]
    pub topic: Option<Sensitive>,
    /// The chat kind.
    pub chat_type: ChatType,
    /// Members (may be empty if not expanded).
    #[serde(default, deserialize_with = "null_default")]
    pub members: Vec<ChatMember>,
    /// The latest message.
    #[serde(default)]
    pub last_message_preview: Option<ChatPreview>,
    /// When the chat was last updated.
    #[serde(default)]
    pub last_updated_date_time: Option<Timestamp>,
    /// When the chat was created.
    #[serde(default)]
    pub created_date_time: Option<Timestamp>,
    /// When the user last read the chat (Graph's `viewpoint.lastMessageReadDateTime`).
    #[serde(default)]
    pub last_message_read_date_time: Option<Timestamp>,
    /// Whether the user hid the chat.
    #[serde(default, deserialize_with = "null_default")]
    pub is_hidden: bool,
    /// The Teams web link.
    #[serde(default)]
    pub web_url: Option<String>,
}

impl Chat {
    /// Returns `true` if the latest message is newer than the user's last read time.
    ///
    /// This is a hint: Graph exposes no unread count for chats.
    #[must_use]
    pub fn has_unread(&self) -> bool {
        let Some(latest) = self.last_message_preview.as_ref().and_then(|p| p.created_date_time) else {
            return false;
        };
        self.last_message_read_date_time.is_none_or(|read| latest > read)
    }
}

/// Where a chat message lives.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageContainer {
    /// In a chat.
    Chat(ChatId),
    /// In a channel of a team.
    #[serde(rename_all = "camelCase")]
    Channel {
        /// The team.
        team_id: TeamId,
        /// The channel.
        channel_id: ChannelId,
    },
}

graph_enum!(
    /// Chat message importance.
    #[derive(Default)]
    ChatImportance {
        /// Normal.
        #[default]
        Normal = "normal",
        /// Important.
        High = "high",
        /// Urgent (repeated notifications).
        Urgent = "urgent",
    }
);

graph_enum!(
    /// The kind of a chat message.
    #[derive(Default)]
    ChatMessageType {
        /// A normal message.
        #[default]
        Message = "message",
        /// A chat event (member added, renamed, ...).
        ChatEvent = "chatEvent",
        /// A typing indicator.
        Typing = "typing",
        /// A system event.
        SystemEventMessage = "systemEventMessage",
    }
);

/// The body of a chat message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatBody {
    /// HTML or text.
    #[serde(default, deserialize_with = "null_default")]
    pub content_type: BodyType,
    /// The content. HTML is untrusted: sanitise with `quorum-sanitize` before display.
    #[serde(default)]
    pub content: Sensitive,
}

/// An @mention in a chat message; `<at id="N">` in the body refers to `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mention {
    /// The index used by `<at id="N">` in the body.
    pub id: u32,
    /// The mention text as shown.
    #[serde(default)]
    pub mention_text: Sensitive,
    /// Who was mentioned.
    #[serde(default)]
    pub mentioned: Option<Identity>,
}

/// A reaction on a chat message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reaction {
    /// The reaction, e.g. `like` or an emoji.
    #[serde(default)]
    pub reaction_type: String,
    /// When it was added.
    #[serde(default)]
    pub created_date_time: Option<Timestamp>,
    /// Who reacted.
    #[serde(default)]
    pub user: Option<Identity>,
}

/// An attachment on a chat message (file reference, card, hosted content).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatAttachment {
    /// The attachment id.
    #[serde(default)]
    pub id: Option<String>,
    /// The content type, e.g. `reference` or `application/vnd.microsoft.card.adaptive`.
    #[serde(default)]
    pub content_type: String,
    /// The URL of the content (untrusted).
    #[serde(default)]
    pub content_url: Option<String>,
    /// The file name.
    #[serde(default)]
    pub name: Option<Sensitive>,
    /// Inline content (e.g. card JSON).
    #[serde(default)]
    pub content: Option<Sensitive>,
    /// A thumbnail URL (untrusted).
    #[serde(default)]
    pub thumbnail_url: Option<String>,
}

/// A chat message or channel post/reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    /// The message id.
    pub id: ChatMessageId,
    /// The chat or channel it is in.
    pub container: MessageContainer,
    /// The parent message, for channel replies.
    #[serde(default)]
    pub reply_to_id: Option<ChatMessageId>,
    /// The sender (user or app); `None` for system messages.
    #[serde(default)]
    pub from: Option<Identity>,
    /// When it was created.
    #[serde(default)]
    pub created_date_time: Option<Timestamp>,
    /// When it was last modified (including reactions).
    #[serde(default)]
    pub last_modified_date_time: Option<Timestamp>,
    /// When its text was last edited.
    #[serde(default)]
    pub last_edited_date_time: Option<Timestamp>,
    /// When it was deleted.
    #[serde(default)]
    pub deleted_date_time: Option<Timestamp>,
    /// The subject (channel posts only).
    #[serde(default)]
    pub subject: Option<Sensitive>,
    /// The body.
    #[serde(default)]
    pub body: ChatBody,
    /// Mentions referenced by `<at>` elements in the body.
    #[serde(default, deserialize_with = "null_default")]
    pub mentions: Vec<Mention>,
    /// Reactions.
    #[serde(default, deserialize_with = "null_default")]
    pub reactions: Vec<Reaction>,
    /// Attachments.
    #[serde(default, deserialize_with = "null_default")]
    pub attachments: Vec<ChatAttachment>,
    /// Importance.
    #[serde(default, deserialize_with = "null_default")]
    pub importance: ChatImportance,
    /// Message, event or system message.
    #[serde(default, deserialize_with = "null_default")]
    pub message_type: ChatMessageType,
    /// The Teams web link.
    #[serde(default)]
    pub web_url: Option<String>,
}

impl ChatMessage {
    /// Returns `true` if the message was deleted.
    #[must_use]
    pub fn is_deleted(&self) -> bool {
        self.deleted_date_time.is_some()
    }

    /// Returns `true` if the message text was edited.
    #[must_use]
    pub fn is_edited(&self) -> bool {
        self.last_edited_date_time.is_some()
    }
}

/// A team the user belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Team {
    /// The team id.
    pub id: TeamId,
    /// The team name.
    #[serde(default)]
    pub display_name: Sensitive,
    /// The description.
    #[serde(default)]
    pub description: Option<Sensitive>,
    /// Whether the team is archived (read-only).
    #[serde(default, deserialize_with = "null_default")]
    pub is_archived: bool,
    /// The Teams web link.
    #[serde(default)]
    pub web_url: Option<String>,
}

graph_enum!(
    /// Who can see a channel.
    #[derive(Default)]
    ChannelMembershipType {
        /// Everyone in the team.
        #[default]
        Standard = "standard",
        /// Only channel members.
        Private = "private",
        /// Members, possibly from other teams or tenants.
        Shared = "shared",
    }
);

/// A channel in a team.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    /// The channel id.
    pub id: ChannelId,
    /// The team it belongs to.
    #[serde(default)]
    pub team_id: Option<TeamId>,
    /// The channel name.
    #[serde(default)]
    pub display_name: Sensitive,
    /// The description.
    #[serde(default)]
    pub description: Option<Sensitive>,
    /// Standard, private or shared.
    #[serde(default, deserialize_with = "null_default")]
    pub membership_type: ChannelMembershipType,
    /// The channel's email address, if enabled.
    #[serde(default)]
    pub email: Option<Sensitive>,
    /// When it was created.
    #[serde(default)]
    pub created_date_time: Option<Timestamp>,
    /// The Teams web link.
    #[serde(default)]
    pub web_url: Option<String>,
}
