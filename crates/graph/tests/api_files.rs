//! Tests for `quorum_graph::api::files` and `quorum_graph::api::sites` against the fixture
//! transport (synthetic data only).

use std::time::Duration;

use quorum_graph::api::files::{
    self, ConflictBehavior, CopyStatus, DriveChange, Invite, ItemLocation, LinkScope, LinkType, SecretUrl,
};
use quorum_graph::api::sites;
use quorum_graph::{
    ChunkSize, Client, DeltaOutcome, ErrorKind, FakeClock, FixtureResponse, FixtureTransport, HttpRequest, Method,
    RateLimit, ResourceFamily, Route, StaticToken, UPLOAD_CHUNK_UNIT,
};
use quorum_model::{DriveId, DriveItemId, DriveKind, ListId, ListItemId, MailAddress, SiteId};
use serde_json::{Map, Value, json};

const D: &str = "b!synthetic-drive-0001";
const D2: &str = "b!synthetic-drive-0002";
const ITEM: &str = "01SYNTHETICITEM00000000000000001";
const ROOT: &str = "01SYNTHETICROOT00000000000000000";
const DL_HOST: &str = "https://contoso-my.sharepoint.com";

fn client_with_clock(t: &FixtureTransport, clock: &FakeClock) -> Client {
    let mut b = Client::builder(StaticToken::new("test-token"));
    for f in ResourceFamily::ALL {
        b = b.rate(f, RateLimit::new(10_000.0, 10_000).unwrap());
    }
    b.transport(t.clone()).clock(clock.clone()).jitter_seed(3).build().unwrap()
}

fn client(t: &FixtureTransport) -> Client {
    client_with_clock(t, &FakeClock::new())
}

fn file(name: &str) -> FixtureResponse {
    FixtureResponse::file(200, name).unwrap()
}

fn q(req: &HttpRequest, key: &str) -> Option<String> {
    req.url.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned())
}

fn body(req: &HttpRequest) -> Value {
    serde_json::from_slice(&req.body).unwrap()
}

fn drive() -> DriveId {
    DriveId::new(D).unwrap()
}

fn item() -> DriveItemId {
    DriveItemId::new(ITEM).unwrap()
}

fn ipath(rest: &str) -> String {
    format!("/drives/{D}/items/{ITEM}{rest}")
}

fn raw(status: u16, bytes: &[u8], headers: &[(&str, &str)]) -> FixtureResponse {
    let mut r = FixtureResponse::bytes(status, bytes.to_vec());
    if let FixtureResponse::Http(h) = &mut r {
        h.headers = headers.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
    }
    r
}

#[tokio::test]
async fn drives_and_children_paging() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me/drive", file("files/drive.json"));
    t.on(Method::Get, "/me/drives", file("files/drives.json"));
    t.on(Method::Get, &format!("/drives/{D}/root/children"), file("files/children-page1.json"));
    t.add(
        Route::new(Method::Get, format!("/drives/{D}/root/children"))
            .query("$skiptoken", "children-2")
            .respond(file("files/children-page2.json")),
    );
    let c = client(&t);
    let me = c.my_drive().await.unwrap();
    assert_eq!(me.drive_type, DriveKind::Business);
    assert_eq!(me.owner.as_ref().unwrap().display_name.as_str(), "Test User");
    assert_eq!(q(&t.calls()[0], "$select").as_deref(), Some("id,driveType,name,owner,webUrl"));
    let drives = c.list_my_drives().await.unwrap();
    assert_eq!(drives.len(), 2, "drive without a type is skipped");
    assert_eq!(drives[1].drive_type, DriveKind::DocumentLibrary);
    assert!(drives[1].owner.is_none(), "group owners are not user identities");

    let page = c.list_root_children(&drive(), 5000).await.unwrap();
    let call = t.calls().last().unwrap().clone();
    assert_eq!(q(&call, "$top").as_deref(), Some("999"));
    assert_eq!(q(&call, "$select").as_deref(), Some(files::SELECT_DRIVE_ITEM));
    assert_eq!(page.items.len(), 2);
    let doc = &page.items[0];
    assert_eq!(doc.drive_id, drive());
    assert_eq!(doc.parent_id.as_ref().unwrap().as_str(), ROOT);
    assert!(doc.shared);
    assert_eq!(doc.mime_type(), Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"));
    assert_eq!(doc.last_modified_by.as_ref().unwrap().display_name.as_str(), "Alex Sample");
    assert!(page.items[1].is_folder());
    assert_eq!(page.items[1].folder.as_ref().unwrap().child_count, 2);
    let page2 = c.list_items_next(Some(&drive()), page.next_link.as_deref().unwrap()).await.unwrap();
    assert_eq!(page2.items.len(), 2);
    assert_eq!(page2.items[1].drive_id, drive(), "missing parentReference falls back to the drive");
}

