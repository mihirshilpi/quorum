//! The signed-in account: configuration, sign-in state and the Graph client it feeds.
//!
//! The Graph [`Client`] is built once per session. Its [`TokenSource`] reads whichever
//! [`TokenProvider`] is current, so signing in or out never rebuilds the client. In mock mode
//! the client runs on recorded fixtures with a static token and sign-in is simulated.

use std::path::PathBuf;
use std::sync::Arc;

use quorum_auth::{AuthConfig, Authenticator, TokenProvider};
use quorum_graph::{Client, TokenSource};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::error::{Error, ErrorKind, Result};

/// Every delegated scope Quorum may use (`plan/graph-permissions.md`).
pub const REQUIRED_SCOPES: &[&str] = &[
    "User.Read",
    "User.ReadBasic.All",
    "User.Read.All",
    "People.Read",
    "GroupMember.Read.All",
    "Group.ReadWrite.All",
    "Contacts.ReadWrite",
    "Mail.ReadWrite",
    "Mail.Send",
    "Mail.ReadWrite.Shared",
    "Mail.Send.Shared",
    "MailboxSettings.ReadWrite",
    "Calendars.ReadWrite",
    "Calendars.ReadWrite.Shared",
    "Place.Read.All",
    "OnlineMeetings.ReadWrite",
    "Presence.Read.All",
    "Presence.ReadWrite",
    "Chat.ReadWrite",
    "ChatMessage.Send",
    "Chat.Create",
    "Team.ReadBasic.All",
    "Channel.ReadBasic.All",
    "ChannelMessage.Read.All",
    "ChannelMessage.Send",
    "Files.ReadWrite.All",
    "Sites.ReadWrite.All",
    "Tasks.ReadWrite",
];

/// Tenant and app registration Quorum signs in with.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccountConfig {
    /// Directory (tenant) id, or `organizations`.
    pub tenant: String,
    /// Application (client) id of the Entra app registration.
    pub client_id: String,
}

impl AccountConfig {
    /// Reads `QUORUM_TENANT` / `QUORUM_CLIENT_ID`, else the config file, else `None`.
    pub fn load() -> Option<Self> {
        if let (Ok(tenant), Ok(client_id)) = (std::env::var("QUORUM_TENANT"), std::env::var("QUORUM_CLIENT_ID")) {
            return Some(Self { tenant, client_id });
        }
        let text = std::fs::read_to_string(config_path()).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Writes the config file.
    ///
    /// # Errors
    /// The file could not be written.
    pub fn save(&self) -> Result<()> {
        let path = config_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| Error::new(ErrorKind::Store, "cannot create the settings folder").with_source(e))?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, text)
            .map_err(|e| Error::new(ErrorKind::Store, "cannot save account settings").with_source(e))
    }
}

/// Quorum's per-user data folder: `~/Library/Application Support/Quorum` (macOS),
/// `%APPDATA%\Quorum` (Windows), `$XDG_DATA_HOME/quorum` or `~/.local/share/quorum` (Linux).
pub fn data_dir() -> PathBuf {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    data_dir_from(var("HOME"), var("APPDATA"), var("XDG_DATA_HOME"))
}

fn data_dir_from(home: Option<PathBuf>, appdata: Option<PathBuf>, xdg: Option<PathBuf>) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.unwrap_or_default().join("Library/Application Support/Quorum")
    } else if cfg!(target_os = "windows") {
        appdata.or_else(|| home.map(|h| h.join("AppData").join("Roaming"))).unwrap_or_default().join("Quorum")
    } else {
        xdg.filter(|p| p.is_absolute()).unwrap_or_else(|| home.unwrap_or_default().join(".local/share")).join("quorum")
    }
}

fn config_path() -> PathBuf {
    data_dir().join("account.json")
}

/// Shared slot holding the current token provider.
type Slot = Arc<RwLock<Option<TokenProvider>>>;

/// Adapts the current [`TokenProvider`] to the Graph client's [`TokenSource`].
#[derive(Clone)]
struct ProviderTokens(Slot);

impl TokenSource for ProviderTokens {
    async fn token(&self) -> quorum_graph::Result<String> {
        let provider = self.0.read().await.clone();
        let unauthorized = |m: &str| quorum_graph::Error::new(quorum_graph::ErrorKind::Unauthorized, m.to_owned());
        let provider = provider.ok_or_else(|| unauthorized("not signed in"))?;
        provider.access_token().await.map(|t| t.secret().to_owned()).map_err(|_| unauthorized("sign-in required"))
    }
}

/// Sign-in state of the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountState {
    /// No tenant or client id configured yet.
    NotConfigured,
    /// Configured but not signed in.
    SignedOut,
    /// Signed in.
    SignedIn,
}

/// The account part of a session.
pub(crate) struct Accounts {
    slot: Slot,
    graph: Client,
    mock: bool,
    mock_signed_in: std::sync::atomic::AtomicBool,
}

