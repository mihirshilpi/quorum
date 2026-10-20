//! The crate's canonical error type (M-ERRORS-CANONICAL-STRUCTS).

use std::backtrace::Backtrace;
use std::fmt;

/// A value failed validation while constructing a domain type.
///
/// Inspect [`Error::kind`] to see what went wrong and [`Error::type_name`] to see which type
/// rejected it. The rejected value itself is never stored, so the error is safe to log.
pub struct Error {
    kind: ErrorKind,
    type_name: &'static str,
    backtrace: Backtrace,
}

/// What kind of validation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The value was empty.
    Empty,
    /// The value was longer than the type allows.
    TooLong {
        /// The maximum length in bytes.
        max: usize,
    },
    /// The value contained a control character.
    ControlCharacter,
    /// The value is not a plausible SMTP address.
    InvalidAddress,
    /// The time zone name is not known and not an IANA name.
    UnknownTimeZone,
    /// The local date and time could not be resolved in the time zone.
    InvalidDateTime,
}

impl Error {
    pub(crate) fn new(kind: ErrorKind, type_name: &'static str) -> Self {
        Self { kind, type_name, backtrace: Backtrace::capture() }
    }

    /// Returns what kind of validation failed.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Returns the name of the type that rejected the value, e.g. `"MessageId"`.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        self.type_name
    }

    /// Returns the backtrace captured when the error was created.
    #[must_use]
    pub fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Error").field("kind", &self.kind).field("type_name", &self.type_name).finish_non_exhaustive()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.kind {
            ErrorKind::Empty => "value is empty".to_owned(),
            ErrorKind::TooLong { max } => format!("value is longer than {max} bytes"),
            ErrorKind::ControlCharacter => "value contains a control character".to_owned(),
            ErrorKind::InvalidAddress => "value is not a valid email address".to_owned(),
            ErrorKind::UnknownTimeZone => "unknown time zone".to_owned(),
            ErrorKind::InvalidDateTime => "date and time cannot be resolved".to_owned(),
        };
        write!(f, "invalid {}: {what}", self.type_name)
    }
}

impl std::error::Error for Error {}
