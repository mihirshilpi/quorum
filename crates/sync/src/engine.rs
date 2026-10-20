//! The supervisor that runs resources, replays the outbox and keeps presence alive.
//!
//! [`Engine::start`] spawns, on the current tokio runtime:
//! - a **supervisor** that calls every [`Module::discover`] at start, every
//!   [`EngineConfig::rediscover_every`] and on `poke("discover:<module>")`, and keeps exactly
//!   one task per discovered [`crate::Resource`];
//! - one **resource task** per resource: run a pass, wait per [`crate::Cadence`] (focused area
//!   fastest, ±10% jitter), wake early on [`EngineHandle::poke`]. Passes run under a panic
//!   guard; errors back off exponentially (capped), throttling honours `Retry-After`, and an
//!   auth error pauses every task until [`EngineHandle::resume`];
//! - the **outbox replayer** (one task, entries in order per resource key);
//! - the **presence keep-alive** (see [`crate::keepalive`]).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, Semaphore, watch};
use tokio::task::JoinHandle;

use crate::error::{ErrorKind, Result};
use crate::{Ctx, Module, OutboxHandler};

/// What the user is doing, which sets every resource's cadence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    /// Window focused and user active.
    #[default]
    Focused,
    /// App open but not focused.
    Background,
    /// User idle, screen locked or low power.
    Idle,
}

impl Activity {
    /// Parses `focused`, `background` or `idle` (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "focused" => Some(Self::Focused),
            "background" => Some(Self::Background),
            "idle" => Some(Self::Idle),
            _ => None,
        }
    }
}

/// Health of one resource, for `sync.status`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ResourceStatus {
    /// The resource key.
    pub key: String,
    /// The module that discovered it.
    pub module: String,
    /// Its UI area.
    pub area: String,
    /// When its last pass succeeded.
    pub last_success: Option<Timestamp>,
    /// The kind of its last error (`remote`, `throttled`, `auth`, ...), cleared on success.
    pub last_error: Option<String>,
    /// Consecutive failed passes.
    pub failures: u32,
    /// Whether a pass is running right now.
    pub running: bool,
}

/// A snapshot of sync health for the UI and `sync.status`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Status {
    /// Resources being kept in sync.
    pub resources: usize,
    /// Pending outbox entries.
    pub pending_writes: usize,
    /// Failed outbox entries waiting for the user.
    pub failed_writes: usize,
    /// Whether the last passes reached Graph.
    pub online: bool,
    /// The user paused sync ([`EngineHandle::pause`]).
    pub paused: bool,
    /// Sync is paused until the user signs in again ([`EngineHandle::resume`] after sign-in).
    pub paused_for_auth: bool,
    /// The current activity.
    pub activity: Activity,
    /// The focused area (or finer focus such as `chat:<id>`), if any.
    pub focus: Option<String>,
    /// Whether the presence keep-alive is wanted.
    pub keepalive: bool,
    /// Whether a presence session is currently held.
    pub keepalive_active: bool,
    /// Per-resource health, by key.
    pub resources_detail: Vec<ResourceStatus>,
}

/// What [`EngineHandle::discard_failed`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DiscardOutcome {
    /// The entry existed (as a failed write) and was removed.
    pub discarded: bool,
    /// The handler's revert ran and succeeded.
    pub reverted: bool,
}

/// Engine tuning. The defaults are the production values.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Most passes running at once across all resources.
    pub max_concurrent_passes: usize,
    /// How often every module is asked to discover its resources again.
    pub rediscover_every: Duration,
    /// First backoff after a failed pass; doubles per failure.
    pub base_backoff: Duration,
    /// Longest backoff after failed passes.
    pub max_backoff: Duration,
    /// How often the outbox replayer re-checks the queue without being woken.
    pub outbox_poll: Duration,
    /// How often the presence session is refreshed.
    pub keepalive_every: Duration,
    /// The presence session's `expirationDuration`.
    pub presence_expiration: Duration,
    /// Most resources one module may register (further ones are ignored with a warning).
    pub max_resources_per_module: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_concurrent_passes: 6,
            rediscover_every: Duration::from_secs(10 * 60),
            base_backoff: Duration::from_secs(5),
            max_backoff: Duration::from_secs(10 * 60),
            outbox_poll: Duration::from_secs(60),
            keepalive_every: Duration::from_secs(4 * 60),
            presence_expiration: Duration::from_secs(60 * 60),
            max_resources_per_module: 5_000,
        }
    }
}

