//! Outlook calendar: calendars and groups (own and other people's), calendar views and their
//! delta, events (CRUD, RSVP, reminders, series instances, Teams meetings), free/busy
//! (`getSchedule`), `findMeetingTimes` and rooms (`/places`).
//!
//! Conventions (see also [`super::mail`]):
//! - Event requests carry `Prefer: IdType="ImmutableId"` and `outlook.timezone="UTC"`, so ids are
//!   stable and every `start`/`end` comes back in UTC (the UI converts to the viewer's zone).
//! - Calendar views need a range; Graph expands recurring series into occurrences inside it.
//! - [`Calendar::source`] and [`Event::calendar_id`] are not in Graph's JSON; the wrappers fill
//!   them from the request context (see [`calendar_from_json`]).

use std::time::Duration;

use jiff::Timestamp;
use quorum_model::{
    Attendee, AttendeeAvailability, AttendeeType, Calendar, CalendarId, CalendarSource, DateTimeZone, DayOfWeek,
    EmailAddress, Event, EventId, EventType, FreeBusyStatus, GraphEnum, Location, MailAddress, MeetingTimeSuggestion,
    MessageBody, OnlineMeetingProvider, RecurrencePatternType, RecurrenceRangeType, RecurrenceSummary, ResponseStatus,
    ScheduleInformation, ScheduleItem, Sensitive, Sensitivity, TimeSlot,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::mail::{
    Change, Mailbox, PREFER_HTML_BODY, PREFER_IMMUTABLE_ID, Recipient, clamp_top, config, is_removed, mapped_delta,
    mapped_page, null_default, recipient_json, removed_id, segment, with_prefer,
};
use crate::client::Client;
use crate::error::{Error, ErrorKind, Result};
use crate::paging::{DeltaPage, Page};
use crate::request::{Method, Request, ResourceFamily};

/// `Prefer` value asking for event times in UTC.
pub const PREFER_UTC: &str = "outlook.timezone=\"UTC\"";
/// Default page size for calendar views.
pub const DEFAULT_VIEW_PAGE_SIZE: u32 = 100;
/// Largest page size for calendar collections.
pub const MAX_VIEW_PAGE_SIZE: u32 = 1000;
/// Schedules sent per `getSchedule` request. Graph documents no hard cap for `getSchedule`;
/// Exchange's availability service rejects more than 100 mailboxes per call and large requests
/// get slow and throttled, so [`Client::get_schedule`] chunks at 20 (the same size as a `$batch`) and
/// concatenates the results in input order.
pub const MAX_SCHEDULES_PER_REQUEST: usize = 20;
/// Longest window `getSchedule` accepts (62 days).
pub const MAX_SCHEDULE_WINDOW: Duration = Duration::from_secs(62 * 24 * 3600);
/// Fields of an event the store keeps.
pub const EVENT_SELECT: &str = "id,subject,start,end,isAllDay,location,locations,organizer,attendees,showAs,type,\
recurrence,seriesMasterId,isOnlineMeeting,onlineMeetingProvider,onlineMeeting,responseStatus,sensitivity,\
bodyPreview,isCancelled,isOrganizer,isReminderOn,reminderMinutesBeforeStart,webLink";
/// Fields of a calendar the store keeps.
pub const CALENDAR_SELECT: &str = "id,name,color,hexColor,owner,canEdit,canShare,isDefaultCalendar";

fn cal_req(mailbox: &Mailbox, method: Method, rest: &str) -> Result<Request> {
    Ok(with_prefer(mailbox.request(method, rest, ResourceFamily::Calendar)?, &[PREFER_IMMUTABLE_ID, PREFER_UTC]))
}

fn event_path(id: &EventId) -> Result<String> {
    Ok(format!("/events/{}", segment(id.as_str())?))
}

// ---------------------------------------------------------------------------------------------
// Calendars and calendar groups.
// ---------------------------------------------------------------------------------------------

/// How to decide [`Calendar::source`] for calendars listed from a mailbox.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceContext {
    /// The signed-in user's address; when known, a calendar owned by someone else is `Shared`.
    pub me: Option<MailAddress>,
}

/// Maps Graph calendar JSON, setting `source`:
/// - from another user's mailbox ([`Mailbox::User`]): `OpenedUser`;
/// - from `/me`: `Own` when the owner is `ctx.me`, `Shared` when it is someone else; without an
///   owner match, Graph's `canShare` decides (only the owner can share a calendar).
#[must_use]
pub fn calendar_from_json(v: Value, mailbox: &Mailbox, ctx: &SourceContext) -> Option<Calendar> {
    let mut cal: Calendar = serde_json::from_value(v).ok()?;
    cal.source = match mailbox {
        Mailbox::User(_) => CalendarSource::OpenedUser,
        Mailbox::Me => {
            let owner = cal.owner.as_ref().and_then(|o| o.address.as_ref());
            match (owner, &ctx.me) {
                (Some(owner), Some(me)) if owner.eq_ignore_case(me) => CalendarSource::Own,
                (Some(_), Some(_)) => CalendarSource::Shared,
                _ if cal.can_share => CalendarSource::Own,
                _ => CalendarSource::Shared,
            }
        }
    };
    Some(cal)
}

/// Builds `GET {mailbox}/calendars`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox.
pub fn list_calendars_request(mailbox: &Mailbox) -> Result<Request> {
    Ok(cal_req(mailbox, Method::Get, "/calendars")?
        .query("$select", CALENDAR_SELECT)
        .query("$top", MAX_VIEW_PAGE_SIZE.to_string()))
}

/// A calendar group ("My calendars", "Other calendars", ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarGroup {
    /// Group id.
    pub id: String,
    /// Name.
    pub name: Sensitive,
    /// Class id (identifies the group across clients).
    pub class_id: Option<String>,
}

fn group_from_json(v: Value) -> Option<CalendarGroup> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Wire {
        id: String,
        #[serde(default)]
        name: Sensitive,
        #[serde(default)]
        class_id: Option<String>,
    }
    let w: Wire = serde_json::from_value(v).ok()?;
    if w.id.is_empty() || w.id.len() > quorum_model::MAX_ID_LEN {
        return None;
    }
    Some(CalendarGroup { id: w.id, name: w.name, class_id: w.class_id })
}

