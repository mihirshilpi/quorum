//! Mail sync (architecture §7 "Download once, then only changes", §8 Mail).
//!
//! Resources:
//! - `mail.folders` ([`FoldersResource`]): the folder tree from `/me/mailFolders/delta`
//!   (10 min), the well-known folder roles (`inbox`, `archive`, ... resolved once by name, since
//!   Graph v1.0 does not return `wellKnownName`), mailbox settings and master categories.
//! - `mail.folder:<id>` ([`FolderMessages`]): one per folder. The first pass stores the newest
//!   page of headers so the folder is usable at once; later passes run the folder's message
//!   delta (bounded to a recent window), then backfill older headers newest to oldest
//!   ([`quorum_store::DeltaState::backfill_cursor`]), then prefetch bodies for the last N days
//!   (setting [`PREFETCH_DAYS_SETTING`], default 30). Inbox runs at 15 s / 60 s / 5 min,
//!   other folders at 2 / 5 / 15 min. A 410 / `syncStateNotFound` resyncs that one folder,
//!   merging by id and keeping cached bodies.
//!
//! Outbox handlers ([`handlers`]): `mail.update` (read, flag, categories, focused),
//! `mail.move`, `mail.delete_permanent`, `mail.send`, `mail.draft_sync`, `mail.draft_delete`,
//! `mail.auto_reply`. The payload types live in [`payload`] so the engine's commands and the
//! handlers agree on them.
//!
//! Bodies are stored as Graph sends them (encrypted by the store); the engine sanitises at read
//! time, so a sanitiser policy change applies to mail already cached.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use quorum_model::{GraphEnum, MailFolder, MailFolderId, MessageId, WellKnownFolder};

use crate::{Ctx, Module, OutboxHandler, Resource, Result};

mod body;
pub mod compose;
mod folders;
mod handlers;
mod messages;
pub mod payload;

pub use body::{
    MAX_DOWNLOAD_BYTES, MAX_INLINE_ATTACHMENT_BYTES, MAX_INLINE_ATTACHMENTS, attachment_blob_key, download_attachment,
    fetch_body,
};
pub use compose::{Compose, ComposeAttachment, ComposeMode, merge_quote};
pub use folders::FoldersResource;
pub use handlers::{
    AutoReplyHandler, DeletePermanentHandler, DraftDeleteHandler, DraftSyncHandler, MoveHandler, SendHandler,
    UpdateHandler,
};
pub use messages::{FolderMessages, Window};

/// Resource key of the folder tree.
pub const FOLDERS_RESOURCE: &str = "mail.folders";
/// Settings key: well-known folder name (`inbox`, `archive`, ...) → folder id.
pub const WELL_KNOWN_SETTING: &str = "mail.well_known";
/// Settings key: prefetch bodies for messages received in the last N days (default 30).
pub const PREFETCH_DAYS_SETTING: &str = "mail.prefetch_days";
/// Default for [`PREFETCH_DAYS_SETTING`].
pub const DEFAULT_PREFETCH_DAYS: u32 = 30;
/// Settings key: the delta window of a folder's first sync in days (default 30); older
/// headers come from the backfill.
pub const SYNC_WINDOW_SETTING: &str = "mail.sync_window_days";
/// Default for [`SYNC_WINDOW_SETTING`].
pub const DEFAULT_SYNC_WINDOW_DAYS: u32 = 30;
/// Settings key: the mailbox settings last read from Graph ([`quorum_graph::api::mail::MailboxSettings`]).
pub const MAILBOX_SETTINGS_SETTING: &str = "mail.mailbox_settings";
/// Settings key: the master category list (`Vec<Category>`).
pub const CATEGORIES_SETTING: &str = "mail.categories";
/// The well-known folders whose ids are resolved by name.
pub const RESOLVED_FOLDERS: &[WellKnownFolder] = &[
    WellKnownFolder::Inbox,
    WellKnownFolder::Drafts,
    WellKnownFolder::SentItems,
    WellKnownFolder::DeletedItems,
    WellKnownFolder::Archive,
    WellKnownFolder::JunkEmail,
    WellKnownFolder::Outbox,
];
/// The most folders synced as separate resources.
pub const MAX_FOLDERS: usize = 500;

/// The resource key of a folder's messages.
#[must_use]
pub fn folder_key(id: &MailFolderId) -> String {
    format!("mail.folder:{}", id.as_str())
}

/// The outbox ordering key for writes to one message.
#[must_use]
pub fn message_key(id: &MessageId) -> String {
    format!("mail.message:{}", id.as_str())
}

/// The outbox ordering key for writes to one local draft.
#[must_use]
pub fn draft_key(draft_id: &str) -> String {
    format!("mail.draft:{draft_id}")
}

/// The well-known folder map (`inbox` → id) from the store; empty before the first sync.
///
/// # Errors
/// Store failures.
pub async fn well_known(store: &quorum_store::Store) -> Result<BTreeMap<String, MailFolderId>> {
    Ok(store.setting::<BTreeMap<String, MailFolderId>>(WELL_KNOWN_SETTING.to_owned()).await?.unwrap_or_default())
}

/// Whether a folder is synced as its own resource: not hidden (unless it has a role we use)
/// and not a system folder that holds no mail we show.
#[must_use]
pub fn is_synced_folder(f: &MailFolder) -> bool {
    match &f.well_known_name {
        Some(w) => RESOLVED_FOLDERS.contains(w),
        None => !f.is_hidden,
    }
}

/// The mail sync module.
#[derive(Debug, Clone, Default)]
pub struct MailModule;

impl MailModule {
    /// The module.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Module for MailModule {
    fn name(&self) -> &'static str {
        "mail"
    }

    fn discover<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Vec<Arc<dyn Resource>>>> {
        Box::pin(async move {
            let mut out: Vec<Arc<dyn Resource>> = vec![Arc::new(FoldersResource::new())];
            let mut folders = ctx.store.mail_folders().await?;
            if folders.items.is_empty() {
                // First start: learn the folder tree now, so every folder syncs from the first
                // round instead of waiting for the next rediscovery.
                if let Err(e) = FoldersResource::new().run(ctx).await {
                    tracing::info!(name: "sync.mail.first_folders_failed", kind = ?e.kind(), "first folder sync failed");
                }
                folders = ctx.store.mail_folders().await?;
            }
            let roles = well_known(&ctx.store).await?;
            for f in folders.items.into_iter().filter(is_synced_folder).take(MAX_FOLDERS) {
                let role = roles.iter().find(|(_, id)| **id == f.id).map(|(name, _)| WellKnownFolder::from_graph(name));
                out.push(Arc::new(FolderMessages::new(f.id, role.or(f.well_known_name))));
            }
            Ok(out)
        })
    }

    fn handlers(&self) -> Vec<Arc<dyn OutboxHandler>> {
        handlers::all()
    }
}

/// Whether a sync error is Graph's "delta token expired" (410 / `syncStateNotFound`).
pub(crate) fn delta_expired(e: &crate::Error) -> bool {
    graph_kind(e) == Some(quorum_graph::ErrorKind::DeltaExpired)
}

/// The Graph error kind behind a sync error, if it came from Graph.
pub(crate) fn graph_kind(e: &crate::Error) -> Option<quorum_graph::ErrorKind> {
    std::error::Error::source(e).and_then(|s| s.downcast_ref::<quorum_graph::Error>()).map(quorum_graph::Error::kind)
}
