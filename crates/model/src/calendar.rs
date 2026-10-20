//! Calendars, events, free/busy and meeting time suggestions.

use jiff::Timestamp;
use jiff::civil::Date;
use serde::{Deserialize, Serialize};

use crate::address::EmailAddress;
use crate::de::null_default;
use crate::enums::graph_enum;
use crate::id::{CalendarId, EventId};
use crate::sensitive::Sensitive;
use crate::time::DateTimeZone;

graph_enum!(
    /// A calendar's colour theme (Graph's `calendarColor`).
    #[derive(Default)]
    CalendarColor {
        /// Automatic.
        #[default]
        Auto = "auto",
        /// Light blue.
        LightBlue = "lightBlue",
        /// Light green.
        LightGreen = "lightGreen",
        /// Light orange.
        LightOrange = "lightOrange",
        /// Light grey.
        LightGray = "lightGray",
        /// Light yellow.
        LightYellow = "lightYellow",
        /// Light teal.
        LightTeal = "lightTeal",
        /// Light pink.
        LightPink = "lightPink",
        /// Light brown.
        LightBrown = "lightBrown",
        /// Light red.
        LightRed = "lightRed",
    }
);

graph_enum!(
    /// Where a calendar comes from in the merged view.
    #[derive(Default)]
    CalendarSource {
        /// One of the user's own calendars.
        #[default]
        Own = "own",
        /// A calendar someone shared with the user.
        Shared = "shared",
        /// A Microsoft 365 group calendar.
        Group = "group",
        /// Another person's calendar the user opened (free/busy or details).
        OpenedUser = "openedUser",
    }
);

/// A calendar layer in the merged view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Calendar {
    /// The calendar id.
    pub id: CalendarId,
    /// The calendar's name.
    #[serde(default)]
    pub name: Sensitive,
    /// The colour theme.
    #[serde(default, deserialize_with = "null_default")]
    pub color: CalendarColor,
    /// An explicit `#rrggbb` colour, if set.
    #[serde(default)]
    pub hex_color: Option<String>,
    /// The owner.
    #[serde(default)]
    pub owner: Option<EmailAddress>,
    /// Whether the user can create and edit events.
    #[serde(default, deserialize_with = "null_default")]
    pub can_edit: bool,
    /// Whether the user can share it.
    #[serde(default, deserialize_with = "null_default")]
    pub can_share: bool,
    /// Whether it is the owner's default calendar.
    #[serde(default, deserialize_with = "null_default")]
    pub is_default_calendar: bool,
    /// Where it comes from.
    #[serde(default, deserialize_with = "null_default")]
    pub source: CalendarSource,
}

graph_enum!(
    /// Free/busy state (Graph's `freeBusyStatus`, used for `showAs` and schedules).
    #[derive(Default)]
    FreeBusyStatus {
        /// Free.
        Free = "free",
        /// Tentative.
        Tentative = "tentative",
        /// Busy.
        #[default]
        Busy = "busy",
        /// Out of office.
        Oof = "oof",
        /// Working elsewhere.
        WorkingElsewhere = "workingElsewhere",
    }
);

graph_enum!(
    /// An attendee's (or the user's) response to a meeting.
    #[derive(Default)]
    ResponseType {
        /// No response.
        #[default]
        None = "none",
        /// The user is the organizer.
        Organizer = "organizer",
        /// Tentatively accepted.
        TentativelyAccepted = "tentativelyAccepted",
        /// Accepted.
        Accepted = "accepted",
        /// Declined.
        Declined = "declined",
        /// Not responded.
        NotResponded = "notResponded",
    }
);

graph_enum!(
    /// Whether an attendee is required, optional or a resource.
    #[derive(Default)]
    AttendeeType {
        /// Required.
        #[default]
        Required = "required",
        /// Optional.
        Optional = "optional",
        /// A room or equipment.
        Resource = "resource",
    }
);

graph_enum!(
    /// Event sensitivity.
    #[derive(Default)]
    Sensitivity {
        /// Normal.
        #[default]
        Normal = "normal",
        /// Personal.
        Personal = "personal",
        /// Private.
        Private = "private",
        /// Confidential.
        Confidential = "confidential",
    }
);

graph_enum!(
    /// The kind of an event in a recurring series.
    #[derive(Default)]
    EventType {
        /// A one-off event.
        #[default]
        SingleInstance = "singleInstance",
        /// An occurrence of a series.
        Occurrence = "occurrence",
        /// A modified occurrence.
        Exception = "exception",
        /// The series master.
        SeriesMaster = "seriesMaster",
    }
);

graph_enum!(
    /// The online meeting provider of an event.
    OnlineMeetingProvider {
        /// Microsoft Teams.
        TeamsForBusiness = "teamsForBusiness",
        /// Skype for Business.
        SkypeForBusiness = "skypeForBusiness",
        /// Skype (consumer).
        SkypeForConsumer = "skypeForConsumer",
    }
);

