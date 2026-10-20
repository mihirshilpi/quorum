//! Mail endpoint wrappers against the fixture transport: request shape, mapping, delta, errors.

use quorum_graph::api::mail::{
    self, Category, Change, DeltaOptions, FileAttachmentUpload, FolderRef, ListOptions, MESSAGE_SELECT, Mailbox,
    MailboxSettings, MessageRule, MessageUpdate, OutgoingMessage, ReplyKind,
};
use quorum_graph::{
    Client, DEFAULT_UPLOAD_HOST_SUFFIXES, DeltaOutcome, ErrorKind, FakeClock, FixtureResponse, FixtureTransport,
    HttpRequest, Method, RateLimit, ResourceFamily, Route, StaticToken,
};
use quorum_model::{
    AttachmentId, AttachmentKind, EmailAddress, FlagStatus, Importance, InferenceClassification, MailFolderId,
    MessageBody, MessageId, Sensitive, WellKnownFolder,
};
use serde_json::{Value, json};

const GRAPH: &str = "https://graph.microsoft.com/v1.0";

fn client(t: &FixtureTransport) -> Client {
    let mut b = Client::builder(StaticToken::new("test-token"));
    for f in ResourceFamily::ALL {
        b = b.rate(f, RateLimit::new(10_000.0, 10_000).unwrap());
    }
    let hosts: Vec<&str> =
        DEFAULT_UPLOAD_HOST_SUFFIXES.iter().chain(mail::MAIL_UPLOAD_HOST_SUFFIXES).copied().collect();
    b.transport(t.clone()).clock(FakeClock::new()).jitter_seed(1).upload_host_suffixes(hosts).build().unwrap()
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

fn mid(s: &str) -> MessageId {
    MessageId::new(s).unwrap()
}

fn addr(name: &str, a: &str) -> EmailAddress {
    EmailAddress::new(name, a).unwrap()
}

#[tokio::test]
async fn list_messages_sends_select_top_orderby_and_immutable_ids_then_follows_next_link() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me/mailFolders/inbox/messages", file("mail/inbox-messages-page1.json"));
    t.add(
        Route::new(Method::Get, "/me/mailFolders/inbox/messages")
            .query("$skiptoken", "inbox-page2")
            .respond(file("mail/inbox-messages-page2.json")),
    );
    let c = client(&t);
    let req = mail::list_messages_request(&Mailbox::Me, &FolderRef::inbox(), &ListOptions::default()).unwrap();
    assert_eq!(req.resource_family(), ResourceFamily::Mail);
    assert_eq!(req.bucket_key(), "me");
    let page = c.messages_page(req.clone()).await.unwrap();
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.skipped, 0);

    let call = &t.calls()[0];
    assert_eq!(call.method, Method::Get);
    assert_eq!(call.url.path(), "/v1.0/me/mailFolders/inbox/messages");
    assert_eq!(q(call, "$select").as_deref(), Some(MESSAGE_SELECT));
    assert_eq!(q(call, "$top").as_deref(), Some("50"));
    assert_eq!(q(call, "$orderby").as_deref(), Some("receivedDateTime desc"));
    assert_eq!(call.header("prefer"), Some("IdType=\"ImmutableId\""));

    let m = &page.items[0];
    assert_eq!(m.id.as_str(), "AAMkAD-msg-0001=");
    assert_eq!(m.importance, Importance::High);
    assert!(m.is_flagged());
    assert!(m.is_read && m.has_attachments);
    assert_eq!(m.categories, vec!["Blue category".to_owned()]);
    assert_eq!(m.from.as_ref().unwrap().address.as_ref().unwrap().as_str(), "avery@contoso.example");
    assert_eq!(m.to_recipients.len(), 1);
    let m2 = &page.items[1];
    assert!(m2.sender.as_ref().unwrap().address.is_none(), "X.500 sender has no SMTP address");
    assert!(m2.cc_recipients.is_empty());
    assert_eq!(m2.inference_classification, InferenceClassification::Other);

    let next = req.follow(page.next_link.unwrap());
    let page2 = c.messages_page(next).await.unwrap();
    assert_eq!(page2.items.len(), 1);
    assert!(page2.next_link.is_none());
    let call2 = &t.calls()[1];
    assert_eq!(q(call2, "$skiptoken").as_deref(), Some("inbox-page2"));
    assert_eq!(call2.header("prefer"), Some("IdType=\"ImmutableId\""));
}

