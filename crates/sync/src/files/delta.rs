//! Drive delta, Recent, Shared with me and followed sites.

use futures::future::BoxFuture;
use quorum_graph::api::files::{FILES_BUCKET, drive_delta_request, parse_drive_item};
use quorum_graph::{DeltaOutcome, Request, ResourceFamily};
use quorum_model::{Drive, DriveId, DriveItem, DriveItemId, List, SiteId};
use quorum_store::DeltaState;
use serde_json::Value;

use super::state::{self, ItemRef};
use crate::{Cadence, Ctx, Error, ErrorKind, Outcome, Resource, Result};

/// Delta pages applied per pass before yielding ([`Outcome::More`]).
pub const MAX_DELTA_PAGES: usize = 20;
/// Key of the user's OneDrive delta resource.
pub const MY_DRIVE_KEY: &str = "files.mydrive";
/// Key of the Recent resource.
pub const RECENT_KEY: &str = "files.recent";
/// Key of the Shared-with-me resource.
pub const SHARED_KEY: &str = "files.shared";
/// Key of the followed-sites resource.
pub const SITES_KEY: &str = "files.sites";
/// Key prefix of a site library's delta resource.
pub const LIBRARY_PREFIX: &str = "files.library:";

/// Maps a Graph error; request-building (`Config`) and mapping (`Malformed`) failures can never
/// succeed on retry.
pub(crate) fn gerr(e: quorum_graph::Error) -> Error {
    use quorum_graph::ErrorKind as G;
    match e.kind() {
        G::Config | G::Malformed | G::TooLarge => Error::new(ErrorKind::Permanent, "request rejected").with_source(e),
        _ => Error::from(e),
    }
}

/// Splits a raw delta page into upserts, deletes and the root item id (if seen).
pub(crate) fn split_changes(
    raw: Vec<Value>,
    drive: &DriveId,
) -> (Vec<DriveItem>, Vec<DriveItemId>, Option<DriveItemId>, usize) {
    let mut upserts = Vec::new();
    let mut deletes = Vec::new();
    let mut root = None;
    let mut skipped = 0usize;
    for v in raw {
        let gone = v.get("@removed").is_some() || v.get("deleted").is_some_and(|d| !d.is_null());
        if gone {
            match v.get("id").and_then(Value::as_str).and_then(|s| DriveItemId::new(s).ok()) {
                Some(id) => deletes.push(id),
                None => skipped = skipped.saturating_add(1),
            }
            continue;
        }
        let is_root = v.get("root").is_some_and(|r| !r.is_null());
        match parse_drive_item(v, Some(drive)) {
            Some(mut item) => {
                // A drive's delta only describes that drive.
                item.drive_id = drive.clone();
                if is_root {
                    item.parent_id = None;
                    root = Some(item.id.clone());
                } else if item.parent_id.as_ref() == Some(&item.id) {
                    // A self-parented item would hide itself; treat it as top level.
                    item.parent_id = None;
                }
                upserts.push(item);
            }
            None => skipped = skipped.saturating_add(1),
        }
    }
    (upserts, deletes, root, skipped)
}

fn files_link(link: &str) -> Request {
    Request::link(link).family(ResourceFamily::Files).bucket(FILES_BUCKET)
}

/// Runs one delta pass over `drive`: follows the stored `nextLink` / `deltaLink` (or starts a
/// full listing), applying each page with its new delta state in one transaction. A 410
/// (`syncStateNotFound`) drops the drive's cached items and starts over.
///
/// # Errors
/// Graph or the store failed.
pub async fn sync_drive(ctx: &Ctx, drive: &DriveId, max_pages: usize) -> Result<Outcome> {
    let key = super::drive_resource(drive);
    let mut st = ctx.store.delta_state(key.clone()).await?.unwrap_or_else(|| DeltaState::new(key.clone()));
    for _ in 0..max_pages.max(1) {
        let link = st.next_link.clone().or_else(|| st.delta_link.clone());
        let req = match &link {
            Some(l) => files_link(l),
            None => drive_delta_request(drive, None),
        };
        let page = match ctx.graph.delta_page::<Value>(req).await {
            Ok(p) => p,
            Err(e) if e.kind() == quorum_graph::ErrorKind::DeltaExpired => {
                tracing::info!(name: "files.delta_expired", "drive delta token expired; resyncing");
                resync(ctx, drive).await?;
                return Ok(Outcome::More);
            }
            Err(e) => return Err(gerr(e)),
        };
        let (upserts, deletes, root, skipped) = split_changes(page.items, drive);
        let skipped = skipped.saturating_add(page.skipped);
        if skipped > 0 {
            tracing::warn!(name: "files.delta_skipped", count = skipped, "skipped unreadable drive items");
        }
        let done = matches!(page.outcome, DeltaOutcome::Done(_));
        match page.outcome {
            DeltaOutcome::Next(l) => st.next_link = Some(l),
            DeltaOutcome::Done(l) => {
                st.delta_link = Some(l);
                st.next_link = None;
            }
        }
        let d = drive.clone();
        let next = st.clone();
        let synced = state::synced_at_key(&key);
        ctx.store
            .write(move |tx| {
                tx.delete_drive_items(&d, &deletes)?;
                tx.upsert_drive_items(&upserts)?;
                if let Some(r) = root {
                    tx.set_setting(&state::root_key(&d), &r)?;
                }
                tx.set_delta_state(&next)?;
                tx.set_setting(&synced, &state::now_ms())
            })
            .await?;
        if done {
            return Ok(Outcome::CaughtUp);
        }
    }
    Ok(Outcome::More)
}

