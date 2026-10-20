//! People and presence sync: watch TTL, batched presence, preferred-presence and status
//! message request shapes, org chart, photos, hostile payloads.

use std::time::{Duration, Instant};

use quorum_graph::{
    Client, FakeClock, FixtureResponse, FixtureTransport, Method, RateLimit, ResourceFamily, StaticToken,
};
use quorum_model::{Activity, Availability, UserId};
use quorum_store::Store;
use quorum_sync::people::{
    self as sp, ClearPreferredHandler, MyPresence, PresenceChoice, PresenceWatch, SetPreferredHandler,
    StatusMessageHandler, WatchedPresence,
};
use quorum_sync::{Ctx, ErrorKind, OutboxHandler, Resource};
use serde_json::{Value, json};

fn ctx(t: &FixtureTransport) -> Ctx {
    let mut b = Client::builder(StaticToken::new("test-token"));
    for f in ResourceFamily::ALL {
        b = b.rate(f, RateLimit::new(10_000.0, 10_000).unwrap());
    }
    let graph = b.transport(t.clone()).clock(FakeClock::new()).jitter_seed(1).build().unwrap();
    Ctx { graph, store: Store::open_in_memory().unwrap(), client_id: "00000000-0000-0000-0000-0000000000c1".into() }
}

fn uid(n: usize) -> UserId {
    UserId::new(format!("00000000-0000-0000-0000-{n:012}")).unwrap()
}

fn body(req: &quorum_graph::HttpRequest) -> Value {
    serde_json::from_slice(&req.body).unwrap_or(Value::Null)
}

#[test]
fn watch_ttl_expires_and_refreshes() {
    let w = PresenceWatch::new();
    let t0 = Instant::now();
    assert_eq!(w.watch_at(&[uid(1), uid(2), uid(1)], Duration::from_secs(10), t0), 2);
    assert_eq!(w.active_at(t0 + Duration::from_secs(5)).len(), 2);
    // Re-watching uid(1) later extends only it.
    assert_eq!(w.watch_at(&[uid(1)], Duration::from_secs(10), t0 + Duration::from_secs(8)), 0);
    let later = w.active_at(t0 + Duration::from_secs(12));
    assert_eq!(later, vec![uid(1)]);
    assert!(w.active_at(t0 + Duration::from_secs(30)).is_empty());
    assert!(w.is_empty());
}

#[test]
fn watch_is_bounded() {
    let w = PresenceWatch::new();
    let t0 = Instant::now();
    let ids: Vec<UserId> = (0..sp::MAX_WATCHED + 50).map(uid).collect();
    w.watch_at(&ids, Duration::from_secs(60), t0);
    assert_eq!(w.len(), sp::MAX_WATCHED);
    // Clamped TTL: zero becomes the minimum.
    let w2 = PresenceWatch::new();
    w2.watch(&[uid(1)], Duration::ZERO);
    assert_eq!(w2.active().len(), 1);
}

#[tokio::test]
async fn watched_presence_batches_visible_ids() {
    let t = FixtureTransport::new();
    t.on(
        Method::Post,
        "/communications/getPresencesByUserId",
        FixtureResponse::json(
            200,
            &json!({ "value": [
                { "id": uid(1).as_str(), "availability": "Busy", "activity": "InAMeeting" },
                { "id": uid(2).as_str(), "availability": "Weird", "activity": 5 },
                "garbage"
            ]}),
        ),
    );
    let c = ctx(&t);
    let w = PresenceWatch::new();
    let r = WatchedPresence::new(w.clone());
    assert_eq!(r.key(), "presence.watch");
    assert_eq!(r.cadence().focused, Duration::from_secs(30));
    assert_eq!(r.cadence().background, Duration::from_secs(120));

    // Nothing watched: no request.
    r.run(&c).await.unwrap();
    assert_eq!(t.call_count(), 0);

    w.watch(&[uid(2), uid(1), uid(1)], sp::DEFAULT_WATCH_TTL);
    r.run(&c).await.unwrap();
    let calls = t.calls();
    assert_eq!(calls.len(), 1);
    let ids = body(&calls[0])["ids"].as_array().cloned().unwrap();
    assert_eq!(ids.len(), 2);
    let stored = c.store.presences(vec![uid(1), uid(2)]).await.unwrap().items;
    assert_eq!(stored.len(), 2);
    assert!(stored.iter().any(|p| p.presence.availability == Availability::Busy));
}

#[tokio::test]
async fn more_than_650_ids_go_through_batch() {
    let t = FixtureTransport::new();
    let item = |id: &str| json!({ "id": id, "status": 200, "body": { "value": [] } });
    t.on(Method::Post, "/$batch", FixtureResponse::json(200, &json!({ "responses": [item("p0"), item("p1")] })));
    let c = ctx(&t);
    let w = PresenceWatch::new();
    let ids: Vec<UserId> = (0..700).map(uid).collect();
    w.watch(&ids, sp::DEFAULT_WATCH_TTL);
    WatchedPresence::new(w).run(&c).await.unwrap();
    let calls = t.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].url.path().ends_with("/$batch"));
    let reqs = body(&calls[0])["requests"].as_array().cloned().unwrap();
    assert_eq!(reqs.len(), 2);
    let n: usize = reqs.iter().map(|r| r["body"]["ids"].as_array().map_or(0, Vec::len)).sum();
    assert_eq!(n, 700);
}