#[tokio::test]
async fn list_options_and_shared_mailbox_paths() {
    let opts = ListOptions { top: Some(5000), filter: Some("isRead eq false".into()), order_by: None };
    let mb = Mailbox::user("Shared.Desk@contoso.example");
    let folder = FolderRef::Id(MailFolderId::new("AAMk/odd id?").unwrap());
    let req = mail::list_messages_request(&mb, &folder, &opts).unwrap();
    assert_eq!(req.target(), "/users/Shared.Desk@contoso.example/mailFolders/AAMk%2Fodd%20id%3F/messages");
    assert_eq!(req.bucket_key(), "shared.desk@contoso.example");

    let t = FixtureTransport::new();
    t.on(
        Method::Get,
        "/users/Shared.Desk@contoso.example/mailFolders/AAMk%2Fodd%20id%3F/messages",
        FixtureResponse::json(200, &json!({"value": []})),
    );
    let page = client(&t).messages_page(req).await.unwrap();
    assert!(page.items.is_empty());
    let call = &t.calls()[0];
    assert_eq!(q(call, "$top").as_deref(), Some("1000"), "top is clamped");
    assert_eq!(q(call, "$filter").as_deref(), Some("isRead eq false"));

    assert_eq!(Mailbox::user("..").prefix().unwrap_err().kind(), ErrorKind::Config);
    assert_eq!(Mailbox::user("").prefix().unwrap_err().kind(), ErrorKind::Config);
}

#[tokio::test]
async fn get_message_asks_for_html_body() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me/messages/AAMkAD-msg-0001=", file("mail/message-with-body.json"));
    let m = client(&t).get_message(&Mailbox::Me, &mid("AAMkAD-msg-0001=")).await.unwrap();
    assert_eq!(m.message.subject(), "Synthetic subject 1");
    assert_eq!(m.body.content_type, quorum_model::BodyType::Html);
    assert!(m.body.content.as_str().contains("<b>HTML</b>"));
    let call = &t.calls()[0];
    assert_eq!(call.header("prefer"), Some("IdType=\"ImmutableId\", outlook.body-content-type=\"html\""));
    assert!(q(call, "$select").unwrap().ends_with(",body"));
}

#[tokio::test]
async fn hostile_messages_parse_without_panics_and_count_skips() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me/mailFolders/inbox/messages", file("mail/messages-hostile.json"));
    let page = client(&t).list_messages(&Mailbox::Me, &FolderRef::inbox(), &ListOptions::default()).await.unwrap();
    assert_eq!(page.items.len(), 4);
    assert_eq!(page.skipped, 6, "wrong types, missing/invalid ids, bad dates and non-objects are skipped");
    let ids: Vec<&str> = page.items.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["AAMkAD-msg-min=", "AAMkAD-msg-0006=", "AAMkAD-msg-0008=", "AAMkAD-msg-0010="]);
    let unknown = &page.items[1];
    assert_eq!(unknown.importance, Importance::Unknown("superUrgent".into()));
    assert_eq!(unknown.flag_status, FlagStatus::Unknown("tomorrowish".into()));
    assert_eq!(unknown.inference_classification, InferenceClassification::Unknown("somethingNew".into()));
    let weird = &page.items[2];
    assert!(weird.from.is_none());
    assert_eq!(weird.to_recipients.len(), 3);
    assert!(weird.to_recipients.iter().all(|r| r.address.is_none()));
    assert_eq!(page.items[3].flag_status, FlagStatus::NotFlagged);
    assert!(page.items[3].categories.is_empty());
}

