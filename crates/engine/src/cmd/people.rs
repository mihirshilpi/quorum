//! People: person cards, pickers and the org chart.
//!
//! Commands: `people.open` (asks the UI to show the person card), `people.email`
//! (`mail.compose` to them), `people.chat` (`chat.new` with them), `people.schedule`
//! (`calendar.compose` with them as attendee).
//!
//! Queries: `people.search` (local prefix + full-text, merged with a directory search when
//! Graph answers in time; used by every picker), `people.person` (profile, org chart, presence,
//! recent mail and chats), `people.list` (relevance-ranked people), `people.photo` (cached data
//! URL or `null`; a miss starts a background fetch and announces table `photo` when done).
//!
//! Person rows everywhere have the shape
//! `{id, display_name, name, email, job_title, department, office_location, phone, presence,
//! availability, activity, status_message}`, where `id` is the user id (or `mail:<address>` for
//! people outside the directory) and `presence` is the short id (`available`, `busy`, `dnd`,
//! `brb`, `away`, `offline`, `unknown`).

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use jiff::Timestamp;
use quorum_ipc::CoreEvent;
use quorum_model::{GraphEnum, Person, UserId};
use quorum_store::{CachedPresence, SearchScope};
use quorum_sync::people as sp;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::command::{CommandFuture, CommandSpec, always, parse};
use crate::error::{Error, ErrorKind, Result};
use crate::session::Session;

/// Longest a query waits for Graph before answering from the store alone.
pub(crate) const REMOTE_TIMEOUT: Duration = Duration::from_secs(3);
/// Most rows `people.search` returns.
pub(crate) const MAX_SEARCH_LIMIT: u32 = 50;
/// Longest search text accepted (longer input is cut).
pub(crate) const MAX_QUERY_CHARS: usize = 256;
/// Table name announced when a photo lands in the cache.
pub const PHOTO_TABLE: &str = "photo";

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
    Error::new(kind, "Microsoft Graph request failed").with_source(e)
}

/// Runs a remote lookup with [`REMOTE_TIMEOUT`]; `None` on timeout or failure.
pub(crate) async fn remote<T>(f: impl std::future::Future<Output = quorum_sync::Result<T>>) -> Option<T> {
    match tokio::time::timeout(REMOTE_TIMEOUT, f).await {
        Ok(Ok(v)) => Some(v),
        Ok(Err(e)) => {
            tracing::debug!(target: "quorum::people", kind = ?e.kind(), "remote lookup failed");
            None
        }
        Err(_) => None,
    }
}

/// Cleans search text: control characters dropped, trimmed, cut to [`MAX_QUERY_CHARS`].
pub(crate) fn clean_query(q: &str) -> String {
    let s: String = q.chars().filter(|c| !c.is_control()).take(MAX_QUERY_CHARS).collect();
    s.trim().to_owned()
}

/// The row id of a person: user id, else `mail:<lower-cased address>`.
pub(crate) fn person_key(p: &Person) -> Option<String> {
    if let Some(id) = &p.id {
        return Some(id.as_str().to_owned());
    }
    p.email.as_ref().map(|e| format!("mail:{}", e.as_str().to_lowercase()))
}

/// A person row (shape in the module docs).
pub(crate) fn person_row(p: &Person, presence: Option<&CachedPresence>) -> Value {
    let opt = |s: &Option<quorum_model::Sensitive>| s.as_ref().map(|v| v.as_str().to_owned());
    let name = p.display_name.as_str();
    let email = p.email.as_ref().map(|e| e.as_str().to_owned());
    let display = if name.trim().is_empty() { email.clone().unwrap_or_default() } else { name.to_owned() };
    let mut row = json!({
        "id": person_key(p),
        "user_id": p.id.as_ref().map(UserId::as_str),
        "display_name": display,
        "name": display,
        "email": email,
        "job_title": opt(&p.job_title),
        "department": opt(&p.department),
        "office_location": opt(&p.office_location),
        "phone": opt(&p.phone),
    });
    if let Some(obj) = row.as_object_mut() {
        for (k, v) in presence_fields(presence) {
            obj.insert(k.to_owned(), v);
        }
    }
    row
}

/// `presence`, `availability`, `activity`, `status_message` of a cached presence.
pub(crate) fn presence_fields(p: Option<&CachedPresence>) -> Vec<(&'static str, Value)> {
    match p {
        None => vec![
            ("presence", json!("unknown")),
            ("availability", Value::Null),
            ("activity", Value::Null),
            ("status_message", Value::Null),
        ],
        Some(c) => vec![
            ("presence", json!(sp::short_id(&c.presence.availability))),
            ("availability", json!(c.presence.availability.as_graph())),
            ("activity", json!(c.presence.activity.as_graph())),
            ("status_message", json!(c.presence.current_status_message(Timestamp::now()))),
        ],
    }
}

