//! The crate's canonical error type (M-ERRORS-CANONICAL-STRUCTS).

use std::backtrace::Backtrace;
use std::fmt;

/// Convenience alias for results returned by this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// What went wrong, coarsely; callers branch on this (see [`Error::kind`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// SQLite reported an error (I/O inside the database, constraint, busy after the timeout).
    Sqlite,
    /// The schema could not be migrated, or the database is from a newer version of Quorum.
    Migration,
    /// Encryption or decryption failed (wrong key, tampered ciphertext, key source failed).
    Crypto,
    /// The requested row does not exist.
    NotFound,
    /// A stored row could not be decoded (bad JSON, bad id, truncated ciphertext).
    Corrupt,
    /// A file-system operation failed (blob cache, database directory).
    Io,
    /// The caller passed a value the store rejects (bad cursor, oversized body or blob).
    InvalidInput,
    /// A contract violation inside this crate; logged and reported, never a panic (ADR 0000).
    Bug,
}

impl ErrorKind {
    /// Short stable name for logs and messages.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::Migration => "migration",
            Self::Crypto => "crypto",
            Self::NotFound => "not_found",
            Self::Corrupt => "corrupt",
            Self::Io => "io",
            Self::InvalidInput => "invalid_input",
            Self::Bug => "bug",
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An error from the local store.
///
/// The detail text is written by this crate and never contains tenant data (subjects, bodies,
/// names, addresses), so `Display` and `Debug` are safe to log (M-PUBLIC-DEBUG).
pub struct Error {
    inner: Box<Inner>,
}

struct Inner {
    kind: ErrorKind,
    detail: String,
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    backtrace: Backtrace,
}

impl Error {
    /// Creates an error of `kind` with a short description that must not contain tenant data.
    #[must_use]
    pub fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self { inner: Box::new(Inner { kind, detail: detail.into(), source: None, backtrace: Backtrace::capture() }) }
    }

    /// Attaches the underlying cause.
    #[must_use]
    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.inner.source = Some(Box::new(source));
        self
    }

    pub(crate) fn with_boxed_source(mut self, source: Box<dyn std::error::Error + Send + Sync + 'static>) -> Self {
        self.inner.source = Some(source);
        self
    }

    /// A contract violation: `debug_assert!`s in debug builds, returns [`ErrorKind::Bug`] in release.
    pub(crate) fn bug(detail: &'static str) -> Self {
        debug_assert!(false, "quorum-store bug: {detail}");
        tracing::error!(name: "store.bug", detail, "contract violation in quorum-store");
        Self::new(ErrorKind::Bug, detail)
    }

    pub(crate) fn corrupt(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::Corrupt, detail)
    }

    pub(crate) fn not_found(what: &'static str) -> Self {
        Self::new(ErrorKind::NotFound, format!("{what} not found"))
    }

    pub(crate) fn invalid(detail: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidInput, detail)
    }

    /// The coarse category of this error.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.inner.kind
    }

    /// The short description (never contains tenant data).
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.inner.detail
    }

    /// The backtrace captured when this error was created (empty unless `RUST_BACKTRACE` is set).
    #[must_use]
    pub fn backtrace(&self) -> &Backtrace {
        &self.inner.backtrace
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Error")
            .field("kind", &self.inner.kind)
            .field("detail", &self.inner.detail)
            .field("source", &self.inner.source)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "store error ({}): {}", self.inner.kind, self.inner.detail)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.inner.source.as_deref().map(|e| e as &(dyn std::error::Error + 'static))
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        // rusqlite's Display can include SQL text but never bound parameter values.
        let kind = match &e {
            rusqlite::Error::QueryReturnedNoRows => ErrorKind::NotFound,
            rusqlite::Error::FromSqlConversionFailure(..)
            | rusqlite::Error::IntegralValueOutOfRange(..)
            | rusqlite::Error::InvalidColumnType(..) => ErrorKind::Corrupt,
            _ => ErrorKind::Sqlite,
        };
        Self::new(kind, "sqlite operation failed").with_source(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::new(ErrorKind::Io, format!("i/o failed ({:?})", e.kind())).with_source(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        // serde_json errors name the line/column and expected type, never the value.
        Self::new(ErrorKind::Corrupt, format!("json decode failed ({:?})", e.classify())).with_source(e)
    }
}

impl From<quorum_model::Error> for Error {
    fn from(e: quorum_model::Error) -> Self {
        Self::new(ErrorKind::Corrupt, format!("invalid {}", e.type_name())).with_source(e)
    }
}

impl From<tokio::task::JoinError> for Error {
    fn from(e: tokio::task::JoinError) -> Self {
        // A panic inside a blocking store task: never re-raised (ADR 0000), reported as a bug.
        tracing::error!(name: "store.task_failed", panicked = e.is_panic(), "blocking store task failed");
        Self::new(ErrorKind::Bug, "blocking store task failed").with_source(e)
    }
}
