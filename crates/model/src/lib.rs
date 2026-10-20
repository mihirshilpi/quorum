//! Quorum domain types: strongly typed Graph ids, mail, calendar, chat, presence and file models.
//!
//! This crate is layer L0: it performs no I/O (apart from [`DateTimeZone::to_timestamp`], which
//! consults the time zone database) and depends only on `serde`, `serde_json`, `jiff` and
//! `thiserror`.
//!
//! # Conventions
//!
//! * **Ids** ([`MessageId`], [`ChatId`], ...) are newtypes with fallible constructors
//!   (M-STRONG-TYPES-GUARD): non-empty, at most [`MAX_ID_LEN`] bytes, no control characters.
//!   Deserialising validates too.
//! * **Personal content** (subjects, bodies, previews, names, addresses, status messages) is held
//!   in [`Sensitive`] or [`MailAddress`], whose `Debug` prints only a length (M-PUBLIC-DEBUG), so
//!   every struct in this crate can derive `Debug` without leaking.
//! * **Enums from Graph** never fail to parse: unknown values become `Unknown(String)`.
//! * **Serde** uses Graph's camelCase field names and enum values, so the types round-trip through
//!   JSON and accept most Graph payloads directly; `quorum-graph` owns the exact wire mapping.
//! * **Time** uses `jiff`: [`jiff::Timestamp`] for instants and [`DateTimeZone`] for Graph's
//!   `{dateTime, timeZone}` pairs (see [`windows_to_iana`]).

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable,
    clippy::indexing_slicing
)]
#![warn(missing_docs)]

mod address;
mod calendar;
mod de;
mod enums;
mod error;
mod files;
mod id;
mod identity;
mod mail;
mod person;
mod presence;
mod sensitive;
mod teams;
mod time;

pub use address::{EmailAddress, MAX_ADDRESS_LEN, MailAddress};
pub use calendar::{
    Attendee, AttendeeAvailability, AttendeeType, Calendar, CalendarColor, CalendarSource, DayOfWeek, Event, EventType,
    FreeBusyStatus, Location, MeetingTimeSuggestion, OnlineMeetingProvider, RecurrencePatternType, RecurrenceRangeType,
    RecurrenceSummary, ResponseStatus, ResponseType, ScheduleInformation, ScheduleItem, Sensitivity, TimeSlot,
    parse_availability_view,
};
pub use enums::GraphEnum;
pub use error::{Error, ErrorKind};
pub use files::{Drive, DriveItem, DriveKind, FileFacet, FolderFacet, List, ListItem, Site};
pub use id::{
    AccountId, AttachmentId, CalendarId, ChannelId, ChatId, ChatMessageId, ConversationId, DriveId, DriveItemId,
    EventId, ListId, ListItemId, MAX_ID_LEN, MailFolderId, MessageId, SiteId, TeamId, UserId,
};
pub use identity::{Identity, IdentityKind};
pub use mail::{
    Attachment, AttachmentKind, BodyType, FlagStatus, Importance, InferenceClassification, MailFolder, Message,
    MessageBody, WellKnownFolder,
};
pub use person::Person;
pub use presence::{Activity, Availability, Presence};
pub use sensitive::Sensitive;
pub use teams::{
    Channel, ChannelMembershipType, Chat, ChatAttachment, ChatBody, ChatImportance, ChatMember, ChatMessage,
    ChatMessageType, ChatPreview, ChatType, Mention, MessageContainer, Reaction, Team,
};
pub use time::{DateTimeZone, TimeZoneName, windows_to_iana};
