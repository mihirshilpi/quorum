//! OneDrive and SharePoint files.
//!
//! Commands (writes go through the outbox with an optimistic local change; programmatic calls
//! never open dialogs): `files.open`, `files.open_in_browser`, `files.open_in_desktop`,
//! `files.download`, `files.upload`, `files.new_folder`, `files.rename`, `files.move`,
//! `files.copy`, `files.delete`, `files.share`, `files.copy_link`, `files.unshare`,
//! `files.versions`, `files.restore_version`, `files.checkout`, `files.checkin`.
//!
//! Queries: `files.locations`, `files.children`, `files.item`, `files.preview_url`,
//! `files.search`, `files.versions`, `files.uploads`.
//!
//! Items are addressed by `"<drive id>|<item id>"` (the `id` of every file row). Commands
//! accept that as `item` or `id`, or a bare item id with `drive`. Locations are `mine`,
//! `recent`, `shared` or `drive:<drive id>` (a site library).
//!
//! Opening things never shows a dialog: Office files open in the desktop app through the
//! Office URI schemes (`ms-word:ofe|u|<url>`, `ms-excel:`, `ms-powerpoint:`, `ms-visio:`,
//! `ms-project:`, `ms-onenote:`), handed to the OS (never in mock mode); the engine then emits
//! `CoreEvent::Ui` `files.launch` (`{url, target: "desktop" | "browser", opened}`). Other files
//! and folders emit `files.show` (`{id, folder, preview?}`; the UI navigates or previews).
//!
//! File rows: `{id, drive, item, name, folder, child_count, ext, kind, size, modified,
//! modified_label, modified_by, shared, web_url, parent, pending}`.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use quorum_graph::api::files::validate_name;
use quorum_ipc::CoreEvent;
use quorum_model::{DriveId, DriveItem, DriveItemId, FileFacet, FolderFacet, MailAddress, Sensitive};
use quorum_store::{Cursor, NewOutboxEntry, SearchScope, Store, Tx};
use quorum_sync::Ctx;
use quorum_sync::files::handlers::{
    CheckPayload, CopyPayload, CreateFolderPayload, DeletePayload, DeletePermissionPayload, InvitePayload,
    RestoreVersionPayload, ShareLinkPayload, UpdatePayload, UploadPayload, link_kind,
};
use quorum_sync::files::state::{self as fstate, UploadProgress, UploadState};
use quorum_sync::files::{self as sf, kinds};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::command::{CommandFuture, CommandSpec, RunFn, always, parse};
use crate::error::{Error, ErrorKind, Result};
use crate::session::Session;

/// Separator between drive id and item id in row ids.
pub(crate) const SEP: char = '|';
/// Most rows a children listing loads from the store.
const MAX_CHILDREN: usize = 10_000;
/// Default page of a children listing.
const DEFAULT_LIMIT: usize = 5_000;
/// Parents walked for a breadcrumb.
const MAX_DEPTH: usize = 64;
/// A drive opened after this long without sync is synced again.
const STALE_DRIVE: Duration = Duration::from_secs(60);
/// Recent / shared refresh when older than this.
const STALE_VIEWS: Duration = Duration::from_secs(300);
/// Sharing / version details refresh when older than this.
const STALE_META: Duration = Duration::from_secs(120);
/// Longest a query waits for Graph.
const REMOTE_TIMEOUT: Duration = Duration::from_secs(5);
/// Most files in one upload command.
const MAX_UPLOAD_FILES: usize = 100;
/// Largest file sent inline (base64 `data`) by the UI.
const MAX_INLINE_UPLOAD: usize = 64 * 1024 * 1024;
/// Most items in one delete.
const MAX_DELETE: usize = 500;
/// Longest search text.
const MAX_QUERY_CHARS: usize = 255;
/// Longest accepted local path.
const MAX_PATH_LEN: usize = 4096;

pub(crate) fn store_err(e: quorum_store::Error) -> Error {
    Error::new(ErrorKind::Store, "the local store failed").with_source(e)
}

pub(crate) fn sync_err(e: quorum_sync::Error) -> Error {
    let kind = match e.kind() {
        quorum_sync::ErrorKind::Store => ErrorKind::Store,
        quorum_sync::ErrorKind::Auth => ErrorKind::Auth,
        quorum_sync::ErrorKind::Invalid => ErrorKind::InvalidParams,
        quorum_sync::ErrorKind::Bug => ErrorKind::Bug,
        _ => ErrorKind::Remote,
    };
    let msg = e.message().to_owned();
    Error::new(kind, msg).with_source(e)
}

fn spec(
    id: &'static str,
    label: &'static str,
    menu: Option<&'static str>,
    shortcut: Option<&'static str>,
    params_doc: &'static str,
    destructive: bool,
    run: RunFn,
) -> CommandSpec {
    CommandSpec { id, label, menu, shortcut, params_doc, destructive, enabled: always, run }
}

const ITEM_DOC: &str = r#"{"item": "<drive>|<item>"} (or "id"; or a bare item id with "drive")"#;

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        spec("files.open", "Open", Some("Files/Open"), None, ITEM_DOC, false, open),
        spec(
            "files.open_in_browser",
            "Open in Browser",
            Some("Files/Open in Browser"),
            None,
            ITEM_DOC,
            false,
            open_in_browser,
        ),
        spec(
            "files.open_in_desktop",
            "Open in Desktop App",
            Some("Files/Open in Desktop App"),
            None,
            ITEM_DOC,
            false,
            open_in_desktop,
        ),
        spec(
            "files.download",
            "Download",
            Some("Files/Download"),
            Some("Cmd+Shift+D"),
            r#"{"item": string, "dest"?: absolute folder (default ~/Downloads)} → {"path"}; never overwrites: adds " (1)""#,
            false,
            download,
        ),
        spec(
            "files.upload",
            "Upload",
            Some("Files/Upload"),
            Some("Cmd+U"),
            r#"{"paths"?: [absolute path], "files"?: [{"name", "data": base64}], "folder"?: "<drive>|<item>" | location (default: My files root), "replace"?: bool}"#,
            false,
            upload,
        ),
        spec(
            "files.new_folder",
            "New Folder",
            Some("Files/New Folder"),
            Some("Cmd+Shift+N"),
            r#"{"folder"?: "<drive>|<item>" | location (default: My files root), "name"?: string (default "New folder")}"#,
            false,
            new_folder,
        ),
        spec(
            "files.rename",
            "Rename",
            Some("Files/Rename"),
            None,
            r#"{"item": string, "name": string}"#,
            false,
            rename,
        ),
        spec(
            "files.move",
            "Move To",
            Some("Files/Move To"),
            Some("Cmd+Shift+M"),
            r#"{"item": string, "folder": "<drive>|<item>" | location}"#,
            false,
            move_item,
        ),
        spec(
            "files.copy",
            "Copy To",
            Some("Files/Copy To"),
            None,
            r#"{"item": string, "folder": "<drive>|<item>" | location, "name"?: string}"#,
            false,
            copy_item,
        ),
        spec(
            "files.delete",
            "Delete",
            Some("Files/Delete"),
            Some("Cmd+Backspace"),
            r#"{"item": string} or {"items": [string]} (to the recycle bin)"#,
            true,
            delete,
        ),
        spec(
            "files.share",
            "Share",
            Some("Files/Share"),
            None,
            r#"{"item": string, "type"?: "view"|"edit" (view), "scope"?: "organization"|"users"|"anonymous" (organization), "recipients"?: [email], "message"?: string}"#,
            true,
            share,
        ),
        spec(
            "files.copy_link",
            "Copy Link",
            Some("Files/Copy Link"),
            Some("Cmd+Shift+C"),
            r#"{"item": string, "type"?: "view"|"edit", "scope"?: "organization"|"users"|"anonymous"} → {"url"} (creates the link if needed)"#,
            true,
            copy_link,
        ),
        spec("files.unshare", "Remove Access", None, None, r#"{"item": string, "permission": string}"#, true, unshare),
        spec("files.versions", "Version History", Some("Files/Version History"), None, ITEM_DOC, false, versions),
        spec(
            "files.restore_version",
            "Restore Version",
            None,
            None,
            r#"{"item": string, "version": string}"#,
            false,
            restore_version,
        ),
        spec("files.checkout", "Check Out", Some("Files/Check Out"), None, ITEM_DOC, false, checkout),
        spec(
            "files.checkin",
            "Check In",
            Some("Files/Check In"),
            None,
            r#"{"item": string, "comment"?: string}"#,
            false,
            checkin,
        ),
    ]
}

