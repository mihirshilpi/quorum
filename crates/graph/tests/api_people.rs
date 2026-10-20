//! People, directory, contacts and Microsoft Search wrappers against the fixture transport.

use quorum_graph::api::mail::Mailbox;
use quorum_graph::api::people::{self, NewContact, PhotoSize};
use quorum_graph::api::search::{self, EntityType, SearchHit, SearchQuery};
use quorum_graph::{
    Client, ErrorKind, FakeClock, FixtureResponse, FixtureTransport, HttpRequest, HttpResponse, Method, RateLimit,
    ResourceFamily, Route, StaticToken,
};
use quorum_model::{EmailAddress, Sensitive};
use serde_json::{Value, json};

fn client(t: &FixtureTransport) -> Client {
    let mut b = Client::builder(StaticToken::new("test-token"));
    for f in ResourceFamily::ALL {
        b = b.rate(f, RateLimit::new(10_000.0, 10_000).unwrap());
    }
    b.transport(t.clone()).clock(FakeClock::new()).jitter_seed(1).build().unwrap()
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

fn not_found() -> FixtureResponse {
    FixtureResponse::json(404, &json!({"error": {"code": "ImageNotFound", "message": "synthetic"}}))
}

#[tokio::test]
async fn me_user_manager_and_reports() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me", file("people/me.json"));
    t.on(Method::Get, "/me/manager", file("people/manager.json"));
    t.on(Method::Get, "/users/user02@contoso.example/manager", not_found());
    t.on(Method::Get, "/me/directReports", file("people/direct-reports.json"));
    let c = client(&t);

    let me = c.me().await.unwrap();
    assert_eq!(me.display_name.as_str(), "Test User");
    assert_eq!(me.email.as_ref().unwrap().as_str(), "test.user@contoso.example");
    assert_eq!(me.job_title, None);
    assert_eq!(me.phone.as_ref().map(Sensitive::as_str), Some("+1 555 0101"), "mobile when no business phone");
    let call = &t.calls()[0];
    assert_eq!(q(call, "$select").as_deref(), Some(people::USER_SELECT));

    let mgr = c.get_manager(&Mailbox::Me).await.unwrap().unwrap();
    assert_eq!(mgr.display_name.as_str(), "Avery Example");
    assert!(c.get_manager(&Mailbox::user("user02@contoso.example")).await.unwrap().is_none(), "404 = none");

    let reports = c.list_direct_reports(&Mailbox::Me).await.unwrap();
    assert_eq!(reports.items.len(), 1);
    assert_eq!(reports.skipped, 1, "org contacts are not users");
}

#[tokio::test]
async fn user_search_uses_eventual_consistency_and_maps_upn_fallback() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/users", file("people/users-search.json"));
    let c = client(&t);
    let page = c.search_users("Al\"ex\\", Some(10)).await.unwrap();
    let call = &t.calls()[0];
    assert_eq!(call.header("consistencylevel"), Some("eventual"));
    assert_eq!(q(call, "$search").as_deref(), Some("\"displayName:Alex\" OR \"mail:Alex\""));
    assert_eq!(q(call, "$count").as_deref(), Some("true"));
    assert_eq!(q(call, "$top").as_deref(), Some("10"));
    assert_eq!(page.items.len(), 3);
    assert_eq!(page.skipped, 2, "control-character id and missing id");
    assert!(page.items.iter().all(|p| p.id.is_some()));
    assert_eq!(page.items[1].email.as_ref().unwrap().as_str(), "alex.nomail@contoso.example");
    assert_eq!(page.items[1].phone, None);

    c.users_starting_with("O'Brien", None).await.unwrap();
    assert_eq!(
        q(&t.calls()[1], "$filter").as_deref(),
        Some("startswith(displayName,'O''Brien') or startswith(mail,'O''Brien')")
    );
    assert_eq!(people::search_users_request("  \"\" ", None).unwrap_err().kind(), ErrorKind::Config);
    assert_eq!(t.call_count(), 2);
}

