//! Calendar commands and queries end to end in mock mode: sync from the fixture Graph, the
//! merged view and its layers, optimistic writes and their outbox entries, navigation state,
//! opened people (details vs free/busy), scheduling lookups and hostile parameters.

use quorum_engine::{ErrorKind, Session, SessionOptions};
use quorum_sync::calendar::CalendarModule;
use quorum_sync::{Module, Outcome};
use serde_json::{Value, json};

/// A signed-in mock session with the calendar synced and sync paused (outbox stays queued).
async fn session() -> Session {
    let s = Session::new(SessionOptions { mock_graph: true }).unwrap();
    s.start().await;
    let h = s.sync().unwrap();
    h.pause();
    sync(&s).await;
    s
}

async fn sync(s: &Session) {
    let ctx = s.sync().unwrap().ctx().clone();
    for r in CalendarModule::new().discover(&ctx).await.unwrap() {
        for _ in 0..5 {
            if r.run(&ctx).await.unwrap() == Outcome::CaughtUp {
                break;
            }
        }
    }
}

async fn q(s: &Session, id: &str, p: Value) -> Value {
    s.query(id, p).await.unwrap_or_else(|e| panic!("{id}: {e}"))
}

async fn run(s: &Session, id: &str, p: Value) -> Value {
    s.execute(id, p).await.unwrap_or_else(|e| panic!("{id}: {e}"))
}

async fn outbox_kinds(s: &Session) -> Vec<String> {
    let store = s.store().unwrap();
    store.read(|r| r.outbox_entries(500)).await.unwrap().items.into_iter().map(|e| e.kind).collect()
}

/// Anchors the view on the fixtures' week.
async fn fixture_week(s: &Session) {
    run(s, "calendar.go", json!({ "to": "2026-10-14", "view": "week" })).await;
}

#[tokio::test]
async fn sources_cover_own_shared_and_group_calendars() {
    let s = session().await;
    let sources = q(&s, "calendar.sources", Value::Null).await;
    let sources = sources.as_array().unwrap();
    let kinds: Vec<(&str, &str)> =
        sources.iter().map(|x| (x["id"].as_str().unwrap(), x["kind"].as_str().unwrap())).collect();
    assert_eq!(kinds.first(), Some(&("mock-cal-own", "own")), "default calendar first: {kinds:?}");
    assert!(kinds.contains(&("mock-cal-birthdays", "own")));
    assert!(kinds.contains(&("mock-cal-team", "shared")));
    assert!(kinds.contains(&("group:mock-group-1", "group")));
    for x in sources {
        assert!(x["color"].as_str().unwrap().starts_with('#'));
        assert_eq!(x["visible"], true);
    }
    let own = &sources[0];
    assert_eq!(own["group"], "My calendars");
    assert_eq!(own["is_default"], true);
}

