//! Link parsing for the shell: deep links (`quorum://open?...`), `mailto:` and the URL forms
//! of our custom schemes. Everything here is pure and treats its input as hostile.

use serde_json::{Value, json};

/// Longest deep link or mailto URL handled.
const MAX_LINK: usize = 16 * 1024;
/// Most recipients taken from one `mailto:`.
const MAX_RECIPIENTS: usize = 100;
/// Longest subject / body taken from a `mailto:`.
const MAX_FIELD: usize = 8 * 1024;

/// What an incoming link asks for: an engine/UI command id with JSON params.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkAction {
    /// Command id (`view.open`, `view.mail`, `mail.compose`, ...).
    pub command: String,
    /// Params.
    pub params: Value,
}

/// Decodes `%XX` (and, when `plus_is_space`, `+`). `None` for bad escapes or non-UTF-8.
pub fn percent_decode(value: &str, plus_is_space: bool) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'%' => {
                let hi = hex(*bytes.get(i.checked_add(1)?)?)?;
                let lo = hex(*bytes.get(i.checked_add(2)?)?)?;
                out.push((hi << 4) | lo);
                i = i.checked_add(3)?;
            }
            b'+' if plus_is_space => {
                out.push(b' ');
                i = i.checked_add(1)?;
            }
            _ => {
                out.push(b);
                i = i.checked_add(1)?;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Splits `a=b&c=d` into decoded pairs (bounded).
fn query_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .take(64)
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            Some((percent_decode(k, true)?.to_ascii_lowercase(), percent_decode(v, true)?))
        })
        .collect()
}

fn clip(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').take(max).collect()
}

/// Plain text to escaped HTML with line breaks (`mail.compose` takes `body_html`).
pub fn text_to_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '\n' => out.push_str("<br>"),
            c => out.push(c),
        }
    }
    out
}

fn addresses(list: &str) -> Vec<String> {
    list.split([',', ';'])
        .map(str::trim)
        .filter(|a| !a.is_empty() && a.len() <= 320 && a.contains('@') && !a.chars().any(char::is_control))
        .take(MAX_RECIPIENTS)
        .map(str::to_owned)
        .collect()
}

