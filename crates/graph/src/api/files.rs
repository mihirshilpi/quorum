//! OneDrive and SharePoint files: drives, items, delta, recent and shared items, search, download,
//! folder/rename/move/copy/delete, small and resumable uploads, sharing links and permissions,
//! versions, thumbnails, preview and check-in/out.
//!
//! Mapping: `parentReference.driveId` / `parentReference.id` give [`DriveItem::drive_id`] /
//! [`DriveItem::parent_id`] (falling back to the drive the request addressed); for Recent and
//! Shared-with-me entries the `remoteItem` (the real item, often in another drive) wins; the
//! `shared` facet becomes a flag; `lastModifiedBy` (an `identitySet`) becomes an
//! [`quorum_model::Identity`].
//!
//! **Pre-authenticated URLs** (`@microsoft.graph.downloadUrl`, the `/content` redirect target,
//! copy monitors, thumbnails, preview and sharing links) are capabilities: they are never logged,
//! their `Debug` is redacted, and our bearer token is never sent to them. They must be `https`
//! on one of the client's upload/download host suffixes (SharePoint, OneDrive).

use std::fmt;
use std::time::Duration;

use jiff::Timestamp;
use quorum_model::{
    Drive, DriveId, DriveItem, DriveItemId, DriveKind, FileFacet, FolderFacet, GraphEnum, Identity, Sensitive,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use url::Url;
use uuid::Uuid;

use super::chat::{
    MAX_NAME_CHARS, WireIdentitySet, cap, id_of, identity, lenient, map_page, opaque, seg, sens, strings, ts, web_url,
};
use crate::client::Client;
use crate::error::{Error, ErrorKind, Result};
use crate::http::HttpResponse;
use crate::paging::{DeltaPage, Page};
use crate::request::{Method, Request, ResourceFamily};
use crate::upload::{ChunkSize, ChunkSource};

/// Concurrency bucket for OneDrive and SharePoint requests.
pub const FILES_BUCKET: &str = "files";
/// Largest file [`small_upload_request`] accepts (Graph's simple-upload limit, 4 MiB).
pub const MAX_SMALL_UPLOAD_BYTES: usize = 4 * 1024 * 1024;
/// Largest `$top` used for children listings.
pub const MAX_CHILDREN_PAGE_SIZE: u32 = 999;
/// Redirect hops followed for a download.
const MAX_DOWNLOAD_REDIRECTS: usize = 3;

/// The fields stored for a drive item.
pub const SELECT_DRIVE_ITEM: &str = "id,name,size,file,folder,parentReference,lastModifiedBy,lastModifiedDateTime,\
createdDateTime,webUrl,eTag,cTag,shared,deleted,remoteItem";
const SELECT_DRIVE: &str = "id,driveType,name,owner,webUrl";

pub(crate) fn files(req: Request) -> Request {
    req.family(ResourceFamily::Files).bucket(FILES_BUCKET)
}

fn config(detail: &'static str) -> Error {
    Error::new(ErrorKind::Config, detail)
}

fn drive_path(drive: &DriveId) -> String {
    format!("/drives/{}", seg(drive.as_str()))
}

fn item_path(drive: &DriveId, item: &DriveItemId) -> String {
    format!("{}/items/{}", drive_path(drive), seg(item.as_str()))
}

/// Validates a file or folder name for OneDrive/SharePoint.
///
/// # Errors
/// [`ErrorKind::Config`] for an empty name, `.`/`..`, over 255 chars, control characters,
/// characters SharePoint forbids (`" * : < > ? / \ |`) or leading/trailing spaces.
pub fn validate_name(name: &str) -> Result<&str> {
    let bad = |c: char| c.is_control() || "\"*:<>?/\\|".contains(c);
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.chars().count() > 255
        || name.chars().any(bad)
        || name.trim() != name
    {
        return Err(config("invalid file or folder name"));
    }
    Ok(name)
}

/// Encodes a drive-relative path (`Documents/Q3 report.docx`) for `root:/{path}:` addressing.
fn encode_rel_path(path: &str) -> Result<String> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() || trimmed.len() > 4096 {
        return Err(config("path must name an item below the root"));
    }
    let mut parts = Vec::new();
    for part in trimmed.split('/') {
        parts.push(seg(validate_name(part)?));
    }
    Ok(parts.join("/"))
}