/// Builds `GET {mailbox}/calendarGroups`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox.
pub fn list_calendar_groups_request(mailbox: &Mailbox) -> Result<Request> {
    Ok(cal_req(mailbox, Method::Get, "/calendarGroups")?.query("$select", "id,name,classId"))
}

/// Builds `GET {mailbox}/calendarGroups/{id}/calendars`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid group id.
pub fn group_calendars_request(mailbox: &Mailbox, group_id: &str) -> Result<Request> {
    Ok(cal_req(mailbox, Method::Get, &format!("/calendarGroups/{}/calendars", segment(group_id)?))?
        .query("$select", CALENDAR_SELECT))
}

// ---------------------------------------------------------------------------------------------
// Events: mapping.
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WirePattern {
    #[serde(rename = "type")]
    kind: RecurrencePatternType,
    #[serde(default, deserialize_with = "null_default")]
    interval: u32,
    #[serde(default, deserialize_with = "null_default")]
    days_of_week: Vec<DayOfWeek>,
    #[serde(default)]
    day_of_month: Option<u8>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireRange {
    #[serde(default, rename = "type")]
    kind: Option<RecurrenceRangeType>,
    #[serde(default)]
    start_date: Option<String>,
    #[serde(default)]
    end_date: Option<String>,
    #[serde(default)]
    number_of_occurrences: Option<u32>,
}

#[derive(Deserialize)]
struct WireRecurrence {
    pattern: WirePattern,
    #[serde(default)]
    range: Option<WireRange>,
}

/// Graph sends `"0001-01-01"` for "no end date"; treat unparseable or year-1 dates as absent.
fn graph_date(s: Option<String>) -> Option<jiff::civil::Date> {
    let d: jiff::civil::Date = s?.parse().ok()?;
    (d.year() > 1).then_some(d)
}

fn flatten_recurrence(r: WireRecurrence) -> RecurrenceSummary {
    let range = r.range;
    let (range_type, start_date, end_date, number_of_occurrences) = match range {
        Some(range) => (
            range.kind,
            graph_date(range.start_date),
            graph_date(range.end_date),
            range.number_of_occurrences.filter(|n| *n > 0),
        ),
        None => (None, None, None, None),
    };
    RecurrenceSummary {
        pattern: r.pattern.kind,
        interval: r.pattern.interval,
        days_of_week: r.pattern.days_of_week,
        day_of_month: r.pattern.day_of_month.filter(|d| *d > 0),
        range_type,
        start_date,
        end_date,
        number_of_occurrences,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireOnlineMeeting {
    #[serde(default)]
    join_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireEvent {
    id: EventId,
    #[serde(default)]
    subject: Sensitive,
    start: DateTimeZone,
    end: DateTimeZone,
    #[serde(default, deserialize_with = "null_default")]
    is_all_day: bool,
    #[serde(default)]
    location: Option<Location>,
    #[serde(default, deserialize_with = "null_default")]
    locations: Vec<Location>,
    #[serde(default)]
    organizer: Option<Recipient>,
    #[serde(default, deserialize_with = "null_default")]
    attendees: Vec<Attendee>,
    #[serde(default, deserialize_with = "null_default")]
    show_as: FreeBusyStatus,
    #[serde(default, rename = "type", deserialize_with = "null_default")]
    event_type: EventType,
    #[serde(default)]
    recurrence: Option<Value>,
    #[serde(default)]
    series_master_id: Option<EventId>,
    #[serde(default, deserialize_with = "null_default")]
    is_online_meeting: bool,
    #[serde(default)]
    online_meeting_provider: Option<OnlineMeetingProvider>,
    #[serde(default)]
    online_meeting: Option<WireOnlineMeeting>,
    #[serde(default, deserialize_with = "null_default")]
    response_status: ResponseStatus,
    #[serde(default, deserialize_with = "null_default")]
    sensitivity: Sensitivity,
    #[serde(default)]
    body_preview: Sensitive,
    #[serde(default, deserialize_with = "null_default")]
    is_cancelled: bool,
    #[serde(default, deserialize_with = "null_default")]
    is_organizer: bool,
    #[serde(default, deserialize_with = "null_default")]
    is_reminder_on: bool,
    #[serde(default)]
    reminder_minutes_before_start: Option<u32>,
    #[serde(default)]
    web_link: Option<String>,
    #[serde(default)]
    body: Option<MessageBody>,
}

impl WireEvent {
    fn into_model(self, calendar_id: Option<&CalendarId>) -> (Event, Option<MessageBody>) {
        // Graph sends `onlineMeetingProvider: "unknown"` for plain meetings; keep it as None.
        let provider = self.online_meeting_provider.filter(|p| !matches!(p, OnlineMeetingProvider::Unknown(_)));
        let event = Event {
            id: self.id,
            calendar_id: calendar_id.cloned(),
            subject: self.subject,
            start: self.start,
            end: self.end,
            is_all_day: self.is_all_day,
            location: self.location.filter(|l| !l.display_name.is_empty() || l.location_uri.is_some()),
            locations: self.locations,
            organizer: self.organizer.map(|r| r.email_address),
            attendees: self.attendees,
            show_as: self.show_as,
            event_type: self.event_type,
            // A malformed recurrence drops only the recurrence, not the event.
            recurrence: self
                .recurrence
                .and_then(|r| serde_json::from_value::<WireRecurrence>(r).ok())
                .map(flatten_recurrence),
            series_master_id: self.series_master_id,
            is_online_meeting: self.is_online_meeting,
            online_meeting_provider: provider,
            online_meeting_join_url: self.online_meeting.and_then(|m| m.join_url),
            response_status: self.response_status,
            sensitivity: self.sensitivity,
            body_preview: self.body_preview,
            is_cancelled: self.is_cancelled,
            is_organizer: self.is_organizer,
            is_reminder_on: self.is_reminder_on,
            reminder_minutes_before_start: self.reminder_minutes_before_start,
            web_link: self.web_link,
        };
        (event, self.body)
    }
}

/// Maps Graph event JSON to an [`Event`]: organizer `{emailAddress}` unwrapped, recurrence
/// pattern and range flattened into [`RecurrenceSummary`] (Graph's `0001-01-01` "no date" becomes
/// `None`), `onlineMeeting.joinUrl` lifted, and `calendar_id` set from the context.
#[must_use]
pub fn event_from_json(v: Value, calendar_id: Option<&CalendarId>) -> Option<Event> {
    serde_json::from_value::<WireEvent>(v).ok().map(|w| w.into_model(calendar_id).0)
}

/// An event with its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventWithBody {
    /// The event.
    pub event: Event,
    /// The description (HTML is untrusted; sanitise before display).
    pub body: MessageBody,
}

// ---------------------------------------------------------------------------------------------
// Calendar views.
// ---------------------------------------------------------------------------------------------

/// Which calendar a view or event list reads.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CalendarRef {
    /// All calendars of the mailbox's default calendar view (`{mailbox}/calendarView`).
    #[default]
    Default,
    /// A specific calendar (`{mailbox}/calendars/{id}/calendarView`).
    Id(CalendarId),
}

fn view_base(calendar: &CalendarRef) -> Result<String> {
    match calendar {
        CalendarRef::Default => Ok(String::new()),
        CalendarRef::Id(id) => Ok(format!("/calendars/{}", segment(id.as_str())?)),
    }
}

fn check_range(start: Timestamp, end: Timestamp) -> Result<()> {
    if end <= start { Err(config("calendar range must end after it starts")) } else { Ok(()) }
}

/// Builds `GET {mailbox}[/calendars/{id}]/calendarView?startDateTime&endDateTime`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox or calendar id, or an empty range.
pub fn calendar_view_request(
    mailbox: &Mailbox,
    calendar: &CalendarRef,
    start: Timestamp,
    end: Timestamp,
    top: Option<u32>,
) -> Result<Request> {
    check_range(start, end)?;
    Ok(cal_req(mailbox, Method::Get, &format!("{}/calendarView", view_base(calendar)?))?
        .query("startDateTime", start.to_string())
        .query("endDateTime", end.to_string())
        .query("$select", EVENT_SELECT)
        .query("$top", clamp_top(top, DEFAULT_VIEW_PAGE_SIZE, MAX_VIEW_PAGE_SIZE).to_string()))
}

/// Builds the first `GET {mailbox}/calendarView/delta?startDateTime&endDateTime` request.
///
/// Graph's calendar-view delta does not support `$select` (it returns the full event) and keeps
/// the range in its links. Page size goes in `Prefer: odata.maxpagesize`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox or an empty range.
pub fn calendar_view_delta_request(
    mailbox: &Mailbox,
    start: Timestamp,
    end: Timestamp,
    page_size: Option<u32>,
) -> Result<Request> {
    check_range(start, end)?;
    let size = clamp_top(page_size, DEFAULT_VIEW_PAGE_SIZE, MAX_VIEW_PAGE_SIZE);
    Ok(with_prefer(
        mailbox.request(Method::Get, "/calendarView/delta", ResourceFamily::Calendar)?,
        &[PREFER_IMMUTABLE_ID, PREFER_UTC, &format!("odata.maxpagesize={size}")],
    )
    .query("startDateTime", start.to_string())
    .query("endDateTime", end.to_string()))
}

/// Rebuilds a request for a stored calendar-view `nextLink` / `deltaLink` with the same headers.
#[must_use]
pub fn calendar_delta_link_request(mailbox: &Mailbox, link: impl Into<String>, page_size: Option<u32>) -> Request {
    let size = clamp_top(page_size, DEFAULT_VIEW_PAGE_SIZE, MAX_VIEW_PAGE_SIZE);
    with_prefer(
        Request::link(link).family(ResourceFamily::Calendar).bucket(mailbox.bucket()),
        &[PREFER_IMMUTABLE_ID, PREFER_UTC, &format!("odata.maxpagesize={size}")],
    )
}

fn map_event_change(v: Value) -> Option<Change<Event, EventId>> {
    if is_removed(&v) { removed_id(&v).map(Change::Removed) } else { event_from_json(v, None).map(Change::Upsert) }
}

/// Builds `GET {mailbox}/events/{id}/instances?startDateTime&endDateTime` (occurrences of a series).
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid id or an empty range.
pub fn instances_request(
    mailbox: &Mailbox,
    series_master: &EventId,
    start: Timestamp,
    end: Timestamp,
) -> Result<Request> {
    check_range(start, end)?;
    Ok(cal_req(mailbox, Method::Get, &format!("{}/instances", event_path(series_master)?))?
        .query("startDateTime", start.to_string())
        .query("endDateTime", end.to_string())
        .query("$select", EVENT_SELECT)
        .query("$top", DEFAULT_VIEW_PAGE_SIZE.to_string()))
}

// ---------------------------------------------------------------------------------------------
// Event CRUD.
// ---------------------------------------------------------------------------------------------

/// Builds `GET {mailbox}/events/{id}` with the HTML body.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid id.
pub fn get_event_request(mailbox: &Mailbox, id: &EventId) -> Result<Request> {
    Ok(with_prefer(
        mailbox.request(Method::Get, &event_path(id)?, ResourceFamily::Calendar)?,
        &[PREFER_IMMUTABLE_ID, PREFER_UTC, PREFER_HTML_BODY],
    )
    .query("$select", format!("{EVENT_SELECT},body")))
}

/// Serialises a [`RecurrenceSummary`] as Graph's `patternedRecurrence`.
///
/// # Errors
/// [`ErrorKind::Config`] without a start date (Graph requires `range.startDate`).
pub fn recurrence_json(r: &RecurrenceSummary) -> Result<Value> {
    let start = r.start_date.ok_or_else(|| config("recurrence needs a start date"))?;
    let mut pattern = Map::new();
    pattern.insert("type".into(), Value::String(r.pattern.as_graph().to_owned()));
    pattern.insert("interval".into(), json!(r.interval.max(1)));
    if !r.days_of_week.is_empty() {
        let days: Vec<&str> = r.days_of_week.iter().map(GraphEnum::as_graph).collect();
        pattern.insert("daysOfWeek".into(), json!(days));
    }
    if let Some(d) = r.day_of_month {
        pattern.insert("dayOfMonth".into(), json!(d));
    }
    let mut range = Map::new();
    let kind = r.range_type.clone().unwrap_or(match (r.end_date, r.number_of_occurrences) {
        (Some(_), _) => RecurrenceRangeType::EndDate,
        (None, Some(_)) => RecurrenceRangeType::Numbered,
        (None, None) => RecurrenceRangeType::NoEnd,
    });
    range.insert("type".into(), Value::String(kind.as_graph().to_owned()));
    range.insert("startDate".into(), Value::String(start.to_string()));
    if let Some(end) = r.end_date {
        range.insert("endDate".into(), Value::String(end.to_string()));
    }
    if let Some(n) = r.number_of_occurrences {
        range.insert("numberOfOccurrences".into(), json!(n));
    }
    Ok(json!({ "pattern": Value::Object(pattern), "range": Value::Object(range) }))
}

fn dtz_json(d: &DateTimeZone) -> Value {
    json!({ "dateTime": d.date_time.to_string(), "timeZone": d.time_zone.as_str() })
}

fn attendees_json(list: &[Attendee]) -> Result<Value> {
    let mut out = Vec::with_capacity(list.len());
    for a in list {
        let mut v = recipient_json(&a.email_address)?;
        if let Some(m) = v.as_object_mut() {
            m.insert("type".into(), Value::String(a.kind.as_graph().to_owned()));
        }
        out.push(v);
    }
    Ok(Value::Array(out))
}

fn location_json(l: &Location) -> Value {
    let mut m = Map::new();
    m.insert("displayName".into(), Value::String(l.display_name.as_str().to_owned()));
    if let Some(uri) = &l.location_uri {
        m.insert("locationUri".into(), Value::String(uri.clone()));
    }
    if let Some(email) = &l.location_email_address {
        m.insert("locationEmailAddress".into(), Value::String(email.as_str().to_owned()));
    }
    Value::Object(m)
}

/// A new event, or the fields of an update (`None` / empty leaves a field unchanged on update).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventDraft {
    /// Subject.
    pub subject: Option<Sensitive>,
    /// Description.
    pub body: Option<MessageBody>,
    /// Start.
    pub start: Option<DateTimeZone>,
    /// End.
    pub end: Option<DateTimeZone>,
    /// All day.
    pub is_all_day: Option<bool>,
    /// Location.
    pub location: Option<Location>,
    /// Attendees (replaces the list on update).
    pub attendees: Option<Vec<Attendee>>,
    /// Free/busy display.
    pub show_as: Option<FreeBusyStatus>,
    /// Sensitivity.
    pub sensitivity: Option<Sensitivity>,
    /// Recurrence (series master).
    pub recurrence: Option<RecurrenceSummary>,
    /// Reminder on/off.
    pub is_reminder_on: Option<bool>,
    /// Reminder lead time.
    pub reminder_minutes_before_start: Option<u32>,
    /// Make it an online meeting with this provider (`teamsForBusiness` creates a Teams link).
    pub online_meeting: Option<OnlineMeetingProvider>,
    /// Whether attendees may propose new times.
    pub allow_new_time_proposals: Option<bool>,
    /// Client-chosen idempotency key (Graph's `transactionId`); set it on creates so the outbox
    /// may retry without duplicating the event.
    pub transaction_id: Option<String>,
}

impl EventDraft {
    /// A Teams meeting: sets `isOnlineMeeting` and `onlineMeetingProvider=teamsForBusiness`.
    #[must_use]
    pub fn with_teams_meeting(mut self) -> Self {
        self.online_meeting = Some(OnlineMeetingProvider::TeamsForBusiness);
        self
    }

    /// The Graph event JSON (only set fields).
    ///
    /// # Errors
    /// [`ErrorKind::Config`] for attendees without addresses or a recurrence without a start.
    pub fn to_json(&self) -> Result<Value> {
        let mut m = Map::new();
        if let Some(s) = &self.subject {
            m.insert("subject".into(), Value::String(s.as_str().to_owned()));
        }
        if let Some(b) = &self.body {
            m.insert("body".into(), json!({ "contentType": b.content_type.as_graph(), "content": b.content.as_str() }));
        }
        if let Some(s) = &self.start {
            m.insert("start".into(), dtz_json(s));
        }
        if let Some(e) = &self.end {
            m.insert("end".into(), dtz_json(e));
        }
        if let Some(a) = self.is_all_day {
            m.insert("isAllDay".into(), Value::Bool(a));
        }
        if let Some(l) = &self.location {
            m.insert("location".into(), location_json(l));
        }
        if let Some(a) = &self.attendees {
            m.insert("attendees".into(), attendees_json(a)?);
        }
        if let Some(s) = &self.show_as {
            m.insert("showAs".into(), Value::String(s.as_graph().to_owned()));
        }
        if let Some(s) = &self.sensitivity {
            m.insert("sensitivity".into(), Value::String(s.as_graph().to_owned()));
        }
        if let Some(r) = &self.recurrence {
            m.insert("recurrence".into(), recurrence_json(r)?);
        }
        if let Some(r) = self.is_reminder_on {
            m.insert("isReminderOn".into(), Value::Bool(r));
        }
        if let Some(r) = self.reminder_minutes_before_start {
            m.insert("reminderMinutesBeforeStart".into(), json!(r));
        }
        if let Some(p) = &self.online_meeting {
            m.insert("isOnlineMeeting".into(), Value::Bool(true));
            m.insert("onlineMeetingProvider".into(), Value::String(p.as_graph().to_owned()));
        }
        if let Some(a) = self.allow_new_time_proposals {
            m.insert("allowNewTimeProposals".into(), Value::Bool(a));
        }
        if let Some(t) = &self.transaction_id {
            m.insert("transactionId".into(), Value::String(t.clone()));
        }
        Ok(Value::Object(m))
    }
}

/// Builds `POST {mailbox}[/calendars/{id}]/events`. Retried automatically only when a
/// `transaction_id` is set (Graph de-duplicates on it).
///
/// # Errors
/// [`ErrorKind::Config`] without start/end or subject, or as [`EventDraft::to_json`].
pub fn create_event_request(mailbox: &Mailbox, calendar: &CalendarRef, draft: &EventDraft) -> Result<Request> {
    if draft.start.is_none() || draft.end.is_none() {
        return Err(config("a new event needs a start and an end"));
    }
    let idempotent = draft.transaction_id.is_some();
    Ok(cal_req(mailbox, Method::Post, &format!("{}/events", view_base(calendar)?))?
        .json_value(draft.to_json()?)
        .idempotent(idempotent))
}

/// Builds `PATCH {mailbox}/events/{id}` (absolute values, so retry-safe).
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid id or an empty update.
pub fn update_event_request(mailbox: &Mailbox, id: &EventId, draft: &EventDraft) -> Result<Request> {
    let body = draft.to_json()?;
    if body.as_object().is_none_or(Map::is_empty) {
        return Err(config("event update changes nothing"));
    }
    Ok(cal_req(mailbox, Method::Patch, &event_path(id)?)?.json_value(body).idempotent(true))
}

/// Builds `DELETE {mailbox}/events/{id}`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid id.
pub fn delete_event_request(mailbox: &Mailbox, id: &EventId) -> Result<Request> {
    cal_req(mailbox, Method::Delete, &event_path(id)?)
}

// ---------------------------------------------------------------------------------------------
// Responses and reminders.
// ---------------------------------------------------------------------------------------------

/// A meeting response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rsvp {
    /// `accept`.
    Accept,
    /// `tentativelyAccept`, optionally proposing a new time.
    Tentative {
        /// Proposed new time.
        proposed_new_time: Option<TimeSlot>,
    },
    /// `decline`, optionally proposing a new time.
    Decline {
        /// Proposed new time.
        proposed_new_time: Option<TimeSlot>,
    },
}

