//! Teams chat and channel sync: chat list change detection, incremental message fetch, channel
//! delta, outbox request shapes and hostile Graph JSON.

#[expect(dead_code, reason = "shared test helpers; this file uses only the Graph and store fakes")]
mod common;

use jiff::{SignedDuration, Timestamp};
use quorum_graph::{FixtureResponse, FixtureTransport, HttpRequest, Method, Route};
use quorum_model::{ChannelId, ChatId, ChatMessageId, MessageContainer, TeamId};
use quorum_sync::teams::payload::{MentionKind, MentionPayload, ReactPayload, SendPayload, Target};
use quorum_sync::teams::{TeamsModule, channel_key, chat_key, older_request_key};
use quorum_sync::{Ctx, Module, OutboxHandler, Outcome};
use serde_json::{Value, json};

const CHAT: &str = "19:test-chat-1@unq.gbl.spaces";
const CHAT2: &str = "19:test-chat-2@thread.v2";
const ME: &str = "00000000-0000-0000-0000-000000000001";
const TEAM: &str = "00000000-0000-0000-0000-00000000f001";
const CHANNEL: &str = "19:test-channel@thread.tacv2";

fn ago(minutes: i64) -> String {
    (Timestamp::now() - SignedDuration::from_mins(minutes)).to_string()
}

fn chat_json(id: &str, preview_id: &str, preview_at: &str) -> Value {
    json!({
        "id": id, "chatType": "oneOnOne", "topic": null,
        "viewpoint": {"isHidden": false, "lastMessageReadDateTime": ago(600)},
        "members": [{"@odata.type": "#microsoft.graph.aadUserConversationMember", "id": "bQ==", "displayName": "Test User", "userId": ME}],
        "lastMessagePreview": {"id": preview_id, "createdDateTime": preview_at, "isDeleted": false,
            "body": {"contentType": "text", "content": "hi"},
            "from": {"user": {"id": "00000000-0000-0000-0000-000000000101", "displayName": "Morgan Sample"}}}
    })
}

fn msg_json(id: &str, at: &str, text: &str) -> Value {
    json!({
        "id": id, "messageType": "message", "createdDateTime": at, "lastModifiedDateTime": at,
        "from": {"user": {"id": "00000000-0000-0000-0000-000000000101", "displayName": "Morgan Sample"}},
        "body": {"contentType": "html", "content": format!("<p>{text}</p>")}
    })
}

fn page(values: Vec<Value>) -> FixtureResponse {
    FixtureResponse::json(200, &json!({ "value": values }))
}

fn calls_to(t: &FixtureTransport, method: Method, path_suffix: &str) -> Vec<HttpRequest> {
    t.calls().into_iter().filter(|c| c.method == method && c.url.path().ends_with(path_suffix)).collect()
}

fn module() -> TeamsModule {
    TeamsModule::new()
}

async fn chat_messages(ctx: &Ctx, chat: &str) -> Vec<String> {
    let page = ctx.store.chat_messages(ChatId::new(chat).unwrap(), None, 200).await.unwrap();
    page.items.into_iter().map(|m| m.id.into_inner()).collect()
}

#[tokio::test]
async fn chat_list_hydrates_recent_chats_and_skips_unchanged_ones() {
    let t = FixtureTransport::new();
    let at = ago(5);
    t.on(Method::Get, "/me/chats", page(vec![chat_json(CHAT, "m2", &at), chat_json(CHAT2, "x1", &ago(60 * 24 * 90))]));
    t.on(
        Method::Get,
        &format!("/chats/{CHAT}/messages"),
        page(vec![msg_json("m2", &at, "second"), msg_json("m1", &ago(10), "first")]),
    );
    let ctx = common::ctx(&t);
    let list = module().chat_list();
    assert_eq!(list.key(), "chats");
    assert_eq!(list.run(&ctx).await.unwrap(), Outcome::CaughtUp);

    let chats = ctx.store.chats(None, 50, false).await.unwrap();
    assert_eq!(chats.items.len(), 2);
    assert_eq!(chat_messages(&ctx, CHAT).await, vec!["m2", "m1"]);
    // The old chat (90 days) is not hydrated until opened.
    assert!(calls_to(&t, Method::Get, &format!("{CHAT2}/messages")).is_empty());
    let first_fetch = calls_to(&t, Method::Get, &format!("{CHAT}/messages"));
    assert_eq!(first_fetch.len(), 1);
    assert!(first_fetch[0].url.query_pairs().all(|(k, _)| k != "$filter"), "hydration has no filter");
    let state = ctx.store.delta_state(chat_key(&ChatId::new(CHAT).unwrap())).await.unwrap().unwrap();
    assert!(state.delta_link.is_some(), "watermark stored");

    // Same previews: nothing fetched.
    list.run(&ctx).await.unwrap();
    assert_eq!(calls_to(&t, Method::Get, &format!("{CHAT}/messages")).len(), 1);
}

