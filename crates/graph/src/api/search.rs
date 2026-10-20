//! Microsoft Search (`POST /search/query`) across mail, events, files, chat messages, people,
//! list items and sites, with `from`/`size` paging. Hits map to a [`SearchHit`] enum by the
//! resource's `@odata.type`; mail and event hits reuse the mail and calendar mappers.
//!
//! Graph restricts which entity types may be combined in one request (for example `message`
//! and `event` each go alone or with each other only, `person` alone); this module passes the
//! combination through and Graph answers 400 for an invalid one. Page size is capped at 25 when
//! the request includes mail, events, chat messages or people, and at 500 otherwise, per the
//! Search API limits.

use std::fmt;

use jiff::Timestamp;
use quorum_model::{Event, Message, Person, Sensitive};
use serde_json::{Value, json};

use super::mail::{SensitiveJson, config};
use crate::client::Client;
use crate::error::{Error, ErrorKind, Result};
use crate::request::{Request, ResourceFamily};

/// Page size cap for mail, events, chat messages and people.
pub const MAX_SIZE_SMALL: u32 = 25;
/// Page size cap for files, list items and sites.
pub const MAX_SIZE_LARGE: u32 = 500;
/// Longest query string accepted (Graph's KQL limit is far above what a person types).
const MAX_QUERY_LEN: usize = 1024;

/// A searchable entity type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntityType {
    /// Mail (`message`).
    Message,
    /// Calendar events (`event`).
    Event,
    /// OneDrive and SharePoint files (`driveItem`).
    DriveItem,
    /// Teams chat and channel messages (`chatMessage`).
    ChatMessage,
    /// People (`person`).
    Person,
    /// SharePoint list items (`listItem`).
    ListItem,
    /// SharePoint sites (`site`).
    Site,
}

impl EntityType {
    /// The Graph name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Event => "event",
            Self::DriveItem => "driveItem",
            Self::ChatMessage => "chatMessage",
            Self::Person => "person",
            Self::ListItem => "listItem",
            Self::Site => "site",
        }
    }

    fn small_pages(self) -> bool {
        matches!(self, Self::Message | Self::Event | Self::ChatMessage | Self::Person)
    }

    fn family(self) -> ResourceFamily {
        match self {
            Self::Message => ResourceFamily::Mail,
            Self::Event => ResourceFamily::Calendar,
            Self::ChatMessage => ResourceFamily::Teams,
            Self::DriveItem | Self::ListItem | Self::Site => ResourceFamily::Files,
            Self::Person => ResourceFamily::Other,
        }
    }
}

/// A search request.
#[derive(Clone, PartialEq, Eq)]
pub struct SearchQuery {
    /// What to search.
    pub entity_types: Vec<EntityType>,
    /// The KQL query string as typed (personal; `Debug` redacts it).
    pub query: String,
    /// Offset of the first hit (`from`).
    pub from: u32,
    /// Hits per page (`size`), capped per entity type.
    pub size: u32,
}

impl fmt::Debug for SearchQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SearchQuery")
            .field("entity_types", &self.entity_types)
            .field("query_len", &self.query.len())
            .field("from", &self.from)
            .field("size", &self.size)
            .finish()
    }
}

impl SearchQuery {
    /// A query for `text` over `entity_types`, first page of 25.
    #[must_use]
    pub fn new(entity_types: Vec<EntityType>, text: impl Into<String>) -> Self {
        Self { entity_types, query: text.into(), from: 0, size: MAX_SIZE_SMALL }
    }

    /// The page size after the per-type cap.
    #[must_use]
    pub fn effective_size(&self) -> u32 {
        let cap = if self.entity_types.iter().any(|t| t.small_pages()) { MAX_SIZE_SMALL } else { MAX_SIZE_LARGE };
        self.size.clamp(1, cap)
    }

    /// The same query, next page.
    #[must_use]
    pub fn page_from(&self, from: u32) -> Self {
        Self { from, ..self.clone() }
    }
}

/// Builds `POST /search/query`. Search only reads, so it is retry-safe.
///
/// # Errors
/// [`ErrorKind::Config`] for no entity types or an empty query.
pub fn search_request(query: &SearchQuery) -> Result<Request> {
    if query.entity_types.is_empty() {
        return Err(config("search needs at least one entity type"));
    }
    let text: String = query.query.chars().filter(|c| !c.is_control()).take(MAX_QUERY_LEN).collect();
    if text.trim().is_empty() {
        return Err(config("search query is empty"));
    }
    let mut types: Vec<&str> = Vec::new();
    for t in &query.entity_types {
        if !types.contains(&t.as_str()) {
            types.push(t.as_str());
        }
    }
    let body = json!({
        "requests": [{
            "entityTypes": types,
            "query": { "queryString": text },
            "from": query.from,
            "size": query.effective_size(),
        }]
    });
    let family = query.entity_types.first().map_or(ResourceFamily::Other, |t| t.family());
    Ok(Request::post("/search/query").family(family).json_value(body).idempotent(true))
}

