//! Sync engine: supervised delta loops, adaptive cadence, outbox replay and presence keep-alive.
//!
//! Feature areas plug in as [`Module`]s. A module discovers its [`Resource`]s (an Inbox, a
//! calendar view, the chat list, a drive...) and provides [`OutboxHandler`]s that replay
//! queued writes against Graph. The [`Engine`] runs every resource on its [`Cadence`],
//! restarts failed passes from the stored delta state, honours throttling, and replays the
//! outbox in order per resource key.
//!
//! The UI never waits on this crate: resources write into `quorum-store`, whose change feed
//! tells the UI what to re-read.
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

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;

pub mod calendar;
mod engine;
mod error;
pub mod files;
mod guard;
pub mod keepalive;
pub mod mail;
mod outbox;
pub mod people;
mod runner;
mod schedule;
pub mod teams;

#[doc(inline)]
pub use engine::{
    Activity, DiscardOutcome, Engine, EngineConfig, EngineHandle, KEEPALIVE_SETTING, ResourceStatus, Status,
};
#[doc(inline)]
pub use error::{Error, ErrorKind, Result};

/// Everything a resource pass or outbox handler needs.
#[derive(Debug, Clone)]
pub struct Ctx {
    /// Microsoft Graph client.
    pub graph: quorum_graph::Client,
    /// The account's local store.
    pub store: quorum_store::Store,
    /// Application (client) id, used as the presence `sessionId`.
    pub client_id: String,
}

/// How often a resource runs in each activity state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cadence {
    /// The resource is on screen in a focused window.
    pub focused: Duration,
    /// The app is open but this resource is not on screen, or the window is not focused.
    pub background: Duration,
    /// The user is idle or the Mac is on battery saver.
    pub idle: Duration,
}

impl Cadence {
    /// Creates a cadence from seconds.
    pub const fn secs(focused: u64, background: u64, idle: u64) -> Self {
        Self {
            focused: Duration::from_secs(focused),
            background: Duration::from_secs(background),
            idle: Duration::from_secs(idle),
        }
    }
}

/// What a pass found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Caught up; wait for the cadence.
    CaughtUp,
    /// More to do (initial sync or backfill); run again soon at a throttled pace.
    More,
    /// Caught up, and the module's set of resources changed (a mail folder was added, a site
    /// followed): the engine re-runs the owning module's discovery now instead of at the next
    /// periodic rediscovery, then waits for the cadence.
    Rediscover,
}

/// One thing kept in sync, such as a mail folder or the chat list.
pub trait Resource: Send + Sync + 'static {
    /// Stable key, e.g. `mail.folder:<id>`; also the outbox ordering key for writes to it.
    fn key(&self) -> String;

    /// UI area this resource belongs to (`mail`, `calendar`, `chat`, `teams`, `files`,
    /// `people`), so the focused area syncs fastest.
    fn area(&self) -> &'static str;

    /// How often to run.
    fn cadence(&self) -> Cadence;

    /// Runs one pass: fetch changes from Graph and apply them to the store atomically.
    fn run<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Outcome>>;
}

/// Replays one kind of queued write (e.g. `mail.send`) against Graph.
pub trait OutboxHandler: Send + Sync + 'static {
    /// Outbox entry kind this handler accepts.
    fn kind(&self) -> &'static str;

    /// Performs the write. Returning a permanent error moves the entry to Failed for the UI
    /// to Retry or Discard; a transient one retries with backoff. After success the entry's
    /// resource key is poked; a result `{"poke": "key"}` or `{"poke": ["k1", "k2"]}` pokes
    /// those keys too.
    fn apply<'a>(&'a self, ctx: &'a Ctx, payload: Value) -> BoxFuture<'a, Result<Value>>;

    /// Undoes the optimistic local change when the user discards a failed entry.
    fn revert<'a>(&'a self, _ctx: &'a Ctx, _payload: Value) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

/// A feature area's sync plug-in.
pub trait Module: Send + Sync + 'static {
    /// Area name, e.g. `mail`.
    fn name(&self) -> &'static str;

    /// Lists the resources to keep in sync. Called at start and again whenever this module's
    /// resource set may have changed (e.g. a new folder appeared).
    fn discover<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Vec<Arc<dyn Resource>>>>;

    /// Outbox handlers for this area's writes.
    fn handlers(&self) -> Vec<Arc<dyn OutboxHandler>> {
        Vec::new()
    }
}
