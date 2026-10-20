//! Calendar endpoint wrappers against the fixture transport.

use quorum_graph::api::calendar::{
    self, CalendarRef, EVENT_SELECT, EventDraft, MAX_SCHEDULES_PER_REQUEST, MeetingTimesQuery, PlaceKind, Rsvp,
    SourceContext,
};
use quorum_graph::api::mail::{Change, Mailbox};
use quorum_graph::{
    Client, DeltaOutcome, ErrorKind, FakeClock, FixtureResponse, FixtureTransport, HttpRequest, Method, RateLimit,
    ResourceFamily, Route, StaticToken,
};
use quorum_model::{
    AttendeeType, CalendarColor, CalendarId, CalendarSource, DateTimeZone, DayOfWeek, EmailAddress, EventId, EventType,
    FreeBusyStatus, MailAddress, OnlineMeetingProvider, RecurrencePatternType, RecurrenceRangeType, RecurrenceSummary,
    ResponseType, Sensitive, Sensitivity, TimeSlot,
};
use serde_json::{Value, json};

const GRAPH: &str = "https://graph.microsoft.com/v1.0";

fn client(t: &FixtureTransport) -> Client {
    let mut b = Client::builder(StaticToken::new("test-token"));
    for f in ResourceFamily::ALL {
        b = b.rate(f, RateLimit::new(10_000.0, 10_000).unwrap());
    }
    b.transport(t.clone()).clock(FakeClock::new()).jitter_seed(1).build().unwrap()
}

fn file(name: &str) -> FixtureResponse {
    FixtureResponse::file(200, name).unwrap()
}

fn q(req: &HttpRequest, key: &str) -> Option<String> {
    req.url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned())
}

fn body(req: &HttpRequest) -> Value {
    serde_json::from_slice(&req.body).unwrap()
}

fn ts(s: &str) -> jiff::Timestamp {
    s.parse().unwrap()
}

fn eid(s: &str) -> EventId {
    EventId::new(s).unwrap()
}

fn utc(s: &str) -> DateTimeZone {
    DateTimeZone::from_timestamp_utc(ts(s))
}

const PREFER: &str = "IdType=\"ImmutableId\", outlook.timezone=\"UTC\"";

#[tokio::test]
async fn calendars_get_their_source_from_context() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me/calendars", file("calendar/calendars.json"));
    t.on(Method::Get, "/users/avery@contoso.example/calendars", file("calendar/calendars.json"));
    let c = client(&t);

    let page = c.list_calendars(&Mailbox::Me, &SourceContext::default()).await.unwrap();
    assert_eq!(page.items.len(), 3);
    assert_eq!(page.skipped, 1);
    let sources: Vec<CalendarSource> = page.items.iter().map(|c| c.source.clone()).collect();
    assert_eq!(sources, vec![CalendarSource::Own, CalendarSource::Shared, CalendarSource::Shared]);
    assert_eq!(page.items[1].color, CalendarColor::LightGreen);
    assert_eq!(page.items[2].color, CalendarColor::Unknown("neonPurple".into()));
    let call = &t.calls()[0];
    assert_eq!(q(call, "$select").as_deref(), Some(calendar::CALENDAR_SELECT));
    assert_eq!(call.header("prefer"), Some(PREFER));

    let me = SourceContext { me: Some(MailAddress::new("Test.User@contoso.example").unwrap()) };
    let page = c.list_calendars(&Mailbox::Me, &me).await.unwrap();
    assert_eq!(page.items[0].source, CalendarSource::Own);
    assert_eq!(page.items[1].source, CalendarSource::Shared);

    let other = Mailbox::user("avery@contoso.example");
    let page = c.list_calendars(&other, &me).await.unwrap();
    assert!(page.items.iter().all(|c| c.source == CalendarSource::OpenedUser));
    assert_eq!(calendar::list_calendars_request(&other).unwrap().bucket_key(), "avery@contoso.example");
}