/// A file hit (`driveItem`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveItemHit {
    /// Item id.
    pub id: String,
    /// Drive id (from `parentReference`).
    pub drive_id: Option<String>,
    /// File name.
    pub name: Sensitive,
    /// Web link.
    pub web_url: Option<String>,
    /// Size in bytes.
    pub size: Option<u64>,
    /// Last modified.
    pub last_modified: Option<Timestamp>,
}

/// A chat or channel message hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessageHit {
    /// Message id.
    pub id: String,
    /// The chat id, for chat messages.
    pub chat_id: Option<String>,
    /// The channel identity (`teamId`, `channelId`), for channel posts.
    pub channel: Option<(String, String)>,
    /// Sender display name.
    pub from: Sensitive,
    /// When it was created.
    pub created: Option<Timestamp>,
    /// Web link (opens Teams).
    pub web_url: Option<String>,
}

/// A SharePoint list item or site hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteHit {
    /// Item or site id.
    pub id: String,
    /// Name or title.
    pub name: Sensitive,
    /// Web link.
    pub web_url: Option<String>,
}

/// What a hit is.
#[derive(Debug, Clone, PartialEq)]
pub enum SearchHit {
    /// A mail message.
    Message(Message),
    /// A calendar event.
    Event(Event),
    /// A file.
    DriveItem(DriveItemHit),
    /// A Teams message.
    ChatMessage(ChatMessageHit),
    /// A person.
    Person(Person),
    /// A list item.
    ListItem(SiteHit),
    /// A site.
    Site(SiteHit),
    /// A resource type this version does not map (kept as JSON; `Debug` redacts it).
    Unknown {
        /// The `@odata.type`.
        odata_type: String,
        /// The resource.
        resource: SensitiveJson,
    },
}

/// One hit with Graph's ranking and highlighted summary.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    /// Graph's hit id.
    pub hit_id: String,
    /// Rank (1 = best), if given.
    pub rank: Option<u32>,
    /// Highlighted summary (contains `<c0>` markers; untrusted).
    pub summary: Sensitive,
    /// The mapped resource.
    pub hit: SearchHit,
}

/// One page of search results.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SearchPage {
    /// Hits, in Graph's order across containers.
    pub results: Vec<SearchResult>,
    /// Total hits Graph reports (sum over containers), if any.
    pub total: Option<u64>,
    /// Whether more results exist.
    pub more_results_available: bool,
    /// The `from` of the next page, when more exist.
    pub next_from: Option<u32>,
    /// Hits that could not be mapped.
    pub skipped: usize,
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(|s| s.chars().take(2048).collect())
}

fn ts_field(v: &Value, key: &str) -> Option<Timestamp> {
    v.get(key).and_then(Value::as_str).and_then(|s| s.parse().ok())
}

fn non_empty_id(v: &Value) -> Option<String> {
    str_field(v, "id").filter(|s| !s.is_empty() && s.len() <= quorum_model::MAX_ID_LEN)
}

