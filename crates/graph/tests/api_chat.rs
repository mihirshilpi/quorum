//! Tests for `quorum_graph::api::chat` against the fixture transport (synthetic data only).

use quorum_graph::api::chat::{
    self, FileReference, HostedImage, MentionTarget, MessageRef, OutgoingMention, OutgoingMessage, QuotedMessage,
};
use quorum_graph::{
    Client, ErrorKind, FakeClock, FixtureResponse, FixtureTransport, HttpRequest, Method, RateLimit, ResourceFamily,
    Route, StaticToken,
};
use quorum_model::{
    BodyType, ChannelId, ChatBody, ChatId, ChatImportance, ChatMessageId, ChatMessageType, ChatType, DriveId,
    DriveItem, DriveItemId, IdentityKind, MessageContainer, Sensitive, TeamId, UserId,
};
use serde_json::{Value, json};

const C1: &str = "19:00000000000000000000000000000001_00000000000000000000000000000002@unq.gbl.spaces";
const U1: &str = "00000000-0000-0000-0000-000000000011";
const U2: &str = "00000000-0000-0000-0000-000000000012";
const TENANT: &str = "00000000-0000-0000-0000-0000000000f1";

fn client(t: &FixtureTransport) -> Client {
    let mut b = Client::builder(StaticToken::new("test-token"));
    for f in ResourceFamily::ALL {
        b = b.rate(f, RateLimit::new(10_000.0, 10_000).unwrap());
    }
    b.transport(t.clone()).clock(FakeClock::new()).jitter_seed(3).build().unwrap()
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

fn chat_id() -> ChatId {
    ChatId::new(C1).unwrap()
}

fn mid(s: &str) -> ChatMessageId {
    ChatMessageId::new(s).unwrap()
}

#[tokio::test]
async fn list_chats_request_shape_and_mapping() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me/chats", file("chat/chats-page1.json"));
    t.add(
        Route::new(Method::Get, "/me/chats").query("$skiptoken", "chats-page2").respond(file("chat/chats-page2.json")),
    );
    let c = client(&t);
    let page = c.list_chats(500).await.unwrap();
    let call = &t.calls()[0];
    assert_eq!(call.method, Method::Get);
    assert_eq!(call.url.path(), "/v1.0/me/chats");
    assert_eq!(q(call, "$expand").as_deref(), Some("members,lastMessagePreview"));
    assert_eq!(q(call, "$orderby").as_deref(), Some("lastMessagePreview/createdDateTime desc"));
    assert_eq!(q(call, "$top").as_deref(), Some("50"), "top is clamped to Graph's max");
    assert!(q(call, "$select").unwrap().contains("viewpoint"));
    assert_eq!(call.header("authorization"), Some("Bearer test-token"));

    assert_eq!(page.items.len(), 2);
    let one = &page.items[0];
    assert_eq!(one.chat_type, ChatType::OneOnOne);
    assert_eq!(one.members.len(), 2);
    assert_eq!(one.members[1].user_id.as_ref().unwrap().as_str(), U2);
    assert_eq!(one.members[1].email.as_ref().unwrap().as_str(), "alex.sample@contoso.example");
    assert_eq!(one.last_message_read_date_time.unwrap().to_string(), "2026-10-16T09:00:00Z");
    assert!(!one.is_hidden);
    let preview = one.last_message_preview.as_ref().unwrap();
    assert_eq!(preview.body_preview.as_str(), "See you at 10 & bring snacks");
    let from = preview.from.as_ref().unwrap();
    assert_eq!(from.kind, IdentityKind::User);
    assert_eq!(from.id.as_deref(), Some(U2));
    assert_eq!(from.display_name.as_str(), "Alex Sample");
    assert!(one.has_unread());

    let group = &page.items[1];
    assert_eq!(group.topic.as_ref().unwrap().as_str(), "Synthetic project group");
    assert!(group.is_hidden);
    assert!(group.members[1].email.is_none(), "invalid addresses are dropped, not fatal");
    assert_eq!(group.members[1].roles, vec!["guest".to_owned()]);
    let bot = group.last_message_preview.as_ref().unwrap().from.as_ref().unwrap();
    assert_eq!(bot.kind, IdentityKind::Application);
    assert!(!group.has_unread());

    let next = page.next_link.unwrap();
    let page2 = c.list_chats_next(&next).await.unwrap();
    assert_eq!(page2.items.len(), 1);
    assert_eq!(page2.items[0].chat_type, ChatType::Meeting);
    assert_eq!(page2.skipped, 2, "items without chatType or id are skipped");
}