#[tokio::test]
async fn item_by_id_and_path() {
    let t = FixtureTransport::new();
    t.on(Method::Get, &ipath(""), file("files/item.json"));
    t.on(Method::Get, &format!("/drives/{D}/root:/Documents/Q3%20report.docx"), file("files/item.json"));
    let c = client(&t);
    let it = c.get_drive_item(&drive(), &item()).await.unwrap();
    assert_eq!(it.e_tag.as_deref(), Some("\"{5B6C8A52-0000-4000-8000-000000000001},3\""));
    let by_path = c.get_drive_item_by_path(&drive(), "/Documents/Q3 report.docx").await.unwrap();
    assert_eq!(by_path.id, item());
    let call = t.calls().last().unwrap().clone();
    assert!(call.url.as_str().contains("/root:/Documents/Q3%20report.docx"), "{}", call.url);
    for bad in ["", "/", "a/../b", "a//b", "a/b?c", "a\u{0}b", "a/ b"] {
        assert_eq!(c.get_drive_item_by_path(&drive(), bad).await.unwrap_err().kind(), ErrorKind::Config, "{bad:?}");
    }
    assert_eq!(t.call_count(), 2);
}

#[tokio::test]
async fn drive_delta_flow() {
    let t = FixtureTransport::new();
    let delta = format!("/drives/{D}/root/delta");
    t.on(Method::Get, &delta, file("files/delta-page1.json"));
    t.add(Route::new(Method::Get, &delta).query("token", "files-delta-2").respond(file("files/delta-page2.json")));
    t.add(
        Route::new(Method::Get, &delta)
            .query("token", "stale")
            .respond(FixtureResponse::json(410, &json!({"error": {"code": "resyncRequired"}}))),
    );
    let c = client(&t);
    let p1 = c.drive_delta(&drive(), None).await.unwrap();
    assert_eq!(p1.items.len(), 3);
    assert!(matches!(&p1.items[0], DriveChange::Changed(i) if i.is_folder()));
    assert_eq!(p1.items[2], DriveChange::Deleted(DriveItemId::new("01SYNTHETICITEM00000000000000009").unwrap()));
    let DeltaOutcome::Next(next) = &p1.outcome else { panic!("expected nextLink") };
    let p2 = c.drive_delta(&drive(), Some(next)).await.unwrap();
    assert_eq!(p2.items, vec![DriveChange::Deleted(DriveItemId::new("01SYNTHETICITEM00000000000000008").unwrap())]);
    assert!(matches!(&p2.outcome, DeltaOutcome::Done(l) if l.contains("files-token-final")));

    let req = files::drive_delta_request(&drive(), Some("latest"));
    assert_eq!(req.resource_family(), ResourceFamily::Files);
    assert_eq!(req.bucket_key(), files::FILES_BUCKET);
    let stale = format!("https://graph.microsoft.com/v1.0{delta}?token=stale");
    assert_eq!(c.drive_delta(&drive(), Some(&stale)).await.unwrap_err().kind(), ErrorKind::DeltaExpired);
}

#[tokio::test]
async fn recent_shared_and_search() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me/drive/recent", file("files/recent.json"));
    t.on(Method::Get, "/me/drive/sharedWithMe", file("files/recent.json"));
    t.on(Method::Get, &format!("/drives/{D}/root/search(q='it''s%2050%25')"), file("files/children-page2.json"));
    let c = client(&t);
    let recent = c.recent_files().await.unwrap();
    let r = &recent.items[0];
    assert_eq!(r.id.as_str(), "01REMOTEITEM0000000000000000001", "the remote item wins");
    assert_eq!(r.drive_id.as_str(), D2);
    assert_eq!(r.parent_id.as_ref().unwrap().as_str(), "01REMOTEPARENT00000000000000001");
    assert_eq!(r.size, 4096);
    assert!(r.shared);
    assert_eq!(c.shared_with_me().await.unwrap().items.len(), 1);
    let found = c.search_drive(&drive(), "it's 50%").await.unwrap();
    assert_eq!(found.items.len(), 2);
    assert!(t.calls().last().unwrap().url.path().ends_with("/root/search(q='it''s%2050%25')"));
    assert!(c.search_drive(&drive(), "  ").await.is_err());
}

