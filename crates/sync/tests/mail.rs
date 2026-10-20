//! Mail sync from fixture Graph: first page, delta (upsert, `@removed`, moves), backfill, delta
//! expiry, body prefetch, folders and roles, and every outbox handler's request shape.

use std::sync::Arc;

use quorum_graph::api::mail::message_from_json;
use quorum_graph::{
    Client, FakeClock, FixtureResponse, FixtureTransport, HttpRequest, Method, RateLimit, ResourceFamily, StaticToken,
};
use quorum_model::{MailFolderId, MessageBody, MessageId, Sensitive, WellKnownFolder};
use quorum_store::{DeltaState, Draft, DraftKind, Store};
use quorum_sync::mail::payload::{
    DeletePayload, Destination, DraftSyncPayload, MovePayload, Previous, SendPayload, UpdatePayload,
};
use quorum_sync::mail::{
    Compose, ComposeMode, FolderMessages, FoldersResource, MailModule, WELL_KNOWN_SETTING, Window, folder_key,
};
use quorum_sync::{Ctx, ErrorKind, Module, OutboxHandler, Outcome, Resource};
use serde_json::{Value, json};

const G: &str = "https://graph.microsoft.com/v1.0";

fn ctx(t: &FixtureTransport) -> Ctx {
    let mut b = Client::builder(StaticToken::new("test-token"));
    for f in ResourceFamily::ALL {
        b = b.rate(f, RateLimit::new(10_000.0, 10_000).unwrap());
    }
    let graph = b.transport(t.clone()).clock(FakeClock::new()).jitter_seed(1).build().unwrap();
    Ctx { graph, store: Store::open_in_memory().unwrap(), client_id: "client".into() }
}

fn wire(id: &str, folder: &str, received: &str, read: bool) -> Value {
    json!({
        "id": id,
        "conversationId": format!("conv-{id}"),
        "parentFolderId": folder,
        "subject": format!("Synthetic {id}"),
        "from": { "emailAddress": { "name": "Avery Example", "address": "avery@example.com" } },
        "toRecipients": [{ "emailAddress": { "name": "Test User", "address": "test.user@example.com" } }],
        "receivedDateTime": received,
        "bodyPreview": "Synthetic preview.",
        "isRead": read,
        "flag": { "flagStatus": "notFlagged" },
        "inferenceClassification": "focused"
    })
}

fn removed(id: &str) -> Value {
    json!({ "id": id, "@removed": { "reason": "deleted" } })
}

async fn seed(c: &Ctx, items: &[Value]) {
    let msgs: Vec<_> = items.iter().map(|v| message_from_json(v.clone()).unwrap()).collect();
    c.store.write(move |tx| tx.upsert_messages(&msgs)).await.unwrap();
}

fn mid(s: &str) -> MessageId {
    MessageId::new(s).unwrap()
}

fn fid(s: &str) -> MailFolderId {
    MailFolderId::new(s).unwrap()
}

async fn has(c: &Ctx, id: &str) -> bool {
    c.store.message(mid(id)).await.unwrap().is_some()
}

fn inbox() -> FolderMessages {
    FolderMessages::new(fid("F-inbox"), Some(WellKnownFolder::Inbox))
}

async fn seed_state(c: &Ctx, delta_link: Option<&str>, since: &str, complete: bool) {
    let since = since.parse().unwrap();
    let mut st = DeltaState::new(folder_key(&fid("F-inbox")));
    st.delta_link = delta_link.map(str::to_owned);
    st.backfill_cursor = Some(serde_json::to_string(&Window { since, before: since }).unwrap());
    st.backfill_complete = complete;
    c.store.set_delta_state(st).await.unwrap();
}

fn query_of(r: &HttpRequest, key: &str) -> Option<String> {
    r.url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned())
}

fn body_json(r: &HttpRequest) -> Value {
    serde_json::from_slice(&r.body).unwrap_or(Value::Null)
}