pub(crate) fn queries() -> Vec<CommandSpec> {
    vec![
        spec("files.locations", "File Locations", None, None, "{}", false, locations),
        spec(
            "files.children",
            "Folder Contents",
            None,
            None,
            r#"{"location"?: "mine"|"recent"|"shared"|"drive:<id>", "folder"?: "<drive>|<item>", "sort"?: "name"|"modified"|"size"|"modified_by", "desc"?: bool, "after"?: cursor, "limit"?: int}"#,
            false,
            children,
        ),
        spec("files.item", "File Details", None, None, ITEM_DOC, false, item_details),
        spec("files.preview_url", "File Preview URL", None, None, ITEM_DOC, false, preview_url),
        spec("files.search", "Search Files", None, None, r#"{"query": string}"#, false, search),
        spec("files.versions", "Version History", None, None, ITEM_DOC, false, versions),
        spec("files.uploads", "Uploads", None, None, "{}", false, uploads),
    ]
}

// ---------------------------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------------------------

/// The row id of an item.
pub(crate) fn compose(d: &DriveId, i: &DriveItemId) -> String {
    format!("{}{SEP}{}", d.as_str(), i.as_str())
}

/// Splits a row id into drive and item.
pub(crate) fn split(s: &str) -> Option<(DriveId, DriveItemId)> {
    let (d, i) = s.split_once(SEP)?;
    Some((DriveId::new(d).ok()?, DriveItemId::new(i).ok()?))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ItemSel {
    item: Option<String>,
    id: Option<String>,
    drive: Option<String>,
}

/// Resolves the item a command addresses.
async fn resolve_item(s: &Session, p: &Value) -> Result<(DriveId, DriveItemId)> {
    let sel: ItemSel = parse(p.clone())?;
    let raw = sel.item.or(sel.id).ok_or_else(|| Error::params("`item` is required"))?;
    if let Some(found) = split(&raw) {
        return Ok(found);
    }
    let item = DriveItemId::new(raw).map_err(|_| Error::params("`item` is not a valid item id"))?;
    let drive = match sel.drive {
        Some(d) => DriveId::new(d).map_err(|_| Error::params("`drive` is not a valid drive id"))?,
        None => fstate::my_drive(&s.store()?)
            .await
            .map_err(sync_err)?
            .ok_or_else(|| Error::params("`drive` is required"))?,
    };
    Ok((drive, item))
}

async fn get_item(store: &Store, d: &DriveId, i: &DriveItemId) -> Result<Option<DriveItem>> {
    let (d, i) = (d.clone(), i.clone());
    store.read(move |r| r.drive_item(&d, &i)).await.map_err(store_err)
}

async fn require_item(store: &Store, d: &DriveId, i: &DriveItemId) -> Result<DriveItem> {
    get_item(store, d, i).await?.ok_or_else(|| Error::new(ErrorKind::NotFound, "that file is not in the cache"))
}

/// The extension of a name, lower-cased, without the dot.
pub(crate) fn extension(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 16 => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// A coarse file kind for icons: `folder`, `word`, `excel`, `powerpoint`, `onenote`, `visio`,
/// `project`, `pdf`, `image`, `video`, `audio`, `archive`, `code`, `text` or `file`.
pub fn file_kind(name: &str, folder: bool) -> &'static str {
    if folder {
        return "folder";
    }
    match extension(name).as_str() {
        "doc" | "docx" | "docm" | "dot" | "dotx" | "dotm" | "rtf" | "odt" => "word",
        "xls" | "xlsx" | "xlsm" | "xlsb" | "xlt" | "xltx" | "xltm" | "ods" | "csv" => "excel",
        "ppt" | "pptx" | "pptm" | "pot" | "potx" | "potm" | "pps" | "ppsx" | "ppsm" | "odp" => "powerpoint",
        "one" | "onetoc2" => "onenote",
        "vsd" | "vsdx" | "vsdm" | "vssx" | "vstx" => "visio",
        "mpp" | "mpt" => "project",
        "pdf" => "pdf",
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "heic" | "webp" | "svg" | "tif" | "tiff" => "image",
        "mp4" | "mov" | "m4v" | "avi" | "wmv" | "mkv" | "webm" => "video",
        "mp3" | "wav" | "m4a" | "aac" | "flac" | "ogg" => "audio",
        "zip" | "7z" | "rar" | "gz" | "tar" | "tgz" => "archive",
        "rs" | "js" | "ts" | "py" | "json" | "xml" | "html" | "css" | "sh" | "yml" | "yaml" | "toml" | "c" | "cpp"
        | "java" | "cs" | "go" => "code",
        "txt" | "md" | "log" => "text",
        _ => "file",
    }
}

/// The Office URI scheme for a file name, if an Office desktop app opens it.
pub fn office_scheme(name: &str) -> Option<&'static str> {
    match extension(name).as_str() {
        "doc" | "docx" | "docm" | "dot" | "dotx" | "dotm" | "rtf" | "odt" => Some("ms-word"),
        "xls" | "xlsx" | "xlsm" | "xlsb" | "xlt" | "xltx" | "xltm" | "ods" => Some("ms-excel"),
        "ppt" | "pptx" | "pptm" | "pot" | "potx" | "potm" | "pps" | "ppsx" | "ppsm" | "odp" => Some("ms-powerpoint"),
        "vsd" | "vsdx" | "vsdm" | "vssx" | "vstx" => Some("ms-visio"),
        "mpp" | "mpt" => Some("ms-project"),
        "one" | "onetoc2" => Some("ms-onenote"),
        _ => None,
    }
}

/// Whether `url` is a plain https URL safe to embed in a launch URI.
fn safe_web_url(url: &str) -> bool {
    url.len() <= 4096
        && url.get(..8).is_some_and(|p| p.eq_ignore_ascii_case("https://"))
        && !url.chars().any(|c| c.is_control() || c.is_whitespace() || c == '|' || c == '"' || c == '<' || c == '>')
}

/// The "open for editing" Office URI (`ms-word:ofe|u|<url>`) for a file, if any.
pub fn desktop_uri(name: &str, web_url: &str) -> Option<String> {
    let scheme = office_scheme(name)?;
    safe_web_url(web_url).then(|| format!("{scheme}:ofe|u|{web_url}"))
}

/// A short local-time label: `14:05` today, `Oct 12` this year, `Oct 12, 2024` otherwise.
pub fn time_label(ts: Option<Timestamp>, now: Timestamp, tz: &TimeZone) -> String {
    let Some(ts) = ts else { return String::new() };
    let (z, n) = (ts.to_zoned(tz.clone()), now.to_zoned(tz.clone()));
    if z.date() == n.date() {
        z.strftime("%H:%M").to_string()
    } else if z.year() == n.year() {
        z.strftime("%b %-d").to_string()
    } else {
        z.strftime("%b %-d, %Y").to_string()
    }
}

/// The row shape of a drive item.
pub(crate) fn row(it: &DriveItem, now: Timestamp, tz: &TimeZone) -> Value {
    let name = it.name.as_str();
    json!({
        "id": compose(&it.drive_id, &it.id),
        "drive": it.drive_id,
        "item": it.id,
        "name": name,
        "folder": it.is_folder(),
        "child_count": it.folder.as_ref().map_or(0, |f| f.child_count),
        "ext": extension(name),
        "kind": file_kind(name, it.is_folder()),
        "size": it.size,
        "modified": it.last_modified_date_time,
        "modified_label": time_label(it.last_modified_date_time, now, tz),
        "modified_by": it.last_modified_by.as_ref().map(|i| i.display_name.as_str()).unwrap_or_default(),
        "shared": it.shared,
        "web_url": it.web_url,
        "parent": it.parent_id.as_ref().map(|p| compose(&it.drive_id, p)),
        "pending": it.id.as_str().starts_with(LOCAL_PREFIX),
        "mime": it.mime_type(),
    })
}

/// Id prefix of optimistic placeholders.
pub(crate) const LOCAL_PREFIX: &str = "local-";

/// A fresh operation id (unique within this process run and across restarts).
pub(crate) fn op_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("{}-{n}", Timestamp::now().as_millisecond())
}

