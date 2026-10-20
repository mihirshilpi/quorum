//! Pure scheduling arithmetic: cadence selection, jitter, backoff and backfill pacing.
//!
//! Everything here is integer arithmetic on [`Duration`]s (no float casts) so hostile
//! cadences or retry hints can never panic or overflow.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::Duration;

use crate::Cadence;
use crate::engine::Activity;

/// Shortest interval between two passes of one resource, whatever its cadence says.
pub(crate) const MIN_INTERVAL: Duration = Duration::from_secs(1);
/// Longest interval between two passes of one resource.
pub(crate) const MAX_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Longest `Retry-After` we honour; longer hints are treated as this.
pub(crate) const MAX_RETRY_AFTER: Duration = Duration::from_secs(60 * 60);
/// After this many consecutive [`crate::Outcome::More`] passes a resource rests for one
/// cadence interval, so a resource that never reports caught-up cannot spin forever.
pub(crate) const MAX_CONSECUTIVE_MORE: u32 = 10_000;

/// Whether a resource with `key` in `area` counts as on screen for `focus`.
///
/// `focus` is an area name (`mail`) or a finer string such as `chat:<id>`: a resource is
/// focused when the focus equals its area or its key, or starts with `<area>:`.
pub(crate) fn is_focused(focus: Option<&str>, area: &str, key: &str) -> bool {
    let Some(f) = focus else { return false };
    if f.is_empty() {
        return false;
    }
    f == area || f == key || f.strip_prefix(area).is_some_and(|rest| rest.starts_with(':'))
}

/// The interval a resource should wait between passes in the current state.
pub(crate) fn interval(cadence: Cadence, activity: Activity, focus: Option<&str>, area: &str, key: &str) -> Duration {
    let raw = match activity {
        Activity::Idle => cadence.idle,
        Activity::Background => cadence.background,
        Activity::Focused if is_focused(focus, area, key) => cadence.focused,
        Activity::Focused => cadence.background,
    };
    raw.clamp(MIN_INTERVAL, MAX_INTERVAL)
}

/// `d` scaled by `permille / 1000` (jitter factors are 900..=1100, i.e. ±10%).
pub(crate) fn jittered(d: Duration, permille: u32) -> Duration {
    let ms = d.as_millis().saturating_mul(u128::from(permille)) / 1000;
    Duration::from_millis(u64::try_from(ms).unwrap_or(u64::MAX))
}

/// Exponential backoff after `failures` consecutive failures: `base`, 2×, 4×, … capped at `cap`.
pub(crate) fn backoff(failures: u32, base: Duration, cap: Duration) -> Duration {
    let exp = failures.saturating_sub(1).min(20);
    base.saturating_mul(1u32 << exp).min(cap)
}

/// Delay before the next backfill pass after `consecutive` [`crate::Outcome::More`] results:
/// quick at first so the first screenful arrives fast, then a steadier pace.
pub(crate) fn more_delay(consecutive: u32) -> Duration {
    if consecutive <= 3 { Duration::from_millis(250) } else { Duration::from_secs(1) }
}

/// A `Retry-After` hint made safe: at least one second, at most [`MAX_RETRY_AFTER`].
pub(crate) fn clamp_retry_after(d: Duration) -> Duration {
    d.clamp(Duration::from_secs(1), MAX_RETRY_AFTER)
}

/// Tiny SplitMix64 generator for jitter (no cryptographic use).
#[derive(Debug, Clone)]
pub(crate) struct Jitter(u64);

impl Jitter {
    /// A generator seeded from `key` and process-random state.
    pub(crate) fn seeded(key: &str) -> Self {
        let mut h = RandomState::new().build_hasher();
        h.write(key.as_bytes());
        Self(h.finish())
    }

    /// A generator with a fixed seed (tests).
    #[cfg(test)]
    pub(crate) fn fixed(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A jitter factor in 900..=1100 permille.
    pub(crate) fn permille(&mut self) -> u32 {
        900 + u32::try_from(self.next() % 201).unwrap_or(100)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: Cadence = Cadence::secs(15, 60, 300);

    #[test]
    fn cadence_follows_activity_and_focus() {
        assert_eq!(interval(C, Activity::Focused, Some("mail"), "mail", "mail.folder:1"), Duration::from_secs(15));
        assert_eq!(interval(C, Activity::Focused, Some("chat"), "mail", "mail.folder:1"), Duration::from_secs(60));
        assert_eq!(interval(C, Activity::Focused, None, "mail", "mail.folder:1"), Duration::from_secs(60));
        assert_eq!(interval(C, Activity::Background, Some("mail"), "mail", "mail.folder:1"), Duration::from_secs(60));
        assert_eq!(interval(C, Activity::Idle, Some("mail"), "mail", "mail.folder:1"), Duration::from_secs(300));
    }

    #[test]
    fn finer_focus_matches_area_and_key() {
        assert!(is_focused(Some("chat:42"), "chat", "chat:42"));
        assert!(is_focused(Some("chat:42"), "chat", "chats"));
        assert!(is_focused(Some("mail.folder:1"), "mail", "mail.folder:1"));
        assert!(!is_focused(Some("chats"), "chat", "chat:42"));
        assert!(!is_focused(Some(""), "chat", "chat:42"));
        assert!(!is_focused(Some("mailbox"), "mail", "mail.folder:1"));
    }

    #[test]
    fn hostile_cadences_are_clamped() {
        let zero = Cadence::secs(0, 0, 0);
        assert_eq!(interval(zero, Activity::Focused, None, "a", "k"), MIN_INTERVAL);
        let huge = Cadence::secs(u64::MAX, u64::MAX, u64::MAX);
        assert_eq!(interval(huge, Activity::Idle, None, "a", "k"), MAX_INTERVAL);
        assert_eq!(jittered(Duration::MAX, 1100), Duration::from_millis(u64::MAX));
    }

    #[test]
    fn jitter_stays_within_ten_percent() {
        let mut j = Jitter::fixed(7);
        for _ in 0..10_000 {
            let p = j.permille();
            assert!((900..=1100).contains(&p));
        }
        assert_eq!(jittered(Duration::from_secs(10), 900), Duration::from_secs(9));
        assert_eq!(jittered(Duration::from_secs(10), 1100), Duration::from_secs(11));
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let base = Duration::from_secs(5);
        let cap = Duration::from_secs(600);
        assert_eq!(backoff(1, base, cap), Duration::from_secs(5));
        assert_eq!(backoff(2, base, cap), Duration::from_secs(10));
        assert_eq!(backoff(4, base, cap), Duration::from_secs(40));
        assert_eq!(backoff(50, base, cap), cap);
        assert_eq!(backoff(u32::MAX, base, cap), cap);
        assert_eq!(clamp_retry_after(Duration::ZERO), Duration::from_secs(1));
        assert_eq!(clamp_retry_after(Duration::MAX), MAX_RETRY_AFTER);
    }
}
