//! Public-API tests for the Graph client against the fixture transport (M-INTEGRATION-TESTS).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use quorum_graph::{
    BatchItem, ChunkSize, Client, DeltaOutcome, ErrorKind, FakeClock, FixtureResponse, FixtureTransport, HttpRequest,
    HttpResponse, Method, RateLimit, Request, ResourceFamily, RetryPolicy, Route, StaticToken, Transport,
    UPLOAD_CHUNK_UNIT, UploadSession, parse_retry_after,
};
use serde_json::{Value, json};

const GRAPH: &str = "https://graph.microsoft.com/v1.0";

fn fast_rates(mut b: quorum_graph::ClientBuilder) -> quorum_graph::ClientBuilder {
    for f in ResourceFamily::ALL {
        b = b.rate(f, RateLimit::new(10_000.0, 10_000).unwrap());
    }
    b
}

fn client_with(transport: &FixtureTransport, clock: &FakeClock) -> Client {
    fast_rates(Client::builder(StaticToken::new("test-token")))
        .transport(transport.clone())
        .clock(clock.clone())
        .jitter_seed(7)
        .upload_host_suffixes(["upload.contoso.example"])
        .build()
        .unwrap()
}

fn manifest_client() -> (Client, FixtureTransport, FakeClock) {
    let t = FixtureTransport::from_default_manifest().unwrap();
    let c = FakeClock::new();
    (client_with(&t, &c), t, c)
}

fn query_of(req: &HttpRequest, key: &str) -> Option<String> {
    req.url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned())
}

#[tokio::test]
async fn send_json_adds_auth_and_fresh_client_request_id() {
    let (client, t, _) = manifest_client();
    let me: Value = client.send_json(Request::get("/me")).await.unwrap();
    assert_eq!(me["displayName"], "Test User");
    let _: Value = client.send_json(Request::get("/me")).await.unwrap();
    let calls = t.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].url.as_str(), "https://graph.microsoft.com/v1.0/me");
    assert_eq!(calls[0].header("authorization"), Some("Bearer test-token"));
    let id0 = calls[0].header("client-request-id").unwrap();
    let id1 = calls[1].header("client-request-id").unwrap();
    assert!(uuid::Uuid::parse_str(id0).is_ok());
    assert_ne!(id0, id1);
}

#[tokio::test]
async fn query_and_headers_are_sent() {
    let (client, t, _) = manifest_client();
    let req =
        Request::get("/me/messages").query("$top", "2").query("$filter", "isRead eq false").consistency_eventual();
    let _ = client.page::<Value>(req).await.unwrap();
    let call = &t.calls()[0];
    assert_eq!(query_of(call, "$top").as_deref(), Some("2"));
    assert_eq!(query_of(call, "$filter").as_deref(), Some("isRead eq false"));
    assert_eq!(call.header("consistencylevel"), Some("eventual"));
}

#[tokio::test]
async fn reserved_headers_are_rejected() {
    let (client, t, _) = manifest_client();
    let err = client.send_raw(Request::get("/me").header("Authorization", "Bearer x")).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Config);
    let err = client.send_raw(Request::get("/me").header("X-Test", "a\r\nInjected: 1")).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Config);
    assert_eq!(t.call_count(), 0);
}

#[tokio::test]
async fn paging_follows_next_link_and_keeps_prefer() {
    let (client, t, _) = manifest_client();
    let req = Request::get("/me/messages").query("$top", "2").prefer("outlook.body-content-type=\"text\"");
    let items: Vec<Value> = client.collect_all(req, 10).await.unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[2]["subject"], "Third synthetic message");
    let calls = t.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(query_of(&calls[1], "$skiptoken").as_deref(), Some("page2"));
    assert_eq!(calls[1].header("prefer"), Some("outlook.body-content-type=\"text\""));
}

fn page_with_next(n: usize) -> FixtureResponse {
    FixtureResponse::json(
        200,
        &json!({"value": [{"n": n}], "@odata.nextLink": format!("{GRAPH}/items?$skiptoken={n}")}),
    )
}