#[tokio::test]
async fn download_follows_redirect_without_token() {
    let t = FixtureTransport::new();
    let target = format!("{DL_HOST}/personal/test/_layouts/15/download.aspx?UniqueId=synthetic&tempauth=SECRET");
    t.on(Method::Get, &ipath("/content"), raw(302, b"", &[("Location", &target)]));
    t.on(
        Method::Get,
        "/personal/test/_layouts/15/download.aspx",
        raw(200, b"file-bytes", &[("content-type", "text/plain")]),
    );
    let c = client(&t);
    let got = c.download_item(&drive(), &item()).await.unwrap();
    assert_eq!(got.bytes, b"file-bytes");
    assert_eq!(got.content_type.as_deref(), Some("text/plain"));
    let calls = t.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].header("authorization"), Some("Bearer test-token"));
    assert_eq!(calls[1].url.host_str(), Some("contoso-my.sharepoint.com"));
    assert_eq!(calls[1].header("authorization"), None, "never forward our token to the download host");
    assert_eq!(calls[1].header("client-request-id"), None);
    assert!(!format!("{got:?}").contains("file-bytes"));

    // A redirect to a host outside the allow-list is refused before any I/O.
    let t = FixtureTransport::new();
    t.on(Method::Get, &ipath("/content"), raw(302, b"", &[("Location", "https://evil.example/steal")]));
    let err = client(&t).download_item(&drive(), &item()).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Malformed);
    assert_eq!(t.call_count(), 1);
    // ... as is a redirect back to Graph or over plain http, and a redirect without Location.
    for loc in [
        "https://graph.microsoft.com/v1.0/me",
        "http://contoso.sharepoint.com/x",
        "https://user:pw@contoso.sharepoint.com/x",
    ] {
        let t = FixtureTransport::new();
        t.on(Method::Get, &ipath("/content"), raw(302, b"", &[("Location", loc)]));
        assert!(client(&t).download_item(&drive(), &item()).await.is_err(), "{loc}");
        assert_eq!(t.call_count(), 1);
    }
    let t = FixtureTransport::new();
    t.on(Method::Get, &ipath("/content"), FixtureResponse::status(302));
    assert_eq!(client(&t).download_item(&drive(), &item()).await.unwrap_err().kind(), ErrorKind::Malformed);
    // A redirect loop stops after a few hops.
    let t = FixtureTransport::new();
    t.on(Method::Get, &ipath("/content"), raw(302, b"", &[("Location", &format!("{DL_HOST}/loop"))]));
    t.on(Method::Get, "/loop", raw(302, b"", &[("Location", &format!("{DL_HOST}/loop"))]));
    assert!(client(&t).download_item(&drive(), &item()).await.is_err());
    assert!(t.call_count() <= 4);
}

#[tokio::test]
async fn download_url_is_secret_and_unauthenticated() {
    let t = FixtureTransport::new();
    let dl = format!("{DL_HOST}/personal/test/_layouts/15/download.aspx?tempauth=SECRET");
    t.on(Method::Get, &ipath(""), FixtureResponse::json(200, &json!({"id": ITEM, "@microsoft.graph.downloadUrl": dl})));
    t.on(Method::Get, "/personal/test/_layouts/15/download.aspx", raw(200, b"abc", &[]));
    let c = client(&t);
    let url = c.download_url(&drive(), &item()).await.unwrap().unwrap();
    assert_eq!(q(&t.calls()[0], "$select").as_deref(), Some("id,@microsoft.graph.downloadUrl"));
    assert!(!format!("{url:?}").contains("SECRET"));
    let got = c.download_from_url(&url).await.unwrap();
    assert_eq!(got.bytes, b"abc");
    assert_eq!(t.calls()[1].header("authorization"), None);
    let evil = SecretUrl::parse("https://contoso.sharepoint.com.evil.example/x").unwrap();
    assert!(c.download_from_url(&evil).await.is_err());
    assert_eq!(t.call_count(), 2);
}

