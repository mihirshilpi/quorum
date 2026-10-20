//! People and presence sync (architecture §7 "Presence of visible people", §8 Presence and
//! "Across everything").
//!
//! Resources:
//! - `presence.me` ([`MyPresence`]): the user's own presence, 30 s focused / 2 min background;
//!   learns the user's id from `/me` on first run.
//! - `presence.watch` ([`WatchedPresence`]): presence of every user id the UI shows, batched
//!   through `getPresencesByUserId`, 30 s / 2 min; the set is fed by `presence.watch` with a TTL
//!   ([`PresenceWatch`]).
//! - `people.relevant` ([`RelevantPeople`]): `/me/people`, plus the user's org chart.
//!
//! Outbox handlers: `presence.set_preferred`, `presence.clear_preferred`,
//! `presence.set_status_message`. The keep-alive session (`setPresence`) is the sync engine's
//! own loop, not this module.
//!
//! The helpers ([`fetch_org`], [`directory_search`], [`fetch_photo`], ...) are what engine
//! queries call for on-demand lookups; every result is cached in the store.

use std::sync::Arc;

use futures::future::BoxFuture;

use crate::{Ctx, Module, OutboxHandler, Resource, Result};

mod directory;
mod presence;
mod watch;

pub use directory::{
    MAX_DIRECTORY_HITS, MAX_MANAGER_CHAIN, MAX_PHOTO_BYTES, MAX_RELEVANT, ORG_MAX_AGE_SECS, OrgChart, RelevantPeople,
    cached_org, cached_photo, directory_search, fetch_org, fetch_person, fetch_photo, image_type, org_setting,
    photo_blob, relevant,
};
pub use presence::{
    ClearPreferredHandler, KIND_CLEAR_PREFERRED, KIND_SET_PREFERRED, KIND_STATUS_MESSAGE, MAX_DURATION_MINUTES,
    MAX_STATUS_MESSAGE_CHARS, MyPresence, OWN_PRESENCE_RESOURCE, PresenceChoice, SetPreferredHandler,
    StatusMessageHandler, WatchedPresence, my_id, short_id, validate_status_message, with_choice,
};
pub use watch::{DEFAULT_WATCH_TTL, MAX_WATCH_TTL, MAX_WATCHED, MIN_WATCH_TTL, PresenceWatch};

/// Settings key holding the signed-in user's id (string).
pub const ME_ID_SETTING: &str = "people.me_id";
/// Settings key holding the signed-in user's [`quorum_model::Person`].
pub const ME_SETTING: &str = "people.me";
/// Settings key holding the relevance-ranked people (`Vec<Person>`).
pub const RELEVANT_SETTING: &str = "people.relevant";

/// The people and presence sync module.
#[derive(Debug, Clone)]
pub struct PeopleModule {
    watch: PresenceWatch,
}

impl Default for PeopleModule {
    fn default() -> Self {
        Self::new()
    }
}

impl PeopleModule {
    /// The module over the process-wide watch set.
    #[must_use]
    pub fn new() -> Self {
        Self { watch: PresenceWatch::global() }
    }

    /// The module over a private watch set (tests).
    #[must_use]
    pub fn with_watch(watch: PresenceWatch) -> Self {
        Self { watch }
    }
}

impl Module for PeopleModule {
    fn name(&self) -> &'static str {
        "people"
    }

    fn discover<'a>(&'a self, _ctx: &'a Ctx) -> BoxFuture<'a, Result<Vec<Arc<dyn Resource>>>> {
        let watch = self.watch.clone();
        Box::pin(async move {
            let v: Vec<Arc<dyn Resource>> =
                vec![Arc::new(MyPresence), Arc::new(WatchedPresence::new(watch)), Arc::new(RelevantPeople)];
            Ok(v)
        })
    }

    fn handlers(&self) -> Vec<Arc<dyn OutboxHandler>> {
        vec![Arc::new(SetPreferredHandler), Arc::new(ClearPreferredHandler), Arc::new(StatusMessageHandler)]
    }
}
