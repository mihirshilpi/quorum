//! Universal search across mail, chats, channels, files, people and events.
//!
//! Query `search.all {query, scopes?, limit?, remote?}` merges the local full-text index
//! (`Store::search`) with Microsoft Search (`/search/query`) when Graph answers in time, and
//! groups the hits. Every hit carries an `open` command id and params, so the UI, agents and
//! the CLI can open it the same way. Command `search.open {query?}` (Cmd+F) shows the results
//! view.

use std::collections::HashSet;

use futures::future::join_all;
use quorum_graph::api::search::{EntityType, SearchHit as RemoteHit, SearchQuery, SearchResult};
use quorum_ipc::CoreEvent;
use quorum_model::Sensitive;
use quorum_store::{HitItem, SearchHit, SearchScope};
use serde::Deserialize;
use serde_json::{Value, json};

use super::people::{REMOTE_TIMEOUT, clean_query, store_err};
use crate::command::{CommandFuture, CommandSpec, always, parse};
use crate::error::Error;
use crate::session::Session;

/// Default hits per group.
const DEFAULT_LIMIT: u32 = 8;
/// Most hits per group.
const MAX_LIMIT: u32 = 50;
/// Longest snippet kept from a Graph summary (characters).
const MAX_SNIPPET_CHARS: usize = 300;
/// Most highlight ranges per snippet.
const MAX_HIGHLIGHTS: usize = 32;

/// A result group, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Group {
    Mail,
    Chats,
    Channels,
    Files,
    People,
    Events,
}

impl Group {
    const ALL: [Self; 6] = [Self::Mail, Self::Chats, Self::Channels, Self::Files, Self::People, Self::Events];

    fn id(self) -> &'static str {
        match self {
            Self::Mail => "mail",
            Self::Chats => "chats",
            Self::Channels => "channels",
            Self::Files => "files",
            Self::People => "people",
            Self::Events => "events",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Mail => "Mail",
            Self::Chats => "Chats",
            Self::Channels => "Channels",
            Self::Files => "Files",
            Self::People => "People",
            Self::Events => "Events",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        let s = s.as_str();
        match s {
            "mail" | "messages" | "message" => Some(Self::Mail),
            "chat" | "chats" => Some(Self::Chats),
            "channel" | "channels" => Some(Self::Channels),
            "file" | "files" => Some(Self::Files),
            "people" | "person" => Some(Self::People),
            "event" | "events" | "calendar" => Some(Self::Events),
            _ => None,
        }
    }

    fn local_scope(self) -> Option<SearchScope> {
        match self {
            Self::Mail => Some(SearchScope::Mail),
            Self::Chats => Some(SearchScope::Chat),
            Self::Channels => Some(SearchScope::Channel),
            Self::Files => Some(SearchScope::Files),
            Self::People => Some(SearchScope::People),
            Self::Events => None,
        }
    }

    fn remote_type(self) -> EntityType {
        match self {
            Self::Mail => EntityType::Message,
            Self::Chats | Self::Channels => EntityType::ChatMessage,
            Self::Files => EntityType::DriveItem,
            Self::People => EntityType::Person,
            Self::Events => EntityType::Event,
        }
    }
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "search.open",
        label: "Search Everything",
        menu: Some("Edit/Find/Search Everything"),
        shortcut: Some("Cmd+F"),
        params_doc: r#"{"query"?: string} (shows the search results view)"#,
        destructive: false,
        enabled: always,
        run: open,
    }]
}

pub(crate) fn queries() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "search.all",
        label: "Search Everything",
        menu: None,
        shortcut: None,
        params_doc: r#"{"query": string, "scopes"?: ["mail"|"chats"|"channels"|"files"|"people"|"events"], "limit"?: 1..50 per group (default 8), "remote"?: bool (default true)} -> {query, remote, groups: [{scope, label, hits: [{key, scope, title, subtitle, snippet: {text, highlights: [[start, end]]}, when, source, open: {command, params}}]}]}"#,
        destructive: false,
        enabled: always,
        run: all,
    }]
}

#[derive(Deserialize)]
struct OpenParams {
    #[serde(default)]
    query: Option<String>,
}

