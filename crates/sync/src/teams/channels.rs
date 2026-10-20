//! Joined teams, their channels, and the channel message delta.

use std::collections::HashSet;

use futures::future::BoxFuture;
use quorum_graph::api::teams::ChannelMessageChange;
use quorum_model::{Channel, ChannelId, ChatMessageId, MessageContainer, Team, TeamId};
use quorum_store::DeltaState;

use super::{SharedFocus, TEAMS_KEY, channel_key, graph_err, open_channel_is};
use crate::{Cadence, Ctx, Outcome, Resource, Result};

/// Most teams kept.
const MAX_TEAMS: usize = 200;
/// Pages of one channel delta round.
const DELTA_MAX_PAGES: usize = 20;
/// New channels given their first delta round per roster pass.
pub(crate) const FIRST_ROUNDS_PER_PASS: usize = 3;

pub(crate) struct Roster;

pub(crate) struct ChannelResource {
    pub(crate) team: TeamId,
    pub(crate) channel: ChannelId,
    pub(crate) focus: SharedFocus,
}

impl Resource for Roster {
    fn key(&self) -> String {
        TEAMS_KEY.to_owned()
    }

    fn area(&self) -> &'static str {
        "teams"
    }

    fn cadence(&self) -> Cadence {
        Cadence::secs(300, 600, 1800)
    }

    fn run<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(sync_roster(ctx))
    }
}

impl Resource for ChannelResource {
    fn key(&self) -> String {
        channel_key(&self.team, &self.channel)
    }

    fn area(&self) -> &'static str {
        "teams"
    }

    fn cadence(&self) -> Cadence {
        if open_channel_is(&self.focus, &self.team, &self.channel) {
            Cadence::secs(5, 120, 600)
        } else {
            Cadence::secs(120, 120, 600)
        }
    }

    fn run<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(async move {
            sync_channel(ctx, &self.team, &self.channel).await?;
            Ok(Outcome::CaughtUp)
        })
    }
}

/// Refreshes joined teams and channels, dropping ones the user left, then runs the first delta
/// round of up to [`FIRST_ROUNDS_PER_PASS`] channels that have none yet.
pub(crate) async fn sync_roster(ctx: &Ctx) -> Result<Outcome> {
    let teams: Vec<Team> =
        ctx.graph.list_joined_teams().await.map_err(graph_err)?.into_iter().take(MAX_TEAMS).collect();
    let mut channels: Vec<(TeamId, Vec<Channel>)> = Vec::new();
    for t in &teams {
        match ctx.graph.list_channels(&t.id, true).await {
            Ok(list) => channels.push((t.id.clone(), list)),
            Err(e) => {
                // One unreadable team must not hide the others; keep its cached channels.
                tracing::warn!(name: "sync.team_channels_failed", kind = ?e.kind(), "listing a team's channels failed");
                if matches!(e.kind(), quorum_graph::ErrorKind::Throttled | quorum_graph::ErrorKind::Unauthorized) {
                    return Err(graph_err(e));
                }
            }
        }
    }
    let listed: HashSet<TeamId> = teams.iter().map(|t| t.id.clone()).collect();
    let needs_first = ctx
        .store
        .write(move |tx| {
            let gone: Vec<TeamId> =
                tx.read().teams()?.items.into_iter().map(|t| t.id).filter(|id| !listed.contains(id)).collect();
            tx.delete_teams(&gone)?;
            tx.upsert_teams(&teams)?;
            let mut needs = Vec::new();
            for (team, list) in &channels {
                let keep: HashSet<&ChannelId> = list.iter().map(|c| &c.id).collect();
                let stale: Vec<ChannelId> =
                    tx.read().channels(team)?.items.into_iter().map(|c| c.id).filter(|id| !keep.contains(id)).collect();
                tx.delete_channels(&stale)?;
                tx.upsert_channels(team, list)?;
                for c in list {
                    if tx.read().delta_state(&channel_key(team, &c.id))?.is_none() {
                        needs.push((team.clone(), c.id.clone()));
                    }
                }
            }
            Ok(needs)
        })
        .await?;
    for (team, channel) in needs_first.iter().take(FIRST_ROUNDS_PER_PASS) {
        if let Err(e) = sync_channel(ctx, team, channel).await {
            tracing::warn!(name: "sync.channel_first_round_failed", kind = ?e.kind(), "first channel delta failed");
        }
    }
    Ok(if needs_first.len() > FIRST_ROUNDS_PER_PASS { Outcome::More } else { Outcome::CaughtUp })
}

/// One channel delta round: posts and replies changed since the stored delta link. An expired
/// link restarts the round from scratch (messages are merged by id). Returns changes applied.
///
/// # Errors
/// Graph or store failures.
pub async fn sync_channel(ctx: &Ctx, team: &TeamId, channel: &ChannelId) -> Result<usize> {
    let key = channel_key(team, channel);
    let state = ctx.store.delta_state(key.clone()).await?;
    let link = state.as_ref().and_then(|s| s.delta_link.clone());
    let round = match ctx.graph.channel_messages_delta_round(team, channel, link.as_deref(), DELTA_MAX_PAGES).await {
        Ok(r) => r,
        Err(e) if e.kind() == quorum_graph::ErrorKind::DeltaExpired && link.is_some() => {
            tracing::info!(name: "sync.channel_delta_expired", "channel delta link expired; resyncing");
            ctx.graph.channel_messages_delta_round(team, channel, None, DELTA_MAX_PAGES).await.map_err(graph_err)?
        }
        Err(e) => return Err(graph_err(e)),
    };
    let container = MessageContainer::Channel { team_id: team.clone(), channel_id: channel.clone() };
    let mut changed = Vec::new();
    let mut removed: Vec<ChatMessageId> = Vec::new();
    for c in round.items {
        match c {
            ChannelMessageChange::Changed(m) => {
                let mut m = *m;
                if m.body.content.len() > quorum_store::MAX_BODY_LEN {
                    m.body.content = quorum_model::Sensitive::new("<p>(This message is too large to show.)</p>");
                }
                changed.push(m);
            }
            ChannelMessageChange::Removed(id) => removed.push(id),
        }
    }
    let n = changed.len().saturating_add(removed.len());
    let mut st = state.unwrap_or_else(|| DeltaState::new(key));
    st.delta_link = Some(round.delta_link);
    st.next_link = None;
    st.backfill_complete = true;
    ctx.store
        .write(move |tx| {
            tx.upsert_chat_messages(&changed)?;
            tx.delete_chat_messages(&container, &removed)?;
            tx.set_delta_state(&st)?;
            Ok(())
        })
        .await?;
    Ok(n)
}
