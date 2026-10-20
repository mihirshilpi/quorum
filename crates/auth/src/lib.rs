//! Entra ID sign-in with authorization code + PKCE and keychain token storage.
//!
//! Quorum is a public client (no secret). [`Authenticator::sign_in`] opens the system browser
//! and receives the redirect on a loopback port (RFC 8252); [`Authenticator::start_device_code`]
//! and [`Authenticator::finish_device_code`] serve `quorum-cli login --device`. Both return a
//! [`TokenProvider`], whose [`TokenProvider::access_token`] refreshes silently five minutes
//! before expiry, with concurrent callers sharing one refresh.
//!
//! Refresh tokens live only in a [`SecretStore`] ([`KeyringStore`] in production); access
//! tokens only in memory. Every secret type redacts itself in `Debug`/`Display`, and logs
//! (`tracing`, events named `auth.*`) carry no tokens or user names.
//!
//! When Graph answers `401` with a Conditional Access challenge, parse it with
//! [`ClaimsChallenge::from_www_authenticate`] and pass it to the next sign-in via
//! [`SignInOptions::claims`]. A refresh that needs the user fails with
//! [`ErrorKind::InteractionRequired`] (and [`Error::claims`] when Entra sent claims).
//!
//! The HTTP transport, keychain and clock are traits ([`Transport`], [`SecretStore`],
//! [`Clock`]); the `test-util` feature adds `FakeTransport`, `MemoryStore` and `FakeClock`.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable,
    clippy::indexing_slicing
)]
#![warn(missing_docs)]

mod account;
mod authenticator;
mod browser;
mod claims;
mod clock;
mod config;
mod error;
mod loopback;
mod oauth;
mod pkce;
mod provider;
mod secret;
mod store;
mod transport;

use std::future::Future;
use std::pin::Pin;

pub use account::{Account, GrantedScopes};
pub use authenticator::{Authenticator, AuthenticatorBuilder, DEFAULT_SIGN_IN_TIMEOUT, Prompt, SignInOptions};
pub use browser::{BrowserOpener, SystemBrowser};
pub use claims::ClaimsChallenge;
#[cfg(any(test, feature = "test-util"))]
pub use clock::FakeClock;
pub use clock::{Clock, SystemClock};
pub use config::{AuthConfig, AuthConfigBuilder, DEFAULT_AUTHORITY_HOST, DEFAULT_SCOPES};
pub use error::{Error, ErrorKind, Result, aadsts_help};
pub use oauth::DeviceCode;
pub use provider::{REFRESH_MARGIN, TokenProvider};
pub use secret::{AccessToken, AuthorizationCode, DeviceCodeSecret, PkceVerifier, RefreshToken};
#[cfg(any(test, feature = "test-util"))]
pub use store::MemoryStore;
pub use store::{KEYRING_SERVICE, KeyringStore, SecretStore};
#[cfg(any(test, feature = "test-util"))]
pub use transport::FakeTransport;
pub use transport::{FormRequest, FormResponse, MAX_RESPONSE_BYTES, ReqwestTransport, Transport};

/// A boxed, `Send` future, used at the trait seams so they stay object-safe.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