#[tokio::test]
async fn message_delta_follows_next_link_to_delta_link_with_removed_items() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me/mailFolders/inbox/messages/delta", file("mail/messages-delta-page1.json"));
    t.add(
        Route::new(Method::Get, "/me/mailFolders/inbox/messages/delta")
            .query("$skiptoken", "mail-delta-2")
            .respond(file("mail/messages-delta-page2.json")),
    );
    t.add(
        Route::new(Method::Get, "/me/mailFolders/inbox/messages/delta")
            .query("$deltatoken", "expired")
            .respond(FixtureResponse::file(410, "delta-expired.json").unwrap()),
    );
    let c = client(&t);
    let opts = DeltaOptions { page_size: Some(25), received_since: None };
    let start = mail::messages_delta_request(&Mailbox::Me, &FolderRef::inbox(), &opts).unwrap();

    let first = c.messages_delta_page(start.clone()).await.unwrap();
    assert!(matches!(first.outcome, DeltaOutcome::Next(_)));
    assert_eq!(first.items.len(), 2);
    assert!(matches!(&first.items[1], Change::Removed(id) if id.as_str() == "AAMkAD-msg-0009="));

    let round = c.messages_delta_round(start, 10).await.unwrap();
    assert_eq!(round.pages, 2);
    assert_eq!(round.changes.len(), 3);
    assert_eq!(round.delta_link, format!("{GRAPH}/me/mailFolders/inbox/messages/delta?$deltatoken=mail-token-1"));
    match &round.changes[2] {
        Change::Upsert(m) => assert_eq!(m.flag_status, FlagStatus::Complete),
        other => panic!("{other:?}"),
    }
    let calls = t.calls();
    let first_call = &calls[0];
    assert_eq!(q(first_call, "$select").as_deref(), Some(MESSAGE_SELECT));
    assert_eq!(first_call.header("prefer"), Some("IdType=\"ImmutableId\", odata.maxpagesize=25"));
    assert_eq!(calls[2].header("prefer"), Some("IdType=\"ImmutableId\", odata.maxpagesize=25"), "next keeps Prefer");

    // A stored, expired delta link maps to DeltaExpired.
    let stored = mail::delta_link_request(
        &Mailbox::Me,
        format!("{GRAPH}/me/mailFolders/inbox/messages/delta?$deltatoken=expired"),
        Some(25),
    );
    let err = c.messages_delta_page(stored).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::DeltaExpired);
    assert_eq!(t.calls().last().unwrap().header("prefer"), Some("IdType=\"ImmutableId\", odata.maxpagesize=25"));
}

#[tokio::test]
async fn delta_filter_and_backfill_cursor() {
    let since: jiff::Timestamp = "2026-09-01T00:00:00Z".parse().unwrap();
    let req = mail::messages_delta_request(
        &Mailbox::Me,
        &FolderRef::inbox(),
        &DeltaOptions { page_size: None, received_since: Some(since) },
    )
    .unwrap();
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me/mailFolders/inbox/messages/delta", file("mail/messages-delta-page2.json"));
    t.on(Method::Get, "/me/mailFolders/inbox/messages", file("mail/inbox-messages-page1.json"));
    let c = client(&t);
    c.messages_delta_page(req).await.unwrap();
    assert_eq!(q(&t.calls()[0], "$filter").as_deref(), Some("receivedDateTime ge 2026-09-01T00:00:00Z"));

    let before: jiff::Timestamp = "2026-10-20T00:00:00Z".parse().unwrap();
    let b = c.backfill_messages(&Mailbox::Me, &FolderRef::inbox(), before, Some(2)).await.unwrap();
    assert_eq!(b.page.items.len(), 2);
    assert_eq!(b.older_than, Some("2026-10-18T09:00:00Z".parse().unwrap()));
    let call = &t.calls()[1];
    assert_eq!(q(call, "$filter").as_deref(), Some("receivedDateTime lt 2026-10-20T00:00:00Z"));
    assert_eq!(q(call, "$orderby").as_deref(), Some("receivedDateTime desc"));
    assert_eq!(q(call, "$top").as_deref(), Some("2"));
}