#[test]
fn choices_map_to_preferred_pairs() {
    let cases = [
        ("available", Availability::Available, Activity::Available),
        ("busy", Availability::Busy, Activity::Busy),
        ("dnd", Availability::DoNotDisturb, Activity::DoNotDisturb),
        ("do_not_disturb", Availability::DoNotDisturb, Activity::DoNotDisturb),
        ("DoNotDisturb", Availability::DoNotDisturb, Activity::DoNotDisturb),
        ("brb", Availability::BeRightBack, Activity::BeRightBack),
        ("be_right_back", Availability::BeRightBack, Activity::BeRightBack),
        ("away", Availability::Away, Activity::Away),
        ("offline", Availability::Offline, Activity::OffWork),
        ("appear_offline", Availability::Offline, Activity::OffWork),
    ];
    for (s, a, b) in cases {
        assert_eq!(PresenceChoice::parse(s), Some(PresenceChoice::Set { availability: a, activity: b }), "{s}");
    }
    assert_eq!(PresenceChoice::parse("reset"), Some(PresenceChoice::Reset));
    for bad in ["", "online", "x".repeat(1000).as_str(), "busy\u{0}", "InAMeeting"] {
        assert_eq!(PresenceChoice::parse(bad), None, "{bad}");
    }
    let (kind, payload) = PresenceChoice::parse("busy").unwrap().outbox_payload(Some(Duration::from_secs(3600)));
    assert_eq!(kind, sp::KIND_SET_PREFERRED);
    assert_eq!(payload["expirationSecs"], 3600);
    assert_eq!(PresenceChoice::Reset.outbox_payload(None).0, sp::KIND_CLEAR_PREFERRED);
}

#[tokio::test]
async fn set_preferred_sends_documented_shape() {
    let t = FixtureTransport::new();
    t.on(Method::Post, "/me/presence/setUserPreferredPresence", FixtureResponse::status(200));
    t.on(Method::Post, "/me/presence/clearUserPreferredPresence", FixtureResponse::status(200));
    let c = ctx(&t);
    let (_, payload) = PresenceChoice::parse("dnd").unwrap().outbox_payload(Some(Duration::from_secs(3600)));
    SetPreferredHandler.apply(&c, payload).await.unwrap();
    let (_, payload) = PresenceChoice::parse("away").unwrap().outbox_payload(None);
    SetPreferredHandler.apply(&c, payload).await.unwrap();
    ClearPreferredHandler.apply(&c, json!({})).await.unwrap();
    let calls = t.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(
        body(&calls[0]),
        json!({ "availability": "DoNotDisturb", "activity": "DoNotDisturb", "expirationDuration": "PT1H" })
    );
    assert_eq!(body(&calls[1]), json!({ "availability": "Away", "activity": "Away" }));
    assert!(calls[2].url.path().ends_with("/me/presence/clearUserPreferredPresence"));
}

#[tokio::test]
async fn hostile_outbox_payloads_fail_permanently_without_requests() {
    let t = FixtureTransport::new();
    let c = ctx(&t);
    for p in [
        Value::Null,
        json!(5),
        json!({ "availability": "Busy", "activity": "InACall" }),
        json!({ "availability": "Busy", "activity": "Busy", "expirationSecs": 1 }),
        json!({ "availability": "x".repeat(10_000), "activity": "Busy" }),
    ] {
        let e = SetPreferredHandler.apply(&c, p.clone()).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Permanent, "{p}");
    }
    for p in [json!({ "message": "x".repeat(281) }), json!({ "message": "bad\u{7}" }), json!({ "message": 5 })] {
        let e = StatusMessageHandler.apply(&c, p.clone()).await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Permanent, "{p}");
    }
    assert_eq!(t.call_count(), 0);
}

#[tokio::test]
async fn status_message_shape() {
    let t = FixtureTransport::new();
    t.on(Method::Post, "/me/presence/setStatusMessage", FixtureResponse::status(200));
    let c = ctx(&t);
    StatusMessageHandler
        .apply(&c, json!({ "message": "Synthetic focus", "expiry": "2026-10-18T08:00:00Z" }))
        .await
        .unwrap();
    StatusMessageHandler.apply(&c, json!({ "message": "" })).await.unwrap();
    let calls = t.calls();
    let first = body(&calls[0]);
    assert_eq!(first["statusMessage"]["message"]["content"], "Synthetic focus");
    assert_eq!(first["statusMessage"]["expiryDateTime"]["timeZone"], "UTC");
    assert_eq!(body(&calls[1])["statusMessage"]["message"]["content"], "");
}

