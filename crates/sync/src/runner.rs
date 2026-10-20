//! The supervisor (discovery and reconciliation) and the per-resource task loop.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::Notify;
use tokio::time::Instant;

use crate::engine::{Control, ResourceStatus, Shared, Slot, kind_name, lock};
use crate::error::ErrorKind;
use crate::guard::{guarded, guarded_sync};
use crate::schedule::{self, Jitter};
use crate::{Outcome, Resource};

/// Longest resource key accepted from a module.
const MAX_KEY_LEN: usize = 1024;

fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= MAX_KEY_LEN && !key.chars().any(char::is_control)
}

/// Runs discovery and keeps one task per resource until the engine stops.
pub(crate) async fn supervise(shared: Arc<Shared>) {
    let mut rx = shared.control.subscribe();
    let mut next_full = Instant::now();
    // Module name -> (consecutive failures, retry at).
    let mut retry: HashMap<&'static str, (u32, Instant)> = HashMap::new();
    loop {
        let ctl = rx.borrow_and_update().clone();
        if ctl.stopped {
            return;
        }
        if ctl.halted() {
            // Discovery waits for resume (a request made meanwhile stays queued).
            if rx.changed().await.is_err() {
                return;
            }
            continue;
        }
        {
            let now = Instant::now();
            let full = now >= next_full;
            let requested: HashSet<String> = std::mem::take(&mut *lock(&shared.discover_requests));
            let all = full || requested.contains("*");
            for m in &shared.modules {
                let name = guarded_sync("module name", || m.name()).unwrap_or("unknown");
                let due = all || requested.contains(name) || retry.get(name).is_some_and(|(_, at)| *at <= now);
                if !due {
                    continue;
                }
                match guarded("discover", m.discover(&shared.ctx)).await {
                    Ok(list) => {
                        retry.remove(name);
                        shared.note_reachability(Ok(()));
                        if guarded_sync("reconcile", || reconcile(&shared, name, list)).is_none() {
                            retry.insert(name, (1, now));
                        }
                    }
                    Err(e) => {
                        shared.note_reachability(Err(e.kind()));
                        tracing::warn!(name: "sync.discover_failed", module = name, kind = kind_name(e.kind()), "discovery failed");
                        if e.kind() == ErrorKind::Auth {
                            shared.pause_for_auth();
                            retry.insert(name, (0, now));
                        } else {
                            let n = retry.get(name).map_or(0, |(n, _)| *n).saturating_add(1);
                            let wait = match (e.kind(), e.retry_after()) {
                                (ErrorKind::Throttled, Some(ra)) => schedule::clamp_retry_after(ra),
                                _ => schedule::backoff(n, shared.config.base_backoff, shared.config.max_backoff),
                            };
                            retry.insert(name, (n, now + wait));
                        }
                    }
                }
                if rx.borrow().stopped {
                    return;
                }
            }
            if full {
                next_full = now + shared.config.rediscover_every;
            }
        }
        let mut next = next_full;
        for (_, at) in retry.values() {
            next = next.min(*at);
        }
        tokio::select! {
            () = tokio::time::sleep_until(next) => {}
            () = shared.supervisor_wake.notified() => {}
            r = rx.changed() => if r.is_err() { return; },
        }
    }
}

/// Makes the running tasks for `module` match `list`.
fn reconcile(shared: &Arc<Shared>, module: &'static str, list: Vec<Arc<dyn Resource>>) {
    let mut seen: HashSet<String> = HashSet::new();
    let mut tasks = lock(&shared.tasks);
    let cap = shared.config.max_resources_per_module;
    if list.len() > cap {
        tracing::warn!(name: "sync.too_many_resources", module, count = list.len(), cap, "ignoring resources over the cap");
    }
    for res in list.into_iter().take(cap) {
        let key = res.key();
        if !valid_key(&key) {
            tracing::warn!(name: "sync.invalid_resource_key", module, "resource with an invalid key ignored");
            continue;
        }
        if !seen.insert(key.clone()) {
            continue;
        }
        let respawn = match tasks.get(&key) {
            Some(slot) if slot.module != module => {
                tracing::warn!(name: "sync.resource_key_collision", module, other = slot.module, "two modules use one key; first wins");
                seen.remove(&key);
                continue;
            }
            Some(slot) => slot.handle.as_ref().is_none_or(tokio::task::JoinHandle::is_finished),
            None => true,
        };
        if respawn {
            let wake = Arc::new(Notify::new());
            let area = res.area();
            let handle = shared.spawn(resource_loop(Arc::clone(shared), res, key.clone(), Arc::clone(&wake)));
            tasks.insert(key.clone(), Slot { module, wake, handle });
            lock(&shared.health).resources.entry(key.clone()).or_insert_with(|| ResourceStatus {
                key: key.clone(),
                module: module.to_owned(),
                area: area.to_owned(),
                ..ResourceStatus::default()
            });
        }
    }
    let gone: Vec<String> =
        tasks.iter().filter(|(k, s)| s.module == module && !seen.contains(*k)).map(|(k, _)| k.clone()).collect();
    let mut health = lock(&shared.health);
    for k in gone {
        if let Some(slot) = tasks.remove(&k)
            && let Some(h) = slot.handle
        {
            h.abort();
        }
        health.resources.remove(&k);
    }
}

