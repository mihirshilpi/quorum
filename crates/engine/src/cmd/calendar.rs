//! Calendar: one merged calendar over own, shared, group and opened people's calendars.
//!
//! Commands (writes go through the outbox with an optimistic local change):
//! `calendar.new_event`, `calendar.update_event`, `calendar.delete_event` (destructive),
//! `calendar.respond`, `calendar.cancel` (destructive), `calendar.forward` (destructive),
//! `calendar.snooze`, `calendar.dismiss`; layers: `calendar.open_person`,
//! `calendar.close_person`, `calendar.toggle_source`; navigation (stored as a UI preference,
//! so MCP and the keyboard drive the same view): `calendar.set_view`, `calendar.go`,
//! `calendar.view.day|work_week|week|month|agenda`; `calendar.join` (asks the UI to open the
//! Teams join link); `calendar.compose` (UI: opens the new-event form, prefilled).
//!
//! Queries: `calendar.view` (everything the calendar shows: days, merged events, free/busy
//! blocks, mini month), `calendar.sources`, `calendar.events`, `calendar.event`,
//! `calendar.upcoming`, `calendar.free_busy`, `calendar.find_times`, `calendar.rooms`,
//! `calendar.room_lists`.
//!
//! Times in params: `YYYY-MM-DD` (a date), `YYYY-MM-DDTHH:MM[:SS]` (wall-clock time in the
//! mailbox time zone) or RFC 3339 with an offset (`…Z`, `…+02:00`).

use std::time::Duration;

use jiff::civil::{Date, DateTime};
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan};
use quorum_graph::api::calendar::{MeetingTimesQuery, Place};
use quorum_ipc::CoreEvent;
use quorum_model::{
    Attendee, AttendeeType, CalendarId, CalendarSource, DateTimeZone, DayOfWeek, EmailAddress, Event, EventId,
    FreeBusyStatus, GraphEnum, Location, MailAddress, RecurrencePatternType, RecurrenceRangeType, RecurrenceSummary,
    ResponseStatus, Sensitive, Sensitivity, TimeSlot, TimeZoneName,
};
use quorum_sync::calendar::outbox::{
    self as ob, CreatePayload, EventFields, ForwardPayload, ReminderPayload, RemovePayload, RespondPayload, Response,
    Target, UpdatePayload,
};
use quorum_sync::calendar::prefs::{
    self as pf, LOCAL_ID_PREFIX, MAX_OPENED_PEOPLE, OpenedPerson, PersonMode, SourcePrefs, ViewKind, ViewState,
};
use quorum_sync::calendar::query::{self as cq, CalendarContext};
use quorum_sync::calendar::view as vw;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::command::{CommandFuture, CommandSpec, always, parse};
use crate::error::{Error, ErrorKind, Result};
use crate::session::Session;

/// Longest subject accepted, in characters.
pub const MAX_SUBJECT: usize = 1024;
/// Largest description accepted, in bytes.
pub const MAX_BODY: usize = 1024 * 1024;
/// Most attendees on one event.
pub const MAX_ATTENDEES: usize = 500;
/// Longest a query waits for Graph (free/busy, suggestions, rooms).
pub const REMOTE_TIMEOUT: Duration = Duration::from_secs(8);
/// Longest message to attendees / organizer, in characters.
pub const MAX_COMMENT: usize = 8 * 1024;

fn store_err(e: quorum_store::Error) -> Error {
    let kind = match e.kind() {
        quorum_store::ErrorKind::InvalidInput => ErrorKind::InvalidParams,
        quorum_store::ErrorKind::NotFound => ErrorKind::NotFound,
        _ => ErrorKind::Store,
    };
    Error::new(kind, "the local store failed").with_source(e)
}

fn sync_err(e: quorum_sync::Error) -> Error {
    let (kind, msg) = match e.kind() {
        quorum_sync::ErrorKind::Store => (ErrorKind::Store, "the local store failed"),
        quorum_sync::ErrorKind::Auth => (ErrorKind::Auth, "sign in again to continue"),
        quorum_sync::ErrorKind::Invalid => (ErrorKind::InvalidParams, e.message()),
        quorum_sync::ErrorKind::Bug => (ErrorKind::Bug, "internal error"),
        _ => (ErrorKind::Remote, "Microsoft Graph request failed"),
    };
    let msg = msg.to_owned();
    Error::new(kind, msg).with_source(e)
}

async fn remote<T>(f: impl std::future::Future<Output = quorum_sync::Result<T>>) -> Result<T> {
    match tokio::time::timeout(REMOTE_TIMEOUT, f).await {
        Ok(r) => r.map_err(sync_err),
        Err(_) => Err(Error::new(ErrorKind::Remote, "Microsoft Graph did not answer in time")),
    }
}

fn spec(
    id: &'static str,
    label: &'static str,
    menu: Option<&'static str>,
    shortcut: Option<&'static str>,
    params_doc: &'static str,
    destructive: bool,
    run: crate::command::RunFn,
) -> CommandSpec {
    CommandSpec { id, label, menu, shortcut, params_doc, destructive, enabled: always, run }
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        spec(
            "calendar.new_event",
            "New Event",
            Some("Calendar/New Event"),
            Some("Cmd+N"),
            r#"{"subject"?: string, "start"?: time (default: next half hour), "end"?: time (default start+30 min), "all_day"?: bool, "attendees"?: [address | {"address","name"?,"type"?: "required"|"optional"|"resource"}], "location"?: string | {"name","address"?}, "body"?: string, "body_html"?: bool, "online_meeting"?: bool (Teams), "recurrence"?: "daily"|"weekly"|"monthly"|"yearly" | {"pattern","interval"?,"days_of_week"?,"day_of_month"?,"until"?: date,"count"?}, "calendar"?: calendar id, "show_as"?: "busy"|"free"|"tentative"|"oof"|"workingElsewhere", "reminder_minutes"?: n, "private"?: bool} -> {"id" (local until synced), "pending": true}"#,
            false,
            new_event,
        ),
        spec(
            "calendar.update_event",
            "Edit Event",
            Some("Calendar/Edit Event"),
            None,
            r#"{"id": event id, ...any field of calendar.new_event except calendar} -> {"id"}"#,
            false,
            update_event,
        ),
        spec(
            "calendar.delete_event",
            "Delete Event",
            Some("Calendar/Delete Event"),
            Some("Cmd+Backspace"),
            r#"{"id": event id}"#,
            true,
            delete_event,
        ),
        spec(
            "calendar.respond",
            "Respond to Invitation",
            Some("Calendar/Respond"),
            None,
            r#"{"id": event id, "response": "accept"|"tentative"|"decline", "message"?: string, "propose"?: {"start": time, "end": time}, "send"?: bool (default true)}"#,
            false,
            respond,
        ),
        spec(
            "calendar.cancel",
            "Cancel Meeting",
            Some("Calendar/Cancel Meeting"),
            None,
            r#"{"id": event id, "message"?: string} (organizer only; notifies attendees)"#,
            true,
            cancel,
        ),
        spec(
            "calendar.forward",
            "Forward Invitation",
            Some("Calendar/Forward"),
            None,
            r#"{"id": event id, "to": [address], "message"?: string}"#,
            true,
            forward,
        ),
        spec(
            "calendar.snooze",
            "Snooze Reminder",
            Some("Calendar/Snooze Reminder"),
            None,
            r#"{"id": event id, "minutes"?: n (default 5) | "until"?: time}"#,
            false,
            snooze,
        ),
        spec(
            "calendar.dismiss",
            "Dismiss Reminder",
            Some("Calendar/Dismiss Reminder"),
            None,
            r#"{"id": event id}"#,
            false,
            dismiss,
        ),
        spec(
            "calendar.open_person",
            "Open Someone's Calendar",
            Some("Calendar/Open Calendar…"),
            None,
            r#"{"user": address, "name"?: string} -> {"source"} (their events when shared with you, else free/busy)"#,
            false,
            open_person,
        ),
        spec(
            "calendar.close_person",
            "Close Person's Calendar",
            None,
            None,
            r#"{"user": address | source id}"#,
            false,
            close_person,
        ),
        spec(
            "calendar.toggle_source",
            "Show or Hide Calendar",
            None,
            None,
            r##"{"id": source id, "visible"?: bool (default: flip), "color"?: "#rrggbb"}"##,
            false,
            toggle_source,
        ),
        spec(
            "calendar.set_view",
            "Calendar View",
            None,
            None,
            r#"{"view": "day"|"work_week"|"week"|"month"|"agenda"}"#,
            false,
            set_view,
        ),
        spec("calendar.view.day", "Day View", Some("View/Calendar/Day"), Some("Cmd+Alt+1"), "{}", false, view_day),
        spec(
            "calendar.view.work_week",
            "Work Week View",
            Some("View/Calendar/Work Week"),
            Some("Cmd+Alt+2"),
            "{}",
            false,
            view_work_week,
        ),
        spec("calendar.view.week", "Week View", Some("View/Calendar/Week"), Some("Cmd+Alt+3"), "{}", false, view_week),
        spec(
            "calendar.view.month",
            "Month View",
            Some("View/Calendar/Month"),
            Some("Cmd+Alt+4"),
            "{}",
            false,
            view_month,
        ),
        spec(
            "calendar.view.agenda",
            "Agenda View",
            Some("View/Calendar/Agenda"),
            Some("Cmd+Alt+5"),
            "{}",
            false,
            view_agenda,
        ),
        spec(
            "calendar.go",
            "Go to Date",
            Some("Calendar/Go to Date"),
            None,
            r#"{"to": "today"|"next"|"previous"|"YYYY-MM-DD"} or {"date": "YYYY-MM-DD"}"#,
            false,
            go,
        ),
        spec(
            "calendar.join",
            "Join Online Meeting",
            Some("Calendar/Join"),
            Some("Cmd+Shift+J"),
            r#"{"id": event id} -> {"url"} (the UI opens it in the browser)"#,
            false,
            join,
        ),
        spec(
            "calendar.compose",
            "New Event With…",
            None,
            None,
            r#"{"attendees"?: [address], "subject"?: string, "start"?: time, "end"?: time} (opens the new-event form in the UI; saves nothing)"#,
            false,
            compose,
        ),
    ]
}