#[tokio::test]
async fn get_chat_and_errors_map() {
    let t = FixtureTransport::new();
    let c = client(&t);
    let err = c.get_chat(&chat_id()).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotFound);
    let path = t.calls()[0].url.path().to_owned();
    assert!(path.starts_with("/v1.0/chats/19:"), "{path}");
    assert!(path.ends_with("@unq.gbl.spaces"));

    let t = FixtureTransport::new();
    t.on(
        Method::Get,
        &format!("/chats/{C1}"),
        FixtureResponse::json(403, &json!({"error": {"code": "Forbidden", "message": "synthetic"}})),
    );
    let err = client(&t).get_chat(&chat_id()).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Forbidden);
    assert_eq!(err.code(), Some("Forbidden"));

    let t = FixtureTransport::new();
    t.on(Method::Get, &format!("/chats/{C1}"), FixtureResponse::json(200, &json!({"id": C1})));
    let err = client(&t).get_chat(&chat_id()).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Malformed);
}

#[tokio::test]
async fn path_segments_are_encoded() {
    let t = FixtureTransport::new();
    let c = client(&t);
    let odd = ChatId::new("19:a/b?c#d e%@thread.v2").unwrap();
    let _ = c.get_chat(&odd).await;
    let url = t.calls()[0].url.clone();
    assert_eq!(url.path(), "/v1.0/chats/19:a%2Fb%3Fc%23d%20e%25@thread.v2");
    assert_eq!(url.host_str(), Some("graph.microsoft.com"));
}

#[tokio::test]
async fn chat_messages_map_container_identity_mentions_and_attachments() {
    let t = FixtureTransport::new();
    t.on(Method::Get, &format!("/chats/{C1}/messages"), file("chat/messages.json"));
    let page = client(&t).list_chat_messages(&chat_id(), 20, None).await.unwrap();
    let call = &t.calls()[0];
    assert_eq!(q(call, "$top").as_deref(), Some("20"));
    assert_eq!(q(call, "$orderby").as_deref(), Some("lastModifiedDateTime desc"));
    assert_eq!(q(call, "$filter"), None);

    assert_eq!(page.items.len(), 3);
    let m = &page.items[0];
    assert_eq!(m.container, MessageContainer::Chat(chat_id()));
    assert_eq!(m.importance, ChatImportance::High);
    assert!(m.is_edited());
    assert_eq!(m.body.content_type, BodyType::Html);
    assert_eq!(m.from.as_ref().unwrap().id.as_deref(), Some(U2));
    assert_eq!(m.mentions.len(), 2);
    assert_eq!(m.mentions[0].mentioned.as_ref().unwrap().kind, IdentityKind::User);
    assert_eq!(m.mentions[1].mentioned.as_ref().unwrap().kind.to_string(), "conversation");
    assert_eq!(m.reactions[0].reaction_type, "like");
    assert_eq!(m.reactions[0].user.as_ref().unwrap().id.as_deref(), Some(U1));
    assert_eq!(m.attachments.len(), 2);
    assert_eq!(m.attachments[0].content_type, "reference");
    assert!(m.attachments[0].content_url.as_deref().unwrap().starts_with("https://contoso.sharepoint.com/"));
    assert!(
        m.attachments[1].content.as_ref().unwrap().as_str().contains("AdaptiveCard"),
        "object content is kept as JSON text"
    );
    assert!(m.attachments[1].thumbnail_url.is_none(), "non-https URLs are dropped");

    let system = &page.items[1];
    assert_eq!(system.container, MessageContainer::Chat(chat_id()), "falls back to the addressed chat");
    assert_eq!(system.message_type, ChatMessageType::SystemEventMessage);
    assert!(system.from.is_none());

    let deleted = &page.items[2];
    assert!(deleted.is_deleted());
    assert_eq!(deleted.from.as_ref().unwrap().kind, IdentityKind::Application);
    assert!(matches!(deleted.importance, ChatImportance::Unknown(_)));
}