#[tokio::test]
async fn copy_monitor_polling() {
    let t = FixtureTransport::new();
    let monitor = format!("{DL_HOST}/_api/v2.0/monitor/synthetic-monitor-1");
    t.on(Method::Post, &ipath("/copy"), raw(202, b"", &[("Location", &monitor)]));
    t.add(
        Route::new(Method::Get, "/_api/v2.0/monitor/synthetic-monitor-1")
            .respond(FixtureResponse::json(202, &json!({"status": "notStarted"})))
            .respond(FixtureResponse::json(202, &json!({"status": "inProgress", "percentageComplete": 140.5})))
            .respond(FixtureResponse::json(
                200,
                &json!({"status": "completed", "resourceId": "01COPIEDITEM000000000000000001"}),
            )),
    );
    let clock = FakeClock::new();
    let c = client_with_clock(&t, &clock);
    let dest = ItemLocation { drive: DriveId::new(D2).unwrap(), folder: DriveItemId::new("01DESTFOLDER").unwrap() };
    let mon = c.copy_drive_item(&drive(), &item(), &dest, Some("Copy of Report.docx")).await.unwrap();
    let b = body(&t.calls()[0]);
    assert_eq!(b, json!({"parentReference": {"driveId": D2, "id": "01DESTFOLDER"}, "name": "Copy of Report.docx"}));
    assert!(!format!("{mon:?}").contains("monitor"));
    assert_eq!(c.copy_status(&mon).await.unwrap(), CopyStatus::InProgress { percent: None });
    assert_eq!(c.copy_status(&mon).await.unwrap(), CopyStatus::InProgress { percent: Some(100.0) }, "clamped");
    let done = c.wait_for_copy(&mon, Duration::from_secs(2), 5).await.unwrap();
    assert_eq!(done, CopyStatus::Completed { item: Some(DriveItemId::new("01COPIEDITEM000000000000000001").unwrap()) });
    assert!(t.calls()[1..].iter().all(|r| r.header("authorization").is_none()), "monitor polls are unauthenticated");

    // Polls run out while still in progress; sleeps use the client clock.
    let t = FixtureTransport::new();
    t.on(Method::Get, "/_api/v2.0/monitor/m2", FixtureResponse::json(202, &json!({"status": "inProgress"})));
    let clock = FakeClock::new();
    let c = client_with_clock(&t, &clock);
    let m2 = SecretUrl::parse(&format!("{DL_HOST}/_api/v2.0/monitor/m2")).unwrap();
    let last = c.wait_for_copy(&m2, Duration::from_secs(3), 3).await.unwrap();
    assert!(matches!(last, CopyStatus::InProgress { .. }));
    assert_eq!(t.call_count(), 3);
    assert!(clock.sleeps().iter().filter(|d| **d == Duration::from_secs(3)).count() >= 2);

    // Failure and 303 See Other.
    let t = FixtureTransport::new();
    t.on(
        Method::Get,
        "/_api/v2.0/monitor/m3",
        FixtureResponse::json(200, &json!({"status": "failed", "error": {"code": "nameAlreadyExists"}})),
    );
    t.on(Method::Get, "/_api/v2.0/monitor/m4", FixtureResponse::status(303));
    let c = client(&t);
    let m3 = SecretUrl::parse(&format!("{DL_HOST}/_api/v2.0/monitor/m3")).unwrap();
    assert_eq!(c.copy_status(&m3).await.unwrap(), CopyStatus::Failed { code: Some("nameAlreadyExists".into()) });
    let m4 = SecretUrl::parse(&format!("{DL_HOST}/_api/v2.0/monitor/m4")).unwrap();
    assert_eq!(c.copy_status(&m4).await.unwrap(), CopyStatus::Completed { item: None });

    // A copy whose monitor points off the allowed hosts is rejected.
    let t = FixtureTransport::new();
    t.on(Method::Post, &ipath("/copy"), raw(202, b"", &[("Location", "https://evil.example/monitor")]));
    assert!(client(&t).copy_drive_item(&drive(), &item(), &dest, None).await.is_err());
}