fn open(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: OpenParams = parse(p)?;
        let q = p.query.as_deref().map(clean_query);
        s.emit(CoreEvent::Ui { command: "search.show".into(), params: json!({ "query": q }) });
        Ok(Value::Null)
    })
}

#[derive(Deserialize)]
struct AllParams {
    query: String,
    #[serde(default)]
    scopes: Option<Vec<Value>>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    remote: Option<bool>,
}

/// One hit as returned to callers.
#[derive(Debug, Clone)]
pub(crate) struct Hit {
    group: Group,
    key: String,
    value: Value,
}

fn snippet_json(text: &str, highlights: &[(u32, u32)]) -> Value {
    json!({ "text": text, "highlights": highlights.iter().map(|(a, b)| json!([a, b])).collect::<Vec<_>>() })
}

/// Text of a chat preview, cut to [`MAX_SNIPPET_CHARS`].
pub(crate) fn preview_text(p: &quorum_model::ChatPreview) -> String {
    p.body_preview.as_str().chars().take(MAX_SNIPPET_CHARS).collect()
}

/// Maps a local hit to its group and JSON.
fn local(h: &SearchHit) -> Option<Hit> {
    let (group, key, open) = match &h.item {
        HitItem::Mail { id, folder } => (
            Group::Mail,
            format!("mail:{}", id.as_str()),
            json!({ "command": "view.open", "params": { "area": "mail", "id": id.as_str(), "folder": folder.as_ref().map(|f| f.as_str()) } }),
        ),
        HitItem::ChatMessage { chat, id } => (
            Group::Chats,
            format!("chat:{}", id.as_str()),
            json!({ "command": "chat.open", "params": { "id": chat.as_str(), "message": id.as_str() } }),
        ),
        HitItem::ChannelMessage { team, channel, id, reply_to } => (
            Group::Channels,
            format!("channel:{}", id.as_str()),
            json!({ "command": "channel.open", "params": { "team": team.as_str(), "channel": channel.as_str(), "post": reply_to.as_ref().unwrap_or(id).as_str() } }),
        ),
        HitItem::DriveItem { drive, id } => (
            Group::Files,
            format!("file:{}", id.as_str()),
            json!({ "command": "view.open", "params": { "area": "files", "id": id.as_str(), "drive": drive.as_str() } }),
        ),
        HitItem::Person { user, email } => {
            let target = user
                .as_ref()
                .map(|u| u.as_str().to_owned())
                .or_else(|| email.as_ref().map(|e| e.as_str().to_lowercase()))?;
            (
                Group::People,
                format!("person:{target}"),
                json!({ "command": "people.open", "params": { "user": target } }),
            )
        }
    };
    let value = json!({
        "key": key,
        "scope": group.id(),
        "title": h.title.as_str(),
        "subtitle": Value::Null,
        "snippet": snippet_json(h.snippet.text.as_str(), &h.snippet.highlights),
        "when": h.when,
        "source": "local",
        "open": open,
    });
    Some(Hit { group, key, value })
}

/// A local hit's JSON (used by `people.person` for recent mail).
pub(crate) fn local_hit(h: &SearchHit) -> Option<Value> {
    local(h).map(|h| h.value)
}