pub(crate) fn queries() -> Vec<CommandSpec> {
    vec![
        spec("calendar.view", "Calendar View", None, None, r#"{"mini_offset"?: months} -> view model"#, false, q_view),
        spec("calendar.sources", "Calendar Layers", None, None, "{} -> [source]", false, q_sources),
        spec(
            "calendar.events",
            "Events in Range",
            None,
            None,
            r#"{"from": time, "to": time, "sources"?: [source id]} -> [event] (merged, tagged with source and colour)"#,
            false,
            q_events,
        ),
        spec("calendar.event", "Event Details", None, None, r#"{"id": event id}"#, false, q_event),
        spec(
            "calendar.upcoming",
            "Upcoming Events",
            None,
            None,
            r#"{"from"?: time (default now), "to"?: time (default from+2h)} -> [event] starting in [from, to)"#,
            false,
            q_upcoming,
        ),
        spec(
            "calendar.free_busy",
            "Free/Busy",
            None,
            None,
            r#"{"users": [address], "from": time, "to": time, "interval"?: minutes (default 30)}"#,
            false,
            q_free_busy,
        ),
        spec(
            "calendar.find_times",
            "Suggest Meeting Times",
            None,
            None,
            r#"{"attendees": [address | {"address","type"?}], "duration"?: minutes (default 30), "window"?: {"from","to"} (default the next 7 days), "max"?: n, "suggest_room"?: bool}"#,
            false,
            q_find_times,
        ),
        spec("calendar.rooms", "Rooms", None, None, r#"{"list"?: room list address}"#, false, q_rooms),
        spec("calendar.room_lists", "Room Lists", None, None, "{}", false, q_room_lists),
    ]
}

// ---------------------------------------------------------------------------------------------
// Parsing.
// ---------------------------------------------------------------------------------------------

/// A parsed time parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum When {
    /// A date only.
    Date(Date),
    /// An instant.
    At(Timestamp),
}

/// Parses a time parameter (see the module docs) in `tz`.
pub(crate) fn parse_when(s: &str, tz: &TimeZone) -> Result<When> {
    let s = s.trim();
    if s.is_empty() || s.len() > 64 {
        return Err(Error::params("time must be YYYY-MM-DD, YYYY-MM-DDTHH:MM or RFC 3339"));
    }
    if let Ok(ts) = s.parse::<Timestamp>() {
        return Ok(When::At(ts));
    }
    if s.len() > 10
        && let Ok(dt) = s.parse::<DateTime>()
    {
        let ts = tz.to_timestamp(dt).map_err(|_| Error::params("that local time does not exist"))?;
        return Ok(When::At(ts));
    }
    if let Ok(d) = s.parse::<Date>() {
        return Ok(When::Date(d));
    }
    Err(Error::params("time must be YYYY-MM-DD, YYYY-MM-DDTHH:MM or RFC 3339"))
}

fn instant(w: When, tz: &TimeZone) -> Result<Timestamp> {
    match w {
        When::At(t) => Ok(t),
        When::Date(d) => d.to_zoned(tz.clone()).map(|z| z.timestamp()).map_err(|_| Error::params("invalid date")),
    }
}

fn local_date(w: When, tz: &TimeZone) -> Date {
    match w {
        When::Date(d) => d,
        When::At(t) => t.to_zoned(tz.clone()).date(),
    }
}

fn tz_label(tz: &TimeZone) -> String {
    vw::tz_name(tz)
}

/// A floating all-day boundary (midnight in the display zone, as Graph wants it).
fn day_dtz(d: Date, tz: &TimeZone) -> DateTimeZone {
    DateTimeZone::new(d.to_datetime(jiff::civil::Time::midnight()), TimeZoneName::new(tz_label(tz)))
}

fn clean_text(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').take(max).collect()
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum AttendeeParam {
    Address(String),
    Full {
        address: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default, rename = "type")]
        kind: Option<String>,
    },
}

fn attendee(p: &AttendeeParam) -> Result<Attendee> {
    let (address, name, kind) = match p {
        AttendeeParam::Address(a) => (a.as_str(), None, None),
        AttendeeParam::Full { address, name, kind } => (address.as_str(), name.as_deref(), kind.as_deref()),
    };
    let address = MailAddress::new(address).map_err(|_| Error::params("an attendee address is not valid"))?;
    let kind = match kind.map(|k| k.to_ascii_lowercase()) {
        None => AttendeeType::Required,
        Some(k) if k == "required" => AttendeeType::Required,
        Some(k) if k == "optional" => AttendeeType::Optional,
        Some(k) if k == "resource" || k == "room" => AttendeeType::Resource,
        Some(_) => return Err(Error::params("attendee type must be required, optional or resource")),
    };
    Ok(Attendee {
        email_address: EmailAddress {
            name: Sensitive::new(clean_text(name.unwrap_or(""), 256)),
            address: Some(address),
        },
        kind,
        status: ResponseStatus::default(),
    })
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LocationParam {
    Name(String),
    Full {
        #[serde(default)]
        name: String,
        #[serde(default)]
        address: Option<String>,
    },
}

fn location(p: &LocationParam) -> Result<Location> {
    let (name, address) = match p {
        LocationParam::Name(n) => (n.as_str(), None),
        LocationParam::Full { name, address } => (name.as_str(), address.as_deref()),
    };
    let address = match address {
        Some(a) => Some(MailAddress::new(a).map_err(|_| Error::params("location address is not valid"))?),
        None => None,
    };
    Ok(Location {
        display_name: Sensitive::new(clean_text(name, 512)),
        location_uri: address.as_ref().map(|a| a.as_str().to_owned()),
        location_email_address: address.map(|a| Sensitive::new(a.as_str())),
    })
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RecurrenceParam {
    Simple(String),
    Full {
        pattern: String,
        #[serde(default)]
        interval: Option<u32>,
        #[serde(default)]
        days_of_week: Option<Vec<String>>,
        #[serde(default)]
        day_of_month: Option<u8>,
        #[serde(default)]
        until: Option<String>,
        #[serde(default)]
        count: Option<u32>,
    },
}

fn day_of_week(d: Date) -> DayOfWeek {
    use jiff::civil::Weekday as W;
    match d.weekday() {
        W::Monday => DayOfWeek::Monday,
        W::Tuesday => DayOfWeek::Tuesday,
        W::Wednesday => DayOfWeek::Wednesday,
        W::Thursday => DayOfWeek::Thursday,
        W::Friday => DayOfWeek::Friday,
        W::Saturday => DayOfWeek::Saturday,
        W::Sunday => DayOfWeek::Sunday,
    }
}

fn recurrence(p: &RecurrenceParam, start: Date) -> Result<Option<RecurrenceSummary>> {
    let (pattern, interval, days, dom, until, count) = match p {
        RecurrenceParam::Simple(s) => (s.as_str(), None, None, None, None, None),
        RecurrenceParam::Full { pattern, interval, days_of_week, day_of_month, until, count } => {
            (pattern.as_str(), *interval, days_of_week.as_ref(), *day_of_month, until.as_deref(), *count)
        }
    };
    let pattern = match pattern.trim().to_ascii_lowercase().as_str() {
        "" | "none" | "never" => return Ok(None),
        "daily" | "day" => RecurrencePatternType::Daily,
        "weekly" | "week" => RecurrencePatternType::Weekly,
        "monthly" | "month" | "absolutemonthly" => RecurrencePatternType::AbsoluteMonthly,
        "relativemonthly" => RecurrencePatternType::RelativeMonthly,
        "yearly" | "year" | "absoluteyearly" => RecurrencePatternType::AbsoluteYearly,
        _ => return Err(Error::params("recurrence must be daily, weekly, monthly or yearly")),
    };
    let interval = interval.unwrap_or(1);
    if !(1..=99).contains(&interval) {
        return Err(Error::params("recurrence interval must be 1..=99"));
    }
    let mut days_of_week = Vec::new();
    if let Some(days) = days {
        for d in days.iter().take(7) {
            match DayOfWeek::from_graph(d.trim()) {
                DayOfWeek::Unknown(_) => return Err(Error::params("days_of_week must be day names")),
                known => days_of_week.push(known),
            }
        }
    }
    if days_of_week.is_empty()
        && matches!(pattern, RecurrencePatternType::Weekly | RecurrencePatternType::RelativeMonthly)
    {
        days_of_week.push(day_of_week(start));
    }
    let day_of_month = match pattern {
        RecurrencePatternType::AbsoluteMonthly | RecurrencePatternType::AbsoluteYearly => {
            Some(dom.unwrap_or_else(|| u8::try_from(start.day()).unwrap_or(1)).clamp(1, 31))
        }
        _ => None,
    };
    let end_date = match until {
        Some(u) => Some(u.trim().parse::<Date>().map_err(|_| Error::params("recurrence until must be YYYY-MM-DD"))?),
        None => None,
    };
    if end_date.is_some_and(|e| e < start) {
        return Err(Error::params("recurrence ends before it starts"));
    }
    if count.is_some_and(|c| c == 0 || c > 999) {
        return Err(Error::params("recurrence count must be 1..=999"));
    }
    let range_type = match (end_date, count) {
        (Some(_), _) => RecurrenceRangeType::EndDate,
        (None, Some(_)) => RecurrenceRangeType::Numbered,
        (None, None) => RecurrenceRangeType::NoEnd,
    };
    Ok(Some(RecurrenceSummary {
        pattern,
        interval,
        days_of_week,
        day_of_month,
        range_type: Some(range_type),
        start_date: Some(start),
        end_date,
        number_of_occurrences: count,
    }))
}

/// Event fields as JSON params (shared by create and update).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct EventParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    start: Option<String>,
    #[serde(default)]
    end: Option<String>,
    #[serde(default)]
    all_day: Option<bool>,
    #[serde(default)]
    attendees: Option<Vec<AttendeeParam>>,
    #[serde(default)]
    location: Option<LocationParam>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    body_html: Option<bool>,
    #[serde(default)]
    online_meeting: Option<bool>,
    #[serde(default)]
    recurrence: Option<RecurrenceParam>,
    #[serde(default)]
    calendar: Option<String>,
    #[serde(default)]
    show_as: Option<String>,
    #[serde(default)]
    reminder_minutes: Option<u32>,
    #[serde(default)]
    private: Option<bool>,
}

/// Next half hour from now in `tz`.
fn next_half_hour(tz: &TimeZone) -> Timestamp {
    let now = Timestamp::now();
    let secs = now.as_second();
    let slot = 30 * 60;
    let next = secs.div_euclid(slot).saturating_add(1).saturating_mul(slot);
    let _ = tz;
    Timestamp::from_second(next).unwrap_or(now)
}

/// Builds the fields of an event from params. For creates (`existing = None`) start/end get
/// defaults; for updates the existing event fills what the params leave out.
fn fields_from(p: &EventParams, tz: &TimeZone, existing: Option<&Event>) -> Result<EventFields> {
    let mut f = EventFields::default();
    if let Some(s) = &p.subject {
        f.subject = Some(Sensitive::new(clean_text(s, MAX_SUBJECT)));
    } else if existing.is_none() {
        f.subject = Some(Sensitive::new(""));
    }
    let start = p.start.as_deref().map(|s| parse_when(s, tz)).transpose()?;
    let end = p.end.as_deref().map(|s| parse_when(s, tz)).transpose()?;
    let all_day = p
        .all_day
        .or_else(|| matches!(start, Some(When::Date(_))).then_some(true))
        .or(existing.map(|e| e.is_all_day))
        .unwrap_or(false);
    if start.is_some() || end.is_some() || p.all_day.is_some() || existing.is_none() {
        let (old_start, old_end) = match existing {
            Some(e) => (e.start.to_timestamp().ok(), e.end.to_timestamp().ok()),
            None => (None, None),
        };
        if all_day {
            let sd = match (start, existing) {
                (Some(w), _) => local_date(w, tz),
                (None, Some(e)) if e.is_all_day => e.start.date_time.date(),
                (None, _) => old_start
                    .map_or_else(|| Timestamp::now().to_zoned(tz.clone()).date(), |t| local_date(When::At(t), tz)),
            };
            let ed = match end {
                // An end date is inclusive for people ("Mon to Wed"), exclusive for Graph.
                Some(When::Date(d)) => vw::add_days(d, 1),
                Some(w @ When::At(_)) => vw::add_days(local_date(w, tz), 1),
                None => match existing {
                    Some(e) if e.is_all_day && start.is_none() => e.end.date_time.date(),
                    _ => vw::add_days(sd, 1),
                },
            };
            if ed <= sd {
                return Err(Error::params("the event must end after it starts"));
            }
            f.start = Some(day_dtz(sd, tz));
            f.end = Some(day_dtz(ed, tz));
            f.is_all_day = Some(true);
        } else {
            let s = match start {
                Some(w) => instant(w, tz)?,
                None => old_start.unwrap_or_else(|| next_half_hour(tz)),
            };
            let e = match end {
                Some(w) => instant(w, tz)?,
                None => match (old_start, old_end) {
                    (Some(os), Some(oe)) if start.is_some() => s.checked_add(os.duration_until(oe)).unwrap_or(s),
                    (_, Some(oe)) if start.is_none() => oe,
                    _ => s.checked_add(30.minutes()).unwrap_or(s),
                },
            };
            if e <= s {
                return Err(Error::params("the event must end after it starts"));
            }
            if s.duration_until(e) > jiff::SignedDuration::from_hours(24 * 366) {
                return Err(Error::params("an event can last at most a year"));
            }
            f.start = Some(DateTimeZone::from_timestamp_utc(s));
            f.end = Some(DateTimeZone::from_timestamp_utc(e));
            f.is_all_day = Some(false);
        }
    }
    if let Some(a) = &p.attendees {
        if a.len() > MAX_ATTENDEES {
            return Err(Error::params("too many attendees"));
        }
        let mut list: Vec<Attendee> = Vec::with_capacity(a.len());
        for x in a {
            let att = attendee(x)?;
            let dup = list.iter().any(|y| y.email_address.address == att.email_address.address);
            if !dup {
                list.push(att);
            }
        }
        f.attendees = Some(list);
    }
    if let Some(l) = &p.location {
        f.location = Some(location(l)?);
    }
    if let Some(b) = &p.body {
        if b.len() > MAX_BODY {
            return Err(Error::params("the description is too long"));
        }
        f.body = Some(Sensitive::new(b.clone()));
        f.body_html = p.body_html.unwrap_or(false);
    }
    f.teams = p.online_meeting;
    if let Some(r) = &p.recurrence {
        let start_date = f
            .start
            .as_ref()
            .map(|s| {
                if all_day {
                    s.date_time.date()
                } else {
                    s.to_timestamp().map(|t| t.to_zoned(tz.clone()).date()).unwrap_or(s.date_time.date())
                }
            })
            .or_else(|| existing.map(|e| e.start.date_time.date()))
            .unwrap_or_else(|| Timestamp::now().to_zoned(tz.clone()).date());
        f.recurrence = recurrence(r, start_date)?;
    }
    if let Some(s) = &p.show_as {
        match FreeBusyStatus::from_graph(s.trim()) {
            FreeBusyStatus::Unknown(_) => {
                return Err(Error::params("show_as must be free, tentative, busy, oof or workingElsewhere"));
            }
            known => f.show_as = Some(known),
        }
    }
    if let Some(m) = p.reminder_minutes {
        if m > 60 * 24 * 14 {
            return Err(Error::params("reminder must be at most two weeks before"));
        }
        f.reminder_minutes = Some(m);
    }
    if let Some(private) = p.private {
        f.sensitivity = Some(if private { Sensitivity::Private } else { Sensitivity::Normal });
    }
    Ok(f)
}

fn event_id(s: &str) -> Result<EventId> {
    EventId::new(s.trim()).map_err(|_| Error::params("invalid event id"))
}

#[derive(Debug, Deserialize)]
struct IdParam {
    id: String,
}

fn local_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    format!("{LOCAL_ID_PREFIX}cal-{}-{n}", Timestamp::now().as_nanosecond())
}

// ---------------------------------------------------------------------------------------------
// Shared helpers.
// ---------------------------------------------------------------------------------------------

async fn context(s: &Session) -> Result<(quorum_store::Store, CalendarContext)> {
    let store = s.store()?;
    let cx = cq::load(&store).await.map_err(sync_err)?;
    Ok((store, cx))
}

async fn load_event(store: &quorum_store::Store, id: &EventId) -> Result<Event> {
    let id2 = id.clone();
    store
        .read(move |r| r.event(&id2))
        .await
        .map_err(store_err)?
        .ok_or_else(|| Error::new(ErrorKind::NotFound, "no such event"))
}

/// Whose mailbox an event lives in (`None` = the signed-in user).
fn mailbox_of(cx: &CalendarContext, e: &Event) -> Option<String> {
    let cal = e.calendar_id.as_ref()?;
    let c = cx.calendars.iter().find(|c| &c.id == cal)?;
    if c.source == CalendarSource::OpenedUser {
        c.owner.as_ref()?.address.as_ref().map(|a| a.as_str().to_owned())
    } else {
        None
    }
}

fn target(cx: &CalendarContext, e: &Event) -> Target {
    Target { id: e.id.as_str().to_owned(), mailbox: mailbox_of(cx, e) }
}

/// Enqueues `payload` and applies `local` in one write, then wakes the outbox.
async fn enqueue<P, F>(
    s: &Session,
    kind: &'static str,
    id: String,
    local_id: Option<String>,
    payload: P,
    local: F,
) -> Result<()>
where
    P: serde::Serialize + Send + 'static,
    F: FnOnce(&mut quorum_store::Tx<'_>) -> quorum_store::Result<()> + Send + 'static,
{
    let store = s.store()?;
    store
        .write(move |tx| {
            local(tx)?;
            ob::enqueue(tx, kind, &id, local_id.as_deref(), &payload)
        })
        .await
        .map_err(store_err)?;
    if let Ok(sync) = s.sync() {
        sync.outbox_changed();
    }
    Ok(())
}

async fn save_view(s: &Session, state: ViewState) -> Result<Value> {
    let store = s.store()?;
    store.set_ui_pref(pf::VIEW_PREF.to_owned(), state).await.map_err(store_err)?;
    if let Ok(sync) = s.sync() {
        sync.set_focus("calendar");
    }
    Ok(json!({ "view": state.view.as_str(), "date": state.date.map(|d| d.to_string()) }))
}

// ---------------------------------------------------------------------------------------------
// Commands.
// ---------------------------------------------------------------------------------------------

fn new_event(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: EventParams = parse(p)?;
        if p.id.is_some() {
            return Err(Error::params("calendar.new_event takes no id; use calendar.update_event"));
        }
        let (_, cx) = context(s).await?;
        let fields = fields_from(&p, &cx.tz, None)?;
        let calendar = match &p.calendar {
            Some(c) => {
                let id = CalendarId::new(c.trim()).map_err(|_| Error::params("invalid calendar id"))?;
                let cal = cx
                    .calendars
                    .iter()
                    .find(|x| x.id == id)
                    .ok_or_else(|| Error::new(ErrorKind::NotFound, "no such calendar"))?;
                if !cal.can_edit {
                    return Err(Error::params("you can't add events to that calendar"));
                }
                cal.clone()
            }
            None => cx
                .calendars
                .iter()
                .find(|c| c.is_default_calendar && c.source == CalendarSource::Own)
                .or_else(|| cx.calendars.iter().find(|c| c.source == CalendarSource::Own && c.can_edit))
                .cloned()
                .ok_or_else(|| Error::new(ErrorKind::NotAvailable, "calendars have not synced yet"))?,
        };
        let mailbox = if calendar.source == CalendarSource::OpenedUser {
            calendar.owner.as_ref().and_then(|o| o.address.as_ref()).map(|a| a.as_str().to_owned())
        } else {
            None
        };
        let lid = local_id();
        let local = EventId::new(lid.clone()).map_err(|_| Error::new(ErrorKind::Bug, "local id"))?;
        let me = cx.me.as_ref().and_then(|m| EmailAddress::new("", m.clone()).ok());
        let event = ob::optimistic_event(&local, &calendar.id, &fields, me)
            .ok_or_else(|| Error::new(ErrorKind::Bug, "event without times"))?;
        let cal_id = calendar.id.clone();
        let is_default = calendar.is_default_calendar && calendar.source == CalendarSource::Own;
        let payload = CreatePayload {
            local_id: lid.clone(),
            calendar: if is_default { None } else { Some(cal_id.clone()) },
            mailbox,
            fields,
        };
        enqueue(s, ob::KIND_CREATE, lid.clone(), Some(lid.clone()), payload, move |tx| {
            tx.upsert_events(&cal_id, &[event]).map(|_| ())
        })
        .await?;
        Ok(json!({ "id": lid, "pending": true }))
    })
}