#[tokio::test]
async fn folder_rename_move_delete_and_uploads() {
    let t = FixtureTransport::new();
    t.on(
        Method::Post,
        &format!("/drives/{D}/items/{ROOT}/children"),
        FixtureResponse::file(201, "files/item.json").unwrap(),
    );
    t.on(Method::Patch, &ipath(""), file("files/item.json"));
    t.on(Method::Delete, &ipath(""), FixtureResponse::status(204));
    t.on(
        Method::Put,
        &format!("/drives/{D}/items/{ROOT}:/notes.txt:/content"),
        FixtureResponse::file(201, "files/item.json").unwrap(),
    );
    let c = client(&t);
    let root = DriveItemId::new(ROOT).unwrap();
    c.create_folder(&drive(), &root, "New folder", ConflictBehavior::Fail).await.unwrap();
    assert_eq!(
        body(&t.calls()[0]),
        json!({"name": "New folder", "folder": {}, "@microsoft.graph.conflictBehavior": "fail"})
    );
    for bad in ["", "a/b", "con:x", " lead", "..", "x".repeat(256).as_str()] {
        assert!(c.create_folder(&drive(), &root, bad, ConflictBehavior::Rename).await.is_err(), "{bad}");
    }
    let dest = ItemLocation { drive: drive(), folder: root.clone() };
    c.update_drive_item(&drive(), &item(), Some("Renamed.docx"), Some(&dest), Some("\"etag-1\"")).await.unwrap();
    let call = t.calls().last().unwrap().clone();
    assert_eq!(call.method, Method::Patch);
    assert_eq!(call.header("if-match"), Some("\"etag-1\""));
    assert_eq!(body(&call), json!({"name": "Renamed.docx", "parentReference": {"driveId": D, "id": ROOT}}));
    assert!(c.update_drive_item(&drive(), &item(), None, None, None).await.is_err());
    c.delete_drive_item(&drive(), &item(), None).await.unwrap();

    c.upload_small(&drive(), &root, "notes.txt", "text/plain", b"hello".to_vec(), ConflictBehavior::Replace)
        .await
        .unwrap();
    let up = t.calls().last().unwrap().clone();
    assert_eq!(up.method, Method::Put);
    assert_eq!(up.header("content-type"), Some("text/plain"));
    assert_eq!(up.body, b"hello");
    assert_eq!(q(&up, "@microsoft.graph.conflictBehavior").as_deref(), Some("replace"));
    let too_big = vec![0u8; files::MAX_SMALL_UPLOAD_BYTES + 1];
    let err =
        c.upload_small(&drive(), &root, "big.bin", "application/octet-stream", too_big, ConflictBehavior::Rename).await;
    assert_eq!(err.unwrap_err().kind(), ErrorKind::Config);

    // 412 on an eTag mismatch maps to PreconditionFailed.
    let t = FixtureTransport::new();
    t.on(Method::Patch, &ipath(""), FixtureResponse::json(412, &json!({"error": {"code": "resourceModified"}})));
    let err = client(&t).update_drive_item(&drive(), &item(), Some("x.docx"), None, Some("\"old\"")).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::PreconditionFailed);
}

#[tokio::test]
async fn large_upload_uses_session() {
    let t = FixtureTransport::new();
    let upload_url = format!("{DL_HOST}/upload/session-1?tempauth=SECRET");
    t.on(
        Method::Post,
        &format!("/drives/{D}/items/{ROOT}:/big.bin:/createUploadSession"),
        FixtureResponse::json(200, &json!({"uploadUrl": upload_url, "nextExpectedRanges": ["0-"]})),
    );
    t.add(
        Route::new(Method::Put, "/upload/session-1")
            .respond(FixtureResponse::json(202, &json!({"nextExpectedRanges": [format!("{UPLOAD_CHUNK_UNIT}-")]})))
            .respond(FixtureResponse::file(201, "files/item.json").unwrap()),
    );
    let c = client(&t);
    let data = vec![7u8; UPLOAD_CHUNK_UNIT + 10];
    let root = DriveItemId::new(ROOT).unwrap();
    let it = c
        .upload_large(&drive(), &root, "big.bin", ConflictBehavior::Rename, &data, ChunkSize::from_units(1).unwrap())
        .await
        .unwrap();
    assert_eq!(it.id, item());
    let calls = t.calls();
    assert_eq!(body(&calls[0]), json!({"item": {"@microsoft.graph.conflictBehavior": "rename", "name": "big.bin"}}));
    assert_eq!(calls.len(), 3);
    assert!(calls[1..].iter().all(|r| r.header("authorization").is_none()));
}

