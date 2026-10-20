//! On-demand Graph reads the engine's queries need right away: list items, sharing and
//! version metadata, preview URLs, drive search, copy-link and downloads. Results that are
//! worth keeping are cached in the store.

use std::path::{Path, PathBuf};

use quorum_graph::api::files::{LinkScope, LinkType, Permission};
use quorum_graph::api::sites::ListColumn;
use quorum_model::{DriveId, DriveItem, DriveItemId, ListId, ListItem, SiteId};

use super::delta::gerr;
use super::state::{self, ColumnInfo, ItemMeta, PermissionInfo, VersionInfo};
use crate::{Ctx, Error, ErrorKind, Result};

/// List item pages followed per on-demand fetch.
pub const MAX_LIST_PAGES: usize = 5;
/// List items requested per page.
pub const LIST_PAGE_SIZE: u32 = 200;
/// Largest file name written to disk, in bytes.
const MAX_FILE_NAME_BYTES: usize = 200;
/// Most " (n)" suffixes tried before giving up.
const MAX_DEDUP: u32 = 10_000;

fn column_info(c: ListColumn) -> ColumnInfo {
    ColumnInfo {
        name: c.name,
        display_name: c.display_name.into_inner(),
        kind: c.kind.unwrap_or_else(|| "unknown".to_owned()),
        choices: c.choices,
        read_only: c.read_only,
        required: c.required,
        hidden: c.hidden,
    }
}

/// Fetches a list's columns and up to [`MAX_LIST_PAGES`] pages of items, caching both.
/// Returns how many items were stored.
///
/// # Errors
/// Graph or the store failed.
pub async fn fetch_list_items(ctx: &Ctx, site: &SiteId, list: &ListId) -> Result<usize> {
    let columns: Vec<ColumnInfo> = match ctx.graph.list_columns(site, list).await {
        Ok(c) => c.into_iter().take(256).map(column_info).collect(),
        Err(e) => {
            tracing::warn!(name: "files.list_columns_failed", kind = e.kind().as_str(), "list columns unavailable");
            Vec::new()
        }
    };
    let mut items: Vec<ListItem> = Vec::new();
    let mut page = ctx.graph.list_items(site, list, &[], LIST_PAGE_SIZE).await.map_err(gerr)?;
    for _ in 0..MAX_LIST_PAGES {
        items.append(&mut page.items);
        let Some(next) = page.next_link.take() else { break };
        page = match ctx.graph.list_items_next_page(list, &next).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(name: "files.list_page_failed", kind = e.kind().as_str(), "stopping list paging");
                break;
            }
        };
    }
    let n = items.len();
    let l = list.clone();
    let synced = state::synced_at_key(&super::list_resource(list));
    ctx.store
        .write(move |tx| {
            tx.upsert_list_items(&l, &items)?;
            if !columns.is_empty() {
                tx.set_setting(&state::columns_key(&l), &columns)?;
            }
            tx.set_setting(&synced, &state::now_ms())
        })
        .await?;
    Ok(n)
}

pub(crate) fn permission_info(p: &Permission) -> PermissionInfo {
    PermissionInfo {
        id: p.id.clone(),
        roles: p.roles.clone(),
        kind: if p.link_type.is_some() { "link".to_owned() } else { "direct".to_owned() },
        link_type: p.link_type.clone(),
        link_scope: p.link_scope.clone(),
        granted_to: p.granted_to.iter().map(|g| g.display_name.as_str().to_owned()).filter(|n| !n.is_empty()).collect(),
        inherited: p.inherited,
    }
}

/// Fetches an item's permissions and versions and caches them ([`state::meta`]). Folders have
/// no versions; a failed half is left empty.
///
/// # Errors
/// The store failed, or both halves failed at Graph.
pub async fn fetch_item_meta(ctx: &Ctx, drive: &DriveId, item: &DriveItemId, folder: bool) -> Result<ItemMeta> {
    let perms = ctx.graph.list_permissions(drive, item).await;
    let versions = if folder { Ok(Vec::new()) } else { ctx.graph.list_versions(drive, item).await };
    let perms = match (perms, &versions) {
        (Err(e), Err(_)) => return Err(gerr(e)),
        (p, _) => p,
    };
    let meta = ItemMeta {
        fetched_at: state::now_ms(),
        permissions: perms.unwrap_or_default().iter().take(200).map(permission_info).collect(),
        versions: versions
            .unwrap_or_default()
            .into_iter()
            .take(500)
            .map(|v| VersionInfo {
                id: v.id,
                modified: v.last_modified_date_time,
                modified_by: v.last_modified_by.map(|i| i.display_name.into_inner()),
                size: v.size,
            })
            .collect(),
    };
    let key = state::meta_key(drive, item);
    let stored = meta.clone();
    ctx.store.write(move |tx| tx.set_setting(&key, &stored)).await?;
    Ok(meta)
}

/// An embeddable, short-lived preview URL. Treat as a secret: never log or cache it.
///
/// # Errors
/// Graph failed.
pub async fn preview_url(ctx: &Ctx, drive: &DriveId, item: &DriveItemId) -> Result<String> {
    Ok(ctx.graph.preview_url(drive, item).await.map_err(gerr)?.as_str().to_owned())
}