/// Drops the drive's cached items and delta state (the drive row stays) so the next pass lists
/// everything again.
///
/// # Errors
/// The store failed.
pub(crate) async fn resync(ctx: &Ctx, drive: &DriveId) -> Result<()> {
    let d = drive.clone();
    ctx.store
        .write(move |tx| {
            let keep: Vec<Drive> = tx.read().drives()?.items.into_iter().filter(|x| x.id == d).collect();
            tx.delete_drives(std::slice::from_ref(&d))?;
            tx.upsert_drives(&keep)?;
            tx.clear_delta_state(&super::drive_resource(&d))
        })
        .await?;
    Ok(())
}

/// The user's OneDrive id: from the store, else `GET /me/drive` (stored with the drive).
///
/// # Errors
/// Graph or the store failed.
pub async fn ensure_my_drive(ctx: &Ctx) -> Result<DriveId> {
    if let Some(d) = state::my_drive(&ctx.store).await? {
        return Ok(d);
    }
    let drive = ctx.graph.my_drive().await.map_err(gerr)?;
    let id = drive.id.clone();
    let stored = id.clone();
    ctx.store
        .write(move |tx| {
            tx.upsert_drives(&[drive])?;
            tx.set_setting(state::MY_DRIVE, &stored)
        })
        .await?;
    Ok(id)
}

async fn store_refs(ctx: &Ctx, key: &'static str, items: Vec<DriveItem>) -> Result<()> {
    let refs: Vec<ItemRef> = items.iter().take(state::MAX_REFS).map(ItemRef::of).collect();
    let synced = state::synced_at_key(key);
    ctx.store
        .write(move |tx| {
            tx.upsert_drive_items(&items)?;
            tx.set_setting(key, &refs)?;
            tx.set_setting(&synced, &state::now_ms())
        })
        .await?;
    Ok(())
}

/// Refreshes the Recent view (`/me/drive/recent`).
///
/// # Errors
/// Graph or the store failed.
pub async fn refresh_recent(ctx: &Ctx) -> Result<()> {
    let page = ctx.graph.recent_files().await.map_err(gerr)?;
    store_refs(ctx, state::RECENT, page.items).await
}

/// Refreshes the Shared-with-me view (`/me/drive/sharedWithMe`).
///
/// # Errors
/// Graph or the store failed.
pub async fn refresh_shared(ctx: &Ctx) -> Result<()> {
    let page = ctx.graph.shared_with_me().await.map_err(gerr)?;
    store_refs(ctx, state::SHARED, page.items).await
}

