//! Local bookkeeping for files, kept in store settings (`files.*` keys).
//!
//! Nothing here is secret: upload session and copy monitor URLs go to encrypted blobs
//! ([`upload_session_blob`], [`copy_monitor_blob`]).

use std::collections::BTreeMap;

use jiff::Timestamp;
use quorum_model::{DriveId, DriveItem, DriveItemId, ListId, SiteId};
use quorum_store::Store;
use serde::{Deserialize, Serialize};

use crate::Result;

/// The user's OneDrive id.
pub const MY_DRIVE: &str = "files.my_drive";
/// Ordered refs of the Recent view.
pub const RECENT: &str = "files.recent";
/// Ordered refs of the Shared-with-me view.
pub const SHARED: &str = "files.shared";
/// Followed site ids, in Graph's order.
pub const FOLLOWED: &str = "files.followed";
/// Drive ids of every followed site's document libraries.
pub const LIBRARIES: &str = "files.libraries";
/// Upload progress, by operation id.
pub const UPLOADS: &str = "files.uploads";

/// Most followed sites tracked.
pub const MAX_SITES: usize = 200;
/// Most library drives kept in sync.
pub const MAX_LIBRARIES: usize = 400;
/// Most Recent / Shared refs kept.
pub const MAX_REFS: usize = 500;
/// Upload progress rows kept (finished rows are pruned oldest first).
pub const MAX_UPLOAD_ROWS: usize = 50;

/// Key of a drive's root item id.
#[must_use]
pub fn root_key(drive: &DriveId) -> String {
    format!("files.root:{}", drive.as_str())
}

/// Key of the drive ids of one site.
#[must_use]
pub fn site_drives_key(site: &SiteId) -> String {
    format!("files.site_drives:{}", site.as_str())
}

/// Key of when a resource last synced (Unix ms).
#[must_use]
pub fn synced_at_key(resource: &str) -> String {
    format!("files.synced_at:{resource}")
}

/// Key of a list's cached columns.
#[must_use]
pub fn columns_key(list: &ListId) -> String {
    format!("files.columns:{}", list.as_str())
}

/// Key of an item's cached sharing and version metadata.
#[must_use]
pub fn meta_key(drive: &DriveId, item: &DriveItemId) -> String {
    format!("files.meta:{}:{}", drive.as_str(), item.as_str())
}

/// Blob key holding a resumable upload session (secret URL).
#[must_use]
pub fn upload_session_blob(op: &str) -> String {
    format!("files.upload_session:{op}")
}

/// Blob key holding a copy monitor URL (secret).
#[must_use]
pub fn copy_monitor_blob(op: &str) -> String {
    format!("files.copy_monitor:{op}")
}

/// A drive item address.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ItemRef {
    /// The drive.
    pub drive: DriveId,
    /// The item.
    pub id: DriveItemId,
}

impl ItemRef {
    /// The address of `item`.
    #[must_use]
    pub fn of(item: &DriveItem) -> Self {
        Self { drive: item.drive_id.clone(), id: item.id.clone() }
    }
}

/// A list column as cached for the list view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnInfo {
    /// Internal name (the key in item fields).
    pub name: String,
    /// Display name.
    pub display_name: String,
    /// Kind (`text`, `number`, `choice`, `dateTime`, `boolean`, `personOrGroup`, ...; `unknown`).
    pub kind: String,
    /// Choices, for choice columns.
    #[serde(default)]
    pub choices: Vec<String>,
    /// Not editable.
    #[serde(default)]
    pub read_only: bool,
    /// Required.
    #[serde(default)]
    pub required: bool,
    /// Hidden from views.
    #[serde(default)]
    pub hidden: bool,
}

/// A permission as shown in the details pane. Link URLs are never cached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionInfo {
    /// The permission id.
    pub id: String,
    /// Roles (`read`, `write`, `owner`).
    pub roles: Vec<String>,
    /// `link` or `direct`.
    pub kind: String,
    /// Link type (`view`, `edit`), for links.
    #[serde(default)]
    pub link_type: Option<String>,
    /// Link scope (`anonymous`, `organization`, `users`), for links.
    #[serde(default)]
    pub link_scope: Option<String>,
    /// Display names of the people or groups it was granted to.
    #[serde(default)]
    pub granted_to: Vec<String>,
    /// Inherited from a parent folder.
    #[serde(default)]
    pub inherited: bool,
}

/// A version as shown in the details pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    /// The version id (e.g. `3.0`).
    pub id: String,
    /// When it was saved.
    #[serde(default)]
    pub modified: Option<Timestamp>,
    /// Who saved it.
    #[serde(default)]
    pub modified_by: Option<String>,
    /// Size in bytes.
    #[serde(default)]
    pub size: u64,
}

/// Sharing and version metadata of one item, fetched on demand.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ItemMeta {
    /// When this was fetched (Unix ms).
    pub fetched_at: i64,
    /// Permissions (links and direct grants).
    pub permissions: Vec<PermissionInfo>,
    /// Versions, newest first.
    pub versions: Vec<VersionInfo>,
}