/// Parses a `mailto:` URL (RFC 6068) into `mail.compose` params.
pub fn parse_mailto(url: &str) -> Option<LinkAction> {
    if url.len() > MAX_LINK {
        return None;
    }
    let (scheme, rest) = url.split_once(':')?;
    if !scheme.eq_ignore_ascii_case("mailto") {
        return None;
    }
    let (to, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut params = serde_json::Map::new();
    let mut to_list = addresses(&percent_decode(to, false)?);
    let (mut cc, mut bcc) = (Vec::new(), Vec::new());
    for (k, v) in query_pairs(query) {
        match k.as_str() {
            "to" => to_list.extend(addresses(&v)),
            "cc" => cc.extend(addresses(&v)),
            "bcc" => bcc.extend(addresses(&v)),
            "subject" => {
                params.insert("subject".into(), json!(clip(&v, MAX_FIELD)));
            }
            "body" => {
                params.insert("body_html".into(), json!(text_to_html(&clip(&v, MAX_FIELD))));
            }
            _ => {}
        }
    }
    to_list.truncate(MAX_RECIPIENTS);
    params.insert("to".into(), json!(to_list));
    if !cc.is_empty() {
        params.insert("cc".into(), json!(cc));
    }
    if !bcc.is_empty() {
        params.insert("bcc".into(), json!(bcc));
    }
    Some(LinkAction { command: "mail.compose".into(), params: Value::Object(params) })
}

/// Areas a deep link may open.
const AREAS: &[&str] = &["mail", "calendar", "chat", "teams", "files", "people", "search", "settings"];

/// Parses `quorum://open?area=mail&id=...` (or `quorum://mail`): `view.open` with an id,
/// otherwise `view.<area>`.
pub fn parse_deep_link(url: &str) -> Option<LinkAction> {
    if url.len() > MAX_LINK {
        return None;
    }
    let rest = url.strip_prefix("quorum://").or_else(|| url.strip_prefix("quorum:"))?;
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let path = path.trim_matches('/');
    let pairs = query_pairs(query);
    let get = |k: &str| pairs.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
    if path == "setup" {
        // `quorum://setup?tenant=..&client_id=..` from the admin: the UI pre-fills the welcome
        // screen and the person confirms with "Save and sign in"; nothing is saved here.
        let id = |k: &str| {
            get(k).filter(|v| {
                !v.is_empty() && v.len() <= 128 && v.chars().all(|c| c.is_ascii_alphanumeric() || "-._".contains(c))
            })
        };
        return Some(LinkAction {
            command: "setup.prefill".into(),
            params: json!({ "tenant": id("tenant")?, "client_id": id("client_id")? }),
        });
    }
    let area = match path {
        "open" | "" => get("area")?,
        other => other.to_owned(),
    };
    if !AREAS.contains(&area.as_str()) {
        return None;
    }
    match get("id").filter(|id| !id.is_empty() && id.len() <= 1024 && !id.chars().any(char::is_control)) {
        Some(id) => Some(LinkAction { command: "view.open".into(), params: json!({ "area": area, "id": id }) }),
        None => Some(LinkAction { command: format!("view.{area}"), params: json!({}) }),
    }
}

/// Any link the OS hands us (deep link or mailto).
pub fn parse_incoming(url: &str) -> Option<LinkAction> {
    parse_deep_link(url).or_else(|| parse_mailto(url))
}

/// The decoded first path segment of a custom-scheme request (`quorum-cid://localhost/<enc>`
/// or `http://quorum-cid.localhost/<enc>`), i.e. the value sanitize percent-encoded.
pub fn scheme_value(path: &str) -> Option<String> {
    let enc = path.strip_prefix('/').unwrap_or(path);
    if enc.is_empty() || enc.len() > MAX_LINK {
        return None;
    }
    percent_decode(enc, false).filter(|v| !v.chars().any(char::is_control))
}

/// The base URL the webview uses for a custom scheme on this platform.
pub fn scheme_base(scheme: &str) -> String {
    if cfg!(windows) { format!("http://{scheme}.localhost/") } else { format!("{scheme}://localhost/") }
}

/// Rewrites sanitize's opaque `quorum-cid:` / `quorum-remote:` / `quorum-hosted:` URLs to
/// the hierarchical form this platform's webview routes to a scheme handler.
pub fn webview_urls(html: &str) -> String {
    let platform = if cfg!(windows) {
        quorum_sanitize::WebviewPlatform::Windows
    } else {
        quorum_sanitize::WebviewPlatform::MacOsLinux
    };
    quorum_sanitize::webview_urls(html, platform)
}

/// The real URL behind an intercepted navigation, if it should be confirmed: `quorum-link:`
/// targets, and any http(s)/mailto navigation away from the app's own pages.
pub fn intercepted_link(url: &str) -> Option<String> {
    if let Some(enc) = url.strip_prefix("quorum-link:") {
        return percent_decode(enc, false);
    }
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    if lower.starts_with("mailto:") {
        return Some(url.to_owned());
    }
    let external = (lower.starts_with("https://") || lower.starts_with("http://")) && !is_app_origin(url);
    external.then(|| url.to_owned())
}

/// Whether `url` is one of the app's own origins (bundled UI, dev server, custom schemes).
fn is_app_origin(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else { return false };
    match u.host_str() {
        Some(h) => h == "localhost" || h == "127.0.0.1" || h.ends_with(".localhost"),
        None => false,
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn setup_links_prefill_only_valid_ids() {
        let a =
            parse_deep_link("quorum://setup?tenant=11111111-2222-3333-4444-555555555555&client_id=abc-123").unwrap();
        assert_eq!(a.command, "setup.prefill");
        assert_eq!(a.params["client_id"], "abc-123");
        assert!(parse_deep_link("quorum://setup?tenant=x").is_none());
        assert!(parse_deep_link("quorum://setup?tenant=a%20b&client_id=c").is_none());
        assert!(parse_deep_link("quorum://setup?tenant=a&client_id=%3Cscript%3E").is_none());
    }

    use super::*;

    #[test]
    fn mailto_parses_recipients_and_fields() {
        let a = parse_mailto(
            "mailto:a@example.com,b@example.com?cc=c%40example.com&subject=Hi%20there&body=Line+1%0ALine%202&x=y",
        );
        let a = a.unwrap_or_else(|| LinkAction { command: String::new(), params: Value::Null });
        assert_eq!(a.command, "mail.compose");
        assert_eq!(a.params["to"], json!(["a@example.com", "b@example.com"]));
        assert_eq!(a.params["cc"], json!(["c@example.com"]));
        assert_eq!(a.params["subject"], json!("Hi there"));
        assert_eq!(a.params["body_html"], json!("Line 1<br>Line 2"));
        assert_eq!(text_to_html("<b>&\"'"), "&lt;b&gt;&amp;&quot;&#39;");
        assert_eq!(parse_mailto("MAILTO:").map(|a| a.params["to"].clone()), Some(json!([])));
    }

    #[test]
    fn hostile_links_never_panic() {
        for bad in [
            "mailto:%",
            "mailto:%zz",
            "mailto:%E2%82",
            "mailto:?subject=%",
            "quorum://open?area=%",
            "quorum://",
            "quorum://open?area=nope",
            "quorum://open?area=mail&id=%00",
            "http://x",
            "",
            "mailto",
            "quorum-link:%",
            "quorum:open?area=mail\u{0}",
        ] {
            let _ = parse_incoming(bad);
            let _ = intercepted_link(bad);
            let _ = scheme_value(bad);
        }
        let huge = format!("mailto:{}", "a@b.c,".repeat(10_000));
        assert!(parse_mailto(&huge).is_none());
        let many = format!("mailto:x@example.com?cc={}", "a@b.c,".repeat(1000));
        let n = parse_mailto(&many).and_then(|a| a.params["cc"].as_array().map(Vec::len));
        assert_eq!(n, Some(MAX_RECIPIENTS));
    }

    #[test]
    fn deep_links_open_areas_and_items() {
        let a = parse_deep_link("quorum://open?area=mail&id=AAMk%2Fx");
        assert_eq!(
            a.map(|a| (a.command, a.params)),
            Some(("view.open".into(), json!({"area": "mail", "id": "AAMk/x"})))
        );
        assert_eq!(parse_deep_link("quorum://calendar").map(|a| a.command), Some("view.calendar".into()));
        assert_eq!(parse_deep_link("quorum://open?area=evil"), None);
        assert_eq!(parse_deep_link("https://example.com"), None);
    }

    #[test]
    fn scheme_urls_are_rewritten_and_decoded() {
        let html = r#"<img src="quorum-cid:m1%2Fa1"><a href="quorum-link:https%3A%2F%2Fexample.com">x</a><p style="background:url(&quot;quorum-remote:x&quot;)">"#;
        let out = webview_urls(html);
        assert!(out.contains(&format!("\"{}m1%2Fa1\"", scheme_base("quorum-cid"))));
        assert!(out.contains("\"quorum-link:https"));
        assert_eq!(scheme_value("/m1%2Fa1"), Some("m1/a1".into()));
        assert_eq!(scheme_value("/"), None);
        assert_eq!(scheme_value("/%00"), None);
    }

    #[test]
    fn navigations_are_intercepted() {
        assert_eq!(intercepted_link("quorum-link:https%3A%2F%2Fexample.com%2F"), Some("https://example.com/".into()));
        assert_eq!(intercepted_link("https://example.com/"), Some("https://example.com/".into()));
        assert_eq!(intercepted_link("http://localhost:1420/"), None);
        assert_eq!(intercepted_link("tauri://localhost/"), None);
        assert_eq!(intercepted_link("quorum-mail://localhost/abc"), None);
        assert_eq!(intercepted_link("mailto:a@example.com"), Some("mailto:a@example.com".into()));
    }
}
