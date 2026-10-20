//! OneDrive / SharePoint sync: delta (with subtree deletes and resync), uploads (resume),
//! copy monitors, sharing request shapes, downloads (no overwrite) and hostile input.

use std::path::PathBuf;
use std::sync::Arc;

use quorum_graph::{Client, FixtureResponse, FixtureTransport, HttpRequest, Method, Route, StaticToken};
use quorum_model::{DriveId, DriveItemId, ListId, SiteId};
use quorum_store::Store;
use quorum_sync::files::handlers::{
    CopyPayload, CreateFolderPayload, DeletePayload, InvitePayload, ListItemPayload, ShareLinkPayload, UploadPayload,
};
use quorum_sync::files::state::{self, UploadProgress, UploadState};
use quorum_sync::files::{self as sf, FilesModule, kinds};
use quorum_sync::{Ctx, ErrorKind, Module, OutboxHandler, Outcome};
use serde_json::{Value, json};

const D: &str = "b!test-drive-1";
const ROOT: &str = "ROOT";
const HOST: &str = "https://contoso.sharepoint.com";

fn setup() -> (Ctx, FixtureTransport) {
    let t = FixtureTransport::new();
    let graph = Client::builder(StaticToken::new("t")).transport(t.clone()).build().unwrap();
    let store = Store::open_in_memory().unwrap();
    (Ctx { graph, store, client_id: "00000000-0000-0000-0000-000000000000".into() }, t)
}

fn drive() -> DriveId {
    DriveId::new(D).unwrap()
}

fn id(s: &str) -> DriveItemId {
    DriveItemId::new(s).unwrap()
}

fn item(id: &str, name: &str, parent: Option<&str>, folder: bool) -> Value {
    let mut v = json!({
        "id": id,
        "name": name,
        "size": 10,
        "lastModifiedDateTime": "2026-10-01T10:00:00Z",
        "parentReference": { "driveId": D },
    });
    if let Some(p) = parent {
        v["parentReference"]["id"] = json!(p);
    }
    if folder {
        v["folder"] = json!({ "childCount": 1 });
    } else {
        v["file"] = json!({ "mimeType": "text/plain" });
    }
    v
}

fn delta_path() -> String {
    format!("/drives/{D}/root/delta")
}

fn link(token: &str) -> String {
    format!("https://graph.microsoft.com/v1.0/drives/{D}/root/delta?token={token}")
}

fn handler(kind: &str) -> Arc<dyn OutboxHandler> {
    FilesModule.handlers().into_iter().find(|h| h.kind() == kind).unwrap()
}

async fn children(ctx: &Ctx, parent: &str) -> Vec<String> {
    let page = ctx.store.drive_children(drive(), Some(id(parent)), None, 500).await.unwrap();
    page.items.into_iter().map(|i| i.name.as_str().to_owned()).collect()
}

fn calls_to(t: &FixtureTransport, method: Method, path: &str) -> Vec<HttpRequest> {
    t.calls().into_iter().filter(|c| c.method == method && c.url.path().ends_with(path)).collect()
}

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("quorum-files-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn every_kind_has_a_handler() {
    let handlers = FilesModule.handlers();
    for k in kinds::ALL {
        assert!(handlers.iter().any(|h| h.kind() == k), "{k}");
    }
    assert_eq!(handlers.len(), kinds::ALL.len());
}

