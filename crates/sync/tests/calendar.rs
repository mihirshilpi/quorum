//! Calendar sync from fixture Graph: calendar list (groups, Microsoft 365 groups, mailbox time
//! zone), the default calendar's delta over the rolling window (re-anchor, prune, expiry),
//! polling of other layers, opened people, the merged range query, free/busy chunking and
//! caching, and every outbox handler's request shape.

use jiff::Timestamp;
use jiff::civil::date;
use quorum_graph::{
    Client, FakeClock, FixtureResponse, FixtureTransport, HttpRequest, Method, RateLimit, ResourceFamily, Route,
    StaticToken,
};
use quorum_model::{
    Calendar, CalendarColor, CalendarId, CalendarSource, DateTimeZone, EmailAddress, Event, EventId, ResponseType,
    Sensitive, TimeSlot,
};
use quorum_store::{DeltaState, Store};
use quorum_sync::calendar::outbox::{
    self as ob, CreatePayload, EventFields, ForwardPayload, ReminderPayload, RemovePayload, RespondPayload, Response,
    Target, UpdatePayload,
};
use quorum_sync::calendar::prefs::{
    OpenedPerson, PEOPLE_KEY, PersonMode, SOURCES_KEY, SourcePrefs, TIME_ZONE_KEY, local_map_key,
};
use quorum_sync::calendar::resources::{
    CalendarList, DEFAULT_DELTA_KEY, DefaultDelta, OtherCalendars, People, sync_window,
};
use quorum_sync::calendar::{CalendarModule, query};
use quorum_sync::{Ctx, Module, Outcome, Resource};
use serde_json::{Value, json};

fn ctx(t: &FixtureTransport) -> Ctx {
    let mut b = Client::builder(StaticToken::new("test-token"));
    for f in ResourceFamily::ALL {
        b = b.rate(f, RateLimit::new(10_000.0, 10_000).unwrap());
    }
    let graph = b.transport(t.clone()).clock(FakeClock::new()).jitter_seed(1).build().unwrap();
    Ctx { graph, store: Store::open_in_memory().unwrap(), client_id: "client".into() }
}

fn file(name: &str) -> FixtureResponse {
    FixtureResponse::file(200, name).unwrap()
}

fn q(r: &HttpRequest, key: &str) -> Option<String> {
    r.url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned())
}

fn body(r: &HttpRequest) -> Value {
    serde_json::from_slice(&r.body).unwrap_or(Value::Null)
}

fn cal(id: &str, source: CalendarSource, default: bool, owner: &str) -> Calendar {
    Calendar {
        id: CalendarId::new(id).unwrap(),
        name: Sensitive::new(id),
        color: CalendarColor::Auto,
        hex_color: None,
        owner: EmailAddress::new("Owner", owner).ok(),
        can_edit: true,
        can_share: true,
        is_default_calendar: default,
        source,
    }
}

fn ev(id: &str, start: &str, end: &str) -> Event {
    serde_json::from_value(json!({
        "id": id,
        "subject": format!("Synthetic {id}"),
        "start": { "dateTime": start, "timeZone": "UTC" },
        "end": { "dateTime": end, "timeZone": "UTC" },
        "isOrganizer": false,
        "attendees": [{ "emailAddress": { "name": "A", "address": "a@example.com" }, "type": "required" }],
    }))
    .unwrap()
}

async fn seed(c: &Ctx, cals: Vec<Calendar>, events: Vec<(&str, Event)>) {
    let events: Vec<(String, Event)> = events.into_iter().map(|(c, e)| (c.to_owned(), e)).collect();
    c.store
        .write(move |tx| {
            tx.upsert_calendars(&cals)?;
            for (cal, e) in events {
                tx.upsert_events(&CalendarId::new(cal).unwrap(), &[e])?;
            }
            Ok(())
        })
        .await
        .unwrap();
}

fn today() -> jiff::civil::Date {
    Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC).date()
}