/// How the task waits before its next pass.
#[derive(Debug, Clone, Copy)]
enum Wait {
    /// Run as soon as allowed.
    Now,
    /// Wait one jittered cadence interval from `since` (re-evaluated when activity or focus
    /// change, and with the cadence re-read from the resource).
    Cadence { since: Instant, permille: u32 },
    /// Wait until `until`; a poke cuts it short only if `pokeable`.
    Fixed { until: Instant, pokeable: bool },
}

fn deadline(wait: Wait, ctl: &Control, res: &dyn Resource, key: &str) -> Instant {
    match wait {
        Wait::Now => Instant::now(),
        Wait::Cadence { since, permille } => {
            let cadence = guarded_sync("cadence", || res.cadence()).unwrap_or(crate::Cadence::secs(60, 300, 900));
            let area = guarded_sync("area", || res.area()).unwrap_or("");
            let d = schedule::interval(cadence, ctl.activity, ctl.focus.as_deref(), area, key);
            since + schedule::jittered(d, permille)
        }
        Wait::Fixed { until, .. } => until,
    }
}

/// Updates the resource's health entry.
fn record(shared: &Shared, key: &str, f: impl FnOnce(&mut ResourceStatus)) {
    if let Some(s) = lock(&shared.health).resources.get_mut(key) {
        f(s);
    }
}

/// One resource's life: wait, run a guarded pass, schedule the next one.
async fn resource_loop(shared: Arc<Shared>, res: Arc<dyn Resource>, key: String, wake: Arc<Notify>) {
    let mut rx = shared.control.subscribe();
    let mut jitter = Jitter::seeded(&key);
    let mut wait = Wait::Now;
    let mut failures: u32 = 0;
    let mut mores: u32 = 0;
    loop {
        // Wait phase.
        loop {
            let ctl = rx.borrow_and_update().clone();
            if ctl.stopped {
                return;
            }
            if ctl.halted() {
                if rx.changed().await.is_err() {
                    return;
                }
                continue;
            }
            let at = deadline(wait, &ctl, res.as_ref(), &key);
            if at <= Instant::now() {
                break;
            }
            tokio::select! {
                () = tokio::time::sleep_until(at) => break,
                () = wake.notified() => {
                    if !matches!(wait, Wait::Fixed { pokeable: false, .. }) {
                        break;
                    }
                }
                r = rx.changed() => if r.is_err() { return; },
            }
        }
        // Run phase.
        let Ok(permit) = shared.passes.acquire().await else { return };
        if rx.borrow().halted() {
            continue;
        }
        record(&shared, &key, |s| s.running = true);
        let result = guarded("sync pass", res.run(&shared.ctx)).await;
        drop(permit);
        let now = Instant::now();
        let utc = shared.now_utc();
        match result {
            Ok(outcome) => {
                failures = 0;
                shared.note_reachability(Ok(()));
                record(&shared, &key, |s| {
                    s.running = false;
                    s.last_success = Some(utc);
                    s.last_error = None;
                    s.failures = 0;
                });
                wait = match outcome {
                    Outcome::More if mores < schedule::MAX_CONSECUTIVE_MORE => {
                        mores = mores.saturating_add(1);
                        Wait::Fixed { until: now + schedule::more_delay(mores), pokeable: true }
                    }
                    Outcome::More => {
                        tracing::warn!(name: "sync.backfill_resting", "resource reported More too often; resting one interval");
                        mores = 0;
                        Wait::Cadence { since: now, permille: jitter.permille() }
                    }
                    Outcome::CaughtUp => {
                        mores = 0;
                        Wait::Cadence { since: now, permille: jitter.permille() }
                    }
                    Outcome::Rediscover => {
                        mores = 0;
                        let module = lock(&shared.tasks).get(&key).map(|slot| slot.module);
                        if let Some(module) = module {
                            tracing::debug!(name: "sync.rediscover_requested", module, "resource set changed");
                            shared.request_discover(module);
                        }
                        Wait::Cadence { since: now, permille: jitter.permille() }
                    }
                };
            }
            Err(e) => {
                let kind = e.kind();
                shared.note_reachability(Err(kind));
                tracing::warn!(name: "sync.pass_failed", kind = kind_name(kind), "sync pass failed");
                if kind != ErrorKind::Auth {
                    failures = failures.saturating_add(1);
                }
                record(&shared, &key, |s| {
                    s.running = false;
                    s.last_error = Some(kind_name(kind).to_owned());
                    s.failures = failures;
                });
                wait = match (kind, e.retry_after()) {
                    (ErrorKind::Auth, _) => {
                        shared.pause_for_auth();
                        Wait::Now
                    }
                    (ErrorKind::Throttled, Some(ra)) => {
                        Wait::Fixed { until: now + schedule::clamp_retry_after(ra), pokeable: false }
                    }
                    (ErrorKind::Throttled, None) => Wait::Fixed {
                        until: now + schedule::backoff(failures, shared.config.base_backoff, shared.config.max_backoff),
                        pokeable: false,
                    },
                    _ => {
                        let d = schedule::backoff(failures, shared.config.base_backoff, shared.config.max_backoff);
                        Wait::Fixed { until: now + schedule::jittered(d, jitter.permille()), pokeable: true }
                    }
                };
            }
        }
    }
}