#[tokio::test]
async fn folders_list_get_delta_and_well_known_batch() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me/mailFolders", file("mail/folders.json"));
    t.on(Method::Get, "/me/mailFolders/delta", file("mail/folders-delta.json"));
    t.on(
        Method::Get,
        "/me/mailFolders/inbox",
        FixtureResponse::json(200, &json!({"id": "AAMkAD-folder-inbox=", "displayName": "Inbox"})),
    );
    t.on(
        Method::Post,
        "/$batch",
        FixtureResponse::json(
            200,
            &json!({"responses": [
                {"id": "f1", "status": 404, "body": {"error": {"code": "ErrorFolderNotFound"}}},
                {"id": "f0", "status": 200, "body": {"id": "AAMkAD-folder-inbox="}}
            ]}),
        ),
    );
    let c = client(&t);
    let page = c.list_mail_folders(&Mailbox::Me).await.unwrap();
    assert_eq!(page.items.len(), 3);
    assert_eq!(page.skipped, 2, "empty id and negative count are skipped");
    assert_eq!(page.items[0].unread_item_count, 3);
    assert!(page.items[2].is_hidden);
    assert_eq!(q(&t.calls()[0], "includeHiddenFolders").as_deref(), Some("true"));

    let inbox = c.get_mail_folder(&Mailbox::Me, &FolderRef::inbox()).await.unwrap();
    assert_eq!(inbox.well_known_name, Some(WellKnownFolder::Inbox));

    let delta = c.mail_folders_delta_page(mail::folders_delta_request(&Mailbox::Me).unwrap()).await.unwrap();
    assert!(matches!(delta.outcome, DeltaOutcome::Done(ref l) if l.ends_with("folders-token-1")));
    assert!(matches!(&delta.items[1], Change::Removed(id) if id.as_str() == "AAMkAD-folder-old="));

    let resolved =
        c.resolve_well_known_folders(&Mailbox::Me, &[WellKnownFolder::Inbox, WellKnownFolder::Archive]).await.unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].0, WellKnownFolder::Inbox);
    let batch = body(t.calls().last().unwrap());
    assert_eq!(batch["requests"][1]["url"], "/me/mailFolders/archive?%24select=id");
    assert_eq!(batch["requests"][0]["headers"]["Prefer"], "IdType=\"ImmutableId\"");
}

#[tokio::test]
async fn attachments_map_kind_from_odata_type_and_download_bytes() {
    let t = FixtureTransport::new();
    let base = "/me/messages/AAMkAD-msg-0001=/attachments";
    t.on(Method::Get, base, file("mail/attachments.json"));
    t.on(Method::Get, &format!("{base}/AAMkAD-att-0007="), file("mail/attachment-inline.json"));
    t.on(
        Method::Get,
        &format!("{base}/AAMkAD-att-0001=/$value"),
        FixtureResponse::bytes(200, b"%PDF-synthetic".to_vec()),
    );
    let c = client(&t);
    let id = mid("AAMkAD-msg-0001=");
    let page = c.list_attachments(&Mailbox::Me, &id).await.unwrap();
    assert_eq!(page.items.len(), 5);
    assert_eq!(page.skipped, 2);
    let kinds: Vec<AttachmentKind> = page.items.iter().map(|a| a.kind.clone()).collect();
    assert_eq!(
        kinds,
        vec![
            AttachmentKind::File,
            AttachmentKind::Item,
            AttachmentKind::Reference,
            AttachmentKind::Unknown("future".into()),
            AttachmentKind::Unknown(String::new()),
        ]
    );
    assert_eq!(page.items[0].size, 12345);
    assert_eq!(q(&t.calls()[0], "$select").as_deref(), Some(mail::ATTACHMENT_SELECT));

    let inline = c.get_attachment(&Mailbox::Me, &id, &AttachmentId::new("AAMkAD-att-0007=").unwrap()).await.unwrap();
    assert!(inline.is_inline);
    assert_eq!(inline.content_id.as_deref(), Some("image001.png@01D00000.00000000"));

    let bytes = c.attachment_content(&Mailbox::Me, &id, &AttachmentId::new("AAMkAD-att-0001=").unwrap()).await.unwrap();
    assert_eq!(bytes, b"%PDF-synthetic");
    let call = t.calls().pop().unwrap();
    assert_eq!(call.url.path(), "/v1.0/me/messages/AAMkAD-msg-0001=/attachments/AAMkAD-att-0001=/$value");
    assert_eq!(call.header("accept"), Some("*/*"));
}

