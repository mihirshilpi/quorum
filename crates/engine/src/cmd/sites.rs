//! SharePoint sites and lists.
//!
//! Commands: `files.follow_site`, `files.unfollow_site`, `lists.create_item`,
//! `lists.update_item`, `lists.delete_item` (outbox, optimistic).
//!
//! Queries: `sites.list` (followed sites), `sites.lists {site}` (lists and libraries),
//! `lists.items {site, list, after?, limit?}` → `{columns, items, next}`. List items are
//! fetched from Graph on first open (bounded wait) and refreshed in the background after.
//!
//! Item rows: `{id, fields: {<column>: value}, modified, modified_label, web_url, pending}`;
//! columns: `{name, display_name, kind, choices, read_only, required}`.

use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use quorum_model::{ListId, ListItem, ListItemId, SiteId};
use quorum_store::{Cursor, Store};
use quorum_sync::files::handlers::{FollowPayload, ListItemPayload};
use quorum_sync::files::state::{self as fstate, ColumnInfo};
use quorum_sync::files::{self as sf, kinds};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::files::{LOCAL_PREFIX, op_id, store_err, sync_err, time_label};
use crate::command::{CommandFuture, CommandSpec, RunFn, always, parse};
use crate::error::{Error, ErrorKind, Result};
use crate::session::Session;

/// A list opened after this long is refreshed in the background.
const STALE_LIST: Duration = Duration::from_secs(60);
/// Longest the first open of a list waits for Graph.
const FIRST_FETCH_TIMEOUT: Duration = Duration::from_secs(8);
/// Most fields in one write.
const MAX_FIELDS: usize = 256;
/// Largest JSON of one write's fields.
const MAX_FIELDS_BYTES: usize = 64 * 1024;
/// Most cached items scanned to find one by id.
const MAX_SCAN_PAGES: usize = 20;
/// Columns SharePoint adds to every list that a list view does not show.
const INTERNAL_COLUMNS: &[&str] = &[
    "ContentType",
    "Attachments",
    "Edit",
    "LinkTitleNoMenu",
    "LinkTitle",
    "DocIcon",
    "ItemChildCount",
    "FolderChildCount",
    "AppAuthor",
    "AppEditor",
    "ComplianceAssetId",
    "Author",
    "Editor",
    "Created",
    "Modified",
    "ID",
    "id",
    "_UIVersionString",
];

fn spec(
    id: &'static str,
    label: &'static str,
    menu: Option<&'static str>,
    params_doc: &'static str,
    destructive: bool,
    run: RunFn,
) -> CommandSpec {
    CommandSpec { id, label, menu, shortcut: None, params_doc, destructive, enabled: always, run }
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        spec("files.follow_site", "Follow Site", Some("Files/Follow Site"), r#"{"site": string}"#, false, follow),
        spec(
            "files.unfollow_site",
            "Stop Following Site",
            Some("Files/Stop Following Site"),
            r#"{"site": string}"#,
            false,
            unfollow,
        ),
        spec(
            "lists.create_item",
            "New List Item",
            Some("Files/New List Item"),
            r#"{"site": string, "list": string, "fields": {<internal column name>: value}}"#,
            false,
            create_item,
        ),
        spec(
            "lists.update_item",
            "Edit List Item",
            None,
            r#"{"site": string, "list": string, "item": string, "fields": {<internal column name>: value}}"#,
            false,
            update_item,
        ),
        spec(
            "lists.delete_item",
            "Delete List Item",
            None,
            r#"{"site": string, "list": string, "item": string}"#,
            true,
            delete_item,
        ),
    ]
}

