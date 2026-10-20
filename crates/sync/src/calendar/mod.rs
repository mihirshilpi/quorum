//! The merged calendar: one view over the user's own calendars (and calendar groups),
//! calendars shared with them, Microsoft 365 group calendars and people they opened (synced
//! when shared, free/busy otherwise).
//!
//! - [`resources`]: what is kept in sync (calendar list, the default calendar's view delta over
//!   a rolling −30 d … +180 d window, polling of every other layer, opened-people checks).
//! - [`outbox`]: replay of event writes (create with `transactionId`, update, delete, RSVP with
//!   proposed times, cancel, forward, reminders).
//! - [`query`], [`view`], [`sources`]: the merged range query and view model the engine serves.
//! - [`lookup`]: free/busy, meeting time suggestions and rooms, on demand with a short cache.

use std::sync::Arc;

use futures::future::BoxFuture;

use crate::{Ctx, Module, OutboxHandler, Resource, Result};

pub mod lookup;
pub mod outbox;
pub mod prefs;
pub mod query;
pub mod resources;
pub mod sources;
pub mod view;

/// The calendar sync module.
#[derive(Debug, Default)]
pub struct CalendarModule;

impl CalendarModule {
    /// Creates the module (and forgets lookups cached for a previous sign-in).
    #[must_use]
    pub fn new() -> Self {
        lookup::clear_cache();
        Self
    }
}

impl Module for CalendarModule {
    fn name(&self) -> &'static str {
        "calendar"
    }

    fn discover<'a>(&'a self, _ctx: &'a Ctx) -> BoxFuture<'a, Result<Vec<Arc<dyn Resource>>>> {
        Box::pin(async {
            let v: Vec<Arc<dyn Resource>> = vec![
                Arc::new(resources::CalendarList),
                Arc::new(resources::People),
                Arc::new(resources::DefaultDelta),
                Arc::new(resources::OtherCalendars::default()),
            ];
            Ok(v)
        })
    }

    fn handlers(&self) -> Vec<Arc<dyn OutboxHandler>> {
        outbox::handlers()
    }
}