impl Accounts {
    pub(crate) fn new(mock: bool) -> Result<Self> {
        let slot: Slot = Arc::new(RwLock::new(None));
        let graph = if mock {
            let transport = quorum_graph::FixtureTransport::from_default_manifest()
                .map_err(|e| Error::new(ErrorKind::NotAvailable, "mock Graph fixtures are missing").with_source(e))?;
            Client::builder(quorum_graph::StaticToken::new("mock-token")).transport(transport).build()
        } else {
            Client::builder(ProviderTokens(slot.clone())).build()
        }
        .map_err(|e| Error::new(ErrorKind::Bug, "cannot build the Graph client").with_source(e))?;
        let accounts = Self { slot, graph, mock, mock_signed_in: std::sync::atomic::AtomicBool::new(mock) };
        Ok(accounts)
    }

    pub(crate) fn graph(&self) -> &Client {
        &self.graph
    }

    fn authenticator() -> Result<Authenticator> {
        let cfg = AccountConfig::load().ok_or_else(|| {
            Error::new(ErrorKind::NotAvailable, "set the tenant and client id first (account.configure)")
        })?;
        let cfg = AuthConfig::builder(cfg.client_id)
            .tenant(cfg.tenant)
            .build()
            .map_err(|e| Error::new(ErrorKind::InvalidParams, "tenant or client id is not valid").with_source(e))?;
        Authenticator::new(cfg).map_err(auth_err)
    }

    /// Restores the first stored account without touching the network.
    pub(crate) async fn restore(&self) -> Result<bool> {
        if self.mock {
            return Ok(true);
        }
        let Ok(auth) = Self::authenticator() else { return Ok(false) };
        let accounts = auth.accounts().map_err(auth_err)?;
        let Some(first) = accounts.first() else { return Ok(false) };
        let provider = auth.restore(first.id()).map_err(auth_err)?;
        let found = provider.is_some();
        *self.slot.write().await = provider;
        Ok(found)
    }

    pub(crate) async fn sign_in(&self) -> Result<()> {
        if self.mock {
            self.mock_signed_in.store(true, std::sync::atomic::Ordering::Relaxed);
            return Ok(());
        }
        let auth = Self::authenticator()?;
        let provider =
            auth.sign_in(&quorum_auth::SystemBrowser, quorum_auth::SignInOptions::default()).await.map_err(auth_err)?;
        *self.slot.write().await = Some(provider);
        Ok(())
    }

    pub(crate) async fn sign_out(&self) -> Result<()> {
        if self.mock {
            self.mock_signed_in.store(false, std::sync::atomic::Ordering::Relaxed);
            return Ok(());
        }
        let provider = self.slot.write().await.take();
        if let Some(p) = provider {
            Self::authenticator()?.sign_out(p.account().id()).map_err(auth_err)?;
        }
        Ok(())
    }

    pub(crate) async fn state(&self) -> AccountState {
        if self.mock {
            return if self.mock_signed_in.load(std::sync::atomic::Ordering::Relaxed) {
                AccountState::SignedIn
            } else {
                AccountState::SignedOut
            };
        }
        if self.slot.read().await.is_some() {
            AccountState::SignedIn
        } else if AccountConfig::load().is_some() {
            AccountState::SignedOut
        } else {
            AccountState::NotConfigured
        }
    }

    pub(crate) async fn provider(&self) -> Option<TokenProvider> {
        self.slot.read().await.clone()
    }

    pub(crate) fn is_mock(&self) -> bool {
        self.mock
    }
}

fn auth_err(e: quorum_auth::Error) -> Error {
    let kind = match e.kind() {
        quorum_auth::ErrorKind::Network => ErrorKind::Remote,
        quorum_auth::ErrorKind::Config => ErrorKind::InvalidParams,
        _ => ErrorKind::Auth,
    };
    let message = match (e.help(), e.aadsts()) {
        (Some(help), Some(n)) => format!("{help} (AADSTS{n})"),
        (_, Some(n)) => format!("Sign-in failed: {} (AADSTS{n}).", e.kind()),
        _ if e.kind() == quorum_auth::ErrorKind::Cancelled => "Sign-in was cancelled.".to_owned(),
        _ if e.kind() == quorum_auth::ErrorKind::Timeout => "Sign-in timed out. Try again.".to_owned(),
        _ if e.kind() == quorum_auth::ErrorKind::Network => {
            "Couldn't reach Microsoft to sign in. Check your connection and try again.".to_owned()
        }
        _ => format!("Sign-in failed: {}.", e.kind()),
    };
    Error::new(kind, message).with_source(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_follows_platform_conventions_and_never_lands_in_the_working_folder() {
        let home = Some(PathBuf::from("/home/u"));
        let dir = data_dir_from(home.clone(), Some(PathBuf::from("/appdata")), Some(PathBuf::from("/xdg")));
        let fallback = data_dir_from(home, None, Some(PathBuf::from("relative")));
        if cfg!(target_os = "macos") {
            assert_eq!(dir, PathBuf::from("/home/u/Library/Application Support/Quorum"));
        } else if cfg!(target_os = "windows") {
            assert_eq!(dir, PathBuf::from("/appdata").join("Quorum"));
            assert_eq!(fallback, PathBuf::from("/home/u").join("AppData").join("Roaming").join("Quorum"));
        } else {
            assert_eq!(dir, PathBuf::from("/xdg/quorum"));
            assert_eq!(fallback, PathBuf::from("/home/u/.local/share/quorum"));
        }
    }
}