fn update_event(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: EventParams = parse(p)?;
        let id = event_id(p.id.as_deref().ok_or_else(|| Error::params("`id` is required"))?)?;
        if p.calendar.is_some() {
            return Err(Error::params("moving an event to another calendar is not supported"));
        }
        let (store, cx) = context(s).await?;
        let original = load_event(&store, &id).await?;
        let fields = fields_from(&p, &cx.tz, Some(&original))?;
        if fields == EventFields::default() {
            return Err(Error::params("nothing to change"));
        }
        let mut updated = original.clone();
        fields.apply_to(&mut updated);
        let cal = original.calendar_id.clone().ok_or_else(|| Error::new(ErrorKind::Bug, "event without calendar"))?;
        let payload = UpdatePayload { target: target(&cx, &original), fields, original: Some(original) };
        enqueue(s, ob::KIND_UPDATE, id.as_str().to_owned(), None, payload, move |tx| {
            tx.upsert_events(&cal, &[updated]).map(|_| ())
        })
        .await?;
        Ok(json!({ "id": id.as_str() }))
    })
}

fn delete_event(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: IdParam = parse(p)?;
        let id = event_id(&p.id)?;
        let (store, cx) = context(s).await?;
        let original = load_event(&store, &id).await?;
        let payload = RemovePayload { target: target(&cx, &original), comment: None, original: Some(original) };
        let id2 = id.clone();
        enqueue(s, ob::KIND_DELETE, id.as_str().to_owned(), None, payload, move |tx| tx.delete_events(&[id2])).await?;
        Ok(json!({ "id": id.as_str() }))
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RespondParams {
    id: String,
    response: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    propose: Option<SlotParam>,
    #[serde(default)]
    send: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct SlotParam {
    start: String,
    end: String,
}

fn respond(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: RespondParams = parse(p)?;
        let id = event_id(&p.id)?;
        let response = Response::parse(&p.response)
            .ok_or_else(|| Error::params("response must be accept, tentative or decline"))?;
        let (store, cx) = context(s).await?;
        let original = load_event(&store, &id).await?;
        if original.is_organizer {
            return Err(Error::params("you organize this meeting; there is nothing to respond to"));
        }
        let propose = match &p.propose {
            Some(slot) => {
                if response == Response::Accept {
                    return Err(Error::params("a new time can be proposed only with tentative or decline"));
                }
                let a = instant(parse_when(&slot.start, &cx.tz)?, &cx.tz)?;
                let b = instant(parse_when(&slot.end, &cx.tz)?, &cx.tz)?;
                if b <= a {
                    return Err(Error::params("the proposed time must end after it starts"));
                }
                Some(TimeSlot { start: DateTimeZone::from_timestamp_utc(a), end: DateTimeZone::from_timestamp_utc(b) })
            }
            None => None,
        };
        let mut updated = original.clone();
        updated.response_status = ResponseStatus { response: response.status(), time: Some(Timestamp::now()) };
        let cal = original.calendar_id.clone().ok_or_else(|| Error::new(ErrorKind::Bug, "event without calendar"))?;
        let payload = RespondPayload {
            target: target(&cx, &original),
            response,
            comment: p.message.as_deref().map(|m| Sensitive::new(clean_text(m, MAX_COMMENT))),
            propose,
            send: p.send.unwrap_or(true),
            original: Some(original.response_status.clone()),
        };
        enqueue(s, ob::KIND_RESPOND, id.as_str().to_owned(), None, payload, move |tx| {
            tx.upsert_events(&cal, &[updated]).map(|_| ())
        })
        .await?;
        Ok(json!({ "id": id.as_str(), "response": response.status().as_graph() }))
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelParams {
    id: String,
    #[serde(default)]
    message: Option<String>,
}

fn cancel(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: CancelParams = parse(p)?;
        let id = event_id(&p.id)?;
        let (store, cx) = context(s).await?;
        let original = load_event(&store, &id).await?;
        if !original.is_organizer {
            return Err(Error::params("only the organizer can cancel a meeting; decline it instead"));
        }
        let payload = RemovePayload {
            target: target(&cx, &original),
            comment: p.message.as_deref().map(|m| Sensitive::new(clean_text(m, MAX_COMMENT))),
            original: Some(original),
        };
        let id2 = id.clone();
        enqueue(s, ob::KIND_CANCEL, id.as_str().to_owned(), None, payload, move |tx| tx.delete_events(&[id2])).await?;
        Ok(json!({ "id": id.as_str() }))
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForwardParams {
    id: String,
    to: Vec<String>,
    #[serde(default)]
    message: Option<String>,
}

fn forward(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: ForwardParams = parse(p)?;
        let id = event_id(&p.id)?;
        if p.to.is_empty() || p.to.len() > MAX_ATTENDEES {
            return Err(Error::params("forward needs 1 to 500 recipients"));
        }
        let to =
            p.to.iter()
                .map(|a| EmailAddress::new("", a.trim()).map_err(|_| Error::params("a recipient address is not valid")))
                .collect::<Result<Vec<_>>>()?;
        let (store, cx) = context(s).await?;
        let original = load_event(&store, &id).await?;
        let payload = ForwardPayload {
            target: target(&cx, &original),
            to,
            comment: p.message.as_deref().map(|m| Sensitive::new(clean_text(m, MAX_COMMENT))),
        };
        enqueue(s, ob::KIND_FORWARD, id.as_str().to_owned(), None, payload, |_| Ok(())).await?;
        Ok(json!({ "id": id.as_str() }))
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnoozeParams {
    id: String,
    #[serde(default)]
    minutes: Option<u32>,
    #[serde(default)]
    until: Option<String>,
}

fn snooze(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: SnoozeParams = parse(p)?;
        let id = event_id(&p.id)?;
        let (store, cx) = context(s).await?;
        let original = load_event(&store, &id).await?;
        let until = match (&p.until, p.minutes) {
            (Some(u), _) => instant(parse_when(u, &cx.tz)?, &cx.tz)?,
            (None, m) => {
                let m = m.unwrap_or(5);
                if m == 0 || m > 60 * 24 * 14 {
                    return Err(Error::params("snooze minutes must be 1..=20160"));
                }
                Timestamp::now().checked_add(i64::from(m).minutes()).map_err(|_| Error::params("snooze too long"))?
            }
        };
        let payload = ReminderPayload { target: target(&cx, &original), until: Some(until) };
        enqueue(s, ob::KIND_SNOOZE, id.as_str().to_owned(), None, payload, |_| Ok(())).await?;
        Ok(json!({ "id": id.as_str(), "until": until.to_string() }))
    })
}

fn dismiss(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: IdParam = parse(p)?;
        let id = event_id(&p.id)?;
        let (store, cx) = context(s).await?;
        let original = load_event(&store, &id).await?;
        let cal = original.calendar_id.clone();
        let mut updated = original.clone();
        updated.is_reminder_on = false;
        let payload = ReminderPayload { target: target(&cx, &original), until: None };
        enqueue(s, ob::KIND_DISMISS, id.as_str().to_owned(), None, payload, move |tx| match cal {
            Some(cal) => tx.upsert_events(&cal, &[updated]).map(|_| ()),
            None => Ok(()),
        })
        .await?;
        Ok(json!({ "id": id.as_str() }))
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersonParams {
    user: String,
    #[serde(default)]
    name: Option<String>,
}

fn person_address(user: &str) -> Result<String> {
    let user = user.trim();
    let user = user.strip_prefix(pf::PERSON_SOURCE_PREFIX).unwrap_or(user);
    MailAddress::new(user)
        .map(|a| a.as_str().to_ascii_lowercase())
        .map_err(|_| Error::params("`user` must be an email address"))
}

fn open_person(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: PersonParams = parse(p)?;
        let address = person_address(&p.user)?;
        let store = s.store()?;
        let me: Option<String> = store.setting(pf::ME_KEY.to_owned()).await.map_err(store_err)?;
        if me.as_deref() == Some(address.as_str()) {
            return Err(Error::params("that is your own calendar"));
        }
        let name = clean_text(p.name.as_deref().unwrap_or(""), 256);
        let addr2 = address.clone();
        store
            .write(move |tx| {
                let mut people: Vec<OpenedPerson> =
                    tx.read().setting(pf::PEOPLE_KEY).ok().flatten().unwrap_or_default();
                if !people.iter().any(|x| x.address == addr2) {
                    if people.len() >= MAX_OPENED_PEOPLE {
                        return Err(quorum_store::Error::new(
                            quorum_store::ErrorKind::InvalidInput,
                            "too many opened calendars",
                        ));
                    }
                    people.push(OpenedPerson { address: addr2, name, mode: PersonMode::Pending });
                }
                tx.set_setting(pf::PEOPLE_KEY, &people)
            })
            .await
            .map_err(store_err)?;
        if let Ok(sync) = s.sync() {
            sync.poke("calendar.people");
            sync.poke("calendar.others");
        }
        Ok(json!({ "source": format!("{}{address}", pf::PERSON_SOURCE_PREFIX) }))
    })
}

fn close_person(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: PersonParams = parse(p)?;
        let address = person_address(&p.user)?;
        let store = s.store()?;
        store
            .write(move |tx| {
                let r = tx.read();
                let mut people: Vec<OpenedPerson> = r.setting(pf::PEOPLE_KEY).ok().flatten().unwrap_or_default();
                let cals: Vec<CalendarId> = r
                    .calendars()?
                    .items
                    .into_iter()
                    .filter(|c| c.source == CalendarSource::OpenedUser)
                    .filter(|c| {
                        c.owner
                            .as_ref()
                            .and_then(|o| o.address.as_ref())
                            .is_some_and(|a| a.as_str().eq_ignore_ascii_case(&address))
                    })
                    .map(|c| c.id)
                    .collect();
                people.retain(|x| x.address != address);
                tx.delete_calendars(&cals)?;
                tx.set_setting(pf::PEOPLE_KEY, &people)
            })
            .await
            .map_err(store_err)?;
        Ok(Value::Null)
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToggleParams {
    id: String,
    #[serde(default)]
    visible: Option<bool>,
    #[serde(default)]
    color: Option<String>,
}

fn toggle_source(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: ToggleParams = parse(p)?;
        let id = p.id.trim().to_owned();
        if id.is_empty() || id.len() > quorum_model::MAX_ID_LEN || id.chars().any(char::is_control) {
            return Err(Error::params("invalid source id"));
        }
        let color = match &p.color {
            Some(c) => Some(pf::valid_hex(c).ok_or_else(|| Error::params("color must be #rrggbb"))?),
            None => None,
        };
        let (store, cx) = context(s).await?;
        if !cx.sources().iter().any(|x| x.id == id) {
            return Err(Error::new(ErrorKind::NotFound, "no such calendar layer"));
        }
        let visible = p.visible;
        let shown = store
            .write(move |tx| {
                let mut prefs: SourcePrefs = tx.read().setting(pf::SOURCES_KEY).ok().flatten().unwrap_or_default();
                let show = visible.unwrap_or_else(|| prefs.hidden.contains(&id));
                if show {
                    prefs.hidden.remove(&id);
                } else {
                    prefs.hidden.insert(id.clone());
                }
                if let Some(c) = color {
                    prefs.colors.insert(id, c);
                }
                tx.set_setting(pf::SOURCES_KEY, &prefs)?;
                Ok(show)
            })
            .await
            .map_err(store_err)?;
        Ok(json!({ "visible": shown }))
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewParams {
    view: String,
}

fn set_view(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: ViewParams = parse(p)?;
        let view = ViewKind::parse(&p.view)
            .ok_or_else(|| Error::params("view must be day, work_week, week, month or agenda"))?;
        set_view_kind(s, view).await
    })
}

async fn set_view_kind(s: &Session, view: ViewKind) -> Result<Value> {
    let (_, cx) = context(s).await?;
    save_view(s, ViewState { view, date: cx.view.date }).await
}

fn view_day(s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(set_view_kind(s, ViewKind::Day))
}
fn view_work_week(s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(set_view_kind(s, ViewKind::WorkWeek))
}
fn view_week(s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(set_view_kind(s, ViewKind::Week))
}
fn view_month(s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(set_view_kind(s, ViewKind::Month))
}
fn view_agenda(s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(set_view_kind(s, ViewKind::Agenda))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoParams {
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    view: Option<String>,
}

/// The new anchor for `calendar.go` (pure).
pub(crate) fn go_target(state: &ViewState, today: Date, to: &str) -> Result<Option<Date>> {
    let anchor = state.date.unwrap_or(today);
    match to.trim().to_ascii_lowercase().as_str() {
        "today" | "now" => Ok(None),
        "next" | "forward" => Ok(Some(vw::step(state.view, anchor, 1))),
        "previous" | "prev" | "back" => Ok(Some(vw::step(state.view, anchor, -1))),
        other => {
            let d: Date =
                other.parse().map_err(|_| Error::params("`to` must be today, next, previous or YYYY-MM-DD"))?;
            Ok(if d == today { None } else { Some(d) })
        }
    }
}

fn go(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: GoParams = parse(p)?;
        let (_, cx) = context(s).await?;
        let to = p.to.or(p.date).unwrap_or_else(|| "today".to_owned());
        let mut state = cx.view;
        if let Some(v) = &p.view {
            state.view = ViewKind::parse(v).ok_or_else(|| Error::params("invalid view"))?;
        }
        state.date = go_target(&state, cx.today(), &to)?;
        save_view(s, state).await
    })
}

fn join(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: IdParam = parse(p)?;
        let id = event_id(&p.id)?;
        let store = s.store()?;
        let e = load_event(&store, &id).await?;
        let url = vw::safe_join_url(e.online_meeting_join_url.as_deref())
            .ok_or_else(|| Error::new(ErrorKind::NotAvailable, "this event has no online meeting to join"))?
            .to_owned();
        s.emit(CoreEvent::Ui { command: "calendar.join".into(), params: json!({ "url": url, "id": id.as_str() }) });
        Ok(json!({ "url": url }))
    })
}