/// Turns a Graph summary (`<c0>match</c0>`, `<ddd/>` for an ellipsis, untrusted) into plain
/// text and byte highlight ranges. Other tags are dropped; output is capped.
pub(crate) fn parse_summary(summary: &str) -> (String, Vec<(u32, u32)>) {
    let mut out = String::new();
    let mut highlights = Vec::new();
    let mut open: Option<usize> = None;
    let mut rest = summary;
    let mut chars = 0usize;
    while let Some(c) = rest.chars().next() {
        if chars >= MAX_SNIPPET_CHARS {
            break;
        }
        if c == '<'
            && let Some(end) =
                rest.get(1..).and_then(|r| r.char_indices().take(12).find(|(_, ch)| *ch == '>').map(|(i, _)| i + 1))
            && let Some(tag) = rest.get(1..end)
            && tag.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '/')
        {
            match tag {
                "c0" => {
                    if open.is_none() {
                        open = Some(out.len());
                    }
                }
                "/c0" => {
                    if let Some(start) = open.take()
                        && out.len() > start
                        && highlights.len() < MAX_HIGHLIGHTS
                    {
                        highlights.push((u32::try_from(start).unwrap_or(0), u32::try_from(out.len()).unwrap_or(0)));
                    }
                }
                "ddd/" => {
                    out.push('…');
                    chars = chars.saturating_add(1);
                }
                _ => {}
            }
            rest = rest.get(end.saturating_add(1)..).unwrap_or("");
            continue;
        }
        out.push(if c.is_control() { ' ' } else { c });
        chars = chars.saturating_add(1);
        rest = rest.get(c.len_utf8()..).unwrap_or("");
    }
    if let Some(start) = open
        && out.len() > start
        && highlights.len() < MAX_HIGHLIGHTS
    {
        highlights.push((u32::try_from(start).unwrap_or(0), u32::try_from(out.len()).unwrap_or(0)));
    }
    (out, highlights)
}

fn s_text(s: &Sensitive) -> String {
    s.as_str().chars().take(MAX_SNIPPET_CHARS).collect()
}

/// Maps a Graph hit to its group and JSON (`None` for types we don't show).
fn remote(r: &SearchResult) -> Option<Hit> {
    let (text, highlights) = parse_summary(r.summary.as_str());
    let snippet = snippet_json(&text, &highlights);
    let (group, key, title, subtitle, when, open) = match &r.hit {
        RemoteHit::Message(m) => (
            Group::Mail,
            format!("mail:{}", m.id.as_str()),
            s_text(&m.subject),
            m.from.as_ref().map(|f| s_text(&f.name)),
            json!(m.received_date_time),
            json!({ "command": "view.open", "params": { "area": "mail", "id": m.id.as_str(), "folder": m.parent_folder_id.as_ref().map(|f| f.as_str()) } }),
        ),
        RemoteHit::Event(e) => (
            Group::Events,
            format!("event:{}", e.id.as_str()),
            s_text(&e.subject),
            e.organizer.as_ref().map(|o| s_text(&o.name)),
            json!(e.start.to_timestamp().ok()),
            json!({ "command": "view.open", "params": { "area": "calendar", "id": e.id.as_str() } }),
        ),
        RemoteHit::DriveItem(d) => (
            Group::Files,
            format!("file:{}", d.id),
            s_text(&d.name),
            None,
            json!(d.last_modified),
            json!({ "command": "view.open", "params": { "area": "files", "id": d.id, "drive": d.drive_id } }),
        ),
        RemoteHit::ChatMessage(c) => {
            let title = s_text(&c.from);
            match (&c.channel, &c.chat_id) {
                (Some((team, channel)), _) => (
                    Group::Channels,
                    format!("channel:{}", c.id),
                    title,
                    None,
                    json!(c.created),
                    json!({ "command": "channel.open", "params": { "team": team, "channel": channel, "post": c.id } }),
                ),
                (None, Some(chat)) => (
                    Group::Chats,
                    format!("chat:{}", c.id),
                    title,
                    None,
                    json!(c.created),
                    json!({ "command": "chat.open", "params": { "id": chat, "message": c.id } }),
                ),
                (None, None) => return None,
            }
        }
        RemoteHit::Person(p) => {
            let target = super::people::person_key(p)?;
            let target = target.strip_prefix("mail:").map(str::to_owned).unwrap_or(target);
            (
                Group::People,
                format!("person:{target}"),
                s_text(&p.display_name),
                p.job_title.as_ref().map(s_text),
                Value::Null,
                json!({ "command": "people.open", "params": { "user": target } }),
            )
        }
        _ => return None,
    };
    let value = json!({
        "key": key,
        "scope": group.id(),
        "title": title,
        "subtitle": subtitle,
        "snippet": snippet,
        "when": when,
        "source": "remote",
        "open": open,
    });
    Some(Hit { group, key, value })
}

