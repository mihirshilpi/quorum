//! Tests for `quorum_graph::api::teams` against the fixture transport (synthetic data only).

use quorum_graph::api::chat::{MessageRef, OutgoingMessage};
use quorum_graph::api::teams::{self, ChannelMessageChange};
use quorum_graph::{
    Client, DeltaOutcome, ErrorKind, FakeClock, FixtureResponse, FixtureTransport, HttpRequest, Method, RateLimit,
    ResourceFamily, Route, StaticToken,
};
use quorum_model::{ChannelId, ChannelMembershipType, ChatMessageId, MessageContainer, TeamId};
use serde_json::{Value, json};

const T1: &str = "00000000-0000-0000-0000-0000000000b1";
const CH1: &str = "19:synthetic-channel-0001@thread.tacv2";

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

fn team() -> TeamId {
    TeamId::new(T1).unwrap()
}

fn channel() -> ChannelId {
    ChannelId::new(CH1).unwrap()
}

fn cpath(rest: &str) -> String {
    format!("/teams/{T1}/channels/{CH1}{rest}")
}

#[tokio::test]
async fn joined_teams_and_channels() {
    let t = FixtureTransport::new();
    t.on(Method::Get, "/me/joinedTeams", file("teams/joined-teams.json"));
    t.on(Method::Get, &format!("/teams/{T1}/channels"), file("teams/channels.json"));
    t.on(Method::Get, &format!("/teams/{T1}/allChannels"), file("teams/channels.json"));
    t.on(
        Method::Get,
        &format!("/teams/{T1}"),
        FixtureResponse::json(200, &json!({"id": T1, "displayName": "Synthetic Team"})),
    );
    let c = client(&t);
    let teams = c.list_joined_teams().await.unwrap();
    assert_eq!(teams.len(), 2);
    assert_eq!(teams[0].display_name.as_str(), "Synthetic Team");
    assert!(teams[1].is_archived);
    let one = c.get_team(&team()).await.unwrap();
    assert_eq!(one.id, team());
    assert!(q(&t.calls()[1], "$select").unwrap().contains("isArchived"));

    let chans = c.list_channels(&team(), false).await.unwrap();
    assert_eq!(chans.len(), 3);
    assert_eq!(chans[0].team_id.as_ref(), Some(&team()));
    assert_eq!(chans[0].email.as_ref().unwrap().as_str(), "general@contoso.example");
    assert_eq!(chans[1].membership_type, ChannelMembershipType::Shared);
    assert!(matches!(chans[2].membership_type, ChannelMembershipType::Unknown(_)));
    let all = c.list_channels(&team(), true).await.unwrap();
    assert_eq!(all.len(), 3);
    assert!(t.calls().last().unwrap().url.path().ends_with("/allChannels"));
}