#[tokio::test]
async fn changed_preview_fetches_incrementally_after_the_watermark() {
    let t = FixtureTransport::new();
    let at = ago(5);
    t.add(
        Route::new(Method::Get, "/me/chats")
            .respond(page(vec![chat_json(CHAT, "m1", &at)]))
            .respond(page(vec![chat_json(CHAT, "m2", &ago(1))])),
    );
    t.add(
        Route::new(Method::Get, format!("/chats/{CHAT}/messages"))
            .respond(page(vec![msg_json("m1", &at, "first")]))
            .respond(page(vec![msg_json("m2", &ago(1), "second")])),
    );
    let ctx = common::ctx(&t);
    let list = module().chat_list();
    list.run(&ctx).await.unwrap();
    list.run(&ctx).await.unwrap();
    let fetches = calls_to(&t, Method::Get, &format!("{CHAT}/messages"));
    assert_eq!(fetches.len(), 2);
    let filter: Vec<String> =
        fetches[1].url.query_pairs().filter(|(k, _)| k == "$filter").map(|(_, v)| v.into_owned()).collect();
    assert_eq!(filter.len(), 1);
    assert!(filter[0].starts_with("lastModifiedDateTime gt "), "{filter:?}");
    let mut ids = chat_messages(&ctx, CHAT).await;
    ids.sort();
    assert_eq!(ids, vec!["m1", "m2"]);
}

#[tokio::test]
async fn open_chat_loads_older_pages_on_request() {
    let t = FixtureTransport::new();
    let old = "https://graph.microsoft.com/v1.0/chats/x/messages?$skiptoken=older";
    t.on(
        Method::Get,
        &format!("/chats/{CHAT}/messages"),
        FixtureResponse::json(
            200,
            &json!({ "value": [msg_json("m9", &ago(60 * 24 * 40), "old enough")], "@odata.nextLink": old }),
        ),
    );
    t.add(Route::new(Method::Get, "/chats/x/messages").query("$skiptoken", "older").respond(page(vec![msg_json(
        "m0",
        &ago(60 * 24 * 50),
        "oldest",
    )])));
    let ctx = common::ctx(&t);
    let chat = ChatId::new(CHAT).unwrap();
    let res = module().chat(chat.clone());
    assert_eq!(res.key(), format!("chat:{CHAT}"));
    res.run(&ctx).await.unwrap();
    // Stopped at the 30-day cutoff: older page not fetched yet.
    assert_eq!(chat_messages(&ctx, CHAT).await, vec!["m9"]);
    ctx.store.set_setting(older_request_key(&chat), true).await.unwrap();
    res.run(&ctx).await.unwrap();
    assert_eq!(chat_messages(&ctx, CHAT).await, vec!["m9", "m0"]);
    let flag: Option<bool> = ctx.store.setting(older_request_key(&chat)).await.unwrap();
    assert_eq!(flag, None, "request cleared");
    let st = ctx.store.delta_state(chat_key(&chat)).await.unwrap().unwrap();
    assert!(st.backfill_complete);
}

