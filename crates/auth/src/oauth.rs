//! Token endpoint requests and response parsing (authorization code, refresh, device code).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;
use url::Url;

use crate::account::{GrantedScopes, IdClaims};
use crate::claims::ClaimsChallenge;
use crate::config::AuthConfig;
use crate::error::{Error, ErrorKind, Result};
use crate::secret::{AccessToken, AuthorizationCode, DeviceCodeSecret, PkceVerifier, RefreshToken};
use crate::transport::{FormRequest, FormResponse, Transport};

/// Longest lifetime we believe for an access token or device code.
const MAX_EXPIRES_IN: i64 = 48 * 3600;
const DEFAULT_EXPIRES_IN: i64 = 3600;
const MAX_FIELD_LEN: usize = 32 * 1024;

/// A successful token response.
pub(crate) struct TokenSet {
    pub access_token: AccessToken,
    pub refresh_token: Option<RefreshToken>,
    pub id_claims: Option<IdClaims>,
    pub scopes: GrantedScopes,
}

impl fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenSet")
            .field("access_token", &self.access_token)
            .field("refresh_token", &self.refresh_token)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct RawToken {
    access_token: String,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<serde_json::Value>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawError {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_codes: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    claims: Option<String>,
}

#[derive(Deserialize)]
struct RawDeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    expires_in: Option<serde_json::Value>,
    #[serde(default)]
    interval: Option<serde_json::Value>,
    #[serde(default)]
    message: Option<String>,
}

/// A device-code sign-in in progress (`quorum-cli login --device`).
///
/// Show [`DeviceCode::message`] (or the code and URI) to the user, then call
/// [`crate::Authenticator::finish_device_code`].
#[derive(Clone)]
pub struct DeviceCode {
    pub(crate) device_code: DeviceCodeSecret,
    user_code: String,
    verification_uri: Url,
    message: String,
    expires_at: Timestamp,
    pub(crate) interval: Duration,
}

impl DeviceCode {
    /// The short code the user types at [`DeviceCode::verification_uri`].
    #[must_use]
    pub fn user_code(&self) -> &str {
        &self.user_code
    }

    /// Where the user enters the code (e.g. `https://microsoft.com/devicelogin`).
    #[must_use]
    pub fn verification_uri(&self) -> &Url {
        &self.verification_uri
    }

    /// The instruction text from Entra, ready to print.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// When the code stops working.
    #[must_use]
    pub fn expires_at(&self) -> Timestamp {
        self.expires_at
    }
}

impl fmt::Debug for DeviceCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceCode")
            .field("device_code", &self.device_code)
            .field("user_code", &"<redacted>")
            .field("verification_uri", &self.verification_uri.as_str())
            .field("expires_at", &self.expires_at)
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
}

/// Result of one device-code poll.
#[derive(Debug)]
pub(crate) enum Poll {
    Done(Box<TokenSet>),
    Pending,
    SlowDown,
}

/// Talks to the token endpoint.
#[derive(Debug, Clone)]
pub(crate) struct OAuth {
    pub config: AuthConfig,
    transport: Arc<dyn Transport>,
}

impl OAuth {
    pub fn new(config: AuthConfig, transport: Arc<dyn Transport>) -> Self {
        Self { config, transport }
    }

    fn base_form(&self, grant_type: &str) -> Vec<(String, String)> {
        vec![
            ("client_id".to_owned(), self.config.client_id().to_owned()),
            ("grant_type".to_owned(), grant_type.to_owned()),
            ("scope".to_owned(), self.config.scope_param()),
        ]
    }

    async fn post_token(&self, form: Vec<(String, String)>) -> Result<FormResponse> {
        let url = self.config.token_endpoint()?;
        self.transport.post_form(FormRequest { url, form }).await
    }

    pub async fn exchange_code(
        &self,
        code: &AuthorizationCode,
        verifier: &PkceVerifier,
        redirect_uri: &str,
        now: Timestamp,
    ) -> Result<TokenSet> {
        let mut form = self.base_form("authorization_code");
        form.push(("code".to_owned(), code.secret().to_owned()));
        form.push(("redirect_uri".to_owned(), redirect_uri.to_owned()));
        form.push(("code_verifier".to_owned(), verifier.secret().to_owned()));
        let resp = self.post_token(form).await?;
        parse_token_response(&resp, now)
    }

    pub async fn refresh(&self, refresh_token: &RefreshToken, now: Timestamp) -> Result<TokenSet> {
        let mut form = self.base_form("refresh_token");
        form.push(("refresh_token".to_owned(), refresh_token.secret().to_owned()));
        let resp = self.post_token(form).await?;
        parse_token_response(&resp, now)
    }