#[tokio::test]
async fn people_group_members_and_paging() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me/people", file("people/people.json"));
    let members = "/groups/00000000-0000-0000-0000-00000000000e/members";
    t.on(Method::Get, members, file("people/group-members.json"));
    t.add(
        Route::new(Method::Get, members)
            .query("$skiptoken", "members-2")
            .respond(FixtureResponse::json(200, &json!({"value": []}))),
    );
    let c = client(&t);
    let ranked = c.list_people(Some("rank"), None).await.unwrap();
    assert_eq!(ranked.items.len(), 3);
    assert_eq!(ranked.items[0].email.as_ref().unwrap().as_str(), "ranked@contoso.example");
    assert_eq!(ranked.items[0].department, None, "empty strings become None");
    assert_eq!(ranked.items[0].phone.as_ref().map(Sensitive::as_str), Some("+1 555 0109"));
    assert_eq!(ranked.items[1].id, None);
    assert_eq!(ranked.items[1].email.as_ref().unwrap().as_str(), "outside@example.com", "first valid address");
    assert_eq!(q(&t.calls()[0], "$search").as_deref(), Some("\"rank\""));

    let req = people::group_members_request("00000000-0000-0000-0000-00000000000e").unwrap();
    let page = c.users_page(req.clone()).await.unwrap();
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.skipped, 1, "nested group");
    let next = c.users_page(req.follow(page.next_link.unwrap())).await.unwrap();
    assert!(next.items.is_empty());
}

#[tokio::test]
async fn photos_by_size_and_missing_photo() {
    let t = FixtureTransport::new();
    t.on(
        Method::Get,
        "/me/photos/96x96/$value",
        FixtureResponse::Http(HttpResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "image/jpeg".into())],
            body: vec![0xff, 0xd8, 0xff],
        }),
    );
    t.on(Method::Get, "/users/00000000-0000-0000-0000-000000000003/photo/$value", not_found());
    let c = client(&t);
    let photo = c.get_photo(&Mailbox::Me, Some(PhotoSize::S96)).await.unwrap().unwrap();
    assert_eq!(photo.bytes, vec![0xff, 0xd8, 0xff]);
    assert_eq!(photo.content_type.as_deref(), Some("image/jpeg"));
    assert_eq!(t.calls()[0].header("accept"), Some("image/*"));
    assert!(!format!("{photo:?}").contains("255"));
    let none = c.get_photo(&Mailbox::user("00000000-0000-0000-0000-000000000003"), None).await.unwrap();
    assert!(none.is_none());
}

#[tokio::test]
async fn contacts_list_and_create() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me/contacts", file("people/contacts.json"));
    t.on(
        Method::Post,
        "/me/contacts",
        FixtureResponse::json(201, &json!({"id": "AAMkAD-contact-new=", "displayName": "Dana New"})),
    );
    let c = client(&t);
    let page = c.list_contacts(&Mailbox::Me, None).await.unwrap();
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.skipped, 1);
    let casey = &page.items[0];
    assert_eq!(casey.person.email.as_ref().unwrap().as_str(), "casey@example.com");
    assert_eq!(casey.person.phone.as_ref().map(Sensitive::as_str), Some("+1 555 0110"));
    assert_eq!(casey.company_name.as_ref().map(Sensitive::as_str), Some("Example Ltd"));
    assert!(page.items[1].person.email.is_none());
    assert!(page.items[1].company_name.is_none());
    assert_eq!(people::list_contacts_request(&Mailbox::Me, None).unwrap().resource_family(), ResourceFamily::Mail);

    let new = NewContact {
        given_name: Some("Dana".into()),
        surname: Some("New".into()),
        email_addresses: vec![EmailAddress::new("Dana", "dana@example.com").unwrap(), EmailAddress::default()],
        business_phones: vec!["+1 555 0111".into()],
        ..Default::default()
    };
    let created = c.create_contact(&Mailbox::Me, &new).await.unwrap();
    assert_eq!(created.id, "AAMkAD-contact-new=");
    assert_eq!(
        body(&t.calls()[1]),
        json!({
            "givenName": "Dana",
            "surname": "New",
            "emailAddresses": [{"address": "dana@example.com", "name": "Dana"}],
            "businessPhones": ["+1 555 0111"]
        })
    );
    assert_eq!(
        people::create_contact_request(&Mailbox::Me, &NewContact::default()).unwrap_err().kind(),
        ErrorKind::Config
    );
}

