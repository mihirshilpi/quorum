//! Strongly typed Graph identifiers (M-STRONG-TYPES-GUARD).

use crate::error::{Error, ErrorKind};

/// The maximum length of an id in bytes.
///
/// Graph ids are opaque; the longest seen in practice (base64 Exchange ids, SharePoint
/// composite ids) are a few hundred bytes. Anything longer is treated as hostile.
pub const MAX_ID_LEN: usize = 1024;

/// Checks the shared id rules: non-empty, at most [`MAX_ID_LEN`] bytes, no control characters.
fn validate(value: &str, type_name: &'static str) -> Result<(), Error> {
    if value.is_empty() {
        return Err(Error::new(ErrorKind::Empty, type_name));
    }
    if value.len() > MAX_ID_LEN {
        return Err(Error::new(ErrorKind::TooLong { max: MAX_ID_LEN }, type_name));
    }
    if value.chars().any(char::is_control) {
        return Err(Error::new(ErrorKind::ControlCharacter, type_name));
    }
    Ok(())
}

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        ///
        /// Construct with [`new`](Self::new) (or `TryFrom`/`FromStr`); the value must be
        /// non-empty, at most [`MAX_ID_LEN`] bytes and free of control characters.
        /// Serialises as a plain string; deserialising applies the same validation.
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            /// Validates and wraps an id.
            ///
            /// # Errors
            ///
            /// Returns an [`Error`] if the value is empty, longer than [`MAX_ID_LEN`] bytes or
            /// contains a control character.
            pub fn new(value: impl Into<String>) -> Result<Self, Error> {
                let value = value.into();
                validate(&value, stringify!($name))?;
                Ok(Self(value))
            }

            /// Returns the id as a string slice.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Returns the owned id string.
            #[must_use]
            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl std::str::FromStr for $name {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self, Error> {
                Self::new(s)
            }
        }

        impl TryFrom<String> for $name {
            type Error = Error;
            fn try_from(value: String) -> Result<Self, Error> {
                Self::new(value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = Error;
            fn try_from(value: &str) -> Result<Self, Error> {
                Self::new(value)
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let value = String::deserialize(d)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

id_type!(
    /// The local id of a signed-in account (one Entra ID identity in one tenant).
    AccountId
);
id_type!(
    /// The Entra ID object id of a user.
    UserId
);
id_type!(
    /// The id of a mail message.
    MessageId
);
id_type!(
    /// The id of a mail conversation (thread), shared by its messages.
    ConversationId
);
id_type!(
    /// The id of a mail folder.
    MailFolderId
);
id_type!(
    /// The id of a mail or event attachment.
    AttachmentId
);
id_type!(
    /// The id of a calendar.
    CalendarId
);
id_type!(
    /// The id of a calendar event (single instance, occurrence, exception or series master).
    EventId
);
id_type!(
    /// The id of a Teams chat.
    ChatId
);
id_type!(
    /// The id of a Teams chat or channel message.
    ChatMessageId
);
id_type!(
    /// The id of a team.
    TeamId
);
id_type!(
    /// The id of a channel within a team.
    ChannelId
);
id_type!(
    /// The id of a OneDrive or SharePoint drive (document library).
    DriveId
);
id_type!(
    /// The id of a file or folder in a drive.
    DriveItemId
);
id_type!(
    /// The id of a SharePoint site.
    SiteId
);
id_type!(
    /// The id of a SharePoint list.
    ListId
);
id_type!(
    /// The id of an item in a SharePoint list.
    ListItemId
);
