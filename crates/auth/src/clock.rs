//! Injectable time source (M-MOCKABLE-SYSCALLS).

use std::fmt::Debug;
use std::time::Duration;

use jiff::Timestamp;

use crate::BoxFuture;

/// Wall-clock time and sleeping, behind a trait so tests can control time.
pub trait Clock: Debug + Send + Sync + 'static {
    /// The current time.
    fn now(&self) -> Timestamp;

    /// Waits for `duration` (device-code polling interval).
    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()>;
}

/// The real clock: [`Timestamp::now`] and [`tokio::time::sleep`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(duration))
    }
}

#[cfg(any(test, feature = "test-util"))]
pub use fake::FakeClock;

#[cfg(any(test, feature = "test-util"))]
mod fake {
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;

    use jiff::{SignedDuration, Timestamp};

    use super::Clock;
    use crate::BoxFuture;

    /// A manually driven clock. `sleep` returns immediately and advances the clock.
    #[derive(Debug, Clone)]
    pub struct FakeClock {
        now: Arc<Mutex<Timestamp>>,
        slept: Arc<Mutex<Vec<Duration>>>,
    }

    impl FakeClock {
        /// A clock frozen at `start`.
        #[must_use]
        pub fn new(start: Timestamp) -> Self {
            Self { now: Arc::new(Mutex::new(start)), slept: Arc::default() }
        }

        /// Moves time forward by `by`.
        pub fn advance(&self, by: Duration) {
            let mut now = self.now.lock().unwrap_or_else(PoisonError::into_inner);
            let by = SignedDuration::try_from(by).unwrap_or(SignedDuration::MAX);
            *now = now.checked_add(by).unwrap_or(Timestamp::MAX);
        }

        /// Every duration passed to [`Clock::sleep`] so far.
        #[must_use]
        pub fn sleeps(&self) -> Vec<Duration> {
            self.slept.lock().unwrap_or_else(PoisonError::into_inner).clone()
        }
    }

    impl Default for FakeClock {
        fn default() -> Self {
            Self::new(Timestamp::from_second(1_700_000_000).unwrap_or(Timestamp::UNIX_EPOCH))
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Timestamp {
            *self.now.lock().unwrap_or_else(PoisonError::into_inner)
        }

        fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()> {
            self.slept.lock().unwrap_or_else(PoisonError::into_inner).push(duration);
            self.advance(duration);
            Box::pin(std::future::ready(()))
        }
    }
}