#[tokio::test]
async fn module_lists_resources_and_handlers() {
    let t = FixtureTransport::new();
    let c = ctx(&t);
    let m = CalendarModule::new();
    let keys: Vec<String> = m.discover(&c).await.unwrap().iter().map(|r| r.key()).collect();
    assert_eq!(keys, ["calendar.list", "calendar.people", "calendar.default", "calendar.others"]);
    let kinds: Vec<&str> = m.handlers().iter().map(|h| h.kind()).collect();
    for k in [
        ob::KIND_CREATE,
        ob::KIND_UPDATE,
        ob::KIND_DELETE,
        ob::KIND_RESPOND,
        ob::KIND_CANCEL,
        ob::KIND_FORWARD,
        ob::KIND_SNOOZE,
        ob::KIND_DISMISS,
    ] {
        assert!(kinds.contains(&k), "{k}");
    }
}

#[tokio::test]
async fn calendar_list_syncs_calendars_groups_and_time_zone() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me", file("people/me.json"));
    t.on(Method::Get, "/me/mailboxSettings", file("mail/mailbox-settings.json"));
    t.on(Method::Get, "/me/calendars", file("calendar/calendars.json"));
    t.on(Method::Get, "/me/calendarGroups", file("calendar/calendar-groups.json"));
    t.on(
        Method::Get,
        "/me/calendarGroups/AAMkAD-grp-1=/calendars",
        FixtureResponse::json(200, &json!({ "value": [] })),
    );
    t.on(
        Method::Get,
        "/me/memberOf/microsoft.graph.group",
        FixtureResponse::json(
            200,
            &json!({ "value": [
                { "id": "g-1", "displayName": "Synthetic team", "groupTypes": ["Unified"] },
                { "id": "g-2", "displayName": "Security group", "groupTypes": [] }
            ] }),
        ),
    );
    let c = ctx(&t);
    // A calendar that disappeared from the server is removed.
    seed(&c, vec![cal("gone", CalendarSource::Own, false, "test.user@contoso.example")], vec![]).await;
    assert_eq!(CalendarList.run(&c).await.unwrap(), Outcome::CaughtUp);
    let cals = c.store.calendars().await.unwrap().items;
    let ids: Vec<&str> = cals.iter().map(|c| c.id.as_str()).collect();
    assert!(ids.contains(&"AAMkAD-cal-own="), "{ids:?}");
    assert!(ids.contains(&"group:g-1"));
    assert!(!ids.contains(&"group:g-2"));
    assert!(!ids.contains(&"gone"));
    let own = cals.iter().find(|c| c.id.as_str() == "AAMkAD-cal-own=").unwrap();
    assert_eq!(own.source, CalendarSource::Own);
    let shared = cals.iter().find(|c| c.id.as_str() == "AAMkAD-cal-shared=").unwrap();
    assert_eq!(shared.source, CalendarSource::Shared);
    let tz: Option<String> = c.store.setting(TIME_ZONE_KEY.to_owned()).await.unwrap();
    assert_eq!(tz.as_deref(), Some("W. Europe Standard Time"));
    let cx = query::load(&c.store).await.unwrap();
    assert_eq!(cx.tz.iana_name(), Some("Europe/Berlin"));
    assert_eq!(cx.work.start_min, 480);
    // Missing optional pieces (mailbox settings, groups) never fail the pass.
    let t2 = FixtureTransport::new();
    t2.on(Method::Get, "/me", file("people/me.json"));
    t2.on(Method::Get, "/me/calendars", file("calendar/calendars.json"));
    let c2 = ctx(&t2);
    assert_eq!(CalendarList.run(&c2).await.unwrap(), Outcome::CaughtUp);
    assert!(!c2.store.calendars().await.unwrap().items.is_empty());
}

