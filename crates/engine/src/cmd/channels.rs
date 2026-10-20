//! Teams channel commands and queries: open, post, reply, edit, delete, react; teams with their
//! channels, channel posts (with reply summaries) and replies.
//!
//! Every command takes `channel` (or `id`) and an optional `team`; without `team` the channel's
//! team is looked up in the store.

use quorum_ipc::CoreEvent;
use quorum_model::{ChannelId, ChatMessage, ChatMessageId, ChatMessageType, MessageContainer, TeamId, UserId};
use quorum_store::{Cursor, DraftKind};
use quorum_sync::teams::payload::{SendPayload, Target};
use quorum_sync::teams::{OPEN_CHANNEL_SETTING, OpenChannel, channel_key};
use serde::Deserialize;
use serde_json::{Value, json};

use super::chat::{
    DataParam, MentionParam, Prepared, check_reaction, delete_target, draft_inner, edit_target, enqueue, ensure_mine,
    has_content, identity, importance, is_local, local_id, mentions, message_id, message_row, outgoing_html,
    pending_message, prepare_attachments, react_target, save_draft_inner, store_blobs,
};
use super::people::store_err;
use crate::command::{CommandFuture, CommandSpec, always, parse};
use crate::error::{Error, ErrorKind, Result};
use crate::session::Session;

fn spec(
    id: &'static str,
    label: &'static str,
    menu: Option<&'static str>,
    shortcut: Option<&'static str>,
    params_doc: &'static str,
    destructive: bool,
    run: crate::command::RunFn,
) -> CommandSpec {
    CommandSpec { id, label, menu, shortcut, params_doc, destructive, enabled: always, run }
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        spec(
            "channel.open",
            "Open Channel",
            None,
            None,
            r#"{"team"?: team id, "channel"|"id": channel id, "post"?: message id}"#,
            false,
            open,
        ),
        spec(
            "channel.post",
            "New Channel Post",
            Some("Teams/New Post"),
            Some("Cmd+N"),
            r#"{"team"?: team id, "channel"|"id": channel id, "subject"?: string, "html"?: string, "text"?: string, "mentions"?: [{"id": n, "text": string, "user"|"target": id, "kind"?: "user"|"channel"|"team"}], "importance"?: string, "images"?: [...], "files"?: [...]} -> {"local_id"}"#,
            true,
            post,
        ),
        spec(
            "channel.reply",
            "Reply to Post",
            Some("Teams/Reply"),
            None,
            r#"{"team"?: team id, "channel": channel id, "post": message id, "html"?: string, "text"?: string, "mentions"?: [...], "images"?: [...], "files"?: [...]} -> {"local_id"}"#,
            true,
            reply,
        ),
        spec(
            "channel.edit",
            "Edit Channel Message",
            None,
            None,
            r#"{"team"?: team id, "channel": channel id, "message": post id, "reply"?: reply id, "html"?: string, "text"?: string, "mentions"?: [...]}"#,
            false,
            edit,
        ),
        spec(
            "channel.delete",
            "Delete Channel Message",
            None,
            None,
            r#"{"team"?: team id, "channel": channel id, "message": post id, "reply"?: reply id}"#,
            true,
            delete,
        ),
        spec(
            "channel.react",
            "React to Channel Message",
            None,
            None,
            r#"{"team"?: team id, "channel": channel id, "message": post id, "reply"?: reply id, "reaction": emoji, "remove"?: bool}"#,
            false,
            react,
        ),
        spec(
            "channel.save_draft",
            "Save Channel Draft",
            None,
            None,
            r#"{"team"?: team id, "channel": channel id, "post"?: message id (reply draft), "html": string}"#,
            false,
            save_draft,
        ),
    ]
}

pub(crate) fn queries() -> Vec<CommandSpec> {
    vec![
        spec(
            "teams.list",
            "Teams and Channels",
            None,
            None,
            "{} -> [{id, name, description, archived, channels: [...]}]",
            false,
            teams_list,
        ),
        spec(
            "channel.posts",
            "Channel Posts",
            None,
            None,
            r#"{"team"?: team id, "channel"|"id": channel id, "before"?: cursor, "limit"?: 1..100 (default 30)} -> {"items": [post, oldest first, with "reply_count" and the latest "replies"], "next"}"#,
            false,
            posts,
        ),
        spec(
            "channel.replies",
            "Post Replies",
            None,
            None,
            r#"{"team"?: team id, "channel": channel id, "post": message id} -> [reply, oldest first]"#,
            false,
            replies,
        ),
        spec(
            "channel.draft",
            "Channel Draft",
            None,
            None,
            r#"{"team"?: team id, "channel": channel id, "post"?: message id}"#,
            false,
            draft,
        ),
    ]
}

