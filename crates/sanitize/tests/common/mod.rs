//! Shared helpers: an independent checker for the safety invariants of sanitised output.

/// A tag found in sanitised output.
#[derive(Debug)]
pub struct Tag {
    pub name: String,
    pub closing: bool,
    pub attrs: Vec<(String, String)>,
}

/// Splits sanitised HTML into tags. Text never contains a raw `<` (ammonia escapes it), so
/// every `<` starts a tag; attribute values may contain `>` inside quotes.
pub fn tags(html: &str) -> Vec<Tag> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        rest = &rest[start + 1..];
        // Find the closing '>' outside quotes.
        let mut quote = None;
        let mut end = rest.len();
        for (i, c) in rest.char_indices() {
            match (quote, c) {
                (Some(q), c) if c == q => quote = None,
                (Some(_), _) => {}
                (None, '"' | '\'') => quote = Some(c),
                (None, '>') => {
                    end = i;
                    break;
                }
                _ => {}
            }
        }
        let inner = &rest[..end];
        rest = &rest[end.min(rest.len())..];
        out.push(parse_tag(inner));
    }
    out
}

fn parse_tag(inner: &str) -> Tag {
    let (closing, inner) = match inner.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, inner),
    };
    let name_end = inner.find(|c: char| c.is_whitespace() || c == '/').unwrap_or(inner.len());
    let name = inner[..name_end].to_ascii_lowercase();
    let mut attrs = Vec::new();
    let mut s = inner[name_end..].trim_start();
    while !s.is_empty() {
        let key_end = s.find(|c: char| c == '=' || c.is_whitespace()).unwrap_or(s.len());
        let key = s[..key_end].to_ascii_lowercase();
        s = s[key_end..].trim_start();
        let mut value = String::new();
        if let Some(after) = s.strip_prefix('=') {
            let after = after.trim_start();
            if let Some(q) = after.chars().next().filter(|c| *c == '"' || *c == '\'') {
                let body = &after[1..];
                let close = body.find(q).unwrap_or(body.len());
                value = body[..close].to_owned();
                s = body.get(close + 1..).unwrap_or("").trim_start();
            } else {
                let end = after.find(char::is_whitespace).unwrap_or(after.len());
                value = after[..end].to_owned();
                s = after[end..].trim_start();
            }
        }
        if !key.is_empty() && key != "/" {
            attrs.push((key, decode_entities(&value)));
        }
    }
    Tag { name, closing, attrs }
}

fn decode_entities(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&nbsp;", "\u{a0}")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

const SAFE_URL_PREFIXES: &[&str] = &[
    "quorum-link:",
    "quorum-cid:",
    "quorum-remote:",
    "quorum-hosted:",
    "data:image/png;",
    "data:image/gif;",
    "data:image/jpeg;",
    "data:image/jpg;",
    "data:image/webp;",
    "data:image/bmp;",
];

/// Asserts the safety invariants of sanitised output; returns a description of the first
/// violation.
pub fn check_safe(html: &str, allowed_tags: &[&str]) -> Result<(), String> {
    for tag in tags(html) {
        if !allowed_tags.contains(&tag.name.as_str()) {
            return Err(format!("disallowed tag <{}> in {html}", tag.name));
        }
        for (key, value) in &tag.attrs {
            let lower = value.to_ascii_lowercase();
            if key.starts_with("on") {
                return Err(format!("event handler {key} in {html}"));
            }
            if matches!(
                key.as_str(),
                "srcdoc" | "formaction" | "action" | "srcset" | "background" | "xlink:href" | "id" | "class" | "ping"
            ) && !(tag.name == "at" && key == "id")
            {
                return Err(format!("dangerous attribute {key} in {html}"));
            }
            if (key == "href" || key == "src") && !SAFE_URL_PREFIXES.iter().any(|p| lower.starts_with(p)) {
                return Err(format!("unsafe URL {key}={value} in {html}"));
            }
            if key == "style" {
                if lower.contains("expression") || lower.contains("javascript") || lower.contains('\\') {
                    return Err(format!("unsafe style {value} in {html}"));
                }
                let mut rest = lower.as_str();
                while let Some(pos) = rest.find("url(") {
                    let arg = rest[pos + 4..].trim_start_matches(['"', '\'', ' ']);
                    if !SAFE_URL_PREFIXES.iter().any(|p| arg.starts_with(p)) {
                        return Err(format!("unsafe style url in {value} in {html}"));
                    }
                    rest = &rest[pos + 4..];
                }
            }
        }
    }
    Ok(())
}

pub const MAIL_TAGS: &[&str] = &[
    "a",
    "abbr",
    "acronym",
    "address",
    "article",
    "aside",
    "b",
    "bdi",
    "bdo",
    "big",
    "blockquote",
    "br",
    "caption",
    "center",
    "cite",
    "code",
    "col",
    "colgroup",
    "dd",
    "del",
    "details",
    "dfn",
    "div",
    "dl",
    "dt",
    "em",
    "figcaption",
    "figure",
    "font",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "i",
    "img",
    "ins",
    "kbd",
    "li",
    "main",
    "mark",
    "nav",
    "ol",
    "p",
    "pre",
    "q",
    "rp",
    "rt",
    "ruby",
    "s",
    "samp",
    "section",
    "small",
    "span",
    "strike",
    "strong",
    "sub",
    "summary",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "time",
    "tr",
    "tt",
    "u",
    "ul",
    "var",
    "wbr",
];

pub const CHAT_TAGS: &[&str] = &[
    "p",
    "br",
    "b",
    "strong",
    "i",
    "em",
    "u",
    "s",
    "strike",
    "del",
    "a",
    "ul",
    "ol",
    "li",
    "code",
    "pre",
    "blockquote",
    "img",
    "at",
    "div",
    "span",
];

/// A tiny deterministic xorshift generator (no external dependency).
#[derive(Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n.max(1) as u64) as usize
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}
