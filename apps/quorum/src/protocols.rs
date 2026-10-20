//! Custom URI schemes served by the Rust core.
//!
//! | Scheme | Serves |
//! |---|---|
//! | `quorum-mail://localhost/<id>[?remote=1]` | the sanitized mail body as a script-free HTML document |
//! | `quorum-cid://localhost/<token>` | inline attachment bytes from the store's blob cache |
//! | `quorum-remote://localhost/<url>` | a remote image fetched by the core (https, image/*, ≤ 10 MiB, no cookies or referrer) |
//! | `quorum-hosted://localhost/<url>` | Teams hosted content fetched from Graph with the user's token |
//!
//! Every handler returns a [`Resp`]; nothing here panics on hostile URLs or bodies.

use std::time::Duration;

use futures::StreamExt as _;
use quorum_engine::Session;
use serde_json::{Value, json};

use crate::links::{scheme_base, scheme_value, webview_urls};

/// Largest proxied remote image, in bytes.
pub const MAX_REMOTE_IMAGE: usize = 10 * 1024 * 1024;
/// Largest served mail document, in bytes.
const MAX_MAIL_DOC: usize = 16 * 1024 * 1024;

/// The reading styles of a mail body document (our own CSS).
const READING_CSS: &str = "\
:root { color-scheme: light dark; }
html { -webkit-text-size-adjust: 100%; }
body { margin: 0; padding: 16px 20px 24px; font: 14px/1.5 -apple-system, BlinkMacSystemFont, system-ui, sans-serif;
  color: #1f1f1f; background: #ffffff; overflow-wrap: anywhere; }
@media (prefers-color-scheme: dark) { body { color: #e8e8e8; background: #1e1e1e; } a { color: #8ab4ff; } }
img { max-width: 100%; height: auto; }
table { max-width: 100%; }
blockquote { margin: 0 0 0 4px; padding-left: 12px; border-left: 3px solid rgba(128,128,128,.4); }
pre, code { white-space: pre-wrap; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 13px; }
a { color: #0a5fd6; }
";

/// A scheme handler's response, independent of Tauri (tested directly).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resp {
    /// HTTP status.
    pub status: u16,
    /// `Content-Type`.
    pub content_type: String,
    /// Extra headers.
    pub headers: Vec<(&'static str, String)>,
    /// Body.
    pub body: Vec<u8>,
}

impl Resp {
    fn new(status: u16, content_type: &str, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type: content_type.to_owned(),
            headers: vec![
                ("X-Content-Type-Options", "nosniff".to_owned()),
                ("Referrer-Policy", "no-referrer".to_owned()),
                ("Cache-Control", "no-store".to_owned()),
            ],
            body,
        }
    }

    /// A plain-text error response (no personal data in `message`).
    pub fn error(status: u16, message: &str) -> Self {
        Self::new(status, "text/plain; charset=utf-8", message.as_bytes().to_vec())
    }
}

/// Dispatches one request to the handler for `scheme`.
pub async fn handle(
    session: &Session,
    scheme: &str,
    path: &str,
    query: Option<&str>,
    remote: &reqwest::Client,
) -> Resp {
    let Some(value) = scheme_value(path) else { return Resp::error(400, "bad request") };
    match scheme {
        "quorum-mail" => mail(session, &value, query.is_some_and(|q| q.split('&').any(|p| p == "remote=1"))).await,
        "quorum-cid" => cid(session, &value).await,
        "quorum-remote" => remote_image(remote, &value).await,
        "quorum-hosted" => hosted(session, &value).await,
        _ => Resp::error(404, "unknown scheme"),
    }
}

/// The body HTML in a `mail.message` result (several shapes accepted).
pub fn body_html(v: &Value) -> Option<&str> {
    v.get("body_html")
        .or_else(|| v.get("html"))
        .or_else(|| v.get("body").and_then(|b| b.get("html")))
        .or_else(|| v.get("body").and_then(|b| b.get("content")))
        .and_then(Value::as_str)
}

/// The Content-Security-Policy of a mail document.
pub fn mail_csp(allow_remote: bool) -> String {
    let mut img = vec!["data:".to_owned(), "quorum-cid:".to_owned(), scheme_base("quorum-cid")];
    if allow_remote {
        img.push("quorum-remote:".to_owned());
        img.push(scheme_base("quorum-remote"));
    }
    format!(
        "default-src 'none'; style-src 'unsafe-inline'; img-src {}; font-src data:; base-uri 'none'; form-action 'none'",
        img.join(" ")
    )
}

/// Wraps a sanitized body fragment in our reading document.
pub fn mail_document(fragment: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"color-scheme\" content=\"light dark\">\
         <meta name=\"referrer\" content=\"no-referrer\"><style>{READING_CSS}</style></head><body>{}</body></html>",
        webview_urls(fragment)
    )
}

async fn mail(session: &Session, id: &str, remote_requested: bool) -> Resp {
    let default_remote = quorum_engine::settings::get(session, "mail.remote_images")
        .await
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let allow_remote = remote_requested || default_remote;
    let reply = session.query("mail.message", json!({ "id": id, "allow_remote_images": allow_remote })).await;
    let value = match reply {
        Ok(v) => v,
        Err(e) if e.kind() == quorum_engine::ErrorKind::NotFound => return Resp::error(404, "message not found"),
        Err(e) => return Resp::error(503, e.message()),
    };
    let Some(fragment) = body_html(&value) else { return Resp::error(404, "message has no body yet") };
    let doc = mail_document(fragment);
    if doc.len() > MAX_MAIL_DOC {
        return Resp::error(413, "message too large to show");
    }
    let mut r = Resp::new(200, "text/html; charset=utf-8", doc.into_bytes());
    r.headers.push(("Content-Security-Policy", mail_csp(allow_remote)));
    r
}

/// Whether a declared content type is an image we serve (no SVG: it can carry scripts and
/// external references when opened directly).
pub fn image_type(content_type: Option<&str>) -> Option<String> {
    let ct = content_type?.split(';').next()?.trim().to_ascii_lowercase();
    (ct.starts_with("image/") && ct != "image/svg+xml" && ct.len() <= 64 && !ct.contains(char::is_whitespace))
        .then_some(ct)
}

/// Guesses an image type from magic bytes (attachments often lack a reliable type).
pub fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    let starts = |p: &[u8]| bytes.starts_with(p);
    if starts(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if starts(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if starts(b"GIF87a") || starts(b"GIF89a") {
        Some("image/gif")
    } else if starts(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else if starts(b"BM") {
        Some("image/bmp")
    } else {
        None
    }
}

async fn cid(session: &Session, token: &str) -> Resp {
    match quorum_engine::settings::blob_bytes(session, token).await {
        Ok((bytes, declared)) => {
            let ct = sniff_image(&bytes)
                .map(str::to_owned)
                .or_else(|| image_type(declared.as_deref()))
                .unwrap_or_else(|| "application/octet-stream".to_owned());
            Resp::new(200, &ct, bytes)
        }
        Err(e) if e.kind() == quorum_engine::ErrorKind::NotFound => Resp::error(404, "not cached yet"),
        Err(_) => Resp::error(400, "bad request"),
    }
}

/// Validates a remote image URL: https, a host name or public IP, no credentials.
pub fn check_remote_url(url: &str) -> Option<url::Url> {
    let u = url::Url::parse(url).ok()?;
    if u.scheme() != "https" || !u.username().is_empty() || u.password().is_some() {
        return None;
    }
    match u.host()? {
        url::Host::Domain(d) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            (d != "localhost" && !d.ends_with(".localhost") && !d.ends_with(".local") && d.contains('.'))
                .then_some(())?;
        }
        url::Host::Ipv4(ip) => {
            (!(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
                || ip.is_documentation()
                || ip.octets().first() == Some(&100) && ip.octets().get(1).is_some_and(|b| (64..128).contains(b))))
            .then_some(())?;
        }
        url::Host::Ipv6(ip) => {
            let seg0 = ip.segments().first().copied().unwrap_or(0);
            (!(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || (seg0 & 0xfe00) == 0xfc00
                || (seg0 & 0xffc0) == 0xfe80
                || ip.to_ipv4_mapped().is_some()))
            .then_some(())?;
        }
    }
    Some(u)
}

/// The HTTP client for remote images: no cookies, no referrer, https-only redirects.
///
/// # Errors
/// The TLS backend could not be initialised.
pub fn remote_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .referer(false)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("too many redirects")
            } else if check_remote_url(attempt.url().as_str()).is_none() {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .user_agent("Quorum image proxy")
        .build()
}

async fn remote_image(client: &reqwest::Client, url: &str) -> Resp {
    let Some(u) = check_remote_url(url) else { return Resp::error(403, "only https images on public hosts") };
    let resp = match client.get(u).header("Accept", "image/*").send().await {
        Ok(r) => r,
        Err(_) => return Resp::error(502, "image could not be fetched"),
    };
    if !resp.status().is_success() {
        return Resp::error(502, "image could not be fetched");
    }
    let declared = resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let Some(ct) = image_type(declared.as_deref()) else { return Resp::error(415, "not an image") };
    if resp.content_length().is_some_and(|n| n > MAX_REMOTE_IMAGE as u64) {
        return Resp::error(413, "image too large");
    }
    let mut body = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { return Resp::error(502, "image could not be fetched") };
        if body.len().saturating_add(chunk.len()) > MAX_REMOTE_IMAGE {
            return Resp::error(413, "image too large");
        }
        body.extend_from_slice(&chunk);
    }
    Resp::new(200, &ct, body)
}

/// Whether `url` is a Graph URL we may fetch hosted content from.
pub fn check_hosted_url(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| {
        u.scheme() == "https"
            && u.host_str() == Some("graph.microsoft.com")
            && u.port().is_none()
            && u.username().is_empty()
            && u.password().is_none()
            && u.path().contains("/hostedContents/")
    })
}