graph_enum!(
    /// A recurrence pattern type.
    RecurrencePatternType {
        /// Every N days.
        Daily = "daily",
        /// Every N weeks on given days.
        Weekly = "weekly",
        /// On a day of the month.
        AbsoluteMonthly = "absoluteMonthly",
        /// On e.g. the second Tuesday of the month.
        RelativeMonthly = "relativeMonthly",
        /// On a date each year.
        AbsoluteYearly = "absoluteYearly",
        /// On e.g. the last Friday of March.
        RelativeYearly = "relativeYearly",
    }
);

graph_enum!(
    /// How a recurrence ends.
    RecurrenceRangeType {
        /// On an end date.
        EndDate = "endDate",
        /// Never.
        NoEnd = "noEnd",
        /// After a number of occurrences.
        Numbered = "numbered",
    }
);

graph_enum!(
    /// A day of the week.
    DayOfWeek {
        /// Sunday.
        Sunday = "sunday",
        /// Monday.
        Monday = "monday",
        /// Tuesday.
        Tuesday = "tuesday",
        /// Wednesday.
        Wednesday = "wednesday",
        /// Thursday.
        Thursday = "thursday",
        /// Friday.
        Friday = "friday",
        /// Saturday.
        Saturday = "saturday",
    }
);

/// A response and when it was given.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseStatus {
    /// The response.
    #[serde(default, deserialize_with = "null_default")]
    pub response: ResponseType,
    /// When it was given.
    #[serde(default)]
    pub time: Option<Timestamp>,
}

/// An event location.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Location {
    /// The name shown to people.
    #[serde(default)]
    pub display_name: Sensitive,
    /// A URI for the location (e.g. a room resource).
    #[serde(default)]
    pub location_uri: Option<String>,
    /// The room's email address, for rooms.
    #[serde(default)]
    pub location_email_address: Option<Sensitive>,
}

/// An event attendee.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attendee {
    /// Who.
    #[serde(default)]
    pub email_address: EmailAddress,
    /// Required, optional or resource.
    #[serde(rename = "type", default, deserialize_with = "null_default")]
    pub kind: AttendeeType,
    /// Their response.
    #[serde(default, deserialize_with = "null_default")]
    pub status: ResponseStatus,
}

/// A human-oriented summary of an event's recurrence (pattern and range flattened).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecurrenceSummary {
    /// The pattern type.
    pub pattern: RecurrencePatternType,
    /// Every N days/weeks/months/years.
    #[serde(default, deserialize_with = "null_default")]
    pub interval: u32,
    /// The days for weekly and relative patterns.
    #[serde(default, deserialize_with = "null_default")]
    pub days_of_week: Vec<DayOfWeek>,
    /// The day of the month for absolute patterns.
    #[serde(default)]
    pub day_of_month: Option<u8>,
    /// How the series ends.
    #[serde(default)]
    pub range_type: Option<RecurrenceRangeType>,
    /// The first date of the series.
    #[serde(default)]
    pub start_date: Option<Date>,
    /// The last date, for `endDate` ranges.
    #[serde(default)]
    pub end_date: Option<Date>,
    /// The number of occurrences, for `numbered` ranges.
    #[serde(default)]
    pub number_of_occurrences: Option<u32>,
}

/// A calendar event (or occurrence).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    /// The event id.
    pub id: EventId,
    /// The calendar it is on.
    #[serde(default)]
    pub calendar_id: Option<CalendarId>,
    /// The subject.
    #[serde(default)]
    pub subject: Sensitive,
    /// Start time.
    pub start: DateTimeZone,
    /// End time.
    pub end: DateTimeZone,
    /// Whether it is an all-day event.
    #[serde(default, deserialize_with = "null_default")]
    pub is_all_day: bool,
    /// The primary location.
    #[serde(default)]
    pub location: Option<Location>,
    /// All locations.
    #[serde(default, deserialize_with = "null_default")]
    pub locations: Vec<Location>,
    /// The organizer.
    #[serde(default)]
    pub organizer: Option<EmailAddress>,
    /// Attendees.
    #[serde(default, deserialize_with = "null_default")]
    pub attendees: Vec<Attendee>,
    /// How the time shows on free/busy.
    #[serde(default, deserialize_with = "null_default")]
    pub show_as: FreeBusyStatus,
    /// Single instance, occurrence, exception or series master.
    #[serde(default, rename = "type", deserialize_with = "null_default")]
    pub event_type: EventType,
    /// The recurrence, for series masters.
    #[serde(default)]
    pub recurrence: Option<RecurrenceSummary>,
    /// The series master, for occurrences and exceptions.
    #[serde(default)]
    pub series_master_id: Option<EventId>,
    /// Whether it is an online meeting.
    #[serde(default, deserialize_with = "null_default")]
    pub is_online_meeting: bool,
    /// The online meeting provider.
    #[serde(default)]
    pub online_meeting_provider: Option<OnlineMeetingProvider>,
    /// The join URL of the online meeting (untrusted: open only via the system browser).
    #[serde(default)]
    pub online_meeting_join_url: Option<String>,
    /// The user's response.
    #[serde(default, deserialize_with = "null_default")]
    pub response_status: ResponseStatus,
    /// Sensitivity.
    #[serde(default, deserialize_with = "null_default")]
    pub sensitivity: Sensitivity,
    /// Plain-text preview of the body.
    #[serde(default)]
    pub body_preview: Sensitive,
    /// Whether it was cancelled.
    #[serde(default, deserialize_with = "null_default")]
    pub is_cancelled: bool,
    /// Whether the user is the organizer.
    #[serde(default, deserialize_with = "null_default")]
    pub is_organizer: bool,
    /// Whether a reminder is set.
    #[serde(default, deserialize_with = "null_default")]
    pub is_reminder_on: bool,
    /// Minutes before start the reminder fires.
    #[serde(default)]
    pub reminder_minutes_before_start: Option<u32>,
    /// The Outlook on the web link.
    #[serde(default)]
    pub web_link: Option<String>,
}