#[tokio::test]
async fn delta_applies_pages_and_deletes_subtrees() {
    let (ctx, t) = setup();
    let mut root = item(ROOT, "root", None, true);
    root["root"] = json!({});
    let page1 = json!({
        "@odata.nextLink": link("page-2"),
        "value": [root, item("A", "Alpha", Some(ROOT), true), item("A1", "a1.txt", Some("A"), false), item("S", "Sub", Some("A"), true)],
    });
    let page2 = json!({
        "@odata.deltaLink": link("round-1"),
        "value": [item("S1", "deep.txt", Some("S"), false), item("F", "keep.txt", Some(ROOT), false)],
    });
    t.on(Method::Get, &delta_path(), FixtureResponse::json(200, &page1));
    t.add(Route::new(Method::Get, delta_path()).query("token", "page-2").respond(FixtureResponse::json(200, &page2)));
    assert_eq!(sf::sync_drive(&ctx, &drive(), 10).await.unwrap(), Outcome::CaughtUp);
    assert_eq!(state::root_of(&ctx.store, &drive()).await.unwrap(), Some(id(ROOT)));
    assert_eq!(children(&ctx, ROOT).await, vec!["Alpha", "keep.txt"]);
    assert_eq!(children(&ctx, "S").await, vec!["deep.txt"]);

    // Next round: the folder is deleted; everything below it goes too.
    let gone =
        json!({ "@odata.deltaLink": link("round-2"), "value": [{ "id": "A", "deleted": { "state": "deleted" } }] });
    t.add(Route::new(Method::Get, delta_path()).query("token", "round-1").respond(FixtureResponse::json(200, &gone)));
    assert_eq!(sf::sync_drive(&ctx, &drive(), 10).await.unwrap(), Outcome::CaughtUp);
    assert_eq!(children(&ctx, ROOT).await, vec!["keep.txt"]);
    assert!(children(&ctx, "S").await.is_empty());
    assert!(ctx.store.read(|r| r.drive_item(&drive(), &id("S1"))).await.unwrap().is_none());
    let st = ctx.store.delta_state(sf::drive_resource(&drive())).await.unwrap().unwrap();
    assert_eq!(st.delta_link.as_deref(), Some(link("round-2").as_str()));
    assert!(st.next_link.is_none());
}

#[tokio::test]
async fn page_budget_stops_with_more_and_resumes() {
    let (ctx, t) = setup();
    let page1 = json!({ "@odata.nextLink": link("p2"), "value": [item("X", "x.txt", Some(ROOT), false)] });
    let page2 = json!({ "@odata.deltaLink": link("done"), "value": [item("Y", "y.txt", Some(ROOT), false)] });
    t.on(Method::Get, &delta_path(), FixtureResponse::json(200, &page1));
    t.add(Route::new(Method::Get, delta_path()).query("token", "p2").respond(FixtureResponse::json(200, &page2)));
    assert_eq!(sf::sync_drive(&ctx, &drive(), 1).await.unwrap(), Outcome::More);
    assert_eq!(children(&ctx, ROOT).await, vec!["x.txt"]);
    // A restart resumes from the stored nextLink, not from scratch.
    assert_eq!(sf::sync_drive(&ctx, &drive(), 1).await.unwrap(), Outcome::CaughtUp);
    assert_eq!(children(&ctx, ROOT).await, vec!["x.txt", "y.txt"]);
    assert_eq!(calls_to(&t, Method::Get, "/root/delta").len(), 2);
}

#[tokio::test]
async fn expired_token_resyncs_from_scratch() {
    let (ctx, t) = setup();
    let page = json!({ "@odata.deltaLink": link("old"), "value": [item("X", "stale.txt", Some(ROOT), false)] });
    t.on(Method::Get, &delta_path(), FixtureResponse::json(200, &page));
    sf::sync_drive(&ctx, &drive(), 5).await.unwrap();
    let expired = json!({ "error": { "code": "resyncRequired", "message": "x" } });
    t.add(Route::new(Method::Get, delta_path()).query("token", "old").respond(FixtureResponse::json(410, &expired)));
    let out = sf::sync_drive(&ctx, &drive(), 5).await.unwrap();
    assert_eq!(out, Outcome::More);
    assert!(children(&ctx, ROOT).await.is_empty(), "cached items are dropped");
    assert!(ctx.store.delta_state(sf::drive_resource(&drive())).await.unwrap().is_none());
}

