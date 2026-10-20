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

/// How long the user has been idle, from the operating system:
/// - macOS: `ioreg -c IOHIDSystem` → `HIDIdleTime` (nanoseconds).
/// - Windows: `GetLastInputInfo` (through `user-idle-time`).
/// - Linux: GNOME's idle monitor over D-Bus (`gdbus`), which works on Wayland and X11.
///   Other desktops report unknown, so only window focus decides.
#[derive(Debug, Default)]
pub struct SystemIdle;

impl IdleSource for SystemIdle {
    fn idle_seconds(&self) -> Option<u64> {
        platform_idle_seconds()
    }
}

#[cfg(target_os = "macos")]
fn platform_idle_seconds() -> Option<u64> {
    let out = std::process::Command::new("/usr/sbin/ioreg").args(["-c", "IOHIDSystem", "-d", "4"]).output().ok()?;
    parse_hid_idle(&String::from_utf8_lossy(out.stdout.get(..out.stdout.len().min(1 << 20))?))
}

#[cfg(windows)]
fn platform_idle_seconds() -> Option<u64> {
    user_idle_time::get_idle_time().ok().map(|d| d.as_secs())
}

#[cfg(target_os = "linux")]
fn platform_idle_seconds() -> Option<u64> {
    let out = std::process::Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--timeout",
            "2",
            "--dest",
            "org.gnome.Mutter.IdleMonitor",
            "--object-path",
            "/org/gnome/Mutter/IdleMonitor/Core",
            "--method",
            "org.gnome.Mutter.IdleMonitor.GetIdletime",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_gdbus_idle(&String::from_utf8_lossy(out.stdout.get(..out.stdout.len().min(4096))?))
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
fn platform_idle_seconds() -> Option<u64> {
    None
}

#[cfg(any(target_os = "linux", test))]
/// Parses gdbus's `(uint64 123456,)` (milliseconds) into seconds (pure, tested).
pub fn parse_gdbus_idle(text: &str) -> Option<u64> {
    let digits: String = text
        .trim()
        .trim_start_matches('(')
        .trim_start_matches("uint64")
        .trim()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse::<u64>().ok().map(|ms| ms / 1000)
}

#[cfg(any(target_os = "macos", test))]
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
    fn gdbus_idle_parses_milliseconds_and_rejects_junk() {
        assert_eq!(parse_gdbus_idle("(uint64 123456,)\n"), Some(123));
        assert_eq!(parse_gdbus_idle("(uint64 999,)"), Some(0));
        for junk in ["", "()", "(uint64 ,)", "Error: no such name", "(uint64 99999999999999999999999,)"] {
            assert_eq!(parse_gdbus_idle(junk), None, "{junk}");
        }
    }

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