#[tokio::test]
async fn first_pass_stores_the_newest_page() {
    let t = FixtureTransport::new();
    t.on(
        Method::Get,
        "/me/mailFolders/F-inbox/messages",
        FixtureResponse::json(
            200,
            &json!({ "value": [
                wire("m1", "F-inbox", "2026-10-16T09:00:00Z", false),
                wire("m2", "F-inbox", "2026-10-15T09:00:00Z", true),
            ]}),
        ),
    );
    let c = ctx(&t);
    let r = inbox();
    assert_eq!(r.run(&c).await.unwrap(), Outcome::More);
    assert!(has(&c, "m1").await && has(&c, "m2").await);
    let call = t.calls().into_iter().next().unwrap();
    assert_eq!(query_of(&call, "$top").as_deref(), Some("50"));
    assert_eq!(query_of(&call, "$orderby").as_deref(), Some("receivedDateTime desc"));
    let st = c.store.delta_state(r.key()).await.unwrap().unwrap();
    assert!(Window::parse(st.backfill_cursor.as_deref()).is_some());
    assert!(st.delta_link.is_none());
    assert_eq!(r.cadence(), quorum_sync::Cadence::secs(15, 60, 300));
}

#[tokio::test]
async fn delta_upserts_removes_keeps_moved_and_backfills_older_pages() {
    let t = FixtureTransport::new();
    t.on(
        Method::Get,
        "/me/mailFolders/F-inbox/messages/delta",
        FixtureResponse::json(
            200,
            &json!({
                "value": [wire("m3", "F-inbox", "2026-10-17T09:00:00Z", false)],
                "@odata.nextLink": format!("{G}/me/mailFolders/F-inbox/messages/delta?$skiptoken=p2"),
            }),
        ),
    );
    t.add(
        quorum_graph::Route::new(Method::Get, "/me/mailFolders/F-inbox/messages/delta")
            .query("$skiptoken", "p2")
            .respond(FixtureResponse::json(
                200,
                &json!({
                    "value": [wire("m1", "F-inbox", "2026-10-16T09:00:00Z", true), removed("m2"), removed("m9")],
                    "@odata.deltaLink": format!("{G}/me/mailFolders/F-inbox/messages/delta?$deltatoken=t1"),
                }),
            )),
    );
    t.add(
        quorum_graph::Route::new(Method::Get, "/me/mailFolders/F-inbox/messages/delta")
            .query("$deltatoken", "t1")
            .respond(FixtureResponse::json(
                200,
                &json!({
                    "value": [],
                    "@odata.deltaLink": format!("{G}/me/mailFolders/F-inbox/messages/delta?$deltatoken=t1"),
                }),
            )),
    );
    t.add(
        quorum_graph::Route::new(Method::Get, "/me/mailFolders/F-inbox/messages")
            .query("$filter", "receivedDateTime lt 2026-09-01T00:00:00Z")
            .respond(FixtureResponse::json(
                200,
                &json!({ "value": [wire("m-old", "F-inbox", "2026-08-01T00:00:00Z", true)] }),
            )),
    );
    t.add(
        quorum_graph::Route::new(Method::Get, "/me/mailFolders/F-inbox/messages")
            .query("$filter", "receivedDateTime lt 2026-08-01T00:00:00Z")
            .respond(FixtureResponse::json(200, &json!({ "value": [] }))),
    );
    let c = ctx(&t);
    c.store.set_setting("mail.prefetch_days".into(), 0u32).await.unwrap();
    seed(
        &c,
        &[
            wire("m1", "F-inbox", "2026-10-16T09:00:00Z", false),
            wire("m2", "F-inbox", "2026-10-15T09:00:00Z", false),
            // Already moved to another folder by that folder's delta: the inbox's @removed must not delete it.
            wire("m9", "F-other", "2026-10-14T09:00:00Z", false),
        ],
    )
    .await;
    seed_state(&c, None, "2026-09-01T00:00:00Z", false).await;
    let r = inbox();
    assert_eq!(r.run(&c).await.unwrap(), Outcome::More, "backfill not finished yet");
    assert!(has(&c, "m3").await);
    assert!(c.store.message(mid("m1")).await.unwrap().unwrap().is_read);
    assert!(!has(&c, "m2").await, "@removed deletes");
    assert!(has(&c, "m9").await, "a message moved elsewhere is kept");
    assert!(has(&c, "m-old").await, "backfill stored the older page");
    let first_delta = t.calls().into_iter().find(|c| c.url.path().ends_with("/delta")).unwrap();
    assert_eq!(query_of(&first_delta, "$filter").as_deref(), Some("receivedDateTime ge 2026-09-01T00:00:00Z"));
    assert!(first_delta.header("Prefer").unwrap_or("").contains("ImmutableId"));

    assert_eq!(r.run(&c).await.unwrap(), Outcome::CaughtUp);
    let st = c.store.delta_state(r.key()).await.unwrap().unwrap();
    assert!(st.delta_link.unwrap().contains("t1"));
    assert!(st.backfill_complete);
}

