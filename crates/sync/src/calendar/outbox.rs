//! Calendar writes: payloads queued by the engine (with their optimistic local change) and the
//! outbox handlers that replay them against Graph.

use futures::future::BoxFuture;
use jiff::Timestamp;
use quorum_graph::api::calendar::{CalendarRef, EventDraft, Rsvp};
use quorum_graph::api::mail::Mailbox;
use quorum_graph::{ErrorKind as G, Method, Request, ResourceFamily};
use quorum_model::{
    Attendee, BodyType, CalendarId, DateTimeZone, EmailAddress, Event, EventId, EventType, FreeBusyStatus, Location,
    MessageBody, OnlineMeetingProvider, RecurrenceSummary, ResponseStatus, ResponseType, Sensitive, Sensitivity,
    TimeSlot,
};
use quorum_store::{NewOutboxEntry, Tx};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::prefs::{LOCAL_ID_PREFIX, local_map_key};
use super::resources::GROUP_CAL_PREFIX;
use crate::{Ctx, Error, ErrorKind, OutboxHandler, Result};

/// Outbox kind: create an event.
pub const KIND_CREATE: &str = "calendar.event.create";
/// Outbox kind: update an event.
pub const KIND_UPDATE: &str = "calendar.event.update";
/// Outbox kind: delete an event.
pub const KIND_DELETE: &str = "calendar.event.delete";
/// Outbox kind: accept / tentative / decline.
pub const KIND_RESPOND: &str = "calendar.event.respond";
/// Outbox kind: cancel a meeting (organizer).
pub const KIND_CANCEL: &str = "calendar.event.cancel";
/// Outbox kind: forward an invitation.
pub const KIND_FORWARD: &str = "calendar.event.forward";
/// Outbox kind: snooze a reminder.
pub const KIND_SNOOZE: &str = "calendar.reminder.snooze";
/// Outbox kind: dismiss a reminder.
pub const KIND_DISMISS: &str = "calendar.reminder.dismiss";

/// The outbox ordering key of an event.
#[must_use]
pub fn resource_key(id: &str) -> String {
    format!("calendar.event:{id}")
}

/// The fields of a new event, or the changed fields of an update (`None` = unchanged).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventFields {
    /// Subject.
    #[serde(default)]
    pub subject: Option<Sensitive>,
    /// Start.
    #[serde(default)]
    pub start: Option<DateTimeZone>,
    /// End.
    #[serde(default)]
    pub end: Option<DateTimeZone>,
    /// All day.
    #[serde(default)]
    pub is_all_day: Option<bool>,
    /// Attendees (replaces the list).
    #[serde(default)]
    pub attendees: Option<Vec<Attendee>>,
    /// Location.
    #[serde(default)]
    pub location: Option<Location>,
    /// Description.
    #[serde(default)]
    pub body: Option<Sensitive>,
    /// The description is HTML (else text).
    #[serde(default)]
    pub body_html: bool,
    /// Make it a Teams meeting.
    #[serde(default)]
    pub teams: Option<bool>,
    /// Recurrence.
    #[serde(default)]
    pub recurrence: Option<RecurrenceSummary>,
    /// Free/busy display.
    #[serde(default)]
    pub show_as: Option<FreeBusyStatus>,
    /// Reminder lead time in minutes.
    #[serde(default)]
    pub reminder_minutes: Option<u32>,
    /// Sensitivity.
    #[serde(default)]
    pub sensitivity: Option<Sensitivity>,
}

impl EventFields {
    /// The Graph draft.
    #[must_use]
    pub fn to_draft(&self, transaction_id: Option<String>) -> EventDraft {
        EventDraft {
            subject: self.subject.clone(),
            body: self.body.as_ref().map(|b| MessageBody {
                content_type: if self.body_html { BodyType::Html } else { BodyType::Text },
                content: b.clone(),
            }),
            start: self.start.clone(),
            end: self.end.clone(),
            is_all_day: self.is_all_day,
            location: self.location.clone(),
            attendees: self.attendees.clone(),
            show_as: self.show_as.clone(),
            sensitivity: self.sensitivity.clone(),
            recurrence: self.recurrence.clone(),
            is_reminder_on: self.reminder_minutes.map(|_| true),
            reminder_minutes_before_start: self.reminder_minutes,
            online_meeting: self.teams.filter(|t| *t).map(|_| OnlineMeetingProvider::TeamsForBusiness),
            allow_new_time_proposals: None,
            transaction_id,
        }
    }

