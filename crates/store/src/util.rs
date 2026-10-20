//! Shared helpers: time conversion, paging, cursors, row decoding, text extraction.

use std::sync::atomic::{AtomicU64, Ordering};

use jiff::Timestamp;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The largest page any list query returns; larger requested limits are clamped.
pub const MAX_PAGE: u32 = 500;

/// Clamps a requested page size to `1..=MAX_PAGE`.
pub(crate) fn clamp_limit(limit: u32) -> u32 {
    limit.clamp(1, MAX_PAGE)
}

/// Milliseconds since the epoch for an instant.
pub(crate) fn ms(ts: Timestamp) -> i64 {
    ts.as_millisecond()
}

/// Milliseconds for an optional instant (`0` when absent, which sorts oldest).
pub(crate) fn ms_or_zero(ts: Option<Timestamp>) -> i64 {
    ts.map_or(0, ms)
}

/// An instant from stored milliseconds; out-of-range values clamp to the epoch.
pub(crate) fn ts(ms: i64) -> Timestamp {
    Timestamp::from_millisecond(ms).unwrap_or(Timestamp::UNIX_EPOCH)
}

/// Converts a `u64` to SQLite's `i64`, saturating.
pub(crate) fn i64_from_u64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// Converts a stored `i64` back to `u64` (negative values become 0).
pub(crate) fn u64_from_i64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// Counts rows that could not be decoded, process-wide per store.
#[derive(Debug, Default)]
pub(crate) struct Stats {
    pub(crate) corrupt_rows: AtomicU64,
}

impl Stats {
    pub(crate) fn record_corrupt(&self, table: &'static str, err: &Error) {
        self.corrupt_rows.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(name: "store.corrupt_row", table, kind = %err.kind(), "skipped a stored row that could not be decoded");
    }
}

/// Decodes a JSON column into a model type.
pub(crate) fn from_json<T: DeserializeOwned>(json: &str) -> Result<T> {
    Ok(serde_json::from_str(json)?)
}

/// Encodes a model type as JSON.
pub(crate) fn to_json<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|e| Error::new(crate::ErrorKind::Bug, "json encode failed").with_source(e))
}

/// One page of results.
///
/// Rows that could not be decoded are skipped and counted in `skipped` (and in
/// [`Store::corrupt_rows_skipped`](crate::Store::corrupt_rows_skipped)); a corrupt row never
/// fails the whole page.
#[derive(Debug, Clone, PartialEq)]
pub struct Page<T> {
    /// The items, in the query's order.
    pub items: Vec<T>,
    /// Pass to the same query as `after` to get the next page; `None` when this is the last.
    pub next: Option<Cursor>,
    /// How many rows were skipped because they could not be decoded.
    pub skipped: u32,
}

impl<T> Page<T> {
    pub(crate) fn new() -> Self {
        Self { items: Vec::new(), next: None, skipped: 0 }
    }
}

/// An opaque keyset-paging cursor (the sort key of the last item of a page).
///
/// Serialises as a string so it can travel over IPC; a cursor that does not parse is rejected
/// with [`ErrorKind::InvalidInput`](crate::ErrorKind::InvalidInput).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Cursor(String);

impl Cursor {
    pub(crate) fn from_key(sort: i64, id: &str) -> Self {
        Self(format!("{sort}:{id}"))
    }

    pub(crate) fn from_text_key(sort: &str, id: &str) -> Self {
        // Length-prefixed so either part may contain ':'.
        Self(format!("t{}:{sort}{id}", sort.len()))
    }

    pub(crate) fn key(&self) -> Result<(i64, &str)> {
        let (sort, id) = self.0.split_once(':').ok_or_else(|| Error::invalid("malformed cursor"))?;
        let sort = sort.parse::<i64>().map_err(|_| Error::invalid("malformed cursor"))?;
        Ok((sort, id))
    }

    pub(crate) fn text_key(&self) -> Result<(&str, &str)> {
        let rest = self.0.strip_prefix('t').ok_or_else(|| Error::invalid("malformed cursor"))?;
        let (len, rest) = rest.split_once(':').ok_or_else(|| Error::invalid("malformed cursor"))?;
        let len = len.parse::<usize>().map_err(|_| Error::invalid("malformed cursor"))?;
        rest.split_at_checked(len).ok_or_else(|| Error::invalid("malformed cursor"))
    }