#[tokio::test]
async fn calendar_groups_and_their_calendars() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me/calendarGroups", file("calendar/calendar-groups.json"));
    t.on(Method::Get, "/me/calendarGroups/AAMkAD-grp-1=/calendars", file("calendar/calendars.json"));
    let c = client(&t);
    let groups = c.list_calendar_groups(&Mailbox::Me).await.unwrap();
    assert_eq!(groups.items.len(), 2);
    assert_eq!(groups.skipped, 1);
    let cals = c.list_group_calendars(&Mailbox::Me, &groups.items[0].id, &SourceContext::default()).await.unwrap();
    assert_eq!(cals.items.len(), 3);
}

#[tokio::test]
async fn calendar_view_maps_recurrence_teams_and_follows_next_link() {
    let t = FixtureTransport::new();
    let path = "/me/calendars/AAMkAD-cal-own=/calendarView";
    t.on(Method::Get, path, file("calendar/calendar-view-page1.json"));
    t.add(
        Route::new(Method::Get, path)
            .query("$skiptoken", "view-page2")
            .respond(file("calendar/calendar-view-page2.json")),
    );
    let c = client(&t);
    let cal = CalendarId::new("AAMkAD-cal-own=").unwrap();
    let (start, end) = (ts("2026-10-19T00:00:00Z"), ts("2026-10-26T00:00:00Z"));
    let req = calendar::calendar_view_request(&Mailbox::Me, &CalendarRef::Id(cal.clone()), start, end, None).unwrap();
    assert_eq!(req.resource_family(), ResourceFamily::Calendar);
    let page = c.events_page(req.clone(), Some(&cal)).await.unwrap();
    let call = &t.calls()[0];
    assert_eq!(q(call, "startDateTime").as_deref(), Some("2026-10-19T00:00:00Z"));
    assert_eq!(q(call, "endDateTime").as_deref(), Some("2026-10-26T00:00:00Z"));
    assert_eq!(q(call, "$select").as_deref(), Some(EVENT_SELECT));
    assert_eq!(call.header("prefer"), Some(PREFER));

    assert_eq!(page.items.len(), 2);
    let single = &page.items[0];
    assert_eq!(single.calendar_id.as_ref(), Some(&cal));
    assert_eq!(single.organizer.as_ref().unwrap().display(), "Avery Example");
    assert_eq!(single.attendees[1].kind, AttendeeType::Resource);
    assert_eq!(single.response_status.response, ResponseType::Accepted);
    assert_eq!(single.online_meeting_provider, None, "provider `unknown` maps to None");
    assert_eq!(single.start.to_timestamp().unwrap(), ts("2026-10-21T09:00:00Z"));

    let series = &page.items[1];
    assert_eq!(series.event_type, EventType::SeriesMaster);
    let r = series.recurrence.as_ref().unwrap();
    assert_eq!(r.pattern, RecurrencePatternType::Weekly);
    assert_eq!(r.days_of_week, vec![DayOfWeek::Monday, DayOfWeek::Wednesday]);
    assert_eq!(r.range_type, Some(RecurrenceRangeType::NoEnd));
    assert_eq!(r.start_date, Some(jiff::civil::date(2026, 9, 7)));
    assert_eq!(r.end_date, None, "0001-01-01 means no end date");
    assert_eq!(r.day_of_month, None);
    assert_eq!(r.number_of_occurrences, None);

    let page2 = c.events_page(req.follow(page.next_link.unwrap()), Some(&cal)).await.unwrap();
    let teams = &page2.items[0];
    assert!(teams.is_online_meeting);
    assert_eq!(teams.online_meeting_provider, Some(OnlineMeetingProvider::TeamsForBusiness));
    assert_eq!(teams.online_meeting_join_url.as_deref(), Some("https://teams.example/l/meetup-join/synthetic"));
    assert_eq!(t.calls()[1].header("prefer"), Some(PREFER));

    assert_eq!(
        calendar::calendar_view_request(&Mailbox::Me, &CalendarRef::Default, end, start, None).unwrap_err().kind(),
        ErrorKind::Config
    );
    let other = calendar::calendar_view_request(
        &Mailbox::user("avery@contoso.example"),
        &CalendarRef::Default,
        start,
        end,
        None,
    )
    .unwrap();
    assert_eq!(other.target(), "/users/avery@contoso.example/calendarView");
}

