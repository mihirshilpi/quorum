//! Free/busy chunking and caching. In its own test binary because the lookup cache is
//! process-wide: `CalendarModule::new()` in concurrently running tests clears it.

use jiff::Timestamp;
use quorum_graph::{
    Client, FakeClock, FixtureResponse, FixtureTransport, HttpRequest, Method, RateLimit, ResourceFamily, StaticToken,
};
use quorum_model::MailAddress;
use quorum_store::Store;
use quorum_sync::Ctx;
use quorum_sync::calendar::lookup;
use serde_json::Value;

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

fn body(r: &HttpRequest) -> Value {
    serde_json::from_slice(&r.body).unwrap_or(Value::Null)
}

#[tokio::test]
async fn free_busy_is_chunked_and_cached() {
    let t = FixtureTransport::new();
    t.on(Method::Post, "/me/calendar/getSchedule", file("calendar/schedule.json"));
    let c = ctx(&t);
    lookup::clear_cache();
    let users: Vec<MailAddress> = (0..25).map(|i| MailAddress::new(format!("p{i}@example.com")).unwrap()).collect();
    let from: Timestamp = "2030-01-01T00:00:00Z".parse().unwrap();
    let to: Timestamp = "2030-04-11T00:00:00Z".parse().unwrap(); // 100 days: two windows
    let found = lookup::free_busy(&c, &users, from, to, 30).await.unwrap();
    assert!(!found.is_empty());
    let calls = t.calls();
    assert_eq!(calls.len(), 4, "2 chunks of ≤20 people × 2 windows of ≤62 days");
    let sizes: Vec<usize> = calls.iter().map(|r| body(r)["schedules"].as_array().map_or(0, Vec::len)).collect();
    assert_eq!(sizes, [20, 5, 20, 5]);
    assert_eq!(body(&calls[0])["availabilityViewInterval"], 30);
    // Cached.
    lookup::free_busy(&c, &users, from, to, 30).await.unwrap();
    assert_eq!(t.call_count(), 4);
    // Hostile params are rejected before any request.
    assert!(lookup::free_busy(&c, &users, to, from, 30).await.is_err());
    assert!(lookup::free_busy(&c, &users, from, to, 1).await.is_err());
    let many: Vec<MailAddress> = (0..101).map(|i| MailAddress::new(format!("x{i}@example.com")).unwrap()).collect();
    assert!(lookup::free_busy(&c, &many, from, to, 30).await.is_err());
    assert!(lookup::free_busy(&c, &[], from, to, 30).await.unwrap().is_empty());
}