pub(crate) fn queries() -> Vec<CommandSpec> {
    vec![
        spec("sites.list", "Followed Sites", None, "{}", false, sites_list),
        spec("sites.lists", "Site Lists", None, r#"{"site": string}"#, false, site_lists),
        spec(
            "lists.items",
            "List Items",
            None,
            r#"{"site": string, "list": string, "after"?: cursor, "limit"?: int}"#,
            false,
            list_items,
        ),
    ]
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SiteParams {
    site: Option<String>,
    list: Option<String>,
    item: Option<String>,
    fields: Option<Map<String, Value>>,
    after: Option<String>,
    limit: Option<u32>,
}

impl SiteParams {
    fn site(&self) -> Result<SiteId> {
        let raw = self.site.as_deref().ok_or_else(|| Error::params("`site` is required"))?;
        SiteId::new(raw.trim_start_matches("site:")).map_err(|_| Error::params("`site` is not a valid site id"))
    }
    fn list(&self) -> Result<ListId> {
        let raw = self.list.as_deref().ok_or_else(|| Error::params("`list` is required"))?;
        ListId::new(raw).map_err(|_| Error::params("`list` is not a valid list id"))
    }
    fn item(&self) -> Result<ListItemId> {
        let raw = self.item.as_deref().ok_or_else(|| Error::params("`item` is required"))?;
        ListItemId::new(raw).map_err(|_| Error::params("`item` is not a valid item id"))
    }
    fn fields(&self) -> Result<Map<String, Value>> {
        let f = self.fields.clone().ok_or_else(|| Error::params("`fields` is required"))?;
        check_fields(&f)?;
        Ok(f)
    }
}

/// Field names must be internal column names; sizes are bounded.
fn check_fields(f: &Map<String, Value>) -> Result<()> {
    if f.is_empty() || f.len() > MAX_FIELDS {
        return Err(Error::params("`fields` needs 1 to 256 columns"));
    }
    if f.keys().any(|k| k.is_empty() || k.len() > 255 || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')) {
        return Err(Error::params("field names must be internal column names (letters, digits, _)"));
    }
    if serde_json::to_string(f).map(|s| s.len()).unwrap_or(usize::MAX) > MAX_FIELDS_BYTES {
        return Err(Error::params("`fields` is too large"));
    }
    Ok(())
}

async fn enqueue_list(
    s: &Session,
    kind: &str,
    payload: &ListItemPayload,
    apply: impl FnOnce(&mut quorum_store::Tx<'_>) -> quorum_store::Result<()> + Send + 'static,
) -> Result<i64> {
    let store = s.store()?;
    let entry = quorum_store::NewOutboxEntry {
        kind: kind.to_owned(),
        resource: sf::list_resource(&payload.list),
        local_id: Some(payload.item.as_str().to_owned()),
        payload: serde_json::to_value(payload)
            .map_err(|e| Error::new(ErrorKind::Bug, "payload encoding failed").with_source(e))?,
    };
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

/// Finds a cached list item by id.
async fn find_item(store: &Store, list: &ListId, id: &ListItemId) -> Result<Option<ListItem>> {
    let (list, id) = (list.clone(), id.clone());
    store
        .read(move |r| {
            let mut after: Option<Cursor> = None;
            for _ in 0..MAX_SCAN_PAGES {
                let page = r.list_items(&list, after.as_ref(), quorum_store::MAX_PAGE)?;
                if let Some(found) = page.items.into_iter().find(|i| i.id == id) {
                    return Ok(Some(found));
                }
                match page.next {
                    Some(n) => after = Some(n),
                    None => break,
                }
            }
            Ok(None)
        })
        .await
        .map_err(store_err)
}

async fn set_followed(s: &Session, site: SiteId, follow: bool) -> Result<Value> {
    let store = s.store()?;
    let prev = fstate::followed(&store).await.map_err(sync_err)?.unwrap_or_default();
    let mut next = prev.clone();
    next.retain(|x| *x != site);
    if follow {
        next.push(site.clone());
    }
    let payload = FollowPayload { site, prev: Some(prev) };
    let entry = quorum_store::NewOutboxEntry {
        kind: if follow { kinds::FOLLOW_SITE } else { kinds::UNFOLLOW_SITE }.to_owned(),
        resource: sf::SITES_RESOURCE.to_owned(),
        local_id: None,
        payload: serde_json::to_value(&payload)
            .map_err(|e| Error::new(ErrorKind::Bug, "payload encoding failed").with_source(e))?,
    };
    let id = store
        .write(move |tx| {
            tx.set_setting(fstate::FOLLOWED, &next)?;
            tx.outbox_enqueue(&entry)
        })
        .await
        .map_err(store_err)?;
    s.sync()?.outbox_changed();
    Ok(json!({ "outbox": id.0 }))
}

fn follow(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let params: SiteParams = parse(p)?;
        set_followed(s, params.site()?, true).await
    })
}

fn unfollow(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let params: SiteParams = parse(p)?;
        set_followed(s, params.site()?, false).await
    })
}

