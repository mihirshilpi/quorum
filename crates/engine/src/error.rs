//! The engine's error type.

use std::backtrace::Backtrace;
use std::fmt;

/// What went wrong, in a form callers can match on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// No command or query has that id.
    UnknownCommand,
    /// Parameters were missing or malformed.
    InvalidParams,
    /// The command exists but cannot run now (signed out, offline, missing scope).
    NotAvailable,
    /// A requested item does not exist.
    NotFound,
    /// The store failed.
    Store,
    /// Microsoft Graph or the network failed.
    Remote,
    /// Sign-in is required or failed.
    Auth,
    /// A contract violation inside Quorum; logged and reported, never a crash.
    Bug,
}

impl ErrorKind {
    /// Stable snake_case name used on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnknownCommand => "unknown_command",
            Self::InvalidParams => "invalid_params",
            Self::NotAvailable => "not_available",
            Self::NotFound => "not_found",
            Self::Store => "store",
            Self::Remote => "remote",
            Self::Auth => "auth",
            Self::Bug => "bug",
        }
    }
}

/// Engine error: a kind, a user-safe message, an optional cause and a backtrace.
pub struct Error {
    kind: ErrorKind,
    message: String,
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    backtrace: Backtrace,
}

impl Error {
    /// Creates an error of `kind` with a user-safe `message` (no personal data).
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), source: None, backtrace: Backtrace::capture() }
    }

    /// Attaches the underlying cause.
    #[must_use]
    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// The error's kind.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The user-safe message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Backtrace captured at creation (empty unless `RUST_BACKTRACE` is set).
    pub fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }

    /// Shorthand for [`ErrorKind::InvalidParams`].
    pub fn params(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidParams, message)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Error").field("kind", &self.kind).field("message", &self.message).finish_non_exhaustive()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.as_str(), self.message)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_deref().map(|e| e as &(dyn std::error::Error + 'static))
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::new(ErrorKind::InvalidParams, "parameters are not valid for this command").with_source(e)
    }
}

/// Engine result alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;
