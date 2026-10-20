//! Time and sleeping behind a trait so tests never really wait (M-MOCKABLE-SYSCALLS).

use std::future::Future;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;

/// A source of monotonic time, wall-clock time and sleeps.
pub trait Clock: Send + Sync + 'static {
    /// Monotonic now, for rate limiting.
    fn now(&self) -> Instant;
    /// Wall-clock now, for `Retry-After` HTTP-dates.
    fn now_utc(&self) -> jiff::Timestamp;
    /// Waits for `duration`.
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send;
}

/// The real clock, sleeping on the tokio timer.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn now_utc(&self) -> jiff::Timestamp {
        jiff::Timestamp::now()
    }

    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> + Send {
        tokio::time::sleep(duration)
    }
}

/// Object-safe form of [`Clock`].
pub(crate) trait DynClock: Send + Sync {
    fn now(&self) -> Instant;
    fn now_utc(&self) -> jiff::Timestamp;
    fn sleep(&self, duration: Duration) -> BoxFuture<'_, ()>;
}

impl<C: Clock> DynClock for C {
    fn now(&self) -> Instant {
        Clock::now(self)
    }

    fn now_utc(&self) -> jiff::Timestamp {
        Clock::now_utc(self)
    }

    fn sleep(&self, duration: Duration) -> BoxFuture<'_, ()> {
        Box::pin(Clock::sleep(self, duration))
    }
}

#[cfg(any(test, feature = "test-util"))]
pub use fake::FakeClock;

#[cfg(any(test, feature = "test-util"))]
mod fake {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::Clock;

    /// A clock whose `sleep` returns at once and advances virtual time; records every sleep.
    #[derive(Clone, Debug)]
    pub struct FakeClock {
        base: Instant,
        base_utc: jiff::Timestamp,
        state: Arc<Mutex<State>>,
    }

    #[derive(Debug, Default)]
    struct State {
        offset: Duration,
        sleeps: Vec<Duration>,
    }

    impl Default for FakeClock {
        fn default() -> Self {
            Self::new()
        }
    }

    impl FakeClock {
        /// A fake clock starting at the real current instant and 2026-01-01T00:00:00Z.
        #[must_use]
        pub fn new() -> Self {
            Self::at(jiff::Timestamp::from_second(1_767_225_600).unwrap_or(jiff::Timestamp::UNIX_EPOCH))
        }

        /// A fake clock whose wall time starts at `utc`.
        #[must_use]
        pub fn at(utc: jiff::Timestamp) -> Self {
            Self { base: Instant::now(), base_utc: utc, state: Arc::default() }
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, State> {
            self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        }

        /// Every duration passed to `sleep`, in order.
        #[must_use]
        pub fn sleeps(&self) -> Vec<Duration> {
            self.lock().sleeps.clone()
        }

        /// Total virtual time elapsed.
        #[must_use]
        pub fn elapsed(&self) -> Duration {
            self.lock().offset
        }

        /// Advances virtual time without recording a sleep.
        pub fn advance(&self, by: Duration) {
            let mut s = self.lock();
            s.offset = s.offset.saturating_add(by);
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            let offset = self.lock().offset;
            self.base.checked_add(offset).unwrap_or(self.base)
        }

        fn now_utc(&self) -> jiff::Timestamp {
            let offset = self.lock().offset;
            jiff::SignedDuration::try_from(offset)
                .ok()
                .and_then(|d| self.base_utc.checked_add(d).ok())
                .unwrap_or(self.base_utc)
        }

        async fn sleep(&self, duration: Duration) {
            {
                let mut s = self.lock();
                s.sleeps.push(duration);
                s.offset = s.offset.saturating_add(duration);
            }
            tokio::task::yield_now().await;
        }
    }
}