    /// Applies the set fields to a local copy (the optimistic change).
    pub fn apply_to(&self, e: &mut Event) {
        if let Some(s) = &self.subject {
            e.subject = s.clone();
        }
        if let Some(s) = &self.start {
            e.start = s.clone();
        }
        if let Some(s) = &self.end {
            e.end = s.clone();
        }
        if let Some(a) = self.is_all_day {
            e.is_all_day = a;
        }
        if let Some(a) = &self.attendees {
            e.attendees = a.clone();
        }
        if let Some(l) = &self.location {
            e.location = Some(l.clone());
            e.locations = vec![l.clone()];
        }
        if let Some(b) = &self.body {
            let preview: String = b.as_str().chars().take(255).collect();
            e.body_preview = Sensitive::new(if self.body_html { strip_tags(&preview) } else { preview });
        }
        if let Some(t) = self.teams {
            e.is_online_meeting = t;
            e.online_meeting_provider = t.then_some(OnlineMeetingProvider::TeamsForBusiness);
        }
        if let Some(r) = &self.recurrence {
            e.recurrence = Some(r.clone());
        }
        if let Some(s) = &self.show_as {
            e.show_as = s.clone();
        }
        if let Some(m) = self.reminder_minutes {
            e.is_reminder_on = true;
            e.reminder_minutes_before_start = Some(m);
        }
        if let Some(s) = &self.sensitivity {
            e.sensitivity = s.clone();
        }
    }
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Builds the optimistic local event for a create.
#[must_use]
pub fn optimistic_event(
    local_id: &EventId,
    calendar: &CalendarId,
    fields: &EventFields,
    me: Option<EmailAddress>,
) -> Option<Event> {
    let mut e = Event {
        id: local_id.clone(),
        calendar_id: Some(calendar.clone()),
        subject: Sensitive::default(),
        start: fields.start.clone()?,
        end: fields.end.clone()?,
        is_all_day: false,
        location: None,
        locations: Vec::new(),
        organizer: me,
        attendees: Vec::new(),
        show_as: FreeBusyStatus::Busy,
        event_type: if fields.recurrence.is_some() { EventType::SeriesMaster } else { EventType::SingleInstance },
        recurrence: None,
        series_master_id: None,
        is_online_meeting: false,
        online_meeting_provider: None,
        online_meeting_join_url: None,
        response_status: ResponseStatus { response: ResponseType::Organizer, time: None },
        sensitivity: Sensitivity::Normal,
        body_preview: Sensitive::default(),
        is_cancelled: false,
        is_organizer: true,
        is_reminder_on: true,
        reminder_minutes_before_start: Some(15),
        web_link: None,
    };
    fields.apply_to(&mut e);
    // A local series shows as a single instance until the server expands it.
    if e.event_type == EventType::SeriesMaster {
        e.event_type = EventType::SingleInstance;
    }
    Some(e)
}

/// `{"id": ..., "mailbox": ...}`: which event and whose mailbox it lives in (`None` = `/me`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    /// Event id (possibly a local id of a pending create).
    pub id: String,
    /// Owner address for events in another user's calendar.
    #[serde(default)]
    pub mailbox: Option<String>,
}

impl Target {
    fn mailbox(&self) -> Mailbox {
        self.mailbox.as_ref().map_or(Mailbox::Me, |m| Mailbox::user(m.clone()))
    }
}

/// Create payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatePayload {
    /// The optimistic id (also Graph's `transactionId`).
    pub local_id: String,
    /// Target calendar (`None` = default).
    #[serde(default)]
    pub calendar: Option<CalendarId>,
    /// Owner address when creating in another user's calendar.
    #[serde(default)]
    pub mailbox: Option<String>,
    /// The fields.
    pub fields: EventFields,
}

/// Update payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatePayload {
    /// The event.
    pub target: Target,
    /// Changed fields.
    pub fields: EventFields,
    /// The event before the change (to revert).
    #[serde(default)]
    pub original: Option<Event>,
}

/// Delete or cancel payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovePayload {
    /// The event.
    pub target: Target,
    /// Message to attendees (cancel only).
    #[serde(default)]
    pub comment: Option<Sensitive>,
    /// The event before the change (to revert).
    #[serde(default)]
    pub original: Option<Event>,
}

/// A meeting response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    /// Accept.
    Accept,
    /// Tentatively accept.
    Tentative,
    /// Decline.
    Decline,
}