fn compose(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let params = if p.is_null() { json!({}) } else { p };
        if !params.is_object() || params.to_string().len() > 64 * 1024 {
            return Err(Error::params("parameters must be a small object"));
        }
        s.emit(CoreEvent::Ui { command: "calendar.compose".into(), params });
        Ok(Value::Null)
    })
}

// ---------------------------------------------------------------------------------------------
// Queries.
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct ViewQuery {
    #[serde(default)]
    mini_offset: Option<i32>,
}

fn q_view(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: ViewQuery = parse(p)?;
        let (store, cx) = context(s).await?;
        let now = Timestamp::now().to_zoned(cx.tz.clone());
        let today = now.date();
        let anchor = cx.view.date.unwrap_or(today);
        let (first, n) = vw::view_days(cx.view.view, anchor);
        let sources = cx.sources();
        let visible = cq::pick_sources(&sources, None);
        let events = cq::merged_events(&store, &cx, &visible, first, n).await.map_err(sync_err)?;
        let (busy, busy_error) = match s.sync() {
            Ok(sync) => {
                let ctx = sync.ctx().clone();
                match tokio::time::timeout(REMOTE_TIMEOUT, cq::busy_blocks(&ctx, &cx, &visible, first, n)).await {
                    Ok(r) => r,
                    Err(_) => (Vec::new(), Some("free/busy timed out".to_owned())),
                }
            }
            Err(_) => (Vec::new(), None),
        };
        let now_min = i32::from(now.hour()) * 60 + i32::from(now.minute());
        Ok(json!({
            "view": cx.view.view.as_str(),
            "view_label": cx.view.view.label(),
            "date": anchor.to_string(),
            "today": today.to_string(),
            "now_min": now_min,
            "time_zone": tz_label(&cx.tz),
            "title": vw::title(cx.view.view, anchor),
            "days": vw::days(cx.view.view, anchor, today),
            "work_start": cx.work.start_min,
            "work_end": cx.work.end_min,
            "work_days": cx.work.days,
            "events": events,
            "busy": busy,
            "busy_error": busy_error,
            "mini": vw::mini_month(&cx.view, today, p.mini_offset.unwrap_or(0)),
            "sources": sources,
        }))
    })
}