#[tokio::test]
async fn expired_delta_resyncs_one_folder_and_keeps_bodies() {
    let t = FixtureTransport::new();
    t.add(
        quorum_graph::Route::new(Method::Get, "/me/mailFolders/F-inbox/messages/delta")
            .query("$deltatoken", "expired")
            .respond(FixtureResponse::file(410, "delta-expired.json").unwrap()),
    );
    t.on(
        Method::Get,
        "/me/mailFolders/F-inbox/messages/delta",
        FixtureResponse::json(
            200,
            &json!({
                "value": [wire("m1", "F-inbox", "2026-10-16T09:00:00Z", true)],
                "@odata.deltaLink": format!("{G}/me/mailFolders/F-inbox/messages/delta?$deltatoken=t2"),
            }),
        ),
    );
    let c = ctx(&t);
    c.store.set_setting("mail.prefetch_days".into(), 0u32).await.unwrap();
    seed(
        &c,
        &[
            wire("m1", "F-inbox", "2026-10-16T09:00:00Z", false),
            wire("m5", "F-inbox", "2026-10-10T09:00:00Z", false),
            wire("m-old", "F-inbox", "2026-08-10T09:00:00Z", false),
        ],
    )
    .await;
    let body = MessageBody { content_type: quorum_model::BodyType::Html, content: Sensitive::new("<p>kept</p>") };
    c.store.put_message_body(mid("m1"), body).await.unwrap();
    seed_state(
        &c,
        Some(&format!("{G}/me/mailFolders/F-inbox/messages/delta?$deltatoken=expired")),
        "2026-09-01T00:00:00Z",
        true,
    )
    .await;
    assert_eq!(inbox().run(&c).await.unwrap(), Outcome::CaughtUp);
    assert!(c.store.message_body(mid("m1")).await.unwrap().is_some(), "cached body kept");
    assert!(c.store.message(mid("m1")).await.unwrap().unwrap().is_read, "merged by id");
    assert!(!has(&c, "m5").await, "gone from the server inside the window");
    assert!(has(&c, "m-old").await, "outside the delta window: kept");
    let st = c.store.delta_state(inbox().key()).await.unwrap().unwrap();
    assert!(st.delta_link.unwrap().contains("t2"));
}

#[tokio::test]
async fn recent_bodies_are_prefetched() {
    let t = FixtureTransport::new();
    t.add(
        quorum_graph::Route::new(Method::Get, "/me/mailFolders/F-inbox/messages/delta")
            .query("$deltatoken", "t1")
            .respond(FixtureResponse::json(
                200,
                &json!({
                    "value": [],
                    "@odata.deltaLink": format!("{G}/me/mailFolders/F-inbox/messages/delta?$deltatoken=t1"),
                }),
            )),
    );
    let now = jiff::Timestamp::now().to_string();
    let mut full = wire("m1", "F-inbox", &now, false);
    full["body"] = json!({ "contentType": "html", "content": "<p>Synthetic <b>body</b></p>" });
    t.on(Method::Get, "/me/messages/m1", FixtureResponse::json(200, &full));
    t.on(
        Method::Get,
        "/me/messages/m-gone",
        FixtureResponse::json(404, &json!({ "error": { "code": "ErrorItemNotFound", "message": "x" } })),
    );
    let c = ctx(&t);
    seed(&c, &[wire("m1", "F-inbox", &now, false), wire("m-gone", "F-inbox", &now, false)]).await;
    seed_state(
        &c,
        Some(&format!("{G}/me/mailFolders/F-inbox/messages/delta?$deltatoken=t1")),
        "2026-09-01T00:00:00Z",
        true,
    )
    .await;
    inbox().run(&c).await.unwrap();
    let b = c.store.message_body(mid("m1")).await.unwrap().unwrap();
    assert!(b.content.as_str().contains("Synthetic"));
    assert!(!has(&c, "m-gone").await, "a message Graph no longer has is dropped");
}

