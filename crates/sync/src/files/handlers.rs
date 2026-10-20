//! Outbox handlers for files, sharing, sites and SharePoint lists.
//!
//! Payloads are the `*Payload` structs below; the engine's commands build them and apply the
//! optimistic local change in the same transaction. Each handler replays the write against
//! Graph and settles the local copy (placeholder replaced by the real item, and so on); `revert`
//! undoes the optimistic change when the user discards a failed entry.
//!
//! Handlers are written to be safe to run twice (replay is at-least-once): deletes treat 404 as
//! done, copies resume their stored monitor, uploads resume their stored session.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use quorum_graph::api::files::{ConflictBehavior, CopyStatus, Invite, ItemLocation, LinkScope, LinkType, SecretUrl};
use quorum_graph::{Request, ResourceFamily};
use quorum_model::{DriveId, DriveItem, DriveItemId, ListId, ListItem, ListItemId, MailAddress, Sensitive, SiteId};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::delta::gerr;
use super::state::{self, UploadState};
use super::upload::{FileSource, UploadJob, upload_file};
use super::{kinds, live};
use crate::{Ctx, Error, ErrorKind, OutboxHandler, Result};

/// Copy monitor polls per attempt.
pub const COPY_POLLS: u32 = 15;
/// Delay between copy monitor polls.
pub const COPY_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// `files.create_folder`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateFolderPayload {
    /// The drive.
    pub drive: DriveId,
    /// The parent folder.
    pub parent: DriveItemId,
    /// The new folder's name.
    pub name: String,
    /// The optimistic placeholder's id.
    pub local_id: DriveItemId,
}

/// `files.update` (rename and/or move).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatePayload {
    /// The drive.
    pub drive: DriveId,
    /// The item.
    pub item: DriveItemId,
    /// New name.
    #[serde(default)]
    pub name: Option<String>,
    /// New parent folder.
    #[serde(default)]
    pub parent: Option<DriveItemId>,
    /// New parent's drive (default: the same drive).
    #[serde(default)]
    pub parent_drive: Option<DriveId>,
    /// The item before the change, for revert.
    #[serde(default)]
    pub prev: Option<DriveItem>,
}

/// `files.copy`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyPayload {
    /// Source drive.
    pub drive: DriveId,
    /// Source item.
    pub item: DriveItemId,
    /// Destination drive.
    pub dest_drive: DriveId,
    /// Destination folder.
    pub dest_folder: DriveItemId,
    /// New name, if any.
    #[serde(default)]
    pub name: Option<String>,
    /// Operation id (keys the stored monitor).
    pub op: String,
}

/// `files.delete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletePayload {
    /// The drive.
    pub drive: DriveId,
    /// The item.
    pub item: DriveItemId,
    /// The item before deletion, for revert.
    #[serde(default)]
    pub prev: Option<DriveItem>,
}

/// `files.upload`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadPayload {
    /// Target drive.
    pub drive: DriveId,
    /// Target folder.
    pub parent: DriveItemId,
    /// Absolute local path.
    pub path: PathBuf,
    /// Name in the drive.
    pub name: String,
    /// Operation id (progress row, stored session).
    pub op: String,
    /// The optimistic placeholder's id.
    pub local_id: DriveItemId,
    /// Replace an existing file of the same name (otherwise Graph picks a new name).
    #[serde(default)]
    pub replace: bool,
    /// `path` is a staging copy owned by Quorum: delete it once the upload settles.
    #[serde(default)]
    pub staged: bool,
}

async fn drop_staged(p: &UploadPayload) {
    if p.staged {
        let _ = tokio::fs::remove_file(&p.path).await;
    }
}

/// `files.share_link`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareLinkPayload {
    /// The drive.
    pub drive: DriveId,
    /// The item.
    pub item: DriveItemId,
    /// `view` or `edit`.
    pub link_type: String,
    /// `organization`, `users` or `anonymous`.
    pub scope: String,
}

/// `files.invite`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvitePayload {
    /// The drive.
    pub drive: DriveId,
    /// The item.
    pub item: DriveItemId,
    /// Recipient addresses.
    pub recipients: Vec<MailAddress>,
    /// Grant edit rights.
    pub write: bool,
    /// Optional message.
    #[serde(default)]
    pub message: Option<String>,
    /// Send the invitation email.
    #[serde(default = "yes")]
    pub send_invitation: bool,
}

fn yes() -> bool {
    true
}