/// The store setting key for the presence keep-alive (JSON bool; absent means on). This is
/// where the engine's `settings.set {"key": "presence.keep_alive"}` stores it.
pub const KEEPALIVE_SETTING: &str = "settings.presence.keep_alive";

/// Shared, watchable control state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Control {
    pub activity: Activity,
    pub focus: Option<String>,
    pub paused: bool,
    pub paused_for_auth: bool,
    pub keepalive: bool,
    pub stopped: bool,
}

impl Control {
    /// Whether network work must wait.
    pub(crate) fn halted(&self) -> bool {
        self.paused || self.paused_for_auth
    }
}

/// One supervised resource task.
pub(crate) struct Slot {
    pub module: &'static str,
    pub wake: Arc<Notify>,
    pub handle: Option<JoinHandle<()>>,
}

/// Health counters behind [`Status`].
#[derive(Debug, Default)]
pub(crate) struct Health {
    pub online: bool,
    pub pending: usize,
    pub failed: usize,
    pub keepalive_active: bool,
    pub resources: BTreeMap<String, ResourceStatus>,
}

/// Everything the engine's tasks share.
pub(crate) struct Shared {
    pub ctx: Ctx,
    pub modules: Vec<Arc<dyn Module>>,
    pub handlers: HashMap<&'static str, Arc<dyn OutboxHandler>>,
    pub config: EngineConfig,
    pub control: watch::Sender<Control>,
    pub tasks: Mutex<HashMap<String, Slot>>,
    pub discover_requests: Mutex<HashSet<String>>,
    pub supervisor_wake: Notify,
    pub outbox_wake: Notify,
    pub passes: Semaphore,
    pub health: Mutex<Health>,
    pub runtime: Option<tokio::runtime::Handle>,
    pub background: Mutex<Vec<JoinHandle<()>>>,
    start_utc: Timestamp,
    start_instant: tokio::time::Instant,
}

/// Locks `m`, recovering from poisoning (a panic elsewhere must not wedge sync).
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Stable snake_case name of an error kind, for status and outbox `last_error`.
pub(crate) fn kind_name(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::Remote => "remote",
        ErrorKind::Throttled => "throttled",
        ErrorKind::Auth => "auth",
        ErrorKind::Store => "store",
        ErrorKind::Permanent => "permanent",
        ErrorKind::Invalid => "invalid",
        ErrorKind::Bug => "bug",
    }
}

impl Shared {
    /// Wall-clock now that also advances with tokio's (possibly paused) clock, so outbox
    /// backoffs behave in tests and never run behind real time.
    pub(crate) fn now_utc(&self) -> Timestamp {
        let elapsed = tokio::time::Instant::now().saturating_duration_since(self.start_instant);
        let virt = jiff::SignedDuration::try_from(elapsed)
            .ok()
            .and_then(|d| self.start_utc.checked_add(d).ok())
            .unwrap_or(self.start_utc);
        virt.max(Timestamp::now())
    }

    /// Pauses every task until [`EngineHandle::resume`].
    pub(crate) fn pause_for_auth(&self) {
        let changed = self.control.send_if_modified(|c| {
            let was = c.paused_for_auth;
            c.paused_for_auth = true;
            !was
        });
        if changed {
            tracing::warn!(name: "sync.paused_for_auth", "sync paused: sign-in required");
        }
    }

    /// Records the outcome of a Graph call for the `online` flag.
    pub(crate) fn note_reachability(&self, result: std::result::Result<(), ErrorKind>) {
        let mut h = lock(&self.health);
        match result {
            Ok(()) | Err(ErrorKind::Throttled | ErrorKind::Permanent | ErrorKind::Invalid) => h.online = true,
            Err(ErrorKind::Remote) => h.online = false,
            Err(_) => {}
        }
    }

    /// Wakes every resource whose key starts with `prefix`; returns how many matched.
    pub(crate) fn wake_matching(&self, prefix: &str) -> usize {
        let tasks = lock(&self.tasks);
        let mut n = 0usize;
        for (k, slot) in tasks.iter() {
            if k.starts_with(prefix) {
                slot.wake.notify_one();
                n = n.saturating_add(1);
            }
        }
        n
    }

    /// Asks the supervisor to re-run discovery for `module` (`*` for all).
    pub(crate) fn request_discover(&self, module: &str) {
        let name: String = module.chars().take(128).collect();
        lock(&self.discover_requests).insert(if name.is_empty() { "*".to_owned() } else { name });
        self.supervisor_wake.notify_one();
    }