impl Event {
    /// Returns the subject text.
    #[must_use]
    pub fn subject(&self) -> &str {
        self.subject.as_str()
    }

    /// Returns `true` if this event is part of a recurring series.
    #[must_use]
    pub fn is_recurring(&self) -> bool {
        matches!(self.event_type, EventType::Occurrence | EventType::Exception | EventType::SeriesMaster)
    }
}

/// A start and end time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeSlot {
    /// Start.
    pub start: DateTimeZone,
    /// End.
    pub end: DateTimeZone,
}

/// A busy block in someone's schedule (from `getSchedule`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleItem {
    /// The free/busy state.
    #[serde(default, deserialize_with = "null_default")]
    pub status: FreeBusyStatus,
    /// Start.
    pub start: DateTimeZone,
    /// End.
    pub end: DateTimeZone,
    /// The subject, if the caller may see details.
    #[serde(default)]
    pub subject: Option<Sensitive>,
    /// The location, if the caller may see details.
    #[serde(default)]
    pub location: Option<Sensitive>,
    /// Whether the event is private.
    #[serde(default, deserialize_with = "null_default")]
    pub is_private: bool,
}

/// One person's or room's schedule (from `getSchedule`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleInformation {
    /// The address the schedule was requested for.
    #[serde(default)]
    pub schedule_id: Sensitive,
    /// Graph's compact availability string, one digit per interval (see
    /// [`parse_availability_view`]).
    #[serde(default)]
    pub availability_view: String,
    /// Busy blocks.
    #[serde(default, deserialize_with = "null_default")]
    pub schedule_items: Vec<ScheduleItem>,
    /// An error message from Graph for this schedule (e.g. no permission).
    #[serde(default)]
    pub error: Option<String>,
}

impl ScheduleInformation {
    /// Returns the availability view as one status per interval.
    #[must_use]
    pub fn availability(&self) -> Vec<FreeBusyStatus> {
        parse_availability_view(&self.availability_view)
    }
}

/// Parses Graph's `availabilityView` string: `0` free, `1` tentative, `2` busy, `3` out of
/// office, `4` working elsewhere; anything else is `Unknown`.
///
/// ```
/// use quorum_model::{FreeBusyStatus, parse_availability_view};
/// assert_eq!(parse_availability_view("02"), vec![FreeBusyStatus::Free, FreeBusyStatus::Busy]);
/// ```
#[must_use]
pub fn parse_availability_view(view: &str) -> Vec<FreeBusyStatus> {
    view.chars()
        .map(|c| match c {
            '0' => FreeBusyStatus::Free,
            '1' => FreeBusyStatus::Tentative,
            '2' => FreeBusyStatus::Busy,
            '3' => FreeBusyStatus::Oof,
            '4' => FreeBusyStatus::WorkingElsewhere,
            other => FreeBusyStatus::Unknown(other.to_string()),
        })
        .collect()
}

/// One attendee's availability in a [`MeetingTimeSuggestion`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttendeeAvailability {
    /// Who.
    #[serde(default)]
    pub attendee: EmailAddress,
    /// Their availability in the slot.
    #[serde(default, deserialize_with = "null_default")]
    pub availability: FreeBusyStatus,
}

/// A suggested meeting time (from `findMeetingTimes`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeetingTimeSuggestion {
    /// The suggested slot.
    pub meeting_time_slot: TimeSlot,
    /// Confidence 0–100 (untrusted; may be out of range).
    #[serde(default, deserialize_with = "null_default")]
    pub confidence: f64,
    /// The organizer's availability.
    #[serde(default, deserialize_with = "null_default")]
    pub organizer_availability: FreeBusyStatus,
    /// Each attendee's availability.
    #[serde(default, deserialize_with = "null_default")]
    pub attendee_availability: Vec<AttendeeAvailability>,
    /// Suggested locations.
    #[serde(default, deserialize_with = "null_default")]
    pub locations: Vec<Location>,
    /// Why this slot was suggested.
    #[serde(default)]
    pub suggestion_reason: Option<String>,
}

impl MeetingTimeSuggestion {
    /// Returns the confidence clamped to 0–100 (0 if not finite).
    #[must_use]
    pub fn confidence_percent(&self) -> f64 {
        if self.confidence.is_finite() { self.confidence.clamp(0.0, 100.0) } else { 0.0 }
    }
}