/// `files.delete_permission`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletePermissionPayload {
    /// The drive.
    pub drive: DriveId,
    /// The item.
    pub item: DriveItemId,
    /// The permission id.
    pub permission: String,
}

/// `files.restore_version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreVersionPayload {
    /// The drive.
    pub drive: DriveId,
    /// The item.
    pub item: DriveItemId,
    /// The version id.
    pub version: String,
}

/// `files.checkout` / `files.checkin`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckPayload {
    /// The drive.
    pub drive: DriveId,
    /// The item.
    pub item: DriveItemId,
    /// Check-in comment.
    #[serde(default)]
    pub comment: String,
}

/// `sites.follow` / `sites.unfollow`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowPayload {
    /// The site.
    pub site: SiteId,
    /// The followed list before the change, for revert.
    #[serde(default)]
    pub prev: Option<Vec<SiteId>>,
}

/// `lists.create_item` / `lists.update_item` / `lists.delete_item`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListItemPayload {
    /// The site.
    pub site: SiteId,
    /// The list.
    pub list: ListId,
    /// The item (the optimistic placeholder's id for creates).
    pub item: ListItemId,
    /// Field values to write.
    #[serde(default)]
    pub fields: Map<String, Value>,
    /// The item before the change, for revert.
    #[serde(default)]
    pub prev: Option<ListItem>,
}

/// Request body for `followedSites/add` and `/remove` (v1.0).
#[must_use]
pub fn follow_request(site: &SiteId, follow: bool) -> Request {
    let op = if follow { "add" } else { "remove" };
    Request::post(format!("/me/followedSites/{op}"))
        .json_value(json!({ "value": [{ "id": site.as_str() }] }))
        .family(ResourceFamily::Files)
        .bucket(quorum_graph::api::sites::SITES_BUCKET)
}

/// Parses `link_type` / `scope` strings as used in payloads and command params.
///
/// # Errors
/// [`ErrorKind::Invalid`] for unknown values.
pub fn link_kind(link_type: &str, scope: &str) -> Result<(LinkType, LinkScope)> {
    let t = match link_type {
        "view" => LinkType::View,
        "edit" => LinkType::Edit,
        _ => return Err(Error::new(ErrorKind::Invalid, "link type must be view or edit")),
    };
    let s = match scope {
        "organization" => LinkScope::Organization,
        "users" => LinkScope::Users,
        "anonymous" => LinkScope::Anonymous,
        _ => return Err(Error::new(ErrorKind::Invalid, "scope must be organization, users or anonymous")),
    };
    Ok((t, s))
}

fn decode<T: DeserializeOwned>(payload: Value) -> Result<T> {
    serde_json::from_value(payload)
        .map_err(|e| Error::new(ErrorKind::Permanent, "malformed outbox payload").with_source(e))
}

fn is_not_found(e: &quorum_graph::Error) -> bool {
    e.kind() == quorum_graph::ErrorKind::NotFound
}

type ApplyFn = for<'a> fn(&'a Ctx, Value) -> BoxFuture<'a, Result<Value>>;
type RevertFn = for<'a> fn(&'a Ctx, Value) -> BoxFuture<'a, Result<()>>;

struct Handler {
    kind: &'static str,
    apply: ApplyFn,
    revert: Option<RevertFn>,
}

impl OutboxHandler for Handler {
    fn kind(&self) -> &'static str {
        self.kind
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx, payload: Value) -> BoxFuture<'a, Result<Value>> {
        (self.apply)(ctx, payload)
    }
    fn revert<'a>(&'a self, ctx: &'a Ctx, payload: Value) -> BoxFuture<'a, Result<()>> {
        match self.revert {
            Some(f) => f(ctx, payload),
            None => Box::pin(async { Ok(()) }),
        }
    }
}