/// Builds `POST {mailbox}/events/{id}/accept|tentativelyAccept|decline`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid id, or a proposed time without `send_response`
/// (Graph rejects proposals that are not sent).
pub fn respond_request(
    mailbox: &Mailbox,
    id: &EventId,
    rsvp: &Rsvp,
    comment: Option<&str>,
    send_response: bool,
) -> Result<Request> {
    let (action, proposal) = match rsvp {
        Rsvp::Accept => ("accept", None),
        Rsvp::Tentative { proposed_new_time } => ("tentativelyAccept", proposed_new_time.as_ref()),
        Rsvp::Decline { proposed_new_time } => ("decline", proposed_new_time.as_ref()),
    };
    let mut body = Map::new();
    body.insert("sendResponse".into(), Value::Bool(send_response));
    if let Some(c) = comment {
        body.insert("comment".into(), Value::String(c.to_owned()));
    }
    if let Some(slot) = proposal {
        if !send_response {
            return Err(config("a proposed new time must be sent"));
        }
        body.insert("proposedNewTime".into(), json!({ "start": dtz_json(&slot.start), "end": dtz_json(&slot.end) }));
    }
    Ok(cal_req(mailbox, Method::Post, &format!("{}/{action}", event_path(id)?))?.json_value(Value::Object(body)))
}