/// Background syncs in flight, so opening a folder repeatedly starts one pass.
fn in_flight() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Runs `f` in the background unless a task with `key` is already running.
fn spawn_once<F>(key: String, f: F)
where
    F: std::future::Future<Output = quorum_sync::Result<()>> + Send + 'static,
{
    {
        let mut set = in_flight().lock().unwrap_or_else(PoisonError::into_inner);
        if !set.insert(key.clone()) {
            return;
        }
    }
    tokio::spawn(async move {
        if let Err(e) = f.await {
            tracing::info!(target: "quorum::files", kind = ?e.kind(), "background files refresh failed");
        }
        in_flight().lock().unwrap_or_else(PoisonError::into_inner).remove(&key);
    });
}

async fn is_stale(store: &Store, resource: &str, max_age: Duration) -> bool {
    match fstate::synced_at(store, resource).await {
        Ok(Some(at)) => {
            let age = state_age(at);
            age > max_age
        }
        _ => true,
    }
}

fn state_age(at_ms: i64) -> Duration {
    let ms = fstate::now_ms().saturating_sub(at_ms);
    Duration::from_millis(u64::try_from(ms).unwrap_or(0))
}

fn ctx(s: &Session) -> Result<Ctx> {
    Ok(s.sync()?.ctx().clone())
}

/// Starts a background delta pass of `drive` if it has not synced recently.
async fn kick_drive(s: &Session, drive: &DriveId) -> Result<()> {
    let store = s.store()?;
    let key = sf::drive_resource(drive);
    if is_stale(&store, &key, STALE_DRIVE).await {
        let c = ctx(s)?;
        let d = drive.clone();
        spawn_once(key, async move { sf::sync_drive(&c, &d, sf::MAX_DELTA_PAGES).await.map(|_| ()) });
    }
    Ok(())
}

/// The user's OneDrive id; when unknown, starts discovering it (and its first sync).
async fn my_drive_or_kick(s: &Session) -> Result<Option<DriveId>> {
    let store = s.store()?;
    if let Some(d) = fstate::my_drive(&store).await.map_err(sync_err)? {
        return Ok(Some(d));
    }
    let c = ctx(s)?;
    spawn_once(sf::MY_DRIVE_KEY.to_owned(), async move {
        let d = sf::ensure_my_drive(&c).await?;
        sf::sync_drive(&c, &d, sf::MAX_DELTA_PAGES).await.map(|_| ())
    });
    Ok(None)
}

/// A folder target: `"<drive>|<item>"`, a location (`mine`, `drive:<id>`: its root) or the
/// default (My files root).
async fn resolve_folder(s: &Session, folder: Option<&str>) -> Result<(DriveId, DriveItemId)> {
    let store = s.store()?;
    let drive = match folder {
        Some(f) if f.contains(SEP) => {
            let (d, i) = split(f).ok_or_else(|| Error::params("`folder` is not a valid folder id"))?;
            if !i.as_str().starts_with(LOCAL_PREFIX) {
                let it = require_item(&store, &d, &i).await?;
                if !it.is_folder() {
                    return Err(Error::params("the target is not a folder"));
                }
            } else {
                return Err(Error::new(ErrorKind::NotAvailable, "that folder is still being created"));
            }
            return Ok((d, i));
        }
        Some(f) if f.starts_with("drive:") => {
            DriveId::new(f.trim_start_matches("drive:")).map_err(|_| Error::params("invalid drive"))?
        }
        None | Some("mine") => my_drive_or_kick(s)
            .await?
            .ok_or_else(|| Error::new(ErrorKind::NotAvailable, "My files has not synced yet"))?,
        Some(_) => return Err(Error::params("`folder` must be a folder id or a location")),
    };
    let root = fstate::root_of(&store, &drive)
        .await
        .map_err(sync_err)?
        .ok_or_else(|| Error::new(ErrorKind::NotAvailable, "this library has not synced yet"))?;
    Ok((drive, root))
}

/// Enqueues an outbox entry together with `apply` (the optimistic change) in one transaction,
/// then wakes the replayer.
async fn enqueue<F>(
    s: &Session,
    kind: &str,
    resource: String,
    local_id: Option<String>,
    payload: Value,
    apply: F,
) -> Result<i64>
where
    F: FnOnce(&mut Tx<'_>) -> quorum_store::Result<()> + Send + 'static,
{
    let store = s.store()?;
    let entry = NewOutboxEntry { kind: kind.to_owned(), resource, local_id, payload };
    let id = store
        .write(move |tx| {
            apply(tx)?;
            tx.outbox_enqueue(&entry)
        })
        .await
        .map_err(store_err)?;
    s.sync()?.outbox_changed();
    Ok(id.0)
}

fn to_value<T: serde::Serialize>(v: &T) -> Result<Value> {
    serde_json::to_value(v).map_err(|e| Error::new(ErrorKind::Bug, "payload encoding failed").with_source(e))
}

fn check_name(name: &str) -> Result<String> {
    let n = name.trim();
    validate_name(n)
        .map(str::to_owned)
        .map_err(|_| Error::params("names cannot be empty, end in a space or contain \" * : < > ? / \\ |"))
}

fn placeholder(
    drive: &DriveId,
    parent: &DriveItemId,
    id: DriveItemId,
    name: &str,
    folder: bool,
    size: u64,
) -> DriveItem {
    DriveItem {
        id,
        drive_id: drive.clone(),
        parent_id: Some(parent.clone()),
        name: Sensitive::new(name),
        size,
        file: (!folder).then(FileFacet::default),
        folder: folder.then(FolderFacet::default),
        last_modified_by: None,
        last_modified_date_time: Some(Timestamp::now()),
        created_date_time: Some(Timestamp::now()),
        web_url: None,
        e_tag: None,
        c_tag: None,
        shared: false,
    }
}

fn emit_ui(s: &Session, command: &str, params: Value) {
    s.emit(CoreEvent::Ui { command: command.to_owned(), params });
}

/// Hands a checked `https` or Office URI to the operating system, except in mock mode (tests
/// and previews never launch apps). Then tells the UI (`files.launch`) so it can show progress
/// or, in a browser preview, open the page itself.
fn launch(s: &Session, url: &str, target: &str) -> Result<bool> {
    let opened = if s.options().mock_graph {
        false
    } else {
        let mut cmd = if cfg!(target_os = "macos") {
            std::process::Command::new("/usr/bin/open")
        } else if cfg!(target_os = "windows") {
            let mut c = std::process::Command::new("rundll32");
            c.arg("url.dll,FileProtocolHandler");
            c
        } else {
            std::process::Command::new("xdg-open")
        };
        cmd.arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let child =
            cmd.spawn().map_err(|e| Error::new(ErrorKind::NotAvailable, "cannot open the app").with_source(e))?;
        // Reap the launcher so it does not linger.
        std::thread::spawn(move || {
            let mut child = child;
            let _ = child.wait();
        });
        true
    };
    emit_ui(s, "files.launch", json!({ "url": url, "target": target, "opened": opened }));
    Ok(opened)
}

// ---------------------------------------------------------------------------------------------
// Opening.
// ---------------------------------------------------------------------------------------------

fn open(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let (d, i) = resolve_item(s, &p).await?;
        let it = require_item(&s.store()?, &d, &i).await?;
        let id = compose(&d, &i);
        if it.is_folder() {
            emit_ui(s, "files.show", json!({ "id": id, "folder": true }));
            return Ok(json!({ "action": "navigate", "id": id }));
        }
        if let Some(uri) = it.web_url.as_deref().and_then(|u| desktop_uri(it.name.as_str(), u)) {
            let opened = launch(s, &uri, "desktop")?;
            return Ok(json!({ "action": "desktop", "url": uri, "opened": opened }));
        }
        emit_ui(s, "files.show", json!({ "id": id, "folder": false, "preview": true }));
        Ok(json!({ "action": "preview", "id": id }))
    })
}

