//! Refresh-token storage: the OS credential store, behind [`SecretStore`].

use std::fmt::Debug;

use serde::{Deserialize, Serialize};

use crate::account::{Account, GrantedScopes};
use crate::error::{Error, ErrorKind, Result};
use crate::secret::RefreshToken;

/// Keychain service name for every Quorum item.
pub const KEYRING_SERVICE: &str = "dev.quorum.app";

/// Longest stored item we accept back from the store.
const MAX_ITEM_LEN: usize = 64 * 1024;

/// A secret store keyed by account id (M-MOCKABLE-SYSCALLS).
///
/// Values are opaque strings; `quorum-auth` stores a small JSON record holding the refresh
/// token and the account profile. Implementations must be safe to call from async code;
/// calls are short and synchronous.
pub trait SecretStore: Debug + Send + Sync + 'static {
    /// Reads the value for `key`; `Ok(None)` when there is none.
    ///
    /// # Errors
    /// [`ErrorKind::Store`] when the store cannot be read.
    fn get(&self, key: &str) -> Result<Option<String>>;

    /// Writes (replaces) the value for `key`.
    ///
    /// # Errors
    /// [`ErrorKind::Store`] when the store cannot be written.
    fn set(&self, key: &str, value: &str) -> Result<()>;

    /// Deletes the value for `key`; deleting a missing key succeeds.
    ///
    /// # Errors
    /// [`ErrorKind::Store`] when the store cannot be written.
    fn delete(&self, key: &str) -> Result<()>;
}

/// The OS credential store via `keyring` (macOS Keychain, Windows Credential Manager,
/// Secret Service on Linux), service [`KEYRING_SERVICE`], one item per account.
#[derive(Debug, Clone)]
pub struct KeyringStore {
    service: String,
}

impl KeyringStore {
    /// A store under [`KEYRING_SERVICE`].
    #[must_use]
    pub fn new() -> Self {
        Self { service: KEYRING_SERVICE.to_owned() }
    }

    /// A store under another service name (e.g. a separate dev profile).
    #[must_use]
    pub fn with_service(service: impl Into<String>) -> Self {
        Self { service: service.into() }
    }

    fn entry(&self, key: &str) -> Result<keyring::Entry> {
        Ok(keyring::Entry::new(&self.service, key)?)
    }
}

impl Default for KeyringStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretStore for KeyringStore {
    fn get(&self, key: &str) -> Result<Option<String>> {
        match self.entry(key)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        Ok(self.entry(key)?.set_password(value)?)
    }

    fn delete(&self, key: &str) -> Result<()> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(any(test, feature = "test-util"))]
pub use memory::MemoryStore;

#[cfg(any(test, feature = "test-util"))]
mod memory {
    use std::collections::HashMap;
    use std::fmt;
    use std::sync::{Arc, Mutex, PoisonError};

    use super::SecretStore;
    use crate::error::{Error, ErrorKind, Result};

    /// An in-memory [`SecretStore`] for tests. Clones share contents.
    #[derive(Clone, Default)]
    pub struct MemoryStore {
        items: Arc<Mutex<HashMap<String, String>>>,
        fail: Arc<Mutex<bool>>,
    }

    impl MemoryStore {
        /// An empty store.
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }

        /// Makes every subsequent call fail with [`ErrorKind::Store`] (or succeed again).
        pub fn set_failing(&self, fail: bool) {
            *self.fail.lock().unwrap_or_else(PoisonError::into_inner) = fail;
        }

        /// The stored keys, sorted.
        #[must_use]
        pub fn keys(&self) -> Vec<String> {
            let mut k: Vec<String> =
                self.items.lock().unwrap_or_else(PoisonError::into_inner).keys().cloned().collect();
            k.sort();
            k
        }

        /// The raw stored value for `key` (for leak assertions in tests).
        #[must_use]
        pub fn raw(&self, key: &str) -> Option<String> {
            self.items.lock().unwrap_or_else(PoisonError::into_inner).get(key).cloned()
        }

