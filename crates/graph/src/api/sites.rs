//! SharePoint: followed sites, site search and lookup, document libraries, lists, columns, list
//! items (read and edit) and site pages.
//!
//! Site pages use the v1.0 `sitePage` API (`/sites/{id}/pages/microsoft.graph.sitePage`); no beta
//! endpoint is used. Graph does not repeat the site or list id on lists and items, so the ids the
//! request addressed are filled in.

use jiff::Timestamp;
use quorum_model::{Drive, List, ListId, ListItem, ListItemId, Sensitive, Site, SiteId};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::chat::{MAX_NAME_CHARS, MAX_TITLE_CHARS, cap, id_of, lenient, map_page, opaque, seg, sens, ts, web_url};
use super::files::parse_drive;
use crate::client::Client;
use crate::error::{Error, ErrorKind, Result};
use crate::paging::Page;
use crate::request::{Request, ResourceFamily};

/// Concurrency bucket for SharePoint site and list requests.
pub const SITES_BUCKET: &str = "sites";
/// Largest `$top` used for list items.
pub const MAX_LIST_ITEMS_PAGE_SIZE: u32 = 999;
/// Most columns accepted in one create/update.
pub const MAX_FIELDS_PER_WRITE: usize = 256;

const SELECT_SITE: &str = "id,displayName,name,webUrl";
const SELECT_LIST: &str = "id,displayName,list,webUrl";

fn sites(req: Request) -> Request {
    req.family(ResourceFamily::Files).bucket(SITES_BUCKET)
}

fn config(detail: &'static str) -> Error {
    Error::new(ErrorKind::Config, detail)
}

fn site_path(site: &SiteId) -> String {
    format!("/sites/{}", seg(site.as_str()))
}

fn list_path(site: &SiteId, list: &ListId) -> String {
    format!("{}/lists/{}", site_path(site), seg(list.as_str()))
}

// ---------------------------------------------------------------------------------------------
// Mapping.
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireSite {
    #[serde(default, deserialize_with = "lenient")]
    id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    display_name: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    name: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    web_url: Option<String>,
}