/// Groups `hits` (local first) by [`Group::ALL`] order, de-duplicated by key, `limit` each.
pub(crate) fn group_hits(groups: &[Group], hits: Vec<Hit>, limit: usize) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for g in Group::ALL {
        if !groups.contains(&g) {
            continue;
        }
        let list: Vec<Value> = hits
            .iter()
            .filter(|h| h.group == g)
            .filter(|h| seen.insert(h.key.clone()))
            .take(limit)
            .map(|h| h.value.clone())
            .collect();
        if !list.is_empty() {
            out.push(json!({ "scope": g.id(), "label": g.label(), "hits": list }));
        }
    }
    out
}

async fn remote_hits(s: &Session, q: &str, groups: &[Group], limit: u32) -> Option<Vec<Hit>> {
    let mut types: Vec<EntityType> = Vec::new();
    for g in groups {
        let t = g.remote_type();
        if !types.contains(&t) {
            types.push(t);
        }
    }
    let graph = s.graph();
    let calls = types.into_iter().map(|t| {
        let mut query = SearchQuery::new(vec![t], q.to_owned());
        query.size = limit;
        async move { graph.search(&query).await }
    });
    let results = tokio::time::timeout(REMOTE_TIMEOUT, join_all(calls)).await.ok()?;
    let mut any_ok = false;
    let mut out = Vec::new();
    for r in results {
        match r {
            Ok(page) => {
                any_ok = true;
                out.extend(page.results.iter().filter_map(remote).filter(|h| groups.contains(&h.group)));
            }
            Err(e) => tracing::debug!(target: "quorum::search", kind = ?e.kind(), "remote search failed"),
        }
    }
    any_ok.then_some(out)
}

fn all(s: &Session, p: Value) -> CommandFuture<'_> {
    Box::pin(async move {
        let p: AllParams = parse(p)?;
        let q = clean_query(&p.query);
        let limit = p.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let groups: Vec<Group> = match &p.scopes {
            None => Group::ALL.to_vec(),
            Some(list) if list.len() > 16 => return Err(Error::params("too many scopes")),
            Some(list) => list.iter().filter_map(Value::as_str).filter_map(Group::parse).collect(),
        };
        if q.is_empty() || groups.is_empty() {
            return Ok(json!({ "query": q, "remote": false, "groups": [] }));
        }
        let store = s.store()?;
        let scopes: Vec<SearchScope> = groups.iter().filter_map(|g| g.local_scope()).collect();
        let local_results = store.search(q.clone(), scopes, limit).await.map_err(store_err)?;
        let mut hits: Vec<Hit> = local_results.hits.iter().filter_map(local).collect();
        let remote_list = if p.remote.unwrap_or(true) { remote_hits(s, &q, &groups, limit).await } else { None };
        let remote_ok = remote_list.is_some();
        hits.extend(remote_list.unwrap_or_default());
        Ok(json!({ "query": q, "remote": remote_ok, "groups": group_hits(&groups, hits, limit as usize) }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_highlights_are_byte_ranges() {
        let (t, h) = parse_summary("Hello <c0>Synthetic</c0> world<ddd/>");
        assert_eq!(t, "Hello Synthetic world…");
        assert_eq!(h, vec![(6, 15)]);
        assert_eq!(t.get(6..15), Some("Synthetic"));
    }

    #[test]
    fn hostile_summaries_never_panic() {
        for s in ["<", "<c0>", "</c0>x", "<<<<c0>>>", "é<c0>ü</c0>", "<script>alert(1)</script>", "\u{0}<c0\u{0}>"] {
            let (t, h) = parse_summary(s);
            for (a, b) in h {
                assert!(t.get(a as usize..b as usize).is_some(), "{s}");
            }
        }
        let long = "<c0>a</c0>".repeat(10_000);
        let (t, h) = parse_summary(&long);
        assert!(t.chars().count() <= MAX_SNIPPET_CHARS);
        assert!(h.len() <= MAX_HIGHLIGHTS);
        assert!(!parse_summary("<script>x</script>").0.contains("script"));
    }

    #[test]
    fn groups_parse_leniently() {
        assert_eq!(Group::parse(" Mail "), Some(Group::Mail));
        assert_eq!(Group::parse("calendar"), Some(Group::Events));
        assert_eq!(Group::parse("nope"), None);
    }
}
