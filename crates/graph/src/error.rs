//! The crate's canonical error type (M-ERRORS-CANONICAL-STRUCTS).

use std::backtrace::Backtrace;
use std::fmt;
use std::time::Duration;

/// Convenience alias for results returned by this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// What went wrong, coarsely; callers branch on this (see [`Error::kind`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// HTTP 401: the token is missing, expired or needs extra claims (see [`Error::claims`]).
    Unauthorized,
    /// HTTP 403: the signed-in user or the app lacks permission.
    Forbidden,
    /// HTTP 404 (or 410 outside delta): the resource does not exist, or an upload session expired.
    NotFound,
    /// HTTP 409: the request conflicts with the current state of the resource.
    Conflict,
    /// HTTP 412: an `If-Match` / eTag precondition failed.
    PreconditionFailed,
    /// HTTP 400 and other client errors not covered by a more specific kind.
    BadRequest,
    /// HTTP 429 (or 503/504 with `Retry-After`) after retries were exhausted or not allowed.
    Throttled,
    /// A delta token is no longer valid (410 Gone, `syncStateNotFound`, `resyncRequired`); resync.
    DeltaExpired,
    /// HTTP 5xx after retries were exhausted or not allowed.
    Server,
    /// Connection, TLS, DNS or timeout failure.
    Network,
    /// The response (or a link inside it) was not what Graph documents: bad JSON, too deep,
    /// missing fields, a `nextLink` to a foreign host.
    Malformed,
    /// A response, page count or upload exceeded a configured cap.
    TooLarge,
    /// The request or client was configured incorrectly (bad path, header, base URL, batch).
    Config,
    /// A contract violation inside this crate; logged and reported, never a panic (ADR 0000).
    Bug,
}

impl ErrorKind {
    /// Short stable name for logs and messages.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::PreconditionFailed => "precondition_failed",
            Self::BadRequest => "bad_request",
            Self::Throttled => "throttled",
            Self::DeltaExpired => "delta_expired",
            Self::Server => "server",
            Self::Network => "network",
            Self::Malformed => "malformed",
            Self::TooLarge => "too_large",
            Self::Config => "config",
            Self::Bug => "bug",
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An error from the Graph client.
///
/// Carries the [`ErrorKind`], the HTTP status, Graph's error `code` and the `request-id` for
/// support, plus a backtrace and optional source. Graph's free-text error message may contain
/// tenant data, so it is available through [`Error::graph_message`] but never printed by
/// `Display` or `Debug` (M-PUBLIC-DEBUG).
pub struct Error {
    inner: Box<Inner>,
}

struct Inner {
    kind: ErrorKind,
    detail: String,
    status: Option<u16>,
    code: Option<String>,
    message: Option<String>,
    request_id: Option<String>,
    retry_after: Option<Duration>,
    claims: Option<String>,
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    backtrace: Backtrace,
}

impl Error {
    /// Creates an error of `kind` with a short description that must not contain tenant data.
    ///
    /// Use this from [`Transport`](crate::Transport) and [`TokenSource`](crate::TokenSource)
    /// implementations.
    #[must_use]
    pub fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            inner: Box::new(Inner {
                kind,
                detail: detail.into(),
                status: None,
                code: None,
                message: None,
                request_id: None,
                retry_after: None,
                claims: None,
                source: None,
                backtrace: Backtrace::capture(),
            }),
        }
    }

    /// Attaches the underlying cause.
    #[must_use]
    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.inner.source = Some(Box::new(source));
        self
    }

    pub(crate) fn with_status(mut self, status: u16) -> Self {
        self.inner.status = Some(status);
        self
    }

    pub(crate) fn with_code(mut self, code: Option<String>) -> Self {
        self.inner.code = code;
        self
    }

    pub(crate) fn with_message(mut self, message: Option<String>) -> Self {
        self.inner.message = message;
        self
    }

    pub(crate) fn with_request_id(mut self, request_id: Option<String>) -> Self {
        self.inner.request_id = request_id;
        self
    }

    pub(crate) fn with_retry_after(mut self, retry_after: Option<Duration>) -> Self {
        self.inner.retry_after = retry_after;
        self
    }

    pub(crate) fn with_claims(mut self, claims: Option<String>) -> Self {
        self.inner.claims = claims;
        self
    }

    /// The coarse category of this error.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.inner.kind
    }

    /// The HTTP status, when the error came from a response.
    #[must_use]
    pub fn status(&self) -> Option<u16> {
        self.inner.status
    }

    /// Graph's error `code` (e.g. `ErrorItemNotFound`, `syncStateNotFound`), when present.
    #[must_use]
    pub fn code(&self) -> Option<&str> {
        self.inner.code.as_deref()
    }

    /// Graph's free-text error message. It may contain tenant data: do not log it.
    #[must_use]
    pub fn graph_message(&self) -> Option<&str> {
        self.inner.message.as_deref()
    }

    /// The `request-id` response header, for Microsoft support.
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        self.inner.request_id.as_deref()
    }

    /// How long Graph asked us to wait (`Retry-After`), for [`ErrorKind::Throttled`].
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        self.inner.retry_after
    }

    /// The `claims` parameter of a `WWW-Authenticate` claims challenge on a 401, verbatim
    /// (base64 JSON). Pass it to the auth layer to acquire a token with those claims.
    #[must_use]
    pub fn claims(&self) -> Option<&str> {
        self.inner.claims.as_deref()
    }

    /// The backtrace captured when this error was created (empty unless `RUST_BACKTRACE` is set).
    #[must_use]
    pub fn backtrace(&self) -> &Backtrace {
        &self.inner.backtrace
    }

    /// Whether retrying later might succeed (throttling, server or network trouble).
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self.inner.kind, ErrorKind::Throttled | ErrorKind::Server | ErrorKind::Network)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "graph {}: {}", self.inner.kind, self.inner.detail)?;
        if let Some(status) = self.inner.status {
            write!(f, " (status {status}")?;
            if let Some(code) = &self.inner.code {
                write!(f, ", code {code}")?;
            }
            if let Some(id) = &self.inner.request_id {
                write!(f, ", request-id {id}")?;
            }
            f.write_str(")")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Error")
            .field("kind", &self.inner.kind)
            .field("detail", &self.inner.detail)
            .field("status", &self.inner.status)
            .field("code", &self.inner.code)
            .field("message", &self.inner.message.as_ref().map(|_| "<redacted>"))
            .field("request_id", &self.inner.request_id)
            .field("retry_after", &self.inner.retry_after)
            .field("claims", &self.inner.claims.as_ref().map(|_| "<redacted>"))
            .field("source", &self.inner.source)
            .finish_non_exhaustive()
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.inner.source.as_ref().map(|s| &**s as &(dyn std::error::Error + 'static))
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::new(ErrorKind::Malformed, "invalid JSON").with_source(e)
    }
}

impl From<url::ParseError> for Error {
    fn from(e: url::ParseError) -> Self {
        Self::new(ErrorKind::Config, "invalid URL").with_source(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::new(ErrorKind::Network, "I/O error").with_source(e)
    }
}