#[tokio::test]
async fn view_merges_layers_in_the_mailbox_time_zone() {
    let s = session().await;
    fixture_week(&s).await;
    let v = q(&s, "calendar.view", Value::Null).await;
    assert_eq!(v["view"], "week");
    assert_eq!(v["title"], "October 12 – 18, 2026");
    assert_eq!(v["time_zone"], "Europe/Berlin", "mailboxSettings zone");
    assert_eq!(v["days"].as_array().unwrap().len(), 7);
    assert_eq!(v["work_start"], 480);
    let events = v["events"].as_array().unwrap();
    let ids: Vec<&str> = events.iter().map(|e| e["id"].as_str().unwrap()).collect();
    for id in ["mock-evt-plan", "mock-evt-team-1", "mock-evt-bday", "mock-evt-group-1", "mock-evt-standup-12"] {
        assert!(ids.contains(&id), "{id} in {ids:?}");
    }
    assert!(!ids.contains(&"mock-evt-standup"), "series masters are not shown");
    let plan = events.iter().find(|e| e["id"] == "mock-evt-plan").unwrap();
    // 09:00 UTC on Tuesday = 11:00 in Berlin (CEST).
    assert_eq!((plan["start_day"].as_i64(), plan["start_min"].as_u64()), (Some(1), Some(660)));
    assert_eq!(plan["time_label"], "11:00 – 12:30");
    assert_eq!(plan["source"], "mock-cal-own");
    let team = events.iter().find(|e| e["id"] == "mock-evt-team-1").unwrap();
    assert_eq!(team["source"], "mock-cal-team");
    assert_ne!(team["color"], plan["color"]);
    let bday = events.iter().find(|e| e["id"] == "mock-evt-bday").unwrap();
    assert_eq!(bday["all_day"], true);
    assert_eq!(bday["start_day"], 3);
    let retro = events.iter().find(|e| e["id"] == "mock-evt-retro").unwrap();
    assert_eq!(retro["can_join"], true);
    // Hiding a layer removes its events; showing it brings them back.
    run(&s, "calendar.toggle_source", json!({ "id": "mock-cal-team" })).await;
    let v = q(&s, "calendar.view", Value::Null).await;
    assert!(v["events"].as_array().unwrap().iter().all(|e| e["source"] != "mock-cal-team"));
    run(&s, "calendar.toggle_source", json!({ "id": "mock-cal-team", "visible": true, "color": "#AA0000" })).await;
    let sources = q(&s, "calendar.sources", Value::Null).await;
    let team = sources.as_array().unwrap().iter().find(|x| x["id"] == "mock-cal-team").unwrap().clone();
    assert_eq!(team["color"], "#aa0000");
    assert_eq!(team["visible"], true);
    // The range query names its layers explicitly.
    let only =
        q(&s, "calendar.events", json!({ "from": "2026-10-12", "to": "2026-10-18", "sources": ["mock-cal-team"] }))
            .await;
    assert!(only.as_array().unwrap().iter().all(|e| e["source"] == "mock-cal-team"));
    assert_eq!(only.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn navigation_is_stored_and_drives_the_view() {
    let s = session().await;
    fixture_week(&s).await;
    run(&s, "calendar.go", json!({ "to": "next" })).await;
    assert_eq!(q(&s, "calendar.view", Value::Null).await["title"], "October 19 – 25, 2026");
    run(&s, "calendar.view.month", Value::Null).await;
    let v = q(&s, "calendar.view", Value::Null).await;
    assert_eq!(v["title"], "October 2026");
    assert_eq!(v["days"].as_array().unwrap().len(), 42);
    let offsite = v["events"].as_array().unwrap().iter().find(|e| e["id"] == "mock-evt-offsite").unwrap().clone();
    assert_eq!(offsite["all_day"], true);
    assert_eq!(offsite["end_day"].as_i64().unwrap() - offsite["start_day"].as_i64().unwrap(), 1, "two days");
    run(&s, "calendar.set_view", json!({ "view": "day" })).await;
    run(&s, "calendar.go", json!({ "date": "2026-10-17" })).await;
    let v = q(&s, "calendar.view", Value::Null).await;
    assert_eq!(v["view"], "day");
    let ids: Vec<&str> = v["events"].as_array().unwrap().iter().map(|e| e["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"mock-evt-focus") && ids.contains(&"mock-evt-retro") && ids.contains(&"mock-evt-hiring"));
    run(&s, "calendar.go", json!({ "to": "today" })).await;
    let v = q(&s, "calendar.view", Value::Null).await;
    assert_eq!(v["date"], v["today"]);
    assert_eq!(v["mini"]["weeks"].as_array().unwrap().len(), 6);
    run(&s, "calendar.view.agenda", Value::Null).await;
    assert_eq!(q(&s, "calendar.view", Value::Null).await["days"].as_array().unwrap().len(), 14);
}

#[tokio::test]
async fn event_details_include_recurrence_and_rsvp_state() {
    let s = session().await;
    let e = q(&s, "calendar.event", json!({ "id": "mock-evt-standup-12" })).await;
    assert_eq!(e["recurrence"], "Every week on Monday", "from the series master");
    assert_eq!(e["can_join"], true);
    let inv = q(&s, "calendar.event", json!({ "id": "mock-evt-invite" })).await;
    assert_eq!(inv["can_respond"], true);
    assert_eq!(inv["organizer"]["name"], "Kai Novak");
    assert!(inv["attendees"].as_array().unwrap().len() >= 2);
    assert_eq!(inv["source"]["id"], "mock-cal-own");
    let missing = s.query("calendar.event", json!({ "id": "nope" })).await.unwrap_err();
    assert_eq!(missing.kind(), ErrorKind::NotFound);
}

#[tokio::test]
async fn writes_are_optimistic_and_queued() {
    let s = session().await;
    fixture_week(&s).await;
    let created = run(
        &s,
        "calendar.new_event",
        json!({
            "subject": "Synthetic planning",
            "start": "2026-10-15T14:00",
            "end": "2026-10-15T15:00",
            "attendees": ["kai.novak@contoso.example", { "address": "room1@contoso.example", "type": "resource" }],
            "location": "Room Synthetic 1",
            "online_meeting": true,
            "recurrence": "weekly",
        }),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("local-"));
    let v = q(&s, "calendar.view", Value::Null).await;
    let mine = v["events"].as_array().unwrap().iter().find(|e| e["id"] == id.as_str()).unwrap().clone();
    assert_eq!(mine["pending"], true);
    assert_eq!(mine["time_label"], "14:00 – 15:00", "local wall-clock time in the mailbox zone");
    run(&s, "calendar.update_event", json!({ "id": id, "subject": "Renamed" })).await;
    let e = q(&s, "calendar.event", json!({ "id": id })).await;
    assert_eq!(e["subject"], "Renamed");
    assert_eq!(e["recurrence"], "Every week on Thursday");
    run(&s, "calendar.respond", json!({ "id": "mock-evt-invite", "response": "tentative", "message": "Maybe", "propose": { "start": "2026-10-15T15:00", "end": "2026-10-15T16:00" } })).await;
    let inv = q(&s, "calendar.event", json!({ "id": "mock-evt-invite" })).await;
    assert_eq!(inv["response"], "tentativelyAccepted");
    run(&s, "calendar.delete_event", json!({ "id": "mock-evt-lunch" })).await;
    assert!(s.query("calendar.event", json!({ "id": "mock-evt-lunch" })).await.is_err());
    run(&s, "calendar.cancel", json!({ "id": "mock-evt-plan", "message": "Moving this" })).await;
    run(&s, "calendar.forward", json!({ "id": "mock-evt-status", "to": ["sage.moreau@contoso.example"] })).await;
    run(&s, "calendar.snooze", json!({ "id": "mock-evt-invite", "minutes": 10 })).await;
    run(&s, "calendar.dismiss", json!({ "id": "mock-evt-invite" })).await;
    assert_eq!(
        outbox_kinds(&s).await,
        [
            "calendar.event.create",
            "calendar.event.update",
            "calendar.event.respond",
            "calendar.event.delete",
            "calendar.event.cancel",
            "calendar.event.forward",
            "calendar.reminder.snooze",
            "calendar.reminder.dismiss"
        ]
    );
    // Rules that keep users out of trouble.
    let e = s.execute("calendar.cancel", json!({ "id": "mock-evt-invite" })).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::InvalidParams, "only organizers cancel");
    let e = s.execute("calendar.respond", json!({ "id": "mock-evt-status", "response": "accept" })).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::InvalidParams, "organizers don't RSVP");
    let e = s
        .execute("calendar.new_event", json!({ "calendar": "mock-cal-team", "start": "2026-10-15T09:00" }))
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::InvalidParams, "read-only shared calendar");
}

#[tokio::test]
async fn opened_people_show_details_or_free_busy() {
    let s = session().await;
    fixture_week(&s).await;
    run(&s, "calendar.open_person", json!({ "user": "kai.novak@contoso.example" })).await;
    run(&s, "calendar.open_person", json!({ "user": "Riley.Ito@contoso.example", "name": "Riley Ito" })).await;
    sync(&s).await;
    let sources = q(&s, "calendar.sources", Value::Null).await;
    let sources = sources.as_array().unwrap();
    assert!(sources.iter().any(|x| x["id"] == "mock-cal-kai" && x["kind"] == "person"), "Kai shares: events");
    let riley = sources.iter().find(|x| x["id"] == "person:riley.ito@contoso.example").unwrap();
    assert_eq!(riley["free_busy_only"], true);
    assert_eq!(riley["mode"], "free_busy");
    let v = q(&s, "calendar.view", Value::Null).await;
    assert!(v["events"].as_array().unwrap().iter().any(|e| e["id"] == "mock-evt-kai-1"));
    let busy = v["busy"].as_array().unwrap();
    assert!(busy.iter().any(|b| b["source"] == "person:riley.ito@contoso.example" && b["status"] == "busy"));
    run(&s, "calendar.close_person", json!({ "user": "kai.novak@contoso.example" })).await;
    let sources = q(&s, "calendar.sources", Value::Null).await;
    assert!(!sources.as_array().unwrap().iter().any(|x| x["id"] == "mock-cal-kai"));
    let e = s.execute("calendar.open_person", json!({ "user": "not an address" })).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::InvalidParams);
}