/// Where an upload stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadState {
    /// Waiting in the outbox.
    Queued,
    /// Bytes are being sent.
    Uploading,
    /// Finished.
    Done,
    /// Gave up (the outbox entry failed).
    Failed,
}

/// Progress of one upload, for the UI's toasts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadProgress {
    /// File name.
    pub name: String,
    /// Total bytes.
    pub total: u64,
    /// Bytes Graph has accepted.
    pub sent: u64,
    /// State.
    pub state: UploadState,
    /// Last change (Unix ms), for pruning.
    pub updated: i64,
}

/// Unix milliseconds now.
#[must_use]
pub fn now_ms() -> i64 {
    Timestamp::now().as_millisecond()
}

async fn get<T: serde::de::DeserializeOwned + Send + 'static>(store: &Store, key: String) -> Result<Option<T>> {
    Ok(store.setting::<T>(key).await?)
}

/// The user's OneDrive id, once known.
///
/// # Errors
/// The store failed.
pub async fn my_drive(store: &Store) -> Result<Option<DriveId>> {
    get(store, MY_DRIVE.to_owned()).await
}

/// A drive's root item id, once its delta has seen it.
///
/// # Errors
/// The store failed.
pub async fn root_of(store: &Store, drive: &DriveId) -> Result<Option<DriveItemId>> {
    get(store, root_key(drive)).await
}

/// The Recent refs, newest first.
///
/// # Errors
/// The store failed.
pub async fn recent(store: &Store) -> Result<Vec<ItemRef>> {
    Ok(get(store, RECENT.to_owned()).await?.unwrap_or_default())
}

/// The Shared-with-me refs.
///
/// # Errors
/// The store failed.
pub async fn shared(store: &Store) -> Result<Vec<ItemRef>> {
    Ok(get(store, SHARED.to_owned()).await?.unwrap_or_default())
}

/// Followed sites, or `None` before the first sites pass.
///
/// # Errors
/// The store failed.
pub async fn followed(store: &Store) -> Result<Option<Vec<SiteId>>> {
    get(store, FOLLOWED.to_owned()).await
}

/// The library drives of `site`.
///
/// # Errors
/// The store failed.
pub async fn site_drives(store: &Store, site: &SiteId) -> Result<Vec<DriveId>> {
    Ok(get(store, site_drives_key(site)).await?.unwrap_or_default())
}

/// Every followed site's library drives.
///
/// # Errors
/// The store failed.
pub async fn libraries(store: &Store) -> Result<Vec<DriveId>> {
    Ok(get(store, LIBRARIES.to_owned()).await?.unwrap_or_default())
}

/// When `resource` last synced (Unix ms).
///
/// # Errors
/// The store failed.
pub async fn synced_at(store: &Store, resource: &str) -> Result<Option<i64>> {
    get(store, synced_at_key(resource)).await
}

/// A list's cached columns.
///
/// # Errors
/// The store failed.
pub async fn columns(store: &Store, list: &ListId) -> Result<Option<Vec<ColumnInfo>>> {
    get(store, columns_key(list)).await
}

/// An item's cached sharing and version metadata.
///
/// # Errors
/// The store failed.
pub async fn meta(store: &Store, drive: &DriveId, item: &DriveItemId) -> Result<Option<ItemMeta>> {
    get(store, meta_key(drive, item)).await
}

/// Upload progress by operation id.
///
/// # Errors
/// The store failed.
pub async fn uploads(store: &Store) -> Result<BTreeMap<String, UploadProgress>> {
    Ok(get(store, UPLOADS.to_owned()).await?.unwrap_or_default())
}

/// Records upload progress for `op` (read-modify-write in one transaction); prunes old
/// finished rows.
///
/// # Errors
/// The store failed.
pub fn put_upload(tx: &mut quorum_store::Tx<'_>, op: &str, progress: UploadProgress) -> quorum_store::Result<()> {
    let mut all: BTreeMap<String, UploadProgress> = tx.read().setting(UPLOADS)?.unwrap_or_default();
    all.insert(op.to_owned(), progress);
    while all.len() > MAX_UPLOAD_ROWS {
        let oldest = all
            .iter()
            .filter(|(_, p)| matches!(p.state, UploadState::Done | UploadState::Failed))
            .min_by_key(|(_, p)| p.updated)
            .or_else(|| all.iter().min_by_key(|(_, p)| p.updated))
            .map(|(k, _)| k.clone());
        match oldest {
            Some(k) => {
                all.remove(&k);
            }
            None => break,
        }
    }
    tx.set_setting(UPLOADS, &all)
}

/// Updates the state (and optionally the byte count) of an existing upload row.
///
/// # Errors
/// The store failed.
pub async fn set_upload_state(store: &Store, op: String, state: UploadState, sent: Option<u64>) -> Result<()> {
    store
        .write(move |tx| {
            let all: BTreeMap<String, UploadProgress> = tx.read().setting(UPLOADS)?.unwrap_or_default();
            if let Some(mut p) = all.get(&op).cloned() {
                p.state = state;
                if let Some(s) = sent {
                    p.sent = s.min(p.total);
                }
                p.updated = now_ms();
                put_upload(tx, &op, p)?;
            }
            Ok(())
        })
        .await?;
    Ok(())
}
