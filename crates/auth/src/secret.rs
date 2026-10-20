//! Secret-holding newtypes with redacting `Debug`/`Display` (M-PUBLIC-DEBUG).

use std::fmt;

use jiff::Timestamp;

const REDACTED: &str = "<redacted>";

macro_rules! secret_string {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq)]
        pub struct $name(String);

        impl $name {
            /// Wraps a secret value.
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// The secret value. Never log it.
            #[must_use]
            pub fn secret(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), REDACTED)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(REDACTED)
            }
        }
    };
}

secret_string!(
    /// An OAuth refresh token. Lives only in the [`crate::SecretStore`].
    RefreshToken
);
secret_string!(
    /// An authorization code received on the loopback redirect.
    AuthorizationCode
);
secret_string!(
    /// A PKCE code verifier (RFC 7636).
    PkceVerifier
);
secret_string!(
    /// A device-code-flow `device_code`.
    DeviceCodeSecret
);

/// A bearer access token for Microsoft Graph, kept only in memory.
///
/// `Debug` and `Display` never show the token.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessToken {
    token: String,
    expires_at: Timestamp,
}

impl AccessToken {
    /// Wraps a token that expires at `expires_at`.
    #[must_use]
    pub fn new(token: impl Into<String>, expires_at: Timestamp) -> Self {
        Self { token: token.into(), expires_at }
    }

    /// The raw token, for the `Authorization: Bearer` header. Never log it.
    #[must_use]
    pub fn secret(&self) -> &str {
        &self.token
    }

    /// When the identity provider says the token expires.
    #[must_use]
    pub fn expires_at(&self) -> Timestamp {
        self.expires_at
    }

    /// The `Authorization` header value (`Bearer <token>`). Never log it.
    #[must_use]
    pub fn bearer_header(&self) -> String {
        format!("Bearer {}", self.token)
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessToken").field("token", &REDACTED).field("expires_at", &self.expires_at).finish()
    }
}

impl fmt::Display for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANARY: &str = "canary-SECRET-7f3a9";

    #[test]
    fn secrets_never_leak_through_debug_or_display() {
        let at = AccessToken::new(CANARY, Timestamp::UNIX_EPOCH);
        let rendered = [
            format!("{at:?}"),
            format!("{at:#?}"),
            format!("{at}"),
            format!("{:?}", RefreshToken::new(CANARY)),
            format!("{}", RefreshToken::new(CANARY)),
            format!("{:?}", AuthorizationCode::new(CANARY)),
            format!("{}", AuthorizationCode::new(CANARY)),
            format!("{:?}", PkceVerifier::new(CANARY)),
            format!("{}", PkceVerifier::new(CANARY)),
            format!("{:?}", DeviceCodeSecret::new(CANARY)),
            format!("{:?}", Some(RefreshToken::new(CANARY))),
        ];
        for r in rendered {
            assert!(!r.contains(CANARY), "leaked: {r}");
            assert!(r.contains("redacted"), "not marked redacted: {r}");
        }
        assert_eq!(at.secret(), CANARY);
        assert_eq!(at.bearer_header(), format!("Bearer {CANARY}"));
    }
}
