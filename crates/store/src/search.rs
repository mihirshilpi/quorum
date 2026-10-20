//! Local full-text search over mail, chat and channel messages, file names and people.
//!
//! User input never reaches FTS5 query syntax: each word becomes a quoted prefix term
//! (`"word"*`), so quotes, `NEAR`, `AND`/`OR`/`NOT`, `*`, `^`, parentheses, column filters and
//! control characters are matched as text or dropped. Snippets are built in Rust from the
//! decrypted text (the FTS tables are contentless) and carry highlight ranges instead of
//! markup, so the UI never has to inject HTML.

use jiff::Timestamp;
use quorum_model::{
    ChannelId, ChatId, ChatMessage, ChatMessageId, DriveId, DriveItem, DriveItemId, MailAddress, MailFolderId, Message,
    MessageId, Person, Sensitive, TeamId, UserId,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::mail::plain_text;
use crate::store::{Reader, Store, tables};
use crate::util::{clamp_limit, from_json, truncate};

const MAX_QUERY_LEN: usize = 256;
const MAX_TERMS: usize = 16;
const MAX_SNIPPET_SOURCE: usize = 64 * 1024;
const SNIPPET_BEFORE: usize = 60;
const SNIPPET_LEN: usize = 200;
const MAX_HIGHLIGHTS: usize = 32;

/// What to search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SearchScope {
    /// Mail subject, sender, preview and cached body text.
    Mail,
    /// Chat message text.
    Chat,
    /// Channel post subjects and message text.
    Channel,
    /// Drive item names.
    Files,
    /// People names and email addresses.
    People,
}

impl SearchScope {
    /// Every scope.
    pub const ALL: [Self; 5] = [Self::Mail, Self::Chat, Self::Channel, Self::Files, Self::People];
}

/// What a search hit refers to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum HitItem {
    /// A mail message.
    #[serde(rename_all = "camelCase")]
    Mail {
        /// The message.
        id: MessageId,
        /// Its folder.
        folder: Option<MailFolderId>,
    },
    /// A chat message.
    #[serde(rename_all = "camelCase")]
    ChatMessage {
        /// The chat.
        chat: ChatId,
        /// The message.
        id: ChatMessageId,
    },
    /// A channel post or reply.
    #[serde(rename_all = "camelCase")]
    ChannelMessage {
        /// The team.
        team: TeamId,
        /// The channel.
        channel: ChannelId,
        /// The message.
        id: ChatMessageId,
        /// The post it replies to, for replies.
        reply_to: Option<ChatMessageId>,
    },
    /// A file or folder.
    #[serde(rename_all = "camelCase")]
    DriveItem {
        /// The drive.
        drive: DriveId,
        /// The item.
        id: DriveItemId,
    },
    /// A person.
    #[serde(rename_all = "camelCase")]
    Person {
        /// The user id, if known.
        user: Option<UserId>,
        /// The email address, if known.
        email: Option<MailAddress>,
    },
}

/// A short excerpt around the first match, with highlight ranges.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snippet {
    /// The excerpt (plain text; a leading `…` marks a cut).
    pub text: Sensitive,
    /// Matched byte ranges `[start, end)` within `text`, sorted and non-overlapping.
    pub highlights: Vec<(u32, u32)>,
}

/// One search result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    /// The scope it came from.
    pub scope: SearchScope,
    /// What it refers to.
    pub item: HitItem,
    /// A title: subject, sender, file name or person name.
    pub title: Sensitive,
    /// An excerpt around the match.
    pub snippet: Snippet,
    /// When the item was received, created or modified, if known.
    pub when: Option<Timestamp>,
    /// The BM25 rank within its scope (lower is better).
    pub rank: f64,
}

/// The parsed form of a user query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedQuery {
    /// The safe FTS5 MATCH expression.
    pub(crate) fts: String,
    /// Lower-cased words, for highlighting.
    pub(crate) words: Vec<String>,
}

/// Turns arbitrary user text into a safe FTS5 query (`None` if nothing searchable remains).
pub(crate) fn parse_query(input: &str) -> Option<ParsedQuery> {
    let cleaned: String =
        truncate(input, MAX_QUERY_LEN).chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let mut fts = Vec::new();
    let mut words = Vec::new();
    for term in cleaned.split_whitespace() {
        if fts.len() >= MAX_TERMS {
            break;
        }
        if !term.chars().any(char::is_alphanumeric) {
            continue;
        }
        fts.push(format!("\"{}\"*", term.replace('"', "\"\"")));
        for w in term.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
            if words.len() < MAX_TERMS * 4 {
                words.push(w.to_lowercase());
            }
        }
    }
    if fts.is_empty() { None } else { Some(ParsedQuery { fts: fts.join(" "), words }) }
}

