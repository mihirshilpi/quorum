//! Conditional Access claims challenges (`WWW-Authenticate: Bearer ... claims="..."`).

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};

/// Longest `WWW-Authenticate` header value we parse.
const MAX_HEADER_LEN: usize = 16 * 1024;

/// A claims challenge: the JSON `claims` request parameter to send on the next sign-in.
///
/// Obtain one from a Graph `401` with [`ClaimsChallenge::from_www_authenticate`], or from
/// [`crate::Error::claims`] when a refresh fails, and pass it in
/// [`crate::SignInOptions::claims`].
#[derive(Clone, PartialEq, Eq)]
pub struct ClaimsChallenge(String);

impl ClaimsChallenge {
    /// Parses the `claims` auth-param of a `WWW-Authenticate` header value.
    ///
    /// The value is base64 of a JSON object (Entra sends standard base64; url-safe and
    /// unpadded forms are accepted too, as is raw JSON). Returns `None` when there is no
    /// `claims` parameter or it is not a JSON object. Never panics on hostile input.
    #[must_use]
    pub fn from_www_authenticate(header: &str) -> Option<Self> {
        if header.len() > MAX_HEADER_LEN {
            return None;
        }
        let raw = auth_params(header).into_iter().find(|(k, _)| k.eq_ignore_ascii_case("claims")).map(|(_, v)| v)?;
        Self::from_encoded(&raw)
    }

    /// Accepts a claims value that is either base64 JSON or raw JSON.
    pub(crate) fn from_encoded(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw.is_empty() || raw.len() > MAX_HEADER_LEN {
            return None;
        }
        if let Some(c) = Self::from_json(raw) {
            return Some(c);
        }
        for engine in [&STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD] {
            if let Ok(bytes) = engine.decode(raw)
                && let Ok(text) = String::from_utf8(bytes)
                && let Some(c) = Self::from_json(&text)
            {
                return Some(c);
            }
        }
        None
    }

    fn from_json(text: &str) -> Option<Self> {
        match serde_json::from_str::<serde_json::Value>(text) {
            Ok(v @ serde_json::Value::Object(_)) => Some(Self(v.to_string())),
            _ => None,
        }
    }

    /// The decoded claims JSON, as sent in the `claims` query parameter.
    #[must_use]
    pub fn as_json(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ClaimsChallenge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Claims JSON holds policy details (e.g. timestamps), not secrets, but keep logs lean.
        write!(f, "ClaimsChallenge({} bytes)", self.0.len())
    }
}

/// Splits `Bearer k1="v1", k2=v2, ...` into key/value pairs, honouring quoted strings with
/// backslash escapes. Malformed input yields whatever pairs could be read.
fn auth_params(header: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut chars = header.chars().peekable();

    // Skip the auth scheme (first token) if it is followed by whitespace and not '='.
    let mut scheme = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() || c == '=' || c == ',' {
            break;
        }
        scheme.push(c);
        chars.next();
    }
    let mut pending_key = if chars.peek() == Some(&'=') { Some(scheme) } else { None };

    loop {
        let key = match pending_key.take() {
            Some(k) => k,
            None => {
                while chars.peek().is_some_and(|c| c.is_whitespace() || *c == ',') {
                    chars.next();
                }
                let mut k = String::new();
                while let Some(&c) = chars.peek() {
                    if c == '=' || c == ',' || c.is_whitespace() {
                        break;
                    }
                    k.push(c);
                    chars.next();
                }
                if k.is_empty() && chars.peek().is_none() {
                    break;
                }
                k
            }
        };
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek() != Some(&'=') {
            // A bare token (e.g. a second scheme); move on.
            if chars.peek().is_none() {
                break;
            }
            if key.is_empty() {
                chars.next();
            }
            continue;
        }
        chars.next(); // '='
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut value = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        if let Some(n) = chars.next() {
                            value.push(n);
                        }
                    }
                    '"' => break,
                    _ => value.push(c),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ',' || c.is_whitespace() {
                    break;
                }
                value.push(c);
                chars.next();
            }
        }
        if !key.is_empty() {
            out.push((key, value));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAIMS_JSON: &str = r#"{"access_token":{"nbf":{"essential":true,"value":"1700000000"}}}"#;

    #[test]
    fn parses_entra_style_header() {
        let b64 = STANDARD.encode(CLAIMS_JSON);
        let header = format!(
            r#"Bearer realm="", authorization_uri="https://login.example.test/common/oauth2/authorize", client_id="00000003-0000-0000-c000-000000000000", error="insufficient_claims", claims="{b64}""#
        );
        let c = ClaimsChallenge::from_www_authenticate(&header).unwrap();
        let v: serde_json::Value = serde_json::from_str(c.as_json()).unwrap();
        assert_eq!(v["access_token"]["nbf"]["value"], "1700000000");
    }

    #[test]
    fn accepts_unquoted_unpadded_and_raw_json() {
        let b64 = URL_SAFE_NO_PAD.encode(CLAIMS_JSON);
        assert!(ClaimsChallenge::from_www_authenticate(&format!("Bearer claims={b64}")).is_some());
        let escaped = CLAIMS_JSON.replace('"', "\\\"");
        assert!(ClaimsChallenge::from_www_authenticate(&format!("Bearer claims=\"{escaped}\"")).is_some());
    }

    #[test]
    fn absent_or_hostile_claims_yield_none() {
        for h in [
            "",
            "Bearer",
            "Bearer realm=\"x\"",
            "Bearer claims=",
            "Bearer claims=\"not base64 !!\"",
            "Bearer claims=\"",
            "Bearer claims=\"\\",
            "claims",
            "=,=,=\"",
            ",,,,",
            "Bearer claims=\"WzEsMiwzXQ==\"", // a JSON array, not an object
            "\u{0}\u{ffff}claims=\u{1F600}",
        ] {
            assert!(ClaimsChallenge::from_www_authenticate(h).is_none(), "{h:?}");
        }
        let huge = format!("Bearer claims=\"{}\"", "A".repeat(MAX_HEADER_LEN));
        assert!(ClaimsChallenge::from_www_authenticate(&huge).is_none());
    }
}