/// Builds `POST {mailbox}/events/{id}/cancel` (organizer only; notifies attendees).
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid id.
pub fn cancel_event_request(mailbox: &Mailbox, id: &EventId, comment: Option<&str>) -> Result<Request> {
    let body = comment.map_or_else(|| json!({}), |c| json!({ "comment": c }));
    Ok(cal_req(mailbox, Method::Post, &format!("{}/cancel", event_path(id)?))?.json_value(body))
}

/// Builds `POST {mailbox}/events/{id}/forward`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid id, no recipients, or a recipient without an address.
pub fn forward_event_request(
    mailbox: &Mailbox,
    id: &EventId,
    to: &[EmailAddress],
    comment: Option<&str>,
) -> Result<Request> {
    if to.is_empty() {
        return Err(config("forward needs at least one recipient"));
    }
    let mut body = Map::new();
    body.insert("toRecipients".into(), super::mail::recipients_json(to)?);
    if let Some(c) = comment {
        body.insert("comment".into(), Value::String(c.to_owned()));
    }
    Ok(cal_req(mailbox, Method::Post, &format!("{}/forward", event_path(id)?))?.json_value(Value::Object(body)))
}

/// Builds `POST {mailbox}/events/{id}/snoozeReminder` (repeatable, so retry-safe).
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid id.
pub fn snooze_reminder_request(mailbox: &Mailbox, id: &EventId, until: Timestamp) -> Result<Request> {
    Ok(cal_req(mailbox, Method::Post, &format!("{}/snoozeReminder", event_path(id)?))?
        .json_value(json!({ "newReminderTime": dtz_json(&DateTimeZone::from_timestamp_utc(until)) }))
        .idempotent(true))
}

