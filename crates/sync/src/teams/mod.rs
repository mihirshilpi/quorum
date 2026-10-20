//! Teams chats and channels (architecture §7 chats/channels rows, §8 Teams).
//!
//! Resources:
//!
//! | Key | What | Cadence (focused / background / idle) |
//! |---|---|---|
//! | `chats` | `/me/chats` ordered by `lastMessagePreview`; chats whose preview changed get their messages fetched incrementally (`lastModifiedDateTime gt` the chat's watermark), never-seen chats active in the last 30 days are hydrated | 5 s / 30 s / 2 min |
//! | `chat:<id>` | the open chat (from the [`OPEN_CHAT_SETTING`] setting): incremental fetch, plus older pages on request ([`older_request_key`]) | 3 s / 30 s / 2 min |
//! | `teams` | joined teams and their channels (including shared ones); first delta round for new channels | 5 min / 10 min / 30 min |
//! | `channel:<team>/<channel>` | channel message delta with replies | open channel 5 s, others 2 min |
//!
//! Per-chat progress lives in the delta state of `chat:<id>`: `delta_link` holds the watermark
//! (RFC 3339 `lastModifiedDateTime` of the newest change seen), `backfill_cursor` the `nextLink`
//! of older messages, `backfill_complete` whether history is fully cached.
//!
//! Outbox kinds are listed in [`outbox::KINDS`]; their payloads are in [`payload`].
//! Inline images (`hostedContents`) are not downloaded here: chat HTML points at
//! `quorum-hosted://localhost/<url>`, which the desktop shell fetches on demand.

mod channels;
mod chats;
pub mod outbox;
pub mod payload;

use std::sync::{Arc, RwLock};

use futures::future::BoxFuture;
use quorum_model::{ChannelId, ChatId, TeamId};
use serde::{Deserialize, Serialize};

use crate::{Ctx, Module, OutboxHandler, Resource, Result};

pub use chats::{HYDRATE_DAYS, sync_chat};

/// Key of the chat list resource.
pub const CHATS_KEY: &str = "chats";
/// Key of the joined teams / channels roster resource.
pub const TEAMS_KEY: &str = "teams";
/// Setting (string chat id) naming the chat on screen; written by `chat.open`.
pub const OPEN_CHAT_SETTING: &str = "chat.open";
/// Setting ([`OpenChannel`]) naming the channel on screen; written by `channel.open`.
pub const OPEN_CHANNEL_SETTING: &str = "channel.open";
/// UI pref (list of chat ids) of chats pinned to the top of the chat list (local only: Graph has
/// no chat-pinning API).
pub const PINNED_CHATS_PREF: &str = "chat.pinned";
/// Most channels kept in sync as their own resources.
pub const MAX_CHANNEL_RESOURCES: usize = 200;

/// The resource (and outbox ordering) key of a chat: `chat:<id>`.
#[must_use]
pub fn chat_key(chat: &ChatId) -> String {
    format!("chat:{}", chat.as_str())
}

/// The resource key of a channel: `channel:<team>/<channel>`.
#[must_use]
pub fn channel_key(team: &TeamId, channel: &ChannelId) -> String {
    format!("channel:{}/{}", team.as_str(), channel.as_str())
}

/// Setting key (bool) asking the `chat:<id>` resource to fetch one older page.
#[must_use]
pub fn older_request_key(chat: &ChatId) -> String {
    format!("chat.older:{}", chat.as_str())
}

/// The channel on screen.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenChannel {
    /// The team.
    pub team: TeamId,
    /// The channel.
    pub channel: ChannelId,
}

/// What is on screen, shared by the resources so cadences follow it.
#[derive(Debug, Default)]
struct Focus {
    channel: Option<OpenChannel>,
}

type SharedFocus = Arc<RwLock<Focus>>;

fn open_channel_is(focus: &SharedFocus, team: &TeamId, channel: &ChannelId) -> bool {
    focus.read().ok().and_then(|f| f.channel.clone()).is_some_and(|c| &c.team == team && &c.channel == channel)
}

/// The Teams sync module: chats, channels and their outbox handlers.
#[derive(Debug, Default, Clone)]
pub struct TeamsModule {
    focus: SharedFocus,
}

impl TeamsModule {
    /// A new module.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The chat list resource (for tests and tools).
    #[must_use]
    pub fn chat_list(&self) -> Arc<dyn Resource> {
        Arc::new(chats::ChatList { focus: Arc::clone(&self.focus) })
    }

    /// The `chat:<id>` resource.
    #[must_use]
    pub fn chat(&self, chat: ChatId) -> Arc<dyn Resource> {
        Arc::new(chats::OpenChat { chat })
    }

    /// The teams roster resource.
    #[must_use]
    pub fn roster(&self) -> Arc<dyn Resource> {
        Arc::new(channels::Roster)
    }

    /// The `channel:<team>/<channel>` resource.
    #[must_use]
    pub fn channel(&self, team: TeamId, channel: ChannelId) -> Arc<dyn Resource> {
        Arc::new(channels::ChannelResource { team, channel, focus: Arc::clone(&self.focus) })
    }

    async fn discover_all(&self, ctx: &Ctx) -> Result<Vec<Arc<dyn Resource>>> {
        let mut out = vec![self.chat_list(), self.roster()];
        let open: Option<String> = ctx.store.setting(OPEN_CHAT_SETTING.to_owned()).await?;
        if let Some(chat) = open.and_then(|s| ChatId::new(s).ok()) {
            out.push(self.chat(chat));
        }
        let open_channel: Option<OpenChannel> = ctx.store.setting(OPEN_CHANNEL_SETTING.to_owned()).await.ok().flatten();
        if let Ok(mut f) = self.focus.write() {
            f.channel = open_channel;
        }
        let pairs = ctx
            .store
            .read(|r| {
                let mut pairs = Vec::new();
                for t in r.teams()?.items {
                    for c in r.channels(&t.id)?.items {
                        pairs.push((t.id.clone(), c.id));
                    }
                }
                Ok(pairs)
            })
            .await?;
        for (team, channel) in pairs.into_iter().take(MAX_CHANNEL_RESOURCES) {
            out.push(self.channel(team, channel));
        }
        Ok(out)
    }
}

impl Module for TeamsModule {
    fn name(&self) -> &'static str {
        "teams"
    }

    fn discover<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Vec<Arc<dyn Resource>>>> {
        Box::pin(self.discover_all(ctx))
    }

    fn handlers(&self) -> Vec<Arc<dyn OutboxHandler>> {
        outbox::handlers()
    }
}

/// Maps a Graph error, treating request validation failures as permanent (retrying the same
/// payload cannot fix them).
pub(crate) fn graph_err(e: quorum_graph::Error) -> crate::Error {
    use quorum_graph::ErrorKind as G;
    let permanent = matches!(e.kind(), G::Config | G::Malformed | G::TooLarge);
    let err = crate::Error::from(e);
    if permanent { crate::Error::new(crate::ErrorKind::Permanent, "request rejected").with_source(err) } else { err }
}