// ---------------------------------------------------------------------------------------------
// Mapping.
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct WireParentRef {
    #[serde(default, deserialize_with = "lenient")]
    drive_id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    id: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct WireFile {
    #[serde(default, deserialize_with = "lenient")]
    mime_type: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct WireFolder {
    #[serde(default, deserialize_with = "lenient")]
    child_count: Option<u32>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct WireItem {
    #[serde(default, deserialize_with = "lenient")]
    id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    name: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    size: Option<u64>,
    #[serde(default, deserialize_with = "lenient")]
    file: Option<WireFile>,
    #[serde(default, deserialize_with = "lenient")]
    folder: Option<WireFolder>,
    #[serde(default, deserialize_with = "lenient")]
    parent_reference: Option<WireParentRef>,
    #[serde(default, deserialize_with = "lenient")]
    last_modified_by: Option<WireIdentitySet>,
    #[serde(default, deserialize_with = "lenient")]
    last_modified_date_time: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    created_date_time: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    web_url: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    e_tag: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    c_tag: Option<String>,
    #[serde(default)]
    shared: Option<Value>,
    #[serde(default)]
    remote_item: Option<Value>,
}

fn facet_present(v: Option<&Value>) -> bool {
    v.is_some_and(|v| !v.is_null())
}

fn item_from_wire(w: WireItem, fallback_drive: Option<&DriveId>) -> Option<DriveItem> {
    // Recent / shared-with-me entries describe the real item in `remoteItem`.
    let remote: Option<WireItem> = w.remote_item.as_ref().and_then(|r| serde_json::from_value(r.clone()).ok());
    let (id, parent) = match &remote {
        Some(r) if r.id.is_some() => (r.id.clone(), r.parent_reference.as_ref()),
        _ => (w.id.clone(), w.parent_reference.as_ref()),
    };
    let id: DriveItemId = id_of(id)?;
    let drive_id = parent
        .and_then(|p| p.drive_id.clone())
        .and_then(|d| DriveId::new(d).ok())
        .or_else(|| w.parent_reference.as_ref().and_then(|p| p.drive_id.clone()).and_then(|d| DriveId::new(d).ok()))
        .or_else(|| fallback_drive.cloned())?;
    let parent_id = parent.and_then(|p| id_of(p.id.clone()));
    let r = remote.unwrap_or_default();
    let file = w.file.or(r.file).map(|f| FileFacet { mime_type: f.mime_type.map(|m| cap(m, 255)) });
    let folder = w.folder.or(r.folder).map(|f| FolderFacet { child_count: f.child_count.unwrap_or(0) });
    let shared = facet_present(w.shared.as_ref()) || facet_present(r.shared.as_ref());
    Some(DriveItem {
        id,
        drive_id,
        parent_id,
        name: sens(w.name.or(r.name), 1024),
        size: w.size.or(r.size).unwrap_or(0),
        file,
        folder,
        last_modified_by: identity(w.last_modified_by.or(r.last_modified_by)),
        last_modified_date_time: ts(w.last_modified_date_time.or(r.last_modified_date_time).as_deref()),
        created_date_time: ts(w.created_date_time.or(r.created_date_time).as_deref()),
        web_url: web_url(w.web_url.or(r.web_url)),
        e_tag: w.e_tag.or(r.e_tag).map(|t| cap(t, 512)),
        c_tag: w.c_tag.or(r.c_tag).map(|t| cap(t, 512)),
        shared,
    })
}

/// Maps a Graph `driveItem` to a [`DriveItem`]; `fallback_drive` is used when Graph omits
/// `parentReference.driveId`. Returns `None` without an id or drive id.
#[must_use]
pub fn parse_drive_item(value: Value, fallback_drive: Option<&DriveId>) -> Option<DriveItem> {
    let w: WireItem = serde_json::from_value(value).ok()?;
    item_from_wire(w, fallback_drive)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireDrive {
    #[serde(default, deserialize_with = "lenient")]
    id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    drive_type: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    name: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    owner: Option<WireIdentitySet>,
    #[serde(default, deserialize_with = "lenient")]
    web_url: Option<String>,
}

/// Maps a Graph `drive` to a [`Drive`] (`None` without an id or drive type).
#[must_use]
pub fn parse_drive(value: Value) -> Option<Drive> {
    let w: WireDrive = serde_json::from_value(value).ok()?;
    Some(Drive {
        id: id_of(w.id)?,
        drive_type: DriveKind::from_graph(w.drive_type.as_deref()?),
        name: sens(w.name, MAX_NAME_CHARS),
        owner: identity(w.owner),
        web_url: web_url(w.web_url),
    })
}

/// One change from a drive delta.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriveChange {
    /// A new or changed item.
    Changed(Box<DriveItem>),
    /// The item was deleted (`deleted` facet or `@removed`).
    Deleted(DriveItemId),
}

fn parse_drive_change(value: Value, drive: &DriveId) -> Option<DriveChange> {
    let gone = value.get("@removed").is_some() || value.get("deleted").is_some_and(|d| !d.is_null());
    if gone {
        return id_of::<DriveItemId>(value.get("id").and_then(Value::as_str).map(str::to_owned))
            .map(DriveChange::Deleted);
    }
    parse_drive_item(value, Some(drive)).map(|i| DriveChange::Changed(Box::new(i)))
}

// ---------------------------------------------------------------------------------------------
// Secret URLs and other result types.
// ---------------------------------------------------------------------------------------------

/// A pre-authenticated URL (download, copy monitor, preview, thumbnail, sharing link).
/// Treat as a secret: `Debug` prints only the host.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretUrl(Url);

impl SecretUrl {
    /// Parses an absolute URL.
    ///
    /// # Errors
    /// [`ErrorKind::Malformed`] if it is not an absolute URL.
    pub fn parse(s: &str) -> Result<Self> {
        Url::parse(s).map(Self).map_err(|e| Error::new(ErrorKind::Malformed, "not a URL").with_source(e))
    }

    /// The URL. Do not log it.
    #[must_use]
    pub fn as_url(&self) -> &Url {
        &self.0
    }

    /// The URL as a string. Do not log it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for SecretUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretUrl({:?})", self.0.host_str().unwrap_or(""))
    }
}

/// Downloaded file content.
#[derive(Clone, PartialEq, Eq)]
pub struct Download {
    /// The `Content-Type` sent (untrusted).
    pub content_type: Option<String>,
    /// The bytes.
    pub bytes: Vec<u8>,
}

impl fmt::Debug for Download {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Download").field("content_type", &self.content_type).field("len", &self.bytes.len()).finish()
    }
}

/// What to do when an item with the same name exists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConflictBehavior {
    /// Pick a new name (`name 1.txt`).
    #[default]
    Rename,
    /// Fail with 409.
    Fail,
    /// Overwrite.
    Replace,
}

impl ConflictBehavior {
    /// The Graph value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rename => "rename",
            Self::Fail => "fail",
            Self::Replace => "replace",
        }
    }
}

/// Where a copy should go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemLocation {
    /// The destination drive.
    pub drive: DriveId,
    /// The destination folder.
    pub folder: DriveItemId,
}

/// The status of an asynchronous copy.
#[derive(Clone, Debug, PartialEq)]
pub enum CopyStatus {
    /// Still running (`notStarted`, `inProgress`, `waiting`, `updating`), with a percentage if sent.
    InProgress {
        /// 0 to 100.
        percent: Option<f64>,
    },
    /// Done; the new item's id when Graph reports it.
    Completed {
        /// The copied item.
        item: Option<DriveItemId>,
    },
    /// Graph reported a failure.
    Failed {
        /// Graph's error code, if any.
        code: Option<String>,
    },
}

/// The kind of sharing link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkType {
    /// Read-only.
    View,
    /// Read-write.
    Edit,
    /// Embeddable (OneDrive personal only).
    Embed,
}

impl LinkType {
    fn as_str(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Edit => "edit",
            Self::Embed => "embed",
        }
    }
}

/// Who a sharing link works for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkScope {
    /// Anyone with the link (if the tenant allows it).
    Anonymous,
    /// Anyone in the organisation.
    Organization,
    /// Only people the item is (or will be) shared with.
    Users,
}

impl LinkScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Anonymous => "anonymous",
            Self::Organization => "organization",
            Self::Users => "users",
        }
    }
}