/// Builds `POST {mailbox}/events/{id}/dismissReminder` (retry-safe).
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid id.
pub fn dismiss_reminder_request(mailbox: &Mailbox, id: &EventId) -> Result<Request> {
    Ok(cal_req(mailbox, Method::Post, &format!("{}/dismissReminder", event_path(id)?))?.idempotent(true))
}

// ---------------------------------------------------------------------------------------------
// Free/busy.
// ---------------------------------------------------------------------------------------------

/// Validates a `getSchedule` query and returns the interval in minutes.
fn check_schedule(start: Timestamp, end: Timestamp, interval_minutes: u32) -> Result<u32> {
    check_range(start, end)?;
    let span = end.duration_since(start);
    let max = jiff::SignedDuration::try_from(MAX_SCHEDULE_WINDOW).unwrap_or(jiff::SignedDuration::MAX);
    if span > max {
        return Err(config("getSchedule window is longer than 62 days"));
    }
    if !(5..=1440).contains(&interval_minutes) {
        return Err(config("availability interval must be 5..=1440 minutes"));
    }
    Ok(interval_minutes)
}

/// Builds one `POST {mailbox}/calendar/getSchedule` for at most [`MAX_SCHEDULES_PER_REQUEST`]
/// addresses. It only reads, so it is retry-safe.
///
/// # Errors
/// [`ErrorKind::Config`] for no or too many schedules, a window over 62 days or an interval
/// outside 5..=1440 minutes.
pub fn get_schedule_request(
    mailbox: &Mailbox,
    schedules: &[MailAddress],
    start: Timestamp,
    end: Timestamp,
    interval_minutes: u32,
) -> Result<Request> {
    if schedules.is_empty() || schedules.len() > MAX_SCHEDULES_PER_REQUEST {
        return Err(config("getSchedule takes 1..=20 schedules per request"));
    }
    let interval = check_schedule(start, end, interval_minutes)?;
    let ids: Vec<&str> = schedules.iter().map(MailAddress::as_str).collect();
    let body = json!({
        "schedules": ids,
        "startTime": dtz_json(&DateTimeZone::from_timestamp_utc(start)),
        "endTime": dtz_json(&DateTimeZone::from_timestamp_utc(end)),
        "availabilityViewInterval": interval,
    });
    Ok(cal_req(mailbox, Method::Post, "/calendar/getSchedule")?.json_value(body).idempotent(true))
}