#[tokio::test]
async fn search_query_body_paging_and_hit_mapping() {
    let t = FixtureTransport::new();
    t.on(Method::Post, "/search/query", file("people/search-mixed.json"));
    let c = client(&t);
    let query =
        SearchQuery { size: 100, ..SearchQuery::new(vec![EntityType::Message, EntityType::Message], "synthetic") };
    assert!(!format!("{query:?}").contains("synthetic"));
    let page = c.search(&query).await.unwrap();
    let sent = body(&t.calls()[0]);
    assert_eq!(
        sent,
        json!({"requests": [{"entityTypes": ["message"], "query": {"queryString": "synthetic"}, "from": 0, "size": 25}]})
    );
    assert_eq!(search::search_request(&query).unwrap().resource_family(), ResourceFamily::Mail);

    assert_eq!(page.results.len(), 8);
    assert_eq!(page.skipped, 0);
    assert_eq!(page.total, Some(6));
    assert!(page.more_results_available);
    assert_eq!(page.next_from, Some(8));
    assert!(
        matches!(&page.results[0].hit, SearchHit::Message(m) if m.from.as_ref().unwrap().display() == "Avery Example")
    );
    assert_eq!(page.results[0].rank, Some(1));
    assert!(page.results[0].summary.as_str().contains("<c0>"));
    assert!(matches!(&page.results[1].hit, SearchHit::Event(e) if e.subject() == "Synthetic meeting"));
    match &page.results[2].hit {
        SearchHit::DriveItem(d) => {
            assert_eq!(d.drive_id.as_deref(), Some("b!synthetic-drive"));
            assert_eq!(d.size, Some(2048));
        }
        other => panic!("{other:?}"),
    }
    assert!(
        matches!(&page.results[3].hit, SearchHit::ChatMessage(m) if m.chat_id.as_deref() == Some("19:synthetic@thread.v2") && m.from.as_str() == "Avery Example")
    );
    assert!(
        matches!(&page.results[4].hit, SearchHit::Person(p) if p.email.as_ref().unwrap().as_str() == "user03@contoso.example")
    );
    assert!(matches!(&page.results[5].hit, SearchHit::Site(s) if s.name.as_str() == "Synthetic site"));
    assert!(matches!(&page.results[6].hit, SearchHit::ListItem(_)));
    match &page.results[7].hit {
        SearchHit::Unknown { odata_type, resource } => {
            assert_eq!(odata_type, "#microsoft.graph.bookmark");
            assert!(!format!("{resource:?}").contains("Future"));
        }
        other => panic!("{other:?}"),
    }

    let files = SearchQuery { size: 1000, ..SearchQuery::new(vec![EntityType::DriveItem], "x") }.page_from(25);
    assert_eq!(files.effective_size(), 500);
    let req = search::search_request(&files).unwrap();
    assert_eq!(req.resource_family(), ResourceFamily::Files);
    assert!(req.is_idempotent());
    assert_eq!(search::search_request(&SearchQuery::new(vec![], "x")).unwrap_err().kind(), ErrorKind::Config);
    assert_eq!(
        search::search_request(&SearchQuery::new(vec![EntityType::Site], "\u{0}  ")).unwrap_err().kind(),
        ErrorKind::Config
    );
}

#[tokio::test]
async fn hostile_search_responses_never_panic() {
    let t = FixtureTransport::new();
    t.add(
        Route::new(Method::Post, "/search/query")
            .respond(file("people/search-hostile.json"))
            .respond(FixtureResponse::json(200, &json!({"unexpected": true})))
            .respond(FixtureResponse::file(429, "throttled.json").unwrap().with_header("Retry-After", "1"))
            .respond(FixtureResponse::json(200, &json!({"value": []}))),
    );
    let c = client(&t);
    let q = SearchQuery::new(vec![EntityType::Site], "synthetic");
    let page = c.search(&q).await.unwrap();
    assert_eq!(page.results.len(), 1);
    assert_eq!(page.skipped, 5);
    assert_eq!(page.results[0].rank, None, "negative rank is dropped");
    assert_eq!(page.total, None);
    assert!(!page.more_results_available);
    assert_eq!(page.next_from, None);

    assert_eq!(c.search(&q).await.unwrap_err().kind(), ErrorKind::Malformed);
    // 429 then success: search is a read, so it is retried.
    let page = c.search(&q).await.unwrap();
    assert!(page.results.is_empty());
    assert_eq!(t.call_count(), 4);
}