/// A sharing permission on an item (link or direct grant).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Permission {
    /// The permission id (to delete it).
    pub id: String,
    /// Roles such as `read`, `write`, `owner`.
    pub roles: Vec<String>,
    /// The link type (`view`, `edit`, ...) for sharing links.
    pub link_type: Option<String>,
    /// The link scope (`anonymous`, `organization`, `users`) for sharing links.
    pub link_scope: Option<String>,
    /// The link URL (a capability for anonymous links: secret).
    pub link_url: Option<SecretUrl>,
    /// Who the permission was granted to.
    pub granted_to: Vec<Identity>,
    /// When it expires.
    pub expiration: Option<Timestamp>,
    /// Inherited from a parent folder.
    pub inherited: bool,
}

/// Maps a Graph `permission` to a [`Permission`] (`None` without an id).
#[must_use]
pub fn parse_permission(value: Value) -> Option<Permission> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct WireLink {
        #[serde(default, deserialize_with = "lenient")]
        r#type: Option<String>,
        #[serde(default, deserialize_with = "lenient")]
        scope: Option<String>,
        #[serde(default, deserialize_with = "lenient")]
        web_url: Option<String>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct WirePermission {
        #[serde(default, deserialize_with = "lenient")]
        id: Option<String>,
        #[serde(default, deserialize_with = "lenient")]
        roles: Option<Vec<Value>>,
        #[serde(default, deserialize_with = "lenient")]
        link: Option<WireLink>,
        #[serde(default, deserialize_with = "lenient")]
        granted_to_v2: Option<WireIdentitySet>,
        #[serde(default, deserialize_with = "lenient")]
        granted_to_identities_v2: Option<Vec<Value>>,
        #[serde(default, deserialize_with = "lenient")]
        expiration_date_time: Option<String>,
        #[serde(default)]
        inherited_from: Option<Value>,
    }
    let w: WirePermission = serde_json::from_value(value).ok()?;
    let id = w.id.filter(|s| opaque(s, "permission id").is_ok())?;
    let mut granted_to: Vec<Identity> = identity(w.granted_to_v2).into_iter().collect();
    granted_to.extend(
        w.granted_to_identities_v2
            .unwrap_or_default()
            .into_iter()
            .take(super::chat::MAX_ITEMS)
            .filter_map(|v| serde_json::from_value::<WireIdentitySet>(v).ok())
            .filter_map(WireIdentitySet::into_identity),
    );
    let (link_type, link_scope, link_url) = match w.link {
        Some(l) => (
            l.r#type.map(|t| cap(t, 64)),
            l.scope.map(|s| cap(s, 64)),
            web_url(l.web_url).and_then(|u| SecretUrl::parse(&u).ok()),
        ),
        None => (None, None, None),
    };
    Some(Permission {
        id,
        roles: strings(w.roles, 64).into_iter().take(16).collect(),
        link_type,
        link_scope,
        link_url,
        granted_to,
        expiration: ts(w.expiration_date_time.as_deref()),
        inherited: facet_present(w.inherited_from.as_ref()),
    })
}

/// A version of a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveItemVersion {
    /// The version id (e.g. `"3.0"`).
    pub id: String,
    /// When it was saved.
    pub last_modified_date_time: Option<Timestamp>,
    /// Who saved it.
    pub last_modified_by: Option<Identity>,
    /// Size in bytes.
    pub size: u64,
}

fn parse_version(value: Value) -> Option<DriveItemVersion> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct W {
        #[serde(default, deserialize_with = "lenient")]
        id: Option<String>,
        #[serde(default, deserialize_with = "lenient")]
        last_modified_date_time: Option<String>,
        #[serde(default, deserialize_with = "lenient")]
        last_modified_by: Option<WireIdentitySet>,
        #[serde(default, deserialize_with = "lenient")]
        size: Option<u64>,
    }
    let w: W = serde_json::from_value(value).ok()?;
    Some(DriveItemVersion {
        id: w.id.filter(|s| opaque(s, "version id").is_ok())?,
        last_modified_date_time: ts(w.last_modified_date_time.as_deref()),
        last_modified_by: identity(w.last_modified_by),
        size: w.size.unwrap_or(0),
    })
}

/// One thumbnail image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thumbnail {
    /// Pre-authenticated image URL.
    pub url: SecretUrl,
    /// Width in pixels.
    pub width: Option<u32>,
    /// Height in pixels.
    pub height: Option<u32>,
}

/// The thumbnails of an item in the standard sizes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThumbnailSet {
    /// The set id (usually `"0"`).
    pub id: Option<String>,
    /// 96 px longest edge.
    pub small: Option<Thumbnail>,
    /// 176 px.
    pub medium: Option<Thumbnail>,
    /// 800 px.
    pub large: Option<Thumbnail>,
}

fn parse_thumbnail_set(value: Value) -> Option<ThumbnailSet> {
    #[derive(Deserialize)]
    struct WT {
        #[serde(default, deserialize_with = "lenient")]
        url: Option<String>,
        #[serde(default, deserialize_with = "lenient")]
        width: Option<u32>,
        #[serde(default, deserialize_with = "lenient")]
        height: Option<u32>,
    }
    #[derive(Deserialize)]
    struct W {
        #[serde(default, deserialize_with = "lenient")]
        id: Option<String>,
        #[serde(default, deserialize_with = "lenient")]
        small: Option<WT>,
        #[serde(default, deserialize_with = "lenient")]
        medium: Option<WT>,
        #[serde(default, deserialize_with = "lenient")]
        large: Option<WT>,
    }
    let thumb = |t: Option<WT>| {
        let t = t?;
        Some(Thumbnail {
            url: web_url(t.url).and_then(|u| SecretUrl::parse(&u).ok())?,
            width: t.width,
            height: t.height,
        })
    };
    let w: W = serde_json::from_value(value).ok()?;
    Some(ThumbnailSet {
        id: w.id.map(|i| cap(i, 64)),
        small: thumb(w.small),
        medium: thumb(w.medium),
        large: thumb(w.large),
    })
}

// ---------------------------------------------------------------------------------------------
// Request builders.
// ---------------------------------------------------------------------------------------------

/// `GET /me/drive`.
#[must_use]
pub fn my_drive_request() -> Request {
    files(Request::get("/me/drive").query("$select", SELECT_DRIVE))
}

