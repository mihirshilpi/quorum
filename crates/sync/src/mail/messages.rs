//! One folder's messages: first page, delta, backfill and body prefetch.

use std::collections::HashSet;

use futures::future::BoxFuture;
use jiff::{SignedDuration, Timestamp};
use quorum_graph::DeltaOutcome;
use quorum_graph::api::mail::{
    Change, DeltaOptions, FolderRef, ListOptions, Mailbox, delta_link_request, messages_delta_request,
};
use quorum_model::{MailFolderId, Message, MessageId, WellKnownFolder};
use quorum_store::{DeltaState, MessageQuery};
use serde::{Deserialize, Serialize};

use super::{DEFAULT_PREFETCH_DAYS, DEFAULT_SYNC_WINDOW_DAYS, PREFETCH_DAYS_SETTING, SYNC_WINDOW_SETTING, folder_key};
use crate::{Cadence, Ctx, Outcome, Resource, Result};

/// Headers stored by a folder's very first pass (newest first, so it is usable at once).
pub const HEAD_PAGE: u32 = 50;
/// Delta page size (`odata.maxpagesize`).
pub const DELTA_PAGE: u32 = 100;
/// Delta pages applied per pass before yielding (`Outcome::More`).
pub const PAGES_PER_PASS: usize = 10;
/// Pages a resync after an expired delta token may take in one pass.
pub const MAX_RESYNC_PAGES: usize = 200;
/// Older headers fetched per backfill page.
pub const BACKFILL_PAGE: u32 = 100;
/// Bodies prefetched per pass.
pub const PREFETCH_PER_PASS: u32 = 5;
/// Local pages examined when reconciling after a resync.
const MAX_RECONCILE_PAGES: usize = 100;
/// The longest window accepted from settings (about ten years).
const MAX_WINDOW_DAYS: u32 = 3650;

/// The delta window and backfill position of a folder, stored as the opaque
/// [`DeltaState::backfill_cursor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    /// The folder delta covers messages received at or after this instant.
    pub since: Timestamp,
    /// The backfill has fetched everything received at or after this instant.
    pub before: Timestamp,
}

impl Window {
    /// Parses a stored cursor; `None` if absent or malformed.
    #[must_use]
    pub fn parse(cursor: Option<&str>) -> Option<Self> {
        cursor.and_then(|c| serde_json::from_str(c).ok())
    }

    fn encode(self) -> Option<String> {
        serde_json::to_string(&self).ok()
    }
}

/// The messages of one folder (`mail.folder:<id>`).
#[derive(Debug, Clone)]
pub struct FolderMessages {
    folder: MailFolderId,
    role: Option<WellKnownFolder>,
}

impl FolderMessages {
    /// The resource for `folder` (`role` = its well-known name, which sets the cadence).
    #[must_use]
    pub fn new(folder: MailFolderId, role: Option<WellKnownFolder>) -> Self {
        Self { folder, role }
    }

    /// The folder.
    #[must_use]
    pub fn folder(&self) -> &MailFolderId {
        &self.folder
    }

    fn folder_ref(&self) -> FolderRef {
        FolderRef::Id(self.folder.clone())
    }
}

impl Resource for FolderMessages {
    fn key(&self) -> String {
        folder_key(&self.folder)
    }

    fn area(&self) -> &'static str {
        "mail"
    }

    fn cadence(&self) -> Cadence {
        match self.role {
            Some(WellKnownFolder::Inbox) => Cadence::secs(15, 60, 300),
            _ => Cadence::secs(120, 300, 900),
        }
    }

    fn run<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(self.pass(ctx))
    }
}

async fn days_setting(ctx: &Ctx, key: &str, default: u32) -> Result<u32> {
    Ok(ctx.store.setting::<u32>(key.to_owned()).await?.unwrap_or(default).min(MAX_WINDOW_DAYS))
}

fn days_ago(days: u32) -> Timestamp {
    let secs = i64::from(days).saturating_mul(86_400);
    Timestamp::now().checked_sub(SignedDuration::from_secs(secs)).unwrap_or(Timestamp::UNIX_EPOCH)
}