#[tokio::test]
async fn folders_roles_and_discovery() {
    let t = FixtureTransport::new();
    t.on(
        Method::Get,
        "/me/mailFolders/inbox",
        FixtureResponse::json(200, &json!({ "id": "F-inbox", "displayName": "Inbox" })),
    );
    t.on(
        Method::Get,
        "/me/mailFolders/archive",
        FixtureResponse::json(200, &json!({ "id": "F-archive", "displayName": "Archive" })),
    );
    t.on(
        Method::Get,
        "/me/mailFolders/delta",
        FixtureResponse::json(
            200,
            &json!({
                "value": [
                    { "id": "F-inbox", "displayName": "Inbox", "parentFolderId": "F-root" },
                    { "id": "F-archive", "displayName": "Archive", "parentFolderId": "F-root" },
                    { "id": "F-sub", "displayName": "Projects", "parentFolderId": "F-inbox" },
                    { "id": "F-hidden", "displayName": "Hidden", "isHidden": true },
                    removed("F-old"),
                ],
                "@odata.deltaLink": format!("{G}/me/mailFolders/delta?$deltatoken=ft1"),
            }),
        ),
    );
    let c = ctx(&t);
    // New folders: the module rediscovers so they sync at once.
    assert_eq!(FoldersResource::new().run(&c).await.unwrap(), Outcome::Rediscover);
    let roles: std::collections::BTreeMap<String, MailFolderId> =
        c.store.setting(WELL_KNOWN_SETTING.into()).await.unwrap().unwrap();
    assert_eq!(roles.get("inbox"), Some(&fid("F-inbox")));
    assert_eq!(roles.get("archive"), Some(&fid("F-archive")));
    assert!(!roles.contains_key("junkemail"), "missing folders are left out");
    let keys: Vec<String> = MailModule::new().discover(&c).await.unwrap().iter().map(|r| r.key()).collect();
    assert!(keys.contains(&"mail.folders".to_owned()));
    assert!(keys.contains(&folder_key(&fid("F-inbox"))));
    assert!(keys.contains(&folder_key(&fid("F-sub"))));
    assert!(!keys.contains(&folder_key(&fid("F-hidden"))), "hidden folders are not synced");
    let st = c.store.delta_state("mail.folders".into()).await.unwrap().unwrap();
    assert!(st.delta_link.unwrap().contains("ft1"));
    // Roles are resolved once.
    let n = t.calls().len();
    // The same folders again: nothing to rediscover.
    assert_eq!(FoldersResource::new().run(&c).await.unwrap(), Outcome::CaughtUp);
    assert!(t.calls().iter().skip(n).all(|r| !r.url.path().ends_with("/inbox")));
}

fn handler(kind: &str) -> Arc<dyn OutboxHandler> {
    MailModule::new().handlers().into_iter().find(|h| h.kind() == kind).unwrap()
}