fn map_hit(resource: Value) -> Option<SearchHit> {
    let odata = resource.get("@odata.type").and_then(Value::as_str).unwrap_or("").to_owned();
    let short = odata.trim_start_matches('#').trim_start_matches("microsoft.graph.").to_ascii_lowercase();
    Some(match short.as_str() {
        "message" => SearchHit::Message(super::mail::message_from_json(resource)?),
        "event" => SearchHit::Event(super::calendar::event_from_json(resource, None)?),
        "driveitem" => SearchHit::DriveItem(DriveItemHit {
            id: non_empty_id(&resource)?,
            drive_id: resource.get("parentReference").and_then(|p| str_field(p, "driveId")),
            name: Sensitive::new(str_field(&resource, "name").unwrap_or_default()),
            web_url: str_field(&resource, "webUrl"),
            size: resource.get("size").and_then(Value::as_u64),
            last_modified: ts_field(&resource, "lastModifiedDateTime"),
        }),
        "chatmessage" => SearchHit::ChatMessage(ChatMessageHit {
            id: non_empty_id(&resource)?,
            chat_id: str_field(&resource, "chatId"),
            channel: resource
                .get("channelIdentity")
                .and_then(|c| Some((str_field(c, "teamId")?, str_field(c, "channelId")?))),
            from: Sensitive::new(
                resource
                    .get("from")
                    .and_then(|f| f.get("emailAddress").or_else(|| f.get("user")))
                    .and_then(|e| str_field(e, "name").or_else(|| str_field(e, "displayName")))
                    .unwrap_or_default(),
            ),
            created: ts_field(&resource, "createdDateTime"),
            web_url: str_field(&resource, "webLink").or_else(|| str_field(&resource, "webUrl")),
        }),
        "person" => SearchHit::Person(search_person(&resource)),
        "listitem" => SearchHit::ListItem(SiteHit {
            id: non_empty_id(&resource)?,
            name: Sensitive::new(str_field(&resource, "name").unwrap_or_default()),
            web_url: str_field(&resource, "webUrl"),
        }),
        "site" => SearchHit::Site(SiteHit {
            id: non_empty_id(&resource)?,
            name: Sensitive::new(
                str_field(&resource, "displayName").or_else(|| str_field(&resource, "name")).unwrap_or_default(),
            ),
            web_url: str_field(&resource, "webUrl"),
        }),
        _ => SearchHit::Unknown { odata_type: odata.chars().take(128).collect(), resource: SensitiveJson(resource) },
    })
}

fn search_person(v: &Value) -> Person {
    let email = v
        .get("emailAddresses")
        .or_else(|| v.get("scoredEmailAddresses"))
        .and_then(Value::as_array)
        .and_then(|list| list.iter().find_map(|e| str_field(e, "address")))
        .and_then(|a| quorum_model::MailAddress::new(a).ok());
    let opt = |key: &str| str_field(v, key).filter(|s| !s.trim().is_empty()).map(Sensitive::new);
    Person {
        id: str_field(v, "id").and_then(|id| quorum_model::UserId::new(id).ok()),
        display_name: Sensitive::new(str_field(v, "displayName").unwrap_or_default()),
        email,
        job_title: opt("jobTitle"),
        department: opt("department"),
        office_location: opt("officeLocation"),
        phone: v
            .get("phones")
            .and_then(Value::as_array)
            .and_then(|l| l.iter().find_map(|p| str_field(p, "number")))
            .map(Sensitive::new),
    }
}

/// Parses a `/search/query` response body (exposed for tests and the mock server).
#[must_use]
pub fn parse_search_response(body: &Value, from: u32) -> SearchPage {
    let mut page = SearchPage::default();
    let containers = body
        .get("value")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| r.get("hitsContainers").and_then(Value::as_array))
        .flatten();
    let mut total: Option<u64> = None;
    for c in containers {
        if let Some(t) = c.get("total").and_then(Value::as_u64) {
            total = Some(total.unwrap_or(0).saturating_add(t));
        }
        if c.get("moreResultsAvailable").and_then(Value::as_bool).unwrap_or(false) {
            page.more_results_available = true;
        }
        for hit in c.get("hits").and_then(Value::as_array).into_iter().flatten() {
            let mapped = hit.get("resource").cloned().and_then(map_hit).map(|h| SearchResult {
                hit_id: str_field(hit, "hitId").unwrap_or_default(),
                rank: hit.get("rank").and_then(Value::as_u64).and_then(|r| u32::try_from(r).ok()),
                summary: Sensitive::new(str_field(hit, "summary").unwrap_or_default()),
                hit: h,
            });
            match mapped {
                Some(r) => page.results.push(r),
                None => page.skipped = page.skipped.saturating_add(1),
            }
        }
    }
    page.total = total;
    let returned = u32::try_from(page.results.len().saturating_add(page.skipped)).unwrap_or(u32::MAX);
    if page.more_results_available && returned > 0 {
        page.next_from = from.checked_add(returned);
    }
    page
}

impl Client {
    /// Runs a search and returns one page. For the next page call again with
    /// `query.page_from(page.next_from)`.
    ///
    /// # Errors
    /// As [`search_request`] and [`Client::send_json`].
    pub async fn search(&self, query: &SearchQuery) -> Result<SearchPage> {
        let body: Value = self.send_json(search_request(query)?).await?;
        if body.get("value").is_none() {
            return Err(Error::new(ErrorKind::Malformed, "search response has no value"));
        }
        Ok(parse_search_response(&body, query.from))
    }
}