#[tokio::test]
async fn incremental_filter_follows_pages() {
    let t = FixtureTransport::new();
    let path = format!("/chats/{C1}/messages");
    let next = format!("https://graph.microsoft.com/v1.0{path}?$skiptoken=m2");
    t.on(
        Method::Get,
        &path,
        FixtureResponse::json(200, &json!({"@odata.nextLink": next, "value": [{"id": "2", "chatId": C1}]})),
    );
    t.add(
        Route::new(Method::Get, &path)
            .query("$skiptoken", "m2")
            .respond(FixtureResponse::json(200, &json!({"value": [{"id": "1"}]}))),
    );
    let since: jiff::Timestamp = "2026-10-16T09:30:00Z".parse().unwrap();
    let msgs = client(&t).chat_messages_since(&chat_id(), since, 5).await.unwrap();
    assert_eq!(msgs.len(), 2);
    let first = &t.calls()[0];
    assert_eq!(q(first, "$filter").as_deref(), Some("lastModifiedDateTime gt 2026-10-16T09:30:00Z"));
    assert_eq!(q(first, "$orderby").as_deref(), Some("lastModifiedDateTime desc"));
    assert_eq!(q(first, "$top").as_deref(), Some("50"));
    assert_eq!(t.call_count(), 2);
}