/// Every handler of this module.
pub(crate) fn all() -> Vec<Arc<dyn OutboxHandler>> {
    let h = |kind, apply: ApplyFn, revert: Option<RevertFn>| -> Arc<dyn OutboxHandler> {
        Arc::new(Handler { kind, apply, revert })
    };
    vec![
        h(kinds::CREATE_FOLDER, create_folder, Some(revert_create_folder)),
        h(kinds::UPDATE, update, Some(revert_update)),
        h(kinds::COPY, copy, None),
        h(kinds::DELETE, delete, Some(revert_delete)),
        h(kinds::UPLOAD, upload, Some(revert_upload)),
        h(kinds::SHARE_LINK, share_link, None),
        h(kinds::INVITE, invite, None),
        h(kinds::DELETE_PERMISSION, delete_permission, None),
        h(kinds::RESTORE_VERSION, restore_version, None),
        h(kinds::CHECKOUT, checkout, None),
        h(kinds::CHECKIN, checkin, None),
        h(kinds::FOLLOW_SITE, follow, Some(revert_follow)),
        h(kinds::UNFOLLOW_SITE, unfollow, Some(revert_follow)),
        h(kinds::LIST_CREATE, list_create, Some(revert_list_create)),
        h(kinds::LIST_UPDATE, list_update, Some(revert_list_change)),
        h(kinds::LIST_DELETE, list_delete, Some(revert_list_change)),
    ]
}

/// Replaces placeholder `local` with the real `item` in one transaction.
async fn settle(ctx: &Ctx, drive: &DriveId, local: Option<DriveItemId>, item: DriveItem) -> Result<()> {
    let d = drive.clone();
    ctx.store
        .write(move |tx| {
            if let Some(l) = local.filter(|l| *l != item.id) {
                tx.delete_drive_items(&d, &[l])?;
            }
            tx.upsert_drive_items(&[item])
        })
        .await?;
    Ok(())
}

async fn remove_local(ctx: &Ctx, drive: DriveId, id: DriveItemId) -> Result<()> {
    ctx.store.write(move |tx| tx.delete_drive_items(&drive, &[id])).await?;
    Ok(())
}

fn create_folder(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: CreateFolderPayload = decode(payload)?;
        let item =
            ctx.graph.create_folder(&p.drive, &p.parent, &p.name, ConflictBehavior::Rename).await.map_err(gerr)?;
        let id = item.id.clone();
        settle(ctx, &p.drive, Some(p.local_id), item).await?;
        Ok(json!({ "id": id }))
    })
}

fn revert_create_folder(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        let p: CreateFolderPayload = decode(payload)?;
        remove_local(ctx, p.drive, p.local_id).await
    })
}

fn update(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: UpdatePayload = decode(payload)?;
        let dest = p
            .parent
            .clone()
            .map(|folder| ItemLocation { drive: p.parent_drive.clone().unwrap_or_else(|| p.drive.clone()), folder });
        let item = ctx
            .graph
            .update_drive_item(&p.drive, &p.item, p.name.as_deref(), dest.as_ref(), None)
            .await
            .map_err(gerr)?;
        let id = item.id.clone();
        if item.drive_id != p.drive {
            // Moved to another drive: the old row is gone from this one.
            remove_local(ctx, p.drive.clone(), p.item.clone()).await?;
        }
        let d = item.drive_id.clone();
        settle(ctx, &d, None, item).await?;
        Ok(json!({ "id": id }))
    })
}

fn revert_update(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        let p: UpdatePayload = decode(payload)?;
        if let Some(prev) = p.prev {
            ctx.store.write(move |tx| tx.upsert_drive_items(&[prev])).await?;
        }
        Ok(())
    })
}

fn copy(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: CopyPayload = decode(payload)?;
        let blob = state::copy_monitor_blob(&p.op);
        // Resume the monitor of an earlier attempt instead of copying twice.
        let stored = ctx
            .store
            .get_blob(blob.clone())
            .await
            .ok()
            .flatten()
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|s| SecretUrl::parse(&s).ok());
        let monitor = match stored {
            Some(m) => m,
            None => {
                let dest = ItemLocation { drive: p.dest_drive.clone(), folder: p.dest_folder.clone() };
                let m = ctx.graph.copy_drive_item(&p.drive, &p.item, &dest, p.name.as_deref()).await.map_err(gerr)?;
                ctx.store.put_blob(blob.clone(), m.as_str().as_bytes().to_vec()).await?;
                m
            }
        };
        let status = ctx.graph.wait_for_copy(&monitor, COPY_POLL_INTERVAL, COPY_POLLS).await.map_err(gerr)?;
        match status {
            CopyStatus::Completed { item } => {
                let _ = ctx.store.delete_blob(blob).await;
                let mut new_id = Value::Null;
                if let Some(id) = item {
                    new_id = json!(id);
                    match ctx.graph.get_drive_item(&p.dest_drive, &id).await {
                        Ok(it) => ctx.store.write(move |tx| tx.upsert_drive_items(&[it])).await?,
                        Err(e) => {
                            tracing::info!(name: "files.copy_fetch_failed", kind = e.kind().as_str(), "copied item will arrive with the next delta")
                        }
                    }
                }
                Ok(json!({ "status": "completed", "id": new_id }))
            }
            CopyStatus::Failed { .. } => {
                let _ = ctx.store.delete_blob(blob).await;
                Err(Error::new(ErrorKind::Permanent, "the copy failed at the server"))
            }
            CopyStatus::InProgress { .. } => Err(Error::new(ErrorKind::Remote, "the copy is still running")),
        }
    })
}