#[tokio::test]
async fn default_delta_covers_the_rolling_window_and_reanchors() {
    let t = FixtureTransport::new();
    t.add(Route::new(Method::Get, "/me/calendarView/delta").respond(file("calendar/view-delta-page1.json")));
    t.add(
        Route::new(Method::Get, "/me/calendarView/delta")
            .query("$skiptoken", "cal-delta-2")
            .respond(file("calendar/view-delta-page2.json")),
    );
    t.add(
        Route::new(Method::Get, "/me/calendarView/delta")
            .query("$deltatoken", "cal-token-1")
            .respond(FixtureResponse::json(200, &json!({ "value": [], "@odata.deltaLink": "https://graph.microsoft.com/v1.0/me/calendarView/delta?$deltatoken=cal-token-2" }))),
    );
    let c = ctx(&t);
    // Without a default calendar the pass waits for the calendar list.
    assert_eq!(DefaultDelta.run(&c).await.unwrap(), Outcome::More);
    assert_eq!(t.call_count(), 0);
    let (from, to) = sync_window(today());
    let stale_start = from.checked_sub(jiff::SignedDuration::from_hours(48)).unwrap();
    let stale_end = from.checked_sub(jiff::SignedDuration::from_hours(47)).unwrap();
    let stale = ev(
        "stale",
        &stale_start.to_zoned(jiff::tz::TimeZone::UTC).datetime().to_string(),
        &stale_end.to_zoned(jiff::tz::TimeZone::UTC).datetime().to_string(),
    );
    seed(&c, vec![cal("own", CalendarSource::Own, true, "me@example.com")], vec![("own", stale)]).await;
    assert_eq!(DefaultDelta.run(&c).await.unwrap(), Outcome::CaughtUp);
    let first = t.calls().into_iter().next().unwrap();
    assert_eq!(q(&first, "startDateTime"), Some(from.to_string()));
    assert_eq!(q(&first, "endDateTime"), Some(to.to_string()));
    assert!(first.header("Prefer").unwrap_or("").contains("outlook.timezone"));
    let e = c.store.read(|r| r.event(&EventId::new("AAMkAD-evt-0004=").unwrap())).await.unwrap().unwrap();
    assert_eq!(e.calendar_id.as_ref().map(CalendarId::as_str), Some("own"));
    assert!(c.store.read(|r| r.event(&EventId::new("stale").unwrap())).await.unwrap().is_none(), "pruned");
    let st = c.store.delta_state(DEFAULT_DELTA_KEY.to_owned()).await.unwrap().unwrap();
    assert!(st.delta_link.as_deref().unwrap().contains("cal-token-1"));
    assert_eq!(st.backfill_cursor, Some(today().to_string()));
    // Next pass polls the delta link only.
    let n = t.call_count();
    assert_eq!(DefaultDelta.run(&c).await.unwrap(), Outcome::CaughtUp);
    let next = t.calls().into_iter().nth(n).unwrap();
    assert_eq!(q(&next, "$deltatoken").as_deref(), Some("cal-token-1"));
    // A new day re-anchors: a fresh round with the new window.
    let mut st = c.store.delta_state(DEFAULT_DELTA_KEY.to_owned()).await.unwrap().unwrap();
    st.backfill_cursor = Some("2000-01-01".into());
    c.store.set_delta_state(st).await.unwrap();
    let n = t.call_count();
    DefaultDelta.run(&c).await.unwrap();
    let fresh = t.calls().into_iter().nth(n).unwrap();
    assert_eq!(q(&fresh, "startDateTime"), Some(from.to_string()));
    assert_eq!(q(&fresh, "$deltatoken"), None);
}

#[tokio::test]
async fn expired_delta_restarts_and_hostile_events_never_panic() {
    let t = FixtureTransport::new();
    t.add(
        Route::new(Method::Get, "/me/calendarView/delta")
            .respond(FixtureResponse::file(410, "delta-expired.json").unwrap()),
    );
    let c = ctx(&t);
    seed(&c, vec![cal("own", CalendarSource::Own, true, "me@example.com")], vec![]).await;
    let mut st = DeltaState::new(DEFAULT_DELTA_KEY);
    st.delta_link = Some("https://graph.microsoft.com/v1.0/me/calendarView/delta?$deltatoken=old".into());
    st.backfill_cursor = Some(today().to_string());
    c.store.set_delta_state(st).await.unwrap();
    assert_eq!(DefaultDelta.run(&c).await.unwrap(), Outcome::More);
    assert!(c.store.delta_state(DEFAULT_DELTA_KEY.to_owned()).await.unwrap().is_none());

    let t = FixtureTransport::new();
    let mut hostile: Value = serde_json::from_slice(
        &std::fs::read(quorum_graph::fixtures_dir().join("calendar/events-hostile.json")).unwrap(),
    )
    .unwrap();
    hostile["@odata.deltaLink"] = json!("https://graph.microsoft.com/v1.0/me/calendarView/delta?$deltatoken=x");
    t.add(Route::new(Method::Get, "/me/calendarView/delta").respond(FixtureResponse::json(200, &hostile)));
    let c = ctx(&t);
    seed(&c, vec![cal("own", CalendarSource::Own, true, "me@example.com")], vec![]).await;
    assert_eq!(DefaultDelta.run(&c).await.unwrap(), Outcome::CaughtUp);
    for garbage in [json!({}), json!({ "value": "nope" }), json!({ "value": [1, null, {"id": ""}] })] {
        let t = FixtureTransport::new();
        t.add(Route::new(Method::Get, "/me/calendarView/delta").respond(FixtureResponse::json(200, &garbage)));
        let c = ctx(&t);
        seed(&c, vec![cal("own", CalendarSource::Own, true, "me@example.com")], vec![]).await;
        let _ = DefaultDelta.run(&c).await; // an error is fine; a panic is not
    }
}