#[tokio::test]
async fn paging_stops_at_cap() {
    let t = FixtureTransport::new();
    let mut route = Route::new(Method::Get, "/items");
    for n in 0..10 {
        route = route.respond(page_with_next(n));
    }
    t.add(route);
    let client = client_with(&t, &FakeClock::new());
    let err = client.collect_all::<Value>(Request::get("/items"), 3).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::TooLarge);
    assert_eq!(t.call_count(), 3);

    // The stream yields the pages it got before the cap error.
    use futures::StreamExt;
    let results: Vec<_> = client.pages_capped::<Value>(Request::get("/items"), 2).collect().await;
    assert_eq!(results.len(), 3);
    assert!(results[0].is_ok() && results[1].is_ok());
    assert_eq!(results[2].as_ref().unwrap_err().kind(), ErrorKind::TooLarge);
}

#[tokio::test]
async fn paging_detects_next_link_loop() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/items", page_with_next(1));
    let client = client_with(&t, &FakeClock::new());
    let err = client.collect_all::<Value>(Request::get("/items"), 50).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Malformed);
    assert_eq!(t.call_count(), 2);
}

#[tokio::test]
async fn paging_skips_items_that_do_not_parse() {
    #[derive(serde::Deserialize)]
    struct Item {
        #[expect(dead_code, reason = "parsed only to test skipping")]
        id: String,
    }
    let t = FixtureTransport::new();
    t.on(
        Method::Get,
        "/items",
        FixtureResponse::json(
            200,
            &json!({"value": [{"id": "a"}, {"id": 5}, {"other": 1}, {"id": "b", "extra": [1,2]}]}),
        ),
    );
    let client = client_with(&t, &FakeClock::new());
    let page = client.page::<Item>(Request::get("/items")).await.unwrap();
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.skipped, 2);
}

#[tokio::test]
async fn delta_round_returns_items_and_delta_link() {
    let (client, t, _) = manifest_client();
    let first = client.delta_page::<Value>(Request::get("/me/mailFolders/inbox/messages/delta")).await.unwrap();
    assert!(matches!(first.outcome, DeltaOutcome::Next(ref l) if l.contains("delta-page2")));
    assert!(first.items[1].get("@removed").is_some());

    let round = client.delta_round::<Value>(Request::get("/me/mailFolders/inbox/messages/delta"), 10).await.unwrap();
    assert_eq!(round.items.len(), 3);
    assert_eq!(round.pages, 2);
    assert!(round.delta_link.ends_with("%24deltatoken=synthetic-token-1"));
    assert_eq!(t.call_count(), 3);
}

#[tokio::test]
async fn delta_expiry_is_delta_expired() {
    let (client, _, _) = manifest_client();
    let link = format!("{GRAPH}/me/mailFolders/inbox/messages/delta?$deltatoken=expired-token");
    let err = client.delta_page::<Value>(Request::link(link)).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::DeltaExpired);
    assert_eq!(err.status(), Some(410));
    assert_eq!(err.code(), Some("syncStateNotFound"));
    assert_eq!(err.request_id(), Some("00000000-0000-0000-0000-00000000a410"));

    // resyncRequired on a 400 is also an expiry.
    let t = FixtureTransport::new();
    t.on(
        Method::Get,
        "/me/events/delta",
        FixtureResponse::json(400, &json!({"error": {"code": "resyncRequired", "message": "x"}})),
    );
    let client = client_with(&t, &FakeClock::new());
    let err = client.delta_page::<Value>(Request::get("/me/events/delta")).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::DeltaExpired);
}

#[tokio::test]
async fn delta_page_without_links_is_malformed() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/d", FixtureResponse::json(200, &json!({"value": []})));
    let client = client_with(&t, &FakeClock::new());
    let err = client.delta_page::<Value>(Request::get("/d")).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Malformed);
}

