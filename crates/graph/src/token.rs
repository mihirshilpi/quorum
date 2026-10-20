//! Access tokens come from a [`TokenSource`] so `quorum-auth` can plug in.

use std::fmt;
use std::future::Future;

use futures::future::BoxFuture;

use crate::error::Result;

/// Supplies a current Graph access token for each request attempt.
///
/// `quorum-auth` implements this with silent refresh; tests use [`StaticToken`]. The returned
/// string is sent as `Authorization: Bearer <token>` and never logged.
pub trait TokenSource: Send + Sync + 'static {
    /// Returns a valid access token.
    ///
    /// # Errors
    /// Usually [`ErrorKind::Unauthorized`](crate::ErrorKind::Unauthorized) when the user must
    /// sign in again.
    fn token(&self) -> impl Future<Output = Result<String>> + Send;
}

/// Object-safe form of [`TokenSource`].
pub(crate) trait DynTokenSource: Send + Sync {
    fn token_dyn(&self) -> BoxFuture<'_, Result<String>>;
}

impl<T: TokenSource> DynTokenSource for T {
    fn token_dyn(&self) -> BoxFuture<'_, Result<String>> {
        Box::pin(self.token())
    }
}

/// A fixed token, for tests and tools. `Debug` redacts it.
#[derive(Clone)]
pub struct StaticToken(String);

impl StaticToken {
    /// Wraps `token`.
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }
}

impl fmt::Debug for StaticToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StaticToken(<redacted>)")
    }
}

impl TokenSource for StaticToken {
    async fn token(&self) -> Result<String> {
        Ok(self.0.clone())
    }
}