#[tokio::test]
async fn calendar_view_delta_round_trip() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me/calendarView/delta", file("calendar/view-delta-page1.json"));
    t.add(
        Route::new(Method::Get, "/me/calendarView/delta")
            .query("$skiptoken", "cal-delta-2")
            .respond(file("calendar/view-delta-page2.json")),
    );
    t.add(
        Route::new(Method::Get, "/me/calendarView/delta")
            .query("$deltatoken", "gone")
            .respond(FixtureResponse::file(410, "delta-expired.json").unwrap()),
    );
    let c = client(&t);
    let start = calendar::calendar_view_delta_request(
        &Mailbox::Me,
        ts("2026-09-17T00:00:00Z"),
        ts("2027-04-15T00:00:00Z"),
        Some(50),
    )
    .unwrap();
    let p1 = c.calendar_view_delta_page(start.clone()).await.unwrap();
    let call = &t.calls()[0];
    assert!(q(call, "$select").is_none(), "calendarView delta does not support $select");
    assert_eq!(q(call, "startDateTime").as_deref(), Some("2026-09-17T00:00:00Z"));
    assert_eq!(call.header("prefer"), Some("IdType=\"ImmutableId\", outlook.timezone=\"UTC\", odata.maxpagesize=50"));
    assert!(matches!(&p1.items[1], Change::Removed(id) if id.as_str() == "AAMkAD-evt-0099="));
    let DeltaOutcome::Next(next) = p1.outcome else { panic!("expected next") };
    let p2 = c.calendar_view_delta_page(start.follow(next)).await.unwrap();
    let DeltaOutcome::Done(delta) = p2.outcome else { panic!("expected delta link") };
    assert_eq!(delta, format!("{GRAPH}/me/calendarView/delta?$deltatoken=cal-token-1"));
    match &p2.items[0] {
        Change::Upsert(e) => assert!(e.is_cancelled),
        other => panic!("{other:?}"),
    }
    let stored = calendar::calendar_delta_link_request(
        &Mailbox::Me,
        format!("{GRAPH}/me/calendarView/delta?$deltatoken=gone"),
        Some(50),
    );
    assert_eq!(c.calendar_view_delta_page(stored).await.unwrap_err().kind(), ErrorKind::DeltaExpired);
}

#[tokio::test]
async fn hostile_events_parse_without_panics() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me/calendarView", file("calendar/events-hostile.json"));
    let c = client(&t);
    let page = c
        .calendar_view(&Mailbox::Me, &CalendarRef::Default, ts("2026-10-01T00:00:00Z"), ts("2026-11-01T00:00:00Z"))
        .await
        .unwrap();
    let ids: Vec<&str> = page.items.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["AAMkAD-evt-min=", "AAMkAD-evt-0006=", "AAMkAD-evt-0008=", "AAMkAD-evt-0009="]);
    assert_eq!(page.skipped, 4);
    let unknown = &page.items[1];
    assert_eq!(unknown.show_as, FreeBusyStatus::Unknown("focusTime".into()));
    assert_eq!(unknown.event_type, EventType::Unknown("newKind".into()));
    assert_eq!(unknown.sensitivity, Sensitivity::Unknown("topSecret".into()));
    assert!(unknown.attendees.is_empty());
    assert!(page.items[2].recurrence.is_none(), "a malformed recurrence drops only the recurrence");
    let lunar = page.items[3].recurrence.as_ref().unwrap();
    assert_eq!(lunar.pattern, RecurrencePatternType::Unknown("lunar".into()));
    assert_eq!(lunar.days_of_week, vec![DayOfWeek::Unknown("funday".into())]);
    assert_eq!(lunar.start_date, None);
}

