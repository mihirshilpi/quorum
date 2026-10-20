//! Sync engine behaviour under tokio's paused clock: cadence, pokes, backoff, panics,
//! throttling, auth pause, discovery, the outbox replayer and the presence keep-alive.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{FakeHandler, FakeModule, FakeResource, Step, advance, ctx, wait_for};
use quorum_graph::{FixtureResponse, FixtureTransport, Method};
use quorum_store::{NewOutboxEntry, OutboxId};
use quorum_sync::{Activity, Cadence, Ctx, Engine, EngineConfig, EngineHandle, ErrorKind, Module, Outcome, Resource};
use serde_json::{Value, json};

const S: fn(u64) -> Duration = Duration::from_secs;

/// A context whose keep-alive is off, so only the resources under test talk to Graph.
async fn quiet_ctx() -> (Ctx, FixtureTransport) {
    let t = FixtureTransport::new();
    let c = ctx(&t);
    c.store.set_setting(quorum_sync::KEEPALIVE_SETTING.to_owned(), false).await.unwrap();
    (c, t)
}

fn res(r: &Arc<FakeResource>) -> Arc<dyn Resource> {
    Arc::clone(r) as Arc<dyn Resource>
}

fn modules(m: &Arc<FakeModule>) -> Vec<Arc<dyn Module>> {
    vec![Arc::clone(m) as Arc<dyn Module>]
}

async fn start(resources: Vec<Arc<dyn Resource>>) -> (EngineHandle, Arc<FakeModule>) {
    let (c, _t) = quiet_ctx().await;
    let m = FakeModule::new("fake", resources);
    (Engine::start(c, modules(&m)), m)
}

fn within(d: Duration, lo: u64, hi: u64) -> bool {
    d >= Duration::from_millis(lo) && d <= Duration::from_millis(hi)
}