#[tokio::test]
async fn channel_delta_applies_changes_removals_and_resyncs_expired_links() {
    let t = FixtureTransport::new();
    let delta = format!("/teams/{TEAM}/channels/{CHANNEL}/messages/delta");
    let link = format!("https://graph.microsoft.com/v1.0{delta}?$deltatoken=tok-1");
    t.on(
        Method::Get,
        &delta,
        FixtureResponse::json(200, &json!({
            "@odata.deltaLink": link,
            "value": [ {
                "id": "p1", "messageType": "message", "createdDateTime": ago(30), "subject": "Hello",
                "body": {"contentType": "html", "content": "<p>post</p>"},
                "replies": [ { "id": "r1", "replyToId": "p1", "createdDateTime": ago(20), "body": {"contentType": "text", "content": "reply"} } ]
            }, { "id": "p2", "createdDateTime": ago(25), "body": {"contentType": "text", "content": "other"} } ]
        })),
    );
    t.add(Route::new(Method::Get, delta.clone()).query("$deltatoken", "tok-1").respond(FixtureResponse::json(
        200,
        &json!({ "@odata.deltaLink": link.replace("tok-1", "tok-2"), "value": [ {"id": "p2", "@removed": {"reason": "deleted"}} ] }),
    )));
    t.add(
        Route::new(Method::Get, delta.clone())
            .query("$deltatoken", "tok-2")
            .respond(FixtureResponse::json(410, &json!({"error": {"code": "syncStateNotFound", "message": "gone"}}))),
    );
    t.on(Method::Get, "/me/joinedTeams", page(vec![json!({"id": TEAM, "displayName": "Product"})]));
    t.on(
        Method::Get,
        &format!("/teams/{TEAM}/allChannels"),
        page(vec![json!({"id": CHANNEL, "displayName": "General"})]),
    );

    let ctx = common::ctx(&t);
    let m = module();
    // The roster stores teams and channels and runs the first delta round of new channels.
    m.roster().run(&ctx).await.unwrap();
    let team = TeamId::new(TEAM).unwrap();
    let channel = ChannelId::new(CHANNEL).unwrap();
    assert_eq!(ctx.store.read(|r| Ok(r.teams()?.items.len())).await.unwrap(), 1);
    let posts = ctx.store.channel_posts(channel.clone(), None, 50).await.unwrap();
    assert_eq!(posts.items.len(), 2);
    let p1 = posts.items.iter().find(|p| p.post.id.as_str() == "p1").unwrap();
    assert_eq!(p1.reply_count, 1);

    // Discovery now includes the channel; its delta removes p2.
    let found = m.discover(&ctx).await.unwrap();
    let key = channel_key(&team, &channel);
    let res = found.iter().find(|r| r.key() == key).expect("channel resource discovered");
    res.run(&ctx).await.unwrap();
    let posts = ctx.store.channel_posts(channel.clone(), None, 50).await.unwrap();
    assert_eq!(posts.items.iter().map(|p| p.post.id.as_str()).collect::<Vec<_>>(), vec!["p1"]);

    // An expired link restarts the round without failing.
    res.run(&ctx).await.unwrap();
    let st = ctx.store.delta_state(key).await.unwrap().unwrap();
    assert!(st.delta_link.unwrap().contains("tok-1"), "restarted round stored its new link");
}

fn handler(kind: &str) -> std::sync::Arc<dyn OutboxHandler> {
    module().handlers().into_iter().find(|h| h.kind() == kind).unwrap_or_else(|| panic!("no handler {kind}"))
}

#[tokio::test]
async fn every_outbox_kind_has_a_handler() {
    let kinds: Vec<&str> = module().handlers().iter().map(|h| h.kind()).collect();
    for k in quorum_sync::teams::outbox::KINDS {
        assert!(kinds.contains(k), "{k}");
    }
    assert_eq!(kinds.len(), quorum_sync::teams::outbox::KINDS.len());
}

#[tokio::test]
async fn send_with_mention_posts_the_documented_shape_and_replaces_the_local_message() {
    let t = FixtureTransport::new();
    t.on(
        Method::Post,
        &format!("/chats/{CHAT}/messages"),
        FixtureResponse::json(201, &msg_json("real-1", &ago(0), "sent")),
    );
    let ctx = common::ctx(&t);
    let chat = ChatId::new(CHAT).unwrap();
    // The optimistic local message the engine would have written.
    let local = ChatMessageId::new("local-msg-1").unwrap();
    let mut pending: quorum_model::ChatMessage =
        serde_json::from_value(json!({"id": "local-msg-1", "container": {"chat": CHAT}})).unwrap();
    pending.created_date_time = Some(Timestamp::now());
    ctx.store.upsert_chat_messages(vec![pending]).await.unwrap();

    let payload = SendPayload {
        target: Target::Chat { chat: chat.clone(), message: None },
        local_id: local,
        html: "<p>Hi <at id=\"0\">Morgan</at></p>".into(),
        subject: None,
        importance: Some("high".into()),
        mentions: vec![MentionPayload {
            id: 0,
            text: "Morgan".into(),
            kind: MentionKind::User,
            target: "00000000-0000-0000-0000-000000000101".into(),
        }],
        images: vec![],
        files: vec![],
        quote: None,
    };
    let out = handler("chat.send").apply(&ctx, serde_json::to_value(&payload).unwrap()).await.unwrap();
    assert_eq!(out["id"], "real-1");
    assert_eq!(out["poke"], format!("chat:{CHAT}"));

    let sent = calls_to(&t, Method::Post, "/messages");
    assert_eq!(sent.len(), 1);
    let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(body["body"]["contentType"], "html");
    assert_eq!(body["importance"], "high");
    assert_eq!(body["mentions"][0]["id"], 0);
    assert_eq!(body["mentions"][0]["mentioned"]["user"]["id"], "00000000-0000-0000-0000-000000000101");
    assert_eq!(chat_messages(&ctx, CHAT).await, vec!["real-1"], "local message replaced");
}