    /// The cursor as a string (for IPC).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Wraps a string previously obtained from [`Cursor::as_str`].
    #[must_use]
    pub fn from_string(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

/// Builds a keyset page from up to `limit + 1` raw rows `(sort key, id, payload)`.
///
/// Rows whose payload fails to decode are skipped and counted; the cursor still advances past
/// them so a corrupt row can never stall paging.
pub(crate) fn keyset_page<R, T>(
    stats: &Stats,
    table: &'static str,
    mut rows: Vec<(i64, String, R)>,
    limit: u32,
    decode: impl Fn(R) -> Result<T>,
) -> Page<T> {
    let limit = limit as usize;
    let has_more = rows.len() > limit;
    rows.truncate(limit);
    let next = if has_more { rows.last().map(|(sort, id, _)| Cursor::from_key(*sort, id)) } else { None };
    let mut page = decode_all(stats, table, rows.into_iter().map(|(_, _, r)| r), decode);
    page.next = next;
    page
}

/// Decodes every payload, skipping and counting corrupt ones.
pub(crate) fn decode_all<R, T>(
    stats: &Stats,
    table: &'static str,
    rows: impl IntoIterator<Item = R>,
    decode: impl Fn(R) -> Result<T>,
) -> Page<T> {
    let mut page = Page::new();
    for raw in rows {
        match decode(raw) {
            Ok(item) => page.items.push(item),
            Err(e) => {
                stats.record_corrupt(table, &e);
                page.skipped = page.skipped.saturating_add(1);
            }
        }
    }
    page
}

/// Truncates `s` to at most `max` bytes on a char boundary.
pub(crate) fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    s.get(..end).unwrap_or("")
}

/// The most text indexed for one item; longer bodies are indexed up to this many bytes.
pub(crate) const MAX_INDEXED_TEXT: usize = 256 * 1024;

/// Extracts indexable plain text from (untrusted) HTML: drops tags, comments, `<script>` and
/// `<style>` contents, decodes common entities and collapses whitespace. Bounded by
/// [`MAX_INDEXED_TEXT`]; this is for search only, never for display.
pub(crate) fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len().min(MAX_INDEXED_TEXT));
    let mut rest = html;
    let mut skip_until: Option<&'static str> = None;
    let mut last_space = true;
    let push_char = |out: &mut String, c: char, last_space: &mut bool| {
        if c.is_whitespace() {
            if !*last_space {
                out.push(' ');
                *last_space = true;
            }
        } else {
            out.push(c);
            *last_space = false;
        }
    };
    while !rest.is_empty() && out.len() < MAX_INDEXED_TEXT {
        if let Some(end_tag) = skip_until {
            match find_ascii_ci(rest, end_tag) {
                Some(pos) => {
                    rest = rest.get(pos..).unwrap_or("");
                    skip_until = None;
                }
                None => break,
            }
            continue;
        }
        if let Some(after) = rest.strip_prefix("<!--") {
            rest = after.find("-->").and_then(|p| after.get(p.saturating_add(3)..)).unwrap_or("");
            continue;
        }
        if let Some(after) = rest.strip_prefix('<') {
            let end = after.find('>').unwrap_or(after.len());
            let tag = after.get(..end).unwrap_or("");
            let name: String =
                tag.trim_start_matches('/').chars().take_while(char::is_ascii_alphanumeric).collect::<String>();
            if !tag.starts_with('/') {
                if name.eq_ignore_ascii_case("script") {
                    skip_until = Some("</script");
                } else if name.eq_ignore_ascii_case("style") {
                    skip_until = Some("</style");
                }
            }
            push_char(&mut out, ' ', &mut last_space);
            rest = after.get(end.saturating_add(1)..).unwrap_or("");
            continue;
        }
        if let Some(after) = rest.strip_prefix('&') {
            let end = after.find(';').filter(|&p| p <= 10);
            if let Some(end) = end {
                let entity = after.get(..end).unwrap_or("");
                if let Some(c) = decode_entity(entity) {
                    push_char(&mut out, c, &mut last_space);
                    rest = after.get(end.saturating_add(1)..).unwrap_or("");
                    continue;
                }
            }
            push_char(&mut out, '&', &mut last_space);
            rest = after;
            continue;
        }
        let mut chars = rest.chars();
        if let Some(c) = chars.next() {
            push_char(&mut out, c, &mut last_space);
        }
        rest = chars.as_str();
    }
    let trimmed_len = truncate(out.trim_end(), MAX_INDEXED_TEXT).len();
    out.truncate(trimmed_len);
    out
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            let num = entity.strip_prefix('#')?;
            let code = if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
                u32::from_str_radix(hex, 16).ok()?
            } else {
                num.parse::<u32>().ok()?
            };
            char::from_u32(code).filter(|c| !c.is_control() || c.is_whitespace())
        }
    }
}

/// Finds `needle` (ASCII) in `hay`, ignoring ASCII case. Returns a byte offset.
fn find_ascii_ci(hay: &str, needle: &str) -> Option<usize> {
    let n = needle.as_bytes();
    let h = hay.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    h.windows(n.len()).position(|w| w.eq_ignore_ascii_case(n))
}

/// Lower-cases a string for prefix comparisons (people search).
pub(crate) fn fold(s: &str) -> String {
    truncate(s, 512).to_lowercase()
}

/// The exclusive upper bound for a prefix range scan over BINARY-collated text.
pub(crate) fn prefix_upper(prefix: &str) -> String {
    let mut s = prefix.to_owned();
    s.push(char::MAX);
    s
}

/// A stable-enough signature of indexed text, to skip re-indexing unchanged rows.
pub(crate) fn signature(parts: &[&str]) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    for p in parts {
        p.hash(&mut h);
    }
    i64::from_ne_bytes(h.finish().to_ne_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_strips_markup() {
        let t = html_to_text("<p>Hello&nbsp;<b>world</b> &amp; co</p><script>evil()</script><style>x{}</style>!");
        assert_eq!(t, "Hello world & co !");
        assert_eq!(html_to_text("<unterminated"), "");
        assert_eq!(html_to_text("a &#x41; &#65; &bogus; &"), "a A A &bogus; &");
    }

    #[test]
    fn cursors_round_trip() {
        let c = Cursor::from_key(-5, "a:b");
        assert_eq!(c.key().unwrap(), (-5, "a:b"));
        let t = Cursor::from_text_key("na:me", "id:1");
        assert_eq!(t.text_key().unwrap(), ("na:me", "id:1"));
        assert!(Cursor::from_string("t99:x").text_key().is_err());
        assert!(Cursor::from_string("nope").key().is_err());
    }
}