/// `GET /me/drives` (the user's OneDrive plus drives they can reach).
#[must_use]
pub fn list_my_drives_request() -> Request {
    files(Request::get("/me/drives").query("$select", SELECT_DRIVE))
}

/// `GET /drives/{id}`.
#[must_use]
pub fn get_drive_request(drive: &DriveId) -> Request {
    files(Request::get(drive_path(drive)).query("$select", SELECT_DRIVE))
}

/// `GET /drives/{id}/root/children`.
#[must_use]
pub fn list_root_children_request(drive: &DriveId, top: u32) -> Request {
    files(
        Request::get(format!("{}/root/children", drive_path(drive)))
            .query("$select", SELECT_DRIVE_ITEM)
            .query("$top", top.clamp(1, MAX_CHILDREN_PAGE_SIZE).to_string()),
    )
}

/// `GET /drives/{id}/items/{id}/children`.
#[must_use]
pub fn list_children_request(drive: &DriveId, folder: &DriveItemId, top: u32) -> Request {
    files(
        Request::get(format!("{}/children", item_path(drive, folder)))
            .query("$select", SELECT_DRIVE_ITEM)
            .query("$top", top.clamp(1, MAX_CHILDREN_PAGE_SIZE).to_string()),
    )
}

/// `GET /drives/{id}/items/{id}`.
#[must_use]
pub fn get_item_request(drive: &DriveId, item: &DriveItemId) -> Request {
    files(Request::get(item_path(drive, item)).query("$select", SELECT_DRIVE_ITEM))
}

/// `GET /drives/{id}/root:/{path}` (each segment validated and percent-encoded).
///
/// # Errors
/// [`ErrorKind::Config`] for an empty path or an invalid segment (including `..`).
pub fn get_item_by_path_request(drive: &DriveId, path: &str) -> Result<Request> {
    let p = encode_rel_path(path)?;
    Ok(files(Request::get(format!("{}/root:/{p}", drive_path(drive))).query("$select", SELECT_DRIVE_ITEM)))
}

/// `GET /drives/{id}/root/delta`: the first request of a delta round. `token` may be `latest`
/// (start from now without listing everything) or a token from an earlier round; stored
/// `deltaLink`s are followed with [`Client::drive_delta`] instead.
#[must_use]
pub fn drive_delta_request(drive: &DriveId, token: Option<&str>) -> Request {
    let mut req = Request::get(format!("{}/root/delta", drive_path(drive))).query("$select", SELECT_DRIVE_ITEM);
    if let Some(t) = token {
        req = req.query("token", t);
    }
    files(req)
}

/// `GET /me/drive/recent`.
#[must_use]
pub fn recent_request() -> Request {
    files(Request::get("/me/drive/recent"))
}

/// `GET /me/drive/sharedWithMe`.
#[must_use]
pub fn shared_with_me_request() -> Request {
    files(Request::get("/me/drive/sharedWithMe"))
}

/// `GET /drives/{id}/root/search(q='...')`.
///
/// # Errors
/// [`ErrorKind::Config`] for an empty or over-long query or control characters.
pub fn search_drive_request(drive: &DriveId, query: &str) -> Result<Request> {
    let q = query.trim();
    if q.is_empty() || q.chars().count() > 255 || q.chars().any(char::is_control) {
        return Err(config("search query must be 1 to 255 characters"));
    }
    let escaped = q.replace('\'', "''");
    Ok(files(
        Request::get(format!("{}/root/search(q='{}')", drive_path(drive), seg(&escaped)))
            .query("$select", SELECT_DRIVE_ITEM),
    ))
}

/// `GET /drives/{id}/items/{id}/content`; Graph answers with a redirect to a pre-authenticated
/// URL, which [`Client::download_item`] follows without our token.
#[must_use]
pub fn download_request(drive: &DriveId, item: &DriveItemId) -> Request {
    files(Request::get(format!("{}/content", item_path(drive, item))).header("Accept", "*/*"))
}

/// `GET /drives/{id}/items/{id}?$select=id,@microsoft.graph.downloadUrl`.
#[must_use]
pub fn download_url_request(drive: &DriveId, item: &DriveItemId) -> Request {
    files(Request::get(item_path(drive, item)).query("$select", "id,@microsoft.graph.downloadUrl"))
}

/// `POST /drives/{id}/items/{parent}/children` creating a folder.
///
/// # Errors
/// As [`validate_name`].
pub fn create_folder_request(
    drive: &DriveId,
    parent: &DriveItemId,
    name: &str,
    conflict: ConflictBehavior,
) -> Result<Request> {
    let body = json!({
        "name": validate_name(name)?,
        "folder": {},
        "@microsoft.graph.conflictBehavior": conflict.as_str(),
    });
    Ok(files(Request::post(format!("{}/children", item_path(drive, parent))).json_value(body)))
}

/// `PATCH /drives/{id}/items/{id}` renaming and/or moving an item. `if_match` (an eTag) guards
/// against overwriting someone else's change.
///
/// # Errors
/// [`ErrorKind::Config`] when neither a name nor a destination is given, or for an invalid name.
pub fn update_item_request(
    drive: &DriveId,
    item: &DriveItemId,
    new_name: Option<&str>,
    new_parent: Option<&ItemLocation>,
    if_match: Option<&str>,
) -> Result<Request> {
    let mut body = Map::new();
    if let Some(n) = new_name {
        body.insert("name".into(), Value::String(validate_name(n)?.to_owned()));
    }
    if let Some(p) = new_parent {
        body.insert("parentReference".into(), json!({"driveId": p.drive.as_str(), "id": p.folder.as_str()}));
    }
    if body.is_empty() {
        return Err(config("rename/move needs a new name or a destination"));
    }
    let mut req = Request::patch(item_path(drive, item)).json_value(Value::Object(body));
    if let Some(tag) = if_match {
        req = req.header("If-Match", opaque(tag, "eTag")?);
    }
    Ok(files(req))
}

/// `POST /drives/{id}/items/{id}/copy` (asynchronous; returns a monitor URL).
///
/// # Errors
/// As [`validate_name`] for `new_name`.
pub fn copy_item_request(
    drive: &DriveId,
    item: &DriveItemId,
    dest: &ItemLocation,
    new_name: Option<&str>,
) -> Result<Request> {
    let mut body = json!({"parentReference": {"driveId": dest.drive.as_str(), "id": dest.folder.as_str()}});
    if let (Some(n), Some(obj)) = (new_name, body.as_object_mut()) {
        obj.insert("name".into(), Value::String(validate_name(n)?.to_owned()));
    }
    Ok(files(Request::post(format!("{}/copy", item_path(drive, item))).json_value(body)))
}