fn open_in_browser(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let (d, i) = resolve_item(s, &p).await?;
        let it = require_item(&s.store()?, &d, &i).await?;
        let url = it
            .web_url
            .filter(|u| safe_web_url(u))
            .ok_or_else(|| Error::new(ErrorKind::NotAvailable, "this item has no web address yet"))?;
        let opened = launch(s, &url, "browser")?;
        Ok(json!({ "url": url, "opened": opened }))
    })
}

fn open_in_desktop(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let (d, i) = resolve_item(s, &p).await?;
        let it = require_item(&s.store()?, &d, &i).await?;
        let url = it.web_url.as_deref().unwrap_or_default();
        let uri = desktop_uri(it.name.as_str(), url)
            .ok_or_else(|| Error::params("no Office desktop app opens this file type"))?;
        let opened = launch(s, &uri, "desktop")?;
        Ok(json!({ "url": uri, "opened": opened }))
    })
}

// ---------------------------------------------------------------------------------------------
// Download and upload.
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DownloadParams {
    dest: Option<String>,
}

fn downloads_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| Error::new(ErrorKind::NotAvailable, "no home folder to download into"))?;
    Ok(PathBuf::from(home).join("Downloads"))
}

fn check_local_path(raw: &str) -> Result<PathBuf> {
    if raw.is_empty() || raw.len() > MAX_PATH_LEN || raw.contains('\0') {
        return Err(Error::params("invalid path"));
    }
    let p = PathBuf::from(raw);
    if !p.is_absolute() {
        return Err(Error::params("paths must be absolute"));
    }
    Ok(p)
}

fn download(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let params: DownloadParams = parse(p.clone())?;
        let (d, i) = resolve_item(s, &p).await?;
        let dir = match params.dest.as_deref() {
            Some(dest) => check_local_path(dest)?,
            None => downloads_dir()?,
        };
        if dir.exists() && !dir.is_dir() {
            return Err(Error::params("`dest` must be a folder"));
        }
        let store = s.store()?;
        let it = get_item(&store, &d, &i).await?;
        if it.as_ref().is_some_and(DriveItem::is_folder) {
            return Err(Error::params("folders cannot be downloaded yet"));
        }
        let name = it.as_ref().map(|x| x.name.as_str().to_owned()).unwrap_or_else(|| "download".to_owned());
        let path = sf::download(&ctx(s)?, &d, &i, &name, &dir).await.map_err(sync_err)?;
        let shown = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        s.emit(CoreEvent::Toast { level: "success".into(), message: format!("Downloaded {shown}") });
        Ok(json!({ "path": path }))
    })
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct InlineFile {
    name: String,
    data: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct UploadParams {
    paths: Vec<String>,
    files: Vec<InlineFile>,
    folder: Option<String>,
    replace: bool,
}

/// Where inline (`data`) uploads are staged until the outbox sends them.
fn staging_dir(store: &Store) -> PathBuf {
    store.dir().map_or_else(|| std::env::temp_dir().join("quorum-uploads"), |d| d.join("uploads"))
}

async fn stage(store: &Store, op: &str, name: &str, data: &str) -> Result<PathBuf> {
    if data.len() > MAX_INLINE_UPLOAD.saturating_mul(4) / 3 + 4 {
        return Err(Error::params("inline uploads are limited to 64 MiB; pass a path instead"));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .map_err(|_| Error::params("`data` must be base64"))?;
    let dir = staging_dir(store);
    let path = dir.join(format!("{op}.bin"));
    let io = |e: std::io::Error| Error::new(ErrorKind::Store, "could not stage the upload").with_source(e);
    tokio::fs::create_dir_all(&dir).await.map_err(io)?;
    tokio::fs::write(&path, bytes).await.map_err(io)?;
    tracing::debug!(target: "quorum::files", len = name.len(), "staged inline upload");
    Ok(path)
}

fn upload(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let params: UploadParams = parse(p)?;
        let total = params.paths.len().saturating_add(params.files.len());
        if total == 0 {
            return Err(Error::params("`paths` or `files` is required"));
        }
        if total > MAX_UPLOAD_FILES {
            return Err(Error::params("at most 100 files per upload"));
        }
        let (drive, parent) = resolve_folder(s, params.folder.as_deref()).await?;
        let store = s.store()?;
        // Validate everything before queuing anything.
        let mut jobs: Vec<(PathBuf, String, u64, String, bool)> = Vec::with_capacity(total);
        for raw in &params.paths {
            let path = check_local_path(raw)?;
            let meta =
                tokio::fs::metadata(&path).await.map_err(|_| Error::params("a file to upload does not exist"))?;
            if !meta.is_file() {
                return Err(Error::params("only files can be uploaded (not folders)"));
            }
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            jobs.push((path, check_name(&name)?, meta.len(), op_id(), false));
        }
        for f in &params.files {
            let name = check_name(&f.name)?;
            let op = op_id();
            let path = stage(&store, &op, &name, &f.data).await?;
            let len = tokio::fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
            jobs.push((path, name, len, op, true));
        }
        let mut queued = Vec::with_capacity(jobs.len());
        for (path, name, size, op, staged) in jobs {
            let local =
                DriveItemId::new(format!("{LOCAL_PREFIX}{op}")).map_err(|_| Error::new(ErrorKind::Bug, "bad id"))?;
            let payload = UploadPayload {
                drive: drive.clone(),
                parent: parent.clone(),
                path,
                name: name.clone(),
                op: op.clone(),
                local_id: local.clone(),
                replace: params.replace,
                staged,
            };
            let ph = placeholder(&drive, &parent, local.clone(), &name, false, size);
            let progress = UploadProgress {
                name: name.clone(),
                total: size,
                sent: 0,
                state: UploadState::Queued,
                updated: fstate::now_ms(),
            };
            let op2 = op.clone();
            let id = enqueue(
                s,
                kinds::UPLOAD,
                sf::drive_resource(&drive),
                Some(local.as_str().to_owned()),
                to_value(&payload)?,
                move |tx| {
                    tx.upsert_drive_items(&[ph])?;
                    fstate::put_upload(tx, &op2, progress)
                },
            )
            .await?;
            queued.push(json!({ "op": op, "outbox": id, "id": compose(&drive, &local), "name": name, "size": size }));
        }
        Ok(json!({ "queued": queued }))
    })
}