impl Response {
    /// Parses `accept`, `tentative` (`tentativelyAccept`, `maybe`) or `decline`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "accept" | "accepted" | "yes" => Some(Self::Accept),
            "tentative" | "tentativelyaccept" | "tentativelyaccepted" | "maybe" => Some(Self::Tentative),
            "decline" | "declined" | "no" => Some(Self::Decline),
            _ => None,
        }
    }

    /// The local response status after this response.
    #[must_use]
    pub fn status(self) -> ResponseType {
        match self {
            Self::Accept => ResponseType::Accepted,
            Self::Tentative => ResponseType::TentativelyAccepted,
            Self::Decline => ResponseType::Declined,
        }
    }
}

/// Respond payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RespondPayload {
    /// The event.
    pub target: Target,
    /// The response.
    pub response: Response,
    /// Message to the organizer.
    #[serde(default)]
    pub comment: Option<Sensitive>,
    /// Proposed new time (tentative or decline).
    #[serde(default)]
    pub propose: Option<TimeSlot>,
    /// Send the response to the organizer.
    #[serde(default = "yes")]
    pub send: bool,
    /// The response before (to revert).
    #[serde(default)]
    pub original: Option<ResponseStatus>,
}

fn yes() -> bool {
    true
}

/// Forward payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardPayload {
    /// The event.
    pub target: Target,
    /// Recipients.
    pub to: Vec<EmailAddress>,
    /// Message.
    #[serde(default)]
    pub comment: Option<Sensitive>,
}

/// Snooze or dismiss payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReminderPayload {
    /// The event.
    pub target: Target,
    /// Snooze until (snooze only).
    #[serde(default)]
    pub until: Option<Timestamp>,
}

/// Queues `payload` of `kind` for event `id` (call inside the write that applies the
/// optimistic change).
///
/// # Errors
/// As [`Tx::outbox_enqueue`], or [`ErrorKind::Invalid`] if the payload does not serialise.
pub fn enqueue<T: Serialize>(
    tx: &mut Tx<'_>,
    kind: &str,
    id: &str,
    local_id: Option<&str>,
    payload: &T,
) -> quorum_store::Result<()> {
    let payload = serde_json::to_value(payload)
        .map_err(|_| quorum_store::Error::new(quorum_store::ErrorKind::InvalidInput, "calendar payload"))?;
    tx.outbox_enqueue(&NewOutboxEntry {
        kind: kind.to_owned(),
        resource: resource_key(id),
        local_id: local_id.map(str::to_owned),
        payload,
    })?;
    Ok(())
}

fn decode<T: serde::de::DeserializeOwned>(payload: Value) -> Result<T> {
    serde_json::from_value(payload).map_err(|_| Error::new(ErrorKind::Permanent, "calendar outbox payload is invalid"))
}

/// Resolves a local id to the server id once its create replayed.
async fn resolve(ctx: &Ctx, id: &str) -> Result<EventId> {
    let id = if id.starts_with(LOCAL_ID_PREFIX) {
        ctx.store
            .setting::<String>(local_map_key(id))
            .await?
            .ok_or_else(|| Error::new(ErrorKind::Permanent, "the event was never created on the server"))?
    } else {
        id.to_owned()
    };
    EventId::new(id).map_err(|_| Error::new(ErrorKind::Permanent, "invalid event id"))
}

async fn restore(ctx: &Ctx, original: Option<Event>) -> Result<()> {
    if let Some(e) = original
        && let Some(cal) = e.calendar_id.clone()
    {
        ctx.store.write(move |tx| tx.upsert_events(&cal, &[e]).map(|_| ())).await?;
    }
    Ok(())
}

async fn drop_local(ctx: &Ctx, id: &str) -> Result<()> {
    if let Ok(id) = EventId::new(id) {
        ctx.store.write(move |tx| tx.delete_events(&[id])).await?;
    }
    Ok(())
}

fn not_found_is_ok(r: quorum_graph::Result<()>) -> Result<()> {
    match r {
        Err(e) if e.kind() == G::NotFound => Ok(()),
        other => other.map_err(Error::from),
    }
}