#[tokio::test]
async fn ssrf_foreign_links_are_rejected_without_io() {
    let (client, t, _) = manifest_client();
    for link in [
        "https://evil.example.com/v1.0/me",
        "https://graph.microsoft.com.evil.example/v1.0/me",
        "http://graph.microsoft.com/v1.0/me",
        "https://graph.microsoft.com:8443/v1.0/me",
        "https://user:pw@graph.microsoft.com/v1.0/me",
        "https://graph.microsoft.com/beta/me",
        "https://graph.microsoft.com/v1.0evil/me",
        "file:///etc/passwd",
        "not a url",
    ] {
        let err = client.send_raw(Request::link(link)).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Malformed, "{link}");
    }
    for path in ["/../beta/me", "//evil.example/v1.0/me", "me", "/%2e%2e/beta/me", "/me#frag"] {
        let err = client.send_raw(Request::get(path)).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Config, "{path}");
    }
    assert_eq!(t.call_count(), 0);
}

#[tokio::test]
async fn ssrf_foreign_next_link_stops_paging() {
    let t = FixtureTransport::new();
    t.on(
        Method::Get,
        "/items",
        FixtureResponse::json(
            200,
            &json!({"value": [1], "@odata.nextLink": "https://attacker.example.com/v1.0/items?x=1"}),
        ),
    );
    let client = client_with(&t, &FakeClock::new());
    let err = client.collect_all::<Value>(Request::get("/items"), 10).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Malformed);
    assert_eq!(t.call_count(), 1);
}

#[test]
fn retry_after_parses_seconds_and_http_dates() {
    let now: jiff::Timestamp = "2026-01-01T00:00:00Z".parse().unwrap();
    assert_eq!(parse_retry_after("120", now), Some(Duration::from_secs(120)));
    assert_eq!(parse_retry_after(" 0 ", now), Some(Duration::ZERO));
    assert_eq!(parse_retry_after("Thu, 01 Jan 2026 00:00:30 GMT", now), Some(Duration::from_secs(30)));
    assert_eq!(parse_retry_after("Wed, 31 Dec 2025 23:00:00 GMT", now), Some(Duration::ZERO));
    assert_eq!(parse_retry_after("soon", now), None);
    assert_eq!(parse_retry_after("-5", now), None);
    assert_eq!(parse_retry_after("", now), None);
    assert_eq!(parse_retry_after("99999999999999999999999", now), Some(Duration::from_secs(u64::MAX)));
}

#[tokio::test]
async fn retry_after_on_429_is_honoured() {
    let t = FixtureTransport::new();
    t.add(
        Route::new(Method::Get, "/me")
            .respond(FixtureResponse::file(429, "throttled.json").unwrap().with_header("Retry-After", "7"))
            .respond(FixtureResponse::file(200, "me.json").unwrap()),
    );
    let clock = FakeClock::new();
    let client = client_with(&t, &clock);
    let me: Value = client.send_json(Request::get("/me")).await.unwrap();
    assert_eq!(me["id"], "00000000-0000-0000-0000-000000000001");
    assert_eq!(t.call_count(), 2);
    assert!(clock.sleeps().contains(&Duration::from_secs(7)));
}

#[tokio::test]
async fn retry_after_http_date_on_503_is_honoured() {
    let start: jiff::Timestamp = "2026-01-01T00:00:00Z".parse().unwrap();
    let clock = FakeClock::at(start);
    let t = FixtureTransport::new();
    t.add(
        Route::new(Method::Get, "/me")
            .respond(FixtureResponse::status(503).with_header("Retry-After", "Thu, 01 Jan 2026 00:00:05 GMT"))
            .respond(FixtureResponse::file(200, "me.json").unwrap()),
    );
    let client = client_with(&t, &clock);
    client.send_raw(Request::get("/me")).await.unwrap();
    assert_eq!(clock.sleeps(), vec![Duration::from_secs(5)]);
}

#[tokio::test]
async fn long_retry_after_surfaces_as_throttled() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me", FixtureResponse::file(429, "throttled.json").unwrap().with_header("Retry-After", "3600"));
    let clock = FakeClock::new();
    let client = client_with(&t, &clock);
    let err = client.send_raw(Request::get("/me")).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Throttled);
    assert_eq!(err.retry_after(), Some(Duration::from_secs(3600)));
    assert_eq!(err.code(), Some("TooManyRequests"));
    assert_eq!(t.call_count(), 1);
}