impl FolderMessages {
    async fn pass(&self, ctx: &Ctx) -> Result<Outcome> {
        let key = self.key();
        let mut state = ctx.store.delta_state(key.clone()).await?.unwrap_or_else(|| DeltaState::new(key.clone()));
        let window = match Window::parse(state.backfill_cursor.as_deref()) {
            Some(w) => w,
            None => {
                // First pass: the newest headers, so the folder is usable within seconds.
                let since = days_ago(days_setting(ctx, SYNC_WINDOW_SETTING, DEFAULT_SYNC_WINDOW_DAYS).await?);
                let window = Window { since, before: since };
                state.backfill_cursor = window.encode();
                let opts = ListOptions { top: Some(HEAD_PAGE), ..ListOptions::default() };
                let page = ctx.graph.list_messages(&Mailbox::Me, &self.folder_ref(), &opts).await?;
                let items = self.own(page.items);
                let st = state.clone();
                ctx.store
                    .write(move |tx| {
                        tx.upsert_messages(&items)?;
                        tx.set_delta_state(&st)
                    })
                    .await?;
                return Ok(Outcome::More);
            }
        };
        let caught_up = match self.delta(ctx, &mut state, window, PAGES_PER_PASS, None).await {
            Ok(done) => done,
            Err(e) if super::delta_expired(&e) => {
                tracing::info!(name: "sync.mail.folder_resync", "message delta expired; resyncing one folder");
                self.resync(ctx, &mut state, window).await?
            }
            Err(e) => return Err(e),
        };
        if !caught_up {
            return Ok(Outcome::More);
        }
        let mut more = false;
        if !state.backfill_complete {
            more |= self.backfill(ctx, &mut state, window).await?;
        }
        more |= self.prefetch(ctx).await?;
        Ok(if more { Outcome::More } else { Outcome::CaughtUp })
    }

    /// Stamps the folder on messages that arrive without `parentFolderId`.
    fn own(&self, mut items: Vec<Message>) -> Vec<Message> {
        for m in &mut items {
            if m.parent_folder_id.is_none() {
                m.parent_folder_id = Some(self.folder.clone());
            }
        }
        items
    }