/// Builds `POST /groups/{id}/events` (group calendars are not under a mailbox).
///
/// # Errors
/// [`ErrorKind::Invalid`] for an invalid group id.
pub fn group_create_request(group_id: &str, draft: &EventDraft) -> Result<Request> {
    if group_id.is_empty() || group_id.len() > 64 || !group_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(Error::new(ErrorKind::Permanent, "invalid group id"));
    }
    if draft.start.is_none() || draft.end.is_none() {
        return Err(Error::new(ErrorKind::Permanent, "a new event needs a start and an end"));
    }
    Ok(Request::new(Method::Post, format!("/groups/{group_id}/events"))
        .family(ResourceFamily::Calendar)
        .prefer(format!(
            "{}, {}",
            quorum_graph::api::mail::PREFER_IMMUTABLE_ID,
            quorum_graph::api::calendar::PREFER_UTC
        ))
        .json_value(draft.to_json()?)
        .idempotent(draft.transaction_id.is_some()))
}

macro_rules! handler {
    ($name:ident, $kind:expr, $apply:ident, $revert:ident) => {
        #[derive(Debug, Default)]
        /// Replays one kind of calendar write.
        pub struct $name;

        impl OutboxHandler for $name {
            fn kind(&self) -> &'static str {
                $kind
            }
            fn apply<'a>(&'a self, ctx: &'a Ctx, payload: Value) -> BoxFuture<'a, Result<Value>> {
                Box::pin($apply(ctx, payload))
            }
            fn revert<'a>(&'a self, ctx: &'a Ctx, payload: Value) -> BoxFuture<'a, Result<()>> {
                Box::pin($revert(ctx, payload))
            }
        }
    };
}

handler!(CreateHandler, KIND_CREATE, apply_create, revert_create);
handler!(UpdateHandler, KIND_UPDATE, apply_update, revert_update);
handler!(DeleteHandler, KIND_DELETE, apply_delete, revert_remove);
handler!(RespondHandler, KIND_RESPOND, apply_respond, revert_respond);
handler!(CancelHandler, KIND_CANCEL, apply_cancel, revert_remove);
handler!(ForwardHandler, KIND_FORWARD, apply_forward, revert_nothing);
handler!(SnoozeHandler, KIND_SNOOZE, apply_snooze, revert_nothing);
handler!(DismissHandler, KIND_DISMISS, apply_dismiss, revert_nothing);

/// Every calendar outbox handler.
#[must_use]
pub fn handlers() -> Vec<std::sync::Arc<dyn OutboxHandler>> {
    vec![
        std::sync::Arc::new(CreateHandler),
        std::sync::Arc::new(UpdateHandler),
        std::sync::Arc::new(DeleteHandler),
        std::sync::Arc::new(RespondHandler),
        std::sync::Arc::new(CancelHandler),
        std::sync::Arc::new(ForwardHandler),
        std::sync::Arc::new(SnoozeHandler),
        std::sync::Arc::new(DismissHandler),
    ]
}

async fn apply_create(ctx: &Ctx, payload: Value) -> Result<Value> {
    let p: CreatePayload = decode(payload)?;
    let draft = p.fields.to_draft(Some(p.local_id.clone()));
    let mailbox = p.mailbox.as_ref().map_or(Mailbox::Me, |m| Mailbox::user(m.clone()));
    let created = match p.calendar.as_ref().and_then(|c| c.as_str().strip_prefix(GROUP_CAL_PREFIX)) {
        Some(gid) => {
            let v: Value = ctx.graph.send_json(group_create_request(gid, &draft)?).await?;
            quorum_graph::api::calendar::event_from_json(v, p.calendar.as_ref())
                .ok_or_else(|| Error::new(ErrorKind::Remote, "created event did not parse"))?
        }
        None => {
            let cal = p.calendar.clone().map_or(CalendarRef::Default, CalendarRef::Id);
            ctx.graph.create_event(&mailbox, &cal, &draft).await?
        }
    };
    let calendar = match p.calendar.clone().or_else(|| created.calendar_id.clone()) {
        Some(c) => Some(c),
        None => ctx
            .store
            .calendars()
            .await?
            .items
            .into_iter()
            .find(|c| c.is_default_calendar && c.source == quorum_model::CalendarSource::Own)
            .map(|c| c.id),
    };
    let real = created.id.as_str().to_owned();
    let local = p.local_id.clone();
    ctx.store
        .write(move |tx| {
            if let Ok(lid) = EventId::new(local.clone()) {
                tx.delete_events(&[lid])?;
            }
            if let Some(cal) = calendar {
                tx.upsert_events(&cal, &[created])?;
            }
            tx.set_setting(&local_map_key(&local), &real)
        })
        .await?;
    Ok(json!({ "id": resolve(ctx, &p.local_id).await?.as_str() }))
}

async fn revert_create(ctx: &Ctx, payload: Value) -> Result<()> {
    let p: CreatePayload = decode(payload)?;
    drop_local(ctx, &p.local_id).await
}