#[tokio::test]
async fn backoff_sequence_grows_with_jitter() {
    let t = FixtureTransport::new();
    let mut route = Route::new(Method::Get, "/me");
    for _ in 0..4 {
        route = route.respond(FixtureResponse::status(500));
    }
    t.add(route.respond(FixtureResponse::file(200, "me.json").unwrap()));
    let clock = FakeClock::new();
    let client = client_with(&t, &clock);
    client.send_raw(Request::get("/me")).await.unwrap();
    let sleeps = clock.sleeps();
    assert_eq!(sleeps.len(), 4);
    for (i, s) in sleeps.iter().enumerate() {
        let full = Duration::from_millis(500 * (1 << i));
        assert!(*s >= full / 2 && *s <= full, "retry {i}: {s:?} not in [{:?}, {full:?}]", full / 2);
    }
    // Same seed, same sequence.
    let t2 = FixtureTransport::new();
    let mut route = Route::new(Method::Get, "/me");
    for _ in 0..4 {
        route = route.respond(FixtureResponse::status(500));
    }
    t2.add(route.respond(FixtureResponse::status(200)));
    let clock2 = FakeClock::new();
    client_with(&t2, &clock2).send_raw(Request::get("/me")).await.unwrap();
    assert_eq!(clock2.sleeps(), sleeps);
}

#[test]
fn backoff_is_capped_and_handles_bad_jitter() {
    let p = RetryPolicy::default();
    assert_eq!(p.backoff(1, 1.0), Duration::from_millis(500));
    assert_eq!(p.backoff(1, 0.0), Duration::from_millis(250));
    assert_eq!(p.backoff(40, 1.0), Duration::from_secs(30));
    assert_eq!(p.backoff(u32::MAX, f64::NAN), Duration::from_secs(15));
    assert_eq!(p.backoff(2, -3.0), Duration::from_millis(500));
}

#[tokio::test]
async fn retries_are_exhausted_into_server_error() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me", FixtureResponse::status(502));
    let clock = FakeClock::new();
    let client = client_with(&t, &clock);
    let err = client.send_raw(Request::get("/me")).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Server);
    assert_eq!(t.call_count(), 5);
    assert_eq!(clock.sleeps().len(), 4);
}

#[tokio::test]
async fn non_idempotent_requests_are_not_retried_unless_flagged() {
    let t = FixtureTransport::new();
    t.on(Method::Post, "/me/sendMail", FixtureResponse::status(503));
    let client = client_with(&t, &FakeClock::new());
    let err = client.send_raw(Request::post("/me/sendMail").json_value(json!({}))).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Server);
    assert_eq!(t.call_count(), 1);

    let t = FixtureTransport::new();
    t.add(
        Route::new(Method::Post, "/me/events")
            .respond(FixtureResponse::status(503))
            .respond(FixtureResponse::status(201)),
    );
    let client = client_with(&t, &FakeClock::new());
    let req = Request::post("/me/events").json_value(json!({"transactionId": "00000000-0000-0000-0000-000000000009"}));
    client.send_raw(req.idempotent(true)).await.unwrap();
    assert_eq!(t.call_count(), 2);
    assert_eq!(t.calls()[0].header("content-type"), Some("application/json"));
}

#[tokio::test]
async fn network_errors_are_retried_for_idempotent_requests() {
    let t = FixtureTransport::new();
    t.add(Route::new(Method::Get, "/me").respond(FixtureResponse::NetworkError).respond(FixtureResponse::status(200)));
    let client = client_with(&t, &FakeClock::new());
    client.send_raw(Request::get("/me")).await.unwrap();
    assert_eq!(t.call_count(), 2);
}

#[tokio::test]
async fn unauthorized_parses_claims_challenge() {
    let t = FixtureTransport::new();
    let challenge = r#"Bearer realm="", authorization_uri="https://login.microsoftonline.com/common/oauth2/authorize", error="insufficient_claims", claims="eyJhY2Nlc3NfdG9rZW4iOnsibmJmIjp7ImVzc2VudGlhbCI6dHJ1ZX19fQ==""#;
    t.on(
        Method::Get,
        "/me",
        FixtureResponse::json(401, &json!({"error": {"code": "InvalidAuthenticationToken", "message": "x"}}))
            .with_header("WWW-Authenticate", challenge)
            .with_header("request-id", "00000000-0000-0000-0000-0000000000aa"),
    );
    let client = client_with(&t, &FakeClock::new());
    let err = client.send_raw(Request::get("/me")).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unauthorized);
    assert_eq!(err.claims(), Some("eyJhY2Nlc3NfdG9rZW4iOnsibmJmIjp7ImVzc2VudGlhbCI6dHJ1ZX19fQ=="));
    assert_eq!(err.code(), Some("InvalidAuthenticationToken"));
    assert_eq!(err.request_id(), Some("00000000-0000-0000-0000-0000000000aa"));
    assert_eq!(t.call_count(), 1);
}

