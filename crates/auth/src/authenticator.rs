//! [`Authenticator`]: interactive and device-code sign-in, restore and sign-out.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::account::Account;
use crate::browser::BrowserOpener;
use crate::claims::ClaimsChallenge;
use crate::clock::{Clock, SystemClock};
use crate::config::AuthConfig;
use crate::error::{Error, ErrorKind, Result};
use crate::loopback::LoopbackListener;
use crate::oauth::{DeviceCode, OAuth, Poll, TokenSet};
use crate::pkce::random_urlsafe;
use crate::provider::TokenProvider;
use crate::secret::PkceVerifier;
use crate::store::{self, KeyringStore, SecretStore};
use crate::transport::{ReqwestTransport, Transport};

/// Default time the user has to finish signing in in the browser.
pub const DEFAULT_SIGN_IN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Upper bound on device-code polls, whatever the server's interval says.
const MAX_DEVICE_POLLS: u32 = 2_000;

/// State shared by the authenticator and every provider it creates.
#[derive(Debug)]
pub(crate) struct Shared {
    pub oauth: OAuth,
    pub store: Arc<dyn SecretStore>,
    pub clock: Arc<dyn Clock>,
}

/// Signs accounts in and out and hands out [`TokenProvider`]s.
///
/// Cheap to clone (M-SERVICES-CLONE).
///
/// ```no_run
/// # async fn demo() -> quorum_auth::Result<()> {
/// use quorum_auth::{AuthConfig, Authenticator, SignInOptions, SystemBrowser};
/// let config = AuthConfig::builder("<client-id>").tenant("<tenant-id>").build()?;
/// let auth = Authenticator::new(config)?;
/// let provider = match auth.accounts()?.first() {
///     Some(a) => auth.restore(a.id())?.ok_or(quorum_auth::ErrorKind::InteractionRequired)?,
///     None => auth.sign_in(&SystemBrowser, SignInOptions::default()).await?,
/// };
/// let token = provider.access_token().await?;
/// # let _ = token; Ok(()) }
/// ```
#[derive(Clone)]
pub struct Authenticator {
    shared: Arc<Shared>,
}

impl fmt::Debug for Authenticator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Authenticator").field("shared", &self.shared).finish()
    }
}

/// Builder for [`Authenticator`], for swapping the keychain, HTTP transport or clock.
#[derive(Debug)]
pub struct AuthenticatorBuilder {
    config: AuthConfig,
    store: Option<Arc<dyn SecretStore>>,
    transport: Option<Arc<dyn Transport>>,
    clock: Option<Arc<dyn Clock>>,
}

impl AuthenticatorBuilder {
    /// Uses `store` instead of the OS keychain ([`KeyringStore`]).
    #[must_use]
    pub fn store(mut self, store: impl SecretStore) -> Self {
        self.store = Some(Arc::new(store));
        self
    }

    /// Uses `transport` instead of [`ReqwestTransport`].
    #[must_use]
    pub fn transport(mut self, transport: impl Transport) -> Self {
        self.transport = Some(Arc::new(transport));
        self
    }

    /// Uses `clock` instead of [`SystemClock`].
    #[must_use]
    pub fn clock(mut self, clock: impl Clock) -> Self {
        self.clock = Some(Arc::new(clock));
        self
    }

    /// Builds the authenticator.
    ///
    /// # Errors
    /// [`ErrorKind::Network`] when the default HTTP client cannot be created.
    pub fn build(self) -> Result<Authenticator> {
        let transport: Arc<dyn Transport> = match self.transport {
            Some(t) => t,
            None => Arc::new(ReqwestTransport::new()?),
        };
        Ok(Authenticator {
            shared: Arc::new(Shared {
                oauth: OAuth::new(self.config, transport),
                store: self.store.unwrap_or_else(|| Arc::new(KeyringStore::new())),
                clock: self.clock.unwrap_or_else(|| Arc::new(SystemClock)),
            }),
        })
    }
}

/// The OpenID Connect `prompt` parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Prompt {
    /// Show the account picker.
    SelectAccount,
    /// Force credentials to be entered again.
    Login,
    /// Show the consent dialog.
    Consent,
}

impl Prompt {
    fn as_str(self) -> &'static str {
        match self {
            Self::SelectAccount => "select_account",
            Self::Login => "login",
            Self::Consent => "consent",
        }
    }
}

