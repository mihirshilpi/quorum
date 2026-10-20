//! The `quorum-*:` URL schemes and percent-encoding.

use std::fmt;

/// Scheme for rewritten links: `quorum-link:<percent-encoded original URL>`.
pub const LINK_SCHEME: &str = "quorum-link";
/// Scheme for mapped inline images: `quorum-cid:<percent-encoded caller token>`.
pub const CID_SCHEME: &str = "quorum-cid";
/// Scheme for allowed remote images, fetched by the core's proxy:
/// `quorum-remote:<percent-encoded original URL>`.
pub const REMOTE_SCHEME: &str = "quorum-remote";
/// Scheme for Teams hosted content (inline chat images), fetched with the user's token:
/// `quorum-hosted:<percent-encoded Graph URL>`.
pub const HOSTED_SCHEME: &str = "quorum-hosted";

/// A decoded `quorum-*:` URL produced by this crate.
///
/// `Debug` prints only the variant and length: link targets can contain addresses.
#[derive(Clone, PartialEq, Eq)]
pub enum QuorumUrl {
    /// A link the user may open; holds the original URL.
    Link(String),
    /// An inline image; holds the token the caller put in [`crate::MailOptions`].
    Cid(String),
    /// A remote image to fetch through the proxy; holds the original URL.
    Remote(String),
    /// A Teams hosted-content image; holds the Graph URL.
    Hosted(String),
}

impl QuorumUrl {
    /// Decodes a `quorum-*` URL in any of its forms. Returns `None` for other URLs or bad
    /// encoding.
    ///
    /// Accepted forms (`x` is `link`, `cid`, `remote` or `hosted`):
    /// * opaque, as produced by the sanitisers: `quorum-x:<enc>`;
    /// * macOS/Linux webview: `quorum-x://localhost/<enc>`;
    /// * Windows webview: `http://quorum-x.localhost/<enc>` (or `https://`).
    ///
    /// ```
    /// use quorum_sanitize::QuorumUrl;
    /// let want = Some(QuorumUrl::Link("https://example.com/".to_owned()));
    /// assert_eq!(QuorumUrl::parse("quorum-link:https%3A%2F%2Fexample.com%2F"), want);
    /// assert_eq!(QuorumUrl::parse("quorum-link://localhost/https%3A%2F%2Fexample.com%2F"), want);
    /// assert_eq!(QuorumUrl::parse("http://quorum-link.localhost/https%3A%2F%2Fexample.com%2F"), want);
    /// ```
    #[must_use]
    pub fn parse(url: &str) -> Option<Self> {
        let (scheme, encoded) = split_quorum_url(url)?;
        // The encoded value never contains '/', '?' or '#' (they are percent-encoded).
        if encoded.contains(['/', '?', '#']) {
            return None;
        }
        let decoded = percent_decode(encoded)?;
        match scheme {
            LINK_SCHEME => Some(Self::Link(decoded)),
            CID_SCHEME => Some(Self::Cid(decoded)),
            REMOTE_SCHEME => Some(Self::Remote(decoded)),
            HOSTED_SCHEME => Some(Self::Hosted(decoded)),
            _ => None,
        }
    }

    /// Returns the decoded value (URL or token).
    #[must_use]
    pub fn target(&self) -> &str {
        match self {
            Self::Link(s) | Self::Cid(s) | Self::Remote(s) | Self::Hosted(s) => s,
        }
    }
}

impl fmt::Debug for QuorumUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            Self::Link(_) => "Link",
            Self::Cid(_) => "Cid",
            Self::Remote(_) => "Remote",
            Self::Hosted(_) => "Hosted",
        };
        write!(f, "{kind}({} bytes)", self.target().len())
    }
}

/// Splits any form of `quorum-*` URL into (scheme, encoded value).
fn split_quorum_url(url: &str) -> Option<(&str, &str)> {
    for prefix in ["http://", "https://"] {
        if let Some(rest) = url.strip_prefix(prefix) {
            let (host, encoded) = rest.split_once('/')?;
            let scheme = host.strip_suffix(".localhost")?;
            return Some((scheme, encoded));
        }
    }
    let (scheme, rest) = url.split_once(':')?;
    match rest.strip_prefix("//") {
        Some(authority) => {
            let encoded = authority.strip_prefix("localhost/")?;
            Some((scheme, encoded))
        }
        None => Some((scheme, rest)),
    }
}

/// How the webview hosting sanitised HTML spells custom-scheme URLs.
///
/// wry builds an `http::Request` from each URL, and `http::Uri` rejects opaque
/// `scheme:value` URLs, so the webview needs a hierarchical form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebviewPlatform {
    /// WKWebView and WebKitGTK: `quorum-x://localhost/<enc>`.
    MacOsLinux,
    /// WebView2: `http://quorum-x.localhost/<enc>`.
    Windows,
}