#[tokio::test]
async fn hostile_delta_pages_never_panic() {
    let (ctx, t) = setup();
    let page = json!({
        "@odata.deltaLink": link("h"),
        "value": [
            null, 1, "x", [], {}, { "id": "" }, { "id": 5 }, { "id": "\u{0}" },
            { "id": "SELF", "name": "loop", "folder": {}, "parentReference": { "id": "SELF" } },
            { "id": "N", "name": "x".repeat(100_000), "size": -1, "folder": { "childCount": "lots" } },
            { "@removed": {}, "id": null },
            { "id": "Z", "deleted": {}, "name": "gone" },
            { "id": "ok", "name": "fine.txt", "file": {}, "parentReference": { "id": ROOT } }
        ],
    });
    t.on(Method::Get, &delta_path(), FixtureResponse::json(200, &page));
    sf::sync_drive(&ctx, &drive(), 5).await.unwrap();
    assert_eq!(children(&ctx, ROOT).await, vec!["fine.txt"]);
    // A self-parented folder is filed at the top level instead of hiding itself.
    let top = ctx.store.drive_children(drive(), None, None, 500).await.unwrap();
    assert!(top.items.iter().any(|i| i.id.as_str() == "SELF"));
}

#[tokio::test]
async fn hostile_payloads_are_permanent_errors() {
    let (ctx, _t) = setup();
    for h in FilesModule.handlers() {
        for p in [
            Value::Null,
            json!([]),
            json!({ "drive": 5 }),
            json!({ "drive": "", "item": "" }),
            json!("x".repeat(10_000)),
        ] {
            let e = h.apply(&ctx, p.clone()).await.err().unwrap();
            assert_eq!(e.kind(), ErrorKind::Permanent, "{} {p}", h.kind());
            let _ = h.revert(&ctx, p).await;
        }
    }
}

#[tokio::test]
async fn create_folder_replaces_the_placeholder() {
    let (ctx, t) = setup();
    let local = item("local-1", "New folder", Some(ROOT), true);
    let local: quorum_model::DriveItem = quorum_graph::api::files::parse_drive_item(local, Some(&drive())).unwrap();
    ctx.store.write(move |tx| tx.upsert_drive_items(&[local])).await.unwrap();
    t.on(
        Method::Post,
        &format!("/drives/{D}/items/{ROOT}/children"),
        FixtureResponse::json(201, &item("REAL", "New folder", Some(ROOT), true)),
    );
    let p =
        CreateFolderPayload { drive: drive(), parent: id(ROOT), name: "New folder".into(), local_id: id("local-1") };
    let out = handler(kinds::CREATE_FOLDER).apply(&ctx, serde_json::to_value(&p).unwrap()).await.unwrap();
    assert_eq!(out["id"], "REAL");
    let names = ctx.store.drive_children(drive(), Some(id(ROOT)), None, 10).await.unwrap();
    assert_eq!(names.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), vec!["REAL"]);
    let body: Value = serde_json::from_slice(&calls_to(&t, Method::Post, "/children")[0].body).unwrap();
    assert_eq!(body["name"], "New folder");
    assert_eq!(body["@microsoft.graph.conflictBehavior"], "rename");
}

#[tokio::test]
async fn delete_treats_404_as_done() {
    let (ctx, t) = setup();
    t.on(Method::Delete, &format!("/drives/{D}/items/GONE"), FixtureResponse::status(404));
    let p = DeletePayload { drive: drive(), item: id("GONE"), prev: None };
    handler(kinds::DELETE).apply(&ctx, serde_json::to_value(&p).unwrap()).await.unwrap();
}