#[tokio::test]
async fn small_attachment_is_base64_and_large_one_uses_upload_session() {
    let t = FixtureTransport::new();
    let base = "/me/messages/AAMkAD-msg-0001=/attachments";
    t.on(
        Method::Post,
        base,
        FixtureResponse::json(
            201,
            &json!({"@odata.type": "#microsoft.graph.fileAttachment", "id": "AAMkAD-att-new=", "name": "a.txt", "size": 3}),
        ),
    );
    t.on(Method::Post, &format!("{base}/createUploadSession"), file("mail/upload-session.json"));
    t.on(
        Method::Put,
        "/api/v2.0/Users('00000000-0000-0000-0000-000000000001')/Messages('AAMkAD-msg-0001=')/AttachmentSessions('synthetic')",
        FixtureResponse::status(201),
    );
    let c = client(&t);
    let id = mid("AAMkAD-msg-0001=");
    let small = FileAttachmentUpload {
        name: Sensitive::new("a.txt"),
        content_type: Some("text/plain".into()),
        bytes: b"abc".to_vec(),
        ..Default::default()
    };
    let att = c.add_attachment(&Mailbox::Me, &id, &small).await.unwrap();
    assert_eq!(att.kind, AttachmentKind::File);
    let sent = body(&t.calls()[0]);
    assert_eq!(sent["contentBytes"], "YWJj");
    assert_eq!(sent["@odata.type"], "#microsoft.graph.fileAttachment");
    assert!(!format!("{small:?}").contains("abc"), "Debug does not print bytes");

    let big = FileAttachmentUpload { bytes: vec![0; mail::MAX_INLINE_ATTACHMENT_BYTES + 1], ..small.clone() };
    assert_eq!(mail::add_attachment_request(&Mailbox::Me, &id, &big).unwrap_err().kind(), ErrorKind::TooLarge);

    let data = vec![7u8; 1000];
    let session = c.create_attachment_upload_session(&Mailbox::Me, &id, "big.bin", 1000, false, None).await.unwrap();
    let create = body(&t.calls()[1]);
    assert_eq!(create["AttachmentItem"]["attachmentType"], "file");
    assert_eq!(create["AttachmentItem"]["size"], 1000);
    c.upload_attachment(&session, data.as_slice()).await.unwrap();
    let put = t.calls().pop().unwrap();
    assert_eq!(put.method, Method::Put);
    assert_eq!(put.url.host_str(), Some("outlook.office.com"));
    assert_eq!(put.header("content-range"), Some("bytes 0-999/1000"));
    assert!(put.header("authorization").is_none(), "pre-authenticated URL gets no bearer token");
    assert_eq!(
        mail::attachment_upload_session_request(&Mailbox::Me, &id, "x", 0, false, None).unwrap_err().kind(),
        ErrorKind::Config
    );
}