/// Cached presences of `people`, by user id.
pub(crate) async fn presences_of(s: &Session, ids: Vec<UserId>) -> Result<HashMap<UserId, CachedPresence>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let store = s.store()?;
    let page = store.presences(ids).await.map_err(store_err)?;
    Ok(page.items.into_iter().filter_map(|c| c.presence.user_id.clone().map(|id| (id, c))).collect())
}

/// Rows for `people`, with presence.
pub(crate) async fn rows(s: &Session, people: &[Person]) -> Result<Vec<Value>> {
    let ids: Vec<UserId> = people.iter().filter_map(|p| p.id.clone()).collect();
    let presence = presences_of(s, ids).await?;
    Ok(people.iter().map(|p| person_row(p, p.id.as_ref().and_then(|id| presence.get(id)))).collect())
}

/// Resolves a `user` param (user id, `mail:<address>` or a bare address) to the stored person.
pub(crate) async fn resolve(s: &Session, user: &str) -> Result<(Option<UserId>, Option<Person>)> {
    let user = user.trim();
    if user.is_empty() || user.len() > quorum_model::MAX_ID_LEN {
        return Err(Error::params("`user` must be a user id or an email address"));
    }
    let store = s.store()?;
    let address = user.strip_prefix("mail:").unwrap_or(user);
    if address.contains('@') {
        let addr = address.to_owned();
        let page = store.people_by_prefix(addr.clone(), 10).await.map_err(store_err)?;
        let found =
            page.items.into_iter().find(|p| p.email.as_ref().is_some_and(|e| e.as_str().eq_ignore_ascii_case(&addr)));
        let id = found.as_ref().and_then(|p| p.id.clone());
        let person = found.or_else(|| {
            quorum_model::MailAddress::new(addr.clone()).ok().map(|e| Person { email: Some(e), ..Person::default() })
        });
        return Ok((id, person));
    }
    let id = UserId::new(user).map_err(|_| Error::params("`user` must be a user id or an email address"))?;
    let lookup = id.clone();
    let person = store.read(move |r| r.person(&lookup)).await.map_err(store_err)?;
    Ok((Some(id), person))
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    let spec = |id, label, menu, run| CommandSpec {
        id,
        label,
        menu: Some(menu),
        shortcut: None,
        params_doc: r#"{"user": string (user id or email address)}"#,
        destructive: false,
        enabled: always,
        run,
    };
    vec![
        spec("people.open", "Show Person Card", "People/Show Person Card", open),
        spec("people.email", "Email Person", "People/Email", email),
        spec("people.chat", "Chat with Person", "People/Chat", chat),
        spec("people.schedule", "Schedule Meeting with Person", "People/Schedule Meeting", schedule),
    ]
}

pub(crate) fn queries() -> Vec<CommandSpec> {
    let spec = |id, label, params_doc, run| CommandSpec {
        id,
        label,
        menu: None,
        shortcut: None,
        params_doc,
        destructive: false,
        enabled: always,
        run,
    };
    vec![
        spec(
            "people.search",
            "Search People",
            r#"{"query": string, "limit"?: 1..50 (default 10), "remote"?: bool (default true)} -> [person row]"#,
            search,
        ),
        spec(
            "people.person",
            "Person Details",
            r#"{"user": string} -> {person, managers: [row], reports: [row], recent_mail: [...], recent_chats: [...]}"#,
            person,
        ),
        spec("people.list", "Relevant People", r#"{"limit"?: 1..100} -> [person row]"#, list),
        spec("people.photo", "Person Photo", r#"{"user": string} -> data URL string or null"#, photo),
    ]
}

#[derive(Deserialize)]
struct UserParams {
    #[serde(alias = "id")]
    user: String,
}

fn open(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: UserParams = parse(p)?;
        let user = p.user.trim();
        if user.is_empty() || user.len() > quorum_model::MAX_ID_LEN || user.chars().any(char::is_control) {
            return Err(Error::params("`user` must be a user id or an email address"));
        }
        s.emit(CoreEvent::Ui { command: "people.card.show".into(), params: json!({ "user": user }) });
        Ok(Value::Null)
    })
}

async fn email_of(s: &Session, user: &str) -> Result<(Option<UserId>, String)> {
    let (id, person) = resolve(s, user).await?;
    let email = person.and_then(|p| p.email).map(|e| e.as_str().to_owned());
    match email {
        Some(e) => Ok((id, e)),
        None => Err(Error::new(ErrorKind::NotFound, "no email address is known for this person")),
    }
}