#[tokio::test]
async fn status_codes_map_to_kinds() {
    let cases = [
        (400, ErrorKind::BadRequest),
        (403, ErrorKind::Forbidden),
        (404, ErrorKind::NotFound),
        (409, ErrorKind::Conflict),
        (412, ErrorKind::PreconditionFailed),
        (413, ErrorKind::TooLarge),
        (418, ErrorKind::BadRequest),
    ];
    for (status, kind) in cases {
        let t = FixtureTransport::new();
        t.on(
            Method::Get,
            "/x",
            FixtureResponse::json(
                status,
                &json!({"error": {"code": "SomeCode", "message": "secret alice@example.com"}}),
            ),
        );
        let client = client_with(&t, &FakeClock::new());
        let err = client.send_raw(Request::get("/x")).await.unwrap_err();
        assert_eq!(err.kind(), kind, "{status}");
        assert_eq!(err.code(), Some("SomeCode"));
        assert_eq!(err.graph_message(), Some("secret alice@example.com"));
        // The message never leaks through Display or Debug.
        assert!(!format!("{err} {err:?}").contains("alice"));
    }
}

#[tokio::test]
async fn debug_output_redacts_secrets() {
    let token = StaticToken::new("super-secret-token");
    assert!(!format!("{token:?}").contains("super-secret"));
    let req = Request::get("/users/alice@example.com/messages")
        .query("$filter", "from eq 'bob@example.com'")
        .json_value(json!({"subject": "Private"}));
    let dbg = format!("{req:?}");
    assert!(!dbg.contains("alice") && !dbg.contains("bob") && !dbg.contains("Private"), "{dbg}");
    let t = FixtureTransport::new();
    let client = client_with(&t, &FakeClock::new());
    let _ = client.send_raw(Request::get("/users/alice@example.com")).await;
    let call = format!("{:?}", t.calls()[0]);
    assert!(!call.contains("alice") && !call.contains("test-token"), "{call}");
}

#[tokio::test]
async fn batch_maps_responses_and_retries_throttled_items() {
    let t = FixtureTransport::new();
    t.on(Method::Post, "/$batch", FixtureResponse::file(200, "batch-partial-throttle.json").unwrap());
    t.on(Method::Get, "/me/messages", FixtureResponse::file(200, "messages-page2.json").unwrap());
    t.on(Method::Get, "/me/messages/m3", FixtureResponse::json(200, &json!({"id": "m3"})));
    let clock = FakeClock::new();
    let client = client_with(&t, &clock);
    let items = vec![
        BatchItem::new("1", Request::get("/me").query("$select", "id,displayName")),
        BatchItem::new("2", Request::get("/me/messages")),
        BatchItem::new("3", Request::get("/me/messages/m3")).depends_on("2"),
        BatchItem::new("4", Request::get("/me/messages/missing")),
    ];
    let out = client.batch(items).await.unwrap();
    let ids: Vec<&str> = out.iter().map(|r| r.id()).collect();
    assert_eq!(ids, ["1", "2", "3", "4"]);
    let statuses: Vec<u16> = out.iter().map(|r| r.status()).collect();
    assert_eq!(statuses, [200, 200, 200, 404]);
    assert_eq!(out[0].json::<Value>().unwrap()["displayName"], "Test User");
    assert_eq!(out[2].json::<Value>().unwrap()["id"], "m3");
    assert_eq!(out[3].error().kind(), ErrorKind::NotFound);
    assert_eq!(out[3].json::<Value>().unwrap_err().code(), Some("ErrorItemNotFound"));
    assert!(clock.sleeps().contains(&Duration::from_secs(3)));

    let calls = t.calls();
    let paths: Vec<&str> = calls.iter().map(|c| c.url.path()).collect();
    assert_eq!(paths, ["/v1.0/$batch", "/v1.0/me/messages", "/v1.0/me/messages/m3"]);
    let envelope: Value = serde_json::from_slice(&calls[0].body).unwrap();
    let reqs = envelope["requests"].as_array().unwrap();
    assert_eq!(reqs.len(), 4);
    assert_eq!(reqs[0]["url"], "/me?%24select=id%2CdisplayName");
    assert_eq!(reqs[0]["method"], "GET");
    assert_eq!(reqs[2]["dependsOn"], json!(["2"]));
}