fn schedule_from_json(v: Value) -> Option<ScheduleInformation> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct WireError {
        #[serde(default)]
        response_code: Option<String>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Wire {
        #[serde(default)]
        schedule_id: Sensitive,
        #[serde(default)]
        availability_view: Option<String>,
        #[serde(default)]
        schedule_items: Option<Vec<Value>>,
        #[serde(default)]
        error: Option<WireError>,
    }
    let w: Wire = serde_json::from_value(v).ok()?;
    // Hostile items are dropped one by one rather than failing the person's whole schedule.
    let items: Vec<ScheduleItem> = w
        .schedule_items
        .unwrap_or_default()
        .into_iter()
        .filter_map(|i| serde_json::from_value::<ScheduleItem>(i).ok())
        .collect();
    // Keep Graph's machine-readable code only: the free-text message may name people.
    let error = w.error.map(|e| e.response_code.unwrap_or_else(|| "error".to_owned()));
    let mut view = w.availability_view.unwrap_or_default();
    view.retain(|c| c.is_ascii_alphanumeric());
    view.truncate(4096);
    Some(ScheduleInformation { schedule_id: w.schedule_id, availability_view: view, schedule_items: items, error })
}

/// Result of [`Client::get_schedule`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Schedules {
    /// One entry per address that came back, in input order.
    pub schedules: Vec<ScheduleInformation>,
    /// Entries that did not parse.
    pub skipped: usize,
    /// Requests sent (one per chunk of 20).
    pub requests: usize,
}

// ---------------------------------------------------------------------------------------------
// findMeetingTimes.
// ---------------------------------------------------------------------------------------------

/// A `findMeetingTimes` query.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MeetingTimesQuery {
    /// Attendees (required, optional, or resource for rooms).
    pub attendees: Vec<(MailAddress, AttendeeType)>,
    /// Windows to search in.
    pub windows: Vec<TimeSlot>,
    /// Meeting length in minutes.
    pub duration_minutes: u32,
    /// Most suggestions wanted.
    pub max_candidates: Option<u32>,
    /// Suggest slots even when the organizer is busy.
    pub is_organizer_optional: bool,
    /// Lowest share of attendees that must be free (0–100).
    pub minimum_attendee_percentage: Option<u8>,
    /// Ask Graph to suggest a room as well.
    pub suggest_location: bool,
}

impl MeetingTimesQuery {
    /// The request body.
    ///
    /// # Errors
    /// [`ErrorKind::Config`] for a zero duration, no windows or a percentage over 100.
    pub fn to_json(&self) -> Result<Value> {
        if self.duration_minutes == 0 || self.duration_minutes > 24 * 60 {
            return Err(config("meeting duration must be 1..=1440 minutes"));
        }
        if self.windows.is_empty() {
            return Err(config("findMeetingTimes needs at least one time window"));
        }
        if self.minimum_attendee_percentage.is_some_and(|p| p > 100) {
            return Err(config("minimum attendee percentage must be 0..=100"));
        }
        let attendees: Vec<Value> = self
            .attendees
            .iter()
            .map(|(a, kind)| json!({ "type": kind.as_graph(), "emailAddress": { "address": a.as_str() } }))
            .collect();
        let slots: Vec<Value> =
            self.windows.iter().map(|s| json!({ "start": dtz_json(&s.start), "end": dtz_json(&s.end) })).collect();
        let mut m = Map::new();
        m.insert("attendees".into(), Value::Array(attendees));
        m.insert("timeConstraint".into(), json!({ "activityDomain": "work", "timeSlots": slots }));
        m.insert("meetingDuration".into(), Value::String(format!("PT{}M", self.duration_minutes)));
        m.insert("isOrganizerOptional".into(), Value::Bool(self.is_organizer_optional));
        m.insert("returnSuggestionReasons".into(), Value::Bool(true));
        if let Some(n) = self.max_candidates {
            m.insert("maxCandidates".into(), json!(n.clamp(1, 100)));
        }
        if let Some(p) = self.minimum_attendee_percentage {
            m.insert("minimumAttendeePercentage".into(), json!(p));
        }
        if self.suggest_location {
            m.insert("locationConstraint".into(), json!({ "isRequired": false, "suggestLocation": true }));
        }
        Ok(Value::Object(m))
    }
}

/// The answer to [`Client::find_meeting_times`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MeetingTimes {
    /// Suggested slots, best first.
    pub suggestions: Vec<MeetingTimeSuggestion>,
    /// Why there are none (`attendeesUnavailable`, `organizerUnavailable`, ...), if empty.
    pub empty_suggestions_reason: Option<String>,
    /// Suggestions that did not parse.
    pub skipped: usize,
}

