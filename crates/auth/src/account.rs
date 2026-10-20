//! The signed-in [`Account`], ID token parsing and granted scopes.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorKind, Result};

/// Longest ID token we decode.
const MAX_ID_TOKEN_LEN: usize = 64 * 1024;
/// Longest claim value we keep (names, user names).
const MAX_CLAIM_LEN: usize = 1024;

/// A signed-in work or school account.
///
/// `Debug` hides the user name and display name (PII); the id is a pair of opaque GUIDs.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    id: String,
    username: String,
    display_name: String,
    tenant_id: String,
}

impl Account {
    /// Builds an account from the ID token's `oid` and `tid` and profile claims.
    ///
    /// # Errors
    /// [`ErrorKind::InvalidResponse`] when `oid` or `tid` is empty, too long, or contains
    /// characters other than ASCII letters, digits and `-`.
    pub fn new(
        object_id: &str,
        tenant_id: &str,
        username: impl Into<String>,
        display_name: impl Into<String>,
    ) -> Result<Self> {
        let guid_like =
            |s: &str| !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        if !guid_like(object_id) || !guid_like(tenant_id) {
            return Err(Error::new(ErrorKind::InvalidResponse, "ID token has a malformed oid or tid"));
        }
        Ok(Self {
            id: format!("{object_id}.{tenant_id}"),
            username: truncate(username.into()),
            display_name: truncate(display_name.into()),
            tenant_id: tenant_id.to_owned(),
        })
    }

    /// Stable account id, `"{oid}.{tid}"`. Also the key in the [`crate::SecretStore`].
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The sign-in name (`preferred_username`, usually the UPN). PII: never log it.
    #[must_use]
    pub fn username(&self) -> &str {
        &self.username
    }

    /// The display name (`name`). PII: never log it.
    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// The directory (tenant) id.
    #[must_use]
    pub fn tenant_id(&self) -> &str {
        &self.tenant_id
    }
}

impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Account")
            .field("id", &self.id)
            .field("username", &"<redacted>")
            .field("display_name", &"<redacted>")
            .field("tenant_id", &self.tenant_id)
            .finish()
    }
}

fn truncate(mut s: String) -> String {
    if s.len() > MAX_CLAIM_LEN {
        let mut cut = MAX_CLAIM_LEN;
        while !s.is_char_boundary(cut) {
            cut = cut.saturating_sub(1);
        }
        s.truncate(cut);
    }
    s
}

/// The claims of an ID token that Quorum uses.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
pub(crate) struct IdClaims {
    #[serde(default)]
    pub oid: Option<String>,
    #[serde(default)]
    pub tid: Option<String>,
    #[serde(default)]
    pub preferred_username: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub nonce: Option<String>,
}

impl fmt::Debug for IdClaims {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdClaims").finish_non_exhaustive()
    }
}

impl IdClaims {
    /// Decodes the payload (second segment) of a compact JWT.
    ///
    /// **The signature is not verified.** That is deliberate and safe here: the ID token is
    /// only ever taken from the token endpoint's response, which we fetched ourselves over
    /// TLS from the configured authority, in exchange for a code bound to our PKCE verifier
    /// (OpenID Connect Core §3.1.3.7 allows skipping validation for tokens received directly
    /// from the token endpoint over TLS). We never accept ID tokens from the browser.
    pub(crate) fn parse(id_token: &str) -> Result<Self> {
        let bad = |m: &str| Error::new(ErrorKind::InvalidResponse, m);
        if id_token.len() > MAX_ID_TOKEN_LEN {
            return Err(bad("ID token too large"));
        }
        let mut parts = id_token.split('.');
        let (Some(_header), Some(payload), Some(_sig), None) = (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(bad("ID token is not a compact JWT"));
        };
        let bytes = URL_SAFE_NO_PAD
            .decode(payload)
            .or_else(|_| URL_SAFE.decode(payload))
            .map_err(|e| bad("ID token payload is not base64url").with_source(e))?;
        let claims: Self =
            serde_json::from_slice(&bytes).map_err(|e| bad("ID token payload is not a JSON object").with_source(e))?;
        Ok(claims)
    }

    /// Builds the account; requires `oid` and `tid`.
    pub(crate) fn account(&self) -> Result<Account> {
        let (Some(oid), Some(tid)) = (&self.oid, &self.tid) else {
            return Err(Error::new(ErrorKind::InvalidResponse, "ID token lacks oid or tid"));
        };
        Account::new(
            oid,
            tid,
            self.preferred_username.clone().unwrap_or_default(),
            self.name.clone().unwrap_or_default(),
        )
    }