#[tokio::test]
async fn batch_splits_into_chunks_of_twenty() {
    let t = FixtureTransport::new();
    let resp = |range: std::ops::Range<usize>| {
        let responses: Vec<Value> =
            range.map(|i| json!({"id": i.to_string(), "status": 200, "body": {"n": i}})).collect();
        FixtureResponse::json(200, &json!({"responses": responses}))
    };
    t.add(Route::new(Method::Post, "/$batch").respond(resp(0..20)).respond(resp(20..25)));
    let client = client_with(&t, &FakeClock::new());
    let items = (0..25).map(|i| BatchItem::new(i.to_string(), Request::get(format!("/users/{i}")))).collect();
    let out = client.batch(items).await.unwrap();
    assert_eq!(out.len(), 25);
    assert_eq!(out[24].json::<Value>().unwrap()["n"], 24);
    assert_eq!(t.call_count(), 2);
    let second: Value = serde_json::from_slice(&t.calls()[1].body).unwrap();
    assert_eq!(second["requests"].as_array().unwrap().len(), 5);
}

#[tokio::test]
async fn batch_missing_sub_response_is_malformed() {
    let t = FixtureTransport::new();
    t.on(Method::Post, "/$batch", FixtureResponse::json(200, &json!({"responses": [{"id": "a", "status": 200}]})));
    let client = client_with(&t, &FakeClock::new());
    let items = vec![BatchItem::new("a", Request::get("/me")), BatchItem::new("b", Request::get("/me"))];
    assert_eq!(client.batch(items).await.unwrap_err().kind(), ErrorKind::Malformed);
}

/// A transport that tracks how many requests are in flight at once.
#[derive(Clone, Default)]
struct SlowTransport {
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl Transport for SlowTransport {
    async fn send(&self, _request: HttpRequest) -> quorum_graph::Result<HttpResponse> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(10)).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(HttpResponse::new(204))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bucket_concurrency_is_limited_per_key() {
    let transport = SlowTransport::default();
    let client = fast_rates(Client::builder(StaticToken::new("t")))
        .transport(transport.clone())
        .clock(FakeClock::new())
        .bucket_concurrency(2)
        .build()
        .unwrap();
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let c = client.clone();
            tokio::spawn(async move { c.send_raw(Request::get("/users/u1/messages").bucket("u1")).await })
        })
        .collect();
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(transport.peak.load(Ordering::SeqCst), 2);

    // Different keys run side by side.
    transport.peak.store(0, Ordering::SeqCst);
    let tasks: Vec<_> = (0..4)
        .map(|i| {
            let c = client.clone();
            tokio::spawn(async move { c.send_raw(Request::get("/me").bucket(format!("u{i}"))).await })
        })
        .collect();
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert!(transport.peak.load(Ordering::SeqCst) > 2);
}

#[tokio::test]
async fn rate_limiter_spaces_requests_per_family() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me/messages", FixtureResponse::status(204));
    t.on(Method::Get, "/me", FixtureResponse::status(204));
    let clock = FakeClock::new();
    let client = Client::builder(StaticToken::new("t"))
        .transport(t.clone())
        .clock(clock.clone())
        .rate(ResourceFamily::Mail, RateLimit::new(1.0, 2).unwrap())
        .rate(ResourceFamily::Other, RateLimit::new(10_000.0, 10_000).unwrap())
        .build()
        .unwrap();
    for _ in 0..4 {
        client.send_raw(Request::get("/me/messages")).await.unwrap();
    }
    // Burst of 2, then one per second.
    let waited = clock.elapsed();
    assert!(waited >= Duration::from_millis(1900) && waited <= Duration::from_millis(2100), "{waited:?}");
    // Another family is not slowed down.
    let before = clock.elapsed();
    client.send_raw(Request::get("/me")).await.unwrap();
    assert_eq!(clock.elapsed(), before);
}