#[tokio::test]
async fn large_upload_resumes_a_stored_session() {
    let (ctx, t) = setup();
    let dir = temp_dir("resume");
    let path = dir.join("big.bin");
    let total: u64 = 5 * 1024 * 1024;
    std::fs::write(&path, vec![7u8; usize::try_from(total).unwrap()]).unwrap();
    let op = "op-resume";
    let session = format!("{HOST}/upload/s1");
    // A previous attempt (before a restart) created the session and sent the first 4 MiB.
    sf::remember_upload_session(&ctx.store, op, &session, None).await.unwrap();
    ctx.store
        .write(move |tx| {
            state::put_upload(
                tx,
                op,
                UploadProgress { name: "big.bin".into(), total, sent: 0, state: UploadState::Queued, updated: 0 },
            )
        })
        .await
        .unwrap();
    t.on(Method::Get, "/upload/s1", FixtureResponse::json(200, &json!({ "nextExpectedRanges": ["4194304-"] })));
    t.on(Method::Put, "/upload/s1", FixtureResponse::json(201, &item("UP", "big.bin", Some(ROOT), false)));
    let p = UploadPayload {
        drive: drive(),
        parent: id(ROOT),
        path: path.clone(),
        name: "big.bin".into(),
        op: op.into(),
        local_id: id("local-up"),
        replace: false,
        staged: false,
    };
    handler(kinds::UPLOAD).apply(&ctx, serde_json::to_value(&p).unwrap()).await.unwrap();
    assert!(calls_to(&t, Method::Post, "createUploadSession").is_empty(), "the stored session is reused");
    let puts = calls_to(&t, Method::Put, "/upload/s1");
    assert_eq!(puts.len(), 1);
    assert_eq!(puts[0].header("Content-Range"), Some("bytes 4194304-5242879/5242880"));
    assert_eq!(puts[0].body.len(), 1024 * 1024);
    assert!(puts[0].header("Authorization").is_none(), "no bearer token to the upload host");
    let progress = state::uploads(&ctx.store).await.unwrap();
    assert_eq!(progress[op].state, UploadState::Done);
    assert!(ctx.store.get_blob(state::upload_session_blob(op)).await.unwrap().is_none());
    assert!(path.exists(), "the user's own file is never deleted");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn large_upload_creates_a_session_and_stores_it_first() {
    let (ctx, t) = setup();
    let dir = temp_dir("create");
    let path = dir.join("big.bin");
    std::fs::write(&path, vec![1u8; 4 * 1024 * 1024 + 10]).unwrap();
    t.on(
        Method::Post,
        &format!("/drives/{D}/items/{ROOT}:/big.bin:/createUploadSession"),
        FixtureResponse::json(200, &json!({ "uploadUrl": format!("{HOST}/upload/s2"), "nextExpectedRanges": ["0-"] })),
    );
    // The chunk fails for good: the session must survive for the next attempt.
    t.on(Method::Put, "/upload/s2", FixtureResponse::json(403, &json!({ "error": { "code": "accessDenied" } })));
    let p = UploadPayload {
        drive: drive(),
        parent: id(ROOT),
        path,
        name: "big.bin".into(),
        op: "op-2".into(),
        local_id: id("local-2"),
        replace: false,
        staged: false,
    };
    let e = handler(kinds::UPLOAD).apply(&ctx, serde_json::to_value(&p).unwrap()).await.err().unwrap();
    assert_eq!(e.kind(), ErrorKind::Permanent);
    assert!(ctx.store.get_blob(state::upload_session_blob("op-2")).await.unwrap().is_some());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn small_upload_is_one_put_and_staged_copies_are_removed() {
    let (ctx, t) = setup();
    let dir = temp_dir("small");
    let path = dir.join("staged.bin");
    std::fs::write(&path, b"hello").unwrap();
    t.on(
        Method::Put,
        &format!("/drives/{D}/items/{ROOT}:/notes%20v2.txt:/content"),
        FixtureResponse::json(201, &item("SMALL", "notes v2.txt", Some(ROOT), false)),
    );
    let p = UploadPayload {
        drive: drive(),
        parent: id(ROOT),
        path: path.clone(),
        name: "notes v2.txt".into(),
        op: "op-3".into(),
        local_id: id("local-3"),
        replace: true,
        staged: true,
    };
    handler(kinds::UPLOAD).apply(&ctx, serde_json::to_value(&p).unwrap()).await.unwrap();
    let put = &calls_to(&t, Method::Put, ":/content")[0];
    assert_eq!(put.body, b"hello");
    assert!(put.url.query().unwrap_or_default().contains("replace"));
    assert!(!path.exists(), "staging copies are cleaned up");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn missing_upload_file_is_permanent() {
    let (ctx, _t) = setup();
    let p = UploadPayload {
        drive: drive(),
        parent: id(ROOT),
        path: PathBuf::from("/definitely/not/here.bin"),
        name: "here.bin".into(),
        op: "op-4".into(),
        local_id: id("local-4"),
        replace: false,
        staged: false,
    };
    let e = handler(kinds::UPLOAD).apply(&ctx, serde_json::to_value(&p).unwrap()).await.err().unwrap();
    assert_eq!(e.kind(), ErrorKind::Permanent);
}

fn copy_payload(op: &str) -> Value {
    serde_json::to_value(CopyPayload {
        drive: drive(),
        item: id("SRC"),
        dest_drive: drive(),
        dest_folder: id(ROOT),
        name: Some("Copy of a.txt".into()),
        op: op.into(),
    })
    .unwrap()
}

#[tokio::test]
async fn copy_polls_the_monitor_and_stores_the_result() {
    let (ctx, t) = setup();
    let monitor = format!("{HOST}/_api/v2.0/monitor/m1");
    t.on(
        Method::Post,
        &format!("/drives/{D}/items/SRC/copy"),
        FixtureResponse::status(202).with_header("Location", &monitor),
    );
    t.on(
        Method::Get,
        "/_api/v2.0/monitor/m1",
        FixtureResponse::json(200, &json!({ "status": "completed", "resourceId": "NEW" })),
    );
    t.on(
        Method::Get,
        &format!("/drives/{D}/items/NEW"),
        FixtureResponse::json(200, &item("NEW", "Copy of a.txt", Some(ROOT), false)),
    );
    let out = handler(kinds::COPY).apply(&ctx, copy_payload("op-c1")).await.unwrap();
    assert_eq!(out["status"], "completed");
    assert_eq!(children(&ctx, ROOT).await, vec!["Copy of a.txt"]);
    let body: Value = serde_json::from_slice(&calls_to(&t, Method::Post, "/copy")[0].body).unwrap();
    assert_eq!(body["parentReference"]["id"], ROOT);
    assert_eq!(body["name"], "Copy of a.txt");
    let monitor_call = &calls_to(&t, Method::Get, "/monitor/m1")[0];
    assert!(monitor_call.header("Authorization").is_none(), "monitors are unauthenticated");
    assert!(ctx.store.get_blob(state::copy_monitor_blob("op-c1")).await.unwrap().is_none());
}

#[tokio::test]
async fn copy_in_progress_retries_and_resumes_without_copying_again() {
    let (ctx, t) = setup();
    let monitor = format!("{HOST}/_api/v2.0/monitor/m2");
    t.on(
        Method::Post,
        &format!("/drives/{D}/items/SRC/copy"),
        FixtureResponse::status(202).with_header("Location", &monitor),
    );
    t.add(
        Route::new(Method::Get, "/_api/v2.0/monitor/m2")
            .respond(FixtureResponse::json(202, &json!({ "status": "inProgress", "percentageComplete": 40.0 })))
            .respond(FixtureResponse::json(200, &json!({ "status": "completed" }))),
    );
    // One poll per attempt, so the first attempt sees "in progress".
    let ctx_short = ctx.clone();
    let first = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        // The handler polls up to COPY_POLLS times; the scripted second answer completes it.
        handler(kinds::COPY).apply(&ctx_short, copy_payload("op-c2")).await
    })
    .await
    .unwrap();
    assert!(first.is_ok());
    assert_eq!(calls_to(&t, Method::Post, "/copy").len(), 1);

    // Resume: a stored monitor is polled instead of starting another copy.
    let (ctx2, t2) = setup();
    ctx2.store
        .put_blob(state::copy_monitor_blob("op-c3"), format!("{HOST}/_api/v2.0/monitor/m3").into_bytes())
        .await
        .unwrap();
    t2.on(
        Method::Get,
        "/_api/v2.0/monitor/m3",
        FixtureResponse::json(200, &json!({ "status": "failed", "error": { "code": "nameAlreadyExists" } })),
    );
    let e = handler(kinds::COPY).apply(&ctx2, copy_payload("op-c3")).await.err().unwrap();
    assert_eq!(e.kind(), ErrorKind::Permanent);
    assert!(calls_to(&t2, Method::Post, "/copy").is_empty());
}

#[tokio::test]
async fn copy_rejects_a_monitor_on_a_foreign_host() {
    let (ctx, t) = setup();
    t.on(
        Method::Post,
        &format!("/drives/{D}/items/SRC/copy"),
        FixtureResponse::status(202).with_header("Location", "https://evil.example/monitor"),
    );
    let e = handler(kinds::COPY).apply(&ctx, copy_payload("op-c4")).await.err().unwrap();
    assert_eq!(e.kind(), ErrorKind::Permanent);
    assert!(calls_to(&t, Method::Get, "/monitor").is_empty());
}

#[tokio::test]
async fn share_requests_have_the_documented_shape() {
    let (ctx, t) = setup();
    t.on(
        Method::Post,
        &format!("/drives/{D}/items/DOC/createLink"),
        FixtureResponse::json(200, &json!({ "id": "p1", "roles": ["write"], "link": { "type": "edit", "scope": "organization", "webUrl": format!("{HOST}/:w:/s1") } })),
    );
    t.on(
        Method::Post,
        &format!("/drives/{D}/items/DOC/invite"),
        FixtureResponse::json(200, &json!({ "value": [{ "id": "p2", "roles": ["read"] }] })),
    );
    t.on(Method::Get, &format!("/drives/{D}/items/DOC/permissions"), FixtureResponse::json(200, &json!({ "value": [{ "id": "p1", "roles": ["write"], "link": { "type": "edit", "scope": "organization", "webUrl": format!("{HOST}/:w:/s1") } }] })));
    t.on(Method::Get, &format!("/drives/{D}/items/DOC/versions"), FixtureResponse::json(200, &json!({ "value": [] })));

    let link =
        ShareLinkPayload { drive: drive(), item: id("DOC"), link_type: "edit".into(), scope: "organization".into() };
    handler(kinds::SHARE_LINK).apply(&ctx, serde_json::to_value(&link).unwrap()).await.unwrap();
    let body: Value = serde_json::from_slice(&calls_to(&t, Method::Post, "/createLink")[0].body).unwrap();
    assert_eq!(body, json!({ "type": "edit", "scope": "organization" }));
    // The cached details never hold the link URL (a capability).
    let meta = state::meta(&ctx.store, &drive(), &id("DOC")).await.unwrap().unwrap();
    assert_eq!(meta.permissions.len(), 1);
    assert!(!serde_json::to_string(&meta).unwrap().contains(":w:/s1"));

    let inv = InvitePayload {
        drive: drive(),
        item: id("DOC"),
        recipients: vec![quorum_model::MailAddress::new("robin@example.com").unwrap()],
        write: false,
        message: Some("Have a look".into()),
        send_invitation: true,
    };
    handler(kinds::INVITE).apply(&ctx, serde_json::to_value(&inv).unwrap()).await.unwrap();
    let body: Value = serde_json::from_slice(&calls_to(&t, Method::Post, "/invite")[0].body).unwrap();
    assert_eq!(body["recipients"], json!([{ "email": "robin@example.com" }]));
    assert_eq!(body["roles"], json!(["read"]));
    assert_eq!(body["requireSignIn"], true);
    assert_eq!(body["sendInvitation"], true);
    assert_eq!(body["message"], "Have a look");

    let bad = ShareLinkPayload { drive: drive(), item: id("DOC"), link_type: "own".into(), scope: "world".into() };
    let e = handler(kinds::SHARE_LINK).apply(&ctx, serde_json::to_value(&bad).unwrap()).await.err().unwrap();
    assert_eq!(e.kind(), ErrorKind::Permanent);
}

#[tokio::test]
async fn follow_uses_followed_sites_add() {
    let (ctx, t) = setup();
    t.on(Method::Post, "/me/followedSites/add", FixtureResponse::json(200, &json!({ "value": [] })));
    t.on(Method::Get, "/me/followedSites", FixtureResponse::json(200, &json!({ "value": [] })));
    let site = SiteId::new("contoso.sharepoint.com,1,2").unwrap();
    let p = quorum_sync::files::handlers::FollowPayload { site, prev: Some(vec![]) };
    handler(kinds::FOLLOW_SITE).apply(&ctx, serde_json::to_value(&p).unwrap()).await.unwrap();
    let body: Value = serde_json::from_slice(&calls_to(&t, Method::Post, "/followedSites/add")[0].body).unwrap();
    assert_eq!(body, json!({ "value": [{ "id": "contoso.sharepoint.com,1,2" }] }));
}

#[tokio::test]
async fn list_create_replaces_the_placeholder() {
    let (ctx, t) = setup();
    let site = SiteId::new("site-1").unwrap();
    let list = ListId::new("list-1").unwrap();
    t.on(
        Method::Post,
        "/sites/site-1/lists/list-1/items",
        FixtureResponse::json(201, &json!({ "id": "42", "fields": { "Title": "New task" } })),
    );
    let fields = serde_json::from_value(json!({ "Title": "New task" })).unwrap();
    let p = ListItemPayload {
        site,
        list: list.clone(),
        item: quorum_model::ListItemId::new("local-9").unwrap(),
        fields,
        prev: None,
    };
    handler(kinds::LIST_CREATE).apply(&ctx, serde_json::to_value(&p).unwrap()).await.unwrap();
    let page = ctx.store.read(move |r| r.list_items(&list, None, 10)).await.unwrap();
    assert_eq!(page.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), vec!["42"]);
}