#[tokio::test]
async fn other_layers_are_polled_and_keep_pending_events() {
    let t = FixtureTransport::new();
    t.on(
        Method::Get,
        "/me/calendars/shared1/calendarView",
        FixtureResponse::json(
            200,
            &json!({ "value": [{
                "id": "s-evt", "subject": "Shared", "start": { "dateTime": format!("{}T09:00:00", today()), "timeZone": "UTC" },
                "end": { "dateTime": format!("{}T10:00:00", today()), "timeZone": "UTC" }
            }] }),
        ),
    );
    t.on(Method::Get, "/groups/g-1/calendarView", FixtureResponse::json(200, &json!({ "value": [] })));
    t.on(Method::Get, "/users/bo@example.com/calendars/p1/calendarView", FixtureResponse::status(403));
    let c = ctx(&t);
    let d = today();
    seed(
        &c,
        vec![
            cal("own", CalendarSource::Own, true, "me@example.com"),
            cal("shared1", CalendarSource::Shared, false, "a@example.com"),
            cal("group:g-1", CalendarSource::Group, false, "x@example.com"),
            cal("p1", CalendarSource::OpenedUser, true, "bo@example.com"),
        ],
        vec![
            ("shared1", ev("old-shared", &format!("{d}T11:00:00"), &format!("{d}T12:00:00"))),
            ("shared1", ev("local-cal-1", &format!("{d}T13:00:00"), &format!("{d}T14:00:00"))),
        ],
    )
    .await;
    assert_eq!(OtherCalendars::default().run(&c).await.unwrap(), Outcome::CaughtUp);
    let paths: Vec<String> = t.calls().iter().map(|r| r.url.path().to_owned()).collect();
    assert!(paths.iter().all(|p| !p.contains("/calendars/own/")), "default calendar is delta-tracked: {paths:?}");
    assert!(paths.iter().any(|p| p.ends_with("/groups/g-1/calendarView")));
    let get = |id: &'static str| {
        let s = c.store.clone();
        async move { s.read(move |r| r.event(&EventId::new(id).unwrap())).await.unwrap() }
    };
    assert!(get("s-evt").await.is_some());
    assert!(get("old-shared").await.is_none(), "replaced");
    assert!(get("local-cal-1").await.is_some(), "pending events survive a poll");
}

#[tokio::test]
async fn opened_people_get_details_or_free_busy() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/users/bo@example.com/calendars", file("calendar/calendars.json"));
    t.on(Method::Get, "/users/cy@example.com/calendars", FixtureResponse::status(403));
    let c = ctx(&t);
    let people = vec![
        OpenedPerson { address: "bo@example.com".into(), name: String::new(), mode: PersonMode::Pending },
        OpenedPerson { address: "cy@example.com".into(), name: "Cy".into(), mode: PersonMode::Pending },
    ];
    c.store.set_setting(PEOPLE_KEY.to_owned(), people).await.unwrap();
    People.run(&c).await.unwrap();
    let people: Vec<OpenedPerson> = c.store.setting(PEOPLE_KEY.to_owned()).await.unwrap().unwrap();
    assert_eq!(people[0].mode, PersonMode::Details);
    assert_eq!(people[1].mode, PersonMode::FreeBusy);
    let cals = c.store.calendars().await.unwrap().items;
    assert!(cals.iter().all(|c| c.source == CalendarSource::OpenedUser));
    let cx = query::load(&c.store).await.unwrap();
    let sources = cx.sources();
    assert!(sources.iter().any(|s| s.id == "person:cy@example.com" && s.free_busy_only));
    assert!(!sources.iter().any(|s| s.id == "person:bo@example.com"));
}