/// Builds a snippet of `text` around the first occurrence of any of `words`.
pub(crate) fn make_snippet(text: &str, words: &[String]) -> Snippet {
    let text = truncate(text, MAX_SNIPPET_SOURCE);
    // Lower-case copy plus a map from each lower-case byte to its source byte offset.
    let mut lower = String::with_capacity(text.len());
    let mut map: Vec<usize> = Vec::with_capacity(text.len().saturating_add(1));
    for (i, c) in text.char_indices() {
        for lc in c.to_lowercase() {
            let before = lower.len();
            lower.push(lc);
            map.extend(std::iter::repeat_n(i, lower.len().saturating_sub(before)));
        }
    }
    map.push(text.len());
    let src = |p: usize| map.get(p).copied().unwrap_or(text.len());
    let first = words.iter().filter(|w| !w.is_empty()).filter_map(|w| lower.find(w.as_str())).min();
    let center = first.map_or(0, src);
    let mut start = center.saturating_sub(SNIPPET_BEFORE);
    while start > 0 && !text.is_char_boundary(start) {
        start = start.saturating_sub(1);
    }
    // Prefer starting at a word boundary.
    if start > 0
        && let Some(sp) = text.get(start..center).and_then(|s| s.find(' '))
    {
        start = start.saturating_add(sp).saturating_add(1);
    }
    let mut end = start.saturating_add(SNIPPET_LEN).min(text.len());
    while end < text.len() && !text.is_char_boundary(end) {
        end = end.saturating_add(1);
    }
    let window = text.get(start..end).unwrap_or("");
    let prefix = if start > 0 { "…" } else { "" };
    let shift = prefix.len();
    let mut highlights: Vec<(u32, u32)> = Vec::new();
    'outer: for w in words.iter().filter(|w| !w.is_empty()) {
        for (p, _) in lower.match_indices(w.as_str()) {
            if highlights.len() >= MAX_HIGHLIGHTS {
                break 'outer;
            }
            let (s, e) = (src(p), src(p.saturating_add(w.len())));
            if s >= start && e <= end && s < e {
                let rel = |x: usize| u32::try_from(x.saturating_sub(start).saturating_add(shift)).unwrap_or(u32::MAX);
                highlights.push((rel(s), rel(e)));
            }
        }
    }
    highlights.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::with_capacity(highlights.len());
    for (s, e) in highlights {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    Snippet { text: Sensitive::new(format!("{prefix}{window}")), highlights: merged }
}

fn is_fts_syntax_error(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(_, Some(msg)) if msg.contains("fts5"))
}

/// Runs a MATCH query; an FTS5 syntax error (which escaping should make impossible) is
/// logged and treated as no results.
fn run_match<T>(
    r: &Reader<'_>,
    sql: &str,
    fts: &str,
    limit: u32,
    map: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    let mut stmt = r.conn.prepare_cached(sql)?;
    let rows = stmt.query_map(params![fts, limit], map).and_then(|it| it.collect::<rusqlite::Result<Vec<T>>>());
    match rows {
        Ok(v) => Ok(v),
        Err(e) if is_fts_syntax_error(&e) => {
            tracing::warn!(name: "store.fts_syntax_error", "escaped search query was rejected by FTS5");
            Ok(Vec::new())
        }
        Err(e) => Err(e.into()),
    }
}

/// Search results across scopes.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResults {
    /// Hits, grouped by scope in the requested order, best first within each scope.
    pub hits: Vec<SearchHit>,
    /// Hits dropped because their rows could not be decoded.
    pub skipped: u32,
}

