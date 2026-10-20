//! Sync errors.

use std::backtrace::Backtrace;
use std::fmt;

/// What went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// Graph or the network failed; usually transient.
    Remote,
    /// Graph throttled us; retry after the given delay.
    Throttled,
    /// Sign-in is required.
    Auth,
    /// The store failed.
    Store,
    /// A queued write can never succeed (e.g. 400/403/404); the user decides.
    Permanent,
    /// Malformed payload or state.
    Invalid,
    /// A contract violation inside Quorum.
    Bug,
}

/// Sync error with a kind, a safe message, an optional cause and a backtrace.
pub struct Error {
    kind: ErrorKind,
    message: String,
    retry_after: Option<std::time::Duration>,
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    backtrace: Backtrace,
}

impl Error {
    /// Creates an error.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), retry_after: None, source: None, backtrace: Backtrace::capture() }
    }

    /// Attaches a cause.
    #[must_use]
    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// Sets how long to wait before retrying (for [`ErrorKind::Throttled`]).
    #[must_use]
    pub fn with_retry_after(mut self, retry_after: std::time::Duration) -> Self {
        self.retry_after = Some(retry_after);
        self
    }

    /// The kind.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Safe message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// How long Graph asked us to wait, if throttled.
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        self.retry_after
    }

    /// Backtrace captured at creation.
    pub fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Error").field("kind", &self.kind).field("message", &self.message).finish_non_exhaustive()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_deref().map(|e| e as &(dyn std::error::Error + 'static))
    }
}

impl From<quorum_graph::Error> for Error {
    fn from(e: quorum_graph::Error) -> Self {
        use quorum_graph::ErrorKind as G;
        let kind = match e.kind() {
            G::Throttled => ErrorKind::Throttled,
            G::Unauthorized => ErrorKind::Auth,
            G::Network | G::Server => ErrorKind::Remote,
            G::Forbidden | G::NotFound | G::BadRequest | G::Conflict | G::PreconditionFailed => ErrorKind::Permanent,
            _ => ErrorKind::Remote,
        };
        let retry_after = e.retry_after();
        let mut err = Self::new(kind, "Microsoft Graph request failed").with_source(e);
        err.retry_after = retry_after;
        err
    }
}

impl From<quorum_store::Error> for Error {
    fn from(e: quorum_store::Error) -> Self {
        Self::new(ErrorKind::Store, "local store failed").with_source(e)
    }
}

/// Sync result alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;
