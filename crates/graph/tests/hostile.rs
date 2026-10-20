//! Hostile Graph responses must never panic (CLAUDE.md "Never crash").

use quorum_graph::{
    BatchItem, ChunkSize, Client, ErrorKind, FakeClock, FixtureResponse, FixtureTransport, Method, RateLimit, Request,
    ResourceFamily, StaticToken, UploadSession,
};
use serde_json::Value;

fn hostile_bodies() -> Vec<Vec<u8>> {
    let mut bodies: Vec<Vec<u8>> = vec![
        b"".to_vec(),
        b"   ".to_vec(),
        b"garbage".to_vec(),
        b"\xff\xfe\x00\x01not utf8".to_vec(),
        b"\xEF\xBB\xBF{\"value\":[]}".to_vec(),
        b"{".to_vec(),
        b"{\"value\": [1, 2".to_vec(),
        b"{\"value\": \"not an array\"}".to_vec(),
        b"{\"value\": null, \"@odata.nextLink\": 5}".to_vec(),
        b"{\"value\": [], \"@odata.nextLink\": \"\"}".to_vec(),
        b"{\"value\": [], \"@odata.deltaLink\": \"javascript:alert(1)\"}".to_vec(),
        b"{\"value\": [], \"@odata.nextLink\": \"https://graph.microsoft.com@evil.example/v1.0/x\"}".to_vec(),
        b"{\"value\": [1e999999, -1e999999, 123456789012345678901234567890]}".to_vec(),
        b"{\"responses\": \"x\"}".to_vec(),
        b"{\"responses\": [{\"id\": 1, \"status\": \"200\"}, {\"id\": \"a\", \"status\": 99999999}, null, []]}"
            .to_vec(),
        b"{\"error\": {\"code\": 5, \"message\": [1], \"innerError\": \"x\"}}".to_vec(),
        b"{\"error\": null}".to_vec(),
        b"{\"uploadUrl\": 7, \"nextExpectedRanges\": [\"-\", \"abc-\", \"99999999999999999999999-\"]}".to_vec(),
        b"{\"nextExpectedRanges\": [\"18446744073709551615-\"]}".to_vec(),
        b"[]".to_vec(),
        b"null".to_vec(),
        b"\"\\ud800\"".to_vec(),
        b"{\"a\":\"\\".to_vec(),
        b"[\"]]]]]]]]\"]".to_vec(),
    ];
    // Deeply nested arrays and objects.
    bodies.push("[".repeat(100_000).into_bytes());
    bodies.push(format!("{}{}", "[".repeat(5_000), "]".repeat(5_000)).into_bytes());
    bodies.push(format!("{{\"value\":{}{}}}", "{\"a\":".repeat(10_000), "}".repeat(10_000)).into_bytes());
    // Huge strings.
    let huge = "A".repeat(1024 * 1024);
    bodies.push(format!("{{\"value\":[{{\"id\":\"{huge}\"}}],\"@odata.nextLink\":\"{huge}\"}}").into_bytes());
    bodies.push(format!("{{\"error\":{{\"code\":\"{huge}\",\"message\":\"{huge}\"}}}}").into_bytes());
    // Truncations of a valid page at many points.
    let valid = std::fs::read(quorum_graph::fixtures_dir().join("messages-page1.json")).unwrap();
    for cut in (0..valid.len()).step_by(37) {
        bodies.push(valid[..cut].to_vec());
    }
    bodies
}

fn client(t: &FixtureTransport) -> Client {
    let mut b = Client::builder(StaticToken::new("t"))
        .transport(t.clone())
        .clock(FakeClock::new())
        .upload_host_suffixes(["upload.contoso.example"]);
    for f in ResourceFamily::ALL {
        b = b.rate(f, RateLimit::new(10_000.0, 10_000).unwrap());
    }
    b.build().unwrap()
}

#[tokio::test]
async fn hostile_graph_json_never_panics() {
    let statuses = [200u16, 201, 202, 400, 401, 404, 410, 429, 500];
    for body in hostile_bodies() {
        for status in statuses {
            let t = FixtureTransport::new();
            let resp = FixtureResponse::bytes(status, body.clone())
                .with_header("Retry-After", "Thu, 99 Foo 2026 25:61:61 GMT")
                .with_header("WWW-Authenticate", "Bearer claims=\"unterminated, error=, =x, \\");
            for (m, p) in [
                (Method::Get, "/x"),
                (Method::Post, "/$batch"),
                (Method::Post, "/me/drive/root:/f:/createUploadSession"),
                (Method::Put, "/upload/s"),
                (Method::Get, "/upload/s"),
            ] {
                t.on(m, p, resp.clone());
            }
            let c = client(&t);
            let _ = c.send_json::<Value>(Request::get("/x")).await;
            let _ = c.page::<Value>(Request::get("/x")).await;
            let _ = c.collect_all::<Value>(Request::get("/x"), 3).await;
            let _ = c.delta_page::<Value>(Request::get("/x")).await;
            let _ = c.delta_round::<Value>(Request::get("/x"), 3).await;
            let _ =
                c.batch(vec![BatchItem::new("a", Request::get("/x")), BatchItem::new("1", Request::get("/x"))]).await;
            let _ = c.create_upload_session(Request::post("/me/drive/root:/f:/createUploadSession")).await;
            let session = UploadSession::from_parts("https://upload.contoso.example/upload/s", None, vec![]).unwrap();
            let _ = c.upload(&session, &vec![0u8; 1000], ChunkSize::default()).await;
            let _ = c.upload_status(&session).await;
        }
    }
}

#[tokio::test]
async fn deeply_nested_json_is_malformed_not_a_stack_overflow() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/x", FixtureResponse::bytes(200, "[".repeat(1_000_000)));
    let err = client(&t).send_json::<Value>(Request::get("/x")).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Malformed);
}

#[tokio::test]
async fn oversized_response_is_too_large() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/x", FixtureResponse::bytes(200, vec![b' '; 2048]));
    let c = Client::builder(StaticToken::new("t"))
        .transport(t.clone())
        .clock(FakeClock::new())
        .max_response_bytes(1024)
        .build()
        .unwrap();
    let err = c.send_raw(Request::get("/x")).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::TooLarge);
}

#[tokio::test]
async fn hostile_retry_after_values_do_not_hang_or_panic() {
    for value in [
        "",
        "-1",
        "1.5",
        "18446744073709551616",
        "Mon, 01 Jan 9999 00:00:00 GMT",
        "\u{0}",
        "Thu, 01 Jan 1970 00:00:00 GMT",
    ] {
        let t = FixtureTransport::new();
        t.on(Method::Get, "/x", FixtureResponse::status(429).with_header("Retry-After", value));
        let clock = FakeClock::new();
        let c = Client::builder(StaticToken::new("t")).transport(t.clone()).clock(clock.clone()).build().unwrap();
        let err = c.send_raw(Request::get("/x")).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Throttled, "{value:?}");
        assert!(t.call_count() <= 5);
    }
}