/// Maps a Graph `site` to a [`Site`] (`None` without a valid id).
#[must_use]
pub fn parse_site(value: Value) -> Option<Site> {
    let w: WireSite = serde_json::from_value(value).ok()?;
    Some(Site {
        id: id_of(w.id)?,
        display_name: sens(w.display_name, MAX_NAME_CHARS),
        name: w.name.map(|n| cap(n, MAX_NAME_CHARS)),
        web_url: web_url(w.web_url),
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireListInfo {
    #[serde(default, deserialize_with = "lenient")]
    template: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireList {
    #[serde(default, deserialize_with = "lenient")]
    id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    display_name: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    list: Option<WireListInfo>,
    #[serde(default, deserialize_with = "lenient")]
    web_url: Option<String>,
}

/// Maps a Graph `list` to a [`List`] of `site` (`list.template` becomes the template).
#[must_use]
pub fn parse_list(value: Value, site: &SiteId) -> Option<List> {
    let w: WireList = serde_json::from_value(value).ok()?;
    Some(List {
        id: id_of(w.id)?,
        site_id: Some(site.clone()),
        display_name: sens(w.display_name, MAX_NAME_CHARS),
        template: w.list.and_then(|l| l.template).map(|t| cap(t, 64)),
        web_url: web_url(w.web_url),
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireListItem {
    #[serde(default, deserialize_with = "lenient")]
    id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    fields: Option<Map<String, Value>>,
    #[serde(default, deserialize_with = "lenient")]
    created_date_time: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    last_modified_date_time: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    web_url: Option<String>,
}

fn cap_fields(fields: Map<String, Value>) -> Map<String, Value> {
    fields
        .into_iter()
        .filter(|(k, _)| k.len() <= 255)
        .take(1024)
        .map(|(k, v)| match v {
            Value::String(s) => (k, Value::String(cap(s, 256 * 1024))),
            other => (k, other),
        })
        .collect()
}

/// Maps a Graph `listItem` (with `$expand=fields`) to a [`ListItem`] of `list`.
#[must_use]
pub fn parse_list_item(value: Value, list: &ListId) -> Option<ListItem> {
    let w: WireListItem = serde_json::from_value(value).ok()?;
    Some(ListItem {
        id: id_of(w.id)?,
        list_id: Some(list.clone()),
        fields: cap_fields(w.fields.unwrap_or_default()),
        created_date_time: ts(w.created_date_time.as_deref()),
        last_modified_date_time: ts(w.last_modified_date_time.as_deref()),
        web_url: web_url(w.web_url),
    })
}

/// A list column definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListColumn {
    /// The column id.
    pub id: String,
    /// The internal name used as the key in [`ListItem::fields`].
    pub name: String,
    /// The display name.
    pub display_name: Sensitive,
    /// The description.
    pub description: Option<Sensitive>,
    /// The column kind from its type facet (`text`, `number`, `choice`, `dateTime`, `lookup`,
    /// `personOrGroup`, `boolean`, `currency`, ...), `None` if unrecognised.
    pub kind: Option<String>,
    /// Choices, for choice columns.
    pub choices: Vec<String>,
    /// Hidden from views.
    pub hidden: bool,
    /// Not editable.
    pub read_only: bool,
    /// Required.
    pub required: bool,
}

const COLUMN_KINDS: &[&str] = &[
    "boolean",
    "calculated",
    "choice",
    "currency",
    "dateTime",
    "geolocation",
    "hyperlinkOrPicture",
    "lookup",
    "number",
    "personOrGroup",
    "text",
    "term",
    "thumbnail",
    "contentApprovalStatus",
];

/// Maps a Graph `columnDefinition` to a [`ListColumn`] (`None` without an id or name).
#[must_use]
pub fn parse_column(value: Value) -> Option<ListColumn> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct W {
        #[serde(default, deserialize_with = "lenient")]
        id: Option<String>,
        #[serde(default, deserialize_with = "lenient")]
        name: Option<String>,
        #[serde(default, deserialize_with = "lenient")]
        display_name: Option<String>,
        #[serde(default, deserialize_with = "lenient")]
        description: Option<String>,
        #[serde(default, deserialize_with = "lenient")]
        hidden: Option<bool>,
        #[serde(default, deserialize_with = "lenient")]
        read_only: Option<bool>,
        #[serde(default, deserialize_with = "lenient")]
        required: Option<bool>,
    }
    let kind = COLUMN_KINDS.iter().find(|k| value.get(**k).is_some_and(|v| !v.is_null())).map(|k| (*k).to_owned());
    let choices = value
        .get("choice")
        .and_then(|c| c.get("choices"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).take(1024).map(|s| cap(s.to_owned(), 255)).collect())
        .unwrap_or_default();
    let w: W = serde_json::from_value(value).ok()?;
    Some(ListColumn {
        id: w.id.filter(|s| opaque(s, "column id").is_ok())?,
        name: w.name.filter(|n| !n.is_empty() && n.len() <= 255)?,
        display_name: sens(w.display_name, MAX_NAME_CHARS),
        description: w.description.filter(|d| !d.is_empty()).map(|d| sens(Some(d), MAX_TITLE_CHARS)),
        kind,
        choices,
        hidden: w.hidden.unwrap_or(false),
        read_only: w.read_only.unwrap_or(false),
        required: w.required.unwrap_or(false),
    })
}

/// A modern SharePoint page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SitePage {
    /// The page id.
    pub id: String,
    /// The file name (e.g. `Home.aspx`).
    pub name: Option<String>,
    /// The page title.
    pub title: Sensitive,
    /// The web link.
    pub web_url: Option<String>,
    /// When it was last modified.
    pub last_modified_date_time: Option<Timestamp>,
    /// The layout (`article`, `home`, ...).
    pub page_layout: Option<String>,
}

fn parse_page(value: Value) -> Option<SitePage> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct W {
        #[serde(default, deserialize_with = "lenient")]
        id: Option<String>,
        #[serde(default, deserialize_with = "lenient")]
        name: Option<String>,
        #[serde(default, deserialize_with = "lenient")]
        title: Option<String>,
        #[serde(default, deserialize_with = "lenient")]
        web_url: Option<String>,
        #[serde(default, deserialize_with = "lenient")]
        last_modified_date_time: Option<String>,
        #[serde(default, deserialize_with = "lenient")]
        page_layout: Option<String>,
    }
    let w: W = serde_json::from_value(value).ok()?;
    Some(SitePage {
        id: w.id.filter(|s| opaque(s, "page id").is_ok())?,
        name: w.name.map(|n| cap(n, 1024)),
        title: sens(w.title, MAX_TITLE_CHARS),
        web_url: web_url(w.web_url),
        last_modified_date_time: ts(w.last_modified_date_time.as_deref()),
        page_layout: w.page_layout.map(|l| cap(l, 64)),
    })
}

// ---------------------------------------------------------------------------------------------
// Request builders.
// ---------------------------------------------------------------------------------------------

/// `GET /me/followedSites`.
#[must_use]
pub fn followed_sites_request() -> Request {
    sites(Request::get("/me/followedSites").query("$select", SELECT_SITE))
}

/// `GET /sites?search=...`.
///
/// # Errors
/// [`ErrorKind::Config`] for an empty or over-long query or control characters.
pub fn search_sites_request(query: &str) -> Result<Request> {
    let q = query.trim();
    if q.is_empty() || q.chars().count() > 255 || q.chars().any(char::is_control) {
        return Err(config("site search query must be 1 to 255 characters"));
    }
    Ok(sites(Request::get("/sites").query("search", q).query("$select", SELECT_SITE)))
}

/// `GET /sites/{id}` (`root` is the tenant root site; composite ids such as
/// `contoso.sharepoint.com,{guid},{guid}` work too).
#[must_use]
pub fn get_site_request(site: &SiteId) -> Request {
    sites(Request::get(site_path(site)).query("$select", SELECT_SITE))
}

/// `GET /sites/{hostname}:/{server-relative path}` (e.g. `contoso.sharepoint.com`, `sites/Team`).
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid hostname or a path with empty, `.` or `..` segments.
pub fn get_site_by_path_request(hostname: &str, path: &str) -> Result<Request> {
    let host_ok = !hostname.is_empty()
        && hostname.len() <= 253
        && hostname.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        && !hostname.starts_with('.')
        && !hostname.ends_with('.');
    if !host_ok {
        return Err(config("invalid SharePoint hostname"));
    }
    let trimmed = path.trim_matches('/');
    let mut parts = Vec::new();
    for p in trimmed.split('/') {
        if p.is_empty() || p == "." || p == ".." || p.chars().any(char::is_control) || p.len() > 255 {
            return Err(config("invalid site path"));
        }
        parts.push(seg(p));
    }
    Ok(sites(Request::get(format!("/sites/{hostname}:/{}", parts.join("/"))).query("$select", SELECT_SITE)))
}

/// `GET /sites/{id}/drives` (the site's document libraries).
#[must_use]
pub fn site_drives_request(site: &SiteId) -> Request {
    sites(Request::get(format!("{}/drives", site_path(site))).query("$select", "id,driveType,name,owner,webUrl"))
}

/// `GET /sites/{id}/lists`.
#[must_use]
pub fn site_lists_request(site: &SiteId) -> Request {
    sites(Request::get(format!("{}/lists", site_path(site))).query("$select", SELECT_LIST))
}

/// `GET /sites/{id}/lists/{id}/columns`.
#[must_use]
pub fn list_columns_request(site: &SiteId, list: &ListId) -> Request {
    sites(Request::get(format!("{}/columns", list_path(site, list))))
}

fn validate_columns(columns: &[&str]) -> Result<String> {
    for c in columns {
        if c.is_empty() || c.len() > 255 || !c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
            return Err(config("column names must be internal names (letters, digits, _)"));
        }
    }
    Ok(columns.join(","))
}

/// `GET /sites/{id}/lists/{id}/items?$expand=fields` (or `fields($select=...)` for `columns`).
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid column name.
pub fn list_items_request(site: &SiteId, list: &ListId, columns: &[&str], top: u32) -> Result<Request> {
    let expand = if columns.is_empty() {
        "fields".to_owned()
    } else {
        format!("fields($select={})", validate_columns(columns)?)
    };
    Ok(sites(
        Request::get(format!("{}/items", list_path(site, list)))
            .query("$expand", expand)
            .query("$select", "id,createdDateTime,lastModifiedDateTime,webUrl")
            .query("$top", top.clamp(1, MAX_LIST_ITEMS_PAGE_SIZE).to_string()),
    ))
}

/// `GET /sites/{id}/lists/{id}/items/{id}?$expand=fields`.
#[must_use]
pub fn get_list_item_request(site: &SiteId, list: &ListId, item: &ListItemId) -> Request {
    sites(Request::get(format!("{}/items/{}", list_path(site, list), seg(item.as_str()))).query("$expand", "fields"))
}

fn check_fields(fields: &Map<String, Value>) -> Result<()> {
    if fields.is_empty() || fields.len() > MAX_FIELDS_PER_WRITE {
        return Err(config("a list item write needs 1 to 256 fields"));
    }
    validate_columns(&fields.keys().map(String::as_str).collect::<Vec<_>>()).map(|_| ())
}

/// `POST /sites/{id}/lists/{id}/items` with `{"fields": {...}}`.
///
/// # Errors
/// [`ErrorKind::Config`] for no fields, too many, or invalid column names.
pub fn create_list_item_request(site: &SiteId, list: &ListId, fields: &Map<String, Value>) -> Result<Request> {
    check_fields(fields)?;
    Ok(sites(Request::post(format!("{}/items", list_path(site, list))).json_value(json!({"fields": fields}))))
}

/// `PATCH /sites/{id}/lists/{id}/items/{id}/fields`; `if_match` (an eTag) guards concurrent edits.
///
/// # Errors
/// As [`create_list_item_request`]; [`ErrorKind::Config`] for an invalid eTag.
pub fn update_list_item_request(
    site: &SiteId,
    list: &ListId,
    item: &ListItemId,
    fields: &Map<String, Value>,
    if_match: Option<&str>,
) -> Result<Request> {
    check_fields(fields)?;
    let mut req = Request::patch(format!("{}/items/{}/fields", list_path(site, list), seg(item.as_str())))
        .json_value(Value::Object(fields.clone()));
    if let Some(tag) = if_match {
        req = req.header("If-Match", opaque(tag, "eTag")?);
    }
    Ok(sites(req))
}

/// `DELETE /sites/{id}/lists/{id}/items/{id}`.
#[must_use]
pub fn delete_list_item_request(site: &SiteId, list: &ListId, item: &ListItemId) -> Request {
    sites(Request::delete(format!("{}/items/{}", list_path(site, list), seg(item.as_str()))))
}

/// `GET /sites/{id}/pages/microsoft.graph.sitePage` (v1.0).
#[must_use]
pub fn site_pages_request(site: &SiteId) -> Request {
    sites(
        Request::get(format!("{}/pages/microsoft.graph.sitePage", site_path(site)))
            .query("$select", "id,name,title,webUrl,lastModifiedDateTime,pageLayout"),
    )
}

// ---------------------------------------------------------------------------------------------
// Typed calls.
// ---------------------------------------------------------------------------------------------

impl Client {
    /// Sites the user follows.
    ///
    /// # Errors
    /// As [`Client::collect_all`].
    pub async fn followed_sites(&self) -> Result<Vec<Site>> {
        let raw: Vec<Value> = self.collect_all(followed_sites_request(), 20).await?;
        Ok(raw.into_iter().filter_map(parse_site).collect())
    }

    /// Searches sites by keyword (first page).
    ///
    /// # Errors
    /// As [`search_sites_request`] and [`Client::page`].
    pub async fn search_sites(&self, query: &str) -> Result<Page<Site>> {
        Ok(map_page(self.page(search_sites_request(query)?).await?, parse_site))
    }

    /// One site by id.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn get_site(&self, site: &SiteId) -> Result<Site> {
        self.send_mapped(get_site_request(site), "site", parse_site).await
    }

    /// One site by hostname and server-relative path.
    ///
    /// # Errors
    /// As [`get_site_by_path_request`] and [`Client::send_json`].
    pub async fn get_site_by_path(&self, hostname: &str, path: &str) -> Result<Site> {
        self.send_mapped(get_site_by_path_request(hostname, path)?, "site", parse_site).await
    }

    /// The site's document libraries.
    ///
    /// # Errors
    /// As [`Client::collect_all`].
    pub async fn site_drives(&self, site: &SiteId) -> Result<Vec<Drive>> {
        let raw: Vec<Value> = self.collect_all(site_drives_request(site), 20).await?;
        Ok(raw.into_iter().filter_map(parse_drive).collect())
    }

    /// The site's lists.
    ///
    /// # Errors
    /// As [`Client::collect_all`].
    pub async fn site_lists(&self, site: &SiteId) -> Result<Vec<List>> {
        let raw: Vec<Value> = self.collect_all(site_lists_request(site), 20).await?;
        Ok(raw.into_iter().filter_map(|v| parse_list(v, site)).collect())
    }

    /// The columns of a list.
    ///
    /// # Errors
    /// As [`Client::collect_all`].
    pub async fn list_columns(&self, site: &SiteId, list: &ListId) -> Result<Vec<ListColumn>> {
        let raw: Vec<Value> = self.collect_all(list_columns_request(site, list), 20).await?;
        Ok(raw.into_iter().filter_map(parse_column).collect())
    }

    /// The first page of a list's items with their fields.
    ///
    /// # Errors
    /// As [`list_items_request`] and [`Client::page`].
    pub async fn list_items(&self, site: &SiteId, list: &ListId, columns: &[&str], top: u32) -> Result<Page<ListItem>> {
        let page = self.page(list_items_request(site, list, columns, top)?).await?;
        Ok(map_page(page, |v| parse_list_item(v, list)))
    }

    /// The next page of list items from a `nextLink`.
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_items_next_page(&self, list: &ListId, next_link: &str) -> Result<Page<ListItem>> {
        let page = self.page(sites(Request::link(next_link))).await?;
        Ok(map_page(page, |v| parse_list_item(v, list)))
    }

    /// One list item with its fields.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn get_list_item(&self, site: &SiteId, list: &ListId, item: &ListItemId) -> Result<ListItem> {
        self.send_mapped(get_list_item_request(site, list, item), "list item", |v| parse_list_item(v, list)).await
    }

    /// Creates a list item.
    ///
    /// # Errors
    /// As [`create_list_item_request`] and [`Client::send_json`].
    pub async fn create_list_item(
        &self,
        site: &SiteId,
        list: &ListId,
        fields: &Map<String, Value>,
    ) -> Result<ListItem> {
        let req = create_list_item_request(site, list, fields)?;
        self.send_mapped(req, "list item", |v| parse_list_item(v, list)).await
    }

    /// Updates fields of a list item; returns the item's fields as Graph now has them.
    ///
    /// # Errors
    /// As [`update_list_item_request`] and [`Client::send_json`].
    pub async fn update_list_item(
        &self,
        site: &SiteId,
        list: &ListId,
        item: &ListItemId,
        fields: &Map<String, Value>,
        if_match: Option<&str>,
    ) -> Result<Map<String, Value>> {
        let req = update_list_item_request(site, list, item, fields, if_match)?;
        self.send_mapped(req, "field values", |v| match v {
            Value::Object(m) => Some(cap_fields(m)),
            _ => None,
        })
        .await
    }

    /// Deletes a list item.
    ///
    /// # Errors
    /// As [`Client::send_raw`].
    pub async fn delete_list_item(&self, site: &SiteId, list: &ListId, item: &ListItemId) -> Result<()> {
        self.send_unit(delete_list_item_request(site, list, item)).await
    }

    /// The site's modern pages (first page).
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn site_pages(&self, site: &SiteId) -> Result<Page<SitePage>> {
        Ok(map_page(self.page(site_pages_request(site)).await?, parse_page))
    }
}
