use quorum_model::{ChannelId, ChatId, ChatMessageId, MessageContainer, TeamId, UserId};
use quorum_store::Store;

use crate::common::{channel_message, chat, chat_message, t};

#[tokio::test]
async fn chats_ordered_by_activity_with_unread() {
    let store = Store::open_in_memory().unwrap();
    store
        .upsert_chats(vec![
            chat("c-old", "2026-01-01T00:00:00Z", Some("2026-01-01T00:00:00Z")),
            chat("c-new", "2026-01-03T00:00:00Z", None),
            chat("c-mid", "2026-01-02T00:00:00Z", Some("2026-01-01T00:00:00Z")),
        ])
        .await
        .unwrap();
    let first = store.chats(None, 2, false).await.unwrap();
    let ids: Vec<_> = first.items.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["c-new", "c-mid"]);
    let rest = store.chats(first.next, 2, false).await.unwrap();
    assert_eq!(rest.items.len(), 1);
    assert!(rest.next.is_none());
    assert_eq!(store.read(|r| r.unread_chat_count()).await.unwrap(), 2);
    store.write(|tx| tx.mark_chat_read(&ChatId::new("c-new")?, t("2026-01-04T00:00:00Z"))).await.unwrap();
    assert_eq!(store.read(|r| r.unread_chat_count()).await.unwrap(), 1);
    let with_bob = store.read(|r| r.chats_with_member(&UserId::new("u2")?)).await.unwrap();
    assert_eq!(with_bob.len(), 3);
}

#[tokio::test]
async fn chat_messages_keyset_paged_with_bodies() {
    let store = Store::open_in_memory().unwrap();
    let msgs: Vec<_> = (0..7)
        .map(|i| chat_message("c1", &format!("m{i}"), &format!("2026-01-01T00:0{i}:00Z"), &format!("hello {i}")))
        .collect();
    store.upsert_chat_messages(msgs).await.unwrap();
    store.upsert_chat_messages(vec![chat_message("c2", "m0", "2026-01-01T00:00:00Z", "other chat")]).await.unwrap();
    let c1 = ChatId::new("c1").unwrap();
    let mut after = None;
    let mut seen = Vec::new();
    for _ in 0..10 {
        let page = store.chat_messages(c1.clone(), after, 3).await.unwrap();
        seen.extend(page.items.iter().map(|m| (m.id.as_str().to_owned(), m.body.content.as_str().to_owned())));
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    let ids: Vec<_> = seen.iter().map(|s| s.0.as_str()).collect();
    assert_eq!(ids, ["m6", "m5", "m4", "m3", "m2", "m1", "m0"]);
    assert_eq!(seen[0].1, "<p>hello 6</p>");

    store
        .write(move |tx| tx.delete_chat_messages(&MessageContainer::Chat(c1), &[ChatMessageId::new("m6")?]))
        .await
        .unwrap();
    let page = store.chat_messages(ChatId::new("c1").unwrap(), None, 50).await.unwrap();
    assert_eq!(page.items.len(), 6);

    // Retention drops bodies but keeps the messages.
    let n = store.prune_chat_bodies(t("2026-01-01T00:03:30Z"), 0).await.unwrap();
    assert_eq!(n, 4 + 1);
    let page = store.chat_messages(ChatId::new("c1").unwrap(), None, 50).await.unwrap();
    assert_eq!(page.items.len(), 6);
    let pruned: Vec<_> = page.items.iter().filter(|m| m.body.content.is_empty()).map(|m| m.id.as_str()).collect();
    assert_eq!(pruned, ["m3", "m2", "m1", "m0"]);
}

#[tokio::test]
async fn channel_posts_with_reply_counts() {
    let store = Store::open_in_memory().unwrap();
    let team = TeamId::new("t1").unwrap();
    let channel = ChannelId::new("19:general").unwrap();
    let ch: quorum_model::Channel =
        serde_json::from_value(serde_json::json!({"id": "19:general", "displayName": "General"})).unwrap();
    let tm: quorum_model::Team =
        serde_json::from_value(serde_json::json!({"id": "t1", "displayName": "Team"})).unwrap();
    store
        .write({
            let team = team.clone();
            move |tx| {
                tx.upsert_teams(&[tm])?;
                tx.upsert_channels(&team, &[ch])
            }
        })
        .await
        .unwrap();
    store
        .upsert_chat_messages(vec![
            channel_message("t1", "19:general", "p1", None, "2026-01-01T00:00:00Z", "first post"),
            channel_message("t1", "19:general", "p2", None, "2026-01-02T00:00:00Z", "second post"),
            channel_message("t1", "19:general", "r1", Some("p1"), "2026-01-03T00:00:00Z", "reply one"),
            channel_message("t1", "19:general", "r2", Some("p1"), "2026-01-04T00:00:00Z", "reply two"),
        ])
        .await
        .unwrap();
    let posts = store.channel_posts(channel.clone(), None, 10).await.unwrap();
    let summary: Vec<_> = posts.items.iter().map(|p| (p.post.id.as_str(), p.reply_count)).collect();
    assert_eq!(summary, [("p2", 0), ("p1", 2)]);
    assert_eq!(posts.items[1].last_reply_at, Some(t("2026-01-04T00:00:00Z")));
    let replies = store.channel_replies(channel.clone(), ChatMessageId::new("p1").unwrap()).await.unwrap();
    let ids: Vec<_> = replies.items.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["r1", "r2"]);
    assert_eq!(replies.items[0].body.content.as_str(), "reply one");

    let teams = store.read(|r| r.teams()).await.unwrap();
    assert_eq!(teams.items.len(), 1);
    let chans = store.read(move |r| r.channels(&team)).await.unwrap();
    assert_eq!(chans.items[0].team_id.as_ref().map(|t| t.as_str()), Some("t1"));

    // Deleting a post deletes its replies.
    store
        .write(|tx| {
            tx.delete_chat_messages(
                &MessageContainer::Channel { team_id: TeamId::new("t1")?, channel_id: ChannelId::new("19:general")? },
                &[ChatMessageId::new("p1")?],
            )
        })
        .await
        .unwrap();
    let posts = store.channel_posts(channel, None, 10).await.unwrap();
    assert_eq!(posts.items.len(), 1);
}