    /// Runs up to `max_pages` delta pages. Returns whether the round finished (delta link stored).
    async fn delta(
        &self,
        ctx: &Ctx,
        state: &mut DeltaState,
        window: Window,
        max_pages: usize,
        mut seen: Option<&mut HashSet<MessageId>>,
    ) -> Result<bool> {
        let mut req = match state.next_link.as_ref().or(state.delta_link.as_ref()) {
            Some(link) => delta_link_request(&Mailbox::Me, link.clone(), Some(DELTA_PAGE)),
            None => messages_delta_request(
                &Mailbox::Me,
                &self.folder_ref(),
                &DeltaOptions { page_size: Some(DELTA_PAGE), received_since: Some(window.since) },
            )
            .map_err(crate::Error::from)?,
        };
        for _ in 0..max_pages.max(1) {
            let page = ctx.graph.messages_delta_page(req.clone()).await?;
            let mut upserts = Vec::new();
            let mut removed = Vec::new();
            for change in page.items {
                match change {
                    Change::Upsert(m) => upserts.push(m),
                    Change::Removed(id) => removed.push(id),
                }
            }
            let upserts = self.own(upserts);
            if let Some(seen) = seen.as_deref_mut() {
                seen.extend(upserts.iter().map(|m| m.id.clone()));
            }
            let done = match page.outcome {
                DeltaOutcome::Done(link) => {
                    state.delta_link = Some(link);
                    state.next_link = None;
                    true
                }
                DeltaOutcome::Next(link) => {
                    req = delta_link_request(&Mailbox::Me, link.clone(), Some(DELTA_PAGE));
                    state.next_link = Some(link);
                    false
                }
            };
            let st = state.clone();
            let folder = self.folder.clone();
            ctx.store
                .write(move |tx| {
                    tx.upsert_messages(&upserts)?;
                    // A message that moved keeps its immutable id: the source folder reports it
                    // `@removed`, but if the destination's delta already stored it there, keep it.
                    let mut gone = Vec::new();
                    for id in removed {
                        match tx.read().message(&id)? {
                            Some(m) if m.parent_folder_id.as_ref().is_some_and(|f| *f != folder) => {}
                            _ => gone.push(id),
                        }
                    }
                    tx.delete_messages(&gone)?;
                    tx.set_delta_state(&st)
                })
                .await?;
            if done {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Resync after an expired token: a fresh round merged by id (cached bodies are kept), then
    /// drop local messages inside the window the server no longer has.
    async fn resync(&self, ctx: &Ctx, state: &mut DeltaState, window: Window) -> Result<bool> {
        state.delta_link = None;
        state.next_link = None;
        let st = state.clone();
        ctx.store.write(move |tx| tx.set_delta_state(&st)).await?;
        let mut seen = HashSet::new();
        let done = self.delta(ctx, state, window, MAX_RESYNC_PAGES, Some(&mut seen)).await?;
        if done {
            let stale = self.stale_local(ctx, &seen, window.since).await?;
            if !stale.is_empty() {
                ctx.store.write(move |tx| tx.delete_messages(&stale)).await?;
            }
        }
        Ok(done)
    }

    async fn stale_local(&self, ctx: &Ctx, seen: &HashSet<MessageId>, since: Timestamp) -> Result<Vec<MessageId>> {
        let mut stale = Vec::new();
        let mut q = MessageQuery::folder(self.folder.clone());
        q.limit = 500;
        for _ in 0..MAX_RECONCILE_PAGES {
            let page = ctx.store.list_messages(q.clone()).await?;
            let mut past_window = false;
            for m in &page.items {
                if m.received_date_time.is_some_and(|r| r < since) {
                    past_window = true;
                    break;
                }
                if !seen.contains(&m.id) && !m.is_draft {
                    stale.push(m.id.clone());
                }
            }
            match page.next {
                Some(next) if !past_window => q.after = Some(next),
                _ => break,
            }
        }
        Ok(stale)
    }

    /// One page of older headers. Returns whether more remain.
    async fn backfill(&self, ctx: &Ctx, state: &mut DeltaState, mut window: Window) -> Result<bool> {
        let bf =
            ctx.graph.backfill_messages(&Mailbox::Me, &self.folder_ref(), window.before, Some(BACKFILL_PAGE)).await?;
        let items = self.own(bf.page.items);
        let complete = match bf.older_than {
            Some(older) if older < window.before && !items.is_empty() => {
                window.before = older;
                false
            }
            _ => true,
        };
        state.backfill_cursor = window.encode();
        state.backfill_complete = complete;
        let st = state.clone();
        ctx.store
            .write(move |tx| {
                tx.upsert_messages(&items)?;
                tx.set_delta_state(&st)
            })
            .await?;
        Ok(!complete)
    }

    /// Prefetches a few bodies of recent messages. Returns whether more remain.
    async fn prefetch(&self, ctx: &Ctx) -> Result<bool> {
        let days = days_setting(ctx, PREFETCH_DAYS_SETTING, DEFAULT_PREFETCH_DAYS).await?;
        if days == 0 {
            return Ok(false);
        }
        let ids = ctx.store.messages_without_body(self.folder.clone(), days_ago(days), PREFETCH_PER_PASS).await?;
        let full = u32::try_from(ids.len()).unwrap_or(u32::MAX) >= PREFETCH_PER_PASS;
        let mut fetched = 0usize;
        for id in ids {
            match super::fetch_body(ctx, &id).await {
                Ok(()) => fetched = fetched.saturating_add(1),
                // A message deleted meanwhile, or one Graph refuses: skip it.
                Err(e) if e.kind() == crate::ErrorKind::Permanent => {}
                Err(e) => return Err(e),
            }
        }
        Ok(full && fetched > 0)
    }
}