fn uploads(s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let all = fstate::uploads(&s.store()?).await.map_err(sync_err)?;
        let mut rows: Vec<Value> = all
            .into_iter()
            .map(|(op, u)| json!({ "op": op, "name": u.name, "total": u.total, "sent": u.sent, "state": u.state, "updated": u.updated }))
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.get("updated").and_then(Value::as_i64).unwrap_or(0)));
        Ok(Value::Array(rows))
    })
}

// ---------------------------------------------------------------------------------------------
// Folder, rename, move, copy, delete.
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct NewFolderParams {
    folder: Option<String>,
    name: Option<String>,
}

fn new_folder(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let params: NewFolderParams = parse(p)?;
        let name = check_name(params.name.as_deref().unwrap_or("New folder"))?;
        let (drive, parent) = resolve_folder(s, params.folder.as_deref()).await?;
        let op = op_id();
        let local =
            DriveItemId::new(format!("{LOCAL_PREFIX}{op}")).map_err(|_| Error::new(ErrorKind::Bug, "bad id"))?;
        let payload = CreateFolderPayload {
            drive: drive.clone(),
            parent: parent.clone(),
            name: name.clone(),
            local_id: local.clone(),
        };
        let ph = placeholder(&drive, &parent, local.clone(), &name, true, 0);
        let id = enqueue(
            s,
            kinds::CREATE_FOLDER,
            sf::drive_resource(&drive),
            Some(local.as_str().to_owned()),
            to_value(&payload)?,
            move |tx| tx.upsert_drive_items(&[ph]),
        )
        .await?;
        Ok(json!({ "outbox": id, "id": compose(&drive, &local), "name": name }))
    })
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RenameParams {
    name: Option<String>,
}

fn not_pending(i: &DriveItemId) -> Result<()> {
    if i.as_str().starts_with(LOCAL_PREFIX) {
        return Err(Error::new(ErrorKind::NotAvailable, "that item is still being created"));
    }
    Ok(())
}

fn rename(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let params: RenameParams = parse(p.clone())?;
        let name = check_name(params.name.as_deref().ok_or_else(|| Error::params("`name` is required"))?)?;
        let (d, i) = resolve_item(s, &p).await?;
        not_pending(&i)?;
        let store = s.store()?;
        let prev = require_item(&store, &d, &i).await?;
        let mut next = prev.clone();
        next.name = Sensitive::new(name.clone());
        let payload = UpdatePayload {
            drive: d.clone(),
            item: i.clone(),
            name: Some(name),
            parent: None,
            parent_drive: None,
            prev: Some(prev),
        };
        let id = enqueue(s, kinds::UPDATE, sf::drive_resource(&d), None, to_value(&payload)?, move |tx| {
            tx.upsert_drive_items(&[next])
        })
        .await?;
        Ok(json!({ "outbox": id }))
    })
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DestParams {
    folder: Option<String>,
    name: Option<String>,
}

/// Whether `folder` is `item` or below it (a move there would orphan the subtree).
async fn is_within(store: &Store, drive: &DriveId, folder: &DriveItemId, item: &DriveItemId) -> Result<bool> {
    let mut cur = Some(folder.clone());
    for _ in 0..MAX_DEPTH {
        let Some(c) = cur else { return Ok(false) };
        if c == *item {
            return Ok(true);
        }
        cur = get_item(store, drive, &c).await?.and_then(|x| x.parent_id);
    }
    Ok(false)
}

fn move_item(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let params: DestParams = parse(p.clone())?;
        let target = params.folder.as_deref().ok_or_else(|| Error::params("`folder` is required"))?;
        let (d, i) = resolve_item(s, &p).await?;
        not_pending(&i)?;
        let (dd, df) = resolve_folder(s, Some(target)).await?;
        let store = s.store()?;
        let prev = require_item(&store, &d, &i).await?;
        if dd == d && is_within(&store, &d, &df, &i).await? {
            return Err(Error::params("a folder cannot be moved into itself"));
        }
        if prev.parent_id.as_ref() == Some(&df) && dd == d {
            return Ok(json!({ "outbox": null, "unchanged": true }));
        }
        let payload = UpdatePayload {
            drive: d.clone(),
            item: i.clone(),
            name: None,
            parent: Some(df.clone()),
            parent_drive: (dd != d).then(|| dd.clone()),
            prev: Some(prev.clone()),
        };
        let same_drive = dd == d;
        let (d2, i2) = (d.clone(), i.clone());
        let id = enqueue(s, kinds::UPDATE, sf::drive_resource(&d), None, to_value(&payload)?, move |tx| {
            if same_drive {
                let mut next = prev;
                next.parent_id = Some(df);
                tx.upsert_drive_items(&[next])
            } else {
                tx.delete_drive_items(&d2, &[i2])
            }
        })
        .await?;
        Ok(json!({ "outbox": id }))
    })
}

fn copy_item(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let params: DestParams = parse(p.clone())?;
        let target = params.folder.as_deref().ok_or_else(|| Error::params("`folder` is required"))?;
        let name = params.name.as_deref().map(check_name).transpose()?;
        let (d, i) = resolve_item(s, &p).await?;
        not_pending(&i)?;
        let (dd, df) = resolve_folder(s, Some(target)).await?;
        let payload =
            CopyPayload { drive: d.clone(), item: i, dest_drive: dd.clone(), dest_folder: df, name, op: op_id() };
        let id = enqueue(s, kinds::COPY, sf::drive_resource(&dd), None, to_value(&payload)?, |_| Ok(())).await?;
        Ok(json!({ "outbox": id }))
    })
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DeleteParams {
    items: Vec<String>,
}

fn delete(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let params: DeleteParams = parse(p.clone())?;
        let targets: Vec<(DriveId, DriveItemId)> = if params.items.is_empty() {
            vec![resolve_item(s, &p).await?]
        } else {
            if params.items.len() > MAX_DELETE {
                return Err(Error::params("at most 500 items per delete"));
            }
            params
                .items
                .iter()
                .map(|x| split(x).ok_or_else(|| Error::params("`items` must be \"<drive>|<item>\" ids")))
                .collect::<Result<_>>()?
        };
        let store = s.store()?;
        let mut queued = Vec::new();
        for (d, i) in targets {
            not_pending(&i)?;
            let prev = get_item(&store, &d, &i).await?;
            let payload = DeletePayload { drive: d.clone(), item: i.clone(), prev };
            let (d2, i2) = (d.clone(), i.clone());
            let id = enqueue(s, kinds::DELETE, sf::drive_resource(&d), None, to_value(&payload)?, move |tx| {
                tx.delete_drive_items(&d2, &[i2])
            })
            .await?;
            queued.push(id);
        }
        Ok(json!({ "outbox": queued }))
    })
}

// ---------------------------------------------------------------------------------------------
// Sharing, versions, check-in/out.
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ShareParams {
    r#type: Option<String>,
    scope: Option<String>,
    recipients: Vec<String>,
    message: Option<String>,
}

