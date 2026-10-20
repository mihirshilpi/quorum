//! Hostile-input JSON handling and Graph error mapping.

use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::error::{Error, ErrorKind, Result};
use crate::http::HttpResponse;

/// Default maximum nesting depth of a JSON response.
pub const DEFAULT_MAX_DEPTH: usize = 64;

/// Longest Graph error `code` kept on an [`Error`].
const MAX_CODE_CHARS: usize = 128;
/// Longest Graph error message kept on an [`Error`].
const MAX_MESSAGE_CHARS: usize = 2048;
/// Longest `claims` challenge kept on an [`Error`].
const MAX_CLAIMS_CHARS: usize = 8192;

/// Error codes Graph uses for an expired or invalid delta token.
const DELTA_EXPIRED_CODES: &[&str] =
    &["syncStateNotFound", "resyncRequired", "syncStateInvalid", "SyncStateNotFound", "ResyncRequired"];

/// Rejects JSON nested deeper than `max_depth` without parsing it (strings are skipped, so
/// brackets inside strings do not count). Malformed JSON passes; the parser reports it.
pub(crate) fn check_depth(bytes: &[u8], max_depth: usize) -> Result<()> {
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escaped = false;
    for &b in bytes {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'[' | b'{' => {
                depth = depth.saturating_add(1);
                if depth > max_depth {
                    return Err(Error::new(ErrorKind::Malformed, "JSON nested too deeply"));
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

/// Parses `bytes` as `T` after the depth check. An empty (or all-whitespace) body parses as `null`.
pub(crate) fn parse<T: DeserializeOwned>(bytes: &[u8], max_depth: usize) -> Result<T> {
    check_depth(bytes, max_depth)?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(serde_json::from_slice(b"null")?);
    }
    Ok(serde_json::from_slice(bytes)?)
}

/// Keeps at most `max` characters of `s`.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    error: Option<ErrorBody>,
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default, rename = "innerError", alias = "innererror")]
    inner: Option<InnerError>,
}

#[derive(Deserialize)]
struct InnerError {
    #[serde(default)]
    code: Option<String>,
    #[serde(default, rename = "request-id")]
    request_id: Option<String>,
}

/// Graph's `{ "error": { "code", "message", "innerError": { "request-id" } } }`, leniently.
pub(crate) struct GraphError {
    pub code: Option<String>,
    pub inner_code: Option<String>,
    pub message: Option<String>,
    pub request_id: Option<String>,
}

pub(crate) fn parse_graph_error(body: &[u8]) -> GraphError {
    let parsed = if body.len() > 1024 * 1024 { None } else { parse::<Envelope>(body, DEFAULT_MAX_DEPTH).ok() };
    let error = parsed.and_then(|e| e.error);
    match error {
        Some(e) => GraphError {
            code: e.code.map(|c| truncate(&c, MAX_CODE_CHARS)),
            inner_code: e.inner.as_ref().and_then(|i| i.code.as_deref()).map(|c| truncate(c, MAX_CODE_CHARS)),
            message: e.message.map(|m| truncate(&m, MAX_MESSAGE_CHARS)),
            request_id: e.inner.and_then(|i| i.request_id).map(|r| truncate(&r, MAX_CODE_CHARS)),
        },
        None => GraphError { code: None, inner_code: None, message: None, request_id: None },
    }
}

/// Returns the value of parameter `key` (case-insensitive) in a `WWW-Authenticate` header such as
/// `Bearer realm="", error="insufficient_claims", claims="eyJ..."`.
pub(crate) fn www_authenticate_param(header: &str, key: &str) -> Option<String> {
    let chars: Vec<char> = header.chars().collect();
    let mut i = 0usize;
    let n = chars.len();
    let at = |i: usize| chars.get(i).copied();
    while i < n {
        // Skip separators.
        while at(i).is_some_and(|c| c == ',' || c.is_whitespace()) {
            i += 1;
        }
        let start = i;
        while at(i).is_some_and(|c| c != '=' && c != ',' && !c.is_whitespace()) {
            i += 1;
        }
        let name: String = chars.get(start..i).map(|s| s.iter().collect()).unwrap_or_default();
        while at(i).is_some_and(char::is_whitespace) {
            i += 1;
        }
        if at(i) != Some('=') {
            // A bare token (the auth scheme); move on.
            if i == start {
                i += 1;
            }
            continue;
        }
        i += 1;
        while at(i).is_some_and(char::is_whitespace) {
            i += 1;
        }
        let mut value = String::new();
        if at(i) == Some('"') {
            i += 1;
            while let Some(c) = at(i) {
                i += 1;
                match c {
                    '\\' => {
                        if let Some(next) = at(i) {
                            value.push(next);
                            i += 1;
                        }
                    }
                    '"' => break,
                    _ => value.push(c),
                }
            }
        } else {
            while let Some(c) = at(i) {
                if c == ',' || c.is_whitespace() {
                    break;
                }
                value.push(c);
                i += 1;
            }
        }
        if name.eq_ignore_ascii_case(key) {
            return Some(value);
        }
    }
    None
}

/// Maps a non-success response to an [`Error`], keeping Graph's code and request id.
pub(crate) fn error_from_response(resp: &HttpResponse, retry_after: Option<Duration>) -> Error {
    let graph = parse_graph_error(&resp.body);
    let code_matches = |codes: &[&str]| {
        graph.code.as_deref().is_some_and(|c| codes.contains(&c))
            || graph.inner_code.as_deref().is_some_and(|c| codes.contains(&c))
    };
    let mut kind = match resp.status {
        400 => ErrorKind::BadRequest,
        401 => ErrorKind::Unauthorized,
        403 => ErrorKind::Forbidden,
        404 => ErrorKind::NotFound,
        409 => ErrorKind::Conflict,
        410 => ErrorKind::DeltaExpired,
        412 => ErrorKind::PreconditionFailed,
        413 => ErrorKind::TooLarge,
        429 => ErrorKind::Throttled,
        503 | 504 if retry_after.is_some() => ErrorKind::Throttled,
        500..=599 => ErrorKind::Server,
        _ => ErrorKind::BadRequest,
    };
    if code_matches(DELTA_EXPIRED_CODES) {
        kind = ErrorKind::DeltaExpired;
    }
    let claims = if resp.status == 401 {
        resp.header("www-authenticate")
            .and_then(|h| www_authenticate_param(h, "claims"))
            .map(|c| truncate(&c, MAX_CLAIMS_CHARS))
    } else {
        None
    };
    let request_id = resp.request_id().map(|r| truncate(r, MAX_CODE_CHARS)).or(graph.request_id);
    Error::new(kind, "request failed")
        .with_status(resp.status)
        .with_code(graph.code.or(graph.inner_code))
        .with_message(graph.message)
        .with_request_id(request_id)
        .with_retry_after(retry_after)
        .with_claims(claims)
}