    pub async fn start_device_code(&self, now: Timestamp) -> Result<DeviceCode> {
        let url = self.config.device_code_endpoint()?;
        let form = vec![
            ("client_id".to_owned(), self.config.client_id().to_owned()),
            ("scope".to_owned(), self.config.scope_param()),
        ];
        let resp = self.transport.post_form(FormRequest { url, form }).await?;
        if resp.status != 200 {
            return Err(error_from_response(&resp));
        }
        let raw: RawDeviceCode = serde_json::from_slice(&resp.body)?;
        let too_long = [&raw.device_code, &raw.user_code, &raw.verification_uri]
            .iter()
            .any(|s| s.is_empty() || s.len() > MAX_FIELD_LEN);
        if too_long || raw.message.as_ref().is_some_and(|m| m.len() > MAX_FIELD_LEN) {
            return Err(Error::new(ErrorKind::InvalidResponse, "device code response has bad fields"));
        }
        let verification_uri = Url::parse(&raw.verification_uri)
            .map_err(|e| Error::new(ErrorKind::InvalidResponse, "bad verification_uri").with_source(e))?;
        if verification_uri.scheme() != "https" && verification_uri.scheme() != "http" {
            return Err(Error::new(ErrorKind::InvalidResponse, "bad verification_uri scheme"));
        }
        let expires_in = seconds(raw.expires_in.as_ref()).unwrap_or(900).clamp(1, MAX_EXPIRES_IN);
        let interval = seconds(raw.interval.as_ref()).unwrap_or(5).clamp(1, 60);
        let message = raw.message.unwrap_or_else(|| {
            format!("To sign in, open {} and enter the code {}.", raw.verification_uri, raw.user_code)
        });
        Ok(DeviceCode {
            device_code: DeviceCodeSecret::new(raw.device_code),
            user_code: raw.user_code,
            verification_uri,
            message,
            expires_at: add_secs(now, expires_in),
            interval: Duration::from_secs(interval.unsigned_abs()),
        })
    }

    pub async fn poll_device_code(&self, code: &DeviceCode, now: Timestamp) -> Result<Poll> {
        let mut form = self.base_form("urn:ietf:params:oauth:grant-type:device_code");
        form.push(("device_code".to_owned(), code.device_code.secret().to_owned()));
        let resp = self.post_token(form).await?;
        match parse_token_response(&resp, now) {
            Ok(set) => Ok(Poll::Done(Box::new(set))),
            Err(e) if e.oauth_error() == Some("authorization_pending") => Ok(Poll::Pending),
            Err(e) if e.oauth_error() == Some("slow_down") => Ok(Poll::SlowDown),
            Err(e) => Err(e),
        }
    }
}