    /// Checks the `nonce` claim against the one sent on the authorize request.
    pub(crate) fn check_nonce(&self, expected: &str) -> Result<()> {
        match &self.nonce {
            Some(n) if constant_time_eq(n.as_bytes(), expected.as_bytes()) => Ok(()),
            _ => Err(Error::new(ErrorKind::StateMismatch, "ID token nonce does not match")),
        }
    }
}

/// Compares two byte strings without an early exit on the first difference.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The scopes granted to the current access token (the token response's `scope`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantedScopes(Vec<String>);

const GRAPH_PREFIX: &str = "https://graph.microsoft.com/";

impl GrantedScopes {
    /// Parses a space-separated `scope` string.
    #[must_use]
    pub fn parse(scope: &str) -> Self {
        let mut v: Vec<String> = scope.split_whitespace().take(1024).map(|s| truncate(s.to_owned())).collect();
        v.sort();
        v.dedup();
        Self(v)
    }

    /// The granted scopes, as returned by Entra (e.g. `Mail.ReadWrite`, `openid`).
    #[must_use]
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    /// True if `scope` was granted. Case-insensitive; the Graph resource prefix
    /// (`https://graph.microsoft.com/`) is ignored on both sides.
    #[must_use]
    pub fn contains(&self, scope: &str) -> bool {
        let want = normalize(scope);
        self.0.iter().any(|g| normalize(g).eq_ignore_ascii_case(want))
    }

    /// The scopes from `required` that were not granted, in the order given.
    #[must_use]
    pub fn missing_scopes(&self, required: &[&str]) -> Vec<String> {
        required.iter().filter(|r| !self.contains(r)).map(|r| (*r).to_owned()).collect()
    }
}

fn normalize(scope: &str) -> &str {
    match scope.get(..GRAPH_PREFIX.len()) {
        Some(p) if p.eq_ignore_ascii_case(GRAPH_PREFIX) => scope.get(GRAPH_PREFIX.len()..).unwrap_or(scope),
        _ => scope,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(payload: &str) -> String {
        format!("eyJhbGciOiJub25lIn0.{}.sig", URL_SAFE_NO_PAD.encode(payload))
    }

    #[test]
    fn parses_id_token_claims() {
        let t = jwt(
            r#"{"oid":"11111111-1111-1111-1111-111111111111","tid":"22222222-2222-2222-2222-222222222222","preferred_username":"user@example.test","name":"Test User","nonce":"n-1","aud":"x","exp":1}"#,
        );
        let c = IdClaims::parse(&t).unwrap();
        let a = c.account().unwrap();
        assert_eq!(a.id(), "11111111-1111-1111-1111-111111111111.22222222-2222-2222-2222-222222222222");
        assert_eq!(a.username(), "user@example.test");
        assert_eq!(a.display_name(), "Test User");
        assert_eq!(a.tenant_id(), "22222222-2222-2222-2222-222222222222");
        assert!(c.check_nonce("n-1").is_ok());
        assert_eq!(c.check_nonce("n-2").unwrap_err().kind(), ErrorKind::StateMismatch);
        let dbg = format!("{a:?} {c:?}");
        assert!(!dbg.contains("user@example.test") && !dbg.contains("Test User"));
    }

    #[test]
    fn hostile_id_tokens_are_errors_not_panics() {
        let big = "a".repeat(MAX_ID_TOKEN_LEN + 1);
        for t in [
            "",
            ".",
            "..",
            "a.b",
            "a.b.c.d",
            "a.!!!.c",
            "a.\u{1F600}.c",
            &jwt("not json"),
            &jwt("[1,2]"),
            &jwt(r#"{"oid":5}"#),
            &big,
        ] {
            assert!(IdClaims::parse(t).is_err(), "{t:?}");
        }
        // Parses, but lacks/garbles oid/tid.
        for p in [r#"{}"#, r#"{"oid":"a"}"#, r#"{"oid":"a/b","tid":"c"}"#, r#"{"oid":"","tid":"c"}"#] {
            assert!(IdClaims::parse(&jwt(p)).unwrap().account().is_err(), "{p}");
        }
        assert!(IdClaims::parse(&jwt("{}")).unwrap().check_nonce("x").is_err());
    }

    #[test]
    fn missing_scopes_normalises_prefix_and_case() {
        let g = GrantedScopes::parse("Mail.ReadWrite https://graph.microsoft.com/Chat.ReadWrite openid  profile");
        assert!(g.contains("mail.readwrite"));
        assert!(g.contains("https://graph.microsoft.com/Mail.ReadWrite"));
        assert!(g.contains("Chat.ReadWrite"));
        assert_eq!(
            g.missing_scopes(&["Mail.ReadWrite", "Mail.Send", "Chat.ReadWrite", "Presence.Read.All"]),
            ["Mail.Send", "Presence.Read.All"]
        );
        assert!(GrantedScopes::parse("").missing_scopes(&[]).is_empty());
    }
}
