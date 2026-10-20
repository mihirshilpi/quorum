//! Teams presence.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::de::null_default;
use crate::enums::graph_enum;
use crate::id::UserId;
use crate::sensitive::Sensitive;

graph_enum!(
    /// Base presence (Graph's `availability`).
    #[derive(Default)]
    Availability {
        /// Available.
        Available = "Available",
        /// Available but idle.
        AvailableIdle = "AvailableIdle",
        /// Away.
        Away = "Away",
        /// Be right back.
        BeRightBack = "BeRightBack",
        /// Busy.
        Busy = "Busy",
        /// Busy but idle.
        BusyIdle = "BusyIdle",
        /// Do not disturb.
        DoNotDisturb = "DoNotDisturb",
        /// Offline.
        Offline = "Offline",
        /// Unknown to Graph.
        #[default]
        PresenceUnknown = "PresenceUnknown",
    }
);

graph_enum!(
    /// Presence detail (Graph's `activity`).
    #[derive(Default)]
    Activity {
        /// Available.
        Available = "Available",
        /// Away.
        Away = "Away",
        /// Be right back.
        BeRightBack = "BeRightBack",
        /// Busy.
        Busy = "Busy",
        /// Do not disturb.
        DoNotDisturb = "DoNotDisturb",
        /// In a call.
        InACall = "InACall",
        /// In a conference call.
        InAConferenceCall = "InAConferenceCall",
        /// Inactive.
        Inactive = "Inactive",
        /// In a meeting.
        InAMeeting = "InAMeeting",
        /// Offline.
        Offline = "Offline",
        /// Off work.
        OffWork = "OffWork",
        /// Out of office.
        OutOfOffice = "OutOfOffice",
        /// Unknown to Graph.
        #[default]
        PresenceUnknown = "PresenceUnknown",
        /// Presenting.
        Presenting = "Presenting",
        /// Focusing (urgent interruptions only).
        UrgentInterruptionsOnly = "UrgentInterruptionsOnly",
    }
);

/// A user's presence.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Presence {
    /// Whose presence this is.
    #[serde(default)]
    pub user_id: Option<UserId>,
    /// Base presence.
    #[serde(default, deserialize_with = "null_default")]
    pub availability: Availability,
    /// Detail.
    #[serde(default, deserialize_with = "null_default")]
    pub activity: Activity,
    /// The status message, if set.
    #[serde(default)]
    pub status_message: Option<Sensitive>,
    /// When the status message expires.
    #[serde(default)]
    pub expiry: Option<Timestamp>,
}

impl Presence {
    /// Returns the status message if it is set and not expired at `now`.
    #[must_use]
    pub fn current_status_message(&self, now: Timestamp) -> Option<&str> {
        let message = self.status_message.as_ref().filter(|m| !m.is_empty())?;
        match self.expiry {
            Some(expiry) if expiry <= now => None,
            _ => Some(message.as_str()),
        }
    }
}