    /// Re-reads the outbox counts from the store.
    pub(crate) async fn refresh_outbox_counts(&self) {
        let counts = self
            .ctx
            .store
            .read(|r| {
                let pending = r.outbox_pending_count()?;
                let failed = r.outbox_failed(quorum_store::MAX_PAGE)?.items.len();
                Ok((pending, failed))
            })
            .await;
        match counts {
            Ok((pending, failed)) => {
                let mut h = lock(&self.health);
                h.pending = usize::try_from(pending).unwrap_or(usize::MAX);
                h.failed = failed;
            }
            Err(e) => tracing::warn!(name: "sync.outbox_counts_failed", error = %e, "cannot read outbox counts"),
        }
    }

    /// Spawns a task on the engine's runtime (`None` without one).
    pub(crate) fn spawn(&self, fut: impl std::future::Future<Output = ()> + Send + 'static) -> Option<JoinHandle<()>> {
        self.runtime.as_ref().map(|rt| rt.spawn(fut))
    }
}

/// Builds and starts the sync engine.
#[derive(Debug, Default)]
pub struct Engine;

impl Engine {
    /// Starts syncing `modules` and returns a handle to steer it.
    ///
    /// Must be called inside a tokio runtime; without one the handle is inert (logged).
    pub fn start(ctx: Ctx, modules: Vec<Arc<dyn Module>>) -> EngineHandle {
        Self::start_with(ctx, modules, EngineConfig::default())
    }

    /// [`Engine::start`] with explicit tuning.
    pub fn start_with(ctx: Ctx, modules: Vec<Arc<dyn Module>>, config: EngineConfig) -> EngineHandle {
        let mut handlers: HashMap<&'static str, Arc<dyn OutboxHandler>> = HashMap::new();
        for m in &modules {
            let hs = crate::guard::guarded_sync("module handlers", || m.handlers()).unwrap_or_default();
            for h in hs {
                let kind = h.kind();
                if handlers.insert(kind, h).is_some() {
                    tracing::warn!(name: "sync.duplicate_handler", kind, "two outbox handlers for one kind; last wins");
                }
            }
        }
        let runtime = tokio::runtime::Handle::try_current().ok();
        if runtime.is_none() {
            tracing::error!(name: "sync.no_runtime", "sync engine started outside a tokio runtime; nothing will sync");
        }
        let (control, _) = watch::channel(Control {
            activity: Activity::Focused,
            focus: None,
            paused: false,
            paused_for_auth: false,
            keepalive: true,
            stopped: false,
        });
        let permits = config.max_concurrent_passes.clamp(1, Semaphore::MAX_PERMITS);
        let shared = Arc::new(Shared {
            ctx,
            modules,
            handlers,
            config,
            control,
            tasks: Mutex::new(HashMap::new()),
            discover_requests: Mutex::new(HashSet::new()),
            supervisor_wake: Notify::new(),
            outbox_wake: Notify::new(),
            passes: Semaphore::new(permits),
            health: Mutex::new(Health { online: true, ..Health::default() }),
            runtime,
            background: Mutex::new(Vec::new()),
            start_utc: Timestamp::now(),
            start_instant: tokio::time::Instant::now(),
        });
        let mut bg = Vec::new();
        bg.extend(shared.spawn(crate::runner::supervise(Arc::clone(&shared))));
        bg.extend(shared.spawn(crate::outbox::replay(Arc::clone(&shared))));
        bg.extend(shared.spawn(crate::keepalive::run(Arc::clone(&shared))));
        *lock(&shared.background) = bg;
        EngineHandle { inner: shared }
    }
}

/// Steers a running engine. Cheap to clone.
#[derive(Clone)]
pub struct EngineHandle {
    inner: Arc<Shared>,
}

impl std::fmt::Debug for EngineHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineHandle").field("modules", &self.inner.modules.len()).finish_non_exhaustive()
    }
}

impl EngineHandle {
    /// The context passes run with.
    pub fn ctx(&self) -> &Ctx {
        &self.inner.ctx
    }

    /// Runs the resource with `key` (or every resource whose key starts with it) now.
    ///
    /// `discover:<module>` (or `discover:*`) re-runs that module's discovery instead. A key
    /// that matches no resource triggers a re-discovery of every module, so a resource that
    /// just came into existence (a newly opened chat) starts right away. Pokes do not cut a
    /// throttling wait short.
    pub fn poke(&self, key: &str) {
        if let Some(module) = key.strip_prefix("discover:") {
            self.inner.request_discover(module);
            return;
        }
        if key == "discover" {
            self.inner.request_discover("*");
            return;
        }
        if self.inner.wake_matching(key) == 0 && !key.is_empty() {
            self.inner.request_discover("*");
        }
    }