#[tokio::test]
async fn send_mail_body_shape_and_no_blind_retry() {
    let t = FixtureTransport::new();
    t.add(
        Route::new(Method::Post, "/me/sendMail")
            .respond(FixtureResponse::status(503))
            .respond(FixtureResponse::status(202)),
    );
    let c = client(&t);
    let msg = OutgoingMessage {
        subject: Sensitive::new("Synthetic hello"),
        body: MessageBody { content_type: quorum_model::BodyType::Html, content: Sensitive::new("<p>Hi</p>") },
        to: vec![addr("Avery Example", "avery@contoso.example")],
        cc: vec![addr("", "cc@example.com")],
        importance: Some(Importance::High),
        ..Default::default()
    };
    let err = c.send_mail(&Mailbox::Me, &msg, true).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Server);
    assert_eq!(t.call_count(), 1, "sendMail is not idempotent and is never retried");
    c.send_mail(&Mailbox::Me, &msg, true).await.unwrap();
    let sent = body(&t.calls()[1]);
    assert_eq!(
        sent,
        json!({
            "message": {
                "subject": "Synthetic hello",
                "body": {"contentType": "html", "content": "<p>Hi</p>"},
                "toRecipients": [{"emailAddress": {"name": "Avery Example", "address": "avery@contoso.example"}}],
                "ccRecipients": [{"emailAddress": {"address": "cc@example.com"}}],
                "importance": "high"
            },
            "saveToSentItems": true
        })
    );
    let none = OutgoingMessage::default();
    assert_eq!(mail::send_mail_request(&Mailbox::Me, &none, true).unwrap_err().kind(), ErrorKind::Config);
    let bad = OutgoingMessage { to: vec![EmailAddress::default()], ..Default::default() };
    assert_eq!(mail::send_mail_request(&Mailbox::Me, &bad, true).unwrap_err().kind(), ErrorKind::Config);
}

#[tokio::test]
async fn drafts_replies_forward_and_send() {
    let t = FixtureTransport::new();
    let draft = json!({"id": "AAMkAD-draft-1=", "isDraft": true, "subject": "RE: Synthetic"});
    t.on(Method::Post, "/me/messages", FixtureResponse::json(201, &draft));
    t.on(Method::Post, "/me/messages/AAMkAD-msg-0001=/createReplyAll", FixtureResponse::json(201, &draft));
    t.on(Method::Post, "/me/messages/AAMkAD-msg-0001=/createForward", FixtureResponse::json(201, &draft));
    t.on(Method::Patch, "/me/messages/AAMkAD-draft-1=", FixtureResponse::json(200, &draft));
    t.on(Method::Post, "/me/messages/AAMkAD-draft-1=/send", FixtureResponse::status(202));
    let c = client(&t);
    let id = mid("AAMkAD-msg-0001=");
    let created =
        c.create_draft(&Mailbox::Me, &OutgoingMessage { subject: "S".into(), ..Default::default() }).await.unwrap();
    assert!(created.is_draft);
    let r = c.create_response_draft(&Mailbox::Me, &id, ReplyKind::ReplyAll, None).await.unwrap();
    assert_eq!(r.id.as_str(), "AAMkAD-draft-1=");
    assert_eq!(body(&t.calls()[1]), json!({}));
    let fwd = OutgoingMessage { to: vec![addr("", "fwd@example.com")], ..Default::default() };
    c.create_response_draft(&Mailbox::Me, &id, ReplyKind::Forward, Some(&fwd)).await.unwrap();
    assert_eq!(
        body(&t.calls()[2]),
        json!({"message": {"toRecipients": [{"emailAddress": {"address": "fwd@example.com"}}]}})
    );
    let with_att = OutgoingMessage {
        subject: "Autosave".into(),
        attachments: vec![FileAttachmentUpload { name: "a".into(), bytes: vec![1], ..Default::default() }],
        ..Default::default()
    };
    c.update_draft(&Mailbox::Me, &mid("AAMkAD-draft-1="), &with_att).await.unwrap();
    assert_eq!(body(&t.calls()[3]), json!({"subject": "Autosave"}), "PATCH never carries attachments");
    c.send_draft(&Mailbox::Me, &mid("AAMkAD-draft-1=")).await.unwrap();
    assert_eq!(t.calls()[4].method, Method::Post);
    for call in t.calls() {
        assert!(call.header("prefer").unwrap().contains("ImmutableId"));
    }
}