#[test]
fn families_are_inferred_from_paths() {
    assert_eq!(ResourceFamily::infer("/me/messages"), ResourceFamily::Mail);
    assert_eq!(ResourceFamily::infer("/me/mailFolders/inbox/messages/delta"), ResourceFamily::Mail);
    assert_eq!(ResourceFamily::infer("/chats/19:abc/messages"), ResourceFamily::Teams);
    assert_eq!(ResourceFamily::infer("/me/calendarView?startDateTime=x"), ResourceFamily::Calendar);
    assert_eq!(ResourceFamily::infer("/drives/b!x/root/delta"), ResourceFamily::Files);
    assert_eq!(ResourceFamily::infer("/me"), ResourceFamily::Other);
}

#[test]
fn builder_validates_base_url() {
    let b = || Client::builder(StaticToken::new("t")).transport(FixtureTransport::new());
    assert_eq!(b().build().unwrap().base_url().as_str(), "https://graph.microsoft.com/v1.0");
    assert_eq!(b().beta(true).build().unwrap().base_url().as_str(), "https://graph.microsoft.com/beta");
    assert_eq!(
        b().base_url("https://graph.microsoft.us/v1.0/").build().unwrap().base_url().as_str(),
        "https://graph.microsoft.us/v1.0"
    );
    assert!(b().base_url("http://127.0.0.1:8080/v1.0").build().is_ok());
    for bad in [
        "http://graph.microsoft.com/v1.0",
        "https://u:p@graph.microsoft.com/v1.0",
        "https://graph.microsoft.com/v1.0?x=1",
        "nope",
    ] {
        assert_eq!(b().base_url(bad).build().unwrap_err().kind(), ErrorKind::Config, "{bad}");
    }
    assert_eq!(b().base_url(GRAPH).beta(true).build().unwrap_err().kind(), ErrorKind::Config);
}

const UPLOAD_URL: &str = "https://upload.contoso.example/upload/session-1?tempauth=synthetic";

#[tokio::test]
async fn upload_session_resumes_from_next_expected_ranges() {
    let t = FixtureTransport::new();
    t.on(
        Method::Post,
        "/me/drive/root:/report.bin:/createUploadSession",
        FixtureResponse::json(200, &json!({"uploadUrl": UPLOAD_URL, "expirationDateTime": "2026-01-02T00:00:00Z", "nextExpectedRanges": ["655360-"]})),
    );
    let total = 3 * UPLOAD_CHUNK_UNIT + 100;
    let final_start = 3 * UPLOAD_CHUNK_UNIT;
    t.add(Route::new(Method::Put, "/upload/session-1").respond(FixtureResponse::NetworkError).respond(
        FixtureResponse::json(
            201,
            &json!({"id": "00000000-0000-0000-0000-000000000301", "name": "report.bin", "size": total}),
        ),
    ));
    t.on(
        Method::Get,
        "/upload/session-1",
        FixtureResponse::json(200, &json!({"nextExpectedRanges": [format!("{final_start}-")]})),
    );
    let clock = FakeClock::new();
    let client = client_with(&t, &clock);
    let session = client
        .create_upload_session(
            Request::post("/me/drive/root:/report.bin:/createUploadSession")
                .json_value(json!({"item": {"@microsoft.graph.conflictBehavior": "rename"}})),
        )
        .await
        .unwrap();
    assert_eq!(session.next_offset(), Some(655_360));
    assert!(!format!("{session:?}").contains("tempauth"));

    let data: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
    let item = client.upload(&session, &data, ChunkSize::from_units(1).unwrap()).await.unwrap();
    assert_eq!(item["name"], "report.bin");

    let calls = t.calls();
    let uploads: Vec<&HttpRequest> =
        calls.iter().filter(|c| c.url.host_str() == Some("upload.contoso.example")).collect();
    assert_eq!(uploads.len(), 3);
    assert_eq!(uploads[0].header("content-range"), Some(format!("bytes 655360-983039/{total}").as_str()));
    assert_eq!(uploads[0].body.len(), UPLOAD_CHUNK_UNIT);
    assert_eq!(uploads[1].method, Method::Get);
    assert_eq!(uploads[2].header("content-range"), Some(format!("bytes {final_start}-{}/{total}", total - 1).as_str()));
    assert_eq!(uploads[2].body, data[final_start..]);
    // The pre-authenticated URL never gets our token.
    assert!(uploads.iter().all(|c| c.header("authorization").is_none()));
}

