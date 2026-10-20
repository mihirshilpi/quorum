//! The crate's canonical error type (M-ERRORS-CANONICAL-STRUCTS).

use std::backtrace::Backtrace;
use std::fmt;

use crate::claims::ClaimsChallenge;

/// Result alias used throughout `quorum-auth`.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// What went wrong, coarse enough for callers to decide what to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The token endpoint or the network could not be reached, or it answered with a
    /// transient failure (5xx, 429). Retrying later may succeed.
    Network,
    /// The identity provider answered with something we could not use (malformed JSON,
    /// missing fields, an unexpected OAuth error, a bad ID token).
    InvalidResponse,
    /// The user has to sign in interactively again (`invalid_grant`, `interaction_required`,
    /// a Conditional Access claims challenge, or no stored refresh token).
    /// See [`Error::claims`] for a claims challenge to pass to the next sign-in.
    InteractionRequired,
    /// The user cancelled or declined the sign-in.
    Cancelled,
    /// The sign-in did not finish in time (loopback callback or device code expired).
    Timeout,
    /// The loopback callback carried a `state` (or the ID token a `nonce`) that does not match
    /// the one we sent, which indicates a forged or replayed response.
    StateMismatch,
    /// The OS credential store (keychain) failed.
    Store,
    /// The configuration is invalid (bad tenant, client id, authority, scopes) or the app
    /// registration was rejected by the identity provider (`invalid_client`).
    Config,
    /// An internal contract was violated. Reported, never a crash (ADR 0000).
    Bug,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Network => "network error",
            Self::InvalidResponse => "invalid response from identity provider",
            Self::InteractionRequired => "interactive sign-in required",
            Self::Cancelled => "sign-in cancelled",
            Self::Timeout => "sign-in timed out",
            Self::StateMismatch => "sign-in response did not match the request",
            Self::Store => "credential store error",
            Self::Config => "invalid authentication configuration",
            Self::Bug => "internal error",
        };
        f.write_str(s)
    }
}

/// An authentication error.
///
/// Messages never contain tokens, codes or user names; they are safe to log.
pub struct Error {
    inner: Box<Inner>,
}

struct Inner {
    kind: ErrorKind,
    message: String,
    oauth_error: Option<String>,
    aadsts: Option<u32>,
    claims: Option<ClaimsChallenge>,
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    backtrace: Backtrace,
}

impl Error {
    /// Creates an error of `kind` with a short, secret-free message.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            inner: Box::new(Inner {
                kind,
                message: message.into(),
                oauth_error: None,
                aadsts: None,
                claims: None,
                source: None,
                backtrace: Backtrace::capture(),
            }),
        }
    }

    pub(crate) fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.inner.source = Some(Box::new(source));
        self
    }

    pub(crate) fn with_oauth_error(mut self, code: impl Into<String>) -> Self {
        self.inner.oauth_error = Some(code.into());
        self
    }

    pub(crate) fn with_aadsts(mut self, code: Option<u32>) -> Self {
        self.inner.aadsts = code;
        self
    }

    pub(crate) fn with_claims(mut self, claims: Option<ClaimsChallenge>) -> Self {
        self.inner.claims = claims;
        self
    }

    /// The error category.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.inner.kind
    }

    /// The OAuth `error` code returned by the identity provider (e.g. `invalid_grant`), if any.
    #[must_use]
    pub fn oauth_error(&self) -> Option<&str> {
        self.inner.oauth_error.as_deref()
    }

    /// The Entra ID error number (`AADSTS65001` → `65001`), if the identity provider sent one.
    #[must_use]
    pub fn aadsts(&self) -> Option<u32> {
        self.inner.aadsts
    }

    /// What the person (or their admin) can do about this error, in plain words, for the
    /// Entra ID errors that come from setup or policy rather than from the app.
    #[must_use]
    pub fn help(&self) -> Option<&'static str> {
        self.inner.aadsts.and_then(aadsts_help)
    }

    /// A Conditional Access claims challenge to pass to the next interactive sign-in
    /// ([`crate::SignInOptions::claims`]), if the identity provider sent one.
    #[must_use]
    pub fn claims(&self) -> Option<&ClaimsChallenge> {
        self.inner.claims.as_ref()
    }

    /// True when the user must sign in interactively again.
    #[must_use]
    pub fn is_interaction_required(&self) -> bool {
        self.inner.kind == ErrorKind::InteractionRequired
    }

    /// The backtrace captured when the error was created (empty unless `RUST_BACKTRACE` is set).
    #[must_use]
    pub fn backtrace(&self) -> &Backtrace {
        &self.inner.backtrace
    }
}