/// Rewrites the opaque `quorum-cid:`, `quorum-remote:` and `quorum-hosted:` URLs in sanitised
/// HTML to the form `platform`'s webview can route to a custom-scheme handler.
///
/// Only URLs right after a `"` or `&quot;` (an attribute value or a CSS `url("…")` inside a
/// `style` attribute) are rewritten, which is where the sanitisers emit them; URLs already in
/// webview form are left alone, so the rewrite is idempotent. `quorum-link:` URLs stay opaque:
/// links are intercepted on navigation and never fetched. [`QuorumUrl::parse`] accepts every
/// form.
///
/// ```
/// use quorum_sanitize::{WebviewPlatform, webview_urls};
/// let html = r#"<img src="quorum-cid:att1">"#;
/// assert_eq!(webview_urls(html, WebviewPlatform::MacOsLinux), r#"<img src="quorum-cid://localhost/att1">"#);
/// assert_eq!(webview_urls(html, WebviewPlatform::Windows), r#"<img src="http://quorum-cid.localhost/att1">"#);
/// ```
#[must_use]
pub fn webview_urls(html: &str, platform: WebviewPlatform) -> String {
    const QUOTES: [&str; 2] = ["\"", "&quot;"];
    let mut out = String::with_capacity(html.len() + html.len() / 8);
    let mut rest = html;
    'scan: while let Some(pos) = rest.find("quorum-") {
        let (before, at) = rest.split_at(pos);
        out.push_str(before);
        if QUOTES.iter().any(|q| out.ends_with(q)) {
            for scheme in [CID_SCHEME, REMOTE_SCHEME, HOSTED_SCHEME] {
                if let Some(value) =
                    at.strip_prefix(scheme).and_then(|r| r.strip_prefix(':')).filter(|v| !v.starts_with("//"))
                {
                    match platform {
                        WebviewPlatform::MacOsLinux => {
                            out.push_str(scheme);
                            out.push_str("://localhost/");
                        }
                        WebviewPlatform::Windows => {
                            out.push_str("http://");
                            out.push_str(scheme);
                            out.push_str(".localhost/");
                        }
                    }
                    rest = value;
                    continue 'scan;
                }
            }
        }
        out.push_str("quorum-");
        rest = at.get("quorum-".len()..).unwrap_or("");
    }
    out.push_str(rest);
    out
}

/// Builds `scheme:` + percent-encoded `value`.
pub(crate) fn encode(scheme: &str, value: &str) -> String {
    let mut out = String::with_capacity(scheme.len() + 1 + value.len() * 3 / 2);
    out.push_str(scheme);
    out.push(':');
    percent_encode_into(value, &mut out);
    out
}

/// Percent-encodes everything except RFC 3986 unreserved characters.
fn percent_encode_into(value: &str, out: &mut String) {
    for &b in value.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(hex_digit(b >> 4));
            out.push(hex_digit(b & 0x0f));
        }
    }
}

/// Decodes `%XX` sequences; `None` if a sequence is malformed or the result is not UTF-8.
pub(crate) fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hi = hex_val(*bytes.get(i + 1)?)?;
            let lo = hex_val(*bytes.get(i + 2)?)?;
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Returns the uppercase hex digit for a nibble (`n` < 16).
fn hex_digit(n: u8) -> char {
    char::from_digit(u32::from(n), 16).unwrap_or('0').to_ascii_uppercase()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_round_trips() {
        let original = "https://example.com/a b?x=1&y=ü#frag";
        let encoded = encode(LINK_SCHEME, original);
        assert!(!encoded.contains(' ') && !encoded.contains('&') && !encoded.contains('"'));
        assert_eq!(QuorumUrl::parse(&encoded), Some(QuorumUrl::Link(original.to_owned())));
    }

    #[test]
    fn webview_forms_parse() {
        let want = Some(QuorumUrl::Cid("a b".to_owned()));
        assert_eq!(QuorumUrl::parse("quorum-cid:a%20b"), want);
        assert_eq!(QuorumUrl::parse("quorum-cid://localhost/a%20b"), want);
        assert_eq!(QuorumUrl::parse("http://quorum-cid.localhost/a%20b"), want);
        assert_eq!(QuorumUrl::parse("https://quorum-cid.localhost/a%20b"), want);
        assert_eq!(QuorumUrl::parse("quorum-cid://evil.test/a"), None);
        assert_eq!(QuorumUrl::parse("http://quorum-cid.evil.test/a"), None);
        assert_eq!(QuorumUrl::parse("http://evil.test/a"), None);
        assert_eq!(QuorumUrl::parse("http://quorum-cid.localhost/a/b"), None);
        assert_eq!(QuorumUrl::parse("quorum-cid://localhost/a?x"), None);
    }

    #[test]
    fn bad_encoding_is_rejected() {
        assert_eq!(QuorumUrl::parse("quorum-link:%zz"), None);
        assert_eq!(QuorumUrl::parse("quorum-link:%4"), None);
        assert_eq!(QuorumUrl::parse("quorum-link:%FF"), None);
        assert_eq!(QuorumUrl::parse("https://example.com"), None);
    }
}