/// Options for [`Authenticator::sign_in`].
#[derive(Debug, Clone)]
pub struct SignInOptions {
    claims: Option<ClaimsChallenge>,
    login_hint: Option<String>,
    prompt: Option<Prompt>,
    timeout: Duration,
}

impl Default for SignInOptions {
    fn default() -> Self {
        Self { claims: None, login_hint: None, prompt: None, timeout: DEFAULT_SIGN_IN_TIMEOUT }
    }
}

impl SignInOptions {
    /// Passes a Conditional Access claims challenge (from a Graph `401` or a failed refresh).
    #[must_use]
    pub fn claims(mut self, claims: Option<ClaimsChallenge>) -> Self {
        self.claims = claims;
        self
    }

    /// Pre-fills the user name (e.g. [`Account::username`] when re-authenticating).
    #[must_use]
    pub fn login_hint(mut self, hint: impl Into<String>) -> Self {
        self.login_hint = Some(hint.into());
        self
    }

    /// Sets the `prompt` parameter.
    #[must_use]
    pub fn prompt(mut self, prompt: Prompt) -> Self {
        self.prompt = Some(prompt);
        self
    }

    /// How long to wait for the browser redirect (default [`DEFAULT_SIGN_IN_TIMEOUT`]).
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Authenticator {
    /// An authenticator with the OS keychain, `reqwest` and the system clock.
    ///
    /// # Errors
    /// [`ErrorKind::Network`] when the HTTP client cannot be created.
    pub fn new(config: AuthConfig) -> Result<Self> {
        Self::builder(config).build()
    }

    /// Starts a builder to replace the keychain, transport or clock.
    #[must_use]
    pub fn builder(config: AuthConfig) -> AuthenticatorBuilder {
        AuthenticatorBuilder { config, store: None, transport: None, clock: None }
    }

    /// The configuration in use.
    #[must_use]
    pub fn config(&self) -> &AuthConfig {
        &self.shared.oauth.config
    }

    /// Interactive sign-in: opens the system browser on the Entra authorize page
    /// (authorization code + PKCE S256, `state`, `nonce`), receives the redirect on
    /// `http://localhost:<ephemeral port>`, exchanges the code, checks the ID token nonce,
    /// stores the refresh token and returns a provider.
    ///
    /// # Errors
    /// [`ErrorKind::Timeout`] if no redirect arrives within the timeout;
    /// [`ErrorKind::StateMismatch`] on a forged redirect or nonce; [`ErrorKind::Cancelled`]
    /// when the user declines; [`ErrorKind::Network`]/[`ErrorKind::InvalidResponse`] from the
    /// token exchange; [`ErrorKind::Store`] if the keychain write fails; any error from
    /// `browser`.
    pub async fn sign_in(&self, browser: &dyn BrowserOpener, options: SignInOptions) -> Result<TokenProvider> {
        let verifier = PkceVerifier::generate();
        let state = random_urlsafe(32);
        let nonce = random_urlsafe(32);
        let listener = LoopbackListener::bind().await?;
        let redirect_uri = listener.redirect_uri();

        let mut url = self.shared.oauth.config.authorize_endpoint()?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("client_id", self.config().client_id())
                .append_pair("response_type", "code")
                .append_pair("redirect_uri", &redirect_uri)
                .append_pair("response_mode", "query")
                .append_pair("scope", &self.config().scope_param())
                .append_pair("state", &state)
                .append_pair("nonce", &nonce)
                .append_pair("code_challenge", &verifier.challenge_s256())
                .append_pair("code_challenge_method", "S256");
            if let Some(c) = &options.claims {
                q.append_pair("claims", c.as_json());
            }
            if let Some(h) = &options.login_hint {
                q.append_pair("login_hint", h);
            }
            if let Some(p) = options.prompt {
                q.append_pair("prompt", p.as_str());
            }
        }

        tracing::info!(name: "auth.sign_in.start", claims = options.claims.is_some(), "opening browser for sign-in");
        browser.open(&url)?;
        let code = listener.wait(&state, options.timeout).await?;

        let now = self.shared.clock.now();
        let set = self.shared.oauth.exchange_code(&code, &verifier, &redirect_uri, now).await?;
        let claims = set
            .id_claims
            .as_ref()
            .ok_or_else(|| Error::new(ErrorKind::InvalidResponse, "token response has no ID token"))?;
        claims.check_nonce(&nonce)?;
        let provider = self.finish(set)?;
        tracing::info!(name: "auth.sign_in.done", "interactive sign-in complete");
        Ok(provider)
    }