fn suggestion_from_json(v: Value) -> Option<MeetingTimeSuggestion> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct WireAttendee {
        #[serde(default, deserialize_with = "null_default")]
        email_address: EmailAddress,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct WireAvailability {
        #[serde(default)]
        attendee: Option<WireAttendee>,
        #[serde(default, deserialize_with = "null_default")]
        availability: FreeBusyStatus,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Wire {
        meeting_time_slot: TimeSlot,
        #[serde(default)]
        confidence: Option<f64>,
        #[serde(default, deserialize_with = "null_default")]
        organizer_availability: FreeBusyStatus,
        #[serde(default, deserialize_with = "null_default")]
        attendee_availability: Vec<WireAvailability>,
        #[serde(default, deserialize_with = "null_default")]
        locations: Vec<Location>,
        #[serde(default)]
        suggestion_reason: Option<String>,
    }
    let w: Wire = serde_json::from_value(v).ok()?;
    Some(MeetingTimeSuggestion {
        meeting_time_slot: w.meeting_time_slot,
        confidence: w.confidence.filter(|c| c.is_finite()).unwrap_or(0.0),
        organizer_availability: w.organizer_availability,
        attendee_availability: w
            .attendee_availability
            .into_iter()
            .map(|a| AttendeeAvailability {
                attendee: a.attendee.map(|x| x.email_address).unwrap_or_default(),
                availability: a.availability,
            })
            .collect(),
        locations: w.locations,
        suggestion_reason: w.suggestion_reason,
    })
}

/// Builds `POST /me/findMeetingTimes` (read-only, retry-safe; always the signed-in user).
///
/// # Errors
/// As [`MeetingTimesQuery::to_json`].
pub fn find_meeting_times_request(query: &MeetingTimesQuery) -> Result<Request> {
    Ok(cal_req(&Mailbox::Me, Method::Post, "/findMeetingTimes")?.json_value(query.to_json()?).idempotent(true))
}

// ---------------------------------------------------------------------------------------------
// Places (rooms).
// ---------------------------------------------------------------------------------------------

/// A room or room list from `/places`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    /// Place id.
    pub id: String,
    /// Room or room list.
    pub kind: PlaceKind,
    /// Name.
    pub display_name: Sensitive,
    /// The mailbox address (what attendees and `getSchedule` use).
    pub email_address: Option<MailAddress>,
    /// Seats (rooms).
    pub capacity: Option<u32>,
    /// Building (rooms).
    pub building: Option<Sensitive>,
    /// Floor (rooms).
    pub floor_number: Option<i32>,
    /// Wheelchair accessible (rooms).
    pub is_wheelchair_accessible: bool,
}

/// What a [`Place`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceKind {
    /// A bookable room.
    Room,
    /// A list (building) of rooms.
    RoomList,
}

fn place_from_json(v: Value, kind: PlaceKind) -> Option<Place> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Wire {
        id: String,
        #[serde(default)]
        display_name: Sensitive,
        #[serde(default)]
        email_address: Option<String>,
        #[serde(default)]
        capacity: Option<u32>,
        #[serde(default)]
        building: Option<Sensitive>,
        #[serde(default)]
        floor_number: Option<i32>,
        #[serde(default, deserialize_with = "null_default")]
        is_wheelchair_accessible: bool,
    }
    let w: Wire = serde_json::from_value(v).ok()?;
    if w.id.is_empty() || w.id.len() > quorum_model::MAX_ID_LEN {
        return None;
    }
    Some(Place {
        id: w.id,
        kind,
        display_name: w.display_name,
        email_address: w.email_address.and_then(|a| MailAddress::new(a).ok()),
        capacity: w.capacity,
        building: w.building.filter(|b| !b.is_empty()),
        floor_number: w.floor_number,
        is_wheelchair_accessible: w.is_wheelchair_accessible,
    })
}

fn places_req(path: String) -> Request {
    Request::get(path).family(ResourceFamily::Calendar).query("$top", "100")
}

/// Builds `GET /places/microsoft.graph.room`.
#[must_use]
pub fn list_rooms_request() -> Request {
    places_req("/places/microsoft.graph.room".to_owned())
}

/// Builds `GET /places/microsoft.graph.roomlist`.
#[must_use]
pub fn list_room_lists_request() -> Request {
    places_req("/places/microsoft.graph.roomlist".to_owned())
}

/// Builds `GET /places/{roomListEmail}/microsoft.graph.roomlist/rooms`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid address.
pub fn rooms_in_list_request(room_list: &MailAddress) -> Result<Request> {
    Ok(places_req(format!("/places/{}/microsoft.graph.roomlist/rooms", segment(room_list.as_str())?)))
}

impl Client {
    /// Parses any page of calendars listed from `mailbox`.
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn calendars_page(&self, req: Request, mailbox: &Mailbox, ctx: &SourceContext) -> Result<Page<Calendar>> {
        mapped_page(self, req, |v| calendar_from_json(v, mailbox, ctx)).await
    }

    /// Calendars of `mailbox`: the user's own plus calendars shared with them (for [`Mailbox::Me`]),
    /// or the calendars another user shared (`/users/{id}/calendars`).
    ///
    /// # Errors
    /// As [`Client::page`]; [`ErrorKind::Forbidden`] when the other user shares nothing.
    pub async fn list_calendars(&self, mailbox: &Mailbox, ctx: &SourceContext) -> Result<Page<Calendar>> {
        self.calendars_page(list_calendars_request(mailbox)?, mailbox, ctx).await
    }

    /// Calendar groups.
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_calendar_groups(&self, mailbox: &Mailbox) -> Result<Page<CalendarGroup>> {
        mapped_page(self, list_calendar_groups_request(mailbox)?, group_from_json).await
    }

    /// Calendars in a group.
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_group_calendars(
        &self,
        mailbox: &Mailbox,
        group_id: &str,
        ctx: &SourceContext,
    ) -> Result<Page<Calendar>> {
        self.calendars_page(group_calendars_request(mailbox, group_id)?, mailbox, ctx).await
    }

    /// Parses any page of events; `calendar_id` (when known) is set on every event.
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn events_page(&self, req: Request, calendar_id: Option<&CalendarId>) -> Result<Page<Event>> {
        mapped_page(self, req, |v| event_from_json(v, calendar_id)).await
    }

    /// The first page of a calendar view (occurrences of series expanded) for `start..end`.
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn calendar_view(
        &self,
        mailbox: &Mailbox,
        calendar: &CalendarRef,
        start: Timestamp,
        end: Timestamp,
    ) -> Result<Page<Event>> {
        let id = match calendar {
            CalendarRef::Id(id) => Some(id),
            CalendarRef::Default => None,
        };
        self.events_page(calendar_view_request(mailbox, calendar, start, end, None)?, id).await
    }

    /// One page of a calendar-view delta.
    ///
    /// # Errors
    /// As [`Client::delta_page`] ([`ErrorKind::DeltaExpired`]: restart the window).
    pub async fn calendar_view_delta_page(&self, req: Request) -> Result<DeltaPage<Change<Event, EventId>>> {
        mapped_delta(self, req, map_event_change).await
    }

    /// Occurrences and exceptions of a series in a range (first page).
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_event_instances(
        &self,
        mailbox: &Mailbox,
        series_master: &EventId,
        start: Timestamp,
        end: Timestamp,
    ) -> Result<Page<Event>> {
        self.events_page(instances_request(mailbox, series_master, start, end)?, None).await
    }