#[tokio::test]
async fn merged_range_query_spans_layers_with_colours() {
    let t = FixtureTransport::new();
    let c = ctx(&t);
    let mut shared = cal("shared1", CalendarSource::Shared, false, "a@example.com");
    shared.hex_color = Some("#87D28E".into());
    seed(
        &c,
        vec![
            cal("own", CalendarSource::Own, true, "me@example.com"),
            shared,
            cal("hidden", CalendarSource::Own, false, "me@example.com"),
        ],
        vec![
            ("own", ev("a", "2026-10-20T09:00:00", "2026-10-20T10:00:00")),
            ("shared1", ev("b", "2026-10-20T09:30:00", "2026-10-20T11:00:00")),
            ("hidden", ev("c", "2026-10-21T09:00:00", "2026-10-21T10:00:00")),
            ("own", ev("outside", "2026-11-20T09:00:00", "2026-11-20T10:00:00")),
            ("own", ev("multi", "2026-10-18T22:00:00", "2026-10-22T08:00:00")),
        ],
    )
    .await;
    let mut prefs = SourcePrefs::default();
    prefs.hidden.insert("hidden".into());
    c.store.set_setting(SOURCES_KEY.to_owned(), prefs).await.unwrap();
    c.store.set_setting(TIME_ZONE_KEY.to_owned(), "UTC".to_owned()).await.unwrap();
    let cx = query::load(&c.store).await.unwrap();
    let all = cx.sources();
    let visible = query::pick_sources(&all, None);
    let first = date(2026, 10, 19);
    let events = query::merged_events(&c.store, &cx, &visible, first, 7).await.unwrap();
    let ids: Vec<&str> = events.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["multi", "a", "b"]);
    let b = events.iter().find(|e| e.id == "b").unwrap();
    assert_eq!(b.source, "shared1");
    assert_eq!(b.color, "#87d28e");
    assert_eq!((b.start_day, b.start_min, b.end_min), (1, 570, 660));
    let multi = events.iter().find(|e| e.id == "multi").unwrap();
    assert!(multi.all_day, "longer than a day goes in the all-day row");
    assert_eq!((multi.start_day, multi.end_day), (-1, 3));
    // Asking for a hidden layer by id includes it.
    let only = vec!["hidden".to_owned()];
    let picked = query::pick_sources(&all, Some(&only));
    let hidden = query::merged_events(&c.store, &cx, &picked, first, 7).await.unwrap();
    assert_eq!(hidden.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["c"]);
}

fn target(id: &str) -> Target {
    Target { id: id.into(), mailbox: None }
}