fn seconds(v: Option<&serde_json::Value>) -> Option<i64> {
    match v? {
        serde_json::Value::Number(n) => n.as_i64().or_else(|| n.as_u64().map(|_| i64::MAX)),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn add_secs(now: Timestamp, secs: i64) -> Timestamp {
    now.checked_add(SignedDuration::from_secs(secs)).unwrap_or(Timestamp::MAX)
}

/// Parses a token endpoint response; non-200 answers become typed errors.
pub(crate) fn parse_token_response(resp: &FormResponse, now: Timestamp) -> Result<TokenSet> {
    if resp.status != 200 {
        return Err(error_from_response(resp));
    }
    let raw: RawToken = serde_json::from_slice(&resp.body)?;
    if raw.access_token.is_empty() || raw.access_token.len() > MAX_FIELD_LEN {
        return Err(Error::new(ErrorKind::InvalidResponse, "access_token missing or too large"));
    }
    if raw.token_type.as_deref().is_some_and(|t| !t.eq_ignore_ascii_case("bearer")) {
        return Err(Error::new(ErrorKind::InvalidResponse, "unexpected token_type"));
    }
    let expires_in = seconds(raw.expires_in.as_ref()).unwrap_or(DEFAULT_EXPIRES_IN).clamp(0, MAX_EXPIRES_IN);
    let refresh_token = match raw.refresh_token {
        Some(rt) if rt.len() > MAX_FIELD_LEN => {
            return Err(Error::new(ErrorKind::InvalidResponse, "refresh_token too large"));
        }
        Some(rt) if !rt.is_empty() => Some(RefreshToken::new(rt)),
        _ => None,
    };
    let id_claims = match raw.id_token.as_deref() {
        Some(t) if !t.is_empty() => Some(IdClaims::parse(t)?),
        _ => None,
    };
    Ok(TokenSet {
        access_token: AccessToken::new(raw.access_token, add_secs(now, expires_in)),
        refresh_token,
        id_claims,
        scopes: GrantedScopes::parse(raw.scope.as_deref().unwrap_or_default()),
    })
}

/// Maps an OAuth error response (or a non-JSON failure) to an [`Error`].
pub(crate) fn error_from_response(resp: &FormResponse) -> Error {
    let raw: RawError = serde_json::from_slice(&resp.body).unwrap_or_default();
    let Some(code) = raw.error.filter(|c| !c.is_empty()) else {
        let kind = if resp.status >= 500 || resp.status == 429 || resp.status == 408 {
            ErrorKind::Network
        } else {
            ErrorKind::InvalidResponse
        };
        return Error::new(kind, format!("identity provider answered HTTP {}", resp.status));
    };
    // Keep only a safe, short form of the code; descriptions may contain user names.
    let code: String = code
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(64)
        .collect::<String>()
        .to_ascii_lowercase();
    let kind = match code.as_str() {
        "invalid_grant" | "interaction_required" | "login_required" | "consent_required" => {
            ErrorKind::InteractionRequired
        }
        "access_denied" | "authorization_declined" => ErrorKind::Cancelled,
        "expired_token" => ErrorKind::Timeout,
        "invalid_client" | "unauthorized_client" | "invalid_scope" | "unsupported_grant_type" | "invalid_resource" => {
            ErrorKind::Config
        }
        "temporarily_unavailable" | "server_error" => ErrorKind::Network,
        _ => ErrorKind::InvalidResponse,
    };
    let aadsts = raw
        .error_codes
        .as_ref()
        .and_then(|c| c.first())
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());
    let claims = raw.claims.as_deref().and_then(ClaimsChallenge::from_encoded);
    Error::new(kind, format!("token endpoint returned HTTP {}", resp.status))
        .with_oauth_error(code)
        .with_aadsts(aadsts)
        .with_claims(claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(status: u16, body: &str) -> FormResponse {
        FormResponse { status, body: body.as_bytes().to_vec() }
    }

    fn now() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    #[test]
    fn parses_success() {
        let set = parse_token_response(
            &resp(
                200,
                r#"{"token_type":"Bearer","access_token":"at","expires_in":"3599","refresh_token":"rt","scope":"Mail.Read openid"}"#,
            ),
            now(),
        )
        .unwrap();
        assert_eq!(set.access_token.secret(), "at");
        assert_eq!(set.access_token.expires_at(), add_secs(now(), 3599));
        assert_eq!(set.refresh_token.unwrap().secret(), "rt");
        assert!(set.scopes.contains("Mail.Read"));
    }

    #[test]
    fn maps_oauth_errors() {
        let e = parse_token_response(
            &resp(400, r#"{"error":"invalid_grant","error_description":"AADSTS50173: user@example.test","error_codes":[50173]}"#),
            now(),
        )
        .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::InteractionRequired);
        assert_eq!(e.oauth_error(), Some("invalid_grant"));
        assert!(!e.to_string().contains("user@example.test"));
        assert!(e.to_string().contains("AADSTS50173"));

        let e = parse_token_response(
            &resp(400, r#"{"error":"interaction_required","claims":"{\"access_token\":{\"capolids\":{\"essential\":true,\"values\":[\"1\"]}}}"}"#),
            now(),
        )
        .unwrap_err();
        assert!(e.is_interaction_required());
        assert!(e.claims().is_some());

        assert_eq!(parse_token_response(&resp(503, "<html>"), now()).unwrap_err().kind(), ErrorKind::Network);
        assert_eq!(
            parse_token_response(&resp(400, r#"{"error":"invalid_client"}"#), now()).unwrap_err().kind(),
            ErrorKind::Config
        );
    }

    #[test]
    fn garbage_token_json_never_panics() {
        let cases: &[(u16, &[u8])] = &[
            (200, b""),
            (200, b"null"),
            (200, b"[]"),
            (200, b"{}"),
            (200, b"{\"access_token\":5}"),
            (200, b"{\"access_token\":\"\"}"),
            (200, b"{\"access_token\":\"a\",\"token_type\":\"pop\"}"),
            (200, b"{\"access_token\":\"a\",\"id_token\":\"garbage\"}"),
            (200, b"\xff\xfe\x00"),
            (200, b"{\"access_token\":\"a\",\"expires_in\":1e400}"),
            (200, &[b'['; 10_000]),
            (400, b""),
            (400, b"{\"error\":12}"),
            (400, b"{\"error\":\"\\u0000\\ud800\"}"),
            (0, b"x"),
            (65535, b"{\"error\":\"x\",\"error_codes\":\"nope\",\"claims\":7}"),
        ];
        for (status, body) in cases {
            let r = FormResponse { status: *status, body: body.to_vec() };
            assert!(parse_token_response(&r, now()).is_err(), "{status} {:?}", String::from_utf8_lossy(body));
        }
        // Absurd lifetimes are clamped, not overflowed.
        for exp in ["-5", "99999999999999999999", "\"abc\"", "{}", "18446744073709551615"] {
            let body = format!("{{\"access_token\":\"a\",\"expires_in\":{exp}}}");
            let set = parse_token_response(&resp(200, &body), now()).unwrap();
            assert!(set.access_token.expires_at() <= add_secs(now(), MAX_EXPIRES_IN));
        }
    }
}