fn create_item(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let params: SiteParams = parse(p)?;
        let (site, list, fields) = (params.site()?, params.list()?, params.fields()?);
        let local =
            ListItemId::new(format!("{LOCAL_PREFIX}{}", op_id())).map_err(|_| Error::new(ErrorKind::Bug, "bad id"))?;
        let now = Timestamp::now();
        let placeholder = ListItem {
            id: local.clone(),
            list_id: Some(list.clone()),
            fields: fields.clone(),
            created_date_time: Some(now),
            last_modified_date_time: Some(now),
            web_url: None,
        };
        let payload = ListItemPayload { site, list: list.clone(), item: local.clone(), fields, prev: None };
        let id = enqueue_list(s, kinds::LIST_CREATE, &payload, move |tx| tx.upsert_list_items(&list, &[placeholder]))
            .await?;
        Ok(json!({ "outbox": id, "id": local }))
    })
}

fn update_item(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let params: SiteParams = parse(p)?;
        let (site, list, item, fields) = (params.site()?, params.list()?, params.item()?, params.fields()?);
        if item.as_str().starts_with(LOCAL_PREFIX) {
            return Err(Error::new(ErrorKind::NotAvailable, "that item is still being created"));
        }
        let store = s.store()?;
        let prev = find_item(&store, &list, &item).await?;
        let mut next = prev.clone();
        if let Some(n) = next.as_mut() {
            for (k, v) in &fields {
                n.fields.insert(k.clone(), v.clone());
            }
            n.last_modified_date_time = Some(Timestamp::now());
        }
        let payload = ListItemPayload { site, list: list.clone(), item, fields, prev };
        let id = enqueue_list(s, kinds::LIST_UPDATE, &payload, move |tx| match next {
            Some(n) => tx.upsert_list_items(&list, &[n]),
            None => Ok(()),
        })
        .await?;
        Ok(json!({ "outbox": id }))
    })
}

fn delete_item(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let params: SiteParams = parse(p)?;
        let (site, list, item) = (params.site()?, params.list()?, params.item()?);
        if item.as_str().starts_with(LOCAL_PREFIX) {
            return Err(Error::new(ErrorKind::NotAvailable, "that item is still being created"));
        }
        let store = s.store()?;
        let prev = find_item(&store, &list, &item).await?;
        let payload = ListItemPayload { site, list: list.clone(), item: item.clone(), fields: Map::new(), prev };
        let id = enqueue_list(s, kinds::LIST_DELETE, &payload, move |tx| tx.delete_list_items(&list, &[item])).await?;
        Ok(json!({ "outbox": id }))
    })
}

fn sites_list(s: &Session, _p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let store = s.store()?;
        let followed = fstate::followed(&store).await.map_err(sync_err)?.unwrap_or_default();
        let sites = store.read(|r| r.sites()).await.map_err(store_err)?.items;
        let rows: Vec<Value> = followed
            .iter()
            .filter_map(|id| sites.iter().find(|x| &x.id == id))
            .map(|x| json!({ "id": x.id, "name": x.display_name.as_str(), "url_name": x.name, "web_url": x.web_url }))
            .collect();
        Ok(Value::Array(rows))
    })
}

fn site_lists(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let params: SiteParams = parse(p)?;
        let site = params.site()?;
        let store = s.store()?;
        let lists = store.read(move |r| r.lists(&site)).await.map_err(store_err)?.items;
        let rows: Vec<Value> = lists
            .iter()
            .map(|l| {
                json!({
                    "id": l.id,
                    "name": l.display_name.as_str(),
                    "template": l.template,
                    "library": l.template.as_deref() == Some("documentLibrary"),
                    "web_url": l.web_url,
                })
            })
            .collect();
        Ok(Value::Array(rows))
    })
}