/// `DELETE /drives/{id}/items/{id}` (to the recycle bin).
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid eTag.
pub fn delete_item_request(drive: &DriveId, item: &DriveItemId, if_match: Option<&str>) -> Result<Request> {
    let mut req = Request::delete(item_path(drive, item));
    if let Some(tag) = if_match {
        req = req.header("If-Match", opaque(tag, "eTag")?);
    }
    Ok(files(req))
}

/// `PUT /drives/{id}/items/{parent}:/{name}:/content` for files up to 4 MiB.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid name, an empty content type or more than 4 MiB.
pub fn small_upload_request(
    drive: &DriveId,
    parent: &DriveItemId,
    name: &str,
    content_type: &str,
    data: Vec<u8>,
    conflict: ConflictBehavior,
) -> Result<Request> {
    if data.len() > MAX_SMALL_UPLOAD_BYTES {
        return Err(config("simple upload is limited to 4 MiB; use an upload session"));
    }
    if content_type.is_empty() {
        return Err(config("content type is required"));
    }
    let path = format!("{}:/{}:/content", item_path(drive, parent), seg(validate_name(name)?));
    Ok(files(
        Request::put(path).query("@microsoft.graph.conflictBehavior", conflict.as_str()).bytes(content_type, data),
    ))
}

/// `PUT /drives/{id}/items/{id}/content`: replaces an existing file's content (≤ 4 MiB).
///
/// # Errors
/// [`ErrorKind::Config`] for more than 4 MiB or an empty content type.
pub fn replace_content_request(
    drive: &DriveId,
    item: &DriveItemId,
    content_type: &str,
    data: Vec<u8>,
) -> Result<Request> {
    if data.len() > MAX_SMALL_UPLOAD_BYTES {
        return Err(config("simple upload is limited to 4 MiB; use an upload session"));
    }
    if content_type.is_empty() {
        return Err(config("content type is required"));
    }
    Ok(files(Request::put(format!("{}/content", item_path(drive, item))).bytes(content_type, data)))
}

/// `POST /drives/{id}/items/{parent}:/{name}:/createUploadSession`.
///
/// # Errors
/// As [`validate_name`].
pub fn create_upload_session_request(
    drive: &DriveId,
    parent: &DriveItemId,
    name: &str,
    conflict: ConflictBehavior,
) -> Result<Request> {
    let name = validate_name(name)?;
    let path = format!("{}:/{}:/createUploadSession", item_path(drive, parent), seg(name));
    let body = json!({"item": {"@microsoft.graph.conflictBehavior": conflict.as_str(), "name": name}});
    Ok(files(Request::post(path).json_value(body)))
}

/// `POST /drives/{id}/items/{id}/createLink`. `expiration` applies where the tenant allows it.
#[must_use]
pub fn create_link_request(
    drive: &DriveId,
    item: &DriveItemId,
    link_type: LinkType,
    scope: LinkScope,
    expiration: Option<Timestamp>,
) -> Request {
    let mut body = json!({"type": link_type.as_str(), "scope": scope.as_str()});
    if let (Some(e), Some(obj)) = (expiration, body.as_object_mut()) {
        obj.insert("expirationDateTime".into(), Value::String(e.to_string()));
    }
    files(Request::post(format!("{}/createLink", item_path(drive, item))).json_value(body))
}

/// `GET /drives/{id}/items/{id}/permissions`.
#[must_use]
pub fn list_permissions_request(drive: &DriveId, item: &DriveItemId) -> Request {
    files(Request::get(format!("{}/permissions", item_path(drive, item))))
}

/// `DELETE /drives/{id}/items/{id}/permissions/{id}`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid permission id.
pub fn delete_permission_request(drive: &DriveId, item: &DriveItemId, permission_id: &str) -> Result<Request> {
    let id = opaque(permission_id, "permission id")?;
    Ok(files(Request::delete(format!("{}/permissions/{}", item_path(drive, item), seg(id)))))
}

/// Options for [`invite_request`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invite {
    /// Recipient email addresses.
    pub recipients: Vec<quorum_model::MailAddress>,
    /// Grant write access (otherwise read).
    pub write: bool,
    /// Send the invitation email.
    pub send_invitation: bool,
    /// Message included in the email.
    pub message: Option<Sensitive>,
    /// When access expires.
    pub expiration: Option<Timestamp>,
}

/// `POST /drives/{id}/items/{id}/invite` (sign-in required).
///
/// # Errors
/// [`ErrorKind::Config`] with no recipients or more than 100.
pub fn invite_request(drive: &DriveId, item: &DriveItemId, invite: &Invite) -> Result<Request> {
    if invite.recipients.is_empty() || invite.recipients.len() > 100 {
        return Err(config("invite needs 1 to 100 recipients"));
    }
    let recipients: Vec<Value> = invite.recipients.iter().map(|r| json!({"email": r.as_str()})).collect();
    let mut body = json!({
        "recipients": recipients,
        "requireSignIn": true,
        "sendInvitation": invite.send_invitation,
        "roles": [if invite.write { "write" } else { "read" }],
    });
    if let Some(obj) = body.as_object_mut() {
        if let Some(m) = &invite.message {
            obj.insert("message".into(), Value::String(cap(m.as_str().to_owned(), 2000)));
        }
        if let Some(e) = invite.expiration {
            obj.insert("expirationDateTime".into(), Value::String(e.to_string()));
        }
    }
    Ok(files(Request::post(format!("{}/invite", item_path(drive, item))).json_value(body)))
}

/// `GET /drives/{id}/items/{id}/versions`.
#[must_use]
pub fn list_versions_request(drive: &DriveId, item: &DriveItemId) -> Request {
    files(Request::get(format!("{}/versions", item_path(drive, item))))
}

/// `POST /drives/{id}/items/{id}/versions/{id}/restoreVersion`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid version id.
pub fn restore_version_request(drive: &DriveId, item: &DriveItemId, version_id: &str) -> Result<Request> {
    let id = opaque(version_id, "version id")?;
    Ok(files(Request::post(format!("{}/versions/{}/restoreVersion", item_path(drive, item), seg(id)))))
}

