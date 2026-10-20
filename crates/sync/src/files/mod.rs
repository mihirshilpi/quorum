//! OneDrive and SharePoint sync (architecture §8 "OneDrive and SharePoint").
//!
//! Resources:
//!
//! | Key | What | Cadence (focused / background / idle) |
//! |---|---|---|
//! | `files.mydrive` | `/drives/{id}/root/delta` of the user's OneDrive | 2 min / 10 min / 30 min |
//! | `files.recent` | `/me/drive/recent` | 5 min / 15 min / 1 h |
//! | `files.shared` | `/me/drive/sharedWithMe` | 5 min / 15 min / 1 h |
//! | `files.sites` | `/me/followedSites`, each site's drives (document libraries) and lists | 15 min / 1 h / 1 h |
//! | `files.library:<drive>` | delta of a followed site's library (also run when opened) | 5 min / 1 h / 1 h |
//!
//! SharePoint list items are fetched on demand ([`live::fetch_list_items`]) and cached.
//!
//! Local bookkeeping lives in store settings under `files.*` keys ([`state`]); secrets
//! (upload session URLs, copy monitor URLs) live in encrypted blobs, never in settings or logs.
//!
//! Writes are queued by the engine's `files.*` / `lists.*` / `sites.*` commands with the
//! [`kinds`] below and replayed by [`handlers`].

mod delta;
pub mod handlers;
pub mod live;
pub mod state;
mod upload;

use std::sync::Arc;

use futures::future::BoxFuture;
use quorum_model::DriveId;

use crate::{Ctx, Module, OutboxHandler, Resource, Result};

#[doc(inline)]
pub use delta::{
    LIBRARY_PREFIX, MAX_DELTA_PAGES, MY_DRIVE_KEY, RECENT_KEY, SHARED_KEY, SITES_KEY, ensure_my_drive, refresh_recent,
    refresh_shared, refresh_sites, sync_drive,
};
#[doc(inline)]
pub use live::{download, unique_path};
#[doc(inline)]
pub use upload::{FileSource, SMALL_UPLOAD_LIMIT, remember_upload_session};

/// Outbox entry kinds replayed by this module.
pub mod kinds {
    /// Create a folder.
    pub const CREATE_FOLDER: &str = "files.create_folder";
    /// Rename and/or move an item.
    pub const UPDATE: &str = "files.update";
    /// Copy an item (asynchronous; the monitor is polled).
    pub const COPY: &str = "files.copy";
    /// Delete an item to the recycle bin.
    pub const DELETE: &str = "files.delete";
    /// Upload a local file (simple or resumable).
    pub const UPLOAD: &str = "files.upload";
    /// Create a sharing link.
    pub const SHARE_LINK: &str = "files.share_link";
    /// Invite people.
    pub const INVITE: &str = "files.invite";
    /// Remove a permission.
    pub const DELETE_PERMISSION: &str = "files.delete_permission";
    /// Restore an older version.
    pub const RESTORE_VERSION: &str = "files.restore_version";
    /// Check a file out.
    pub const CHECKOUT: &str = "files.checkout";
    /// Check a file in.
    pub const CHECKIN: &str = "files.checkin";
    /// Follow a site.
    pub const FOLLOW_SITE: &str = "sites.follow";
    /// Stop following a site.
    pub const UNFOLLOW_SITE: &str = "sites.unfollow";
    /// Create a list item.
    pub const LIST_CREATE: &str = "lists.create_item";
    /// Update a list item.
    pub const LIST_UPDATE: &str = "lists.update_item";
    /// Delete a list item.
    pub const LIST_DELETE: &str = "lists.delete_item";

    /// Every kind, for registration checks.
    pub const ALL: [&str; 16] = [
        CREATE_FOLDER,
        UPDATE,
        COPY,
        DELETE,
        UPLOAD,
        SHARE_LINK,
        INVITE,
        DELETE_PERMISSION,
        RESTORE_VERSION,
        CHECKOUT,
        CHECKIN,
        FOLLOW_SITE,
        UNFOLLOW_SITE,
        LIST_CREATE,
        LIST_UPDATE,
        LIST_DELETE,
    ];
}

/// The outbox ordering key for writes to items of `drive`.
#[must_use]
pub fn drive_resource(drive: &DriveId) -> String {
    format!("files.drive:{}", drive.as_str())
}

/// The outbox ordering key for writes to a SharePoint list.
#[must_use]
pub fn list_resource(list: &quorum_model::ListId) -> String {
    format!("files.list:{}", list.as_str())
}

/// The outbox ordering key for follow / unfollow.
pub const SITES_RESOURCE: &str = "files.sites";

/// The OneDrive / SharePoint sync module.
#[derive(Debug, Clone, Copy, Default)]
pub struct FilesModule;

impl Module for FilesModule {
    fn name(&self) -> &'static str {
        "files"
    }

    fn discover<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Vec<Arc<dyn Resource>>>> {
        Box::pin(async move {
            let mut out: Vec<Arc<dyn Resource>> = vec![
                Arc::new(delta::MyDrive),
                Arc::new(delta::Recent),
                Arc::new(delta::Shared),
                Arc::new(delta::Sites),
            ];
            // Libraries of followed sites known from the last sites pass; a store error only
            // means fewer libraries until the next discovery.
            let libraries = state::libraries(&ctx.store).await.unwrap_or_default();
            for d in libraries.into_iter().take(state::MAX_LIBRARIES) {
                out.push(Arc::new(delta::Library::new(d)));
            }
            Ok(out)
        })
    }

    fn handlers(&self) -> Vec<Arc<dyn OutboxHandler>> {
        handlers::all()
    }
}
