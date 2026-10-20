//! [`AuthConfig`] and its builder (M-INIT-BUILDER).

use url::Url;

use crate::error::{Error, ErrorKind, Result};

/// The default Entra ID authority host.
pub const DEFAULT_AUTHORITY_HOST: &str = "https://login.microsoftonline.com";

/// The default scopes: everything the admin consented to for Graph, plus a refresh token and
/// an ID token.
pub const DEFAULT_SCOPES: &[&str] = &["https://graph.microsoft.com/.default", "offline_access", "openid", "profile"];

const MAX_FIELD_LEN: usize = 256;

/// Entra ID app registration and authority settings.
///
/// Build with [`AuthConfig::builder`]; [`AuthConfigBuilder::build`] validates every field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthConfig {
    tenant: String,
    client_id: String,
    authority_host: Url,
    scopes: Vec<String>,
}

impl AuthConfig {
    /// Starts a builder for the public client `client_id`.
    ///
    /// The tenant defaults to `organizations` (any work or school account).
    #[must_use]
    pub fn builder(client_id: impl Into<String>) -> AuthConfigBuilder {
        AuthConfigBuilder {
            tenant: "organizations".to_owned(),
            client_id: client_id.into(),
            authority_host: DEFAULT_AUTHORITY_HOST.to_owned(),
            scopes: DEFAULT_SCOPES.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    /// The tenant id, verified domain, or `organizations`.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// The application (client) id.
    #[must_use]
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// The authority host, e.g. `https://login.microsoftonline.com/`.
    #[must_use]
    pub fn authority_host(&self) -> &Url {
        &self.authority_host
    }

    /// The scopes requested at sign-in.
    #[must_use]
    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }

    /// The scopes joined with spaces, as sent on the wire.
    pub(crate) fn scope_param(&self) -> String {
        self.scopes.join(" ")
    }

    fn endpoint(&self, leaf: &str) -> Result<Url> {
        let mut url = self.authority_host.clone();
        url.path_segments_mut()
            .map_err(|()| Error::new(ErrorKind::Config, "authority host cannot be a base"))?
            .pop_if_empty()
            .extend([self.tenant.as_str(), "oauth2", "v2.0", leaf]);
        Ok(url)
    }

    /// `{authority}/{tenant}/oauth2/v2.0/authorize`.
    pub(crate) fn authorize_endpoint(&self) -> Result<Url> {
        self.endpoint("authorize")
    }

    /// `{authority}/{tenant}/oauth2/v2.0/token`.
    pub(crate) fn token_endpoint(&self) -> Result<Url> {
        self.endpoint("token")
    }

    /// `{authority}/{tenant}/oauth2/v2.0/devicecode`.
    pub(crate) fn device_code_endpoint(&self) -> Result<Url> {
        self.endpoint("devicecode")
    }
}

/// Builder for [`AuthConfig`].
#[derive(Debug, Clone)]
pub struct AuthConfigBuilder {
    tenant: String,
    client_id: String,
    authority_host: String,
    scopes: Vec<String>,
}

impl AuthConfigBuilder {
    /// Sets the tenant: a tenant id (GUID), a verified domain, or `organizations`.
    #[must_use]
    pub fn tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = tenant.into();
        self
    }

    /// Overrides the authority host (sovereign clouds, or a local fake in tests).
    #[must_use]
    pub fn authority_host(mut self, host: impl Into<String>) -> Self {
        self.authority_host = host.into();
        self
    }

    /// Replaces the requested scopes.
    #[must_use]
    pub fn scopes<I, S>(mut self, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.scopes = scopes.into_iter().map(Into::into).collect();
        self
    }

    /// Validates and builds the configuration.
    ///
    /// # Errors
    /// [`ErrorKind::Config`] when the client id or tenant is empty or contains characters
    /// other than letters, digits, `-`, `.` and `_`; when the authority host is not an
    /// `https` URL (plain `http` is allowed only for `localhost`/`127.0.0.1`, for tests);
    /// or when the scope list is empty or a scope contains whitespace.
    pub fn build(self) -> Result<AuthConfig> {
        let config_err = |m: &str| Error::new(ErrorKind::Config, m);
        let ident_ok = |s: &str| {
            !s.is_empty()
                && s.len() <= MAX_FIELD_LEN
                && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
        };
        if !ident_ok(&self.client_id) {
            return Err(config_err("client id is empty or malformed"));
        }
        if !ident_ok(&self.tenant) || self.tenant.starts_with('.') {
            return Err(config_err("tenant is empty or malformed"));
        }

        let host =
            Url::parse(&self.authority_host).map_err(|e| config_err("authority host is not a URL").with_source(e))?;
        let local = matches!(host.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        let scheme_ok = host.scheme() == "https" || (host.scheme() == "http" && local);
        if !scheme_ok || host.cannot_be_a_base() || host.host_str().is_none() {
            return Err(config_err("authority host must be an https URL"));
        }
        if host.query().is_some() || host.fragment().is_some() || !host.username().is_empty() {
            return Err(config_err("authority host must not carry a query or credentials"));
        }

        if self.scopes.is_empty() {
            return Err(config_err("no scopes requested"));
        }
        if self.scopes.iter().any(|s| s.is_empty() || s.len() > MAX_FIELD_LEN || s.chars().any(char::is_whitespace)) {
            return Err(config_err("scope is empty or contains whitespace"));
        }

        Ok(AuthConfig { tenant: self.tenant, client_id: self.client_id, authority_host: host, scopes: self.scopes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_endpoints() {
        let c = AuthConfig::builder("00000000-0000-0000-0000-000000000001").build().unwrap();
        assert_eq!(c.tenant(), "organizations");
        assert_eq!(c.scopes().len(), 4);
        assert_eq!(
            c.token_endpoint().unwrap().as_str(),
            "https://login.microsoftonline.com/organizations/oauth2/v2.0/token"
        );
        let c = AuthConfig::builder("cid")
            .tenant("contoso.example")
            .authority_host("https://login.example.test/base/")
            .build()
            .unwrap();
        assert_eq!(
            c.authorize_endpoint().unwrap().as_str(),
            "https://login.example.test/base/contoso.example/oauth2/v2.0/authorize"
        );
    }

    #[test]
    fn rejects_bad_config() {
        assert!(AuthConfig::builder("").build().is_err());
        assert!(AuthConfig::builder("a b").build().is_err());
        assert!(AuthConfig::builder("cid").tenant("../x").build().is_err());
        assert!(AuthConfig::builder("cid").tenant("").build().is_err());
        assert!(AuthConfig::builder("cid").authority_host("http://evil.example").build().is_err());
        assert!(AuthConfig::builder("cid").authority_host("not a url").build().is_err());
        assert!(AuthConfig::builder("cid").scopes(Vec::<String>::new()).build().is_err());
        assert!(AuthConfig::builder("cid").scopes(["a b"]).build().is_err());
        let err = AuthConfig::builder("").build().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Config);
        assert!(AuthConfig::builder("cid").authority_host("http://127.0.0.1:9999").build().is_ok());
    }
}
