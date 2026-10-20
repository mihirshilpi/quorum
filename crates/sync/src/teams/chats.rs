//! The chat list and per-chat message sync.

use std::collections::{BTreeSet, HashSet};

use futures::future::BoxFuture;
use jiff::{SignedDuration, Timestamp};
use quorum_model::{Chat, ChatId, ChatMessage, ChatMessageType, Sensitive};
use quorum_store::DeltaState;

use super::{CHATS_KEY, OPEN_CHANNEL_SETTING, OpenChannel, SharedFocus, chat_key, graph_err, older_request_key};
use crate::{Cadence, Ctx, ErrorKind, Outcome, Resource, Result};

/// Days of history hydrated when a chat is first synced; older pages load on demand.
pub const HYDRATE_DAYS: i64 = 30;
/// Page size for chat lists and messages.
const PAGE: u32 = 50;
/// Chat list pages read on the first (full) pass.
const MAX_LIST_PAGES: usize = 40;
/// Message pages read while hydrating one chat.
const HYDRATE_MAX_PAGES: usize = 10;
/// Pages of an incremental fetch.
const INCREMENTAL_MAX_PAGES: usize = 20;
/// Chats whose messages are fetched in one chat list pass; the rest wait for the next pass.
pub(crate) const MAX_FETCH_PER_PASS: usize = 6;
/// Setting holding chat ids that changed but were not fetched yet.
pub(crate) const PENDING_SETTING: &str = "chat.pending_fetch";
/// Bodies larger than this are replaced by a placeholder (the store rejects them).
const MAX_BODY: usize = quorum_store::MAX_BODY_LEN;

pub(crate) struct ChatList {
    pub(crate) focus: SharedFocus,
}

pub(crate) struct OpenChat {
    pub(crate) chat: ChatId,
}

impl Resource for ChatList {
    fn key(&self) -> String {
        CHATS_KEY.to_owned()
    }

    fn area(&self) -> &'static str {
        "chat"
    }

    fn cadence(&self) -> Cadence {
        Cadence::secs(5, 30, 120)
    }

    fn run<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(async move {
            // Keep the open channel's cadence current (written by `channel.open`).
            let open: Option<OpenChannel> = ctx.store.setting(OPEN_CHANNEL_SETTING.to_owned()).await.ok().flatten();
            if let Ok(mut f) = self.focus.write() {
                f.channel = open;
            }
            sync_chat_list(ctx).await
        })
    }
}

impl Resource for OpenChat {
    fn key(&self) -> String {
        chat_key(&self.chat)
    }

    fn area(&self) -> &'static str {
        "chat"
    }

    fn cadence(&self) -> Cadence {
        Cadence::secs(3, 30, 120)
    }

    fn run<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(async move {
            sync_chat(ctx, &self.chat).await?;
            Ok(Outcome::CaughtUp)
        })
    }
}

fn preview_key(c: &Chat) -> Option<(String, Option<Timestamp>)> {
    c.last_message_preview.as_ref().map(|p| (p.id.as_str().to_owned(), p.created_date_time))
}

fn activity(c: &Chat) -> Option<Timestamp> {
    c.last_message_preview.as_ref().and_then(|p| p.created_date_time).or(c.last_updated_date_time)
}

/// One chat list pass: refresh the list, then fetch messages of chats whose preview changed.
pub(crate) async fn sync_chat_list(ctx: &Ctx) -> Result<Outcome> {
    let state = ctx.store.delta_state(CHATS_KEY.to_owned()).await?;
    let full = state.as_ref().is_none_or(|s| !s.backfill_complete);
    let mut page = ctx.graph.list_chats(PAGE).await.map_err(graph_err)?;
    let mut fetched: Vec<Chat> = std::mem::take(&mut page.items);
    let mut pages = 1usize;
    let mut seen_links = HashSet::new();
    while full && pages < MAX_LIST_PAGES {
        let Some(next) = page.next_link.take() else { break };
        if !seen_links.insert(next.clone()) {
            break;
        }
        page = ctx.graph.list_chats_next(&next).await.map_err(graph_err)?;
        fetched.append(&mut page.items);
        pages = pages.saturating_add(1);
    }

    let cutoff = Timestamp::now().checked_sub(SignedDuration::from_hours(24 * HYDRATE_DAYS)).ok();
    let list = fetched.clone();
    let (changed, mut pending) = ctx
        .store
        .read(move |r| {
            let mut changed = Vec::new();
            for c in &list {
                let prev = r.chat(&c.id).ok().flatten();
                let moved = prev.as_ref().is_none_or(|p| preview_key(p) != preview_key(c));
                if !moved || c.last_message_preview.is_none() {
                    continue;
                }
                let hydrated = r.delta_state(&chat_key(&c.id))?.is_some();
                let recent = match (activity(c), cutoff) {
                    (Some(a), Some(cut)) => a >= cut,
                    _ => false,
                };
                if hydrated || recent {
                    changed.push(c.id.clone());
                }
            }
            let pending: Vec<String> = r.setting(PENDING_SETTING)?.unwrap_or_default();
            Ok((changed, pending))
        })
        .await?;

    // Queue: previously pending first (oldest work), then newly changed, without duplicates.
    let mut queue: Vec<ChatId> = Vec::new();
    let mut dedupe = BTreeSet::new();
    for id in pending.drain(..).filter_map(|s| ChatId::new(s).ok()).chain(changed) {
        if dedupe.insert(id.clone()) {
            queue.push(id);
        }
    }
    let now_batch: Vec<ChatId> = queue.iter().take(MAX_FETCH_PER_PASS).cloned().collect();
    let mut rest: Vec<String> = queue.iter().skip(MAX_FETCH_PER_PASS).map(|c| c.as_str().to_owned()).collect();

    ctx.store
        .write(move |tx| {
            tx.upsert_chats(&fetched)?;
            let mut st = DeltaState::new(CHATS_KEY);
            st.backfill_complete = true;
            tx.set_delta_state(&st)?;
            Ok(())
        })
        .await?;

    let mut throttled = None;
    for chat in &now_batch {
        match sync_chat(ctx, chat).await {
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::Permanent) => {
                tracing::warn!(name: "sync.chat_skipped", kind = ?e.kind(), "chat messages could not be fetched");
            }
            Err(e) => {
                rest.push(chat.as_str().to_owned());
                if matches!(e.kind(), ErrorKind::Throttled | ErrorKind::Auth) && throttled.is_none() {
                    throttled = Some(e);
                }
            }
        }
    }
    let more = !rest.is_empty();
    ctx.store.set_setting(PENDING_SETTING.to_owned(), rest).await?;
    if let Some(e) = throttled {
        return Err(e);
    }
    Ok(if more { Outcome::More } else { Outcome::CaughtUp })
}