        fn check(&self) -> Result<()> {
            if *self.fail.lock().unwrap_or_else(PoisonError::into_inner) {
                Err(Error::new(ErrorKind::Store, "fake store failure"))
            } else {
                Ok(())
            }
        }
    }

    impl fmt::Debug for MemoryStore {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("MemoryStore").field("keys", &self.keys().len()).finish_non_exhaustive()
        }
    }

    impl SecretStore for MemoryStore {
        fn get(&self, key: &str) -> Result<Option<String>> {
            self.check()?;
            Ok(self.items.lock().unwrap_or_else(PoisonError::into_inner).get(key).cloned())
        }

        fn set(&self, key: &str, value: &str) -> Result<()> {
            self.check()?;
            self.items.lock().unwrap_or_else(PoisonError::into_inner).insert(key.to_owned(), value.to_owned());
            Ok(())
        }

        fn delete(&self, key: &str) -> Result<()> {
            self.check()?;
            self.items.lock().unwrap_or_else(PoisonError::into_inner).remove(key);
            Ok(())
        }
    }
}

/// Key of the item listing stored account ids (no secrets in it).
pub(crate) const INDEX_KEY: &str = "accounts";

/// The record stored per account.
#[derive(Serialize, Deserialize)]
pub(crate) struct StoredAccount {
    pub account: Account,
    pub refresh_token: String,
    #[serde(default)]
    pub scopes: GrantedScopes,
}

pub(crate) fn account_key(account_id: &str) -> String {
    format!("account:{account_id}")
}

pub(crate) fn load(
    store: &dyn SecretStore,
    account_id: &str,
) -> Result<Option<(Account, RefreshToken, GrantedScopes)>> {
    let Some(raw) = store.get(&account_key(account_id))? else {
        return Ok(None);
    };
    if raw.len() > MAX_ITEM_LEN {
        return Err(Error::new(ErrorKind::Store, "stored credential is too large"));
    }
    let rec: StoredAccount = serde_json::from_str(&raw)
        .map_err(|e| Error::new(ErrorKind::Store, "stored credential is corrupt").with_source(e))?;
    Ok(Some((rec.account, RefreshToken::new(rec.refresh_token), rec.scopes)))
}

pub(crate) fn save(
    store: &dyn SecretStore,
    account: &Account,
    refresh_token: &RefreshToken,
    scopes: &GrantedScopes,
) -> Result<()> {
    let rec = StoredAccount {
        account: account.clone(),
        refresh_token: refresh_token.secret().to_owned(),
        scopes: scopes.clone(),
    };
    let json = serde_json::to_string(&rec).map_err(|e| Error::new(ErrorKind::Bug, "serialize").with_source(e))?;
    store.set(&account_key(account.id()), &json)?;
    let mut ids = index(store)?;
    if !ids.iter().any(|i| i == account.id()) {
        ids.push(account.id().to_owned());
        write_index(store, &ids)?;
    }
    Ok(())
}

pub(crate) fn remove(store: &dyn SecretStore, account_id: &str) -> Result<()> {
    store.delete(&account_key(account_id))?;
    let mut ids = index(store)?;
    let before = ids.len();
    ids.retain(|i| i != account_id);
    if ids.len() != before {
        write_index(store, &ids)?;
    }
    Ok(())
}

pub(crate) fn index(store: &dyn SecretStore) -> Result<Vec<String>> {
    match store.get(INDEX_KEY)? {
        // A corrupt index is not fatal: it only lists ids, the accounts are separate items.
        Some(raw) if raw.len() <= MAX_ITEM_LEN => Ok(serde_json::from_str(&raw).unwrap_or_default()),
        _ => Ok(Vec::new()),
    }
}

fn write_index(store: &dyn SecretStore, ids: &[String]) -> Result<()> {
    let json = serde_json::to_string(ids).map_err(|e| Error::new(ErrorKind::Bug, "serialize").with_source(e))?;
    store.set(INDEX_KEY, &json)
}