#[tokio::test]
async fn outbox_requests_have_the_documented_shapes() {
    let t = FixtureTransport::new();
    let created = json!({
        "id": "real-1", "subject": "Planning",
        "start": { "dateTime": "2026-10-20T09:00:00", "timeZone": "UTC" },
        "end": { "dateTime": "2026-10-20T09:30:00", "timeZone": "UTC" }
    });
    t.on(Method::Post, "/me/events", FixtureResponse::json(201, &created));
    t.on(Method::Patch, "/me/events/real-1", FixtureResponse::json(200, &created));
    t.on(Method::Post, "/me/events/real-1/tentativelyAccept", FixtureResponse::status(202));
    t.on(Method::Post, "/me/events/real-1/decline", FixtureResponse::status(202));
    t.on(Method::Post, "/me/events/real-1/accept", FixtureResponse::status(202));
    t.on(Method::Post, "/me/events/real-1/cancel", FixtureResponse::status(202));
    t.on(Method::Post, "/me/events/real-1/forward", FixtureResponse::status(202));
    t.on(Method::Post, "/me/events/real-1/snoozeReminder", FixtureResponse::status(200));
    t.on(Method::Post, "/me/events/real-1/dismissReminder", FixtureResponse::status(200));
    t.on(Method::Delete, "/me/events/real-1", FixtureResponse::status(404));
    t.on(Method::Post, "/users/bo@example.com/events/real-1/accept", FixtureResponse::status(202));
    let c = ctx(&t);
    seed(&c, vec![cal("own", CalendarSource::Own, true, "me@example.com")], vec![]).await;
    let h = |kind: &str| CalendarModule::new().handlers().into_iter().find(|h| h.kind() == kind).unwrap();

    // Create: transactionId = local id; the local event is replaced by the server's.
    let fields = EventFields {
        subject: Some(Sensitive::new("Planning")),
        start: Some(DateTimeZone::from_timestamp_utc("2026-10-20T09:00:00Z".parse().unwrap())),
        end: Some(DateTimeZone::from_timestamp_utc("2026-10-20T09:30:00Z".parse().unwrap())),
        teams: Some(true),
        ..Default::default()
    };
    let local = EventId::new("local-cal-7").unwrap();
    let opt = ob::optimistic_event(&local, &CalendarId::new("own").unwrap(), &fields, None).unwrap();
    seed(&c, vec![], vec![("own", opt)]).await;
    let payload = CreatePayload { local_id: "local-cal-7".into(), calendar: None, mailbox: None, fields };
    let out = h(ob::KIND_CREATE).apply(&c, serde_json::to_value(&payload).unwrap()).await.unwrap();
    assert_eq!(out["id"], "real-1");
    let call = t.calls().pop().unwrap();
    let b = body(&call);
    assert_eq!(b["transactionId"], "local-cal-7");
    assert_eq!(b["isOnlineMeeting"], true);
    assert_eq!(b["onlineMeetingProvider"], "teamsForBusiness");
    assert!(c.store.read(|r| r.event(&EventId::new("local-cal-7").unwrap())).await.unwrap().is_none());
    assert!(c.store.read(|r| r.event(&EventId::new("real-1").unwrap())).await.unwrap().is_some());
    let mapped: Option<String> = c.store.setting(local_map_key("local-cal-7")).await.unwrap();
    assert_eq!(mapped.as_deref(), Some("real-1"));

    // Update by local id resolves to the server id.
    let up = UpdatePayload {
        target: target("local-cal-7"),
        fields: EventFields { subject: Some(Sensitive::new("Renamed")), ..Default::default() },
        original: None,
    };
    h(ob::KIND_UPDATE).apply(&c, serde_json::to_value(&up).unwrap()).await.unwrap();
    let call = t.calls().pop().unwrap();
    assert_eq!(call.method, Method::Patch);
    assert_eq!(body(&call), json!({ "subject": "Renamed" }));

    // RSVP: tentative with a message and a proposed time.
    let slot = TimeSlot {
        start: DateTimeZone::from_timestamp_utc("2026-10-21T10:00:00Z".parse().unwrap()),
        end: DateTimeZone::from_timestamp_utc("2026-10-21T10:30:00Z".parse().unwrap()),
    };
    let rsvp = RespondPayload {
        target: target("real-1"),
        response: Response::Tentative,
        comment: Some(Sensitive::new("Can we move it?")),
        propose: Some(slot),
        send: true,
        original: None,
    };
    h(ob::KIND_RESPOND).apply(&c, serde_json::to_value(&rsvp).unwrap()).await.unwrap();
    let call = t.calls().pop().unwrap();
    assert!(call.url.path().ends_with("/tentativelyAccept"));
    let b = body(&call);
    assert_eq!(b["sendResponse"], true);
    assert_eq!(b["comment"], "Can we move it?");
    assert_eq!(b["proposedNewTime"]["start"]["dateTime"].as_str().map(|s| &s[..16]), Some("2026-10-21T10:00"));
    // Accept in someone else's calendar goes to their mailbox.
    let acc = RespondPayload {
        target: Target { id: "real-1".into(), mailbox: Some("bo@example.com".into()) },
        response: Response::Accept,
        comment: None,
        propose: None,
        send: true,
        original: None,
    };
    h(ob::KIND_RESPOND).apply(&c, serde_json::to_value(&acc).unwrap()).await.unwrap();
    assert!(t.calls().pop().unwrap().url.path().ends_with("/users/bo@example.com/events/real-1/accept"));
    // Decline removes the event locally (Graph removes it too).
    let dec = RespondPayload { response: Response::Decline, propose: None, comment: None, ..rsvp };
    h(ob::KIND_RESPOND).apply(&c, serde_json::to_value(&dec).unwrap()).await.unwrap();
    assert!(t.calls().pop().unwrap().url.path().ends_with("/decline"));
    assert!(c.store.read(|r| r.event(&EventId::new("real-1").unwrap())).await.unwrap().is_none());

    // Cancel with a message; forward; snooze; dismiss; delete (404 = already gone).
    let cancel = RemovePayload { target: target("real-1"), comment: Some(Sensitive::new("Sorry")), original: None };
    h(ob::KIND_CANCEL).apply(&c, serde_json::to_value(&cancel).unwrap()).await.unwrap();
    assert_eq!(body(&t.calls().pop().unwrap()), json!({ "comment": "Sorry" }));
    let fwd = ForwardPayload {
        target: target("real-1"),
        to: vec![EmailAddress::new("", "c@example.com").unwrap()],
        comment: None,
    };
    h(ob::KIND_FORWARD).apply(&c, serde_json::to_value(&fwd).unwrap()).await.unwrap();
    assert_eq!(body(&t.calls().pop().unwrap())["toRecipients"][0]["emailAddress"]["address"], "c@example.com");
    let snooze = ReminderPayload { target: target("real-1"), until: Some("2026-10-20T08:55:00Z".parse().unwrap()) };
    h(ob::KIND_SNOOZE).apply(&c, serde_json::to_value(&snooze).unwrap()).await.unwrap();
    assert!(body(&t.calls().pop().unwrap())["newReminderTime"]["dateTime"].is_string());
    let dismiss = ReminderPayload { target: target("real-1"), until: None };
    h(ob::KIND_DISMISS).apply(&c, serde_json::to_value(&dismiss).unwrap()).await.unwrap();
    let del = RemovePayload { target: target("real-1"), comment: None, original: None };
    h(ob::KIND_DELETE).apply(&c, serde_json::to_value(&del).unwrap()).await.unwrap();
    assert_eq!(t.calls().pop().unwrap().method, Method::Delete);

    // An unresolvable local id and a garbage payload fail permanently, never panic.
    let orphan = RemovePayload { target: target("local-never"), comment: None, original: None };
    let e = h(ob::KIND_DELETE).apply(&c, serde_json::to_value(&orphan).unwrap()).await.unwrap_err();
    assert_eq!(e.kind(), quorum_sync::ErrorKind::Permanent);
    for handler in CalendarModule::new().handlers() {
        for junk in [Value::Null, json!(1), json!({"target": {"id": ""}}), json!({"id": "x".repeat(5000)})] {
            assert!(handler.apply(&c, junk.clone()).await.is_err());
            let _ = handler.revert(&c, junk).await;
        }
    }
}