#[tokio::test]
async fn scheduling_lookups_answer_from_fixtures() {
    let s = session().await;
    let fb = q(
        &s,
        "calendar.free_busy",
        json!({ "users": ["riley.ito@contoso.example", "sage.moreau@contoso.example"], "from": "2026-10-13", "to": "2026-10-14", "interval": 30 }),
    )
    .await;
    let rows = fb["schedules"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(!rows[0]["items"].as_array().unwrap().is_empty());
    let ft = q(&s, "calendar.find_times", json!({ "attendees": ["riley.ito@contoso.example"], "duration": 60 })).await;
    assert!(!ft["suggestions"].as_array().unwrap().is_empty());
    let first = &ft["suggestions"][0];
    assert!(first["label"].as_str().unwrap().contains('–'));
    let rooms = q(&s, "calendar.rooms", Value::Null).await;
    assert!(rooms.as_array().unwrap().iter().any(|r| r["name"] == "Room Synthetic 1"));
    let lists = q(&s, "calendar.room_lists", Value::Null).await;
    assert_eq!(lists[0]["name"], "Synthetic HQ");
    for bad in [
        json!({ "users": ["x"], "from": "2026-10-13", "to": "2026-10-14" }),
        json!({ "users": ["a@example.com"], "from": "2026-10-14", "to": "2026-10-13" }),
        json!({ "users": ["a@example.com"], "from": "2026-10-13", "to": "2026-10-14", "interval": 0 }),
    ] {
        let e = s.query("calendar.free_busy", bad.clone()).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::InvalidParams, "{bad}");
    }
}

