//! Teams and channels: joined teams, channels (including shared ones), channel posts and replies
//! with the channel message delta, members, and the channel's files folder.
//!
//! Editing, soft-deleting and reacting to channel posts and replies use the shared calls in
//! [`chat`](super::chat) with a [`MessageRef::ChannelPost`] or [`MessageRef::ChannelReply`].

use jiff::Timestamp;
use quorum_model::{
    Channel, ChannelId, ChannelMembershipType, ChatMessage, ChatMessageId, DriveItem, GraphEnum, MessageContainer,
    Team, TeamId,
};
use serde::Deserialize;
use serde_json::Value;

use super::chat::{
    ConversationMember, MAX_CHAT_PAGE_SIZE, MAX_ITEMS, MAX_NAME_CHARS, MAX_TITLE_CHARS, MessageRef, OutgoingMessage,
    TEAMS_BUCKET, id_of, lenient, map_page, message_payload, parse_chat_message, parse_conversation_member, seg, sens,
    ts, web_url,
};
use super::files::parse_drive_item;
use crate::client::Client;
use crate::error::{Error, ErrorKind, Result};
use crate::paging::{DeltaOutcome, DeltaPage, DeltaRound, Page};
use crate::request::{Request, ResourceFamily};

const SELECT_TEAM: &str = "id,displayName,description,isArchived,webUrl";
const SELECT_CHANNEL: &str = "id,displayName,description,membershipType,email,createdDateTime,webUrl";

fn teams(req: Request) -> Request {
    req.family(ResourceFamily::Teams).bucket(TEAMS_BUCKET)
}

fn team_path(team: &TeamId) -> String {
    format!("/teams/{}", seg(team.as_str()))
}

fn channel_path(team: &TeamId, channel: &ChannelId) -> String {
    format!("{}/channels/{}", team_path(team), seg(channel.as_str()))
}

fn container(team: &TeamId, channel: &ChannelId) -> MessageContainer {
    MessageContainer::Channel { team_id: team.clone(), channel_id: channel.clone() }
}

// ---------------------------------------------------------------------------------------------
// Mapping.
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireTeam {
    #[serde(default, deserialize_with = "lenient")]
    id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    display_name: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    description: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    is_archived: Option<bool>,
    #[serde(default, deserialize_with = "lenient")]
    web_url: Option<String>,
}