/// Runs the first of `ids` that is registered.
async fn delegate(s: &Session, ids: &[&str], params: Value) -> Result<Value> {
    for id in ids {
        if s.registry().get(id).is_ok() {
            return Box::pin(s.execute(id, params)).await;
        }
    }
    Err(Error::new(ErrorKind::NotAvailable, "this isn't available yet"))
}

fn email(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: UserParams = parse(p)?;
        let (_, address) = email_of(s, &p.user).await?;
        delegate(s, &["mail.compose"], json!({ "to": [address] })).await
    })
}

fn chat(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: UserParams = parse(p)?;
        let (id, person) = resolve(s, &p.user).await?;
        let email = person.and_then(|p| p.email).map(|e| e.as_str().to_owned());
        if id.is_none() && email.is_none() {
            return Err(Error::new(ErrorKind::NotFound, "this person is not in the directory"));
        }
        let members: Vec<String> = id.iter().map(|i| i.as_str().to_owned()).collect();
        let emails: Vec<String> = if members.is_empty() { email.into_iter().collect() } else { Vec::new() };
        delegate(s, &["chat.new"], json!({ "members": members, "emails": emails })).await
    })
}

fn schedule(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: UserParams = parse(p)?;
        let (_, address) = email_of(s, &p.user).await?;
        delegate(s, &["calendar.compose"], json!({ "attendees": [address] })).await
    })
}

#[derive(Deserialize)]
struct SearchParams {
    query: String,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    remote: Option<bool>,
}

/// Merges local and remote people: local first, de-duplicated by id and by address.
pub(crate) fn merge_people(local: Vec<Person>, remote: Vec<Person>, limit: usize) -> Vec<Person> {
    let mut seen_ids = HashSet::new();
    let mut seen_mail = HashSet::new();
    let mut out = Vec::new();
    for p in local.into_iter().chain(remote) {
        if out.len() >= limit {
            break;
        }
        let id = p.id.as_ref().map(|i| i.as_str().to_owned());
        let mail = p.email.as_ref().map(|e| e.as_str().to_lowercase());
        if id.is_none() && mail.is_none() {
            continue;
        }
        if id.as_ref().is_some_and(|i| seen_ids.contains(i)) || mail.as_ref().is_some_and(|m| seen_mail.contains(m)) {
            continue;
        }
        if let Some(i) = id {
            seen_ids.insert(i);
        }
        if let Some(m) = mail {
            seen_mail.insert(m);
        }
        out.push(p);
    }
    out
}

/// Local people matching `q`: name/address prefix, then word-prefix full-text hits.
async fn local_people(s: &Session, q: &str, limit: u32) -> Result<Vec<Person>> {
    let store = s.store()?;
    let mut people = store.people_by_prefix(q.to_owned(), limit).await.map_err(store_err)?.items;
    let hits = store.search(q.to_owned(), vec![SearchScope::People], limit).await.map_err(store_err)?;
    let ids: Vec<UserId> = hits
        .hits
        .into_iter()
        .filter_map(|h| match h.item {
            quorum_store::HitItem::Person { user: Some(u), .. } => Some(u),
            _ => None,
        })
        .collect();
    let more = store
        .read(move |r| {
            let mut v = Vec::new();
            for id in &ids {
                if let Some(p) = r.person(id)? {
                    v.push(p);
                }
            }
            Ok(v)
        })
        .await
        .map_err(store_err)?;
    people.extend(more);
    Ok(people)
}

fn search(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: SearchParams = parse(p)?;
        let q = clean_query(&p.query);
        let limit = p.limit.unwrap_or(10).clamp(1, MAX_SEARCH_LIMIT);
        if q.is_empty() {
            return Ok(json!([]));
        }
        let local = local_people(s, &q, limit).await?;
        let remote_hits = if p.remote.unwrap_or(true) && local.len() < limit as usize {
            match s.sync() {
                Ok(sync) => remote(sp::directory_search(sync.ctx(), &q, limit)).await.unwrap_or_default(),
                Err(_) => Vec::new(),
            }
        } else {
            Vec::new()
        };
        let merged = merge_people(local, remote_hits, limit as usize);
        Ok(Value::Array(rows(s, &merged).await?))
    })
}

#[derive(Deserialize)]
struct ListParams {
    #[serde(default)]
    limit: Option<u32>,
}

fn list(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: ListParams = parse(p)?;
        let limit = p.limit.unwrap_or(sp::MAX_RELEVANT).clamp(1, sp::MAX_RELEVANT) as usize;
        let store = s.store()?;
        let mut people = sp::relevant(&store).await.map_err(sync_err)?;
        // The user's org (manager and reports) is relevant too.
        if let Some(me) = sp::my_id(&store).await.map_err(sync_err)?
            && let Some(org) = sp::cached_org(&store, &me).await.map_err(sync_err)?
        {
            people.extend(org.managers.into_iter().take(1));
            people.extend(org.reports);
        }
        let people = merge_people(people, Vec::new(), limit);
        Ok(Value::Array(rows(s, &people).await?))
    })
}