fn q_sources(s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let (_, cx) = context(s).await?;
        Ok(serde_json::to_value(cx.sources())?)
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RangeQuery {
    from: String,
    to: String,
    #[serde(default)]
    sources: Option<Vec<String>>,
}

fn q_events(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: RangeQuery = parse(p)?;
        let (store, cx) = context(s).await?;
        let first = local_date(parse_when(&p.from, &cx.tz)?, &cx.tz);
        let last = match parse_when(&p.to, &cx.tz)? {
            When::Date(d) => d,
            w @ When::At(_) => {
                let t = instant(w, &cx.tz)?;
                // An instant at midnight ends the previous day.
                let z = t.to_zoned(cx.tz.clone());
                if z.hour() == 0 && z.minute() == 0 { vw::add_days(z.date(), -1) } else { z.date() }
            }
        };
        let n = vw::days_between(first, last).saturating_add(1);
        if !(1..=vw::MAX_RANGE_DAYS).contains(&n) {
            return Err(Error::params("range must be 1 to 400 days"));
        }
        if p.sources.as_ref().is_some_and(|s| s.len() > quorum_store::MAX_RANGE_CALENDARS) {
            return Err(Error::params("too many sources"));
        }
        let all = cx.sources();
        let picked = cq::pick_sources(&all, p.sources.as_deref());
        let n = i32::try_from(n).unwrap_or(1);
        let events = cq::merged_events(&store, &cx, &picked, first, n).await.map_err(sync_err)?;
        Ok(serde_json::to_value(events)?)
    })
}