/// Maps a Graph `team` to a [`Team`] (`None` without a valid id).
#[must_use]
pub fn parse_team(value: Value) -> Option<Team> {
    let w: WireTeam = serde_json::from_value(value).ok()?;
    Some(Team {
        id: id_of(w.id)?,
        display_name: sens(w.display_name, MAX_NAME_CHARS),
        description: w.description.filter(|d| !d.is_empty()).map(|d| sens(Some(d), MAX_TITLE_CHARS)),
        is_archived: w.is_archived.unwrap_or(false),
        web_url: web_url(w.web_url),
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireChannel {
    #[serde(default, deserialize_with = "lenient")]
    id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    display_name: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    description: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    membership_type: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    email: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    created_date_time: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    web_url: Option<String>,
}

/// Maps a Graph `channel` to a [`Channel`] of `team` (Graph does not repeat the team id).
#[must_use]
pub fn parse_channel(value: Value, team: &TeamId) -> Option<Channel> {
    let w: WireChannel = serde_json::from_value(value).ok()?;
    Some(Channel {
        id: id_of(w.id)?,
        team_id: Some(team.clone()),
        display_name: sens(w.display_name, MAX_NAME_CHARS),
        description: w.description.filter(|d| !d.is_empty()).map(|d| sens(Some(d), MAX_TITLE_CHARS)),
        membership_type: w.membership_type.as_deref().map(ChannelMembershipType::from_graph).unwrap_or_default(),
        email: w.email.filter(|e| !e.is_empty()).map(|e| sens(Some(e), 320)),
        created_date_time: ts(w.created_date_time.as_deref()),
        web_url: web_url(w.web_url),
    })
}

/// One change from the channel message delta.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChannelMessageChange {
    /// A new or changed post or reply (deleted ones carry `deleted_date_time`).
    Changed(Box<ChatMessage>),
    /// Graph reported the message as `@removed`.
    Removed(ChatMessageId),
}

/// Maps one delta entry: the post, followed by its expanded `replies`.
fn delta_changes(mut value: Value, container: &MessageContainer) -> Vec<ChannelMessageChange> {
    if value.get("@removed").is_some() {
        return id_of::<ChatMessageId>(value.get("id").and_then(Value::as_str).map(str::to_owned))
            .map(ChannelMessageChange::Removed)
            .into_iter()
            .collect();
    }
    let replies = value.get_mut("replies").map(Value::take);
    let mut out = Vec::new();
    let Some(post) = parse_chat_message(value, Some(container)) else { return out };
    let parent = post.id.clone();
    out.push(ChannelMessageChange::Changed(Box::new(post)));
    if let Some(Value::Array(replies)) = replies {
        for r in replies.into_iter().take(MAX_ITEMS) {
            if let Some(mut reply) = parse_chat_message(r, Some(container)) {
                reply.reply_to_id.get_or_insert_with(|| parent.clone());
                out.push(ChannelMessageChange::Changed(Box::new(reply)));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Request builders.
// ---------------------------------------------------------------------------------------------

/// `GET /me/joinedTeams`.
#[must_use]
pub fn list_joined_teams_request() -> Request {
    teams(Request::get("/me/joinedTeams"))
}

/// `GET /teams/{id}`.
#[must_use]
pub fn get_team_request(team: &TeamId) -> Request {
    teams(Request::get(team_path(team)).query("$select", SELECT_TEAM))
}

/// `GET /teams/{id}/channels`, or `/allChannels` with `include_shared` (adds channels shared into
/// the team from other teams).
#[must_use]
pub fn list_channels_request(team: &TeamId, include_shared: bool) -> Request {
    let leaf = if include_shared { "allChannels" } else { "channels" };
    teams(Request::get(format!("{}/{leaf}", team_path(team))).query("$select", SELECT_CHANNEL))
}

/// `GET /teams/{id}/channels/{id}`.
#[must_use]
pub fn get_channel_request(team: &TeamId, channel: &ChannelId) -> Request {
    teams(Request::get(channel_path(team, channel)).query("$select", SELECT_CHANNEL))
}

/// `GET /teams/{id}/channels/{id}/messages` (top-level posts, newest first).
#[must_use]
pub fn list_channel_messages_request(team: &TeamId, channel: &ChannelId, top: u32) -> Request {
    teams(
        Request::get(format!("{}/messages", channel_path(team, channel)))
            .query("$top", top.clamp(1, MAX_CHAT_PAGE_SIZE).to_string()),
    )
}

/// `GET /teams/{id}/channels/{id}/messages/delta` with replies expanded: the first request of a
/// delta round. With `modified_after`, only posts changed since then. Later rounds use the stored
/// `@odata.deltaLink` instead.
#[must_use]
pub fn channel_messages_delta_request(
    team: &TeamId,
    channel: &ChannelId,
    modified_after: Option<Timestamp>,
    top: Option<u32>,
) -> Request {
    let mut req = Request::get(format!("{}/messages/delta", channel_path(team, channel))).query("$expand", "replies");
    if let Some(t) = modified_after {
        req = req.query("$filter", format!("lastModifiedDateTime gt {t}"));
    }
    if let Some(top) = top {
        req = req.query("$top", top.clamp(1, MAX_CHAT_PAGE_SIZE).to_string());
    }
    teams(req)
}

/// `GET /teams/{id}/channels/{id}/messages/{id}`.
#[must_use]
pub fn get_channel_message_request(team: &TeamId, channel: &ChannelId, message: &ChatMessageId) -> Request {
    let r = MessageRef::ChannelPost { team: team.clone(), channel: channel.clone(), message: message.clone() };
    teams(Request::get(r.path()))
}

/// `GET /teams/{id}/channels/{id}/messages/{id}/replies`.
#[must_use]
pub fn list_replies_request(team: &TeamId, channel: &ChannelId, message: &ChatMessageId, top: u32) -> Request {
    let r = MessageRef::ChannelPost { team: team.clone(), channel: channel.clone(), message: message.clone() };
    teams(Request::get(format!("{}/replies", r.path())).query("$top", top.clamp(1, MAX_CHAT_PAGE_SIZE).to_string()))
}

/// `POST /teams/{id}/channels/{id}/messages` (a new post; `subject` is allowed).
///
/// # Errors
/// As [`message_payload`].
pub fn post_channel_message_request(team: &TeamId, channel: &ChannelId, msg: &OutgoingMessage) -> Result<Request> {
    Ok(teams(Request::post(format!("{}/messages", channel_path(team, channel))).json_value(message_payload(msg)?)))
}

/// `POST /teams/{id}/channels/{id}/messages/{id}/replies`.
///
/// # Errors
/// As [`message_payload`]; [`ErrorKind::Config`] if the reply has a subject.
pub fn reply_to_channel_message_request(
    team: &TeamId,
    channel: &ChannelId,
    message: &ChatMessageId,
    msg: &OutgoingMessage,
) -> Result<Request> {
    if msg.subject.is_some() {
        return Err(Error::new(ErrorKind::Config, "replies have no subject"));
    }
    let r = MessageRef::ChannelPost { team: team.clone(), channel: channel.clone(), message: message.clone() };
    Ok(teams(Request::post(format!("{}/replies", r.path())).json_value(message_payload(msg)?)))
}

/// `GET /teams/{id}/channels/{id}/filesFolder`: the channel's SharePoint folder.
#[must_use]
pub fn channel_files_folder_request(team: &TeamId, channel: &ChannelId) -> Request {
    teams(Request::get(format!("{}/filesFolder", channel_path(team, channel))))
}

/// `GET /teams/{id}/members`.
#[must_use]
pub fn list_team_members_request(team: &TeamId) -> Request {
    teams(Request::get(format!("{}/members", team_path(team))))
}

/// `GET /teams/{id}/channels/{id}/members`.
#[must_use]
pub fn list_channel_members_request(team: &TeamId, channel: &ChannelId) -> Request {
    teams(Request::get(format!("{}/members", channel_path(team, channel))))
}

// ---------------------------------------------------------------------------------------------
// Typed calls.
// ---------------------------------------------------------------------------------------------

/// Page cap for small rosters (teams, channels, members).
const ROSTER_PAGES: usize = 50;

impl Client {
    /// The teams the user is a member of.
    ///
    /// # Errors
    /// As [`Client::collect_all`].
    pub async fn list_joined_teams(&self) -> Result<Vec<Team>> {
        let raw: Vec<Value> = self.collect_all(list_joined_teams_request(), ROSTER_PAGES).await?;
        Ok(raw.into_iter().filter_map(parse_team).collect())
    }

    /// One team.
    ///
    /// # Errors
    /// As [`Client::send_json`]; `Malformed` if the team does not map.
    pub async fn get_team(&self, team: &TeamId) -> Result<Team> {
        self.send_mapped(get_team_request(team), "team", parse_team).await
    }

    /// The channels of a team (see [`list_channels_request`]).
    ///
    /// # Errors
    /// As [`Client::collect_all`].
    pub async fn list_channels(&self, team: &TeamId, include_shared: bool) -> Result<Vec<Channel>> {
        let raw: Vec<Value> = self.collect_all(list_channels_request(team, include_shared), ROSTER_PAGES).await?;
        Ok(raw.into_iter().filter_map(|v| parse_channel(v, team)).collect())
    }

    /// One channel.
    ///
    /// # Errors
    /// As [`Client::send_json`]; `Malformed` if the channel does not map.
    pub async fn get_channel(&self, team: &TeamId, channel: &ChannelId) -> Result<Channel> {
        self.send_mapped(get_channel_request(team, channel), "channel", |v| parse_channel(v, team)).await
    }

    /// The first page of a channel's posts.
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_channel_messages(
        &self,
        team: &TeamId,
        channel: &ChannelId,
        top: u32,
    ) -> Result<Page<ChatMessage>> {
        let c = container(team, channel);
        let page = self.page(list_channel_messages_request(team, channel, top)).await?;
        Ok(map_page(page, |v| parse_chat_message(v, Some(&c))))
    }

    /// The next page of channel posts or replies from a `nextLink`.
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_channel_messages_next(
        &self,
        team: &TeamId,
        channel: &ChannelId,
        next_link: &str,
    ) -> Result<Page<ChatMessage>> {
        let c = container(team, channel);
        let page = self.page(teams(Request::link(next_link))).await?;
        Ok(map_page(page, |v| parse_chat_message(v, Some(&c))))
    }

    /// One page of the channel message delta: `link` is a `nextLink` or stored `deltaLink`, or
    /// `None` to start a fresh round with [`channel_messages_delta_request`].
    ///
    /// # Errors
    /// [`ErrorKind::DeltaExpired`] when the stored link is no longer valid (start over with
    /// `None`); otherwise as [`Client::delta_page`].
    pub async fn channel_messages_delta(
        &self,
        team: &TeamId,
        channel: &ChannelId,
        link: Option<&str>,
    ) -> Result<DeltaPage<ChannelMessageChange>> {
        let req = match link {
            Some(l) => teams(Request::link(l)),
            None => channel_messages_delta_request(team, channel, None, None),
        };
        let c = container(team, channel);
        let page = self.delta_page::<Value>(req).await?;
        let mut items = Vec::new();
        let mut skipped = page.skipped;
        for v in page.items {
            let changes = delta_changes(v, &c);
            if changes.is_empty() {
                skipped = skipped.saturating_add(1);
            }
            items.extend(changes);
        }
        Ok(DeltaPage { items, skipped, outcome: page.outcome })
    }

    /// A whole channel delta round, following `nextLink`s until the `deltaLink` (up to
    /// `max_pages`). Pass the stored `deltaLink`, or `None` for an initial round.
    ///
    /// # Errors
    /// As [`Client::channel_messages_delta`]; `TooLarge` when the cap is reached; `Malformed` for a
    /// `nextLink` loop.
    pub async fn channel_messages_delta_round(
        &self,
        team: &TeamId,
        channel: &ChannelId,
        link: Option<&str>,
        max_pages: usize,
    ) -> Result<DeltaRound<ChannelMessageChange>> {
        let mut items = Vec::new();
        let mut skipped = 0usize;
        let mut seen = std::collections::HashSet::new();
        let mut next: Option<String> = link.map(str::to_owned);
        for pages in 1..=max_pages.max(1) {
            let page = self.channel_messages_delta(team, channel, next.as_deref()).await?;
            items.extend(page.items);
            skipped = skipped.saturating_add(page.skipped);
            match page.outcome {
                DeltaOutcome::Done(delta_link) => return Ok(DeltaRound { items, skipped, delta_link, pages }),
                DeltaOutcome::Next(l) => {
                    if !seen.insert(l.clone()) {
                        return Err(Error::new(ErrorKind::Malformed, "nextLink repeats an earlier page"));
                    }
                    next = Some(l);
                }
            }
        }
        Err(Error::new(ErrorKind::TooLarge, "page cap reached before the last page"))
    }

    /// One channel post.
    ///
    /// # Errors
    /// As [`Client::send_json`]; `Malformed` if the message does not map.
    pub async fn get_channel_message(
        &self,
        team: &TeamId,
        channel: &ChannelId,
        message: &ChatMessageId,
    ) -> Result<ChatMessage> {
        let c = container(team, channel);
        self.send_mapped(get_channel_message_request(team, channel, message), "channel message", |v| {
            parse_chat_message(v, Some(&c))
        })
        .await
    }

    /// The first page of replies to a post (each reply's `reply_to_id` is set to the post).
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_replies(
        &self,
        team: &TeamId,
        channel: &ChannelId,
        message: &ChatMessageId,
        top: u32,
    ) -> Result<Page<ChatMessage>> {
        let c = container(team, channel);
        let page = self.page(list_replies_request(team, channel, message, top)).await?;
        Ok(map_page(page, |v| {
            let mut m = parse_chat_message(v, Some(&c))?;
            m.reply_to_id.get_or_insert_with(|| message.clone());
            Some(m)
        }))
    }

    /// Posts a new message in a channel.
    ///
    /// # Errors
    /// As [`post_channel_message_request`] and [`Client::send_json`].
    pub async fn post_channel_message(
        &self,
        team: &TeamId,
        channel: &ChannelId,
        msg: &OutgoingMessage,
    ) -> Result<ChatMessage> {
        let c = container(team, channel);
        self.send_mapped(post_channel_message_request(team, channel, msg)?, "channel message", |v| {
            parse_chat_message(v, Some(&c))
        })
        .await
    }

    /// Replies to a channel post.
    ///
    /// # Errors
    /// As [`reply_to_channel_message_request`] and [`Client::send_json`].
    pub async fn reply_to_channel_message(
        &self,
        team: &TeamId,
        channel: &ChannelId,
        message: &ChatMessageId,
        msg: &OutgoingMessage,
    ) -> Result<ChatMessage> {
        let c = container(team, channel);
        self.send_mapped(reply_to_channel_message_request(team, channel, message, msg)?, "channel reply", |v| {
            let mut m = parse_chat_message(v, Some(&c))?;
            m.reply_to_id.get_or_insert_with(|| message.clone());
            Some(m)
        })
        .await
    }

    /// The channel's files folder (a drive item in the team's SharePoint document library).
    ///
    /// # Errors
    /// As [`Client::send_json`]; `Malformed` if the item does not map.
    pub async fn channel_files_folder(&self, team: &TeamId, channel: &ChannelId) -> Result<DriveItem> {
        self.send_mapped(channel_files_folder_request(team, channel), "drive item", |v| parse_drive_item(v, None)).await
    }

    /// The members of a team.
    ///
    /// # Errors
    /// As [`Client::collect_all`].
    pub async fn list_team_members(&self, team: &TeamId) -> Result<Vec<ConversationMember>> {
        let raw: Vec<Value> = self.collect_all(list_team_members_request(team), ROSTER_PAGES).await?;
        Ok(raw.into_iter().filter_map(parse_conversation_member).collect())
    }

    /// The members of a channel (meaningful for private and shared channels).
    ///
    /// # Errors
    /// As [`Client::collect_all`].
    pub async fn list_channel_members(&self, team: &TeamId, channel: &ChannelId) -> Result<Vec<ConversationMember>> {
        let raw: Vec<Value> = self.collect_all(list_channel_members_request(team, channel), ROSTER_PAGES).await?;
        Ok(raw.into_iter().filter_map(parse_conversation_member).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_removed_and_replies() {
        let team = TeamId::new("t").unwrap();
        let channel = ChannelId::new("c").unwrap();
        let c = container(&team, &channel);
        let removed = delta_changes(serde_json::json!({"id": "9", "@removed": {"reason": "deleted"}}), &c);
        assert_eq!(removed, vec![ChannelMessageChange::Removed(ChatMessageId::new("9").unwrap())]);
        let changes = delta_changes(serde_json::json!({"id": "1", "replies": [{"id": "2"}, {"nope": 1}]}), &c);
        assert_eq!(changes.len(), 2);
    }
}