fn delete(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: DeletePayload = decode(payload)?;
        match ctx.graph.delete_drive_item(&p.drive, &p.item, None).await {
            Ok(()) => {}
            Err(e) if is_not_found(&e) => {}
            Err(e) => return Err(gerr(e)),
        }
        // The optimistic delete already removed the subtree; make sure (idempotent).
        remove_local(ctx, p.drive, p.item).await?;
        Ok(Value::Null)
    })
}

fn revert_delete(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        let p: DeletePayload = decode(payload)?;
        let key = super::drive_resource(&p.drive);
        ctx.store
            .write(move |tx| {
                if let Some(prev) = p.prev {
                    tx.upsert_drive_items(&[prev])?;
                }
                // Children went with the optimistic delete: list the drive again.
                tx.clear_delta_state(&key)
            })
            .await?;
        Ok(())
    })
}

fn upload(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: UploadPayload = decode(payload)?;
        let source = FileSource::open(p.path.clone()).await?.with_progress(ctx.store.clone(), p.op.clone());
        let job = UploadJob {
            drive: p.drive.clone(),
            parent: p.parent.clone(),
            name: p.name.clone(),
            op: p.op.clone(),
            conflict: if p.replace { ConflictBehavior::Replace } else { ConflictBehavior::Rename },
        };
        state::set_upload_state(&ctx.store, p.op.clone(), UploadState::Uploading, None).await?;
        let item = match upload_file(ctx, &job, source).await {
            Ok(i) => i,
            Err(e) => {
                if e.kind() == ErrorKind::Permanent {
                    let _ = state::set_upload_state(&ctx.store, p.op.clone(), UploadState::Failed, None).await;
                }
                return Err(e);
            }
        };
        let id = item.id.clone();
        let size = item.size;
        settle(ctx, &p.drive, Some(p.local_id.clone()), item).await?;
        drop_staged(&p).await;
        state::set_upload_state(&ctx.store, p.op, UploadState::Done, Some(size)).await?;
        Ok(json!({ "id": id }))
    })
}

fn revert_upload(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        let p: UploadPayload = decode(payload)?;
        let _ = ctx.store.delete_blob(state::upload_session_blob(&p.op)).await;
        drop_staged(&p).await;
        state::set_upload_state(&ctx.store, p.op, UploadState::Failed, None).await?;
        remove_local(ctx, p.drive, p.local_id).await
    })
}

/// Refreshes the cached permissions after a sharing change; failures only delay the update.
async fn refresh_meta(ctx: &Ctx, drive: &DriveId, item: &DriveItemId) {
    let folder = ctx
        .store
        .read({
            let (d, i) = (drive.clone(), item.clone());
            move |r| r.drive_item(&d, &i)
        })
        .await
        .ok()
        .flatten()
        .is_some_and(|i| i.is_folder());
    if let Err(e) = live::fetch_item_meta(ctx, drive, item, folder).await {
        tracing::info!(name: "files.meta_refresh_failed", kind = ?e.kind(), "sharing details refresh later");
    }
}

fn share_link(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: ShareLinkPayload = decode(payload)?;
        let (t, s) = link_kind(&p.link_type, &p.scope).map_err(|e| Error::new(ErrorKind::Permanent, e.message()))?;
        let perm = ctx.graph.create_sharing_link(&p.drive, &p.item, t, s, None).await.map_err(gerr)?;
        refresh_meta(ctx, &p.drive, &p.item).await;
        Ok(json!({ "permission": perm.id }))
    })
}

fn invite(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: InvitePayload = decode(payload)?;
        let inv = Invite {
            recipients: p.recipients,
            write: p.write,
            send_invitation: p.send_invitation,
            message: p.message.map(Sensitive::new),
            expiration: None,
        };
        let perms = ctx.graph.invite(&p.drive, &p.item, &inv).await.map_err(gerr)?;
        refresh_meta(ctx, &p.drive, &p.item).await;
        Ok(json!({ "permissions": perms.len() }))
    })
}