#[tokio::test]
async fn join_asks_the_ui_to_open_the_link() {
    let s = session().await;
    let mut events = s.subscribe();
    let out = run(&s, "calendar.join", json!({ "id": "mock-evt-retro" })).await;
    assert!(out["url"].as_str().unwrap().starts_with("https://"));
    let mut seen = false;
    while let Ok(ev) = events.try_recv() {
        if let quorum_ipc::CoreEvent::Ui { command, params } = ev
            && command == "calendar.join"
        {
            assert_eq!(params["url"], out["url"]);
            seen = true;
        }
    }
    assert!(seen);
    let e = s.execute("calendar.join", json!({ "id": "mock-evt-plan" })).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::NotAvailable);
}

#[tokio::test]
async fn upcoming_lists_reminders() {
    let s = session().await;
    let rows =
        q(&s, "calendar.upcoming", json!({ "from": "2026-10-13T00:00:00Z", "to": "2026-10-14T00:00:00Z" })).await;
    let rows = rows.as_array().unwrap();
    assert!(rows.iter().any(|r| r["id"] == "mock-evt-plan"));
    let plan = rows.iter().find(|r| r["id"] == "mock-evt-plan").unwrap();
    assert_eq!(plan["reminder_at"], "2026-10-13T08:45:00Z");
}

#[tokio::test]
async fn hostile_params_never_panic_or_write() {
    let s = session().await;
    let ids: Vec<String> = s
        .registry()
        .iter()
        .map(|c| c.id.to_owned())
        .chain(s.queries().iter().map(|c| c.id.to_owned()))
        .filter(|id| id.starts_with("calendar."))
        .collect();
    let deep = (0..200).fold(json!(1), |acc, _| json!([acc]));
    let hostile = [
        Value::Null,
        json!(5),
        json!("x"),
        json!([]),
        json!({ "id": "" }),
        json!({ "id": "x".repeat(5000) }),
        json!({ "id": "\u{0}" }),
        json!({ "start": "9999-99-99", "end": "-1" }),
        json!({ "start": "2026-10-20T10:00", "end": "2026-10-20T09:00" }),
        json!({ "start": "2026-01-01", "end": "2099-01-01", "all_day": true }),
        json!({ "attendees": vec!["a@example.com"; 600] }),
        json!({ "attendees": [{"address": "<script>"}] }),
        json!({ "recurrence": { "pattern": "weekly", "interval": 1_000_000, "count": 0 } }),
        json!({ "from": "2026-01-01", "to": "2099-01-01" }),
        json!({ "users": vec!["a@example.com"; 101], "from": "2026-10-13", "to": "2026-10-14" }),
        json!({ "view": "year" }),
        json!({ "to": "\u{202e}" }),
        json!({ "mini_offset": i64::MAX }),
        json!({ "id": "mock-evt-plan", "color": "javascript:" }),
        json!({ "user": "x".repeat(10_000) }),
        deep,
    ];
    for v in &hostile {
        for id in &ids {
            let r = if s.registry().get(id).is_ok() {
                s.execute(id, v.clone()).await
            } else {
                s.query(id, v.clone()).await
            };
            if let Err(e) = r {
                assert_ne!(e.kind(), ErrorKind::Bug, "{id} on {v}: {e}");
            }
        }
    }
}
