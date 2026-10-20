//! Tells sync what the user is doing: `focused` (window in front), `background` (window not
//! focused), or `idle` (no keyboard or mouse input for `presence.idle_minutes`). Sync uses it
//! for cadence and presence keep-alive (Away when idle).
//!
//! OS idle time comes from an [`IdleSource`]; on macOS that is `ioreg`'s `HIDIdleTime`, read
//! by running the system tool (no FFI, no `unsafe`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use quorum_engine::Session;
use serde_json::json;
use tokio::sync::Notify;

/// How often idle time is polled.
const POLL: Duration = Duration::from_secs(15);

/// Seconds since the last user input, if the platform can tell.
pub trait IdleSource: Send + Sync + 'static {
    /// Idle seconds, or `None` when unknown.
    fn idle_seconds(&self) -> Option<u64>;
}

/// macOS: `ioreg -c IOHIDSystem` → `HIDIdleTime` (nanoseconds). Elsewhere: unknown.
#[derive(Debug, Default)]
pub struct SystemIdle;

impl IdleSource for SystemIdle {
    fn idle_seconds(&self) -> Option<u64> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let out = std::process::Command::new("/usr/sbin/ioreg").args(["-c", "IOHIDSystem", "-d", "4"]).output().ok()?;
        parse_hid_idle(&String::from_utf8_lossy(out.stdout.get(..out.stdout.len().min(1 << 20))?))
    }
}

/// Parses `"HIDIdleTime" = 123456789` (nanoseconds) into seconds (pure, tested).
pub fn parse_hid_idle(text: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.contains("\"HIDIdleTime\""))?;
    let value = line.split('=').nth(1)?.trim();
    let ns: u64 = value.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()?;
    Some(ns / 1_000_000_000)
}

/// The activity to report (pure, tested).
pub fn activity(focused: bool, idle_seconds: Option<u64>, idle_minutes: u64) -> &'static str {
    if idle_seconds.is_some_and(|s| s >= idle_minutes.saturating_mul(60)) {
        "idle"
    } else if focused {
        "focused"
    } else {
        "background"
    }
}

/// Window focus shared with the window-event handler.
#[derive(Debug, Default)]
pub struct Focus {
    focused: AtomicBool,
    changed: Notify,
}

impl Focus {
    /// Records a focus change and wakes the reporter.
    pub fn set(&self, focused: bool) {
        self.focused.store(focused, Ordering::Relaxed);
        self.changed.notify_one();
    }
}

/// Reports activity changes to `sync.set_activity` until the process exits.
pub async fn run(session: Session, focus: Arc<Focus>, idle: impl IdleSource) {
    let mut last = "";
    loop {
        let minutes = quorum_engine::settings::get(&session, "presence.idle_minutes")
            .await
            .ok()
            .and_then(|v| v.as_u64())
            .unwrap_or(5);
        let now = activity(focus.focused.load(Ordering::Relaxed), idle.idle_seconds(), minutes);
        if now != last {
            match session.execute("sync.set_activity", json!({ "activity": now })).await {
                Ok(_) => last = now,
                Err(e) => tracing::debug!(target: "quorum::activity", error = %e, "activity not reported"),
            }
        }
        tokio::select! {
            () = focus.changed.notified() => {}
            () = tokio::time::sleep(POLL) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hid_idle_parses_and_tolerates_junk() {
        let sample = "  | |   \"HIDIdleTime\" = 7500000000\n  | |   \"Other\" = 1";
        assert_eq!(parse_hid_idle(sample), Some(7));
        assert_eq!(parse_hid_idle("\"HIDIdleTime\" = <garbage>"), None);
        assert_eq!(parse_hid_idle(""), None);
        assert_eq!(parse_hid_idle("\"HIDIdleTime\" = 99999999999999999999999999"), None);
    }

    #[test]
    fn activity_prefers_idle_then_focus() {
        assert_eq!(activity(true, Some(10), 5), "focused");
        assert_eq!(activity(true, Some(300), 5), "idle");
        assert_eq!(activity(false, None, 5), "background");
        assert_eq!(activity(false, Some(u64::MAX), u64::MAX), "idle");
    }
}