#[tokio::test]
async fn update_move_copy_delete_and_error_mapping() {
    let t = FixtureTransport::new();
    let path = "/me/messages/AAMkAD-msg-0001=";
    let msg = json!({"id": "AAMkAD-msg-0001=", "isRead": true, "flag": {"flagStatus": "flagged"}});
    t.add(
        Route::new(Method::Patch, path)
            .respond(FixtureResponse::file(429, "throttled.json").unwrap().with_header("Retry-After", "1"))
            .respond(FixtureResponse::json(200, &msg)),
    );
    t.on(Method::Post, &format!("{path}/move"), FixtureResponse::json(201, &msg));
    t.on(Method::Post, &format!("{path}/copy"), FixtureResponse::json(201, &json!({"id": "AAMkAD-copy="})));
    t.on(
        Method::Delete,
        path,
        FixtureResponse::json(404, &json!({"error": {"code": "ErrorItemNotFound", "message": "synthetic"}})),
    );
    let c = client(&t);
    let id = mid("AAMkAD-msg-0001=");
    let update = MessageUpdate {
        is_read: Some(true),
        flag: Some(FlagStatus::Flagged),
        categories: Some(vec!["Blue category".into()]),
        importance: Some(Importance::Low),
        inference_classification: Some(InferenceClassification::Other),
    };
    let m = c.update_message(&Mailbox::Me, &id, &update).await.unwrap();
    assert!(m.is_read && m.is_flagged());
    assert_eq!(t.call_count(), 2, "PATCH with absolute values is retried after 429");
    assert_eq!(
        body(&t.calls()[1]),
        json!({
            "isRead": true,
            "flag": {"flagStatus": "flagged"},
            "categories": ["Blue category"],
            "importance": "low",
            "inferenceClassification": "other"
        })
    );
    assert_eq!(
        mail::update_message_request(&Mailbox::Me, &id, &MessageUpdate::default()).unwrap_err().kind(),
        ErrorKind::Config
    );

    let archive = FolderRef::WellKnown(WellKnownFolder::Archive);
    c.move_message(&Mailbox::Me, &id, &archive).await.unwrap();
    assert_eq!(body(&t.calls()[2]), json!({"destinationId": "archive"}));
    let copy =
        c.copy_message(&Mailbox::Me, &id, &FolderRef::Id(MailFolderId::new("AAMkAD-f=").unwrap())).await.unwrap();
    assert_eq!(copy.id.as_str(), "AAMkAD-copy=");
    assert_eq!(body(&t.calls()[3]), json!({"destinationId": "AAMkAD-f="}));

    let err = c.delete_message(&Mailbox::Me, &id).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotFound);
    assert_eq!(err.code(), Some("ErrorItemNotFound"));
    assert!(!format!("{err:?}").contains("synthetic"), "Graph's message is not printed");
}