fn share(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let params: ShareParams = parse(p.clone())?;
        let (d, i) = resolve_item(s, &p).await?;
        not_pending(&i)?;
        let link_type = params.r#type.unwrap_or_else(|| "view".to_owned());
        let scope = params.scope.unwrap_or_else(|| "organization".to_owned());
        link_kind(&link_type, &scope).map_err(|e| Error::params(e.message().to_owned()))?;
        let (kind, payload) = if params.recipients.is_empty() {
            (kinds::SHARE_LINK, to_value(&ShareLinkPayload { drive: d.clone(), item: i, link_type, scope })?)
        } else {
            if params.recipients.len() > 100 {
                return Err(Error::params("at most 100 recipients"));
            }
            let recipients = params
                .recipients
                .iter()
                .map(|r| MailAddress::new(r.trim()))
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|_| Error::params("`recipients` must be email addresses"))?;
            let message =
                params.message.map(|m| m.chars().filter(|c| !c.is_control() || *c == '\n').take(2000).collect());
            let inv = InvitePayload {
                drive: d.clone(),
                item: i,
                recipients,
                write: link_type == "edit",
                message,
                send_invitation: true,
            };
            (kinds::INVITE, to_value(&inv)?)
        };
        let id = enqueue(s, kind, sf::drive_resource(&d), None, payload, |_| Ok(())).await?;
        Ok(json!({ "outbox": id }))
    })
}

fn copy_link(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let params: ShareParams = parse(p.clone())?;
        let (d, i) = resolve_item(s, &p).await?;
        not_pending(&i)?;
        let (t, sc) =
            link_kind(params.r#type.as_deref().unwrap_or("view"), params.scope.as_deref().unwrap_or("organization"))
                .map_err(|e| Error::params(e.message().to_owned()))?;
        let url = sf::live::create_link(&ctx(s)?, &d, &i, t, sc).await.map_err(sync_err)?;
        Ok(json!({ "url": url }))
    })
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct UnshareParams {
    permission: Option<String>,
}

fn unshare(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let params: UnshareParams = parse(p.clone())?;
        let permission = params
            .permission
            .filter(|x| !x.is_empty() && x.len() <= 1024 && !x.chars().any(char::is_control))
            .ok_or_else(|| Error::params("`permission` is required"))?;
        let (d, i) = resolve_item(s, &p).await?;
        let payload = DeletePermissionPayload { drive: d.clone(), item: i.clone(), permission: permission.clone() };
        let store = s.store()?;
        // Optimistic: drop it from the cached details.
        let meta = fstate::meta(&store, &d, &i).await.map_err(sync_err)?;
        let key = fstate::meta_key(&d, &i);
        let id = enqueue(s, kinds::DELETE_PERMISSION, sf::drive_resource(&d), None, to_value(&payload)?, move |tx| {
            if let Some(mut m) = meta {
                m.permissions.retain(|x| x.id != permission);
                tx.set_setting(&key, &m)?;
            }
            Ok(())
        })
        .await?;
        Ok(json!({ "outbox": id }))
    })
}

/// Cached details, refreshed from Graph (bounded wait) when stale.
async fn meta_for(
    s: &Session,
    d: &DriveId,
    i: &DriveItemId,
    folder: bool,
    force: bool,
) -> Result<(Option<fstate::ItemMeta>, bool)> {
    let store = s.store()?;
    let cached = fstate::meta(&store, d, i).await.map_err(sync_err)?;
    let fresh = cached.as_ref().is_some_and(|m| state_age(m.fetched_at) < STALE_META);
    if (fresh && !force) || i.as_str().starts_with(LOCAL_PREFIX) {
        return Ok((cached, false));
    }
    let c = ctx(s)?;
    match tokio::time::timeout(REMOTE_TIMEOUT, sf::live::fetch_item_meta(&c, d, i, folder)).await {
        Ok(Ok(m)) => Ok((Some(m), false)),
        _ => Ok((cached, true)),
    }
}

fn versions(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let (d, i) = resolve_item(s, &p).await?;
        let (meta, stale) = meta_for(s, &d, &i, false, false).await?;
        Ok(json!({ "versions": meta.map(|m| m.versions).unwrap_or_default(), "stale": stale }))
    })
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct VersionParams {
    version: Option<String>,
}

fn restore_version(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let params: VersionParams = parse(p.clone())?;
        let version = params
            .version
            .filter(|v| !v.is_empty() && v.len() <= 256 && !v.chars().any(char::is_control))
            .ok_or_else(|| Error::params("`version` is required"))?;
        let (d, i) = resolve_item(s, &p).await?;
        not_pending(&i)?;
        let payload = RestoreVersionPayload { drive: d.clone(), item: i, version };
        let id =
            enqueue(s, kinds::RESTORE_VERSION, sf::drive_resource(&d), None, to_value(&payload)?, |_| Ok(())).await?;
        Ok(json!({ "outbox": id }))
    })
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CheckinParams {
    comment: String,
}

fn checkout(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let (d, i) = resolve_item(s, &p).await?;
        not_pending(&i)?;
        let payload = CheckPayload { drive: d.clone(), item: i, comment: String::new() };
        let id = enqueue(s, kinds::CHECKOUT, sf::drive_resource(&d), None, to_value(&payload)?, |_| Ok(())).await?;
        Ok(json!({ "outbox": id }))
    })
}

fn checkin(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let params: CheckinParams = parse(p.clone())?;
        if params.comment.chars().count() > 1000 {
            return Err(Error::params("check-in comments are limited to 1000 characters"));
        }
        let comment: String = params.comment.chars().filter(|c| !c.is_control() || *c == '\n').collect();
        let (d, i) = resolve_item(s, &p).await?;
        not_pending(&i)?;
        let payload = CheckPayload { drive: d.clone(), item: i, comment };
        let id = enqueue(s, kinds::CHECKIN, sf::drive_resource(&d), None, to_value(&payload)?, |_| Ok(())).await?;
        Ok(json!({ "outbox": id }))
    })
}

// ---------------------------------------------------------------------------------------------
// Queries.
// ---------------------------------------------------------------------------------------------

fn drive_name(d: &quorum_model::Drive) -> String {
    let n = d.name.as_str();
    if n.is_empty() { "Documents".to_owned() } else { n.to_owned() }
}

fn locations(s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let store = s.store()?;
        let mine = my_drive_or_kick(s).await?;
        let mut rows = vec![
            json!({ "id": "mine", "name": "My files", "kind": "mine", "depth": 0, "drive": mine }),
            json!({ "id": "recent", "name": "Recent", "kind": "recent", "depth": 0 }),
            json!({ "id": "shared", "name": "Shared with me", "kind": "shared", "depth": 0 }),
        ];
        let followed = fstate::followed(&store).await.map_err(sync_err)?;
        if followed.is_none() || is_stale(&store, sf::SITES_KEY, Duration::from_secs(3600)).await {
            let c = ctx(s)?;
            spawn_once(sf::SITES_KEY.to_owned(), async move { sf::refresh_sites(&c).await });
        }
        let followed = followed.unwrap_or_default();
        let (sites, drives) = store.read(|r| Ok((r.sites()?.items, r.drives()?.items))).await.map_err(store_err)?;
        let mut site_rows = Vec::new();
        for sid in &followed {
            let Some(site) = sites.iter().find(|x| &x.id == sid) else { continue };
            site_rows.push(json!({
                "id": format!("site:{}", site.id.as_str()),
                "name": site.display_name.as_str(),
                "kind": "site",
                "depth": 1,
                "site": site.id,
                "web_url": site.web_url,
            }));
            for did in fstate::site_drives(&store, sid).await.map_err(sync_err)? {
                let name =
                    drives.iter().find(|x| x.id == did).map(drive_name).unwrap_or_else(|| "Documents".to_owned());
                site_rows.push(json!({
                    "id": format!("drive:{}", did.as_str()),
                    "name": name,
                    "kind": "library",
                    "depth": 2,
                    "site": site.id,
                    "drive": did,
                }));
            }
            let sid2 = sid.clone();
            let lists = store.read(move |r| r.lists(&sid2)).await.map_err(store_err)?;
            for l in lists.items.iter().filter(|l| l.template.as_deref() != Some("documentLibrary")).take(100) {
                site_rows.push(json!({
                    "id": format!("list:{}{SEP}{}", site.id.as_str(), l.id.as_str()),
                    "name": l.display_name.as_str(),
                    "kind": "list",
                    "depth": 2,
                    "site": site.id,
                    "list": l.id,
                    "template": l.template,
                }));
            }
        }
        if !site_rows.is_empty() {
            rows.push(json!({ "id": "sites", "name": "Sites", "kind": "header", "depth": 0 }));
            rows.extend(site_rows);
        }
        Ok(Value::Array(rows))
    })
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ChildrenParams {
    location: Option<String>,
    folder: Option<String>,
    sort: Option<String>,
    desc: bool,
    after: Option<String>,
    limit: Option<u64>,
}