/// Replies shown under each post in `channel.posts`.
const INLINE_REPLIES: usize = 3;

#[derive(Deserialize)]
struct Where {
    #[serde(default)]
    team: Option<String>,
    #[serde(default, alias = "id")]
    channel: Option<String>,
}

/// Resolves the channel (and its team, from the store when not given).
async fn locate(s: &Session, team: Option<String>, channel: Option<String>) -> Result<(TeamId, ChannelId)> {
    let channel = ChannelId::new(channel.unwrap_or_default())
        .map_err(|_| Error::params("`channel` (or `id`) must be a channel id"))?;
    if let Some(t) = team {
        let t = TeamId::new(t).map_err(|_| Error::params("`team` must be a team id"))?;
        return Ok((t, channel));
    }
    let store = s.store()?;
    let c = channel.clone();
    let team = store
        .read(move |r| {
            for t in r.teams()?.items {
                if r.channels(&t.id)?.items.iter().any(|x| x.id == c) {
                    return Ok(Some(t.id));
                }
            }
            Ok(None)
        })
        .await
        .map_err(store_err)?;
    let team = team.ok_or_else(|| Error::new(ErrorKind::NotFound, "that channel is not cached; pass `team`"))?;
    Ok((team, channel))
}

fn container(team: &TeamId, channel: &ChannelId) -> MessageContainer {
    MessageContainer::Channel { team_id: team.clone(), channel_id: channel.clone() }
}

fn draft_id(team: &TeamId, channel: &ChannelId, post: Option<&ChatMessageId>) -> String {
    match post {
        Some(p) => format!("channel:{}/{}/{}", team.as_str(), channel.as_str(), p.as_str()),
        None => format!("channel:{}/{}", team.as_str(), channel.as_str()),
    }
}

#[derive(Deserialize)]
struct OpenParams {
    #[serde(default)]
    team: Option<String>,
    #[serde(default, alias = "id")]
    channel: Option<String>,
    #[serde(default)]
    post: Option<String>,
}

fn open(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: OpenParams = parse(p)?;
        let (team, channel) = locate(s, p.team, p.channel).await?;
        let store = s.store()?;
        let open = OpenChannel { team: team.clone(), channel: channel.clone() };
        store.set_setting(OPEN_CHANNEL_SETTING.to_owned(), open).await.map_err(store_err)?;
        if let Ok(sync) = s.sync() {
            let key = channel_key(&team, &channel);
            sync.set_focus(&key);
            sync.poke(&key);
        }
        // The Teams area selects `team/channel` in its tree and the post (if any) in the feed.
        let nav = format!("{}/{}", team.as_str(), channel.as_str());
        let mut params = json!({ "area": "teams", "nav": nav, "team": team.as_str(), "channel": channel.as_str() });
        if let (Some(post), Some(obj)) = (p.post, params.as_object_mut()) {
            obj.insert("id".into(), Value::String(post.clone()));
            obj.insert("post".into(), Value::String(post));
        }
        s.emit(CoreEvent::Ui { command: "view.open".into(), params });
        Ok(json!({ "team": team.as_str(), "channel": channel.as_str() }))
    })
}

#[derive(Deserialize)]
struct PostParams {
    #[serde(default)]
    team: Option<String>,
    #[serde(default, alias = "id")]
    channel: Option<String>,
    #[serde(default)]
    post: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    html: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    mentions: Vec<MentionParam>,
    #[serde(default)]
    importance: Option<String>,
    #[serde(default)]
    images: Vec<DataParam>,
    #[serde(default)]
    files: Vec<DataParam>,
}