/// Searches a drive at Graph (first page) and caches the hits.
///
/// # Errors
/// Graph or the store failed.
pub async fn search_drive(ctx: &Ctx, drive: &DriveId, query: &str) -> Result<Vec<DriveItem>> {
    let page = ctx.graph.search_drive(drive, query).await.map_err(gerr)?;
    let items: Vec<DriveItem> = page.items.into_iter().take(200).collect();
    let stored = items.clone();
    ctx.store.write(move |tx| tx.upsert_drive_items(&stored)).await?;
    Ok(items)
}

/// Creates (or returns the existing) sharing link and returns its URL. The URL is a
/// capability: the caller hands it to the user (clipboard) and never logs it.
///
/// # Errors
/// Graph failed or returned no link.
pub async fn create_link(
    ctx: &Ctx,
    drive: &DriveId,
    item: &DriveItemId,
    link_type: LinkType,
    scope: LinkScope,
) -> Result<String> {
    let p = ctx.graph.create_sharing_link(drive, item, link_type, scope, None).await.map_err(gerr)?;
    p.link_url.map(|u| u.as_str().to_owned()).ok_or_else(|| Error::new(ErrorKind::Remote, "Graph returned no link"))
}

/// Makes a Graph file name safe as a local file name: no path separators, control or
/// reserved characters, no leading dots, bounded length; never empty.
#[must_use]
pub fn safe_file_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| if c.is_control() || "/\\:*?\"<>|".contains(c) { '_' } else { c })
        .collect::<String>()
        .trim()
        .trim_start_matches('.')
        .to_owned();
    if out.len() > MAX_FILE_NAME_BYTES {
        let (stem, ext) = split_ext(&out);
        let ext: String = ext.chars().take(16).collect();
        let mut stem: String = stem.to_owned();
        while stem.len().saturating_add(ext.len()) > MAX_FILE_NAME_BYTES {
            if stem.pop().is_none() {
                break;
            }
        }
        out = format!("{stem}{ext}");
    }
    if out.is_empty() || out == "." || out == ".." { "download".to_owned() } else { out }
}

/// Splits `name` into stem and extension (with the dot); dotfiles have no extension.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    }
}

/// The first path in `dir` for `name` that does not exist: `name`, then `stem (1).ext`,
/// `stem (2).ext`, ... An existing file is never overwritten.
///
/// # Errors
/// [`ErrorKind::Invalid`] if no free name was found.
pub fn unique_path(dir: &Path, name: &str) -> Result<PathBuf> {
    let name = safe_file_name(name);
    let first = dir.join(&name);
    if !first.exists() {
        return Ok(first);
    }
    let (stem, ext) = split_ext(&name);
    for n in 1..=MAX_DEDUP {
        let p = dir.join(format!("{stem} ({n}){ext}"));
        if !p.exists() {
            return Ok(p);
        }
    }
    Err(Error::new(ErrorKind::Invalid, "no free file name in the download folder"))
}

/// Downloads a file into `dir` under its (sanitised) name without overwriting anything, and
/// returns the path written. The file is created exclusively, so a race with another writer
/// picks the next free name.
///
/// # Errors
/// Graph failed, the item is a folder, or the file could not be written.
pub async fn download(ctx: &Ctx, drive: &DriveId, item: &DriveItemId, name: &str, dir: &Path) -> Result<PathBuf> {
    let content = ctx.graph.download_item(drive, item).await.map_err(gerr)?;
    let dir = dir.to_owned();
    let name = name.to_owned();
    tokio::task::spawn_blocking(move || write_new(&dir, &name, &content.bytes))
        .await
        .map_err(|_| Error::new(ErrorKind::Bug, "download writer stopped"))?
}

fn write_new(dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf> {
    use std::io::Write as _;
    let io = |e: std::io::Error| Error::new(ErrorKind::Invalid, "could not write the downloaded file").with_source(e);
    std::fs::create_dir_all(dir).map_err(io)?;
    for _ in 0..8 {
        let path = unique_path(dir, name)?;
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                if let Err(e) = f.write_all(bytes).and_then(|()| f.sync_all()) {
                    let _ = std::fs::remove_file(&path);
                    return Err(io(e));
                }
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io(e)),
        }
    }
    Err(Error::new(ErrorKind::Invalid, "no free file name in the download folder"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_made_safe() {
        assert_eq!(safe_file_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(safe_file_name("..."), "download");
        assert_eq!(safe_file_name(""), "download");
        assert_eq!(safe_file_name("a\u{0}b\nc.txt"), "a_b_c.txt");
        assert_eq!(safe_file_name(".hidden"), "hidden");
        let long = format!("{}.docx", "x".repeat(500));
        let s = safe_file_name(&long);
        assert!(s.len() <= MAX_FILE_NAME_BYTES && s.ends_with(".docx"), "{s}");
        let wide = "é".repeat(300);
        assert!(safe_file_name(&wide).len() <= MAX_FILE_NAME_BYTES);
    }

    #[test]
    fn extension_split() {
        assert_eq!(split_ext("a.tar.gz"), ("a.tar", ".gz"));
        assert_eq!(split_ext("README"), ("README", ""));
        assert_eq!(split_ext(".env"), (".env", ""));
    }
}
