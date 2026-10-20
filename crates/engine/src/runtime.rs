//! The signed-in runtime: the account's store and the sync engine, created after sign-in.

use std::sync::Arc;

use base64::Engine as _;
use quorum_auth::SecretStore;
use quorum_ipc::CoreEvent;
use quorum_store::{KeySource, Store, StoreKey};
use quorum_sync::{Ctx, EngineHandle, Module};

use crate::error::{Error, ErrorKind, Result};
use crate::session::Session;

/// The store and sync engine for the signed-in account.
#[derive(Debug, Clone)]
pub struct Runtime {
    /// The account's local store.
    pub store: Store,
    /// The running sync engine.
    pub sync: EngineHandle,
}

/// The store key lives in the keychain next to the refresh token.
struct KeychainKey {
    account: String,
}

impl KeySource for KeychainKey {
    fn load_or_create(&self) -> std::result::Result<StoreKey, Box<dyn std::error::Error + Send + Sync + 'static>> {
        let keychain = quorum_auth::KeyringStore::new();
        let name = format!("store-key:{}", self.account);
        if let Some(b64) = keychain.get(&name)? {
            let bytes = base64::engine::general_purpose::STANDARD.decode(b64.trim())?;
            let arr: [u8; quorum_store::KEY_LEN] = bytes.try_into().map_err(|_| "store key has the wrong length")?;
            return Ok(StoreKey::from_bytes(arr));
        }
        let key = StoreKey::generate();
        keychain.set(&name, &base64::engine::general_purpose::STANDARD.encode(key.as_bytes()))?;
        Ok(key)
    }
}

/// Every feature area's sync module. Feature areas add theirs here.
pub(crate) fn modules() -> Vec<Arc<dyn Module>> {
    vec![
        Arc::new(quorum_sync::people::PeopleModule::new()),
        Arc::new(quorum_sync::teams::TeamsModule::new()),
        Arc::new(quorum_sync::mail::MailModule::new()),
        Arc::new(quorum_sync::files::FilesModule),
        Arc::new(quorum_sync::calendar::CalendarModule::new()),
    ]
}

/// Opens the store and starts sync for `account_id` (`None` in mock mode: in-memory store).
pub(crate) async fn start(session: &Session, account_id: Option<&str>, client_id: String) -> Result<Runtime> {
    let store_err = |e: quorum_store::Error| Error::new(ErrorKind::Store, "cannot open the local store").with_source(e);
    let store = match account_id {
        None => Store::open_in_memory().map_err(store_err)?,
        Some(id) => {
            let safe: String =
                id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect();
            let dir = crate::account::data_dir().join("accounts").join(safe);
            Store::open(dir, KeychainKey { account: id.to_owned() }).await.map_err(store_err)?
        }
    };
    let mut changes = store.subscribe();
    let s = session.clone();
    tokio::spawn(async move {
        loop {
            match changes.recv().await {
                Ok(c) => s.emit(CoreEvent::StoreChanged { tables: c.tables.iter().map(|t| (*t).to_owned()).collect() }),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    s.emit(CoreEvent::StoreChanged { tables: vec!["*".into()] });
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    let ctx = Ctx { graph: session.graph().clone(), store: store.clone(), client_id };
    let sync = quorum_sync::Engine::start(ctx, modules());
    watch_auth(session.clone(), sync.clone());
    Ok(Runtime { store, sync })
}

/// Tells the UI once when sync pauses because sign-in is required again.
fn watch_auth(session: Session, sync: EngineHandle) {
    tokio::spawn(async move {
        let mut reported = false;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            if sync.is_stopped() {
                break;
            }
            let paused = sync.status().paused_for_auth;
            if paused && !reported {
                session.emit(CoreEvent::Account { state: "needs_reauth".into() });
            }
            reported = paused;
        }
    });
}