#[tokio::test]
async fn event_crud_and_teams_meeting_body() {
    let t = FixtureTransport::new();
    let created = json!({
        "id": "AAMkAD-evt-new=",
        "subject": "Synthetic sync",
        "start": {"dateTime": "2026-10-21T09:00:00.0000000", "timeZone": "UTC"},
        "end": {"dateTime": "2026-10-21T09:30:00.0000000", "timeZone": "UTC"},
        "isOnlineMeeting": true,
        "onlineMeetingProvider": "teamsForBusiness",
        "onlineMeeting": {"joinUrl": "https://teams.example/l/meetup-join/new"}
    });
    t.add(
        Route::new(Method::Post, "/me/calendars/AAMkAD-cal-own=/events")
            .respond(FixtureResponse::status(503))
            .respond(FixtureResponse::json(201, &created)),
    );
    t.on(Method::Get, "/me/events/AAMkAD-evt-0001=", file("calendar/event-with-body.json"));
    t.on(Method::Patch, "/me/events/AAMkAD-evt-new=", FixtureResponse::json(200, &created));
    t.on(
        Method::Delete,
        "/me/events/AAMkAD-evt-missing=",
        FixtureResponse::json(404, &json!({"error": {"code": "ErrorItemNotFound"}})),
    );
    let c = client(&t);
    let cal = CalendarRef::Id(CalendarId::new("AAMkAD-cal-own=").unwrap());
    let draft = EventDraft {
        subject: Some(Sensitive::new("Synthetic sync")),
        start: Some(utc("2026-10-21T09:00:00Z")),
        end: Some(utc("2026-10-21T09:30:00Z")),
        attendees: Some(vec![quorum_model::Attendee {
            email_address: EmailAddress::new("Avery", "avery@contoso.example").unwrap(),
            kind: AttendeeType::Optional,
            status: Default::default(),
        }]),
        recurrence: Some(RecurrenceSummary {
            pattern: RecurrencePatternType::Weekly,
            interval: 1,
            days_of_week: vec![DayOfWeek::Tuesday],
            day_of_month: None,
            range_type: None,
            start_date: Some(jiff::civil::date(2026, 10, 20)),
            end_date: None,
            number_of_occurrences: Some(10),
        }),
        transaction_id: Some("00000000-0000-0000-0000-00000000aaaa".into()),
        ..Default::default()
    }
    .with_teams_meeting();
    let ev = c.create_event(&Mailbox::Me, &cal, &draft).await.unwrap();
    assert_eq!(t.call_count(), 2, "create with transactionId is retried");
    assert_eq!(ev.calendar_id.as_ref().map(CalendarId::as_str), Some("AAMkAD-cal-own="));
    assert_eq!(
        body(&t.calls()[1]),
        json!({
            "subject": "Synthetic sync",
            "start": {"dateTime": "2026-10-21T09:00:00", "timeZone": "UTC"},
            "end": {"dateTime": "2026-10-21T09:30:00", "timeZone": "UTC"},
            "attendees": [{"emailAddress": {"name": "Avery", "address": "avery@contoso.example"}, "type": "optional"}],
            "recurrence": {
                "pattern": {"type": "weekly", "interval": 1, "daysOfWeek": ["tuesday"]},
                "range": {"type": "numbered", "startDate": "2026-10-20", "numberOfOccurrences": 10}
            },
            "isOnlineMeeting": true,
            "onlineMeetingProvider": "teamsForBusiness",
            "transactionId": "00000000-0000-0000-0000-00000000aaaa"
        })
    );
    let no_txn = EventDraft { transaction_id: None, ..draft.clone() };
    assert!(!calendar::create_event_request(&Mailbox::Me, &cal, &no_txn).unwrap().is_idempotent());
    assert_eq!(
        calendar::create_event_request(&Mailbox::Me, &cal, &EventDraft::default()).unwrap_err().kind(),
        ErrorKind::Config
    );

    let full = c.get_event(&Mailbox::Me, &eid("AAMkAD-evt-0001=")).await.unwrap();
    assert!(full.body.content.as_str().contains("agenda"));
    assert_eq!(
        t.calls()[2].header("prefer"),
        Some("IdType=\"ImmutableId\", outlook.timezone=\"UTC\", outlook.body-content-type=\"html\"")
    );

    let update = EventDraft { subject: Some("Renamed".into()), ..Default::default() };
    c.update_event(&Mailbox::Me, &eid("AAMkAD-evt-new="), &update).await.unwrap();
    assert_eq!(body(&t.calls()[3]), json!({"subject": "Renamed"}));
    assert_eq!(
        calendar::update_event_request(&Mailbox::Me, &eid("x"), &EventDraft::default()).unwrap_err().kind(),
        ErrorKind::Config
    );
    let err = c.delete_event(&Mailbox::Me, &eid("AAMkAD-evt-missing=")).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotFound);
}

