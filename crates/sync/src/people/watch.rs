//! The set of user ids whose presence the UI currently shows.
//!
//! UI components call `presence.watch {user_ids}` (debounced) for every avatar on screen; each
//! id stays watched for a TTL after its last report, so people who scroll out of view stop
//! being polled. The watched-presence resource batches the live ids into
//! `getPresencesByUserId` calls.
//!
//! The set is process-wide ([`PresenceWatch::global`]): one Quorum process shows one account,
//! and the engine command and the sync resource must share it without widening [`crate::Ctx`].
//! Tests build their own sets with [`PresenceWatch::new`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use quorum_model::UserId;

/// How long an id stays watched after the UI last reported it.
pub const DEFAULT_WATCH_TTL: Duration = Duration::from_secs(5 * 60);
/// Shortest TTL a caller may ask for.
pub const MIN_WATCH_TTL: Duration = Duration::from_secs(10);
/// Longest TTL a caller may ask for.
pub const MAX_WATCH_TTL: Duration = Duration::from_secs(60 * 60);
/// Most ids watched at once; when full, the ids closest to expiry are dropped first.
pub const MAX_WATCHED: usize = 2_000;

/// A TTL set of watched user ids. Cheap to clone (shared `Arc`).
#[derive(Clone, Default)]
pub struct PresenceWatch {
    inner: Arc<Mutex<HashMap<UserId, Instant>>>,
}

impl std::fmt::Debug for PresenceWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PresenceWatch").field("len", &self.len()).finish()
    }
}

impl PresenceWatch {
    /// An empty, private set (tests, or callers that wire their own resource).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The process-wide set shared by `presence.watch` and the sync resource.
    #[must_use]
    pub fn global() -> Self {
        static GLOBAL: OnceLock<PresenceWatch> = OnceLock::new();
        GLOBAL.get_or_init(Self::new).clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<UserId, Instant>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Watches `ids` for `ttl` (clamped to [`MIN_WATCH_TTL`]..=[`MAX_WATCH_TTL`]) from now.
    /// Returns how many ids were not watched before (the caller polls sooner when > 0).
    pub fn watch(&self, ids: &[UserId], ttl: Duration) -> usize {
        self.watch_at(ids, ttl.clamp(MIN_WATCH_TTL, MAX_WATCH_TTL), Instant::now())
    }

    /// [`PresenceWatch::watch`] at an explicit instant, with the TTL unclamped (tests).
    pub fn watch_at(&self, ids: &[UserId], ttl: Duration, now: Instant) -> usize {
        let until = now.checked_add(ttl).unwrap_or(now);
        let mut map = self.lock();
        map.retain(|_, exp| *exp > now);
        let mut added = 0usize;
        for id in ids {
            match map.get_mut(id) {
                Some(exp) => *exp = (*exp).max(until),
                None => {
                    map.insert(id.clone(), until);
                    added = added.saturating_add(1);
                }
            }
        }
        if map.len() > MAX_WATCHED {
            let mut by_expiry: Vec<(UserId, Instant)> = map.iter().map(|(k, v)| (k.clone(), *v)).collect();
            by_expiry.sort_by_key(|(_, exp)| *exp);
            let excess = map.len().saturating_sub(MAX_WATCHED);
            for (id, _) in by_expiry.into_iter().take(excess) {
                map.remove(&id);
            }
        }
        added
    }

    /// Ids still watched now, sorted (expired ones are dropped).
    #[must_use]
    pub fn active(&self) -> Vec<UserId> {
        self.active_at(Instant::now())
    }

    /// Ids still watched at `now`, sorted.
    #[must_use]
    pub fn active_at(&self, now: Instant) -> Vec<UserId> {
        let mut map = self.lock();
        map.retain(|_, exp| *exp > now);
        let mut ids: Vec<UserId> = map.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Number of ids held (including ones that expired but were not pruned yet).
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Forgets every id (sign-out).
    pub fn clear(&self) {
        self.lock().clear();
    }
}