fn q_upcoming(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        #[derive(Debug, Default, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct P {
            #[serde(default)]
            from: Option<String>,
            #[serde(default)]
            to: Option<String>,
        }
        let p: P = parse(p)?;
        let (store, cx) = context(s).await?;
        let from = match &p.from {
            Some(f) => instant(parse_when(f, &cx.tz)?, &cx.tz)?,
            None => Timestamp::now(),
        };
        let to = match &p.to {
            Some(t) => instant(parse_when(t, &cx.tz)?, &cx.tz)?,
            None => from.checked_add(2.hours()).unwrap_or(from),
        };
        if to <= from || from.duration_until(to) > jiff::SignedDuration::from_hours(24 * 31) {
            return Err(Error::params("range must be up to 31 days"));
        }
        let sources = cx.sources();
        let visible = cq::pick_sources(&sources, None);
        let ids: Vec<CalendarId> =
            visible.iter().filter(|s| !s.free_busy_only).filter_map(|s| CalendarId::new(s.id.clone()).ok()).collect();
        let events = store.events_in_range(ids, from, to).await.map_err(store_err)?.items;
        let mut rows: Vec<(Timestamp, Value)> = events
            .iter()
            .filter_map(|e| {
                let start = if e.is_all_day {
                    e.start.date_time.date().to_zoned(cx.tz.clone()).ok()?.timestamp()
                } else {
                    e.start.to_timestamp().ok()?
                };
                if start < from || start >= to {
                    return None;
                }
                let src =
                    visible.iter().find(|s| Some(s.id.as_str()) == e.calendar_id.as_ref().map(CalendarId::as_str))?;
                let reminder_at = (e.is_reminder_on)
                    .then(|| start.checked_sub(i64::from(e.reminder_minutes_before_start.unwrap_or(15)).minutes()).ok())
                    .flatten();
                Some((
                    start,
                    json!({
                        "id": e.id.as_str(),
                        "subject": if e.subject.is_empty() { "(No subject)" } else { e.subject() },
                        "start": start.to_string(),
                        "end": e.end.to_timestamp().map(|t| t.to_string()).unwrap_or_default(),
                        "location": e.location.as_ref().map(|l| l.display_name.as_str()).unwrap_or(""),
                        "all_day": e.is_all_day,
                        "response": e.response_status.response.as_graph(),
                        "is_cancelled": e.is_cancelled,
                        "is_reminder_on": e.is_reminder_on,
                        "reminder_minutes": e.reminder_minutes_before_start,
                        "reminder_at": reminder_at.map(|t| t.to_string()),
                        "can_join": vw::safe_join_url(e.online_meeting_join_url.as_deref()).is_some(),
                        "source": src.id,
                    }),
                ))
            })
            .collect();
        rows.sort_by_key(|r| r.0);
        Ok(Value::Array(rows.into_iter().map(|(_, v)| v).collect()))
    })
}