async fn send_impl(s: &Session, p: PostParams, is_reply: bool) -> Result<Value> {
    let (team, channel) = locate(s, p.team, p.channel).await?;
    let post = match (is_reply, p.post) {
        (true, Some(post)) => Some(message_id(&post)?),
        (true, None) => return Err(Error::params("`post` (the message to reply to) is required")),
        (false, _) => None,
    };
    if post.as_ref().is_some_and(|p| is_local(p.as_str())) {
        return Err(Error::new(ErrorKind::NotAvailable, "that post is still being sent"));
    }
    let subject = p.subject.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    if is_reply && subject.is_some() {
        return Err(Error::params("replies have no subject"));
    }
    if subject.as_ref().is_some_and(|s| s.chars().count() > 250 || s.chars().any(char::is_control)) {
        return Err(Error::params("subject must be at most 250 characters"));
    }
    let html = outgoing_html(p.html.as_deref(), p.text.as_deref())?;
    let mentions = mentions(&html, &p.mentions)?;
    let imp = importance(p.importance.as_deref())?;
    let local = local_id("post");
    let prepared = prepare_attachments(&local, &p.images, &p.files)?;
    if !has_content(&html) && prepared.images.is_empty() && prepared.files.is_empty() {
        return Err(Error::params("write a message first (`html` or `text`)"));
    }
    let (me, _) = identity(s).await;
    let local_msg = ChatMessageId::new(local.clone()).map_err(|_| Error::new(ErrorKind::Bug, "local id"))?;
    let Prepared { images, files, image_html, blobs } = prepared;
    store_blobs(s, blobs).await?;
    let full = format!("{html}{image_html}");
    let optimistic = pending_message(
        local_msg.clone(),
        container(&team, &channel),
        post.clone(),
        me.as_ref(),
        full.clone(),
        subject.clone(),
        &mentions,
        &files,
        imp.clone(),
    );
    let payload = SendPayload {
        target: Target::Channel { team: team.clone(), channel: channel.clone(), message: post.clone(), reply: None },
        local_id: local_msg,
        html: full,
        subject,
        importance: Some(quorum_model::GraphEnum::as_graph(&imp).to_owned()),
        mentions,
        images,
        files,
        quote: None,
    };
    let kind = if is_reply { "channel.reply" } else { "channel.post" };
    let draft = draft_id(&team, &channel, post.as_ref());
    let value = serde_json::to_value(&payload)
        .map_err(|e| Error::new(ErrorKind::Bug, "payload encode failed").with_source(e))?;
    enqueue(s, kind, channel_key(&team, &channel), Some(local.clone()), value, move |tx| {
        tx.upsert_chat_messages(std::slice::from_ref(&optimistic))?;
        tx.delete_draft(&draft)?;
        Ok(())
    })
    .await?;
    Ok(json!({ "local_id": local }))
}

fn post(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move { send_impl(s, parse(p)?, false).await })
}

fn reply(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move { send_impl(s, parse(p)?, true).await })
}

#[derive(Deserialize)]
struct MessageParams {
    #[serde(default)]
    team: Option<String>,
    #[serde(default, alias = "id")]
    channel: Option<String>,
    message: String,
    #[serde(default)]
    reply: Option<String>,
    #[serde(default)]
    html: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    mentions: Vec<MentionParam>,
    #[serde(default)]
    reaction: Option<String>,
    #[serde(default)]
    remove: bool,
}

async fn target_of(s: &Session, p: &MessageParams) -> Result<(Target, ChatMessage)> {
    let (team, channel) = locate(s, p.team.clone(), p.channel.clone()).await?;
    let message = message_id(&p.message)?;
    let reply = p.reply.as_deref().map(message_id).transpose()?;
    let shown = reply.clone().unwrap_or_else(|| message.clone());
    if is_local(shown.as_str()) || is_local(message.as_str()) {
        return Err(Error::new(ErrorKind::NotAvailable, "this message is still being sent"));
    }
    let store = s.store()?;
    let c = container(&team, &channel);
    let id = shown.clone();
    let cached = store
        .read(move |r| r.chat_message(&c, &id))
        .await
        .map_err(store_err)?
        .ok_or_else(|| Error::new(ErrorKind::NotFound, "that message is not cached"))?;
    Ok((Target::Channel { team, channel, message: Some(message), reply }, cached))
}

fn edit(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: MessageParams = parse(p)?;
        let html = outgoing_html(p.html.as_deref(), p.text.as_deref())?;
        if !has_content(&html) {
            return Err(Error::params("the new text is empty; delete the message instead"));
        }
        let mentions = mentions(&html, &p.mentions)?;
        let (target, before) = target_of(s, &p).await?;
        let (me, _) = identity(s).await;
        ensure_mine(&before, me.as_ref(), "edit")?;
        edit_target(s, "channel.edit", target, html, mentions, before).await
    })
}

fn delete(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: MessageParams = parse(p)?;
        let (target, before) = target_of(s, &p).await?;
        let (me, _) = identity(s).await;
        ensure_mine(&before, me.as_ref(), "delete")?;
        delete_target(s, "channel.delete", target, before).await
    })
}

fn react(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: MessageParams = parse(p)?;
        let reaction = check_reaction(p.reaction.as_deref())?;
        let (target, before) = target_of(s, &p).await?;
        react_target(s, "channel.react", target, reaction, p.remove, before).await
    })
}

#[derive(Deserialize)]
struct DraftParams {
    #[serde(default)]
    team: Option<String>,
    #[serde(default, alias = "id")]
    channel: Option<String>,
    #[serde(default)]
    post: Option<String>,
    #[serde(default)]
    html: String,
}

fn save_draft(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: DraftParams = parse(p)?;
        let (team, channel) = locate(s, p.team, p.channel).await?;
        let post = p.post.as_deref().map(message_id).transpose()?;
        let id = draft_id(&team, &channel, post.as_ref());
        save_draft_inner(s, id.clone(), DraftKind::Channel, id, p.html, json!([])).await
    })
}