/// Sorts rows: folders first, then by `sort` (name by default), ties by name and id.
pub(crate) fn sort_items(items: &mut [DriveItem], sort: &str, desc: bool) {
    let fold = |s: &str| s.to_lowercase();
    items.sort_by(|a, b| {
        let folders = b.is_folder().cmp(&a.is_folder());
        let by_name = || fold(a.name.as_str()).cmp(&fold(b.name.as_str())).then_with(|| a.id.cmp(&b.id));
        let key = match sort {
            "modified" => a.last_modified_date_time.cmp(&b.last_modified_date_time).then_with(by_name),
            "size" => a.size.cmp(&b.size).then_with(by_name),
            "modified_by" => {
                let who = |x: &DriveItem| {
                    x.last_modified_by.as_ref().map(|i| fold(i.display_name.as_str())).unwrap_or_default()
                };
                who(a).cmp(&who(b)).then_with(by_name)
            }
            _ => by_name(),
        };
        folders.then(if desc { key.reverse() } else { key })
    });
}

async fn load_children(store: &Store, drive: &DriveId, parent: Option<&DriveItemId>) -> Result<Vec<DriveItem>> {
    let mut out: Vec<DriveItem> = Vec::new();
    let mut after: Option<Cursor> = None;
    while out.len() < MAX_CHILDREN {
        let page = store
            .drive_children(drive.clone(), parent.cloned(), after.clone(), quorum_store::MAX_PAGE)
            .await
            .map_err(store_err)?;
        out.extend(page.items);
        match page.next {
            Some(n) => after = Some(n),
            None => break,
        }
    }
    out.truncate(MAX_CHILDREN);
    Ok(out)
}

async fn load_refs(store: &Store, refs: Vec<fstate::ItemRef>) -> Result<Vec<DriveItem>> {
    store
        .read(move |r| {
            let mut out = Vec::with_capacity(refs.len());
            for x in refs {
                if let Some(it) = r.drive_item(&x.drive, &x.id)? {
                    out.push(it);
                }
            }
            Ok(out)
        })
        .await
        .map_err(store_err)
}

async fn breadcrumb(
    store: &Store,
    drive: &DriveId,
    folder: &DriveItemId,
    root: Option<&DriveItemId>,
    root_label: &str,
    root_id: &str,
) -> Result<Vec<Value>> {
    let mut chain = Vec::new();
    let mut cur = Some(folder.clone());
    for _ in 0..MAX_DEPTH {
        let Some(c) = cur else { break };
        if Some(&c) == root {
            break;
        }
        match get_item(store, drive, &c).await? {
            Some(it) => {
                chain.push(json!({ "id": compose(drive, &it.id), "name": it.name.as_str() }));
                cur = it.parent_id;
            }
            None => break,
        }
    }
    chain.push(json!({ "id": root_id, "name": root_label }));
    chain.reverse();
    Ok(chain)
}

fn children(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let params: ChildrenParams = parse(p)?;
        let store = s.store()?;
        let now = Timestamp::now();
        let tz = TimeZone::system();
        let sort = params.sort.as_deref().unwrap_or("name");
        if !matches!(sort, "name" | "modified" | "size" | "modified_by") {
            return Err(Error::params("`sort` must be name, modified, size or modified_by"));
        }
        let limit = usize::try_from(params.limit.unwrap_or(DEFAULT_LIMIT as u64))
            .unwrap_or(DEFAULT_LIMIT)
            .clamp(1, MAX_CHILDREN);
        let offset: usize = match params.after.as_deref() {
            None => 0,
            Some(a) => a.parse().map_err(|_| Error::params("`after` is not a valid cursor"))?,
        };
        let mut syncing = false;
        let (location, drive, folder, mut items, crumbs) = match (params.folder.as_deref(), params.location.as_deref())
        {
            (Some(f), _) => {
                let (d, i) = split(f).ok_or_else(|| Error::params("`folder` is not a valid folder id"))?;
                kick_drive(s, &d).await?;
                let root = fstate::root_of(&store, &d).await.map_err(sync_err)?;
                let mine = fstate::my_drive(&store).await.map_err(sync_err)?;
                let (loc, label) = if mine.as_ref() == Some(&d) {
                    ("mine".to_owned(), "My files".to_owned())
                } else {
                    let name = store
                        .read(|r| Ok(r.drives()?.items))
                        .await
                        .map_err(store_err)?
                        .iter()
                        .find(|x| x.id == d)
                        .map(drive_name)
                        .unwrap_or_else(|| "Documents".to_owned());
                    (format!("drive:{}", d.as_str()), name)
                };
                let crumbs = breadcrumb(&store, &d, &i, root.as_ref(), &label, &loc).await?;
                let items = load_children(&store, &d, Some(&i)).await?;
                (loc, Some(d), Some(f.to_owned()), items, crumbs)
            }
            (None, None | Some("mine")) => match my_drive_or_kick(s).await? {
                Some(d) => {
                    kick_drive(s, &d).await?;
                    let root = fstate::root_of(&store, &d).await.map_err(sync_err)?;
                    let items = match &root {
                        Some(r) => load_children(&store, &d, Some(r)).await?,
                        None => {
                            syncing = true;
                            Vec::new()
                        }
                    };
                    let folder = root.as_ref().map(|r| compose(&d, r));
                    ("mine".to_owned(), Some(d), folder, items, vec![json!({ "id": "mine", "name": "My files" })])
                }
                None => {
                    syncing = true;
                    ("mine".to_owned(), None, None, Vec::new(), vec![json!({ "id": "mine", "name": "My files" })])
                }
            },
            (None, Some(view @ ("recent" | "shared"))) => {
                let (key, refs) = if view == "recent" {
                    (sf::RECENT_KEY, fstate::recent(&store).await.map_err(sync_err)?)
                } else {
                    (sf::SHARED_KEY, fstate::shared(&store).await.map_err(sync_err)?)
                };
                if is_stale(&store, key, STALE_VIEWS).await {
                    let c = ctx(s)?;
                    let recent = view == "recent";
                    spawn_once(key.to_owned(), async move {
                        if recent { sf::refresh_recent(&c).await } else { sf::refresh_shared(&c).await }
                    });
                    syncing = refs.is_empty();
                }
                let label = if view == "recent" { "Recent" } else { "Shared with me" };
                let items = load_refs(&store, refs).await?;
                (view.to_owned(), None, None, items, vec![json!({ "id": view, "name": label })])
            }
            (None, Some(loc)) if loc.starts_with("drive:") => {
                let d = DriveId::new(loc.trim_start_matches("drive:")).map_err(|_| Error::params("invalid drive"))?;
                kick_drive(s, &d).await?;
                let root = fstate::root_of(&store, &d).await.map_err(sync_err)?;
                let items = match &root {
                    Some(r) => load_children(&store, &d, Some(r)).await?,
                    None => {
                        syncing = true;
                        Vec::new()
                    }
                };
                let name = store
                    .read(|r| Ok(r.drives()?.items))
                    .await
                    .map_err(store_err)?
                    .iter()
                    .find(|x| x.id == d)
                    .map(drive_name)
                    .unwrap_or_else(|| "Documents".to_owned());
                let folder = root.as_ref().map(|r| compose(&d, r));
                (loc.to_owned(), Some(d), folder, items, vec![json!({ "id": loc, "name": name })])
            }
            (None, Some(_)) => return Err(Error::params("`location` must be mine, recent, shared or drive:<id>")),
        };
        // Recent keeps Graph's order unless a sort was asked for.
        if !(location == "recent" && params.sort.is_none()) {
            sort_items(&mut items, sort, params.desc);
        }
        let total = items.len();
        let page: Vec<Value> = items.iter().skip(offset).take(limit).map(|it| row(it, now, &tz)).collect();
        let end = offset.saturating_add(page.len());
        let next = (end < total).then(|| end.to_string());
        Ok(json!({
            "location": location,
            "drive": drive,
            "folder": folder,
            "breadcrumb": crumbs,
            "items": page,
            "total": total,
            "next": next,
            "syncing": syncing,
            "sort": sort,
            "desc": params.desc,
        }))
    })
}