#[tokio::test]
async fn rsvp_cancel_forward_reminders_and_instances() {
    let t = FixtureTransport::new();
    let base = "/me/events/AAMkAD-evt-0001=";
    for action in ["accept", "tentativelyAccept", "decline", "cancel", "forward", "snoozeReminder", "dismissReminder"] {
        t.on(Method::Post, &format!("{base}/{action}"), FixtureResponse::status(202));
    }
    t.on(Method::Get, &format!("{base}/instances"), file("calendar/calendar-view-page2.json"));
    let c = client(&t);
    let id = eid("AAMkAD-evt-0001=");
    let mb = Mailbox::Me;
    c.respond_to_event(&mb, &id, &Rsvp::Accept, Some("See you"), true).await.unwrap();
    assert_eq!(body(&t.calls()[0]), json!({"sendResponse": true, "comment": "See you"}));
    let slot = TimeSlot { start: utc("2026-10-22T09:00:00Z"), end: utc("2026-10-22T10:00:00Z") };
    c.respond_to_event(&mb, &id, &Rsvp::Decline { proposed_new_time: Some(slot.clone()) }, None, true).await.unwrap();
    assert_eq!(
        body(&t.calls()[1]),
        json!({
            "sendResponse": true,
            "proposedNewTime": {
                "start": {"dateTime": "2026-10-22T09:00:00", "timeZone": "UTC"},
                "end": {"dateTime": "2026-10-22T10:00:00", "timeZone": "UTC"}
            }
        })
    );
    assert_eq!(t.calls()[1].url.path(), "/v1.0/me/events/AAMkAD-evt-0001=/decline");
    let err = calendar::respond_request(&mb, &id, &Rsvp::Tentative { proposed_new_time: Some(slot) }, None, false)
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Config);
    c.respond_to_event(&mb, &id, &Rsvp::Tentative { proposed_new_time: None }, None, false).await.unwrap();
    assert_eq!(t.calls()[2].url.path(), "/v1.0/me/events/AAMkAD-evt-0001=/tentativelyAccept");

    c.cancel_event(&mb, &id, Some("Synthetic cancel")).await.unwrap();
    assert_eq!(body(&t.calls()[3]), json!({"comment": "Synthetic cancel"}));
    let to = [EmailAddress::new("", "fwd@example.com").unwrap()];
    c.forward_event(&mb, &id, &to, None).await.unwrap();
    assert_eq!(body(&t.calls()[4]), json!({"toRecipients": [{"emailAddress": {"address": "fwd@example.com"}}]}));
    assert_eq!(calendar::forward_event_request(&mb, &id, &[], None).unwrap_err().kind(), ErrorKind::Config);
    c.snooze_reminder(&mb, &id, ts("2026-10-21T08:55:00Z")).await.unwrap();
    assert_eq!(body(&t.calls()[5]), json!({"newReminderTime": {"dateTime": "2026-10-21T08:55:00", "timeZone": "UTC"}}));
    c.dismiss_reminder(&mb, &id).await.unwrap();
    assert!(t.calls()[6].body.is_empty());

    let inst = c.list_event_instances(&mb, &id, ts("2026-10-01T00:00:00Z"), ts("2026-11-01T00:00:00Z")).await.unwrap();
    assert_eq!(inst.items.len(), 1);
    assert_eq!(q(&t.calls()[7], "startDateTime").as_deref(), Some("2026-10-01T00:00:00Z"));
}

