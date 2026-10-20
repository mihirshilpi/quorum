//! People: the relevance-ranked list (`/me/people`), org chart (manager chain and direct
//! reports), cached directory lookups, and lazily fetched photos.

use futures::future::BoxFuture;
use jiff::Timestamp;
use quorum_graph::api::mail::Mailbox;
use quorum_graph::api::people::PhotoSize;
use quorum_model::{Person, UserId};
use serde::{Deserialize, Serialize};

use super::{ME_ID_SETTING, RELEVANT_SETTING};
use crate::{Cadence, Ctx, Outcome, Resource, Result};

/// Most people kept in the relevance list.
pub const MAX_RELEVANT: u32 = 100;
/// Most managers followed up the chain.
pub const MAX_MANAGER_CHAIN: usize = 8;
/// Largest photo kept (bytes); bigger ones are treated as missing.
pub const MAX_PHOTO_BYTES: usize = 256 * 1024;
/// Most directory hits one search returns.
pub const MAX_DIRECTORY_HITS: u32 = 50;
/// Org charts older than this are refreshed when someone opens the person.
pub const ORG_MAX_AGE_SECS: i64 = 24 * 60 * 60;

/// Settings key of a person's cached org chart.
#[must_use]
pub fn org_setting(user: &UserId) -> String {
    format!("people.org:{}", user.as_str())
}

/// Blob key of a person's cached small photo.
#[must_use]
pub fn photo_blob(user: &UserId) -> String {
    format!("photo:64:{}", user.as_str())
}

/// A person's place in the org: manager chain (nearest first) and direct reports.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrgChart {
    /// Manager, their manager, … (nearest first).
    #[serde(default)]
    pub managers: Vec<Person>,
    /// Direct reports.
    #[serde(default)]
    pub reports: Vec<Person>,
    /// When it was fetched.
    pub fetched_at: Timestamp,
}

/// The relevance-ranked people list, refreshed every few minutes.
#[derive(Debug, Default)]
pub struct RelevantPeople;

impl Resource for RelevantPeople {
    fn key(&self) -> String {
        "people.relevant".into()
    }

    fn area(&self) -> &'static str {
        "people"
    }

    fn cadence(&self) -> Cadence {
        Cadence::secs(600, 1800, 3600)
    }

    fn run<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(async move {
            let page = ctx.graph.list_people(None, Some(MAX_RELEVANT)).await?;
            let people: Vec<Person> = page.items.into_iter().take(MAX_RELEVANT as usize).collect();
            ctx.store
                .write(move |tx| {
                    tx.upsert_people(&people)?;
                    tx.set_setting(RELEVANT_SETTING, &people)
                })
                .await?;
            // The user's own org chart, so the People area opens instantly.
            let me: Option<String> = ctx.store.setting(ME_ID_SETTING.to_owned()).await?;
            if let Some(me) = me.and_then(|s| UserId::new(s).ok()) {
                let _ = fetch_org(ctx, &me).await;
            }
            Ok(Outcome::CaughtUp)
        })
    }
}

/// The cached relevance list (empty before the first sync).
///
/// # Errors
/// [`crate::ErrorKind::Store`] if the store fails.
pub async fn relevant(store: &quorum_store::Store) -> Result<Vec<Person>> {
    let list: Option<Vec<Person>> = store.setting(RELEVANT_SETTING.to_owned()).await?;
    Ok(list.unwrap_or_default())
}

/// The cached org chart of `user`, if any.
///
/// # Errors
/// [`crate::ErrorKind::Store`] if the store fails.
pub async fn cached_org(store: &quorum_store::Store, user: &UserId) -> Result<Option<OrgChart>> {
    Ok(store.setting(org_setting(user)).await?)
}

/// Fetches `user`'s manager chain and direct reports, caches them (people and chart).
///
/// # Errors
/// Graph or store failures.
pub async fn fetch_org(ctx: &Ctx, user: &UserId) -> Result<OrgChart> {
    let mut managers: Vec<Person> = Vec::new();
    let mut seen = vec![user.clone()];
    let mut current = user.clone();
    for _ in 0..MAX_MANAGER_CHAIN {
        let Some(m) = ctx.graph.get_manager(&Mailbox::user(current.as_str())).await? else { break };
        let Some(id) = m.id.clone() else { break };
        if seen.contains(&id) {
            break; // a cycle in hostile or odd directory data
        }
        seen.push(id.clone());
        managers.push(m);
        current = id;
    }
    let reports = match ctx.graph.list_direct_reports(&Mailbox::user(user.as_str())).await {
        Ok(page) => page.items,
        Err(e) if e.kind() == quorum_graph::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    let chart = OrgChart { managers, reports, fetched_at: Timestamp::now() };
    let key = org_setting(user);
    let stored = chart.clone();
    ctx.store
        .write(move |tx| {
            tx.upsert_people(&stored.managers)?;
            tx.upsert_people(&stored.reports)?;
            tx.set_setting(&key, &stored)
        })
        .await?;
    Ok(chart)
}

/// Looks `user` up in the directory and caches them.
///
/// # Errors
/// Graph or store failures.
pub async fn fetch_person(ctx: &Ctx, user: &UserId) -> Result<Person> {
    let person = ctx.graph.get_user(&Mailbox::user(user.as_str())).await?;
    ctx.store.upsert_people(vec![person.clone()]).await?;
    Ok(person)
}

/// Directory search by name or address (`/users?$search`), caching the hits.
///
/// # Errors
/// Graph or store failures.
pub async fn directory_search(ctx: &Ctx, query: &str, limit: u32) -> Result<Vec<Person>> {
    let limit = limit.clamp(1, MAX_DIRECTORY_HITS);
    let page = ctx.graph.search_users(query, Some(limit)).await?;
    let people: Vec<Person> = page.items.into_iter().take(limit as usize).collect();
    ctx.store.upsert_people(people.clone()).await?;
    Ok(people)
}

/// A cached photo: `None` = not fetched yet, `Some(None)` = the person has no photo.
///
/// # Errors
/// [`crate::ErrorKind::Store`] if the store fails.
pub async fn cached_photo(store: &quorum_store::Store, user: &UserId) -> Result<Option<Option<(String, Vec<u8>)>>> {
    let blob = store.get_blob(photo_blob(user)).await?;
    Ok(blob.map(|bytes| image_type(&bytes).map(|t| (t.to_owned(), bytes))))
}

/// Fetches `user`'s small photo into the blob cache. A missing, oversized or non-image photo is
/// cached as "none" (an empty blob) so it is not fetched again.
///
/// # Errors
/// Graph (other than 404) or store failures.
pub async fn fetch_photo(ctx: &Ctx, user: &UserId) -> Result<Option<(String, Vec<u8>)>> {
    let photo = ctx.graph.get_photo(&Mailbox::user(user.as_str()), Some(PhotoSize::S64)).await?;
    let bytes = photo.map(|p| p.bytes).filter(|b| b.len() <= MAX_PHOTO_BYTES && image_type(b).is_some());
    let stored = bytes.clone().unwrap_or_default();
    ctx.store.put_blob(photo_blob(user), stored).await?;
    Ok(bytes.and_then(|b| image_type(&b).map(|t| (t.to_owned(), b))))
}

/// The MIME type of a raster image by its magic bytes (JPEG, PNG, GIF, WebP); `None` for
/// anything else, including SVG, so a hostile "photo" can never carry markup.
#[must_use]
pub fn image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP".as_slice()) {
        Some("image/webp")
    } else {
        None
    }
}