/// Refreshes followed sites with their document libraries (drives) and lists. A site whose
/// drives or lists cannot be read keeps what was cached for it.
///
/// # Errors
/// Graph (for the followed list itself) or the store failed.
pub async fn refresh_sites(ctx: &Ctx) -> Result<()> {
    let sites = ctx.graph.followed_sites().await.map_err(gerr)?;
    let sites: Vec<_> = sites.into_iter().take(state::MAX_SITES).collect();
    let mut per_site: Vec<(SiteId, Option<Vec<Drive>>, Option<Vec<List>>)> = Vec::with_capacity(sites.len());
    for s in &sites {
        let drives = match ctx.graph.site_drives(&s.id).await {
            Ok(d) => Some(d),
            Err(e) => {
                tracing::warn!(name: "files.site_drives_failed", kind = e.kind().as_str(), "site libraries unavailable");
                None
            }
        };
        let lists = match ctx.graph.site_lists(&s.id).await {
            Ok(l) => Some(l),
            Err(e) => {
                tracing::warn!(name: "files.site_lists_failed", kind = e.kind().as_str(), "site lists unavailable");
                None
            }
        };
        per_site.push((s.id.clone(), drives, lists));
    }
    let followed: Vec<SiteId> = sites.iter().map(|s| s.id.clone()).collect();
    ctx.store
        .write(move |tx| {
            tx.upsert_sites(&sites)?;
            let mut libraries: Vec<DriveId> = Vec::new();
            for (site, drives, lists) in per_site {
                let key = state::site_drives_key(&site);
                let ids: Vec<DriveId> = match drives {
                    Some(d) => {
                        tx.upsert_drives(&d)?;
                        let ids: Vec<DriveId> = d.into_iter().map(|x| x.id).collect();
                        tx.set_setting(&key, &ids)?;
                        ids
                    }
                    None => tx.read().setting(&key)?.unwrap_or_default(),
                };
                for id in ids {
                    if !libraries.contains(&id) && libraries.len() < state::MAX_LIBRARIES {
                        libraries.push(id);
                    }
                }
                if let Some(l) = lists {
                    tx.upsert_lists(&site, &l)?;
                }
            }
            tx.set_setting(state::FOLLOWED, &followed)?;
            tx.set_setting(state::LIBRARIES, &libraries)?;
            tx.set_setting(&state::synced_at_key(SITES_KEY), &state::now_ms())
        })
        .await?;
    Ok(())
}

/// The user's OneDrive.
pub(crate) struct MyDrive;

impl Resource for MyDrive {
    fn key(&self) -> String {
        MY_DRIVE_KEY.to_owned()
    }
    fn area(&self) -> &'static str {
        "files"
    }
    fn cadence(&self) -> Cadence {
        Cadence::secs(120, 600, 1800)
    }
    fn run<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(async move {
            let drive = ensure_my_drive(ctx).await?;
            sync_drive(ctx, &drive, MAX_DELTA_PAGES).await
        })
    }
}

/// `/me/drive/recent`.
pub(crate) struct Recent;

impl Resource for Recent {
    fn key(&self) -> String {
        RECENT_KEY.to_owned()
    }
    fn area(&self) -> &'static str {
        "files"
    }
    fn cadence(&self) -> Cadence {
        Cadence::secs(300, 900, 3600)
    }
    fn run<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(async move { refresh_recent(ctx).await.map(|()| Outcome::CaughtUp) })
    }
}

/// `/me/drive/sharedWithMe`.
pub(crate) struct Shared;

impl Resource for Shared {
    fn key(&self) -> String {
        SHARED_KEY.to_owned()
    }
    fn area(&self) -> &'static str {
        "files"
    }
    fn cadence(&self) -> Cadence {
        Cadence::secs(300, 900, 3600)
    }
    fn run<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(async move { refresh_shared(ctx).await.map(|()| Outcome::CaughtUp) })
    }
}

/// Followed sites, their libraries and lists.
pub(crate) struct Sites;

impl Resource for Sites {
    fn key(&self) -> String {
        SITES_KEY.to_owned()
    }
    fn area(&self) -> &'static str {
        "files"
    }
    fn cadence(&self) -> Cadence {
        Cadence::secs(900, 3600, 3600)
    }
    fn run<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(async move {
            let before = state::libraries(&ctx.store).await.unwrap_or_default();
            refresh_sites(ctx).await?;
            let after = state::libraries(&ctx.store).await.unwrap_or_default();
            // A site was followed or unfollowed: rediscover now so its libraries start (or
            // stop) syncing without waiting for the periodic rediscovery.
            Ok(if after == before { Outcome::CaughtUp } else { Outcome::Rediscover })
        })
    }
}

/// A followed site's document library.
pub(crate) struct Library {
    drive: DriveId,
}

impl Library {
    pub(crate) fn new(drive: DriveId) -> Self {
        Self { drive }
    }
}

impl Resource for Library {
    fn key(&self) -> String {
        format!("{LIBRARY_PREFIX}{}", self.drive.as_str())
    }
    fn area(&self) -> &'static str {
        "files"
    }
    fn cadence(&self) -> Cadence {
        Cadence::secs(300, 3600, 3600)
    }
    fn run<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(sync_drive(ctx, &self.drive, MAX_DELTA_PAGES))
    }
}