#[tokio::test]
async fn upload_follows_202_ranges_and_rejects_foreign_hosts() {
    let t = FixtureTransport::new();
    t.add(
        Route::new(Method::Put, "/upload/session-1")
            .respond(FixtureResponse::json(202, &json!({"nextExpectedRanges": [format!("{}-", UPLOAD_CHUNK_UNIT)]})))
            .respond(FixtureResponse::json(200, &json!({"id": "x"}))),
    );
    let client = client_with(&t, &FakeClock::new());
    let session = UploadSession::from_parts(UPLOAD_URL, None, vec![]).unwrap();
    let data = vec![1u8; UPLOAD_CHUNK_UNIT + 1];
    let item = client.upload(&session, &data, ChunkSize::from_units(1).unwrap()).await.unwrap();
    assert_eq!(item["id"], "x");
    assert_eq!(
        t.calls()[1].header("content-range"),
        Some(format!("bytes {}-{}/{}", UPLOAD_CHUNK_UNIT, UPLOAD_CHUNK_UNIT, UPLOAD_CHUNK_UNIT + 1).as_str())
    );

    let t = FixtureTransport::new();
    t.on(
        Method::Post,
        "/me/drive/items/x/createUploadSession",
        FixtureResponse::json(200, &json!({"uploadUrl": "https://attacker.example.com/upload"})),
    );
    let client = client_with(&t, &FakeClock::new());
    let err = client.create_upload_session(Request::post("/me/drive/items/x/createUploadSession")).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Malformed);
    let evil = UploadSession::from_parts("https://upload.contoso.example.attacker.example/u", None, vec![]).unwrap();
    assert_eq!(client.upload(&evil, &data, ChunkSize::default()).await.unwrap_err().kind(), ErrorKind::Malformed);
}

#[tokio::test]
async fn upload_expired_session_is_not_found() {
    let t = FixtureTransport::new();
    t.on(Method::Put, "/upload/session-1", FixtureResponse::status(404));
    let client = client_with(&t, &FakeClock::new());
    let session = UploadSession::from_parts(UPLOAD_URL, None, vec!["0-".into()]).unwrap();
    let err = client.upload(&session, &vec![0u8; 10], ChunkSize::default()).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotFound);
}

#[test]
fn chunk_size_must_be_in_units() {
    assert_eq!(ChunkSize::from_units(1).unwrap().bytes(), 327_680);
    assert!(ChunkSize::from_units(0).is_err());
    assert!(ChunkSize::from_units(193).is_err());
    assert_eq!(ChunkSize::default().bytes() % UPLOAD_CHUNK_UNIT, 0);
}

#[tokio::test]
async fn throttling_pauses_the_whole_family() {
    let t = FixtureTransport::new();
    t.add(
        Route::new(Method::Get, "/me/messages")
            .respond(FixtureResponse::status(429).with_header("Retry-After", "4"))
            .respond(FixtureResponse::status(200)),
    );
    t.on(Method::Get, "/me/mailFolders", FixtureResponse::status(200));
    let clock = FakeClock::new();
    let client = client_with(&t, &clock);
    // Not idempotent → no retry, but the family is paused.
    let err = client.send_raw(Request::get("/me/messages").idempotent(false)).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Throttled);
    assert_eq!(err.retry_after(), Some(Duration::from_secs(4)));
    client.send_raw(Request::get("/me/mailFolders")).await.unwrap();
    assert!(clock.elapsed() >= Duration::from_secs(4));
}