#[tokio::test]
async fn downloads_never_overwrite() {
    let (ctx, t) = setup();
    let dir = temp_dir("download");
    std::fs::write(dir.join("Report.docx"), b"mine").unwrap();
    t.on(
        Method::Get,
        &format!("/drives/{D}/items/R1/content"),
        FixtureResponse::status(302).with_header("Location", &format!("{HOST}/dl/r1")),
    );
    t.on(Method::Get, "/dl/r1", FixtureResponse::bytes(200, b"remote".to_vec()));
    let first = sf::download(&ctx, &drive(), &id("R1"), "Report.docx", &dir).await.unwrap();
    let second = sf::download(&ctx, &drive(), &id("R1"), "Report.docx", &dir).await.unwrap();
    assert_eq!(first.file_name().unwrap(), "Report (1).docx");
    assert_eq!(second.file_name().unwrap(), "Report (2).docx");
    assert_eq!(std::fs::read(dir.join("Report.docx")).unwrap(), b"mine");
    assert_eq!(std::fs::read(&first).unwrap(), b"remote");
    // Hostile names stay inside the folder.
    let evil = sf::download(&ctx, &drive(), &id("R1"), "../../escape.txt", &dir).await.unwrap();
    assert_eq!(evil.parent().unwrap(), dir.as_path());
    let dotfile = sf::unique_path(&dir, "README").unwrap();
    assert_eq!(dotfile.file_name().unwrap(), "README");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn recent_shared_and_sites_refresh() {
    let (ctx, t) = setup();
    let recent = json!({ "value": [
        { "id": "alias", "name": "a.docx", "remoteItem": { "id": "R1", "name": "a.docx", "file": {}, "parentReference": { "driveId": "b!other", "id": "P" } } }
    ]});
    t.on(Method::Get, "/me/drive/recent", FixtureResponse::json(200, &recent));
    t.on(Method::Get, "/me/drive/sharedWithMe", FixtureResponse::json(200, &json!({ "value": [] })));
    t.on(
        Method::Get,
        "/me/followedSites",
        FixtureResponse::json(200, &json!({ "value": [{ "id": "s1", "displayName": "Site One" }] })),
    );
    t.on(
        Method::Get,
        "/sites/s1/drives",
        FixtureResponse::json(
            200,
            &json!({ "value": [{ "id": "b!lib", "driveType": "documentLibrary", "name": "Documents" }] }),
        ),
    );
    t.on(Method::Get, "/sites/s1/lists", FixtureResponse::json(500, &json!({ "error": { "code": "boom" } })));
    sf::refresh_recent(&ctx).await.unwrap();
    sf::refresh_shared(&ctx).await.unwrap();
    let sites = FilesModule.discover(&ctx).await.unwrap().into_iter().find(|r| r.key() == "files.sites").unwrap();
    // A newly followed site's library: the module rediscovers so it syncs at once.
    assert_eq!(sites.run(&ctx).await.unwrap(), Outcome::Rediscover);
    assert_eq!(sites.run(&ctx).await.unwrap(), Outcome::CaughtUp, "same libraries: nothing to rediscover");
    let r = state::recent(&ctx.store).await.unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].drive.as_str(), "b!other");
    assert_eq!(r[0].id.as_str(), "R1");
    assert!(state::shared(&ctx.store).await.unwrap().is_empty());
    let site = SiteId::new("s1").unwrap();
    assert_eq!(state::followed(&ctx.store).await.unwrap().unwrap(), vec![site.clone()]);
    assert_eq!(state::site_drives(&ctx.store, &site).await.unwrap(), vec![DriveId::new("b!lib").unwrap()]);
    assert_eq!(state::libraries(&ctx.store).await.unwrap().len(), 1);
    let found = FilesModule.discover(&ctx).await.unwrap();
    assert!(found.iter().any(|r| r.key() == "files.library:b!lib"));
    assert!(found.iter().any(|r| r.key() == sf::MY_DRIVE_KEY));
}