    /// Tells the engine which UI area (or finer focus such as `chat:<id>`) is on screen.
    /// An empty string clears the focus.
    pub fn set_focus(&self, area: &str) {
        let focus: String = area.chars().filter(|c| !c.is_control()).take(256).collect();
        let focus = if focus.is_empty() { None } else { Some(focus) };
        self.inner.control.send_if_modified(|c| {
            if c.focus == focus {
                return false;
            }
            c.focus = focus;
            true
        });
    }

    /// Tells the engine what the user is doing.
    pub fn set_activity(&self, activity: Activity) {
        self.inner.control.send_if_modified(|c| {
            let changed = c.activity != activity;
            c.activity = activity;
            changed
        });
    }

    /// Turns the presence keep-alive on or off (the caller persists [`KEEPALIVE_SETTING`]).
    pub fn set_keepalive(&self, enabled: bool) {
        self.inner.control.send_if_modified(|c| {
            let changed = c.keepalive != enabled;
            c.keepalive = enabled;
            changed
        });
    }

    /// Pauses all network work (sync passes, outbox replay, keep-alive) until [`Self::resume`].
    pub fn pause(&self) {
        self.inner.control.send_if_modified(|c| {
            let changed = !c.paused;
            c.paused = true;
            changed
        });
    }

    /// Resumes after [`Self::pause`] or an auth pause (call after signing in again).
    pub fn resume(&self) {
        let changed = self.inner.control.send_if_modified(|c| {
            let changed = c.paused || c.paused_for_auth;
            c.paused = false;
            c.paused_for_auth = false;
            changed
        });
        if changed {
            tracing::info!(name: "sync.resumed", "sync resumed");
            self.inner.outbox_wake.notify_one();
        }
    }

    /// Wakes the outbox replayer after an enqueue.
    pub fn outbox_changed(&self) {
        self.inner.outbox_wake.notify_one();
    }

    /// Current sync health (outbox counts as of the replayer's last look).
    pub fn status(&self) -> Status {
        let ctl = self.inner.control.borrow().clone();
        let resources = lock(&self.inner.tasks).len();
        let h = lock(&self.inner.health);
        Status {
            resources,
            pending_writes: h.pending,
            failed_writes: h.failed,
            online: h.online,
            paused: ctl.paused,
            paused_for_auth: ctl.paused_for_auth,
            activity: ctl.activity,
            focus: ctl.focus,
            keepalive: ctl.keepalive,
            keepalive_active: h.keepalive_active,
            resources_detail: h.resources.values().cloned().collect(),
        }
    }

    /// [`Self::status`] after re-reading the outbox counts from the store.
    pub async fn refresh_status(&self) -> Status {
        self.inner.refresh_outbox_counts().await;
        self.status()
    }

    /// Puts failed outbox entry `id` back in the queue and wakes the replayer.
    ///
    /// # Errors
    /// [`ErrorKind::Store`] if the entry does not exist or the store fails.
    pub async fn retry_failed(&self, id: quorum_store::OutboxId) -> Result<()> {
        self.inner.ctx.store.outbox_retry(id).await?;
        self.inner.refresh_outbox_counts().await;
        self.inner.outbox_wake.notify_one();
        Ok(())
    }

    /// Discards failed outbox entry `id`: runs its handler's revert (undoing the optimistic
    /// change), then removes it. A failing revert is logged and the entry is still removed,
    /// so a broken entry can always be cleared.
    ///
    /// # Errors
    /// [`ErrorKind::Store`] if the store fails.
    pub async fn discard_failed(&self, id: quorum_store::OutboxId) -> Result<DiscardOutcome> {
        crate::outbox::discard(&self.inner, id).await
    }

    /// Stops all tasks. The keep-alive clears the presence session on its way out; an outbox
    /// entry being replayed is allowed to finish.
    pub fn stop(&self) {
        self.inner.control.send_modify(|c| c.stopped = true);
        let slots: Vec<Slot> = lock(&self.inner.tasks).drain().map(|(_, s)| s).collect();
        for s in slots {
            if let Some(h) = s.handle {
                h.abort();
            }
        }
        lock(&self.inner.health).resources.clear();
        self.inner.passes.close();
        self.inner.supervisor_wake.notify_one();
        self.inner.outbox_wake.notify_one();
    }

    /// Whether [`Self::stop`] was called.
    pub fn is_stopped(&self) -> bool {
        self.inner.control.borrow().stopped
    }
}