#[tokio::test]
async fn sharing_permissions_versions_thumbnails_preview_checkin() {
    let t = FixtureTransport::new();
    t.on(
        Method::Post,
        &ipath("/createLink"),
        FixtureResponse::json(201, &json!({"id": "perm-new", "roles": ["write"], "link": {"type": "edit", "scope": "organization", "webUrl": "https://contoso.sharepoint.com/:w:/s/x/SECRET"}})),
    );
    t.on(Method::Get, &ipath("/permissions"), file("files/permissions.json"));
    t.on(Method::Delete, &ipath("/permissions/perm-link-1"), FixtureResponse::status(204));
    t.on(
        Method::Post,
        &ipath("/invite"),
        FixtureResponse::json(200, &json!({"value": [{"id": "perm-invite", "roles": ["read"]}]})),
    );
    t.on(Method::Get, &ipath("/versions"), file("files/versions.json"));
    t.on(Method::Post, &ipath("/versions/2.0/restoreVersion"), FixtureResponse::status(204));
    t.on(Method::Get, &ipath("/thumbnails"), file("files/thumbnails.json"));
    t.on(
        Method::Post,
        &ipath("/preview"),
        FixtureResponse::json(
            200,
            &json!({"getUrl": "https://contoso.sharepoint.com/embed?token=SECRET", "postUrl": null}),
        ),
    );
    t.on(Method::Post, &ipath("/checkout"), FixtureResponse::status(204));
    t.on(Method::Post, &ipath("/checkin"), FixtureResponse::status(204));
    let c = client(&t);
    let link = c.create_sharing_link(&drive(), &item(), LinkType::Edit, LinkScope::Organization, None).await.unwrap();
    assert_eq!(body(&t.calls()[0]), json!({"type": "edit", "scope": "organization"}));
    assert_eq!(link.link_type.as_deref(), Some("edit"));
    assert!(!format!("{link:?}").contains("SECRET"), "link URLs are redacted in Debug");
    let perms = c.list_permissions(&drive(), &item()).await.unwrap();
    assert_eq!(perms.len(), 2);
    assert_eq!(perms[0].link_scope.as_deref(), Some("anonymous"));
    assert!(perms[0].expiration.is_some());
    assert_eq!(perms[1].granted_to[0].display_name.as_str(), "Alex Sample");
    assert!(perms[1].inherited);
    c.delete_permission(&drive(), &item(), "perm-link-1").await.unwrap();
    let invite = Invite {
        recipients: vec![MailAddress::new("alex.sample@contoso.example").unwrap()],
        write: false,
        send_invitation: false,
        message: None,
        expiration: None,
    };
    let created = c.invite(&drive(), &item(), &invite).await.unwrap();
    assert_eq!(created[0].id, "perm-invite");
    let b = body(t.calls().last().unwrap());
    assert_eq!(b["recipients"][0]["email"], "alex.sample@contoso.example");
    assert_eq!(b["roles"][0], "read");
    assert_eq!(b["requireSignIn"], true);
    assert!(c.invite(&drive(), &item(), &Invite { recipients: vec![], ..invite.clone() }).await.is_err());

    let versions = c.list_versions(&drive(), &item()).await.unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].size, 1234);
    assert_eq!(versions[1].size, 0, "negative sizes are ignored");
    c.restore_version(&drive(), &item(), "2.0").await.unwrap();
    let thumbs = c.thumbnails(&drive(), &item()).await.unwrap();
    assert_eq!(thumbs[0].small.as_ref().unwrap().width, Some(72));
    assert!(thumbs[0].medium.is_none(), "http thumbnail URLs are dropped");
    let preview = c.preview_url(&drive(), &item()).await.unwrap();
    assert!(preview.as_str().starts_with("https://contoso.sharepoint.com/embed"));
    c.checkout(&drive(), &item()).await.unwrap();
    c.checkin(&drive(), &item(), "Synthetic check-in").await.unwrap();
    assert_eq!(body(t.calls().last().unwrap()), json!({"comment": "Synthetic check-in"}));
    assert!(c.checkin(&drive(), &item(), "bad\u{1}").await.is_err());
}