fn delete_permission(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: DeletePermissionPayload = decode(payload)?;
        match ctx.graph.delete_permission(&p.drive, &p.item, &p.permission).await {
            Ok(()) => {}
            Err(e) if is_not_found(&e) => {}
            Err(e) => return Err(gerr(e)),
        }
        refresh_meta(ctx, &p.drive, &p.item).await;
        Ok(Value::Null)
    })
}

fn restore_version(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: RestoreVersionPayload = decode(payload)?;
        ctx.graph.restore_version(&p.drive, &p.item, &p.version).await.map_err(gerr)?;
        if let Ok(item) = ctx.graph.get_drive_item(&p.drive, &p.item).await {
            ctx.store.write(move |tx| tx.upsert_drive_items(&[item])).await?;
        }
        refresh_meta(ctx, &p.drive, &p.item).await;
        Ok(Value::Null)
    })
}

fn checkout(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: CheckPayload = decode(payload)?;
        ctx.graph.checkout(&p.drive, &p.item).await.map_err(gerr)?;
        Ok(Value::Null)
    })
}

fn checkin(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: CheckPayload = decode(payload)?;
        ctx.graph.checkin(&p.drive, &p.item, &p.comment).await.map_err(gerr)?;
        Ok(Value::Null)
    })
}

fn follow(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: FollowPayload = decode(payload)?;
        ctx.graph.send_raw(follow_request(&p.site, true)).await.map_err(gerr)?;
        // Pick up the site's libraries and lists.
        if let Err(e) = super::refresh_sites(ctx).await {
            tracing::info!(name: "files.sites_refresh_failed", kind = ?e.kind(), "site libraries arrive with the next pass");
        }
        Ok(Value::Null)
    })
}

fn unfollow(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: FollowPayload = decode(payload)?;
        match ctx.graph.send_raw(follow_request(&p.site, false)).await {
            Ok(_) => {}
            Err(e) if is_not_found(&e) => {}
            Err(e) => return Err(gerr(e)),
        }
        Ok(Value::Null)
    })
}

fn revert_follow(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        let p: FollowPayload = decode(payload)?;
        if let Some(prev) = p.prev {
            ctx.store.write(move |tx| tx.set_setting(state::FOLLOWED, &prev)).await?;
        }
        Ok(())
    })
}

fn list_create(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: ListItemPayload = decode(payload)?;
        let created = ctx.graph.create_list_item(&p.site, &p.list, &p.fields).await.map_err(gerr)?;
        let id = created.id.clone();
        let (list, local) = (p.list.clone(), p.item.clone());
        ctx.store
            .write(move |tx| {
                if local != created.id {
                    tx.delete_list_items(&list, &[local])?;
                }
                tx.upsert_list_items(&list, &[created])
            })
            .await?;
        Ok(json!({ "id": id }))
    })
}

fn revert_list_create(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        let p: ListItemPayload = decode(payload)?;
        ctx.store.write(move |tx| tx.delete_list_items(&p.list, &[p.item])).await?;
        Ok(())
    })
}

fn list_update(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: ListItemPayload = decode(payload)?;
        let fields = ctx.graph.update_list_item(&p.site, &p.list, &p.item, &p.fields, None).await.map_err(gerr)?;
        if let Some(mut it) = p.prev {
            for (k, v) in p.fields.into_iter().chain(fields) {
                it.fields.insert(k, v);
            }
            let list = p.list;
            ctx.store.write(move |tx| tx.upsert_list_items(&list, &[it])).await?;
        }
        Ok(Value::Null)
    })
}

fn list_delete(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<Value>> {
    Box::pin(async move {
        let p: ListItemPayload = decode(payload)?;
        match ctx.graph.delete_list_item(&p.site, &p.list, &p.item).await {
            Ok(()) => {}
            Err(e) if is_not_found(&e) => {}
            Err(e) => return Err(gerr(e)),
        }
        Ok(Value::Null)
    })
}

fn revert_list_change(ctx: &Ctx, payload: Value) -> BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        let p: ListItemPayload = decode(payload)?;
        if let Some(prev) = p.prev {
            ctx.store.write(move |tx| tx.upsert_list_items(&p.list, &[prev])).await?;
        }
        Ok(())
    })
}