fn response_label(r: &quorum_model::ResponseType) -> &'static str {
    use quorum_model::ResponseType as R;
    match r {
        R::Accepted => "Accepted",
        R::TentativelyAccepted => "Tentative",
        R::Declined => "Declined",
        R::Organizer => "Organizer",
        R::NotResponded | R::None => "No response",
        _ => "Unknown",
    }
}

fn q_event(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: IdParam = parse(p)?;
        let id = event_id(&p.id)?;
        let (store, cx) = context(s).await?;
        let e = load_event(&store, &id).await?;
        let master = match (&e.series_master_id, vw::needs_master(&e)) {
            (Some(m), true) => {
                let m = m.clone();
                store.read(move |r| r.event(&m)).await.map_err(store_err)?
            }
            _ => None,
        };
        let recurrence =
            e.recurrence.as_ref().or(master.as_ref().and_then(|m| m.recurrence.as_ref())).map(vw::recurrence_text);
        let sources = cx.sources();
        let src = sources.iter().find(|s| Some(s.id.as_str()) == e.calendar_id.as_ref().map(CalendarId::as_str));
        let when = match vw::local_span(&e, &cx.tz) {
            Some((sd, _, ed, _, _)) if e.is_all_day => {
                if sd == ed {
                    format!("{} · All day", vw::short_date(sd))
                } else {
                    format!("{} – {} · All day", vw::short_date(sd), vw::short_date(ed))
                }
            }
            Some((sd, sm, ed, em, _)) if sd == ed => {
                format!("{} · {} – {}", vw::short_date(sd), vw::hhmm(sm), vw::hhmm(em))
            }
            Some((sd, sm, ed, em, _)) => {
                format!("{} {} – {} {}", vw::short_date(sd), vw::hhmm(sm), vw::short_date(ed), vw::hhmm(em))
            }
            None => String::new(),
        };
        let addr = |a: &EmailAddress| a.address.as_ref().map(|x| x.as_str().to_owned()).unwrap_or_default();
        let name = |a: &EmailAddress| if a.name.is_empty() { addr(a) } else { a.name.as_str().to_owned() };
        let attendees: Vec<Value> = e
            .attendees
            .iter()
            .take(MAX_ATTENDEES)
            .map(|a| {
                json!({
                    "name": name(&a.email_address),
                    "address": addr(&a.email_address),
                    "type": a.kind.as_graph(),
                    "response": a.status.response.as_graph(),
                    "response_label": response_label(&a.status.response),
                })
            })
            .collect();
        let can_edit = src.is_some_and(|s| s.can_edit) && (e.is_organizer || e.attendees.is_empty());
        Ok(json!({
            "id": e.id.as_str(),
            "subject": if e.subject.is_empty() { "(No subject)" } else { e.subject() },
            "when": when,
            "start": e.start.to_timestamp().map(|t| t.to_string()).unwrap_or_default(),
            "end": e.end.to_timestamp().map(|t| t.to_string()).unwrap_or_default(),
            "start_local": vw::local_span(&e, &cx.tz).map(|(d, m, _, _, _)| format!("{d}T{}", vw::hhmm(m))),
            "end_local": vw::local_span(&e, &cx.tz).map(|(_, _, d, m, _)| if m >= 1440 { format!("{}T00:00", vw::add_days(d, 1)) } else { format!("{d}T{}", vw::hhmm(m)) }),
            "all_day": e.is_all_day,
            "time_zone": tz_label(&cx.tz),
            "location": e.location.as_ref().map(|l| l.display_name.as_str()).unwrap_or(""),
            "organizer": e.organizer.as_ref().map(|o| json!({ "name": name(o), "address": addr(o) })),
            "attendees": attendees,
            "response": e.response_status.response.as_graph(),
            "response_label": response_label(&e.response_status.response),
            "is_organizer": e.is_organizer,
            "can_respond": !e.is_organizer && !e.attendees.is_empty() && !e.is_cancelled,
            "can_edit": can_edit,
            "can_cancel": e.is_organizer && !e.attendees.is_empty(),
            "is_online": e.is_online_meeting,
            "can_join": vw::safe_join_url(e.online_meeting_join_url.as_deref()).is_some(),
            "recurrence": recurrence,
            "is_recurring": e.is_recurring(),
            "preview": e.body_preview.as_str(),
            "show_as": e.show_as.as_graph(),
            "sensitivity": e.sensitivity.as_graph(),
            "is_cancelled": e.is_cancelled,
            "reminder_minutes": if e.is_reminder_on { e.reminder_minutes_before_start } else { None },
            "pending": e.id.as_str().starts_with(LOCAL_ID_PREFIX),
            "source": src.map(|s| json!({ "id": s.id, "name": s.name, "color": s.color })),
        }))
    })
}