#[tokio::test]
async fn reverts_restore_the_optimistic_change() {
    let t = FixtureTransport::new();
    let c = ctx(&t);
    let original = ev("e1", "2026-10-20T09:00:00", "2026-10-20T10:00:00");
    let mut with_cal = original.clone();
    with_cal.calendar_id = Some(CalendarId::new("own").unwrap());
    seed(&c, vec![cal("own", CalendarSource::Own, true, "me@example.com")], vec![]).await;
    let h = |kind: &str| CalendarModule::new().handlers().into_iter().find(|h| h.kind() == kind).unwrap();
    // Delete reverted: the event comes back.
    let del = RemovePayload { target: target("e1"), comment: None, original: Some(with_cal.clone()) };
    h(ob::KIND_DELETE).revert(&c, serde_json::to_value(&del).unwrap()).await.unwrap();
    assert!(c.store.read(|r| r.event(&EventId::new("e1").unwrap())).await.unwrap().is_some());
    // RSVP reverted: the old response is back.
    let mut accepted = with_cal.clone();
    accepted.response_status.response = ResponseType::Accepted;
    seed(&c, vec![], vec![("own", accepted)]).await;
    let rsvp = RespondPayload {
        target: target("e1"),
        response: Response::Accept,
        comment: None,
        propose: None,
        send: true,
        original: Some(original.response_status.clone()),
    };
    h(ob::KIND_RESPOND).revert(&c, serde_json::to_value(&rsvp).unwrap()).await.unwrap();
    let e = c.store.read(|r| r.event(&EventId::new("e1").unwrap())).await.unwrap().unwrap();
    assert_eq!(e.response_status.response, ResponseType::None);
    // Create reverted: the local event goes.
    seed(&c, vec![], vec![("own", ev("local-x", "2026-10-20T09:00:00", "2026-10-20T10:00:00"))]).await;
    let create =
        CreatePayload { local_id: "local-x".into(), calendar: None, mailbox: None, fields: EventFields::default() };
    h(ob::KIND_CREATE).revert(&c, serde_json::to_value(&create).unwrap()).await.unwrap();
    assert!(c.store.read(|r| r.event(&EventId::new("local-x").unwrap())).await.unwrap().is_none());
}