async fn hosted(session: &Session, url: &str) -> Resp {
    if !check_hosted_url(url) {
        return Resp::error(403, "only Graph hosted content");
    }
    let req = quorum_graph::Request::link(url).header("Accept", "image/*");
    match session.graph().send_raw(req).await {
        Ok(resp) => {
            let declared =
                resp.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-type")).map(|(_, v)| v.as_str());
            let ct = sniff_image(&resp.body).map(str::to_owned).or_else(|| image_type(declared));
            match ct {
                Some(ct) if resp.body.len() <= MAX_REMOTE_IMAGE => Resp::new(200, &ct, resp.body),
                Some(_) => Resp::error(413, "image too large"),
                None => Resp::error(415, "not an image"),
            }
        }
        Err(_) => Resp::error(502, "content could not be fetched"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csp_has_no_script_and_remote_only_when_allowed() {
        let off = mail_csp(false);
        assert!(off.starts_with("default-src 'none'"));
        assert!(!off.contains("script"));
        assert!(!off.contains("quorum-remote"));
        assert!(mail_csp(true).contains("quorum-remote:"));
    }

    #[test]
    fn mail_document_wraps_and_rewrites() {
        let doc = mail_document(r#"<p>Hi</p><img src="quorum-cid:m%2Fa">"#);
        assert!(doc.starts_with("<!doctype html>"));
        assert!(doc.contains(&scheme_base("quorum-cid")));
        assert!(!doc.contains("<script"));
        assert_eq!(body_html(&json!({ "body": { "content": "x" } })), Some("x"));
        assert_eq!(body_html(&json!({ "body_html": "y", "html": "z" })), Some("y"));
        assert_eq!(body_html(&json!({ "body": 5 })), None);
    }

    #[test]
    fn remote_urls_are_restricted() {
        for ok in ["https://example.com/a.png", "https://cdn.example.org:8443/x?y=1", "https://93.184.216.34/i.gif"] {
            assert!(check_remote_url(ok).is_some(), "{ok}");
        }
        for bad in [
            "http://example.com/a.png",
            "https://localhost/a.png",
            "https://127.0.0.1/a.png",
            "https://10.0.0.1/a.png",
            "https://192.168.1.1/",
            "https://169.254.169.254/latest",
            "https://100.64.0.1/",
            "https://[::1]/",
            "https://[fd00::1]/",
            "https://[::ffff:127.0.0.1]/",
            "https://user:pw@example.com/",
            "https://intranet/",
            "https://printer.local/",
            "file:///etc/passwd",
            "",
            "not a url",
        ] {
            assert!(check_remote_url(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn hosted_urls_are_graph_only() {
        assert!(check_hosted_url("https://graph.microsoft.com/v1.0/chats/19%3Ax/messages/1/hostedContents/abc/$value"));
        assert!(!check_hosted_url("https://graph.microsoft.com.evil.example/v1.0/hostedContents/x"));
        assert!(!check_hosted_url("http://graph.microsoft.com/v1.0/chats/x/messages/1/hostedContents/a/$value"));
        assert!(!check_hosted_url("https://graph.microsoft.com/v1.0/me"));
        assert!(!check_hosted_url("https://x@graph.microsoft.com/v1.0/hostedContents/x"));
    }

    #[test]
    fn image_types_exclude_svg_and_junk() {
        assert_eq!(image_type(Some("image/PNG; charset=x")), Some("image/png".into()));
        assert_eq!(image_type(Some("image/svg+xml")), None);
        assert_eq!(image_type(Some("text/html")), None);
        assert_eq!(image_type(None), None);
        assert_eq!(sniff_image(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(sniff_image(b"RIFF\0\0\0\0WEBPVP8"), Some("image/webp"));
        assert_eq!(sniff_image(b"<svg"), None);
        assert_eq!(sniff_image(b""), None);
    }

    #[tokio::test]
    async fn mock_mail_body_is_served_as_a_locked_down_document() {
        let Ok(s) = Session::new(quorum_engine::SessionOptions { mock_graph: true }) else { return };
        s.start().await;
        let client = reqwest::Client::new();
        let mut last = Resp::error(0, "");
        for _ in 0..100 {
            let id = s
                .query("mail.list", json!({ "folder": "inbox", "limit": 1 }))
                .await
                .ok()
                .and_then(|v| v.get(0).and_then(|r| r.get("id")).and_then(Value::as_str).map(str::to_owned));
            if let Some(id) = id {
                last = handle(&s, "quorum-mail", &format!("/{id}"), None, &client).await;
                if last.status == 200 {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(last.status, 200, "{}", String::from_utf8_lossy(&last.body));
        assert!(last.content_type.starts_with("text/html"));
        let csp = last.headers.iter().find(|(k, _)| *k == "Content-Security-Policy").map(|(_, v)| v.clone());
        assert!(csp.is_some_and(|c| c.starts_with("default-src 'none'") && !c.contains("quorum-remote")));
        let body = String::from_utf8_lossy(&last.body);
        assert!(body.starts_with("<!doctype html>") && !body.contains("<script"));
    }

    #[tokio::test]
    async fn hostile_scheme_requests_never_panic() {
        let s = Session::new(quorum_engine::SessionOptions { mock_graph: true });
        let Ok(s) = s else { return };
        let client = reqwest::Client::new();
        for scheme in ["quorum-mail", "quorum-cid", "quorum-hosted", "nope"] {
            for path in ["/", "", "/%", "/%zz", "/%00", "/a%2Fb", "/x", &format!("/{}", "a".repeat(20_000))] {
                let r = handle(&s, scheme, path, Some("remote=1&&=="), &client).await;
                assert!(r.status >= 200);
            }
        }
        let r = handle(&s, "quorum-remote", "/http%3A%2F%2F127.0.0.1%2F", None, &client).await;
        assert_eq!(r.status, 403);
    }
}