#[tokio::test]
async fn update_and_move_send_the_documented_requests() {
    let t = FixtureTransport::new();
    t.on(
        Method::Patch,
        "/me/messages/m1",
        FixtureResponse::json(200, &wire("m1", "F-inbox", "2026-10-16T09:00:00Z", true)),
    );
    t.on(
        Method::Post,
        "/me/messages/m1/move",
        FixtureResponse::json(201, &wire("m1", "F-archive", "2026-10-16T09:00:00Z", true)),
    );
    t.on(Method::Delete, "/me/messages/m2", FixtureResponse::status(204));
    t.on(
        Method::Delete,
        "/me/messages/m3",
        FixtureResponse::json(404, &json!({ "error": { "code": "ErrorItemNotFound", "message": "x" } })),
    );
    let c = ctx(&t);
    seed(&c, &[wire("m1", "F-inbox", "2026-10-16T09:00:00Z", false)]).await;

    let u = UpdatePayload {
        id: mid("m1"),
        is_read: Some(true),
        flag: Some(quorum_model::FlagStatus::Flagged),
        categories: Some(vec!["Blue".into()]),
        focused: Some(false),
        previous: Previous::default(),
    };
    handler("mail.update").apply(&c, serde_json::to_value(&u).unwrap()).await.unwrap();
    let call = t.calls().pop().unwrap();
    assert_eq!(call.method, Method::Patch);
    assert_eq!(
        body_json(&call),
        json!({ "isRead": true, "flag": { "flagStatus": "flagged" }, "categories": ["Blue"], "inferenceClassification": "other" })
    );

    let m = MovePayload {
        id: mid("m1"),
        destination: Destination::WellKnown("archive".into()),
        previous_folder: Some(fid("F-inbox")),
    };
    handler("mail.move").apply(&c, serde_json::to_value(&m).unwrap()).await.unwrap();
    let call = t.calls().pop().unwrap();
    assert_eq!(body_json(&call), json!({ "destinationId": "archive" }));
    assert_eq!(c.store.message(mid("m1")).await.unwrap().unwrap().parent_folder_id, Some(fid("F-archive")));
    // Discarding a failed move puts it back locally.
    handler("mail.move").revert(&c, serde_json::to_value(&m).unwrap()).await.unwrap();
    assert_eq!(c.store.message(mid("m1")).await.unwrap().unwrap().parent_folder_id, Some(fid("F-inbox")));

    let d = DeletePayload { id: mid("m2"), message: None };
    handler("mail.delete_permanent").apply(&c, serde_json::to_value(&d).unwrap()).await.unwrap();
    let d = DeletePayload { id: mid("m3"), message: None };
    handler("mail.delete_permanent").apply(&c, serde_json::to_value(&d).unwrap()).await.unwrap();
}

async fn save_compose(c: &Ctx, id: &str, compose: &Compose, body: &str) {
    let mut d = Draft::new(id, DraftKind::Mail);
    d.fields = serde_json::to_value(compose).unwrap();
    d.body = Sensitive::new(body);
    c.store.save_draft(d).await.unwrap();
}

fn to(addr: &str) -> Vec<quorum_model::EmailAddress> {
    vec![quorum_model::EmailAddress::new("", addr).unwrap()]
}

#[tokio::test]
async fn new_message_sends_with_send_mail() {
    let t = FixtureTransport::new();
    t.on(Method::Post, "/me/sendMail", FixtureResponse::status(202));
    let c = ctx(&t);
    c.store.put_blob("mail-compose:d1:a1".into(), b"hello".to_vec()).await.unwrap();
    let compose = Compose {
        to: to("test.user@example.com"),
        subject: Sensitive::new("Synthetic hello"),
        attachments: vec![quorum_sync::mail::ComposeAttachment {
            id: "a1".into(),
            name: Sensitive::new("note.txt"),
            content_type: Some("text/plain".into()),
            size: 5,
            blob_key: "mail-compose:d1:a1".into(),
        }],
        sending: true,
        ..Compose::default()
    };
    save_compose(&c, "d1", &compose, "<p>Hi</p>").await;
    let p = SendPayload { draft_id: "d1".into(), compose, body_html: Sensitive::new("<p>Hi</p>") };
    handler("mail.send").apply(&c, serde_json::to_value(&p).unwrap()).await.unwrap();
    let call = t.calls().pop().unwrap();
    assert!(call.url.path().ends_with("/me/sendMail"));
    let body = body_json(&call);
    assert_eq!(body["saveToSentItems"], json!(true));
    assert_eq!(body["message"]["subject"], json!("Synthetic hello"));
    assert_eq!(body["message"]["toRecipients"][0]["emailAddress"]["address"], json!("test.user@example.com"));
    assert_eq!(body["message"]["attachments"][0]["contentBytes"], json!("aGVsbG8="));
    assert!(c.store.draft("d1".into()).await.unwrap().is_none(), "sent drafts are removed");
}