fn cap_body(mut m: ChatMessage) -> ChatMessage {
    if m.body.content.len() > MAX_BODY {
        m.body.content = Sensitive::new("<p>(This message is too large to show.)</p>");
    }
    m
}

fn keep(m: &ChatMessage) -> bool {
    m.message_type != ChatMessageType::Typing
}

fn newest_change(msgs: &[ChatMessage]) -> Option<Timestamp> {
    msgs.iter().filter_map(|m| m.last_modified_date_time.or(m.created_date_time)).max()
}

fn oldest_created(msgs: &[ChatMessage]) -> Option<Timestamp> {
    msgs.iter().filter_map(|m| m.created_date_time).min()
}

/// Brings one chat's cached messages up to date: hydrates the last [`HYDRATE_DAYS`] days the
/// first time, afterwards fetches only messages modified after the stored watermark, and loads
/// one older page when [`older_request_key`] is set. Returns the number of messages written.
///
/// # Errors
/// Graph or store failures.
pub async fn sync_chat(ctx: &Ctx, chat: &ChatId) -> Result<usize> {
    let key = chat_key(chat);
    let older_key = older_request_key(chat);
    let (state, older): (Option<DeltaState>, bool) = {
        let (k, ok) = (key.clone(), older_key.clone());
        ctx.store.read(move |r| Ok((r.delta_state(&k)?, r.setting::<bool>(&ok)?.unwrap_or(false)))).await?
    };
    let watermark = state.as_ref().and_then(|s| s.delta_link.as_deref()).and_then(|w| w.parse::<Timestamp>().ok());
    let mut st = state.unwrap_or_else(|| DeltaState::new(key.clone()));
    let mut msgs: Vec<ChatMessage> = Vec::new();
    match watermark {
        Some(w) => {
            msgs = ctx.graph.chat_messages_since(chat, w, INCREMENTAL_MAX_PAGES).await.map_err(graph_err)?;
        }
        None => {
            let cutoff = Timestamp::now().checked_sub(SignedDuration::from_hours(24 * HYDRATE_DAYS)).ok();
            let mut page = ctx.graph.list_chat_messages(chat, PAGE, None).await.map_err(graph_err)?;
            msgs.append(&mut page.items);
            let mut pages = 1usize;
            let mut seen = HashSet::new();
            loop {
                let reached_cutoff = matches!((oldest_created(&msgs), cutoff), (Some(o), Some(c)) if o < c);
                if reached_cutoff || pages >= HYDRATE_MAX_PAGES {
                    break;
                }
                let Some(next) = page.next_link.take() else { break };
                if !seen.insert(next.clone()) {
                    break;
                }
                page = ctx.graph.list_chat_messages_next(chat, &next).await.map_err(graph_err)?;
                msgs.append(&mut page.items);
                pages = pages.saturating_add(1);
            }
            st.backfill_cursor = page.next_link.take();
            st.backfill_complete = st.backfill_cursor.is_none();
        }
    }
    if older && let Some(cursor) = st.backfill_cursor.clone() {
        let mut page = ctx.graph.list_chat_messages_next(chat, &cursor).await.map_err(graph_err)?;
        msgs.append(&mut page.items);
        st.backfill_cursor = page.next_link.take().filter(|n| n != &cursor);
        st.backfill_complete = st.backfill_cursor.is_none();
    }
    let newest = newest_change(&msgs);
    let mark = match (watermark, newest) {
        (Some(w), Some(n)) => Some(w.max(n)),
        (a, b) => a.or(b).or_else(|| Some(Timestamp::now())),
    };
    st.delta_link = mark.map(|t| t.to_string());
    let msgs: Vec<ChatMessage> = msgs.into_iter().filter(keep).map(cap_body).collect();
    let n = msgs.len();
    ctx.store
        .write(move |tx| {
            tx.upsert_chat_messages(&msgs)?;
            tx.set_delta_state(&st)?;
            if older {
                tx.delete_setting(&older_key)?;
            }
            Ok(())
        })
        .await?;
    Ok(n)
}