/// `GET /drives/{id}/items/{id}/thumbnails`.
#[must_use]
pub fn thumbnails_request(drive: &DriveId, item: &DriveItemId) -> Request {
    files(Request::get(format!("{}/thumbnails", item_path(drive, item))))
}

/// `POST /drives/{id}/items/{id}/preview` (returns an embeddable `getUrl`).
#[must_use]
pub fn preview_request(drive: &DriveId, item: &DriveItemId) -> Request {
    files(Request::post(format!("{}/preview", item_path(drive, item))).json_value(json!({})).idempotent(true))
}

/// `POST /drives/{id}/items/{id}/checkout`.
#[must_use]
pub fn checkout_request(drive: &DriveId, item: &DriveItemId) -> Request {
    files(Request::post(format!("{}/checkout", item_path(drive, item))))
}

/// `POST /drives/{id}/items/{id}/checkin` with a comment.
///
/// # Errors
/// [`ErrorKind::Config`] for a comment with control characters (other than newlines) or over 1000 chars.
pub fn checkin_request(drive: &DriveId, item: &DriveItemId, comment: &str) -> Result<Request> {
    if comment.chars().count() > 1000 || comment.chars().any(|c| c.is_control() && c != '\n') {
        return Err(config("check-in comment must be at most 1000 characters of text"));
    }
    Ok(files(Request::post(format!("{}/checkin", item_path(drive, item))).json_value(json!({"comment": comment}))))
}

// ---------------------------------------------------------------------------------------------
// Typed calls.
// ---------------------------------------------------------------------------------------------

fn location(resp: &HttpResponse) -> Option<&str> {
    resp.header("location")
}

impl Client {
    /// Whether `url` may receive an unauthenticated download/monitor request: `https`, no
    /// credentials, on an allowed upload/download host suffix, and not the Graph host itself
    /// (pre-authenticated URLs never point there).
    pub(crate) fn check_secret_url(&self, url: &Url) -> Result<()> {
        let host = url.host_str().unwrap_or("").to_ascii_lowercase();
        let host_ok = self
            .inner
            .upload_hosts
            .iter()
            .any(|s| host == *s || host.strip_suffix(s.as_str()).is_some_and(|rest| rest.ends_with('.')));
        if url.scheme() != "https"
            || !host_ok
            || !url.username().is_empty()
            || url.password().is_some()
            || self.is_graph_url(url)
        {
            tracing::warn!(name: "graph.secret_url_rejected", host = host.as_str(), "pre-authenticated URL host not allowed");
            return Err(Error::new(ErrorKind::Malformed, "pre-authenticated URL is not on an allowed host"));
        }
        Ok(())
    }

    /// An unauthenticated `GET` of a pre-authenticated URL (never carries our token).
    async fn secret_get(&self, url: &Url, idempotent: bool) -> Result<HttpResponse> {
        self.check_secret_url(url)?;
        let crid = Uuid::new_v4().to_string();
        self.execute_url(
            Method::Get,
            url.clone(),
            vec![("Accept".to_owned(), "*/*".to_owned())],
            Vec::new(),
            ResourceFamily::Files,
            FILES_BUCKET,
            idempotent,
            false,
            &crid,
        )
        .await
    }

    async fn files_page(&self, req: Request, drive: Option<&DriveId>) -> Result<Page<DriveItem>> {
        Ok(map_page(self.page(req).await?, |v| parse_drive_item(v, drive)))
    }

