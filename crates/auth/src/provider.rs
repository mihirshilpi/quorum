//! [`TokenProvider`]: cached access tokens with single-flight silent refresh.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use jiff::SignedDuration;

use crate::account::{Account, GrantedScopes};
use crate::authenticator::Shared;
use crate::claims::ClaimsChallenge;
use crate::error::{Error, ErrorKind, Result};
use crate::secret::AccessToken;
use crate::store;

/// Refresh this long before the access token expires.
pub const REFRESH_MARGIN: SignedDuration = SignedDuration::from_mins(5);

/// Hands out valid access tokens for one signed-in account.
///
/// Cheap to clone (`Arc` inside, M-SERVICES-CLONE); clones share the cache. Access tokens live
/// only in memory; the refresh token is read from and written to the
/// [`crate::SecretStore`] on each refresh.
#[derive(Clone)]
pub struct TokenProvider {
    inner: Arc<Inner>,
}

struct Inner {
    shared: Arc<Shared>,
    account: Account,
    scopes: Mutex<GrantedScopes>,
    /// Held across a refresh, so concurrent callers share one request (single-flight).
    state: tokio::sync::Mutex<State>,
}

#[derive(Default)]
struct State {
    cached: Option<AccessToken>,
    /// Set once refresh needs the user; later calls fail fast without the network.
    blocked: Option<Option<ClaimsChallenge>>,
}

impl fmt::Debug for TokenProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenProvider").field("account", &self.inner.account).finish_non_exhaustive()
    }
}

impl TokenProvider {
    pub(crate) fn new(
        shared: Arc<Shared>,
        account: Account,
        scopes: GrantedScopes,
        access_token: Option<AccessToken>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                shared,
                account,
                scopes: Mutex::new(scopes),
                state: tokio::sync::Mutex::new(State { cached: access_token, blocked: None }),
            }),
        }
    }

    /// The account this provider signs requests for.
    #[must_use]
    pub fn account(&self) -> &Account {
        &self.inner.account
    }

    /// The scopes granted with the most recent token.
    #[must_use]
    pub fn granted_scopes(&self) -> GrantedScopes {
        self.inner.scopes.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// The scopes from `required` that the current grant lacks (see
    /// [`GrantedScopes::missing_scopes`]). Features whose scopes are missing are disabled.
    #[must_use]
    pub fn missing_scopes(&self, required: &[&str]) -> Vec<String> {
        self.granted_scopes().missing_scopes(required)
    }

    /// Returns an access token valid for at least [`REFRESH_MARGIN`], refreshing it silently
    /// when needed. Concurrent callers share a single refresh.
    ///
    /// When a refresh fails with a transient error but the cached token has not expired yet,
    /// the cached token is returned.
    ///
    /// # Errors
    /// - [`ErrorKind::InteractionRequired`] when the refresh token is missing, expired or
    ///   revoked, or Conditional Access wants the user (see [`Error::claims`]). Pause sync and
    ///   call [`crate::Authenticator::sign_in`] again.
    /// - [`ErrorKind::Network`] when the token endpoint cannot be reached.
    /// - [`ErrorKind::Store`] when the keychain cannot be read.
    /// - [`ErrorKind::InvalidResponse`] for an unusable token response.
    pub async fn access_token(&self) -> Result<AccessToken> {
        let mut state = self.inner.state.lock().await;
        let now = self.inner.shared.clock.now();
        if let Some(claims) = &state.blocked {
            return Err(Error::new(ErrorKind::InteractionRequired, "sign-in required").with_claims(claims.clone()));
        }
        if let Some(t) = &state.cached
            && now.checked_add(REFRESH_MARGIN).is_ok_and(|soon| soon < t.expires_at())
        {
            return Ok(t.clone());
        }

        match self.refresh(now).await {
            Ok(token) => {
                state.cached = Some(token.clone());
                Ok(token)
            }
            Err(e) if e.kind() == ErrorKind::InteractionRequired => {
                tracing::warn!(name: "auth.refresh.interaction_required", oauth_error = e.oauth_error(), "refresh needs sign-in");
                state.cached = None;
                state.blocked = Some(e.claims().cloned());
                Err(e)
            }
            Err(e) => {
                tracing::warn!(name: "auth.refresh.failed", kind = ?e.kind(), "token refresh failed");
                match &state.cached {
                    Some(t) if now < t.expires_at() && e.kind() == ErrorKind::Network => Ok(t.clone()),
                    _ => Err(e),
                }
            }
        }
    }

    /// Drops `rejected` from the cache (call after Graph answers `401` with it), so the next
    /// [`TokenProvider::access_token`] refreshes. A newer cached token is kept, so many
    /// requests failing with the same token cause one refresh.
    pub async fn invalidate(&self, rejected: &AccessToken) {
        let mut state = self.inner.state.lock().await;
        if state.cached.as_ref().is_some_and(|c| c.secret() == rejected.secret()) {
            state.cached = None;
        }
    }

    async fn refresh(&self, now: jiff::Timestamp) -> Result<AccessToken> {
        let shared = &self.inner.shared;
        let account_id = self.inner.account.id();
        let Some((_, refresh_token, _)) = store::load(shared.store.as_ref(), account_id)? else {
            return Err(Error::new(ErrorKind::InteractionRequired, "no stored refresh token"));
        };
        tracing::debug!(name: "auth.refresh.start", "refreshing access token");
        let set = shared.oauth.refresh(&refresh_token, now).await?;
        if let Some(claims) = &set.id_claims
            && let Ok(acct) = claims.account()
            && acct.id() != account_id
        {
            return Err(Error::new(ErrorKind::InvalidResponse, "refresh returned a different account"));
        }
        if !set.scopes.as_slice().is_empty() {
            *self.inner.scopes.lock().unwrap_or_else(PoisonError::into_inner) = set.scopes.clone();
        }
        // Entra rotates refresh tokens; keep the newest. A failed write is logged, not fatal:
        // the previous refresh token stays valid.
        let rt = set.refresh_token.as_ref().unwrap_or(&refresh_token);
        if let Err(e) = store::save(shared.store.as_ref(), &self.inner.account, rt, &self.granted_scopes()) {
            tracing::warn!(name: "auth.store.save_failed", kind = ?e.kind(), "could not save refreshed credential");
        }
        let expires_in = set.access_token.expires_at().duration_since(now).as_secs();
        tracing::info!(name: "auth.refresh.done", expires_in_secs = expires_in, "access token refreshed");
        Ok(set.access_token)
    }
}