#[tokio::test]
async fn channel_delta_flow() {
    let t = FixtureTransport::new();
    let delta = cpath("/messages/delta");
    t.on(Method::Get, &delta, file("teams/channel-delta-page1.json"));
    t.add(
        Route::new(Method::Get, &delta)
            .query("$skiptoken", "channel-delta-2")
            .respond(file("teams/channel-delta-page2.json")),
    );
    t.add(Route::new(Method::Get, &delta).query("$deltatoken", "channel-token-1").respond(FixtureResponse::json(
        200,
        &json!({"value": [], "@odata.deltaLink": "https://graph.microsoft.com/v1.0/x?$deltatoken=channel-token-2"}),
    )));
    t.add(
        Route::new(Method::Get, &delta)
            .query("$deltatoken", "expired")
            .respond(FixtureResponse::json(410, &json!({"error": {"code": "syncStateNotFound"}}))),
    );
    let c = client(&t);
    let first = c.channel_messages_delta(&team(), &channel(), None).await.unwrap();
    let call = &t.calls()[0];
    assert_eq!(q(call, "$expand").as_deref(), Some("replies"));
    assert_eq!(call.url.path(), format!("/v1.0{delta}"));
    let DeltaOutcome::Next(next) = &first.outcome else { panic!("expected nextLink") };
    // The post, its two replies (one missing channelIdentity falls back), and a deleted post.
    assert_eq!(first.items.len(), 4);
    let ChannelMessageChange::Changed(post) = &first.items[0] else { panic!() };
    assert_eq!(post.subject.as_ref().unwrap().as_str(), "Synthetic announcement");
    assert_eq!(post.container, MessageContainer::Channel { team_id: team(), channel_id: channel() });
    let ChannelMessageChange::Changed(reply2) = &first.items[2] else { panic!() };
    assert_eq!(reply2.reply_to_id.as_ref().unwrap().as_str(), "1760000001000", "parent filled in for expanded replies");
    assert_eq!(reply2.container, post.container);
    let ChannelMessageChange::Changed(deleted) = &first.items[3] else { panic!() };
    assert!(deleted.is_deleted());

    let second = c.channel_messages_delta(&team(), &channel(), Some(next)).await.unwrap();
    assert_eq!(second.items, vec![ChannelMessageChange::Removed(ChatMessageId::new("1760000003000").unwrap())]);
    assert_eq!(second.skipped, 1);
    let DeltaOutcome::Done(delta_link) = &second.outcome else { panic!("expected deltaLink") };
    assert!(delta_link.contains("channel-token-1"));

    // A whole round from scratch, then a poll of the stored link.
    let round = c.channel_messages_delta_round(&team(), &channel(), None, 10).await.unwrap();
    assert_eq!(round.pages, 2);
    assert_eq!(round.items.len(), 5);
    let poll = c.channel_messages_delta_round(&team(), &channel(), Some(&round.delta_link), 10).await.unwrap();
    assert!(poll.items.is_empty());
    assert!(poll.delta_link.contains("channel-token-2"));

    let expired = format!("https://graph.microsoft.com/v1.0{delta}?$deltatoken=expired");
    let err = c.channel_messages_delta(&team(), &channel(), Some(&expired)).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::DeltaExpired);
    let foreign = c.channel_messages_delta(&team(), &channel(), Some("https://evil.example/v1.0/x")).await.unwrap_err();
    assert_eq!(foreign.kind(), ErrorKind::Malformed);

    let since: jiff::Timestamp = "2026-10-16T00:00:00Z".parse().unwrap();
    let req = teams::channel_messages_delta_request(&team(), &channel(), Some(since), Some(80));
    assert_eq!(req.resource_family(), ResourceFamily::Teams);
}

#[tokio::test]
async fn delta_round_detects_loops_and_caps() {
    let t = FixtureTransport::new();
    let delta = cpath("/messages/delta");
    let link = format!("https://graph.microsoft.com/v1.0{delta}?$skiptoken=loop");
    t.on(Method::Get, &delta, FixtureResponse::json(200, &json!({"value": [], "@odata.nextLink": link})));
    let c = client(&t);
    let err = c.channel_messages_delta_round(&team(), &channel(), None, 10).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Malformed);
    let err = c.channel_messages_delta_round(&team(), &channel(), None, 1).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::TooLarge);
}