#[tokio::test]
async fn reply_creates_a_response_draft_keeps_the_quote_and_sends_it() {
    let t = FixtureTransport::new();
    t.on(
        Method::Post,
        "/me/messages/src/createReply",
        FixtureResponse::json(201, &wire("srv1", "F-drafts", "2026-10-16T09:00:00Z", true)),
    );
    let mut srv = wire("srv1", "F-drafts", "2026-10-16T09:00:00Z", true);
    srv["body"] = json!({ "contentType": "html", "content": "<html><body><p>quoted original</p></body></html>" });
    t.on(Method::Get, "/me/messages/srv1", FixtureResponse::json(200, &srv));
    t.on(Method::Patch, "/me/messages/srv1", FixtureResponse::json(200, &srv));
    t.on(Method::Post, "/me/messages/srv1/send", FixtureResponse::status(202));
    let c = ctx(&t);
    let compose = Compose {
        mode: ComposeMode::Reply,
        source: Some(mid("src")),
        to: to("avery@example.com"),
        subject: Sensitive::new("RE: Synthetic"),
        sending: true,
        ..Compose::default()
    };
    save_compose(&c, "d2", &compose, "<p>My answer</p>").await;
    let p = SendPayload { draft_id: "d2".into(), compose, body_html: Sensitive::new("<p>My answer</p>") };
    handler("mail.send").apply(&c, serde_json::to_value(&p).unwrap()).await.unwrap();
    let calls = t.calls();
    let paths: Vec<String> = calls.iter().map(|r| format!("{} {}", r.method.as_str(), r.url.path())).collect();
    assert_eq!(
        paths,
        vec![
            "POST /v1.0/me/messages/src/createReply",
            "GET /v1.0/me/messages/srv1",
            "PATCH /v1.0/me/messages/srv1",
            "POST /v1.0/me/messages/srv1/send",
        ]
    );
    let create = body_json(&calls[0]);
    assert!(create["message"].get("body").is_none(), "Graph writes the quote");
    let patch = body_json(&calls[2]);
    let content = patch["body"]["content"].as_str().unwrap();
    let answer = content.find("My answer").unwrap();
    let quote = content.find("quoted original").unwrap();
    assert!(answer < quote, "the user's text goes above the quote");
}

#[tokio::test]
async fn draft_sync_creates_then_updates_the_server_draft() {
    let t = FixtureTransport::new();
    t.on(
        Method::Post,
        "/me/messages",
        FixtureResponse::json(201, &wire("srv9", "F-drafts", "2026-10-16T09:00:00Z", true)),
    );
    t.on(
        Method::Patch,
        "/me/messages/srv9",
        FixtureResponse::json(200, &wire("srv9", "F-drafts", "2026-10-16T09:00:00Z", true)),
    );
    let c = ctx(&t);
    let compose = Compose { to: to("test.user@example.com"), subject: Sensitive::new("Draft"), ..Compose::default() };
    save_compose(&c, "d3", &compose, "<p>v1</p>").await;
    let p = serde_json::to_value(DraftSyncPayload { draft_id: "d3".into() }).unwrap();
    handler("mail.draft_sync").apply(&c, p.clone()).await.unwrap();
    handler("mail.draft_sync").apply(&c, p).await.unwrap();
    let methods: Vec<Method> = t.calls().iter().map(|r| r.method).collect();
    assert_eq!(methods, vec![Method::Post, Method::Patch, Method::Patch], "created once, then patched");
    let d = c.store.draft("d3".into()).await.unwrap().unwrap();
    let saved: Compose = serde_json::from_value(d.fields).unwrap();
    assert_eq!(saved.server_id, Some(mid("srv9")));
    // A draft that is gone or being sent is skipped.
    let gone = serde_json::to_value(DraftSyncPayload { draft_id: "nope".into() }).unwrap();
    handler("mail.draft_sync").apply(&c, gone).await.unwrap();
}

#[tokio::test]
async fn hostile_payloads_fail_permanently_without_panicking() {
    let t = FixtureTransport::new();
    let c = ctx(&t);
    let deep = (0..200).fold(json!(1), |acc, _| json!([acc]));
    let values = [
        Value::Null,
        json!(5),
        json!("x"),
        json!({}),
        json!({ "id": "" }),
        json!({ "id": "a/../b", "destination": { "well_known": "archive" } }),
        json!({ "draft_id": 5 }),
        json!({ "server_id": "\u{0}" }),
        json!({ "setting": { "status": "weird" } }),
        deep,
    ];
    for h in MailModule::new().handlers() {
        for v in &values {
            if let Err(e) = h.apply(&c, v.clone()).await {
                assert_ne!(e.kind(), ErrorKind::Bug, "{} on {v}", h.kind());
            }
            let _ = h.revert(&c, v.clone()).await;
        }
    }
    // Unparseable payloads are permanent (never retried forever).
    let e = handler("mail.update").apply(&c, json!({})).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Permanent);
}