#[tokio::test]
async fn react_edit_delete_and_read_hit_the_right_endpoints() {
    let t = FixtureTransport::new();
    let base = format!("/chats/{CHAT}/messages/m1");
    t.on(Method::Post, &format!("{base}/setReaction"), FixtureResponse::status(204));
    t.on(Method::Post, &format!("{base}/unsetReaction"), FixtureResponse::status(204));
    t.on(Method::Patch, &base, FixtureResponse::status(204));
    t.on(Method::Post, &format!("/me{base}/softDelete"), FixtureResponse::status(204));
    t.on(Method::Post, &format!("/chats/{CHAT}/markChatReadForUser"), FixtureResponse::status(204));
    let ctx = common::ctx(&t);
    let target = json!({"kind": "chat", "chat": CHAT, "message": "m1"});

    let react = ReactPayload {
        target: serde_json::from_value(target.clone()).unwrap(),
        reaction: "👍".into(),
        remove: false,
        before: None,
    };
    handler("chat.react").apply(&ctx, serde_json::to_value(&react).unwrap()).await.unwrap();
    let r = calls_to(&t, Method::Post, "/setReaction");
    assert_eq!(serde_json::from_slice::<Value>(&r[0].body).unwrap(), json!({"reactionType": "👍"}));
    handler("chat.unreact").apply(&ctx, json!({"target": target, "reaction": "👍", "remove": true})).await.unwrap();
    assert_eq!(calls_to(&t, Method::Post, "/unsetReaction").len(), 1);

    handler("chat.edit").apply(&ctx, json!({"target": target, "html": "<p>fixed</p>"})).await.unwrap();
    let patch = calls_to(&t, Method::Patch, "/messages/m1");
    let body: Value = serde_json::from_slice(&patch[0].body).unwrap();
    assert_eq!(body["body"]["content"], "<p>fixed</p>");

    handler("chat.delete").apply(&ctx, json!({"target": target})).await.unwrap();
    assert_eq!(calls_to(&t, Method::Post, "/softDelete").len(), 1);

    handler("chat.mark_read")
        .apply(&ctx, json!({"chat": CHAT, "user": ME, "tenant": "00000000-0000-0000-0000-0000000000f1"}))
        .await
        .unwrap();
    let read = calls_to(&t, Method::Post, "/markChatReadForUser");
    let body: Value = serde_json::from_slice(&read[0].body).unwrap();
    assert_eq!(body["user"]["id"], ME);
}

#[tokio::test]
async fn discarding_a_failed_delete_restores_the_message() {
    let t = FixtureTransport::new();
    let ctx = common::ctx(&t);
    let before: quorum_model::ChatMessage = serde_json::from_value(json!({
        "id": "m1", "container": {"chat": CHAT}, "body": {"contentType": "text", "content": "keep me"}
    }))
    .unwrap();
    let payload = json!({"target": {"kind": "chat", "chat": CHAT, "message": "m1"}, "before": before});
    handler("chat.delete").revert(&ctx, payload).await.unwrap();
    let got = ctx
        .store
        .read(|r| {
            r.chat_message(&MessageContainer::Chat(ChatId::new(CHAT).unwrap()), &ChatMessageId::new("m1").unwrap())
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.body.content.as_str(), "keep me");
}

#[tokio::test]
async fn hostile_payloads_and_graph_json_never_panic() {
    let t = FixtureTransport::new();
    t.on(
        Method::Get,
        "/me/chats",
        FixtureResponse::bytes(
            200,
            b"{\"value\": [1, null, {\"id\": 5}, {\"id\": \"\"}, {\"chatType\": []}]}".to_vec(),
        ),
    );
    t.on(Method::Get, "/me/joinedTeams", FixtureResponse::bytes(200, b"not json".to_vec()));
    let ctx = common::ctx(&t);
    let m = module();
    let _ = m.chat_list().run(&ctx).await;
    let _ = m.roster().run(&ctx).await;
    for h in m.handlers() {
        for p in
            [Value::Null, json!({}), json!([]), json!({"target": 5}), json!({"target": {"kind": "chat", "chat": ""}})]
        {
            let e = h.apply(&ctx, p.clone()).await;
            assert!(e.is_err(), "{} accepted {p}", h.kind());
            let _ = h.revert(&ctx, p).await;
        }
    }
}