#[tokio::test]
async fn sites_lists_and_items() {
    let site =
        SiteId::new("contoso.sharepoint.com,00000000-0000-0000-0000-0000000000c1,00000000-0000-0000-0000-0000000000c2")
            .unwrap();
    let list = ListId::new("00000000-0000-0000-0000-0000000000d1").unwrap();
    let sp = format!("/sites/{site}");
    let lp = format!("{sp}/lists/{list}");
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me/followedSites", file("files/sites-followed.json"));
    t.on(Method::Get, "/sites", file("files/sites-followed.json"));
    t.on(
        Method::Get,
        "/sites/contoso.sharepoint.com:/sites/Synthetic%20Site",
        FixtureResponse::json(200, &json!({"id": site.as_str()})),
    );
    t.on(Method::Get, &format!("{sp}/drives"), file("files/drives.json"));
    t.on(Method::Get, &format!("{sp}/lists"), file("files/lists.json"));
    t.on(Method::Get, &format!("{lp}/columns"), file("files/columns.json"));
    t.on(Method::Get, &format!("{lp}/items"), file("files/list-items-page1.json"));
    t.on(
        Method::Post,
        &format!("{lp}/items"),
        FixtureResponse::json(201, &json!({"id": "3", "fields": {"Title": "New"}})),
    );
    t.on(
        Method::Patch,
        &format!("{lp}/items/1/fields"),
        FixtureResponse::json(200, &json!({"Title": "Updated", "Status": "Done"})),
    );
    t.on(Method::Delete, &format!("{lp}/items/1"), FixtureResponse::status(204));
    t.on(Method::Get, &format!("{sp}/pages/microsoft.graph.sitePage"), file("files/site-pages.json"));
    let c = client(&t);

    let followed = c.followed_sites().await.unwrap();
    assert_eq!(followed.len(), 1);
    assert_eq!(followed[0].name.as_deref(), Some("SyntheticSite"));
    let found = c.search_sites("synthetic").await.unwrap();
    assert_eq!(q(t.calls().last().unwrap(), "search").as_deref(), Some("synthetic"));
    assert_eq!(found.skipped, 1);
    c.get_site_by_path("contoso.sharepoint.com", "/sites/Synthetic Site").await.unwrap();
    assert!(t.calls().last().unwrap().url.path().ends_with("/sites/contoso.sharepoint.com:/sites/Synthetic%20Site"));
    assert!(c.get_site_by_path("evil/host", "sites/x").await.is_err());
    assert!(c.get_site_by_path("contoso.sharepoint.com", "sites/../x").await.is_err());
    assert_eq!(c.site_drives(&site).await.unwrap().len(), 2);

    let lists = c.site_lists(&site).await.unwrap();
    assert_eq!(lists[0].template.as_deref(), Some("genericList"));
    assert_eq!(lists[0].site_id.as_ref(), Some(&site));
    let cols = c.list_columns(&site, &list).await.unwrap();
    assert_eq!(cols.len(), 3, "column without a name is skipped");
    assert_eq!(cols[0].kind.as_deref(), Some("text"));
    assert!(cols[0].required);
    assert_eq!(cols[1].choices, vec!["Open".to_owned(), "Done".to_owned()]);
    assert_eq!(cols[2].kind.as_deref(), Some("personOrGroup"));

    let items = c.list_items(&site, &list, &["Title", "Status"], 100).await.unwrap();
    let call = t.calls().last().unwrap().clone();
    assert_eq!(q(&call, "$expand").as_deref(), Some("fields($select=Title,Status)"));
    assert_eq!(q(&call, "$top").as_deref(), Some("100"));
    assert_eq!(items.items.len(), 2);
    assert_eq!(items.skipped, 1);
    assert_eq!(items.items[0].field_str("Title"), Some("Synthetic task"));
    assert_eq!(items.items[0].list_id.as_ref(), Some(&list));
    assert!(items.items[1].fields.is_empty());
    assert!(items.next_link.is_some());
    assert!(c.list_items(&site, &list, &["Bad Column"], 10).await.is_err());

    let mut fields = Map::new();
    fields.insert("Title".into(), json!("New"));
    let created = c.create_list_item(&site, &list, &fields).await.unwrap();
    assert_eq!(created.id.as_str(), "3");
    assert_eq!(body(t.calls().last().unwrap()), json!({"fields": {"Title": "New"}}));
    let item1 = ListItemId::new("1").unwrap();
    let updated = c.update_list_item(&site, &list, &item1, &fields, Some("\"1\"")).await.unwrap();
    assert_eq!(updated.get("Status"), Some(&json!("Done")));
    assert_eq!(t.calls().last().unwrap().header("if-match"), Some("\"1\""));
    assert!(c.create_list_item(&site, &list, &Map::new()).await.is_err());
    c.delete_list_item(&site, &list, &item1).await.unwrap();

    let pages = c.site_pages(&site).await.unwrap();
    assert_eq!(pages.items[0].title.as_str(), "Synthetic Home");
    assert_eq!(pages.items[0].page_layout.as_deref(), Some("home"));
    assert!(t.calls().iter().all(|r| r.url.path().starts_with("/v1.0/")), "v1.0 only, no beta");
    let req = sites::site_pages_request(&site);
    assert_eq!(req.bucket_key(), sites::SITES_BUCKET);
}