#[tokio::test]
async fn list_items_are_fetched_and_cached_with_columns() {
    let (ctx, t) = setup();
    t.on(
        Method::Get,
        "/sites/s1/lists/l1/columns",
        FixtureResponse::json(
            200,
            &json!({ "value": [
                { "id": "c1", "name": "Title", "displayName": "Title", "text": {} },
                { "id": "c2", "name": "Status", "displayName": "Status", "choice": { "choices": ["Open", "Done"] } }
            ]}),
        ),
    );
    t.on(
        Method::Get,
        "/sites/s1/lists/l1/items",
        FixtureResponse::json(
            200,
            &json!({
                "@odata.nextLink": "https://graph.microsoft.com/v1.0/sites/s1/lists/l1/items?$skiptoken=2",
                "value": [{ "id": "1", "fields": { "Title": "One" } }]
            }),
        ),
    );
    t.add(Route::new(Method::Get, "/sites/s1/lists/l1/items").query("$skiptoken", "2").respond(FixtureResponse::json(
        200,
        &json!({
            "value": [{ "id": "2", "fields": { "Title": "Two" } }]
        }),
    )));
    let (site, list) = (SiteId::new("s1").unwrap(), ListId::new("l1").unwrap());
    assert_eq!(sf::live::fetch_list_items(&ctx, &site, &list).await.unwrap(), 2);
    let cols = state::columns(&ctx.store, &list).await.unwrap().unwrap();
    assert_eq!(cols[1].kind, "choice");
    assert_eq!(cols[1].choices, vec!["Open", "Done"]);
}