/// Plain-words help for the Entra ID (`AADSTS`) errors people hit while setting Quorum up.
#[must_use]
pub fn aadsts_help(code: u32) -> Option<&'static str> {
    Some(match code {
        65001 | 65004 => {
            "Your organisation hasn't approved Quorum yet. Ask your IT admin to grant admin consent (Settings ▸ Account ▸ Copy admin consent link)."
        }
        90094 | 90095 | 90099 => {
            "Quorum needs an administrator's approval in your organisation. Send your IT admin the admin consent link (Settings ▸ Account)."
        }
        50020 | 90072 => {
            "This account isn't part of the organisation Quorum is set up for. Sign in with your work account, or check the Directory (tenant) ID."
        }
        90002 | 90023 => "The Directory (tenant) ID wasn't found. Check it with your IT admin.",
        700016 => {
            "No app with this Application (client) ID exists in your organisation. Check both IDs with your IT admin."
        }
        50011 | 500113 => {
            "The app registration is missing its redirect address. Your admin should add http://localhost as a Mobile and desktop (public client) redirect URI."
        }
        7000218 => {
            "Device-code sign-in is turned off for this app. Your admin should set Allow public client flows to Yes, or sign in with the browser."
        }
        50105 => "Your account hasn't been given access to Quorum. Ask your IT admin to assign you to the app.",
        53000 | 53001 | 530003 => {
            "Your organisation only allows sign-in from managed devices. Enrol this Mac with your IT department or ask about an exception for Quorum."
        }
        53003 | 530032 => {
            "Your organisation's sign-in policy (Conditional Access) blocked this sign-in. Contact your IT admin and mention the Quorum app."
        }
        50076 | 50079 | 50074 => "Your organisation needs extra verification. Finish it in the browser and try again.",
        50057 | 50053 => "This account is disabled or locked. Contact your IT admin.",
        70011 | 65005 => {
            "The app registration doesn't allow a permission Quorum asks for. Your admin should re-run the Quorum setup script or add the listed Graph permissions."
        }
        _ => return None,
    })
}

/// The first `AADSTS<digits>` number in an error description.
pub(crate) fn parse_aadsts(text: &str) -> Option<u32> {
    let at = text.find("AADSTS")?;
    let digits: String = text.get(at.saturating_add(6)..)?.chars().take_while(char::is_ascii_digit).take(9).collect();
    digits.parse().ok()
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Error")
            .field("kind", &self.inner.kind)
            .field("message", &self.inner.message)
            .field("oauth_error", &self.inner.oauth_error)
            .field("claims", &self.inner.claims.as_ref().map(|_| "<present>"))
            .field("source", &self.inner.source)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.inner.kind, self.inner.message)?;
        if let Some(code) = &self.inner.oauth_error {
            write!(f, " ({code})")?;
        }
        if let Some(n) = self.inner.aadsts {
            write!(f, " AADSTS{n}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.inner.source.as_ref().map(|e| e.as_ref() as &(dyn std::error::Error + 'static))
    }
}

impl From<ErrorKind> for Error {
    fn from(kind: ErrorKind) -> Self {
        Self::new(kind, kind.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        let kind = if e.kind() == std::io::ErrorKind::TimedOut { ErrorKind::Timeout } else { ErrorKind::Network };
        Self::new(kind, "I/O error").with_source(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::new(ErrorKind::InvalidResponse, "malformed JSON").with_source(e)
    }
}

impl From<keyring::Error> for Error {
    fn from(e: keyring::Error) -> Self {
        Self::new(ErrorKind::Store, "credential store error").with_source(e)
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        // A slow token endpoint is a transient network failure, not a sign-in timeout.
        // reqwest errors can embed the URL; ours carry no secrets in URLs (form bodies only).
        Self::new(ErrorKind::Network, "HTTP request failed").with_source(e.without_url())
    }
}

impl From<url::ParseError> for Error {
    fn from(e: url::ParseError) -> Self {
        Self::new(ErrorKind::Config, "invalid URL").with_source(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aadsts_codes_parse_and_explain() {
        assert_eq!(parse_aadsts("AADSTS65001: The user or administrator has not consented"), Some(65001));
        assert_eq!(parse_aadsts("x AADSTS7000218 y"), Some(7_000_218));
        assert_eq!(parse_aadsts("AADSTS"), None);
        assert_eq!(parse_aadsts("AADSTS99999999999999"), Some(999_999_999));
        assert_eq!(parse_aadsts("nothing"), None);
        let e = Error::new(ErrorKind::InteractionRequired, "x").with_aadsts(Some(65001));
        assert!(e.help().is_some_and(|h| h.contains("admin consent")));
        assert!(e.to_string().ends_with("AADSTS65001"));
        assert!(Error::new(ErrorKind::Network, "x").help().is_none());
        assert!(aadsts_help(1).is_none());
    }
}
