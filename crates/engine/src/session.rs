//! The session: one signed-in (or signed-out) account and its command registry.

use std::sync::Arc;

use quorum_ipc::{CommandInfo, CoreEvent};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::command::Registry;
use crate::error::Result;
use crate::guard::guarded;

/// Capacity of the event channel; slow listeners drop old events and re-query.
const EVENT_CAPACITY: usize = 1024;

tokio::task_local! {
    static CURRENT: &'static str;
}

/// Options for opening a session.
#[derive(Debug, Clone, Default)]
pub struct SessionOptions {
    /// Run against recorded Graph fixtures instead of the network.
    pub mock_graph: bool,
}

struct Inner {
    runtime: std::sync::RwLock<Option<crate::runtime::Runtime>>,
    accounts: crate::account::Accounts,
    registry: Registry,
    queries: Registry,
    events: broadcast::Sender<CoreEvent>,
    options: SessionOptions,
    ui_state: std::sync::Mutex<Value>,
}

/// The Quorum core. Cheap to clone (M-SERVICES-CLONE).
#[derive(Clone)]
pub struct Session {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").field("commands", &self.inner.registry.len()).finish_non_exhaustive()
    }
}

impl Session {
    /// Opens a session with the built-in commands.
    ///
    /// # Errors
    /// Mock mode was requested but the Graph fixtures are missing.
    pub fn new(options: SessionOptions) -> Result<Self> {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let accounts = crate::account::Accounts::new(options.mock_graph)?;
        Ok(Self {
            inner: Arc::new(Inner {
                runtime: std::sync::RwLock::new(None),
                accounts,
                registry: Registry::builtin(),
                queries: Registry::builtin_queries(),
                events,
                options,
                ui_state: std::sync::Mutex::new(Value::Null),
            }),
        })
    }

    /// Restores a stored sign-in and starts background work. Call once after [`Session::new`].
    pub async fn start(&self) {
        match self.inner.accounts.restore().await {
            Ok(true) => self.on_signed_in().await,
            Ok(false) => {}
            Err(e) => tracing::warn!(target: "quorum::account", error = %e, "restoring sign-in failed"),
        }
    }

    /// Runs after a successful sign-in or restore.
    pub(crate) async fn on_signed_in(&self) {
        let (account, client_id) = match self.inner.accounts.provider().await {
            Some(p) => (
                Some(p.account().id().to_owned()),
                crate::account::AccountConfig::load().map(|c| c.client_id).unwrap_or_default(),
            ),
            None => (None, "00000000-0000-0000-0000-000000000000".to_owned()),
        };
        match crate::runtime::start(self, account.as_deref(), client_id).await {
            Ok(rt) => {
                if let Ok(mut g) = self.inner.runtime.write()
                    && let Some(old) = g.replace(rt)
                {
                    old.sync.stop();
                }
            }
            Err(e) => tracing::error!(target: "quorum::runtime", error = %e, "starting the store and sync failed"),
        }
        self.emit(CoreEvent::Account { state: "signed_in".into() });
    }

    /// Stops sync and closes the store (sign-out).
    pub(crate) fn stop_runtime(&self) {
        if let Ok(mut g) = self.inner.runtime.write()
            && let Some(rt) = g.take()
        {
            rt.sync.stop();
        }
    }

    /// The signed-in account's store.
    ///
    /// # Errors
    /// [`crate::ErrorKind::NotAvailable`] before sign-in.
    pub fn store(&self) -> Result<quorum_store::Store> {
        self.runtime().map(|r| r.store)
    }

    /// The running sync engine.
    ///
    /// # Errors
    /// [`crate::ErrorKind::NotAvailable`] before sign-in.
    pub fn sync(&self) -> Result<quorum_sync::EngineHandle> {
        self.runtime().map(|r| r.sync)
    }

    fn runtime(&self) -> Result<crate::runtime::Runtime> {
        self.inner
            .runtime
            .read()
            .ok()
            .and_then(|g| g.clone())
            .ok_or_else(|| crate::Error::new(crate::ErrorKind::NotAvailable, "sign in first"))
    }

    pub(crate) fn accounts(&self) -> &crate::account::Accounts {
        &self.inner.accounts
    }

    /// The Microsoft Graph client (fixture-backed in mock mode).
    pub fn graph(&self) -> &quorum_graph::Client {
        self.inner.accounts.graph()
    }

    /// The options this session was opened with.
    pub fn options(&self) -> &SessionOptions {
        &self.inner.options
    }

    /// The command registry.
    pub fn registry(&self) -> &Registry {
        &self.inner.registry
    }

    /// Runs command `id` with JSON `params` under the panic guard.
    ///
    /// # Errors
    /// Unknown id, invalid params, a disabled command, or the command's own failure.
    pub async fn execute(&self, id: &str, params: Value) -> Result<Value> {
        let spec = *self.inner.registry.get(id)?;
        if !(spec.enabled)(self) {
            return Err(crate::Error::new(
                crate::ErrorKind::NotAvailable,
                format!("`{id}` is not available right now"),
            ));
        }
        tracing::debug!(target: "quorum::cmd", command = id, "execute");
        CURRENT.scope(spec.id, guarded(id, (spec.run)(self, params))).await
    }

    /// Runs query `id` with JSON `params` under the panic guard. Queries only read.
    ///
    /// # Errors
    /// Unknown id, invalid params, or the query's own failure.
    pub async fn query(&self, id: &str, params: Value) -> Result<Value> {
        let spec = *self.inner.queries.get(id)?;
        tracing::trace!(target: "quorum::query", query = id, "query");
        guarded(id, (spec.run)(self, params)).await
    }

    /// The query registry.
    pub fn queries(&self) -> &Registry {
        &self.inner.queries
    }

    /// Describes every command for menus and the palette.
    pub fn commands(&self) -> Vec<CommandInfo> {
        self.inner
            .registry
            .iter()
            .map(|c| CommandInfo {
                id: c.id.to_owned(),
                label: c.label.to_owned(),
                menu: c.menu.map(str::to_owned),
                shortcut: c.shortcut.map(str::to_owned),
                destructive: c.destructive,
                enabled: (c.enabled)(self),
            })
            .collect()
    }

    /// Id of the command whose future is running on this task, if any.
    pub fn current_command(&self) -> Option<String> {
        CURRENT.try_with(|id| (*id).to_owned()).ok()
    }

    /// Stores the UI's latest report of what is on screen.
    pub fn set_ui_state(&self, state: Value) {
        if let Ok(mut g) = self.inner.ui_state.lock() {
            *g = state;
        }
    }

    /// The UI's latest report of what is on screen (`null` before the first report).
    pub fn ui_state(&self) -> Value {
        self.inner.ui_state.lock().map(|g| g.clone()).unwrap_or(Value::Null)
    }

    /// Subscribes to core events.
    pub fn subscribe(&self) -> broadcast::Receiver<CoreEvent> {
        self.inner.events.subscribe()
    }

    /// Publishes a core event; having no listeners is fine.
    pub fn emit(&self, event: CoreEvent) {
        let _ = self.inner.events.send(event);
    }
}