#[tokio::test]
async fn hostile_files_payloads_never_panic() {
    for body in [
        json!({"value": [null, 1, "x", {"id": 5}, {"id": "a", "size": -1, "file": "x", "folder": {"childCount": "many"}, "parentReference": [1]}]}),
        json!({"value": [{"id": "a", "remoteItem": "x", "parentReference": {"driveId": ""}}], "@odata.deltaLink": "https://graph.microsoft.com/v1.0/d"}),
        json!({"value": [{"id": "a", "remoteItem": {"id": 7, "parentReference": {"driveId": {"x": 1}}}, "lastModifiedBy": {"user": {"id": "\u{1}"}}}], "@odata.deltaLink": "https://graph.microsoft.com/v1.0/d"}),
        json!({"value": [{"@removed": {}}, {"deleted": {}, "id": ""}], "@odata.deltaLink": "https://graph.microsoft.com/v1.0/d"}),
        json!({"value": [{"id": "1", "fields": "x"}, {"id": "2", "fields": {"k": "v".repeat(300_000)}}]}),
    ] {
        let t = FixtureTransport::new();
        t.on(Method::Get, &format!("/drives/{D}/root/children"), FixtureResponse::json(200, &body));
        t.on(Method::Get, &format!("/drives/{D}/root/delta"), FixtureResponse::json(200, &body));
        t.on(Method::Get, "/me/drive/recent", FixtureResponse::json(200, &body));
        t.on(Method::Get, "/sites/s/lists/l/items", FixtureResponse::json(200, &body));
        t.on(Method::Get, &ipath("/permissions"), FixtureResponse::json(200, &body));
        t.on(Method::Get, &ipath("/thumbnails"), FixtureResponse::json(200, &body));
        let c = client(&t);
        let _ = c.list_root_children(&drive(), 10).await;
        let _ = c.drive_delta(&drive(), None).await;
        let _ = c.recent_files().await;
        let _ = c.list_permissions(&drive(), &item()).await;
        let _ = c.thumbnails(&drive(), &item()).await;
        let items = c.list_items(&SiteId::new("s").unwrap(), &ListId::new("l").unwrap(), &[], 10).await;
        if let Ok(p) = items {
            for i in p.items {
                assert!(i.field_str("k").is_none_or(|v| v.len() <= 256 * 1024));
            }
        }
    }
    let t = FixtureTransport::new();
    t.on(Method::Post, &ipath("/copy"), FixtureResponse::status(202));
    let dest = ItemLocation { drive: drive(), folder: item() };
    assert_eq!(
        client(&t).copy_drive_item(&drive(), &item(), &dest, None).await.unwrap_err().kind(),
        ErrorKind::Malformed
    );
    let t = FixtureTransport::new();
    t.on(Method::Get, "/m", FixtureResponse::bytes(202, b"not json".to_vec()));
    let m = SecretUrl::parse(&format!("{DL_HOST}/m")).unwrap();
    assert_eq!(client(&t).copy_status(&m).await.unwrap_err().kind(), ErrorKind::Malformed);
}
