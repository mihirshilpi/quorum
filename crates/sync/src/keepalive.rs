//! Presence keep-alive: holds Quorum's application presence session so Teams shows the user
//! as Available while the app is open (and Away while they are idle).
//!
//! While the keep-alive is wanted (setting [`crate::engine::KEEPALIVE_SETTING`], default
//! `true`, toggled with [`crate::EngineHandle::set_keepalive`]) and sync is not paused:
//! - Activity `Focused` or `Background`: `setPresence(Available, Available)`;
//! - Activity `Idle`: `setPresence(Away, Away)`;
//!
//! each with `sessionId` = the app's client id and `expirationDuration` =
//! [`crate::EngineConfig::presence_expiration`], refreshed every
//! [`crate::EngineConfig::keepalive_every`] and immediately when the wanted state changes.
//! Turning it off, pausing or stopping the engine calls `clearPresence` once (not while paused
//! for sign-in, when Graph would refuse anyway).

use std::sync::Arc;

use quorum_model::{Activity as PresenceActivity, Availability};
use tokio::time::Instant;

use crate::engine::{Activity, Control, KEEPALIVE_SETTING, Shared, kind_name, lock};
use crate::error::{Error, ErrorKind};
use crate::schedule;

/// The presence the keep-alive should hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Want {
    /// Available / Available.
    Available,
    /// Away / Away.
    Away,
}

impl Want {
    fn pair(self) -> (Availability, PresenceActivity) {
        match self {
            Self::Available => (Availability::Available, PresenceActivity::Available),
            Self::Away => (Availability::Away, PresenceActivity::Away),
        }
    }
}

/// What the keep-alive should hold for `ctl` (`None`: no session).
pub(crate) fn want(ctl: &Control) -> Option<Want> {
    if ctl.stopped || !ctl.keepalive || ctl.halted() {
        return None;
    }
    Some(match ctl.activity {
        Activity::Focused | Activity::Background => Want::Available,
        Activity::Idle => Want::Away,
    })
}

fn set_active(shared: &Shared, active: bool) {
    lock(&shared.health).keepalive_active = active;
}

async fn clear(shared: &Shared) {
    match shared.ctx.graph.clear_presence(&shared.ctx.client_id).await {
        Ok(()) => tracing::debug!(name: "sync.presence_cleared", "presence session cleared"),
        Err(e) => {
            let e = Error::from(e);
            tracing::warn!(name: "sync.presence_clear_failed", kind = kind_name(e.kind()), "clearing presence failed");
        }
    }
    set_active(shared, false);
}

/// Runs the keep-alive until the engine stops.
pub(crate) async fn run(shared: Arc<Shared>) {
    // Apply the stored preference (default on).
    match shared.ctx.store.setting::<bool>(KEEPALIVE_SETTING.to_owned()).await {
        Ok(Some(enabled)) => {
            shared.control.send_if_modified(|c| {
                let changed = c.keepalive != enabled;
                c.keepalive = enabled;
                changed
            });
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(name: "sync.keepalive_setting_unreadable", error = %e, "keep-alive setting unreadable")
        }
    }
    let mut rx = shared.control.subscribe();
    // The state last attempted, whether a session is held, and when to refresh.
    let mut attempted: Option<Want> = None;
    let mut held = false;
    let mut next_refresh = Instant::now();
    let mut failures: u32 = 0;
    loop {
        let ctl = rx.borrow_and_update().clone();
        let Some(w) = want(&ctl) else {
            if held && !ctl.paused_for_auth {
                clear(&shared).await;
            }
            held = false;
            set_active(&shared, false);
            attempted = None;
            if ctl.stopped || rx.changed().await.is_err() {
                return;
            }
            continue;
        };
        let now = Instant::now();
        if attempted != Some(w) || now >= next_refresh {
            attempted = Some(w);
            let (availability, activity) = w.pair();
            let result = shared
                .ctx
                .graph
                .set_presence(&shared.ctx.client_id, &availability, &activity, shared.config.presence_expiration)
                .await;
            let every = shared.config.keepalive_every.max(std::time::Duration::from_secs(1));
            match result {
                Ok(()) => {
                    failures = 0;
                    held = true;
                    set_active(&shared, true);
                    shared.note_reachability(Ok(()));
                    next_refresh = now + every;
                }
                Err(ge) => {
                    let e = Error::from(ge);
                    failures = failures.saturating_add(1);
                    shared.note_reachability(Err(e.kind()));
                    tracing::warn!(name: "sync.presence_set_failed", kind = kind_name(e.kind()), "setting presence failed");
                    next_refresh = match (e.kind(), e.retry_after()) {
                        (ErrorKind::Auth, _) => {
                            shared.pause_for_auth();
                            now
                        }
                        (ErrorKind::Throttled, Some(ra)) => now + schedule::clamp_retry_after(ra),
                        _ => now + schedule::backoff(failures, shared.config.base_backoff, every),
                    };
                }
            }
        }
        tokio::select! {
            () = tokio::time::sleep_until(next_refresh) => {}
            r = rx.changed() => if r.is_err() { return; },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctl() -> Control {
        Control {
            activity: Activity::Focused,
            focus: None,
            paused: false,
            paused_for_auth: false,
            keepalive: true,
            stopped: false,
        }
    }

    #[test]
    fn wanted_state_follows_activity_and_flags() {
        assert_eq!(want(&ctl()), Some(Want::Available));
        assert_eq!(want(&Control { activity: Activity::Background, ..ctl() }), Some(Want::Available));
        assert_eq!(want(&Control { activity: Activity::Idle, ..ctl() }), Some(Want::Away));
        assert_eq!(want(&Control { keepalive: false, ..ctl() }), None);
        assert_eq!(want(&Control { paused: true, ..ctl() }), None);
        assert_eq!(want(&Control { paused_for_auth: true, ..ctl() }), None);
        assert_eq!(want(&Control { stopped: true, ..ctl() }), None);
    }
}