fn person(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: UserParams = parse(p)?;
        let (id, mut person) = resolve(s, &p.user).await?;
        let store = s.store()?;
        let sync = s.sync().ok();
        if person.is_none()
            && let (Some(id), Some(sync)) = (&id, &sync)
        {
            person = remote(sp::fetch_person(sync.ctx(), id)).await;
        }
        let Some(person) = person else { return Err(Error::new(ErrorKind::NotFound, "no such person")) };
        let mut org = match &id {
            Some(id) => sp::cached_org(&store, id).await.map_err(sync_err)?,
            None => None,
        };
        let stale = org.as_ref().is_none_or(|o| {
            Timestamp::now().as_second().saturating_sub(o.fetched_at.as_second()) > sp::ORG_MAX_AGE_SECS
        });
        if stale
            && let (Some(id), Some(sync)) = (&id, &sync)
            && let Some(fresh) = remote(sp::fetch_org(sync.ctx(), id)).await
        {
            org = Some(fresh);
        }
        let org = org.unwrap_or_else(|| sp::OrgChart { fetched_at: Timestamp::now(), ..sp::OrgChart::default() });
        let row = rows(s, std::slice::from_ref(&person)).await?.into_iter().next().unwrap_or(Value::Null);
        let managers = rows(s, &org.managers).await?;
        let reports = rows(s, &org.reports).await?;
        let recent_mail = match &person.email {
            Some(e) => recent_mail(s, e.as_str()).await?,
            None => Vec::new(),
        };
        let recent_chats = match &id {
            Some(id) => recent_chats(s, id).await?,
            None => Vec::new(),
        };
        Ok(json!({
            "person": row,
            "managers": managers,
            "reports": reports,
            "recent_mail": recent_mail,
            "recent_chats": recent_chats,
        }))
    })
}

async fn recent_mail(s: &Session, address: &str) -> Result<Vec<Value>> {
    let store = s.store()?;
    let hits = store.search(address.to_owned(), vec![SearchScope::Mail], 5).await.map_err(store_err)?;
    Ok(hits.hits.iter().filter_map(crate::cmd::search::local_hit).collect())
}

async fn recent_chats(s: &Session, user: &UserId) -> Result<Vec<Value>> {
    let store = s.store()?;
    let user = user.clone();
    let chats = store
        .read(move |r| {
            let mut out = Vec::new();
            for id in r.chats_with_member(&user)?.into_iter().take(5) {
                if let Some(c) = r.chat(&id)? {
                    out.push(c);
                }
            }
            Ok(out)
        })
        .await
        .map_err(store_err)?;
    Ok(chats
        .into_iter()
        .map(|c| {
            let topic = c.topic.as_ref().map(|t| t.as_str().to_owned()).filter(|t| !t.trim().is_empty());
            let preview = c.last_message_preview.as_ref().map(crate::cmd::search::preview_text);
            json!({
                "id": c.id.as_str(),
                "topic": topic,
                "preview": preview,
                "when": c.last_updated_date_time,
                "open": { "command": "chat.open", "params": { "id": c.id.as_str() } },
            })
        })
        .collect())
}

/// User ids whose photo fetch is in flight (so repeated queries don't refetch).
fn in_flight() -> &'static Mutex<HashSet<UserId>> {
    static SET: OnceLock<Mutex<HashSet<UserId>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

fn data_url(content_type: &str, bytes: &[u8]) -> String {
    format!("data:{content_type};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn photo(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: UserParams = parse(p)?;
        let (Some(id), _) = resolve(s, &p.user).await? else { return Ok(Value::Null) };
        let store = s.store()?;
        match sp::cached_photo(&store, &id).await.map_err(sync_err)? {
            Some(Some((ty, bytes))) => return Ok(Value::String(data_url(&ty, &bytes))),
            Some(None) => return Ok(Value::Null),
            None => {}
        }
        let Ok(sync) = s.sync() else { return Ok(Value::Null) };
        let first = in_flight().lock().unwrap_or_else(PoisonError::into_inner).insert(id.clone());
        if first {
            let session = s.clone();
            tokio::spawn(async move {
                let r = sp::fetch_photo(sync.ctx(), &id).await;
                in_flight().lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
                if matches!(r, Ok(Some(_))) {
                    session.emit(CoreEvent::StoreChanged { tables: vec![PHOTO_TABLE.into()] });
                }
            });
        }
        Ok(Value::Null)
    })
}