async fn apply_update(ctx: &Ctx, payload: Value) -> Result<Value> {
    let p: UpdatePayload = decode(payload)?;
    let id = resolve(ctx, &p.target.id).await?;
    let updated = ctx.graph.update_event(&p.target.mailbox(), &id, &p.fields.to_draft(None)).await?;
    let cal = p.original.as_ref().and_then(|o| o.calendar_id.clone());
    if let Some(cal) = cal {
        ctx.store.write(move |tx| tx.upsert_events(&cal, &[updated]).map(|_| ())).await?;
    }
    Ok(json!({ "id": id.as_str() }))
}

async fn revert_update(ctx: &Ctx, payload: Value) -> Result<()> {
    let p: UpdatePayload = decode(payload)?;
    restore(ctx, p.original).await
}

async fn apply_delete(ctx: &Ctx, payload: Value) -> Result<Value> {
    let p: RemovePayload = decode(payload)?;
    let id = resolve(ctx, &p.target.id).await?;
    not_found_is_ok(ctx.graph.delete_event(&p.target.mailbox(), &id).await)?;
    Ok(Value::Null)
}

async fn revert_remove(ctx: &Ctx, payload: Value) -> Result<()> {
    let p: RemovePayload = decode(payload)?;
    restore(ctx, p.original).await
}

async fn apply_cancel(ctx: &Ctx, payload: Value) -> Result<Value> {
    let p: RemovePayload = decode(payload)?;
    let id = resolve(ctx, &p.target.id).await?;
    let comment = p.comment.as_ref().map(Sensitive::as_str);
    not_found_is_ok(ctx.graph.cancel_event(&p.target.mailbox(), &id, comment).await)?;
    Ok(Value::Null)
}

async fn apply_respond(ctx: &Ctx, payload: Value) -> Result<Value> {
    let p: RespondPayload = decode(payload)?;
    let id = resolve(ctx, &p.target.id).await?;
    let rsvp = match p.response {
        Response::Accept => Rsvp::Accept,
        Response::Tentative => Rsvp::Tentative { proposed_new_time: p.propose.clone() },
        Response::Decline => Rsvp::Decline { proposed_new_time: p.propose.clone() },
    };
    let comment = p.comment.as_ref().map(Sensitive::as_str);
    ctx.graph.respond_to_event(&p.target.mailbox(), &id, &rsvp, comment, p.send || p.propose.is_some()).await?;
    if p.response == Response::Decline {
        // Graph removes declined meetings from the calendar; so do we.
        drop_local(ctx, id.as_str()).await?;
    }
    Ok(Value::Null)
}

async fn revert_respond(ctx: &Ctx, payload: Value) -> Result<()> {
    let p: RespondPayload = decode(payload)?;
    let (Some(original), Ok(id)) = (p.original, EventId::new(p.target.id)) else { return Ok(()) };
    ctx.store
        .write(move |tx| {
            if let Some(mut e) = tx.read().event(&id)?
                && let Some(cal) = e.calendar_id.clone()
            {
                e.response_status = original;
                tx.upsert_events(&cal, &[e])?;
            }
            Ok(())
        })
        .await?;
    Ok(())
}

async fn apply_forward(ctx: &Ctx, payload: Value) -> Result<Value> {
    let p: ForwardPayload = decode(payload)?;
    let id = resolve(ctx, &p.target.id).await?;
    let comment = p.comment.as_ref().map(Sensitive::as_str);
    ctx.graph.forward_event(&p.target.mailbox(), &id, &p.to, comment).await?;
    Ok(Value::Null)
}

async fn apply_snooze(ctx: &Ctx, payload: Value) -> Result<Value> {
    let p: ReminderPayload = decode(payload)?;
    let id = resolve(ctx, &p.target.id).await?;
    let until = p.until.ok_or_else(|| Error::new(ErrorKind::Permanent, "snooze needs a time"))?;
    ctx.graph.snooze_reminder(&p.target.mailbox(), &id, until).await?;
    Ok(Value::Null)
}

async fn apply_dismiss(ctx: &Ctx, payload: Value) -> Result<Value> {
    let p: ReminderPayload = decode(payload)?;
    let id = resolve(ctx, &p.target.id).await?;
    not_found_is_ok(ctx.graph.dismiss_reminder(&p.target.mailbox(), &id).await)?;
    Ok(Value::Null)
}

async fn revert_nothing(_ctx: &Ctx, _payload: Value) -> Result<()> {
    Ok(())
}