    /// The user's OneDrive.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn my_drive(&self) -> Result<Drive> {
        self.send_mapped(my_drive_request(), "drive", parse_drive).await
    }

    /// The drives the user can reach from `/me/drives`.
    ///
    /// # Errors
    /// As [`Client::collect_all`].
    pub async fn list_my_drives(&self) -> Result<Vec<Drive>> {
        let raw: Vec<Value> = self.collect_all(list_my_drives_request(), 20).await?;
        Ok(raw.into_iter().filter_map(parse_drive).collect())
    }

    /// One drive.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn get_drive(&self, drive: &DriveId) -> Result<Drive> {
        self.send_mapped(get_drive_request(drive), "drive", parse_drive).await
    }

    /// The first page of the root folder's children.
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_root_children(&self, drive: &DriveId, top: u32) -> Result<Page<DriveItem>> {
        self.files_page(list_root_children_request(drive, top), Some(drive)).await
    }

    /// The first page of a folder's children.
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_children(&self, drive: &DriveId, folder: &DriveItemId, top: u32) -> Result<Page<DriveItem>> {
        self.files_page(list_children_request(drive, folder, top), Some(drive)).await
    }

    /// The next page of a children, recent, shared or search listing from a `nextLink`.
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_items_next(&self, drive: Option<&DriveId>, next_link: &str) -> Result<Page<DriveItem>> {
        self.files_page(files(Request::link(next_link)), drive).await
    }

    /// One item by id.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn get_drive_item(&self, drive: &DriveId, item: &DriveItemId) -> Result<DriveItem> {
        self.send_mapped(get_item_request(drive, item), "drive item", |v| parse_drive_item(v, Some(drive))).await
    }

    /// One item by drive-relative path.
    ///
    /// # Errors
    /// As [`get_item_by_path_request`] and [`Client::send_json`].
    pub async fn get_drive_item_by_path(&self, drive: &DriveId, path: &str) -> Result<DriveItem> {
        let req = get_item_by_path_request(drive, path)?;
        self.send_mapped(req, "drive item", |v| parse_drive_item(v, Some(drive))).await
    }

    /// One page of a drive delta: `link` is a `nextLink` or stored `deltaLink`, or `None` to start
    /// with [`drive_delta_request`] (no token).
    ///
    /// # Errors
    /// [`ErrorKind::DeltaExpired`] when the token is no longer valid (resync); as
    /// [`Client::delta_page`].
    pub async fn drive_delta(&self, drive: &DriveId, link: Option<&str>) -> Result<DeltaPage<DriveChange>> {
        let req = match link {
            Some(l) => files(Request::link(l)),
            None => drive_delta_request(drive, None),
        };
        let page = self.delta_page::<Value>(req).await?;
        let mut items = Vec::with_capacity(page.items.len());
        let mut skipped = page.skipped;
        for v in page.items {
            match parse_drive_change(v, drive) {
                Some(c) => items.push(c),
                None => skipped = skipped.saturating_add(1),
            }
        }
        Ok(DeltaPage { items, skipped, outcome: page.outcome })
    }

    /// The user's recent files (first page).
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn recent_files(&self) -> Result<Page<DriveItem>> {
        self.files_page(recent_request(), None).await
    }

    /// Files shared with the user (first page).
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn shared_with_me(&self) -> Result<Page<DriveItem>> {
        self.files_page(shared_with_me_request(), None).await
    }

    /// Searches a drive (first page).
    ///
    /// # Errors
    /// As [`search_drive_request`] and [`Client::page`].
    pub async fn search_drive(&self, drive: &DriveId, query: &str) -> Result<Page<DriveItem>> {
        self.files_page(search_drive_request(drive, query)?, Some(drive)).await
    }

    /// Downloads a file: asks Graph for the content and follows the redirect to the
    /// pre-authenticated URL without our token (host-checked, at most 3 hops).
    ///
    /// # Errors
    /// `Malformed` for a redirect without `Location` or to a disallowed host; `TooLarge` above the
    /// response cap; as [`Client::send_raw`].
    pub async fn download_item(&self, drive: &DriveId, item: &DriveItemId) -> Result<Download> {
        let mut resp = self.send_raw(download_request(drive, item)).await?;
        for _ in 0..MAX_DOWNLOAD_REDIRECTS {
            if !(300..400).contains(&resp.status) {
                break;
            }
            let loc = location(&resp).ok_or_else(|| Error::new(ErrorKind::Malformed, "redirect without Location"))?;
            let url = SecretUrl::parse(loc)?;
            resp = self.check(self.secret_get(url.as_url(), true).await?)?;
        }
        if (300..400).contains(&resp.status) {
            return Err(Error::new(ErrorKind::Malformed, "too many download redirects"));
        }
        Ok(Download { content_type: resp.header("content-type").map(str::to_owned), bytes: resp.body })
    }

    /// The item's short-lived `@microsoft.graph.downloadUrl`, if Graph returns one.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn download_url(&self, drive: &DriveId, item: &DriveItemId) -> Result<Option<SecretUrl>> {
        let v: Value = self.send_json(download_url_request(drive, item)).await?;
        Ok(v.get("@microsoft.graph.downloadUrl")
            .and_then(Value::as_str)
            .and_then(|u| web_url(Some(u.to_owned())))
            .and_then(|u| SecretUrl::parse(&u).ok()))
    }

    /// Downloads from a pre-authenticated URL (e.g. `@microsoft.graph.downloadUrl`). Our token is
    /// never sent; the host must be an allowed SharePoint/OneDrive host.
    ///
    /// # Errors
    /// `Malformed` for a disallowed host; `TooLarge` above the response cap; mapped HTTP errors.
    pub async fn download_from_url(&self, url: &SecretUrl) -> Result<Download> {
        let resp = self.check(self.secret_get(url.as_url(), true).await?)?;
        if (300..400).contains(&resp.status) {
            return Err(Error::new(ErrorKind::Malformed, "download URL redirected"));
        }
        Ok(Download { content_type: resp.header("content-type").map(str::to_owned), bytes: resp.body })
    }

    /// Creates a folder.
    ///
    /// # Errors
    /// As [`create_folder_request`] and [`Client::send_json`].
    pub async fn create_folder(
        &self,
        drive: &DriveId,
        parent: &DriveItemId,
        name: &str,
        conflict: ConflictBehavior,
    ) -> Result<DriveItem> {
        let req = create_folder_request(drive, parent, name, conflict)?;
        self.send_mapped(req, "drive item", |v| parse_drive_item(v, Some(drive))).await
    }

    /// Renames and/or moves an item.
    ///
    /// # Errors
    /// As [`update_item_request`] and [`Client::send_json`] (`PreconditionFailed` on an eTag mismatch).
    pub async fn update_drive_item(
        &self,
        drive: &DriveId,
        item: &DriveItemId,
        new_name: Option<&str>,
        new_parent: Option<&ItemLocation>,
        if_match: Option<&str>,
    ) -> Result<DriveItem> {
        let req = update_item_request(drive, item, new_name, new_parent, if_match)?;
        self.send_mapped(req, "drive item", |v| parse_drive_item(v, Some(drive))).await
    }

    /// Starts a copy and returns its monitor URL (poll with [`Client::copy_status`]).
    ///
    /// # Errors
    /// `Malformed` if Graph sends no usable `Location`; as [`Client::send_raw`].
    pub async fn copy_drive_item(
        &self,
        drive: &DriveId,
        item: &DriveItemId,
        dest: &ItemLocation,
        new_name: Option<&str>,
    ) -> Result<SecretUrl> {
        let resp = self.send_raw(copy_item_request(drive, item, dest, new_name)?).await?;
        let loc = location(&resp).ok_or_else(|| Error::new(ErrorKind::Malformed, "copy response has no Location"))?;
        let url = SecretUrl::parse(loc)?;
        self.check_secret_url(url.as_url())?;
        Ok(url)
    }

    /// Polls a copy monitor once (unauthenticated).
    ///
    /// # Errors
    /// `Malformed` for a disallowed host or an unreadable status; mapped HTTP errors.
    pub async fn copy_status(&self, monitor: &SecretUrl) -> Result<CopyStatus> {
        let resp = self.secret_get(monitor.as_url(), true).await?;
        if resp.status == 303 {
            return Ok(CopyStatus::Completed { item: None });
        }
        let resp = self.check(resp)?;
        let v: Value = self.parse_json(&resp.body)?;
        let status = v.get("status").and_then(Value::as_str).unwrap_or("");
        let item = v.get("resourceId").and_then(Value::as_str).and_then(|s| DriveItemId::new(s).ok());
        Ok(match status {
            "completed" => CopyStatus::Completed { item },
            "failed" | "deleteFailed" => CopyStatus::Failed {
                code: v
                    .get("error")
                    .and_then(|e| e.get("code"))
                    .and_then(Value::as_str)
                    .map(|c| cap(c.to_owned(), 128)),
            },
            _ => CopyStatus::InProgress {
                percent: v
                    .get("percentageComplete")
                    .and_then(Value::as_f64)
                    .filter(|p| p.is_finite())
                    .map(|p| p.clamp(0.0, 100.0)),
            },
        })
    }

    /// Polls a copy monitor every `interval` until it completes or fails, at most `max_polls`
    /// times; returns the last status (still `InProgress` if the polls ran out).
    ///
    /// # Errors
    /// As [`Client::copy_status`].
    pub async fn wait_for_copy(&self, monitor: &SecretUrl, interval: Duration, max_polls: u32) -> Result<CopyStatus> {
        let mut last = CopyStatus::InProgress { percent: None };
        for i in 0..max_polls.max(1) {
            if i > 0 {
                self.sleep(interval).await;
            }
            last = self.copy_status(monitor).await?;
            if !matches!(last, CopyStatus::InProgress { .. }) {
                break;
            }
        }
        Ok(last)
    }

    /// Deletes an item (to the recycle bin).
    ///
    /// # Errors
    /// As [`delete_item_request`] and [`Client::send_raw`].
    pub async fn delete_drive_item(&self, drive: &DriveId, item: &DriveItemId, if_match: Option<&str>) -> Result<()> {
        self.send_unit(delete_item_request(drive, item, if_match)?).await
    }

    /// Uploads a file of at most 4 MiB into `parent`.
    ///
    /// # Errors
    /// As [`small_upload_request`] and [`Client::send_json`].
    pub async fn upload_small(
        &self,
        drive: &DriveId,
        parent: &DriveItemId,
        name: &str,
        content_type: &str,
        data: Vec<u8>,
        conflict: ConflictBehavior,
    ) -> Result<DriveItem> {
        let req = small_upload_request(drive, parent, name, content_type, data, conflict)?;
        self.send_mapped(req, "drive item", |v| parse_drive_item(v, Some(drive))).await
    }

    /// Uploads a file of any size through an upload session (resumable chunks; see
    /// [`Client::upload`]).
    ///
    /// # Errors
    /// As [`Client::create_upload_session`] and [`Client::upload`]; `Malformed` if the created
    /// item does not map.
    pub async fn upload_large<S: ChunkSource + ?Sized>(
        &self,
        drive: &DriveId,
        parent: &DriveItemId,
        name: &str,
        conflict: ConflictBehavior,
        source: &S,
        chunk: ChunkSize,
    ) -> Result<DriveItem> {
        let session = self.create_upload_session(create_upload_session_request(drive, parent, name, conflict)?).await?;
        let v = self.upload(&session, source, chunk).await?;
        parse_drive_item(v, Some(drive)).ok_or_else(|| super::chat::unmapped("drive item"))
    }

    /// Creates (or returns the existing) sharing link.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn create_sharing_link(
        &self,
        drive: &DriveId,
        item: &DriveItemId,
        link_type: LinkType,
        scope: LinkScope,
        expiration: Option<Timestamp>,
    ) -> Result<Permission> {
        let req = create_link_request(drive, item, link_type, scope, expiration);
        self.send_mapped(req, "permission", parse_permission).await
    }

    /// The permissions on an item.
    ///
    /// # Errors
    /// As [`Client::collect_all`].
    pub async fn list_permissions(&self, drive: &DriveId, item: &DriveItemId) -> Result<Vec<Permission>> {
        let raw: Vec<Value> = self.collect_all(list_permissions_request(drive, item), 20).await?;
        Ok(raw.into_iter().filter_map(parse_permission).collect())
    }

    /// Removes a permission (link or grant).
    ///
    /// # Errors
    /// As [`delete_permission_request`] and [`Client::send_raw`].
    pub async fn delete_permission(&self, drive: &DriveId, item: &DriveItemId, permission_id: &str) -> Result<()> {
        self.send_unit(delete_permission_request(drive, item, permission_id)?).await
    }

    /// Shares an item with people; returns the permissions created.
    ///
    /// # Errors
    /// As [`invite_request`] and [`Client::send_json`].
    pub async fn invite(&self, drive: &DriveId, item: &DriveItemId, invite: &Invite) -> Result<Vec<Permission>> {
        let v: Value = self.send_json(invite_request(drive, item, invite)?).await?;
        Ok(v.get("value")
            .and_then(Value::as_array)
            .map(|a| a.iter().take(super::chat::MAX_ITEMS).cloned().filter_map(parse_permission).collect())
            .unwrap_or_default())
    }

    /// The versions of a file, newest first.
    ///
    /// # Errors
    /// As [`Client::collect_all`].
    pub async fn list_versions(&self, drive: &DriveId, item: &DriveItemId) -> Result<Vec<DriveItemVersion>> {
        let raw: Vec<Value> = self.collect_all(list_versions_request(drive, item), 20).await?;
        Ok(raw.into_iter().filter_map(parse_version).collect())
    }

    /// Restores an older version as the current one.
    ///
    /// # Errors
    /// As [`restore_version_request`] and [`Client::send_raw`].
    pub async fn restore_version(&self, drive: &DriveId, item: &DriveItemId, version_id: &str) -> Result<()> {
        self.send_unit(restore_version_request(drive, item, version_id)?).await
    }

    /// The thumbnail sets of an item.
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn thumbnails(&self, drive: &DriveId, item: &DriveItemId) -> Result<Vec<ThumbnailSet>> {
        Ok(map_page(self.page(thumbnails_request(drive, item)).await?, parse_thumbnail_set).items)
    }

    /// An embeddable preview URL (`getUrl`) for an item.
    ///
    /// # Errors
    /// `Malformed` without an https `getUrl`; as [`Client::send_json`].
    pub async fn preview_url(&self, drive: &DriveId, item: &DriveItemId) -> Result<SecretUrl> {
        self.send_mapped(preview_request(drive, item), "preview", |v| {
            let u = web_url(v.get("getUrl").and_then(Value::as_str).map(str::to_owned))?;
            SecretUrl::parse(&u).ok()
        })
        .await
    }

    /// Checks a file out.
    ///
    /// # Errors
    /// As [`Client::send_raw`].
    pub async fn checkout(&self, drive: &DriveId, item: &DriveItemId) -> Result<()> {
        self.send_unit(checkout_request(drive, item)).await
    }

    /// Checks a file in with a comment.
    ///
    /// # Errors
    /// As [`checkin_request`] and [`Client::send_raw`].
    pub async fn checkin(&self, drive: &DriveId, item: &DriveItemId, comment: &str) -> Result<()> {
        self.send_unit(checkin_request(drive, item, comment)?).await
    }
}
