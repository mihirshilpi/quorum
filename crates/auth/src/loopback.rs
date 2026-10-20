//! The loopback redirect listener (RFC 8252 §7.3) that receives the authorization code.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use url::Url;

use crate::account::constant_time_eq;
use crate::error::{Error, ErrorKind, Result};
use crate::secret::AuthorizationCode;

/// Longest request head we read; anything longer is rejected.
pub(crate) const MAX_REQUEST_BYTES: usize = 8 * 1024;
/// How long one connection may take to send its request line.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Connections handled at once (browsers open speculative ones).
const MAX_CONNECTIONS: usize = 16;
const MAX_QUERY_PARAMS: usize = 32;

const PAGE_OK: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>Quorum</title></head>\
<body style=\"font-family:system-ui,sans-serif;margin:3em\"><h1>Signed in</h1>\
<p>You can close this window and return to Quorum.</p></body></html>";
const PAGE_FAILED: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>Quorum</title></head>\
<body style=\"font-family:system-ui,sans-serif;margin:3em\"><h1>Sign-in did not complete</h1>\
<p>You can close this window and return to Quorum to try again.</p></body></html>";
const PAGE_BAD: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>Quorum</title></head>\
<body style=\"font-family:system-ui,sans-serif;margin:3em\"><p>Bad request.</p></body></html>";

/// What one HTTP request on the loopback port amounts to.
#[derive(Debug)]
pub(crate) enum Callback {
    /// A valid redirect with our `state` and a code.
    Code(AuthorizationCode),
    /// A redirect that ends the sign-in with an error (state mismatch, `error=`).
    Failed(Error),
    /// Not the redirect (favicon, preconnect, junk); keep waiting.
    Ignore { status: u16 },
}

/// A bound loopback listener on `127.0.0.1:<ephemeral>`.
#[derive(Debug)]
pub(crate) struct LoopbackListener {
    listener: TcpListener,
    port: u16,
}

impl LoopbackListener {
    /// Binds `127.0.0.1:0`.
    pub async fn bind() -> Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?;
        let port = listener.local_addr()?.port();
        Ok(Self { listener, port })
    }

    /// `http://localhost:<port>`, the `redirect_uri` sent to Entra.
    pub fn redirect_uri(&self) -> String {
        format!("http://localhost:{}", self.port)
    }

    /// Waits for the redirect carrying `expected_state`, answering every request with a small
    /// page. Ends with the code, a sign-in error, or [`ErrorKind::Timeout`].
    pub async fn wait(self, expected_state: &str, timeout: Duration) -> Result<AuthorizationCode> {
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        let (tx, mut rx) = mpsc::channel::<Result<AuthorizationCode>>(MAX_CONNECTIONS);
        let mut tasks = JoinSet::new();
        loop {
            tokio::select! {
                () = &mut deadline => {
                    tracing::warn!(name: "auth.loopback.timeout", "no sign-in redirect before the timeout");
                    return Err(Error::new(ErrorKind::Timeout, "no sign-in redirect received in time"));
                }
                Some(outcome) = rx.recv() => return outcome,
                accepted = self.listener.accept() => {
                    let Ok((stream, peer)) = accepted else { continue };
                    if !peer.ip().is_loopback() || tasks.len() >= MAX_CONNECTIONS {
                        continue; // dropped
                    }
                    let state = expected_state.to_owned();
                    let tx = tx.clone();
                    tasks.spawn(async move {
                        if let Some(outcome) = handle_connection(stream, &state).await {
                            let _ = tx.send(outcome).await;
                        }
                    });
                }
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            }
        }
    }
}

async fn handle_connection(mut stream: TcpStream, expected_state: &str) -> Option<Result<AuthorizationCode>> {
    let line = match tokio::time::timeout(READ_TIMEOUT, read_request_line(&mut stream)).await {
        Ok(Ok(Some(line))) => line,
        Ok(Ok(None)) => {
            let _ = respond(&mut stream, 431, PAGE_BAD).await;
            return None;
        }
        Ok(Err(_)) | Err(_) => return None,
    };
    match parse_callback(&line, expected_state) {
        Callback::Code(code) => {
            let _ = respond(&mut stream, 200, PAGE_OK).await;
            tracing::info!(name: "auth.loopback.code_received", "authorization code received");
            Some(Ok(code))
        }
        Callback::Failed(e) => {
            let _ = respond(&mut stream, 400, PAGE_FAILED).await;
            tracing::warn!(name: "auth.loopback.failed", kind = ?e.kind(), "sign-in redirect carried an error");
            Some(Err(e))
        }
        Callback::Ignore { status } => {
            let _ = respond(&mut stream, status, PAGE_BAD).await;
            None
        }
    }
}

/// Reads until the end of the request line. `Ok(None)` when it exceeds [`MAX_REQUEST_BYTES`].
async fn read_request_line(stream: &mut TcpStream) -> std::io::Result<Option<String>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
        if let Some(end) = buf.iter().position(|b| *b == b'\n') {
            let line = buf.get(..end).unwrap_or_default();
            return Ok(Some(String::from_utf8_lossy(line).trim_end_matches('\r').to_owned()));
        }
        if buf.len() > MAX_REQUEST_BYTES {
            return Ok(None);
        }
    }
}