impl Reader<'_> {
    /// Searches the local index. `limit` (clamped to 1..=500) applies per scope. A query with
    /// nothing searchable in it returns no hits; hostile input never errors.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Sqlite`](crate::ErrorKind::Sqlite) if the database fails.
    pub fn search(&self, query: &str, scopes: &[SearchScope], limit: u32) -> Result<SearchResults> {
        let mut out = SearchResults::default();
        let Some(q) = parse_query(query) else { return Ok(out) };
        let limit = clamp_limit(limit);
        let mut seen = Vec::new();
        for &scope in scopes {
            if seen.contains(&scope) {
                continue;
            }
            seen.push(scope);
            match scope {
                SearchScope::Mail => self.search_mail(&q, limit, &mut out)?,
                SearchScope::Chat => self.search_chat(&q, limit, &mut out)?,
                SearchScope::Channel => self.search_channel(&q, limit, &mut out)?,
                SearchScope::Files => self.search_files(&q, limit, &mut out)?,
                SearchScope::People => self.search_people(&q, limit, &mut out)?,
            }
        }
        Ok(out)
    }

    fn skip(&self, out: &mut SearchResults, table: &'static str, e: &crate::Error) {
        self.ctx.stats.record_corrupt(table, e);
        out.skipped = out.skipped.saturating_add(1);
    }

    fn search_mail(&self, q: &ParsedQuery, limit: u32, out: &mut SearchResults) -> Result<()> {
        let rows = run_match(
            self,
            "SELECT m.id, m.json, f.rank FROM mail_fts f JOIN message m ON m.rid = f.rowid
             WHERE mail_fts MATCH ?1 ORDER BY f.rank LIMIT ?2",
            &q.fts,
            limit,
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, f64>(2)?)),
        )?;
        for (id, json, rank) in rows {
            let m: Message = match from_json(&json) {
                Ok(m) => m,
                Err(e) => {
                    self.skip(out, tables::MESSAGE, &e);
                    continue;
                }
            };
            let header = format!("{} {}", m.subject.as_str(), m.body_preview.as_str()).to_lowercase();
            // Excerpt the preview when the header explains the match; otherwise the body.
            let snippet = if q.words.iter().all(|w| header.contains(w.as_str())) {
                make_snippet(m.body_preview.as_str(), &q.words)
            } else {
                match self.body_text(&id) {
                    Ok(body) if !body.is_empty() => make_snippet(&body, &q.words),
                    _ => make_snippet(m.body_preview.as_str(), &q.words),
                }
            };
            out.hits.push(SearchHit {
                scope: SearchScope::Mail,
                item: HitItem::Mail { id: m.id.clone(), folder: m.parent_folder_id.clone() },
                title: m.subject.clone(),
                snippet,
                when: m.received_date_time.or(m.sent_date_time),
                rank,
            });
        }
        Ok(())
    }

    fn search_chat(&self, q: &ParsedQuery, limit: u32, out: &mut SearchResults) -> Result<()> {
        let rows = run_match(
            self,
            "SELECT c.chat_id, c.id, f.rank FROM chat_fts f JOIN chat_message c ON c.rid = f.rowid
             WHERE chat_fts MATCH ?1 ORDER BY f.rank LIMIT ?2",
            &q.fts,
            limit,
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, f64>(2)?)),
        )?;
        for (chat, id, rank) in rows {
            let found = (|| -> Result<Option<(ChatId, ChatMessage)>> {
                let chat = ChatId::new(chat)?;
                let id = ChatMessageId::new(id)?;
                Ok(self.chat_message(&quorum_model::MessageContainer::Chat(chat.clone()), &id)?.map(|m| (chat, m)))
            })();
            match found {
                Ok(Some((chat, m))) => out.hits.push(message_hit(
                    SearchScope::Chat,
                    HitItem::ChatMessage { chat, id: m.id.clone() },
                    &m,
                    q,
                    rank,
                )),
                Ok(None) => {}
                Err(e) => self.skip(out, tables::CHAT_MESSAGE, &e),
            }
        }
        Ok(())
    }

    fn search_channel(&self, q: &ParsedQuery, limit: u32, out: &mut SearchResults) -> Result<()> {
        let rows = run_match(
            self,
            "SELECT c.team_id, c.channel_id, c.id, f.rank FROM channel_fts f JOIN channel_message c ON c.rid = f.rowid
             WHERE channel_fts MATCH ?1 ORDER BY f.rank LIMIT ?2",
            &q.fts,
            limit,
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, f64>(3)?)),
        )?;
        for (team, channel, id, rank) in rows {
            let found = (|| -> Result<Option<(TeamId, ChannelId, ChatMessage)>> {
                let team = TeamId::new(team)?;
                let channel = ChannelId::new(channel)?;
                let container =
                    quorum_model::MessageContainer::Channel { team_id: team.clone(), channel_id: channel.clone() };
                Ok(self.chat_message(&container, &ChatMessageId::new(id)?)?.map(|m| (team, channel, m)))
            })();
            match found {
                Ok(Some((team, channel, m))) => {
                    let item =
                        HitItem::ChannelMessage { team, channel, id: m.id.clone(), reply_to: m.reply_to_id.clone() };
                    let mut hit = message_hit(SearchScope::Channel, item, &m, q, rank);
                    if let Some(subject) = m.subject.as_ref().filter(|s| !s.is_empty()) {
                        hit.title = subject.clone();
                    }
                    out.hits.push(hit);
                }
                Ok(None) => {}
                Err(e) => self.skip(out, tables::CHANNEL_MESSAGE, &e),
            }
        }
        Ok(())
    }

    fn search_files(&self, q: &ParsedQuery, limit: u32, out: &mut SearchResults) -> Result<()> {
        let rows = run_match(
            self,
            "SELECT d.json, f.rank FROM drive_fts f JOIN drive_item d ON d.rid = f.rowid
             WHERE drive_fts MATCH ?1 ORDER BY f.rank LIMIT ?2",
            &q.fts,
            limit,
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)),
        )?;
        for (json, rank) in rows {
            match from_json::<DriveItem>(&json) {
                Ok(it) => out.hits.push(SearchHit {
                    scope: SearchScope::Files,
                    item: HitItem::DriveItem { drive: it.drive_id.clone(), id: it.id.clone() },
                    snippet: make_snippet(it.name.as_str(), &q.words),
                    title: it.name.clone(),
                    when: it.last_modified_date_time,
                    rank,
                }),
                Err(e) => self.skip(out, tables::DRIVE_ITEM, &e),
            }
        }
        Ok(())
    }

    fn search_people(&self, q: &ParsedQuery, limit: u32, out: &mut SearchResults) -> Result<()> {
        let rows = run_match(
            self,
            "SELECT p.json, f.rank FROM person_fts f JOIN person p ON p.rid = f.rowid
             WHERE person_fts MATCH ?1 ORDER BY f.rank LIMIT ?2",
            &q.fts,
            limit,
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)),
        )?;
        for (json, rank) in rows {
            match from_json::<Person>(&json) {
                Ok(p) => out.hits.push(SearchHit {
                    scope: SearchScope::People,
                    snippet: make_snippet(p.email.as_ref().map_or("", |e| e.as_str()), &q.words),
                    item: HitItem::Person { user: p.id.clone(), email: p.email.clone() },
                    title: p.display_name.clone(),
                    when: None,
                    rank,
                }),
                Err(e) => self.skip(out, tables::PERSON, &e),
            }
        }
        Ok(())
    }
}