fn item_details(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let (d, i) = resolve_item(s, &p).await?;
        let store = s.store()?;
        let it = require_item(&store, &d, &i).await?;
        let (meta, stale) = meta_for(s, &d, &i, it.is_folder(), false).await?;
        let meta = meta.unwrap_or_default();
        let links = meta.permissions.iter().filter(|x| x.kind == "link").count();
        let people: HashSet<&str> = meta
            .permissions
            .iter()
            .filter(|x| !x.roles.iter().any(|r| r == "owner"))
            .flat_map(|x| x.granted_to.iter().map(String::as_str))
            .collect();
        let summary = match (people.len(), links) {
            (0, 0) if it.shared => "Shared".to_owned(),
            (0, 0) => "Only you".to_owned(),
            (0, l) => format!("{l} link{}", if l == 1 { "" } else { "s" }),
            (n, 0) => format!("Shared with {n} {}", if n == 1 { "person" } else { "people" }),
            (n, l) => format!(
                "Shared with {n} {} and {l} link{}",
                if n == 1 { "person" } else { "people" },
                if l == 1 { "" } else { "s" }
            ),
        };
        let now = Timestamp::now();
        let tz = TimeZone::system();
        let versions: Vec<Value> = meta
            .versions
            .iter()
            .map(|v| json!({ "id": v.id, "modified": v.modified, "modified_label": time_label(v.modified, now, &tz), "modified_by": v.modified_by, "size": v.size }))
            .collect();
        Ok(json!({
            "item": row(&it, now, &tz),
            "created": it.created_date_time,
            "created_label": time_label(it.created_date_time, now, &tz),
            "permissions": meta.permissions,
            "sharing_summary": summary,
            "versions_count": meta.versions.len(),
            "versions": versions,
            "desktop_app": office_scheme(it.name.as_str()),
            "stale": stale,
        }))
    })
}

fn preview_url(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let (d, i) = resolve_item(s, &p).await?;
        not_pending(&i)?;
        let c = ctx(s)?;
        let url = tokio::time::timeout(REMOTE_TIMEOUT, sf::live::preview_url(&c, &d, &i))
            .await
            .map_err(|_| Error::new(ErrorKind::Remote, "the preview took too long"))?
            .map_err(sync_err)?;
        Ok(json!({ "url": url }))
    })
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SearchParams {
    query: String,
}

fn search(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let params: SearchParams = parse(p)?;
        let q: String = params.query.chars().filter(|c| !c.is_control()).take(MAX_QUERY_CHARS).collect();
        let q = q.trim().to_owned();
        if q.is_empty() {
            return Ok(json!({ "items": [], "remote": false }));
        }
        let store = s.store()?;
        let hits = store.search(q.clone(), vec![SearchScope::Files], 200).await.map_err(store_err)?;
        let refs: Vec<fstate::ItemRef> = hits
            .hits
            .into_iter()
            .filter_map(|h| match h.item {
                quorum_store::HitItem::DriveItem { drive, id } => Some(fstate::ItemRef { drive, id }),
                _ => None,
            })
            .collect();
        let mut items = load_refs(&store, refs).await?;
        let mut remote = false;
        if let Some(d) = fstate::my_drive(&store).await.map_err(sync_err)? {
            let c = ctx(s)?;
            if let Ok(Ok(found)) = tokio::time::timeout(REMOTE_TIMEOUT, sf::live::search_drive(&c, &d, &q)).await {
                remote = true;
                for f in found {
                    if !items.iter().any(|x| x.id == f.id && x.drive_id == f.drive_id) {
                        items.push(f);
                    }
                }
            }
        }
        let now = Timestamp::now();
        let tz = TimeZone::system();
        Ok(json!({ "items": items.iter().map(|x| row(x, now, &tz)).collect::<Vec<_>>(), "remote": remote }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn office_uris() {
        assert_eq!(
            desktop_uri("Plan.DOCX", "https://contoso.sharepoint.com/a/Plan.docx").as_deref(),
            Some("ms-word:ofe|u|https://contoso.sharepoint.com/a/Plan.docx")
        );
        assert_eq!(office_scheme("x.xlsx"), Some("ms-excel"));
        assert_eq!(office_scheme("x.pptx"), Some("ms-powerpoint"));
        assert_eq!(office_scheme("x.vsdx"), Some("ms-visio"));
        assert_eq!(office_scheme("x.mpp"), Some("ms-project"));
        assert_eq!(office_scheme("x.one"), Some("ms-onenote"));
        assert_eq!(office_scheme("x.pdf"), None);
        assert_eq!(office_scheme(".docx"), None);
        assert_eq!(desktop_uri("a.docx", "http://insecure.example/a.docx"), None);
        assert_eq!(desktop_uri("a.docx", "https://x.example/a|b"), None);
        assert_eq!(desktop_uri("a.docx", "https://x.example/a b"), None);
        assert_eq!(desktop_uri("a.docx", "javascript:alert(1)"), None);
    }

    #[test]
    fn row_ids_round_trip() {
        let (d, i) = (DriveId::new("b!abc-_").unwrap(), DriveItemId::new("01ITEM").unwrap());
        assert_eq!(split(&compose(&d, &i)), Some((d, i)));
        assert_eq!(split("nope"), None);
        assert_eq!(split("|x"), None);
    }

    #[test]
    fn kinds_by_extension() {
        assert_eq!(file_kind("a.docx", false), "word");
        assert_eq!(file_kind("a.docx", true), "folder");
        assert_eq!(file_kind("photo.JPG", false), "image");
        assert_eq!(file_kind("noext", false), "file");
    }

    #[test]
    fn time_labels() {
        let tz = TimeZone::UTC;
        let now: Timestamp = "2026-10-17T15:00:00Z".parse().unwrap();
        let today: Timestamp = "2026-10-17T09:05:00Z".parse().unwrap();
        let earlier: Timestamp = "2026-03-02T09:05:00Z".parse().unwrap();
        let old: Timestamp = "2024-03-02T09:05:00Z".parse().unwrap();
        assert_eq!(time_label(Some(today), now, &tz), "09:05");
        assert_eq!(time_label(Some(earlier), now, &tz), "Mar 2");
        assert_eq!(time_label(Some(old), now, &tz), "Mar 2, 2024");
        assert_eq!(time_label(None, now, &tz), "");
    }
}