#[tokio::test(start_paused = true)]
async fn cadence_follows_focus_and_activity() {
    let r = FakeResource::new("mail.folder:inbox", "mail", Cadence::secs(10, 60, 300));
    let (h, _m) = start(vec![res(&r)]).await;
    h.set_focus("mail");
    assert!(wait_for(S(30), || r.count() >= 3).await);
    for g in r.gaps() {
        assert!(within(g, 9_000, 11_100), "focused gap {g:?}");
    }
    h.set_focus("calendar");
    let n = r.count();
    assert!(wait_for(S(200), || r.count() >= n + 2).await);
    let gaps = r.gaps();
    assert!(within(*gaps.last().unwrap(), 54_000, 66_100), "background gap {gaps:?}");
    h.set_activity(Activity::Idle);
    let n = r.count();
    assert!(wait_for(S(1_000), || r.count() >= n + 2).await);
    let gaps = r.gaps();
    assert!(within(*gaps.last().unwrap(), 270_000, 330_100), "idle gap {gaps:?}");
    // Back to focused: the long idle wait is re-evaluated at once, not served out.
    h.set_activity(Activity::Focused);
    h.set_focus("mail");
    let n = r.count();
    assert!(wait_for(S(12), || r.count() > n).await);
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn poke_wakes_matching_resources_only() {
    let a = FakeResource::new("mail.folder:a", "mail", Cadence::secs(300, 300, 300));
    let b = FakeResource::new("chat:b", "chat", Cadence::secs(300, 300, 300));
    let (h, _m) = start(vec![res(&a), res(&b)]).await;
    assert!(wait_for(S(1), || a.count() == 1 && b.count() == 1).await);
    advance(S(5)).await;
    h.poke("mail.folder");
    assert!(wait_for(S(1), || a.count() == 2).await);
    assert_eq!(b.count(), 1);
    h.poke("");
    assert!(wait_for(S(1), || a.count() == 3 && b.count() == 2).await);
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn poke_of_unknown_key_rediscovers_and_new_resources_start() {
    let a = FakeResource::new("chats", "chat", Cadence::secs(300, 300, 300));
    let (h, m) = start(vec![res(&a)]).await;
    assert!(wait_for(S(1), || a.count() == 1).await);
    let before = m.discovers();
    let opened = FakeResource::new("chat:new", "chat", Cadence::secs(3, 30, 120));
    m.set(vec![res(&a), res(&opened)]);
    h.poke("chat:new");
    assert!(wait_for(S(1), || opened.count() == 1).await);
    assert!(m.discovers() > before);
    assert_eq!(h.status().resources, 2);
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn rediscover_outcome_reruns_the_module_discovery_now() {
    // The folder list reports a new folder: its module rediscovers at once, not in 10 minutes.
    let folders = FakeResource::new("mail.folders", "mail", Cadence::secs(600, 600, 600))
        .script([Step::Ok(Outcome::CaughtUp), Step::Ok(Outcome::Rediscover)]);
    let (h, m) = start(vec![res(&folders)]).await;
    assert!(wait_for(S(1), || folders.count() == 1).await);
    let before = m.discovers();
    let added = FakeResource::new("mail.folder:new", "mail", Cadence::secs(600, 600, 600));
    m.set(vec![res(&folders), res(&added)]);
    h.poke("mail.folders");
    assert!(wait_for(S(2), || added.count() == 1).await, "new folder synced without waiting");
    assert!(m.discovers() > before);
    assert_eq!(h.status().resources, 2);
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn vanished_resources_stop_and_discover_runs_periodically() {
    let a = FakeResource::new("files:a", "files", Cadence::secs(5, 5, 5));
    let b = FakeResource::new("files:b", "files", Cadence::secs(5, 5, 5));
    let (h, m) = start(vec![res(&a), res(&b)]).await;
    assert!(wait_for(S(1), || h.status().resources == 2).await);
    m.set(vec![res(&a)]);
    h.poke("discover:fake");
    assert!(wait_for(S(1), || h.status().resources == 1).await);
    let n = b.count();
    advance(S(30)).await;
    assert_eq!(b.count(), n, "removed resource kept running");
    assert!(a.count() > 2);
    // Periodic rediscovery (10 min) picks b up again.
    m.set(vec![res(&a), res(&b)]);
    assert!(wait_for(S(11 * 60), || h.status().resources == 2).await);
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn errors_back_off_exponentially_then_recover() {
    let r = FakeResource::new("mail.folder:x", "mail", Cadence::secs(1, 1, 1)).script([
        Step::Err(ErrorKind::Remote, None),
        Step::Err(ErrorKind::Remote, None),
        Step::Err(ErrorKind::Remote, None),
    ]);
    let (h, _m) = start(vec![res(&r)]).await;
    assert!(wait_for(S(60), || r.count() >= 4).await);
    let g = r.gaps();
    assert!(within(g[0], 4_500, 5_600), "{g:?}");
    assert!(within(g[1], 9_000, 11_100), "{g:?}");
    assert!(within(g[2], 18_000, 22_100), "{g:?}");
    let st = h.status();
    assert!(!st.online || st.resources_detail[0].last_error.is_none());
    assert!(wait_for(S(2), || h.status().resources_detail[0].last_success.is_some()).await);
    assert_eq!(h.status().resources_detail[0].failures, 0);
    assert!(h.status().online);
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn backoff_is_capped_at_ten_minutes() {
    let r = FakeResource::new("k", "mail", Cadence::secs(1, 1, 1))
        .script(std::iter::repeat_n(Step::Err(ErrorKind::Remote, None), 12));
    let (h, _m) = start(vec![res(&r)]).await;
    assert!(wait_for(S(3 * 3600), || r.count() >= 12).await);
    for g in r.gaps() {
        assert!(g <= Duration::from_millis(660_100), "gap {g:?} over the cap");
    }
    assert!(r.gaps().iter().any(|g| *g >= S(540)));
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn panicking_pass_does_not_kill_the_engine() {
    let bad = FakeResource::new("bad", "mail", Cadence::secs(1, 1, 1)).script([Step::Panic, Step::Panic]);
    let good = FakeResource::new("good", "mail", Cadence::secs(1, 1, 1));
    let (h, _m) = start(vec![res(&bad), res(&good)]).await;
    assert!(wait_for(S(1), || bad.count() == 1).await);
    let st = h.status();
    let detail = st.resources_detail.iter().find(|d| d.key == "bad").unwrap();
    assert_eq!(detail.last_error.as_deref(), Some("bug"));
    assert!(wait_for(S(30), || bad.count() >= 3).await, "panicking resource was not restarted");
    assert!(good.count() >= 5);
    assert_eq!(h.status().resources, 2);
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn panicking_discover_is_retried() {
    let r = FakeResource::new("k", "mail", Cadence::secs(60, 60, 60));
    let (c, _t) = quiet_ctx().await;
    let m = FakeModule::new("fake", vec![res(&r)]);
    m.discover_script.lock().unwrap().push_back(Step::Panic);
    let h = Engine::start(c, modules(&m));
    assert!(wait_for(S(30), || r.count() == 1).await);
    assert!(m.discovers() >= 2);
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn throttling_honours_retry_after_even_when_poked() {
    let r = FakeResource::new("chat:t", "chat", Cadence::secs(1, 1, 1))
        .script([Step::Err(ErrorKind::Throttled, Some(S(30)))]);
    let (h, _m) = start(vec![res(&r)]).await;
    assert!(wait_for(S(1), || r.count() == 1).await);
    advance(S(5)).await;
    h.poke("chat:t");
    advance(S(20)).await;
    assert_eq!(r.count(), 1, "poke cut a Retry-After wait short");
    assert!(wait_for(S(10), || r.count() == 2).await);
    let g = r.gaps()[0];
    assert!(within(g, 30_000, 30_200), "{g:?}");
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn more_reruns_quickly_for_backfill() {
    let r = FakeResource::new("mail.folder:b", "mail", Cadence::secs(600, 600, 600))
        .script(std::iter::repeat_n(Step::Ok(Outcome::More), 6));
    let (h, _m) = start(vec![res(&r)]).await;
    assert!(wait_for(S(10), || r.count() >= 7).await);
    for g in r.gaps() {
        assert!(within(g, 200, 1_100), "{g:?}");
    }
    advance(S(60)).await;
    assert_eq!(r.count(), 7, "caught-up resource should wait for its cadence");
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn concurrent_passes_are_capped() {
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let max = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut all = Vec::new();
    let mut list: Vec<Arc<dyn Resource>> = Vec::new();
    for i in 0..10 {
        let mut r = FakeResource::new(&format!("r{i}"), "mail", Cadence::secs(600, 600, 600));
        let rm = Arc::get_mut(&mut r).unwrap();
        rm.active = Arc::clone(&active);
        rm.max_active = Arc::clone(&max);
        let r = r.script([Step::Slow(S(5))]);
        list.push(res(&r));
        all.push(r);
    }
    let (c, _t) = quiet_ctx().await;
    let m = FakeModule::new("fake", list);
    let h = Engine::start_with(c, modules(&m), EngineConfig { max_concurrent_passes: 3, ..EngineConfig::default() });
    assert!(wait_for(S(60), || all.iter().all(|r| r.count() == 1)).await);
    advance(S(10)).await;
    assert_eq!(max.load(std::sync::atomic::Ordering::SeqCst), 3);
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn auth_error_pauses_everything_until_resume() {
    let a = FakeResource::new("a", "mail", Cadence::secs(2, 2, 2)).script([Step::Ok(Outcome::CaughtUp)]);
    let b = FakeResource::new("b", "mail", Cadence::secs(2, 2, 2));
    let (h, _m) = start(vec![res(&a), res(&b)]).await;
    assert!(wait_for(S(1), || a.count() == 1 && b.count() == 1).await);
    a.script([Step::Err(ErrorKind::Auth, None)]);
    assert!(wait_for(S(5), || h.status().paused_for_auth).await);
    let (na, nb) = (a.count(), b.count());
    advance(S(60)).await;
    assert_eq!((a.count(), b.count()), (na, nb), "tasks ran while paused for auth");
    h.poke("");
    advance(S(5)).await;
    assert_eq!(a.count(), na);
    h.resume();
    assert!(!h.status().paused_for_auth);
    assert!(wait_for(S(1), || a.count() > na).await, "the failed resource reruns at once on resume");
    assert!(wait_for(S(3), || b.count() > nb).await);
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn user_pause_and_resume() {
    let a = FakeResource::new("a", "mail", Cadence::secs(2, 2, 2));
    let (h, _m) = start(vec![res(&a)]).await;
    assert!(wait_for(S(1), || a.count() == 1).await);
    h.pause();
    assert!(h.status().paused);
    let n = a.count();
    advance(S(30)).await;
    assert_eq!(a.count(), n);
    h.resume();
    assert!(wait_for(S(3), || a.count() > n).await);
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn stop_ends_all_resource_tasks() {
    let a = FakeResource::new("a", "mail", Cadence::secs(1, 1, 1));
    let (h, _m) = start(vec![res(&a)]).await;
    assert!(wait_for(S(1), || a.count() == 1).await);
    h.stop();
    assert!(h.is_stopped());
    let n = a.count();
    advance(S(30)).await;
    assert_eq!(a.count(), n);
    assert_eq!(h.status().resources, 0);
}

#[tokio::test(start_paused = true)]
async fn hostile_resource_keys_are_ignored() {
    let ok = FakeResource::new("ok", "mail", Cadence::secs(60, 60, 60));
    let empty = FakeResource::new("", "mail", Cadence::secs(60, 60, 60));
    let ctl = FakeResource::new("bad\u{0}key", "mail", Cadence::secs(60, 60, 60));
    let long = FakeResource::new(&"x".repeat(5_000), "mail", Cadence::secs(60, 60, 60));
    let dup = FakeResource::new("ok", "mail", Cadence::secs(60, 60, 60));
    let zero = FakeResource::new("zero", "mail", Cadence::secs(0, 0, 0));
    let (h, _m) = start(vec![res(&ok), res(&empty), res(&ctl), res(&long), res(&dup), res(&zero)]).await;
    assert!(wait_for(S(1), || ok.count() == 1).await);
    advance(S(10)).await;
    assert_eq!(h.status().resources, 2);
    assert_eq!(empty.count() + ctl.count() + long.count() + dup.count(), 0);
    // A zero cadence is clamped to one second, not a busy loop.
    assert!(zero.count() <= 13, "zero cadence ran {} times", zero.count());
    h.stop();
}

// ---------------------------------------------------------------------------------------------
// Outbox.
// ---------------------------------------------------------------------------------------------

async fn enqueue(c: &Ctx, kind: &str, resource: &str, n: i64) -> OutboxId {
    c.store
        .outbox_enqueue(NewOutboxEntry {
            kind: kind.to_owned(),
            resource: resource.to_owned(),
            local_id: None,
            payload: json!({ "n": n }),
        })
        .await
        .unwrap()
}

fn ns(v: &[Value]) -> Vec<i64> {
    v.iter().filter_map(|p| p.get("n").and_then(Value::as_i64)).collect()
}

#[tokio::test(start_paused = true)]
async fn outbox_replays_in_order_and_blocks_behind_a_failure() {
    let (c, _t) = quiet_ctx().await;
    let hd = FakeHandler::new("chat.send").script([Step::Err(ErrorKind::Permanent, None)]);
    let m = FakeModule::with_handlers("fake", Vec::new(), vec![hd.clone()]);
    let e1 = enqueue(&c, "chat.send", "chat:1", 1).await;
    enqueue(&c, "chat.send", "chat:1", 2).await;
    enqueue(&c, "chat.send", "chat:2", 3).await;
    let store = c.store.clone();
    let h = Engine::start(c, modules(&m));
    assert!(wait_for(S(5), || ns(&hd.applied()) == vec![1, 3]).await, "{:?}", hd.applied());
    advance(S(60)).await;
    assert_eq!(ns(&hd.applied()), vec![1, 3], "entry 2 overtook its failed predecessor");
    let st = h.refresh_status().await;
    assert_eq!((st.failed_writes, st.pending_writes), (1, 1));
    let failed = store.outbox_failed(10).await.unwrap().items;
    assert_eq!(failed.len(), 1);
    assert!(failed[0].last_error.as_deref().unwrap().starts_with("permanent"));
    h.retry_failed(e1).await.unwrap();
    assert!(wait_for(S(5), || ns(&hd.applied()) == vec![1, 3, 1, 2]).await, "{:?}", hd.applied());
    assert!(wait_for(S(5), || h.status().pending_writes == 0 && h.status().failed_writes == 0).await);
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn outbox_wakes_on_outbox_changed() {
    let (c, _t) = quiet_ctx().await;
    let hd = FakeHandler::new("mail.send");
    let m = FakeModule::with_handlers("fake", Vec::new(), vec![hd.clone()]);
    let c2 = c.clone();
    let h = Engine::start(c, modules(&m));
    advance(S(1)).await;
    enqueue(&c2, "mail.send", "message:1", 7).await;
    h.outbox_changed();
    assert!(wait_for(Duration::from_millis(500), || ns(&hd.applied()) == vec![7]).await);
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn unknown_kind_fails_permanently() {
    let (c, _t) = quiet_ctx().await;
    let m = FakeModule::new("fake", Vec::new());
    enqueue(&c, "nobody.handles", "x:1", 1).await;
    let store = c.store.clone();
    let h = Engine::start(c, modules(&m));
    assert!(wait_for(S(5), || h.status().failed_writes == 1).await);
    let failed = store.outbox_failed(10).await.unwrap().items;
    assert_eq!(failed[0].attempts, 1);
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn handler_panic_fails_the_entry_and_replay_continues() {
    let (c, _t) = quiet_ctx().await;
    let hd = FakeHandler::new("chat.send").script([Step::Panic]);
    let m = FakeModule::with_handlers("fake", Vec::new(), vec![hd.clone()]);
    enqueue(&c, "chat.send", "chat:1", 1).await;
    enqueue(&c, "chat.send", "chat:2", 2).await;
    let h = Engine::start(c, modules(&m));
    assert!(wait_for(S(5), || ns(&hd.applied()) == vec![1, 2]).await);
    assert!(wait_for(S(5), || h.status().failed_writes == 1).await);
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn throttled_write_waits_for_retry_after_then_succeeds() {
    let (c, _t) = quiet_ctx().await;
    let hd = FakeHandler::new("chat.send").script([Step::Err(ErrorKind::Throttled, Some(S(120)))]);
    let m = FakeModule::with_handlers("fake", Vec::new(), vec![hd.clone()]);
    enqueue(&c, "chat.send", "chat:1", 1).await;
    let h = Engine::start(c, modules(&m));
    assert!(wait_for(S(1), || hd.applied().len() == 1).await);
    h.outbox_changed();
    advance(S(100)).await;
    assert_eq!(hd.applied().len(), 1, "retried before Retry-After");
    assert!(wait_for(S(90), || hd.applied().len() == 2).await);
    assert!(wait_for(S(2), || h.status().pending_writes == 0).await);
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn transient_write_failure_retries_with_backoff() {
    let (c, _t) = quiet_ctx().await;
    let hd = FakeHandler::new("chat.send").script([Step::Err(ErrorKind::Remote, None)]);
    let m = FakeModule::with_handlers("fake", Vec::new(), vec![hd.clone()]);
    enqueue(&c, "chat.send", "chat:1", 1).await;
    let h = Engine::start(c, modules(&m));
    assert!(wait_for(S(10), || hd.applied().len() == 2).await);
    assert_eq!(h.refresh_status().await.failed_writes, 0);
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn auth_failure_requeues_the_write_and_pauses() {
    let (c, _t) = quiet_ctx().await;
    let hd = FakeHandler::new("chat.send").script([Step::Err(ErrorKind::Auth, None)]);
    let m = FakeModule::with_handlers("fake", Vec::new(), vec![hd.clone()]);
    enqueue(&c, "chat.send", "chat:1", 1).await;
    let h = Engine::start(c, modules(&m));
    assert!(wait_for(S(2), || h.status().paused_for_auth).await);
    advance(S(30)).await;
    assert_eq!(hd.applied().len(), 1);
    assert_eq!(h.refresh_status().await.failed_writes, 0);
    h.resume();
    assert!(wait_for(S(2), || hd.applied().len() == 2).await);
    assert!(wait_for(S(2), || h.status().pending_writes == 0).await);
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn discard_reverts_then_removes() {
    let (c, _t) = quiet_ctx().await;
    let hd = FakeHandler::new("mail.flag").script([Step::Err(ErrorKind::Invalid, None)]);
    let m = FakeModule::with_handlers("fake", Vec::new(), vec![hd.clone()]);
    let id = enqueue(&c, "mail.flag", "message:1", 5).await;
    let h = Engine::start(c, modules(&m));
    assert!(wait_for(S(5), || h.status().failed_writes == 1).await);
    let out = h.discard_failed(id).await.unwrap();
    assert!(out.discarded && out.reverted);
    assert_eq!(ns(&hd.reverted()), vec![5]);
    let st = h.refresh_status().await;
    assert_eq!((st.failed_writes, st.pending_writes), (0, 0));
    // Discarding again is a no-op.
    let again = h.discard_failed(id).await.unwrap();
    assert!(!again.discarded);
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn successful_write_pokes_its_resource_and_listed_keys() {
    let (c, _t) = quiet_ctx().await;
    let chat = FakeResource::new("chat:1", "chat", Cadence::secs(600, 600, 600));
    let list = FakeResource::new("chats", "chat", Cadence::secs(600, 600, 600));
    let hd = FakeHandler::returning("chat.send", json!({"poke": ["chats"]}));
    let m = FakeModule::with_handlers("fake", vec![res(&chat), res(&list)], vec![hd.clone()]);
    let c2 = c.clone();
    let h = Engine::start(c, modules(&m));
    assert!(wait_for(S(1), || chat.count() == 1 && list.count() == 1).await);
    advance(S(5)).await;
    enqueue(&c2, "chat.send", "chat:1", 1).await;
    h.outbox_changed();
    assert!(wait_for(S(1), || chat.count() == 2 && list.count() == 2).await);
    h.stop();
}

// ---------------------------------------------------------------------------------------------
// Presence keep-alive.
// ---------------------------------------------------------------------------------------------

fn presence_calls(t: &FixtureTransport, path: &str) -> Vec<Value> {
    t.calls()
        .into_iter()
        .filter(|r| r.url.path().ends_with(path))
        .map(|r| serde_json::from_slice(&r.body).unwrap_or(Value::Null))
        .collect()
}

fn keepalive_transport() -> FixtureTransport {
    let t = FixtureTransport::new();
    t.on(Method::Post, "/me/presence/setPresence", FixtureResponse::status(200));
    t.on(Method::Post, "/me/presence/clearPresence", FixtureResponse::status(200));
    t
}

#[tokio::test(start_paused = true)]
async fn keepalive_refreshes_every_four_minutes_and_goes_away_when_idle() {
    let t = keepalive_transport();
    let h = Engine::start(ctx(&t), Vec::new());
    assert!(wait_for(S(1), || presence_calls(&t, "/setPresence").len() == 1).await);
    let first = &presence_calls(&t, "/setPresence")[0];
    assert_eq!(first["sessionId"], common::CLIENT_ID);
    assert_eq!(first["availability"], "Available");
    assert_eq!(first["activity"], "Available");
    assert_eq!(first["expirationDuration"], "PT1H");
    assert!(h.status().keepalive_active);
    advance(S(235)).await;
    assert_eq!(presence_calls(&t, "/setPresence").len(), 1);
    advance(S(10)).await;
    assert_eq!(presence_calls(&t, "/setPresence").len(), 2);
    // Background keeps Available without an extra call.
    h.set_activity(Activity::Background);
    advance(S(1)).await;
    assert_eq!(presence_calls(&t, "/setPresence").len(), 2);
    h.set_activity(Activity::Idle);
    assert!(wait_for(S(1), || presence_calls(&t, "/setPresence").len() == 3).await);
    let away = &presence_calls(&t, "/setPresence")[2];
    assert_eq!(away["availability"], "Away");
    assert_eq!(away["activity"], "Away");
    advance(S(241)).await;
    assert_eq!(presence_calls(&t, "/setPresence").len(), 4);
    h.set_activity(Activity::Focused);
    assert!(wait_for(S(1), || presence_calls(&t, "/setPresence").len() == 5).await);
    assert_eq!(presence_calls(&t, "/setPresence")[4]["availability"], "Available");
    h.stop();
    assert!(wait_for(S(1), || presence_calls(&t, "/clearPresence").len() == 1).await);
    assert_eq!(presence_calls(&t, "/clearPresence")[0]["sessionId"], common::CLIENT_ID);
}

#[tokio::test(start_paused = true)]
async fn keepalive_toggle_clears_and_restores() {
    let t = keepalive_transport();
    let h = Engine::start(ctx(&t), Vec::new());
    assert!(wait_for(S(1), || presence_calls(&t, "/setPresence").len() == 1).await);
    h.set_keepalive(false);
    assert!(wait_for(S(1), || presence_calls(&t, "/clearPresence").len() == 1).await);
    assert!(!h.status().keepalive_active);
    advance(S(600)).await;
    assert_eq!(presence_calls(&t, "/setPresence").len(), 1);
    h.set_keepalive(true);
    assert!(wait_for(S(1), || presence_calls(&t, "/setPresence").len() == 2).await);
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn keepalive_respects_the_stored_setting() {
    let t = keepalive_transport();
    let c = ctx(&t);
    c.store.set_setting(quorum_sync::KEEPALIVE_SETTING.to_owned(), false).await.unwrap();
    let h = Engine::start(c, Vec::new());
    advance(S(600)).await;
    assert!(presence_calls(&t, "/setPresence").is_empty());
    assert!(!h.status().keepalive);
    h.stop();
}

#[tokio::test(start_paused = true)]
async fn keepalive_backs_off_on_errors_and_pauses_on_auth() {
    let t = FixtureTransport::new();
    t.on(Method::Post, "/me/presence/setPresence", FixtureResponse::status(500));
    let h = Engine::start(ctx(&t), Vec::new());
    advance(S(240)).await;
    let n = presence_calls(&t, "/setPresence").len();
    // Graph's own retries multiply calls; the keep-alive's backoff must still bound them.
    assert!(n > 1 && n < 200, "{n} calls");
    h.stop();

    let t = FixtureTransport::new();
    t.on(Method::Post, "/me/presence/setPresence", FixtureResponse::status(401));
    let h = Engine::start(ctx(&t), Vec::new());
    assert!(wait_for(S(5), || h.status().paused_for_auth).await);
    let n = t.call_count();
    advance(S(600)).await;
    assert_eq!(t.call_count(), n, "keep-alive kept calling while paused for sign-in");
    h.stop();
}

#[tokio::test]
async fn status_serialises_with_details() {
    let (c, _t) = quiet_ctx().await;
    let r = FakeResource::new("mail.folder:inbox", "mail", Cadence::secs(15, 60, 300));
    let m = FakeModule::new("fake", vec![res(&r)]);
    let h = Engine::start(c, modules(&m));
    assert!(wait_for(S(2), || h.status().resources_detail.iter().any(|d| d.last_success.is_some())).await);
    let v = serde_json::to_value(h.status()).unwrap();
    assert_eq!(v["resources"], 1);
    assert_eq!(v["activity"], "focused");
    assert_eq!(v["resources_detail"][0]["key"], "mail.folder:inbox");
    assert_eq!(v["resources_detail"][0]["module"], "fake");
    h.stop();
}

#[test]
fn start_outside_a_runtime_is_inert() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let (c, _t) = rt.block_on(quiet_ctx());
    let h = Engine::start(c, Vec::new());
    h.poke("x");
    h.set_focus("mail");
    assert_eq!(h.status().focus.as_deref(), Some("mail"));
    h.stop();
}