#[tokio::test]
async fn my_presence_learns_who_i_am() {
    let t = FixtureTransport::new();
    t.on(
        Method::Get,
        "/me",
        FixtureResponse::json(
            200,
            &json!({ "id": uid(1).as_str(), "displayName": "Test User", "mail": "test@contoso.example" }),
        ),
    );
    t.on(
        Method::Get,
        "/me/presence",
        FixtureResponse::json(200, &json!({ "id": uid(1).as_str(), "availability": "Away", "activity": "Away" })),
    );
    let c = ctx(&t);
    MyPresence.run(&c).await.unwrap();
    assert_eq!(sp::my_id(&c.store).await.unwrap(), Some(uid(1)));
    let p = c.store.presences(vec![uid(1)]).await.unwrap().items;
    assert_eq!(p.first().map(|p| p.presence.availability.clone()), Some(Availability::Away));
    // Second pass does not refetch /me.
    MyPresence.run(&c).await.unwrap();
    assert_eq!(t.calls().iter().filter(|r| r.url.path().ends_with("/me")).count(), 1);
}

fn user(n: usize, name: &str) -> Value {
    json!({ "@odata.type": "#microsoft.graph.user", "id": uid(n).as_str(), "displayName": name, "mail": format!("u{n}@contoso.example") })
}

#[tokio::test]
async fn org_chart_follows_managers_and_stops_on_cycles() {
    let t = FixtureTransport::new();
    let path = |n: usize, rest: &str| format!("/users/{}{rest}", uid(n).as_str());
    t.on(Method::Get, &path(1, "/manager"), FixtureResponse::json(200, &user(2, "Manager")));
    t.on(Method::Get, &path(2, "/manager"), FixtureResponse::json(200, &user(3, "Director")));
    t.on(Method::Get, &path(3, "/manager"), FixtureResponse::json(200, &user(1, "Cycle back")));
    t.on(
        Method::Get,
        &path(1, "/directReports"),
        FixtureResponse::json(
            200,
            &json!({ "value": [user(4, "Report"), { "@odata.type": "#microsoft.graph.group", "id": "g" }] }),
        ),
    );
    let c = ctx(&t);
    let chart = sp::fetch_org(&c, &uid(1)).await.unwrap();
    assert_eq!(chart.managers.len(), 2);
    assert_eq!(chart.reports.len(), 1);
    let cached = sp::cached_org(&c.store, &uid(1)).await.unwrap().unwrap();
    assert_eq!(cached, chart);
    // People were cached too.
    assert!(c.store.read(|r| r.person(&uid(4))).await.unwrap().is_some());
    // No reports at all (404) is fine.
    let chart = sp::fetch_org(&c, &uid(9)).await.unwrap();
    assert!(chart.managers.is_empty() && chart.reports.is_empty());
}

#[tokio::test]
async fn photos_are_cached_and_hostile_ones_rejected() {
    let t = FixtureTransport::new();
    let png = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
    let photo = |n: usize| format!("/users/{}/photos/64x64/$value", uid(n).as_str());
    t.on(Method::Get, &photo(1), FixtureResponse::bytes(200, png.to_vec()));
    t.on(Method::Get, &photo(2), FixtureResponse::bytes(200, b"<svg onload=alert(1)>".to_vec()));
    let c = ctx(&t);
    assert_eq!(sp::cached_photo(&c.store, &uid(1)).await.unwrap(), None);
    let got = sp::fetch_photo(&c, &uid(1)).await.unwrap();
    assert_eq!(got.map(|g| g.0), Some("image/png".to_owned()));
    assert!(matches!(sp::cached_photo(&c.store, &uid(1)).await.unwrap(), Some(Some(_))));
    assert_eq!(sp::fetch_photo(&c, &uid(2)).await.unwrap(), None);
    assert_eq!(sp::cached_photo(&c.store, &uid(2)).await.unwrap(), Some(None));
    // 404: no photo, remembered.
    assert_eq!(sp::fetch_photo(&c, &uid(3)).await.unwrap(), None);
    assert_eq!(sp::cached_photo(&c.store, &uid(3)).await.unwrap(), Some(None));
    assert_eq!(sp::image_type(b""), None);
    assert_eq!(sp::image_type(b"RIFF"), None);
}

#[tokio::test]
async fn directory_search_caches_people() {
    let t = FixtureTransport::new();
    t.on(
        Method::Get,
        "/users",
        FixtureResponse::json(200, &json!({ "value": [user(5, "Casey Directory"), { "id": "" }] })),
    );
    let c = ctx(&t);
    let found = sp::directory_search(&c, "cas", 500).await.unwrap();
    assert_eq!(found.len(), 1);
    let local = c.store.people_by_prefix("casey".into(), 10).await.unwrap().items;
    assert_eq!(local.len(), 1);
    let req = &t.calls()[0];
    assert_eq!(req.header("ConsistencyLevel"), Some("eventual"));
}

#[test]
fn status_messages_validate() {
    assert!(sp::validate_status_message("Out today\nback tomorrow").is_ok());
    assert!(sp::validate_status_message(&"é".repeat(280)).is_ok());
    assert!(sp::validate_status_message(&"é".repeat(281)).is_err());
    assert!(sp::validate_status_message("\u{1b}[31m").is_err());
}