#[tokio::test]
async fn send_message_with_mentions_files_images_and_quote() {
    let t = FixtureTransport::new();
    t.on(Method::Post, &format!("/chats/{C1}/messages"), file("chat/message-sent.json").with_header("x", "y"));
    let item: DriveItem = DriveItem {
        id: DriveItemId::new("01SYNTHETIC").unwrap(),
        drive_id: DriveId::new("b!drive").unwrap(),
        parent_id: None,
        name: Sensitive::new("Report.docx"),
        size: 1,
        file: None,
        folder: None,
        last_modified_by: None,
        last_modified_date_time: None,
        created_date_time: None,
        web_url: Some("https://contoso.sharepoint.com/Report.docx".into()),
        e_tag: Some("\"{5B6C8A52-0000-4000-8000-000000000001},3\"".into()),
        c_tag: None,
        shared: false,
    };
    let file_ref = FileReference::from_drive_item(&item).unwrap();
    assert_eq!(file_ref.attachment_id, "5b6c8a52-0000-4000-8000-000000000001");
    let mut msg = OutgoingMessage::html(r#"<at id="0">Alex</at> look <img src="../hostedContents/1/$value">"#);
    msg.mentions.push(OutgoingMention {
        id: 0,
        text: "Alex".into(),
        target: MentionTarget::User { id: UserId::new(U2).unwrap(), display_name: "Alex Sample".into() },
    });
    msg.files.push(file_ref);
    msg.images.push(HostedImage {
        temporary_id: "1".into(),
        content_type: "image/png".into(),
        bytes: b"PNGDATA".to_vec(),
    });
    msg.quote = Some(QuotedMessage {
        message_id: mid("1760000000002"),
        preview: "earlier".into(),
        sender: Some((UserId::new(U2).unwrap(), "Alex Sample".into())),
    });
    msg.importance = ChatImportance::Urgent;
    let sent = client(&t).send_chat_message(&chat_id(), &msg).await.unwrap();
    assert_eq!(sent.id.as_str(), "1760000000500");

    let call = &t.calls()[0];
    assert_eq!(call.method, Method::Post);
    assert_eq!(call.header("content-type"), Some("application/json"));
    let b = body(call);
    let content = b["body"]["content"].as_str().unwrap();
    assert_eq!(b["body"]["contentType"], "html");
    assert!(content.starts_with(r#"<attachment id="1760000000002"></attachment>"#));
    assert!(content.contains(r#"<attachment id="5b6c8a52-0000-4000-8000-000000000001"></attachment>"#));
    assert_eq!(b["importance"], "urgent");
    assert_eq!(b["mentions"][0]["mentioned"]["user"]["id"], U2);
    assert_eq!(b["mentions"][0]["mentioned"]["user"]["userIdentityType"], "aadUser");
    assert_eq!(b["attachments"][0]["contentType"], "reference");
    assert_eq!(b["attachments"][1]["contentType"], "messageReference");
    let reference: Value = serde_json::from_str(b["attachments"][1]["content"].as_str().unwrap()).unwrap();
    assert_eq!(reference["messageId"], "1760000000002");
    assert_eq!(b["hostedContents"][0]["@microsoft.graph.temporaryId"], "1");
    assert_eq!(b["hostedContents"][0]["contentBytes"], "UE5HREFUQQ==");
    assert_eq!(b["hostedContents"][0]["contentType"], "image/png");
}

#[test]
fn outgoing_message_validation() {
    assert_eq!(chat::message_payload(&OutgoingMessage::text("  ")).unwrap_err().kind(), ErrorKind::Config);
    let mut m = OutgoingMessage::text("hi");
    m.mentions.push(OutgoingMention {
        id: 0,
        text: "x".into(),
        target: MentionTarget::User { id: UserId::new(U2).unwrap(), display_name: "x".into() },
    });
    assert_eq!(chat::message_payload(&m).unwrap_err().kind(), ErrorKind::Config, "rich content needs html");
    m.body.content_type = BodyType::Html;
    assert!(chat::message_payload(&m).is_err(), "mention without <at> element");
    m.body.content = r#"<at id="0">x</at>"#.into();
    let mut dup = m.clone();
    dup.mentions.push(dup.mentions[0].clone());
    assert!(chat::message_payload(&dup).is_err());
    assert!(chat::message_payload(&m).is_ok());
    let mut img = OutgoingMessage::html("<p>no ref</p>");
    img.images.push(HostedImage { temporary_id: "1".into(), content_type: "image/png".into(), bytes: vec![1] });
    assert!(chat::message_payload(&img).is_err(), "unreferenced image");
    img.body.content = r#"<img src="../hostedContents/1/$value">"#.into();
    img.images[0].content_type = "text/html".into();
    assert!(chat::message_payload(&img).is_err(), "non-image hosted content");
    let big = OutgoingMessage::text("x".repeat(chat::MAX_OUTGOING_BODY_BYTES + 1));
    assert!(chat::message_payload(&big).is_err());
    let channel_mention = OutgoingMention {
        id: 1,
        text: "General".into(),
        target: MentionTarget::Channel {
            id: ChannelId::new("19:c@thread.tacv2").unwrap(),
            display_name: "General".into(),
        },
    };
    let mut ch = OutgoingMessage::html(r#"<at id="1">General</at>"#);
    ch.mentions.push(channel_mention);
    let p = chat::message_payload(&ch).unwrap();
    assert_eq!(p["mentions"][0]["mentioned"]["conversation"]["conversationIdentityType"], "channel");
}

#[tokio::test]
async fn create_chat_and_members() {
    let t = FixtureTransport::new();
    t.on(Method::Post, "/chats", FixtureResponse::json(201, &json!({"id": C1, "chatType": "oneOnOne"})));
    t.on(Method::Get, &format!("/chats/{C1}/members"), file("chat/members.json"));
    t.on(
        Method::Post,
        &format!("/chats/{C1}/members"),
        FixtureResponse::json(201, &json!({"id": "bWVtYmVyLTk=", "userId": U2, "displayName": "Alex"})),
    );
    t.on(Method::Delete, &format!("/chats/{C1}/members/bWVtYmVyLTk="), FixtureResponse::status(204));
    let c = client(&t);
    let users = [UserId::new(U1).unwrap(), UserId::new(U2).unwrap()];
    let chat = c.create_chat(&ChatType::OneOnOne, &users, None).await.unwrap();
    assert_eq!(chat.id, chat_id());
    let b = body(&t.calls()[0]);
    assert_eq!(b["chatType"], "oneOnOne");
    assert_eq!(b["members"][1]["user@odata.bind"], format!("https://graph.microsoft.com/v1.0/users('{U2}')"));
    assert_eq!(b["members"][0]["@odata.type"], "#microsoft.graph.aadUserConversationMember");
    assert_eq!(b["members"][0]["roles"][0], "owner");

    assert!(c.create_chat(&ChatType::OneOnOne, &users[..1], None).await.is_err());
    assert!(c.create_chat(&ChatType::OneOnOne, &users, Some("topic")).await.is_err());
    assert!(c.create_chat(&ChatType::Meeting, &users, None).await.is_err());
    let dup = [users[0].clone(), users[0].clone()];
    assert!(c.create_chat(&ChatType::Group, &dup, None).await.is_err());
    c.create_chat(&ChatType::Group, &users, Some("Synthetic topic")).await.unwrap();
    assert_eq!(body(t.calls().last().unwrap())["topic"], "Synthetic topic");

    let members = c.list_chat_members(&chat_id()).await.unwrap();
    assert_eq!(members.len(), 2, "member without membership id skipped");
    assert_eq!(members[0].membership_id, "bWVtYmVyLTE=");
    let added = c.add_chat_member(&chat_id(), &users[1], Some("2026-01-01T00:00:00Z".parse().unwrap())).await.unwrap();
    assert_eq!(added.membership_id, "bWVtYmVyLTk=");
    let b = body(t.calls().last().unwrap());
    assert_eq!(b["visibleHistoryStartDateTime"], "2026-01-01T00:00:00Z");
    c.remove_chat_member(&chat_id(), &added.membership_id).await.unwrap();
    assert_eq!(t.calls().last().unwrap().method, Method::Delete);
    assert!(c.remove_chat_member(&chat_id(), "").await.is_err());
}

#[tokio::test]
async fn edit_delete_react_and_read_state() {
    let t = FixtureTransport::new();
    let msg_path = format!("/chats/{C1}/messages/1760000000002");
    for (m, p) in [
        (Method::Patch, msg_path.clone()),
        (Method::Post, format!("/me{msg_path}/softDelete")),
        (Method::Post, format!("/me{msg_path}/undoSoftDelete")),
        (Method::Post, format!("{msg_path}/setReaction")),
        (Method::Post, format!("{msg_path}/unsetReaction")),
        (Method::Post, format!("/chats/{C1}/markChatReadForUser")),
        (Method::Post, format!("/chats/{C1}/markChatUnreadForUser")),
    ] {
        t.on(m, &p, FixtureResponse::status(204));
    }
    let c = client(&t);
    let target = MessageRef::Chat { chat: chat_id(), message: mid("1760000000002") };
    let new_body = ChatBody { content_type: BodyType::Html, content: "<p>edited</p>".into() };
    c.edit_message(&target, &new_body, &[]).await.unwrap();
    c.soft_delete_message(&target).await.unwrap();
    c.undo_soft_delete_message(&target).await.unwrap();
    c.set_reaction(&target, "👍").await.unwrap();
    c.unset_reaction(&target, "👍").await.unwrap();
    let me = UserId::new(U1).unwrap();
    c.mark_chat_read(&chat_id(), &me, TENANT).await.unwrap();
    c.mark_chat_unread(&chat_id(), &me, TENANT, "2026-10-16T09:00:00Z".parse().unwrap()).await.unwrap();
    let calls = t.calls();
    assert_eq!(calls.len(), 7);
    assert!(calls.iter().all(|c| c.url.path() != "/v1.0/fixtureNotFound"));
    assert_eq!(body(&calls[0])["body"]["content"], "<p>edited</p>");
    assert_eq!(body(&calls[3])["reactionType"], "👍");
    assert_eq!(body(&calls[5])["user"], json!({"id": U1, "tenantId": TENANT}));
    assert_eq!(body(&calls[6])["lastMessageReadDateTime"], "2026-10-16T09:00:00Z");
    // A scripted 404 would have surfaced as an error, so every path matched a route.
    assert!(c.set_reaction(&target, "").await.is_err());
    assert!(c.set_reaction(&target, &"x".repeat(65)).await.is_err());
    assert!(c.mark_chat_read(&chat_id(), &me, "").await.is_err());
    let empty = ChatBody { content_type: BodyType::Html, content: " ".into() };
    assert!(c.edit_message(&target, &empty, &[]).await.is_err());

    // Channel targets use the team path without the /me prefix.
    let post = MessageRef::ChannelReply {
        team: TeamId::new("t1").unwrap(),
        channel: ChannelId::new("19:c@thread.tacv2").unwrap(),
        message: mid("1"),
        reply: mid("2"),
    };
    let req = chat::soft_delete_message_request(&post);
    assert_eq!(req.target(), "/teams/t1/channels/19:c@thread.tacv2/messages/1/replies/2/softDelete");
}

#[tokio::test]
async fn hosted_content_and_pins() {
    let t = FixtureTransport::new();
    let hc = format!("/chats/{C1}/messages/1760000000002/hostedContents/aWQ9/$value");
    let mut png = FixtureResponse::bytes(200, b"\x89PNG".to_vec());
    if let FixtureResponse::Http(r) = &mut png {
        r.headers = vec![("content-type".into(), "image/png".into())];
    }
    t.on(Method::Get, &hc, png);
    t.on(Method::Get, &format!("/chats/{C1}/pinnedMessages"), file("chat/pinned.json"));
    t.on(
        Method::Post,
        &format!("/chats/{C1}/pinnedMessages"),
        FixtureResponse::json(201, &json!({"id": "cGlubmVkLTM="})),
    );
    t.on(Method::Delete, &format!("/chats/{C1}/pinnedMessages/cGlubmVkLTM="), FixtureResponse::status(204));
    let c = client(&t);
    let got = c.get_hosted_content(&chat_id(), &mid("1760000000002"), "aWQ9").await.unwrap();
    assert_eq!(got.bytes, b"\x89PNG");
    assert_eq!(got.content_type.as_deref(), Some("image/png"));
    assert_eq!(t.calls()[0].header("accept"), Some("*/*"));
    let by_url = c.get_hosted_content_by_url(&format!("https://graph.microsoft.com/v1.0{hc}")).await.unwrap();
    assert_eq!(by_url.bytes, b"\x89PNG");
    let err = c.get_hosted_content_by_url("https://evil.example/v1.0/hostedContents/x/$value").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Malformed);
    assert!(c.get_hosted_content_by_url("https://graph.microsoft.com/v1.0/me").await.is_err());
    assert_eq!(t.call_count(), 2, "rejected URLs never hit the transport");

    let pins = c.list_pinned_messages(&chat_id()).await.unwrap();
    assert_eq!(pins.len(), 2);
    assert_eq!(pins[0].message.as_ref().unwrap().id.as_str(), "1760000000002");
    assert!(pins[1].message.is_none());
    let pinned = c.pin_message(&chat_id(), &mid("1760000000002")).await.unwrap();
    assert_eq!(pinned, "cGlubmVkLTM=");
    assert_eq!(
        body(t.calls().last().unwrap())["message@odata.bind"],
        format!("https://graph.microsoft.com/v1.0/chats/{C1}/messages/1760000000002")
    );
    c.unpin_message(&chat_id(), &pinned).await.unwrap();
}

#[tokio::test]
async fn hostile_chat_fixtures_never_panic() {
    let t = FixtureTransport::new();
    t.on(Method::Get, &format!("/chats/{C1}/messages"), file("chat/hostile-messages.json"));
    let page = client(&t).list_chat_messages(&chat_id(), 10, None).await.unwrap();
    // h1, h2 (falls back to the addressed chat), h3 and h4 map; the huge id and non-objects do not.
    assert_eq!(page.items.len(), 4);
    assert_eq!(page.skipped, 5);
    let h1 = &page.items[0];
    assert!(h1.from.is_none());
    assert!(h1.created_date_time.is_none());
    assert!(h1.mentions.is_empty());
    assert_eq!(h1.reactions.len(), 1);
    let h3 = page.items.iter().find(|m| m.id.as_str() == "h3").unwrap();
    assert_eq!(h3.mentions.len(), 1);
    assert_eq!(h3.mentions[0].mentioned.as_ref().unwrap().kind.to_string(), "tag");
    assert!(h3.last_modified_date_time.is_some(), "offset-less instants read as UTC");

    for body in [
        json!(null),
        json!([]),
        json!({"value": [{"id": C1, "chatType": {"x": 1}, "members": "x", "viewpoint": [1]}]}),
        json!({"value": [{"id": C1, "chatType": "group", "lastMessagePreview": {"id": 5}}]}),
    ] {
        let t = FixtureTransport::new();
        t.on(Method::Get, "/me/chats", FixtureResponse::json(200, &body));
        let _ = client(&t).list_chats(10).await;
    }
    assert!(chat::parse_chat(json!({"id": C1, "chatType": "group", "topic": "x".repeat(100_000)})).is_some());
    assert!(chat::parse_chat_message(json!({"id": "1"}), None).is_none(), "no container, no message");
}

#[test]
fn request_values_carry_family_and_bucket() {
    let req = chat::list_chats_request(10);
    assert_eq!(req.resource_family(), ResourceFamily::Teams);
    assert_eq!(req.bucket_key(), chat::TEAMS_BUCKET);
    let send = chat::send_chat_message_request(&chat_id(), &OutgoingMessage::text("hi")).unwrap();
    assert!(!send.is_idempotent(), "sends are never retried blindly");
    assert_eq!(send.method(), Method::Post);
    let hc = chat::hosted_content_request(&chat_id(), &mid("1"), "a/b").unwrap();
    assert!(hc.target().ends_with("/hostedContents/a%2Fb/$value"));
}