/// The columns a list view shows: cached definitions without hidden and internal ones, or
/// (before they are known) the field names of the items.
pub(crate) fn visible_columns(cached: Option<Vec<ColumnInfo>>, items: &[ListItem]) -> Vec<ColumnInfo> {
    let internal = |n: &str| n.starts_with('_') || n.starts_with('@') || INTERNAL_COLUMNS.contains(&n);
    match cached {
        Some(c) if !c.is_empty() => {
            let mut cols: Vec<ColumnInfo> = c.into_iter().filter(|c| !c.hidden && !internal(&c.name)).collect();
            // Title first, as SharePoint shows it.
            if let Some(pos) = cols.iter().position(|c| c.name == "Title") {
                let t = cols.remove(pos);
                cols.insert(0, t);
            }
            cols.truncate(64);
            cols
        }
        _ => {
            let mut names: Vec<String> = Vec::new();
            for it in items.iter().take(200) {
                for k in it.fields.keys() {
                    if !internal(k) && !names.contains(k) && names.len() < 64 {
                        names.push(k.clone());
                    }
                }
            }
            if let Some(pos) = names.iter().position(|n| n == "Title") {
                let t = names.remove(pos);
                names.insert(0, t);
            }
            names
                .into_iter()
                .map(|n| ColumnInfo {
                    display_name: n.clone(),
                    name: n,
                    kind: "unknown".to_owned(),
                    choices: Vec::new(),
                    read_only: false,
                    required: false,
                    hidden: false,
                })
                .collect()
        }
    }
}

fn list_items(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let params: SiteParams = parse(p)?;
        let (site, list) = (params.site()?, params.list()?);
        let store = s.store()?;
        let key = sf::list_resource(&list);
        let synced = fstate::synced_at(&store, &key).await.map_err(sync_err)?;
        let ctx = s.sync()?.ctx().clone();
        let mut fetch_error = None;
        match synced {
            None => {
                match tokio::time::timeout(FIRST_FETCH_TIMEOUT, sf::live::fetch_list_items(&ctx, &site, &list)).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => fetch_error = Some(e.message().to_owned()),
                    Err(_) => fetch_error = Some("the list is still loading".to_owned()),
                }
            }
            Some(at)
                if fstate::now_ms().saturating_sub(at) > i64::try_from(STALE_LIST.as_millis()).unwrap_or(i64::MAX) =>
            {
                let (c, si, l) = (ctx.clone(), site.clone(), list.clone());
                tokio::spawn(async move {
                    if let Err(e) = sf::live::fetch_list_items(&c, &si, &l).await {
                        tracing::info!(target: "quorum::files", kind = ?e.kind(), "list refresh failed");
                    }
                });
            }
            Some(_) => {}
        }
        let after = params.after.map(Cursor::from_string);
        let limit = params.limit.unwrap_or(quorum_store::MAX_PAGE);
        let l = list.clone();
        let page = store.read(move |r| r.list_items(&l, after.as_ref(), limit)).await.map_err(store_err)?;
        let cols = visible_columns(fstate::columns(&store, &list).await.map_err(sync_err)?, &page.items);
        let now = Timestamp::now();
        let tz = TimeZone::system();
        let items: Vec<Value> = page
            .items
            .iter()
            .map(|it| {
                let when = it.last_modified_date_time.or(it.created_date_time);
                json!({
                    "id": it.id,
                    "fields": it.fields,
                    "modified": when,
                    "modified_label": time_label(when, now, &tz),
                    "web_url": it.web_url,
                    "pending": it.id.as_str().starts_with(LOCAL_PREFIX),
                })
            })
            .collect();
        Ok(json!({
            "site": site,
            "list": list,
            "columns": cols,
            "items": items,
            "next": page.next.map(|c| c.as_str().to_owned()),
            "error": fetch_error,
        }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_names_are_checked() {
        let ok: Map<String, Value> = serde_json::from_value(json!({ "Title": "x", "Due_Date": 1 })).unwrap_or_default();
        assert!(check_fields(&ok).is_ok());
        let bad: Map<String, Value> = serde_json::from_value(json!({ "Ti tle": "x" })).unwrap_or_default();
        assert!(check_fields(&bad).is_err());
        assert!(check_fields(&Map::new()).is_err());
        let big: Map<String, Value> =
            serde_json::from_value(json!({ "Title": "x".repeat(70_000) })).unwrap_or_default();
        assert!(check_fields(&big).is_err());
    }

    #[test]
    fn columns_fall_back_to_fields() {
        let it = ListItem {
            id: ListItemId::new("1").unwrap(),
            list_id: None,
            fields: serde_json::from_value(
                json!({ "Title": "a", "Status": "Open", "_ModerationStatus": 0, "@odata.etag": "x" }),
            )
            .unwrap_or_default(),
            created_date_time: None,
            last_modified_date_time: None,
            web_url: None,
        };
        let cols = visible_columns(None, std::slice::from_ref(&it));
        let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["Title", "Status"]);
    }
}