#[tokio::test]
async fn posts_replies_and_actions() {
    let t = FixtureTransport::new();
    t.on(Method::Get, &cpath("/messages"), file("teams/channel-delta-page2.json"));
    t.on(Method::Get, &cpath("/messages/1760000001000/replies"), file("teams/replies.json"));
    t.on(
        Method::Post,
        &cpath("/messages"),
        FixtureResponse::json(
            201,
            &json!({"id": "1760000009000", "channelIdentity": {"teamId": T1, "channelId": CH1}}),
        ),
    );
    t.on(
        Method::Post,
        &cpath("/messages/1760000001000/replies"),
        FixtureResponse::json(201, &json!({"id": "1760000009001"})),
    );
    t.on(
        Method::Post,
        &cpath("/messages/1760000001000/replies/1760000009001/setReaction"),
        FixtureResponse::status(204),
    );
    t.on(Method::Patch, &cpath("/messages/1760000001000"), FixtureResponse::status(204));
    let c = client(&t);
    let page = c.list_channel_messages(&team(), &channel(), 999).await.unwrap();
    assert_eq!(q(&t.calls()[0], "$top").as_deref(), Some("50"));
    assert_eq!(page.items.len(), 1, "the @removed-only item maps without body; bogus skipped");

    let parent = ChatMessageId::new("1760000001000").unwrap();
    let replies = c.list_replies(&team(), &channel(), &parent, 20).await.unwrap();
    assert_eq!(replies.items[0].reply_to_id.as_ref(), Some(&parent));

    let mut post = OutgoingMessage::html("<p>Synthetic post</p>");
    post.subject = Some("Subject".into());
    let created = c.post_channel_message(&team(), &channel(), &post).await.unwrap();
    assert_eq!(created.id.as_str(), "1760000009000");
    let b: Value = serde_json::from_slice(&t.calls().last().unwrap().body).unwrap();
    assert_eq!(b["subject"], "Subject");
    assert!(c.reply_to_channel_message(&team(), &channel(), &parent, &post).await.is_err(), "replies have no subject");
    let reply = c.reply_to_channel_message(&team(), &channel(), &parent, &OutgoingMessage::text("ok")).await.unwrap();
    assert_eq!(reply.reply_to_id.as_ref(), Some(&parent));
    assert_eq!(reply.container, MessageContainer::Channel { team_id: team(), channel_id: channel() });

    let target =
        MessageRef::ChannelReply { team: team(), channel: channel(), message: parent.clone(), reply: reply.id };
    c.set_reaction(&target, "like").await.unwrap();
    let post_ref = MessageRef::ChannelPost { team: team(), channel: channel(), message: parent };
    let body = quorum_model::ChatBody { content_type: quorum_model::BodyType::Text, content: "edited".into() };
    c.edit_message(&post_ref, &body, &[]).await.unwrap();
}

#[tokio::test]
async fn files_folder_and_members() {
    let t = FixtureTransport::new();
    t.on(Method::Get, &cpath("/filesFolder"), file("teams/files-folder.json"));
    t.on(Method::Get, &format!("/teams/{T1}/members"), file("teams/members.json"));
    t.on(Method::Get, &cpath("/members"), file("teams/members.json"));
    let c = client(&t);
    let folder = c.channel_files_folder(&team(), &channel()).await.unwrap();
    assert!(folder.is_folder());
    assert_eq!(folder.drive_id.as_str(), "b!synthetic-drive-team-0001");
    assert!(folder.parent_id.is_none());
    let members = c.list_team_members(&team()).await.unwrap();
    assert_eq!(members.len(), 2);
    assert_eq!(members[1].member.roles, vec!["guest".to_owned()]);
    let cm = c.list_channel_members(&team(), &channel()).await.unwrap();
    assert_eq!(cm.len(), 2);
}

#[tokio::test]
async fn hostile_teams_payloads_never_panic() {
    for body in [
        json!({"value": [null, 1, "x", {"id": {"nested": true}}, {"id": "ok", "replies": "nope"}, {"id": "r", "@removed": 5}]}),
        json!({"value": [{"id": "1", "replies": [null, {"id": "x".repeat(2000)}, {"id": "2", "from": {"user": [1]}}]}], "@odata.deltaLink": "https://graph.microsoft.com/v1.0/d"}),
        json!({"value": "x", "@odata.deltaLink": "https://graph.microsoft.com/v1.0/d"}),
        json!({"value": []}),
    ] {
        let t = FixtureTransport::new();
        t.on(Method::Get, &cpath("/messages/delta"), FixtureResponse::json(200, &body));
        t.on(Method::Get, "/me/joinedTeams", FixtureResponse::json(200, &body));
        t.on(Method::Get, &format!("/teams/{T1}/channels"), FixtureResponse::json(200, &body));
        let c = client(&t);
        let _ = c.channel_messages_delta(&team(), &channel(), None).await;
        let _ = c.list_joined_teams().await;
        let _ = c.list_channels(&team(), false).await;
    }
    assert!(teams::parse_team(json!({"id": T1, "displayName": 5, "isArchived": "yes"})).is_some());
    assert!(teams::parse_channel(json!([]), &team()).is_none());
}