fn message_hit(scope: SearchScope, item: HitItem, m: &ChatMessage, q: &ParsedQuery, rank: f64) -> SearchHit {
    let text = plain_text(&m.body.content_type, m.body.content.as_str());
    SearchHit {
        scope,
        item,
        title: m.from.as_ref().map(|f| f.display_name.clone()).unwrap_or_default(),
        snippet: make_snippet(&text, &q.words),
        when: m.created_date_time.or(m.last_modified_date_time),
        rank,
    }
}

impl Store {
    /// See [`Reader::search`].
    ///
    /// # Errors
    ///
    /// As [`Reader::search`].
    pub async fn search(&self, query: String, scopes: Vec<SearchScope>, limit: u32) -> Result<SearchResults> {
        self.read(move |r| r.search(&query, &scopes, limit)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_queries_are_quoted() {
        assert_eq!(parse_query("").map(|q| q.fts), None);
        assert_eq!(parse_query("\" ( ) * ^ :").map(|q| q.fts), None);
        assert_eq!(parse_query("a\"b").unwrap().fts, "\"a\"\"b\"*");
        assert_eq!(parse_query("NEAR(x y)").unwrap().fts, "\"NEAR(x\"* \"y)\"*");
        assert_eq!(parse_query("x\0y").unwrap().fts, "\"x\"* \"y\"*");
    }

    #[test]
    fn snippet_highlights_match() {
        let s = make_snippet("Hello World, the Budget is ready", &["budget".to_owned()]);
        let (a, b) = s.highlights[0];
        assert_eq!(&s.text.as_str()[a as usize..b as usize], "Budget");
        let long = format!("{} needle {}", "word ".repeat(100), "tail ".repeat(100));
        let s = make_snippet(&long, &["needle".to_owned()]);
        assert!(s.text.as_str().starts_with('…'));
        let (a, b) = s.highlights[0];
        assert_eq!(&s.text.as_str()[a as usize..b as usize], "needle");
        // Case folding that changes byte length must not break offsets.
        let s = make_snippet("İstanbul trip", &["trip".to_owned()]);
        let (a, b) = s.highlights[0];
        assert_eq!(&s.text.as_str()[a as usize..b as usize], "trip");
    }
}