#[tokio::test]
async fn get_schedule_chunks_by_twenty_and_maps_errors() {
    let t = FixtureTransport::new();
    t.on(Method::Post, "/me/calendar/getSchedule", file("calendar/schedule.json"));
    let c = client(&t);
    let people: Vec<MailAddress> =
        (0..45).map(|i| MailAddress::new(format!("person{i:02}@contoso.example")).unwrap()).collect();
    let (start, end) = (ts("2026-10-20T08:00:00Z"), ts("2026-10-20T18:00:00Z"));
    let out = c.get_schedule(&Mailbox::Me, &people, start, end, 30).await.unwrap();
    assert_eq!(out.requests, 3);
    assert_eq!(t.call_count(), 3);
    let sizes: Vec<usize> = t.calls().iter().map(|r| body(r)["schedules"].as_array().unwrap().len()).collect();
    assert_eq!(sizes, vec![MAX_SCHEDULES_PER_REQUEST, MAX_SCHEDULES_PER_REQUEST, 5]);
    let first = body(&t.calls()[0]);
    assert_eq!(first["schedules"][0], "person00@contoso.example");
    assert_eq!(first["startTime"], json!({"dateTime": "2026-10-20T08:00:00", "timeZone": "UTC"}));
    assert_eq!(first["availabilityViewInterval"], 30);
    assert_eq!(body(&t.calls()[2])["schedules"][0], "person40@contoso.example");

    // Each response holds three schedules; one busy item in the first is hostile and dropped.
    assert_eq!(out.schedules.len(), 9);
    let s0 = &out.schedules[0];
    assert_eq!(s0.schedule_items.len(), 1);
    assert_eq!(
        s0.availability(),
        vec![FreeBusyStatus::Free, FreeBusyStatus::Busy, FreeBusyStatus::Busy, FreeBusyStatus::Free]
    );
    assert_eq!(out.schedules[1].availability()[2], FreeBusyStatus::Unknown("x".into()));
    assert_eq!(out.schedules[2].error.as_deref(), Some("ErrorMailRecipientNotFound"));
    assert!(!format!("{:?}", out.schedules[2]).contains("nobody@"), "free-text error is not kept");

    assert!(c.get_schedule(&Mailbox::Me, &[], start, end, 30).await.unwrap().schedules.is_empty());
    let too_long = c.get_schedule(&Mailbox::Me, &people, start, ts("2027-01-01T00:00:00Z"), 30).await;
    assert_eq!(too_long.unwrap_err().kind(), ErrorKind::Config);
    assert_eq!(
        calendar::get_schedule_request(&Mailbox::Me, &people[..1], start, end, 2).unwrap_err().kind(),
        ErrorKind::Config
    );
    assert_eq!(
        calendar::get_schedule_request(&Mailbox::Me, &people, start, end, 30).unwrap_err().kind(),
        ErrorKind::Config,
        "a single request takes at most 20"
    );
    assert_eq!(t.call_count(), 3, "invalid queries send nothing");
}

#[tokio::test]
async fn get_schedule_retries_after_429() {
    let t = FixtureTransport::new();
    t.add(
        Route::new(Method::Post, "/me/calendar/getSchedule")
            .respond(FixtureResponse::file(429, "throttled.json").unwrap().with_header("Retry-After", "2"))
            .respond(file("calendar/schedule.json")),
    );
    let c = client(&t);
    let one = [MailAddress::new("person01@contoso.example").unwrap()];
    let out =
        c.get_schedule(&Mailbox::Me, &one, ts("2026-10-20T08:00:00Z"), ts("2026-10-20T18:00:00Z"), 60).await.unwrap();
    assert_eq!(t.call_count(), 2);
    assert_eq!(out.schedules.len(), 3);
}