    /// One event with its body.
    ///
    /// # Errors
    /// As [`Client::send_json`]; [`ErrorKind::Malformed`] if it does not parse.
    pub async fn get_event(&self, mailbox: &Mailbox, id: &EventId) -> Result<EventWithBody> {
        let v: Value = self.send_json(get_event_request(mailbox, id)?).await?;
        let wire: WireEvent = serde_json::from_value(v)?;
        let (event, body) = wire.into_model(None);
        Ok(EventWithBody { event, body: body.unwrap_or_default() })
    }

    /// Creates an event (with a Teams link when [`EventDraft::online_meeting`] is set).
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn create_event(&self, mailbox: &Mailbox, calendar: &CalendarRef, draft: &EventDraft) -> Result<Event> {
        let v: Value = self.send_json(create_event_request(mailbox, calendar, draft)?).await?;
        let cal = match calendar {
            CalendarRef::Id(id) => Some(id),
            CalendarRef::Default => None,
        };
        event_from_json(v, cal).ok_or_else(|| Error::new(ErrorKind::Malformed, "event did not parse"))
    }

    /// Updates an event.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn update_event(&self, mailbox: &Mailbox, id: &EventId, draft: &EventDraft) -> Result<Event> {
        let v: Value = self.send_json(update_event_request(mailbox, id, draft)?).await?;
        event_from_json(v, None).ok_or_else(|| Error::new(ErrorKind::Malformed, "event did not parse"))
    }

    /// Deletes an event (an organizer should [`Client::cancel_event`] first to notify attendees).
    ///
    /// # Errors
    /// As [`Client::send_raw`].
    pub async fn delete_event(&self, mailbox: &Mailbox, id: &EventId) -> Result<()> {
        self.send_raw(delete_event_request(mailbox, id)?).await.map(|_| ())
    }

    /// Responds to a meeting invitation.
    ///
    /// # Errors
    /// As [`Client::send_raw`].
    pub async fn respond_to_event(
        &self,
        mailbox: &Mailbox,
        id: &EventId,
        rsvp: &Rsvp,
        comment: Option<&str>,
        send_response: bool,
    ) -> Result<()> {
        self.send_raw(respond_request(mailbox, id, rsvp, comment, send_response)?).await.map(|_| ())
    }

    /// Cancels a meeting.
    ///
    /// # Errors
    /// As [`Client::send_raw`].
    pub async fn cancel_event(&self, mailbox: &Mailbox, id: &EventId, comment: Option<&str>) -> Result<()> {
        self.send_raw(cancel_event_request(mailbox, id, comment)?).await.map(|_| ())
    }

    /// Forwards a meeting invitation.
    ///
    /// # Errors
    /// As [`Client::send_raw`].
    pub async fn forward_event(
        &self,
        mailbox: &Mailbox,
        id: &EventId,
        to: &[EmailAddress],
        comment: Option<&str>,
    ) -> Result<()> {
        self.send_raw(forward_event_request(mailbox, id, to, comment)?).await.map(|_| ())
    }

    /// Snoozes an event's reminder until `until`.
    ///
    /// # Errors
    /// As [`Client::send_raw`].
    pub async fn snooze_reminder(&self, mailbox: &Mailbox, id: &EventId, until: Timestamp) -> Result<()> {
        self.send_raw(snooze_reminder_request(mailbox, id, until)?).await.map(|_| ())
    }

    /// Dismisses an event's reminder.
    ///
    /// # Errors
    /// As [`Client::send_raw`].
    pub async fn dismiss_reminder(&self, mailbox: &Mailbox, id: &EventId) -> Result<()> {
        self.send_raw(dismiss_reminder_request(mailbox, id)?).await.map(|_| ())
    }

    /// Free/busy for any number of people or rooms: chunks `schedules` by
    /// [`MAX_SCHEDULES_PER_REQUEST`] and sends one `getSchedule` per chunk, in order.
    ///
    /// # Errors
    /// As [`get_schedule_request`] and [`Client::send_json`]; the first failing chunk aborts.
    pub async fn get_schedule(
        &self,
        mailbox: &Mailbox,
        schedules: &[MailAddress],
        start: Timestamp,
        end: Timestamp,
        interval_minutes: u32,
    ) -> Result<Schedules> {
        if schedules.is_empty() {
            return Ok(Schedules::default());
        }
        check_schedule(start, end, interval_minutes)?;
        let mut out = Schedules::default();
        for chunk in schedules.chunks(MAX_SCHEDULES_PER_REQUEST) {
            let req = get_schedule_request(mailbox, chunk, start, end, interval_minutes)?;
            let page = mapped_page(self, req, schedule_from_json).await?;
            out.requests = out.requests.saturating_add(1);
            out.skipped = out.skipped.saturating_add(page.skipped);
            out.schedules.extend(page.items);
        }
        Ok(out)
    }

    /// Suggests meeting times.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn find_meeting_times(&self, query: &MeetingTimesQuery) -> Result<MeetingTimes> {
        let v: Value = self.send_json(find_meeting_times_request(query)?).await?;
        let raw = v.get("meetingTimeSuggestions").and_then(Value::as_array).cloned().unwrap_or_default();
        let (suggestions, skipped) = super::mail::map_items(raw, suggestion_from_json);
        let empty_suggestions_reason = v
            .get("emptySuggestionsReason")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(128).collect());
        Ok(MeetingTimes { suggestions, empty_suggestions_reason, skipped })
    }

    /// Parses any page of places of `kind`.
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn places_page(&self, req: Request, kind: PlaceKind) -> Result<Page<Place>> {
        mapped_page(self, req, |v| place_from_json(v, kind)).await
    }

    /// All rooms in the tenant (first page).
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_rooms(&self) -> Result<Page<Place>> {
        self.places_page(list_rooms_request(), PlaceKind::Room).await
    }

    /// Room lists (first page).
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_room_lists(&self) -> Result<Page<Place>> {
        self.places_page(list_room_lists_request(), PlaceKind::RoomList).await
    }

    /// Rooms in a room list (first page).
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_rooms_in(&self, room_list: &MailAddress) -> Result<Page<Place>> {
        self.places_page(rooms_in_list_request(room_list)?, PlaceKind::Room).await
    }
}