    /// Starts a device-code sign-in (for `quorum-cli login --device`). Show
    /// [`DeviceCode::message`] to the user, then await [`Authenticator::finish_device_code`].
    ///
    /// # Errors
    /// [`ErrorKind::Network`], [`ErrorKind::InvalidResponse`] or [`ErrorKind::Config`] (e.g.
    /// public client flows not enabled on the app registration).
    pub async fn start_device_code(&self) -> Result<DeviceCode> {
        let code = self.shared.oauth.start_device_code(self.shared.clock.now()).await?;
        tracing::info!(name: "auth.device_code.start", "device code issued");
        Ok(code)
    }

    /// Polls until the user completes the device-code sign-in, honouring the server's
    /// interval and `slow_down` (each adds 5 s).
    ///
    /// # Errors
    /// [`ErrorKind::Timeout`] when the code expires; [`ErrorKind::Cancelled`] when the user
    /// declines; otherwise as for [`Authenticator::sign_in`].
    pub async fn finish_device_code(&self, code: &DeviceCode) -> Result<TokenProvider> {
        let mut interval = code.interval;
        for _ in 0..MAX_DEVICE_POLLS {
            self.shared.clock.sleep(interval).await;
            let now = self.shared.clock.now();
            if now >= code.expires_at() {
                break;
            }
            match self.shared.oauth.poll_device_code(code, now).await? {
                Poll::Pending => {}
                Poll::SlowDown => {
                    interval = interval.saturating_add(Duration::from_secs(5)).min(Duration::from_secs(120));
                }
                Poll::Done(set) => {
                    let provider = self.finish(*set)?;
                    tracing::info!(name: "auth.device_code.done", "device code sign-in complete");
                    return Ok(provider);
                }
            }
        }
        Err(Error::new(ErrorKind::Timeout, "device code expired before sign-in completed"))
    }

    /// Recreates the provider for a previously signed-in account from the keychain, without
    /// network access. The first [`TokenProvider::access_token`] call refreshes.
    ///
    /// Returns `Ok(None)` when nothing is stored for `account_id`.
    ///
    /// # Errors
    /// [`ErrorKind::Store`] when the keychain cannot be read or the item is corrupt.
    pub fn restore(&self, account_id: &str) -> Result<Option<TokenProvider>> {
        Ok(store::load(self.shared.store.as_ref(), account_id)?
            .map(|(account, _, scopes)| TokenProvider::new(self.shared.clone(), account, scopes, None)))
    }

    /// The accounts with a stored refresh token, in sign-in order.
    ///
    /// # Errors
    /// [`ErrorKind::Store`] when the keychain cannot be read.
    pub fn accounts(&self) -> Result<Vec<Account>> {
        let store = self.shared.store.as_ref();
        let mut out = Vec::new();
        for id in store::index(store)? {
            match store::load(store, &id) {
                Ok(Some((account, _, _))) => out.push(account),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(name: "auth.store.corrupt_item", kind = ?e.kind(), "skipping unreadable account")
                }
            }
        }
        Ok(out)
    }

    /// Signs `account_id` out locally: deletes its refresh token from the keychain. Existing
    /// providers keep their in-memory access token until it expires, then need sign-in.
    ///
    /// # Errors
    /// [`ErrorKind::Store`] when the keychain cannot be written.
    pub fn sign_out(&self, account_id: &str) -> Result<()> {
        store::remove(self.shared.store.as_ref(), account_id)?;
        tracing::info!(name: "auth.sign_out", "account signed out");
        Ok(())
    }

    fn finish(&self, set: TokenSet) -> Result<TokenProvider> {
        let account = set
            .id_claims
            .as_ref()
            .ok_or_else(|| Error::new(ErrorKind::InvalidResponse, "token response has no ID token"))?
            .account()?;
        match &set.refresh_token {
            Some(rt) => store::save(self.shared.store.as_ref(), &account, rt, &set.scopes)?,
            None => tracing::warn!(name: "auth.sign_in.no_refresh_token", "no refresh token; offline_access missing?"),
        }
        Ok(TokenProvider::new(self.shared.clone(), account, set.scopes, Some(set.access_token)))
    }
}
