//! Tests for `quorum_graph::api::presence` against the fixture transport (synthetic data only).

use std::time::Duration;

use quorum_graph::api::presence::{self, MAX_PRESENCE_IDS, iso8601_duration};
use quorum_graph::{
    Client, ErrorKind, FakeClock, FixtureResponse, FixtureTransport, HttpRequest, Method, RateLimit, ResourceFamily,
    StaticToken,
};
use quorum_model::{Activity, Availability, UserId};
use serde_json::{Value, json};

const CLIENT_ID: &str = "00000000-0000-0000-0000-00000000c11e";
const U1: &str = "00000000-0000-0000-0000-000000000011";

fn client(t: &FixtureTransport) -> Client {
    let mut b = Client::builder(StaticToken::new("test-token"));
    for f in ResourceFamily::ALL {
        b = b.rate(f, RateLimit::new(10_000.0, 10_000).unwrap());
    }
    b.transport(t.clone()).clock(FakeClock::new()).jitter_seed(3).build().unwrap()
}

fn body(req: &HttpRequest) -> Value {
    serde_json::from_slice(&req.body).unwrap()
}

fn uid(n: usize) -> UserId {
    UserId::new(format!("00000000-0000-0000-0000-{n:012}")).unwrap()
}

#[tokio::test]
async fn my_presence_maps_status_message_and_expiry() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me/presence", FixtureResponse::file(200, "presence/me.json").unwrap());
    let p = client(&t).my_presence().await.unwrap();
    assert_eq!(p.user_id.as_ref().unwrap().as_str(), U1);
    assert_eq!(p.availability, Availability::Available);
    assert_eq!(p.activity, Activity::Available);
    assert_eq!(p.status_message.as_ref().unwrap().as_str(), "Heads down on synthetic work");
    // 08:00 Pacific Standard Time (PDT on that date) is 15:00 UTC.
    assert_eq!(p.expiry.unwrap().to_string(), "2026-10-18T15:00:00Z");
    let now: jiff::Timestamp = "2026-10-17T12:00:00Z".parse().unwrap();
    assert!(p.current_status_message(now).is_some());
    assert_eq!(t.calls()[0].url.path(), "/v1.0/me/presence");
}

#[tokio::test]
async fn presences_by_user_id_single_call() {
    let t = FixtureTransport::new();
    t.on(
        Method::Post,
        "/communications/getPresencesByUserId",
        FixtureResponse::file(200, "presence/presences.json").unwrap(),
    );
    let users = [uid(12), uid(13), uid(12)];
    let ps = client(&t).presences_by_user_id(&users).await.unwrap();
    assert_eq!(ps.len(), 3, "the non-object entry is dropped");
    assert_eq!(ps[0].availability, Availability::Busy);
    assert_eq!(ps[0].activity, Activity::InAMeeting);
    assert!(matches!(ps[1].availability, Availability::Unknown(_)));
    assert!(ps[1].status_message.is_none(), "empty status messages are none");
    assert!(ps[1].expiry.is_none(), "year-9999 'never' expiry maps to none");
    assert!(ps[2].user_id.is_none(), "invalid ids are dropped, the presence kept");
    let calls = t.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(body(&calls[0])["ids"], json!([uid(12).as_str(), uid(13).as_str()]), "duplicates removed");
    assert!(client(&t).presences_by_user_id(&[]).await.unwrap().is_empty());
    assert_eq!(t.call_count(), 1, "no ids, no request");
}

#[tokio::test]
async fn presences_chunk_by_graph_limit_and_batch() {
    let users: Vec<UserId> = (0..MAX_PRESENCE_IDS * 2 + 5).map(uid).collect();
    let reqs = presence::presences_by_user_id_requests(&users);
    assert_eq!(reqs.len(), 3);
    assert!(reqs.iter().all(|r| r.is_idempotent()));
    let sizes: Vec<usize> = reqs
        .iter()
        .map(|r| match r.body() {
            quorum_graph::Body::Json(v) => v["ids"].as_array().unwrap().len(),
            _ => 0,
        })
        .collect();
    assert_eq!(sizes, vec![650, 650, 5]);

    let t = FixtureTransport::new();
    let sub = |id: &str, user: &str| json!({"id": id, "status": 200, "body": {"value": [{"id": user, "availability": "Away", "activity": "Away"}]}});
    t.on(
        Method::Post,
        "/$batch",
        FixtureResponse::json(200, &json!({"responses": [sub("p2", "u-c"), sub("p0", "u-a"), sub("p1", "u-b")]})),
    );
    let ps = client(&t).presences_by_user_id(&users).await.unwrap();
    assert_eq!(ps.len(), 3);
    let calls = t.calls();
    assert_eq!(calls.len(), 1);
    let env = body(&calls[0]);
    let requests = env["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0]["url"], "/communications/getPresencesByUserId");
    assert_eq!(requests[0]["method"], "POST");
    assert_eq!(requests[2]["body"]["ids"].as_array().unwrap().len(), 5);

    let t = FixtureTransport::new();
    t.on(
        Method::Post,
        "/$batch",
        FixtureResponse::json(
            200,
            &json!({"responses": [
                {"id": "p0", "status": 200, "body": {"value": []}},
                {"id": "p1", "status": 403, "body": {"error": {"code": "Forbidden"}}},
                {"id": "p2", "status": 200, "body": {"value": []}}
            ]}),
        ),
    );
    let err = client(&t).presences_by_user_id(&users).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Forbidden);
}