fn draft(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: DraftParams = parse(p)?;
        let (team, channel) = locate(s, p.team, p.channel).await?;
        let post = p.post.as_deref().map(message_id).transpose()?;
        draft_inner(s, draft_id(&team, &channel, post.as_ref())).await
    })
}

fn teams_list(s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let store = s.store()?;
        let out = store
            .read(|r| {
                let mut out = Vec::new();
                for t in r.teams()?.items {
                    let channels: Vec<Value> = r
                        .channels(&t.id)?
                        .items
                        .into_iter()
                        .map(|c| {
                            json!({
                                "id": c.id.as_str(),
                                "team": t.id.as_str(),
                                "name": c.display_name.as_str(),
                                "description": c.description.as_ref().map(|d| d.as_str().to_owned()),
                                "membership": quorum_model::GraphEnum::as_graph(&c.membership_type),
                                "general": c.display_name.as_str().eq_ignore_ascii_case("general"),
                                "web_url": c.web_url,
                            })
                        })
                        .collect();
                    out.push(json!({
                        "id": t.id.as_str(),
                        "name": t.display_name.as_str(),
                        "description": t.description.as_ref().map(|d| d.as_str().to_owned()),
                        "archived": t.is_archived,
                        "channels": channels,
                    }));
                }
                Ok(out)
            })
            .await
            .map_err(store_err)?;
        Ok(Value::Array(out))
    })
}

#[derive(Deserialize)]
struct PostsParams {
    #[serde(default)]
    team: Option<String>,
    #[serde(default, alias = "id")]
    channel: Option<String>,
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

fn posts(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: PostsParams = parse(p)?;
        let (team, channel) = locate(s, p.team, p.channel).await?;
        let limit = p.limit.unwrap_or(30).clamp(1, 100);
        if p.before.as_ref().is_some_and(|b| b.len() > 2048) {
            return Err(Error::params("`before` is not a valid cursor"));
        }
        let before = p.before.map(Cursor::from_string);
        let (me, _) = identity(s).await;
        let store = s.store()?;
        let ch = channel.clone();
        let key = channel_key(&team, &channel);
        let (page, replies, synced) = store
            .read(move |r| {
                let page = r.channel_posts(&ch, before.as_ref(), limit)?;
                let mut replies = Vec::new();
                for post in &page.items {
                    let list =
                        if post.reply_count > 0 { r.channel_replies(&ch, &post.post.id)?.items } else { Vec::new() };
                    replies.push(list);
                }
                Ok((page, replies, r.delta_state(&key)?.is_some()))
            })
            .await
            .map_err(|e| match e.kind() {
                quorum_store::ErrorKind::InvalidInput => Error::params("`before` is not a valid cursor"),
                _ => store_err(e),
            })?;
        let mut items: Vec<Value> = page
            .items
            .iter()
            .zip(replies.iter())
            .filter(|(p, _)| p.post.message_type != ChatMessageType::Typing)
            .map(|(p, reps)| {
                let mut row = message_row(&p.post, me.as_ref());
                let shown: Vec<Value> =
                    reps.iter().rev().take(INLINE_REPLIES).rev().map(|m| message_row(m, me.as_ref())).collect();
                if let Some(obj) = row.as_object_mut() {
                    obj.insert(
                        "reply_count".into(),
                        json!(reps.len().max(usize::try_from(p.reply_count).unwrap_or(0))),
                    );
                    obj.insert("last_reply_at".into(), json!(p.last_reply_at));
                    obj.insert("replies".into(), Value::Array(shown));
                }
                row
            })
            .collect();
        items.reverse();
        Ok(json!({
            "team": team.as_str(),
            "channel": channel.as_str(),
            "items": items,
            "next": page.next.as_ref().map(Cursor::as_str),
            "synced": synced,
            "me": me.as_ref().map(UserId::as_str),
        }))
    })
}

#[derive(Deserialize)]
struct RepliesParams {
    #[serde(flatten)]
    at: Where,
    post: String,
}

fn replies(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: RepliesParams = parse(p)?;
        let (_team, channel) = locate(s, p.at.team, p.at.channel).await?;
        let post = message_id(&p.post)?;
        let (me, _) = identity(s).await;
        let list = s.store()?.channel_replies(channel, post).await.map_err(store_err)?;
        Ok(Value::Array(list.items.iter().map(|m| message_row(m, me.as_ref())).collect()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_ids_are_scoped() {
        let t = TeamId::new("t").unwrap();
        let c = ChannelId::new("c").unwrap();
        let p = ChatMessageId::new("p").unwrap();
        assert_eq!(draft_id(&t, &c, None), "channel:t/c");
        assert_eq!(draft_id(&t, &c, Some(&p)), "channel:t/c/p");
    }
}