async fn respond(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        431 => "Request Header Fields Too Large",
        _ => "Bad Request",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await?;
    stream.shutdown().await
}

/// Interprets one HTTP request line. Pure, so hostile inputs are easy to test.
pub(crate) fn parse_callback(line: &str, expected_state: &str) -> Callback {
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) = (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Callback::Ignore { status: 400 };
    };
    if !version.starts_with("HTTP/1.") {
        return Callback::Ignore { status: 400 };
    }
    if method != "GET" {
        return Callback::Ignore { status: 405 };
    }
    if !target.starts_with('/') {
        return Callback::Ignore { status: 400 };
    }
    let Ok(url) = Url::parse("http://localhost/").and_then(|base| base.join(target)) else {
        return Callback::Ignore { status: 400 };
    };
    if url.host_str() != Some("localhost") {
        return Callback::Ignore { status: 400 };
    }
    if url.path() != "/" {
        return Callback::Ignore { status: 404 };
    }

    let (mut code, mut state, mut error, mut description) = (None, None, None, None);
    for (count, (k, v)) in url.query_pairs().enumerate() {
        if count >= MAX_QUERY_PARAMS {
            return Callback::Ignore { status: 400 };
        }
        let slot = match k.as_ref() {
            "code" => &mut code,
            "state" => &mut state,
            "error" => &mut error,
            "error_description" => &mut description,
            _ => continue,
        };
        if slot.is_some() {
            // Duplicate parameters are ambiguous; refuse them.
            return Callback::Ignore { status: 400 };
        }
        *slot = Some(v.into_owned());
    }

    let Some(state) = state else {
        return Callback::Ignore { status: 400 };
    };
    if !constant_time_eq(state.as_bytes(), expected_state.as_bytes()) {
        return Callback::Failed(Error::new(ErrorKind::StateMismatch, "redirect state does not match the request"));
    }
    if let Some(err) = error {
        let code_str: String = err
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .take(64)
            .collect::<String>()
            .to_ascii_lowercase();
        let kind = match code_str.as_str() {
            "access_denied" => ErrorKind::Cancelled,
            "interaction_required" | "login_required" | "consent_required" => ErrorKind::InteractionRequired,
            "invalid_client" | "unauthorized_client" | "invalid_scope" | "invalid_request" => ErrorKind::Config,
            "temporarily_unavailable" | "server_error" => ErrorKind::Network,
            _ => ErrorKind::InvalidResponse,
        };
        // Only the AADSTS number is kept: descriptions can name the user.
        let aadsts = description.as_deref().and_then(crate::error::parse_aadsts);
        return Callback::Failed(
            Error::new(kind, "sign-in redirect carried an error").with_oauth_error(code_str).with_aadsts(aadsts),
        );
    }
    match code {
        Some(c) if !c.is_empty() && c.len() <= MAX_REQUEST_BYTES => Callback::Code(AuthorizationCode::new(c)),
        _ => Callback::Ignore { status: 400 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_redirect() {
        match parse_callback("GET /?code=abc%2Fdef&state=s1&session_state=x HTTP/1.1", "s1") {
            Callback::Code(c) => assert_eq!(c.secret(), "abc/def"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn state_mismatch_fails_the_sign_in() {
        match parse_callback("GET /?code=abc&state=evil HTTP/1.1", "s1") {
            Callback::Failed(e) => assert_eq!(e.kind(), ErrorKind::StateMismatch),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn error_redirects_are_typed() {
        match parse_callback("GET /?error=access_denied&error_description=user%40x&state=s1 HTTP/1.1", "s1") {
            Callback::Failed(e) => {
                assert_eq!(e.kind(), ErrorKind::Cancelled);
                assert_eq!(e.oauth_error(), Some("access_denied"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn hostile_request_lines_never_panic() {
        let long = format!("GET /?{} HTTP/1.1", "a=b&".repeat(5000));
        let lines = [
            "",
            " ",
            "GET",
            "GET /",
            "GET / HTTP/1.1 extra",
            "POST /?code=a&state=s1 HTTP/1.1",
            "GET /favicon.ico HTTP/1.1",
            "GET http://evil.example/?code=a&state=s1 HTTP/1.1",
            "GET //evil.example/?code=a&state=s1 HTTP/1.1",
            "GET /?code=a HTTP/1.1",
            "GET /?state=s1 HTTP/1.1",
            "GET /?code=&state=s1 HTTP/1.1",
            "GET /?code=a&code=b&state=s1 HTTP/1.1",
            "GET /?code=a&state=s1&state=s1 HTTP/1.1",
            "GET /?%ff%fe=%00&state=%ZZ HTTP/1.1",
            "GET /?code=a&state=s1 SPDY/3",
            "GET /\u{0}?\u{1F600} HTTP/1.1",
            &long,
        ];
        for l in lines {
            assert!(!matches!(parse_callback(l, "s1"), Callback::Code(_)), "{l:?}");
        }
    }
}