#[tokio::test]
async fn mailbox_settings_rules_categories_and_overrides() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me/mailboxSettings", file("mail/mailbox-settings.json"));
    t.on(Method::Patch, "/me/mailboxSettings", FixtureResponse::json(200, &json!({"timeZone": "UTC"})));
    t.on(Method::Get, "/me/mailFolders/inbox/messageRules", file("mail/rules.json"));
    t.on(Method::Post, "/me/mailFolders/inbox/messageRules", FixtureResponse::json(201, &json!({"id": "AQAAAJ-new="})));
    t.on(Method::Delete, "/me/mailFolders/inbox/messageRules/AQAAAJ-rule-1=", FixtureResponse::status(204));
    t.on(Method::Get, "/me/outlook/masterCategories", file("mail/categories.json"));
    t.on(
        Method::Post,
        "/me/outlook/masterCategories",
        FixtureResponse::json(201, &json!({"id": "c3", "displayName": "New", "color": "preset2"})),
    );
    t.on(Method::Get, "/me/inferenceClassification/overrides", file("mail/overrides.json"));
    t.on(
        Method::Post,
        "/me/inferenceClassification/overrides",
        FixtureResponse::json(
            201,
            &json!({"id": "o1", "classifyAs": "other", "senderEmailAddress": {"address": "bulk@example.com"}}),
        ),
    );
    let c = client(&t);
    let mb = Mailbox::Me;

    let s = c.get_mailbox_settings(&mb).await.unwrap();
    let ar = s.automatic_replies_setting.as_ref().unwrap();
    assert_eq!(ar.status, mail::AutomaticRepliesStatus::Scheduled);
    assert_eq!(ar.external_audience, Some(mail::ExternalAudience::ContactsOnly));
    assert_eq!(s.time_zone.as_deref(), Some("W. Europe Standard Time"));
    assert_eq!(s.working_hours.as_ref().unwrap().days_of_week.len(), 5);
    let patch = MailboxSettings { time_zone: Some("UTC".into()), ..Default::default() };
    c.update_mailbox_settings(&mb, &patch).await.unwrap();
    assert_eq!(body(&t.calls()[1]), json!({"timeZone": "UTC"}));
    assert_eq!(
        mail::update_mailbox_settings_request(&mb, &MailboxSettings::default()).unwrap_err().kind(),
        ErrorKind::Config
    );

    let rules = c.list_message_rules(&mb).await.unwrap();
    assert_eq!(rules.items.len(), 2);
    assert!(rules.items[1].has_error && rules.items[1].is_read_only);
    assert!(!format!("{:?}", rules.items[0]).contains("newsletter@"), "rule conditions are redacted in Debug");
    let new_rule = MessageRule {
        display_name: "Synthetic".into(),
        sequence: 2,
        is_enabled: true,
        actions: mail::SensitiveJson(json!({"markAsRead": true})),
        ..Default::default()
    };
    c.create_message_rule(&mb, &new_rule).await.unwrap();
    assert_eq!(
        body(&t.calls()[3]),
        json!({"displayName": "Synthetic", "sequence": 2, "isEnabled": true, "actions": {"markAsRead": true}})
    );
    c.delete_message_rule(&mb, "AQAAAJ-rule-1=").await.unwrap();

    let cats = c.list_master_categories(&mb).await.unwrap();
    assert_eq!(cats.items[0].color, "preset7");
    let created = c
        .create_master_category(&mb, &Category { id: None, display_name: "New".into(), color: "preset2".into() })
        .await
        .unwrap();
    assert_eq!(created.id.as_deref(), Some("c3"));
    assert_eq!(body(&t.calls()[6]), json!({"displayName": "New", "color": "preset2"}));

    let overrides = c.list_focused_overrides(&mb).await.unwrap();
    assert_eq!(overrides.items.len(), 2);
    assert_eq!(overrides.skipped, 1);
    assert_eq!(overrides.items[1].classify_as, InferenceClassification::Other);
    c.create_focused_override(&mb, &addr("Bulk", "bulk@example.com"), &InferenceClassification::Other).await.unwrap();
    assert_eq!(
        body(&t.calls()[8]),
        json!({"classifyAs": "other", "senderEmailAddress": {"name": "Bulk", "address": "bulk@example.com"}})
    );
}

#[test]
fn every_api_fixture_through_every_mapper_never_panics() {
    use quorum_graph::api::{calendar, people, search};
    let dir = quorum_graph::fixtures_dir();
    let mut checked = 0;
    for sub in ["mail", "calendar", "people"] {
        for entry in std::fs::read_dir(dir.join(sub)).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let doc: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let mut values = vec![doc.clone()];
            if let Some(items) = doc.get("value").and_then(Value::as_array) {
                values.extend(items.iter().cloned());
            }
            for v in values {
                let _ = mail::message_from_json(v.clone());
                let _ = mail::attachment_from_json(v.clone());
                let _ = calendar::event_from_json(v.clone(), None);
                let _ = calendar::calendar_from_json(v.clone(), &Mailbox::Me, &Default::default());
                let _ = people::person_from_user_json(v.clone());
                let _ = people::person_from_people_json(v.clone());
                let _ = people::contact_from_json(v.clone());
                let _ = search::parse_search_response(&v, u32::MAX);
                checked += 1;
            }
        }
    }
    assert!(checked > 50);
}