#[tokio::test]
async fn find_meeting_times_body_and_mapping() {
    let t = FixtureTransport::new();
    t.add(
        Route::new(Method::Post, "/me/findMeetingTimes")
            .respond(file("calendar/find-meeting-times.json"))
            .respond(file("calendar/find-meeting-times-empty.json")),
    );
    let c = client(&t);
    let query = MeetingTimesQuery {
        attendees: vec![(MailAddress::new("person01@contoso.example").unwrap(), AttendeeType::Required)],
        windows: vec![TimeSlot { start: utc("2026-10-21T08:00:00Z"), end: utc("2026-10-21T17:00:00Z") }],
        duration_minutes: 60,
        max_candidates: Some(5),
        minimum_attendee_percentage: Some(100),
        ..Default::default()
    };
    let found = c.find_meeting_times(&query).await.unwrap();
    assert_eq!(found.suggestions.len(), 1);
    assert_eq!(found.skipped, 2, "NaN-as-string confidence and null slot are skipped");
    assert_eq!(found.empty_suggestions_reason, None);
    let s = &found.suggestions[0];
    assert_eq!(s.confidence_percent(), 100.0);
    assert_eq!(s.attendee_availability[0].attendee.address.as_ref().unwrap().as_str(), "person01@contoso.example");
    assert_eq!(s.locations[0].display_name.as_str(), "Room Synthetic 1");
    let sent = body(&t.calls()[0]);
    assert_eq!(sent["meetingDuration"], "PT60M");
    assert_eq!(
        sent["attendees"][0],
        json!({"type": "required", "emailAddress": {"address": "person01@contoso.example"}})
    );
    assert_eq!(sent["timeConstraint"]["timeSlots"][0]["start"]["timeZone"], "UTC");
    assert_eq!(sent["maxCandidates"], 5);

    let empty = c.find_meeting_times(&query).await.unwrap();
    assert!(empty.suggestions.is_empty());
    assert_eq!(empty.empty_suggestions_reason.as_deref(), Some("attendeesUnavailable"));
    let bad = MeetingTimesQuery { duration_minutes: 0, ..query };
    assert_eq!(calendar::find_meeting_times_request(&bad).unwrap_err().kind(), ErrorKind::Config);
}

#[tokio::test]
async fn places_rooms_and_room_lists() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/places/microsoft.graph.room", file("calendar/rooms.json"));
    t.on(Method::Get, "/places/microsoft.graph.roomlist", file("calendar/room-lists.json"));
    t.on(Method::Get, "/places/hq-rooms@contoso.example/microsoft.graph.roomlist/rooms", file("calendar/rooms.json"));
    let c = client(&t);
    let rooms = c.list_rooms().await.unwrap();
    assert_eq!(rooms.items.len(), 2);
    assert_eq!(rooms.skipped, 1);
    let r = &rooms.items[0];
    assert_eq!(r.kind, PlaceKind::Room);
    assert_eq!(r.capacity, Some(8));
    assert_eq!(r.floor_number, Some(2));
    assert!(r.is_wheelchair_accessible);
    assert!(rooms.items[1].email_address.is_none());
    assert!(rooms.items[1].building.is_none());
    let lists = c.list_room_lists().await.unwrap();
    assert_eq!(lists.items[0].kind, PlaceKind::RoomList);
    let list_addr = lists.items[0].email_address.clone().unwrap();
    let inner = c.list_rooms_in(&list_addr).await.unwrap();
    assert_eq!(inner.items.len(), 2);
    assert_eq!(calendar::list_rooms_request().resource_family(), ResourceFamily::Calendar);
}