fn addresses(list: &[String]) -> Result<Vec<MailAddress>> {
    let mut out: Vec<MailAddress> = Vec::with_capacity(list.len());
    for a in list {
        let a = MailAddress::new(a.trim()).map_err(|_| Error::params("an address is not valid"))?;
        if !out.iter().any(|x| x.eq_ignore_case(&a)) {
            out.push(a);
        }
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FreeBusyQuery {
    users: Vec<String>,
    from: String,
    to: String,
    #[serde(default)]
    interval: Option<u32>,
}

fn q_free_busy(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: FreeBusyQuery = parse(p)?;
        if p.users.len() > quorum_sync::calendar::lookup::MAX_FREE_BUSY_USERS {
            return Err(Error::params("at most 100 people"));
        }
        let users = addresses(&p.users)?;
        let (_, cx) = context(s).await?;
        let from = instant(parse_when(&p.from, &cx.tz)?, &cx.tz)?;
        let to = instant(parse_when(&p.to, &cx.tz)?, &cx.tz)?;
        let interval = p.interval.unwrap_or(30);
        let ctx = s.sync()?.ctx().clone();
        let found = remote(quorum_sync::calendar::lookup::free_busy(&ctx, &users, from, to, interval)).await?;
        let rows: Vec<Value> = users
            .iter()
            .map(|u| {
                let sched = found.iter().find(|x| x.schedule_id.as_str().eq_ignore_ascii_case(u.as_str()));
                let availability: Vec<&str> =
                    sched.map(|x| x.availability().iter().map(|a| status_name(a)).collect()).unwrap_or_default();
                let items: Vec<Value> = sched
                    .map(|x| {
                        x.schedule_items
                            .iter()
                            .take(2000)
                            .map(|i| {
                                json!({
                                    "status": i.status.as_graph(),
                                    "start": i.start.to_timestamp().map(|t| t.to_string()).unwrap_or_default(),
                                    "end": i.end.to_timestamp().map(|t| t.to_string()).unwrap_or_default(),
                                    "subject": i.subject.as_ref().map(Sensitive::as_str),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                json!({
                    "user": u.as_str(),
                    "availability": availability,
                    "items": items,
                    "error": sched.map_or(Some("no answer"), |x| x.error.as_deref()),
                })
            })
            .collect();
        Ok(json!({ "from": from.to_string(), "to": to.to_string(), "interval": interval, "schedules": rows }))
    })
}

fn status_name(s: &FreeBusyStatus) -> &'static str {
    match s {
        FreeBusyStatus::Free => "free",
        FreeBusyStatus::Tentative => "tentative",
        FreeBusyStatus::Busy => "busy",
        FreeBusyStatus::Oof => "oof",
        FreeBusyStatus::WorkingElsewhere => "workingElsewhere",
        _ => "unknown",
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FindTimesQuery {
    attendees: Vec<AttendeeParam>,
    #[serde(default)]
    duration: Option<u32>,
    #[serde(default)]
    window: Option<SlotRange>,
    #[serde(default)]
    max: Option<u32>,
    #[serde(default)]
    suggest_room: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct SlotRange {
    from: String,
    to: String,
}

fn q_find_times(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: FindTimesQuery = parse(p)?;
        if p.attendees.len() > MAX_ATTENDEES {
            return Err(Error::params("too many attendees"));
        }
        let (_, cx) = context(s).await?;
        let attendees = p
            .attendees
            .iter()
            .map(|a| {
                attendee(a).and_then(|a| {
                    a.email_address.address.map(|ad| (ad, a.kind)).ok_or_else(|| Error::params("address"))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let (from, to) = match &p.window {
            Some(w) => (instant(parse_when(&w.from, &cx.tz)?, &cx.tz)?, instant(parse_when(&w.to, &cx.tz)?, &cx.tz)?),
            None => {
                let now = Timestamp::now();
                (
                    now,
                    now.checked_add(7.days().to_duration(&now.to_zoned(TimeZone::UTC)).unwrap_or_default())
                        .unwrap_or(now),
                )
            }
        };
        if to <= from || from.duration_until(to) > jiff::SignedDuration::from_hours(24 * 62) {
            return Err(Error::params("window must be up to 62 days"));
        }
        let duration = p.duration.unwrap_or(30);
        let query = MeetingTimesQuery {
            attendees,
            windows: vec![TimeSlot {
                start: DateTimeZone::from_timestamp_utc(from),
                end: DateTimeZone::from_timestamp_utc(to),
            }],
            duration_minutes: duration,
            max_candidates: Some(p.max.unwrap_or(10).clamp(1, 50)),
            is_organizer_optional: false,
            minimum_attendee_percentage: None,
            suggest_location: p.suggest_room.unwrap_or(false),
        };
        // Validate before going remote so bad params are InvalidParams, not Remote.
        query.to_json().map_err(|_| Error::params("duration must be 1..=1440 minutes"))?;
        let ctx = s.sync()?.ctx().clone();
        let found = remote(quorum_sync::calendar::lookup::find_times(&ctx, &query)).await?;
        let suggestions: Vec<Value> = found
            .suggestions
            .iter()
            .filter_map(|sug| {
                let (sd, sm, ed, em, _) = vw::local_span_of(&sug.meeting_time_slot.start, &sug.meeting_time_slot.end, false, &cx.tz)?;
                Some(json!({
                    "start": sug.meeting_time_slot.start.to_timestamp().ok()?.to_string(),
                    "end": sug.meeting_time_slot.end.to_timestamp().ok()?.to_string(),
                    "date": sd.to_string(),
                    "start_min": sm,
                    "end_min": em,
                    "label": if sd == ed { format!("{} · {} – {}", vw::short_date(sd), vw::hhmm(sm), vw::hhmm(em)) } else { format!("{} {} – {}", vw::short_date(sd), vw::hhmm(sm), vw::hhmm(em)) },
                    "confidence": sug.confidence_percent(),
                    "organizer": status_name(&sug.organizer_availability),
                    "attendees": sug.attendee_availability.iter().map(|a| json!({
                        "address": a.attendee.address.as_ref().map(|x| x.as_str()).unwrap_or(""),
                        "availability": status_name(&a.availability),
                    })).collect::<Vec<_>>(),
                    "locations": sug.locations.iter().map(|l| l.display_name.as_str()).collect::<Vec<_>>(),
                    "reason": sug.suggestion_reason.as_deref().map(|r| r.chars().take(256).collect::<String>()),
                }))
            })
            .collect();
        Ok(json!({ "suggestions": suggestions, "empty_reason": found.empty_suggestions_reason }))
    })
}

fn place_json(p: &Place) -> Value {
    json!({
        "id": p.id,
        "name": p.display_name.as_str(),
        "address": p.email_address.as_ref().map(MailAddress::as_str),
        "capacity": p.capacity,
        "building": p.building.as_ref().map(Sensitive::as_str),
        "floor": p.floor_number,
        "accessible": p.is_wheelchair_accessible,
    })
}

fn q_rooms(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        #[derive(Debug, Default, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct P {
            #[serde(default)]
            list: Option<String>,
        }
        let p: P = parse(p)?;
        let list = match &p.list {
            Some(l) => {
                Some(MailAddress::new(l.trim()).map_err(|_| Error::params("`list` must be a room list address"))?)
            }
            None => None,
        };
        let ctx = s.sync()?.ctx().clone();
        let rooms = remote(quorum_sync::calendar::lookup::rooms(&ctx, list.as_ref())).await?;
        Ok(Value::Array(rooms.iter().map(place_json).collect()))
    })
}

fn q_room_lists(s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let ctx = s.sync()?.ctx().clone();
        let lists = remote(quorum_sync::calendar::lookup::room_lists(&ctx)).await?;
        Ok(Value::Array(lists.iter().map(place_json).collect()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_times() {
        let tz = TimeZone::get("Europe/Berlin").unwrap();
        assert_eq!(parse_when("2026-10-20", &tz).unwrap(), When::Date(jiff::civil::date(2026, 10, 20)));
        let w = parse_when("2026-10-20T09:00", &tz).unwrap();
        assert_eq!(w, When::At("2026-10-20T07:00:00Z".parse().unwrap()));
        assert_eq!(parse_when("2026-10-20T09:00:00Z", &tz).unwrap(), When::At("2026-10-20T09:00:00Z".parse().unwrap()));
        for bad in ["", "tomorrow", "2026-13-01", "9:00", &"9".repeat(100)] {
            assert!(parse_when(bad, &tz).is_err(), "{bad}");
        }
    }

    #[test]
    fn go_moves_by_view() {
        let today = jiff::civil::date(2026, 10, 17);
        let week = ViewState { view: ViewKind::Week, date: None };
        assert_eq!(go_target(&week, today, "next").unwrap(), Some(jiff::civil::date(2026, 10, 24)));
        assert_eq!(go_target(&week, today, "today").unwrap(), None);
        assert_eq!(go_target(&week, today, "2026-10-17").unwrap(), None);
        assert!(go_target(&week, today, "someday").is_err());
        let month = ViewState { view: ViewKind::Month, date: Some(jiff::civil::date(2026, 1, 31)) };
        assert_eq!(go_target(&month, today, "previous").unwrap(), Some(jiff::civil::date(2025, 12, 1)));
    }

    #[test]
    fn fields_defaults_and_validation() {
        let tz = TimeZone::UTC;
        let p = EventParams { start: Some("2026-10-20T09:00Z".into()), ..Default::default() };
        let f = fields_from(&p, &tz, None).unwrap();
        assert_eq!(f.end.unwrap().date_time.to_string(), "2026-10-20T09:30:00");
        let p = EventParams { start: Some("2026-10-20".into()), end: Some("2026-10-21".into()), ..Default::default() };
        let f = fields_from(&p, &tz, None).unwrap();
        assert_eq!(f.is_all_day, Some(true));
        assert_eq!(f.end.unwrap().date_time.date(), jiff::civil::date(2026, 10, 22));
        let p = EventParams {
            start: Some("2026-10-20T10:00Z".into()),
            end: Some("2026-10-20T09:00Z".into()),
            ..Default::default()
        };
        assert!(fields_from(&p, &tz, None).is_err());
        let p = EventParams {
            start: Some("2026-10-20T10:00Z".into()),
            recurrence: Some(RecurrenceParam::Simple("weekly".into())),
            ..Default::default()
        };
        let r = fields_from(&p, &tz, None).unwrap().recurrence.unwrap();
        assert_eq!(r.days_of_week, vec![DayOfWeek::Tuesday]);
    }
}