#[tokio::test]
async fn set_presence_validates_and_shapes_body() {
    let t = FixtureTransport::new();
    for p in
        ["setPresence", "clearPresence", "setUserPreferredPresence", "clearUserPreferredPresence", "setStatusMessage"]
    {
        t.on(Method::Post, &format!("/me/presence/{p}"), FixtureResponse::status(200));
    }
    let c = client(&t);
    c.set_presence(CLIENT_ID, &Availability::Available, &Activity::Available, Duration::from_secs(3600)).await.unwrap();
    let b = body(&t.calls()[0]);
    assert_eq!(
        b,
        json!({"sessionId": CLIENT_ID, "availability": "Available", "activity": "Available", "expirationDuration": "PT1H"})
    );
    assert_eq!(t.calls()[0].url.path(), "/v1.0/me/presence/setPresence");

    let bad = [
        ("not-a-guid", Availability::Available, Activity::Available, 600),
        (CLIENT_ID, Availability::Available, Activity::Busy, 600),
        (CLIENT_ID, Availability::Offline, Activity::Offline, 600),
        (CLIENT_ID, Availability::Available, Activity::Available, 299),
        (CLIENT_ID, Availability::Available, Activity::Available, 4 * 3600 + 1),
    ];
    for (id, a, act, secs) in bad {
        let err = c.set_presence(id, &a, &act, Duration::from_secs(secs)).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Config);
    }
    c.set_presence(CLIENT_ID, &Availability::Busy, &Activity::InACall, Duration::from_secs(300)).await.unwrap();
    c.set_presence(CLIENT_ID, &Availability::DoNotDisturb, &Activity::Presenting, Duration::from_secs(14_400))
        .await
        .unwrap();
    assert_eq!(body(t.calls().last().unwrap())["expirationDuration"], "PT4H");
    c.clear_presence(CLIENT_ID).await.unwrap();
    assert_eq!(body(t.calls().last().unwrap()), json!({"sessionId": CLIENT_ID}));
    assert_eq!(t.call_count(), 4, "invalid calls never reach the transport");

    c.set_preferred_presence(&Availability::DoNotDisturb, &Activity::DoNotDisturb, Some(Duration::from_secs(5400)))
        .await
        .unwrap();
    assert_eq!(
        body(t.calls().last().unwrap()),
        json!({"availability": "DoNotDisturb", "activity": "DoNotDisturb", "expirationDuration": "PT1H30M"})
    );
    c.set_preferred_presence(&Availability::Offline, &Activity::OffWork, None).await.unwrap();
    assert!(body(t.calls().last().unwrap()).get("expirationDuration").is_none());
    assert!(c.set_preferred_presence(&Availability::Busy, &Activity::InACall, None).await.is_err());
    assert!(
        c.set_preferred_presence(&Availability::Busy, &Activity::Busy, Some(Duration::from_secs(30))).await.is_err()
    );
    c.clear_preferred_presence().await.unwrap();

    let expiry: jiff::Timestamp = "2026-10-18T17:00:00Z".parse().unwrap();
    c.set_status_message("Synthetic offsite", Some(expiry)).await.unwrap();
    assert_eq!(
        body(t.calls().last().unwrap()),
        json!({"statusMessage": {
            "message": {"content": "Synthetic offsite", "contentType": "text"},
            "expiryDateTime": {"dateTime": "2026-10-18T17:00:00", "timeZone": "UTC"}
        }})
    );
    c.set_status_message("", None).await.unwrap();
    assert!(c.set_status_message(&"x".repeat(281), None).await.is_err());
    assert!(c.set_status_message("bad\u{7}", None).await.is_err());
}

#[test]
fn iso_durations() {
    assert_eq!(iso8601_duration(Duration::from_secs(300)), "PT5M");
    assert_eq!(iso8601_duration(Duration::from_secs(3661)), "PT1H1M1S");
    assert_eq!(iso8601_duration(Duration::ZERO), "PT0S");
    let req = presence::my_presence_request();
    assert_eq!(req.resource_family(), ResourceFamily::Teams);
    assert_eq!(req.bucket_key(), presence::PRESENCE_BUCKET);
}

#[tokio::test]
async fn hostile_presence_payloads_never_panic() {
    for body in [
        json!(null),
        json!([]),
        json!({"id": 5, "availability": ["x"], "statusMessage": "x"}),
        json!({"statusMessage": {"message": {"content": 7}, "expiryDateTime": {"dateTime": "garbage", "timeZone": "Nowhere/Zone"}}}),
        json!({"statusMessage": {"expiryDateTime": {"dateTime": "2026-10-18T08:00:00", "timeZone": "Not A Windows Zone"}}}),
    ] {
        let t = FixtureTransport::new();
        t.on(Method::Get, "/me/presence", FixtureResponse::json(200, &body));
        t.on(Method::Post, "/communications/getPresencesByUserId", FixtureResponse::json(200, &body));
        let c = client(&t);
        let _ = c.my_presence().await;
        let _ = c.presences_by_user_id(&[uid(1)]).await;
    }
    let p = presence::parse_presence(
        json!({"statusMessage": {"expiryDateTime": {"dateTime": "2026-10-18T08:00:00", "timeZone": "Not A Zone"}}}),
    )
    .unwrap();
    assert!(p.expiry.is_none());
}
