//! Retry policy, `Retry-After` parsing, rate limiting and per-bucket concurrency.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::clock::DynClock;
use crate::error::{Error, ErrorKind, Result};
use crate::request::ResourceFamily;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// When and how often a failed request is retried.
///
/// Retries happen only for requests that are [idempotent](crate::Request::is_idempotent), on
/// 429/503/504 (waiting `Retry-After` when given), other 5xx and network errors.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct RetryPolicy {
    /// Total attempts including the first (minimum 1).
    pub max_attempts: u32,
    /// Backoff before the first retry when Graph gives no `Retry-After`.
    pub base_delay: Duration,
    /// Ceiling on a computed backoff.
    pub max_delay: Duration,
    /// Longest `Retry-After` we wait inside a call; longer requests surface as
    /// [`ErrorKind::Throttled`] with [`Error::retry_after`] set so the caller can reschedule.
    pub max_retry_after: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
            max_retry_after: Duration::from_secs(120),
        }
    }
}

impl RetryPolicy {
    /// Backoff before retry number `retry` (1-based), with "equal jitter": the result lies in
    /// `[d/2, d]` where `d = min(max_delay, base_delay * 2^(retry-1))`. `unit` is a random number
    /// in `[0, 1]` (clamped; NaN counts as 0).
    #[must_use]
    pub fn backoff(&self, retry: u32, unit: f64) -> Duration {
        let exp = retry.saturating_sub(1).min(32);
        let factor = 1u128 << exp;
        let max = self.max_delay.as_nanos();
        let full = self.base_delay.as_nanos().saturating_mul(factor).min(max);
        let half = full / 2;
        let unit = if unit.is_nan() { 0.0 } else { unit.clamp(0.0, 1.0) };
        // `as` from f64 saturates; `unit` is finite and in range.
        let jitter = ((half as f64) * unit) as u128;
        let nanos = half.saturating_add(jitter).min(full);
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }
}

/// Parses a `Retry-After` value: delay seconds (`"120"`) or an HTTP-date
/// (`"Wed, 21 Oct 2015 07:28:00 GMT"`), relative to `now`. Dates in the past give zero.
/// Returns `None` for anything else.
#[must_use]
pub fn parse_retry_after(value: &str, now: jiff::Timestamp) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() || value.len() > 64 {
        return None;
    }
    if value.bytes().all(|b| b.is_ascii_digit()) {
        let secs: u64 = value.parse().unwrap_or(u64::MAX);
        return Some(Duration::from_secs(secs));
    }
    let when = jiff::fmt::rfc2822::DateTimeParser::new().parse_timestamp(value).ok()?;
    let diff = when.duration_since(now);
    if diff.is_negative() {
        return Some(Duration::ZERO);
    }
    Duration::try_from(diff).ok()
}

/// A token-bucket rate: sustained requests per second and burst size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateLimit {
    per_second: f64,
    burst: u32,
}

impl RateLimit {
    /// `per_second` sustained requests (finite, > 0) with bursts of up to `burst` (≥ 1).
    ///
    /// # Errors
    /// [`ErrorKind::Config`] for out-of-range values.
    pub fn new(per_second: f64, burst: u32) -> Result<Self> {
        if !per_second.is_finite() || per_second <= 0.0 || per_second > 1_000_000.0 || burst == 0 {
            return Err(Error::new(ErrorKind::Config, "rate limit must be positive and finite"));
        }
        Ok(Self { per_second, burst })
    }

    /// Sustained requests per second.
    #[must_use]
    pub fn per_second(&self) -> f64 {
        self.per_second
    }

    /// Burst size.
    #[must_use]
    pub fn burst(&self) -> u32 {
        self.burst
    }

    /// The default rate for `family` (well under Graph's documented service limits).
    #[must_use]
    pub fn default_for(family: ResourceFamily) -> Self {
        match family {
            ResourceFamily::Teams => Self { per_second: 5.0, burst: 10 },
            ResourceFamily::Mail | ResourceFamily::Calendar | ResourceFamily::Files | ResourceFamily::Other => {
                Self { per_second: 10.0, burst: 20 }
            }
        }
    }
}

#[derive(Debug)]
struct Bucket {
    rate: RateLimit,
    tokens: f64,
    last: Instant,
    paused_until: Option<Instant>,
}

impl Bucket {
    /// Takes a token, or says how long to wait for one.
    fn try_take(&mut self, now: Instant) -> std::result::Result<(), Duration> {
        if let Some(until) = self.paused_until {
            if now < until {
                return Err(until.duration_since(now));
            }
            self.paused_until = None;
        }
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        let cap = f64::from(self.rate.burst);
        self.tokens = (self.tokens + elapsed * self.rate.per_second).min(cap);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            let missing = (1.0 - self.tokens).max(0.0);
            let secs = missing / self.rate.per_second;
            Err(Duration::try_from_secs_f64(secs).unwrap_or(Duration::from_millis(1)).max(Duration::from_millis(1)))
        }
    }
}

/// Global per-family token buckets plus per-key concurrency semaphores.
pub(crate) struct Limiter {
    buckets: Vec<Mutex<Bucket>>,
    semaphores: Mutex<HashMap<String, Arc<Semaphore>>>,
    concurrency: usize,
}

/// Most distinct concurrency keys kept; idle ones are pruned beyond this.
const MAX_BUCKET_KEYS: usize = 1024;
/// Most waits for a rate token before giving up (each wait is at least 1 ms).
const MAX_RATE_WAITS: u32 = 10_000;

impl Limiter {
    pub(crate) fn new(rates: [RateLimit; 5], concurrency: usize, now: Instant) -> Self {
        let buckets = rates
            .iter()
            .map(|&rate| Mutex::new(Bucket { rate, tokens: f64::from(rate.burst), last: now, paused_until: None }))
            .collect();
        Self { buckets, semaphores: Mutex::new(HashMap::new()), concurrency: concurrency.max(1) }
    }

    /// Waits for a rate token in `family`.
    pub(crate) async fn acquire_rate(&self, family: ResourceFamily, clock: &dyn DynClock) -> Result<()> {
        let Some(bucket) = self.buckets.get(family.index()) else {
            debug_assert!(false, "missing bucket for family");
            return Err(Error::new(ErrorKind::Bug, "missing rate bucket"));
        };
        for _ in 0..MAX_RATE_WAITS {
            let wait = lock(bucket).try_take(clock.now());
            match wait {
                Ok(()) => return Ok(()),
                Err(d) => clock.sleep(d).await,
            }
        }
        Err(Error::new(ErrorKind::Throttled, "rate limiter wait exceeded"))
    }

    /// Stops `family` from sending for `duration` (after Graph said 429).
    pub(crate) fn pause(&self, family: ResourceFamily, duration: Duration, now: Instant) {
        if let Some(bucket) = self.buckets.get(family.index()) {
            let until = now.checked_add(duration).unwrap_or(now);
            let mut b = lock(bucket);
            if b.paused_until.is_none_or(|u| u < until) {
                b.paused_until = Some(until);
            }
        }
    }

    /// Takes a concurrency permit for `key`.
    pub(crate) async fn acquire_slot(&self, key: &str) -> Result<OwnedSemaphorePermit> {
        let sem = {
            let mut map = lock(&self.semaphores);
            if !map.contains_key(key) && map.len() >= MAX_BUCKET_KEYS {
                // Drop idle keys (nobody else holds the semaphore).
                map.retain(|_, s| Arc::strong_count(s) > 1);
            }
            Arc::clone(map.entry(key.to_owned()).or_insert_with(|| Arc::new(Semaphore::new(self.concurrency))))
        };
        sem.acquire_owned().await.map_err(|_| Error::new(ErrorKind::Bug, "concurrency semaphore closed"))
    }
}

/// A small deterministic PRNG (SplitMix64) for backoff jitter; seedable for tests.
pub(crate) struct Jitter(Mutex<u64>);

impl Jitter {
    pub(crate) fn new(seed: u64) -> Self {
        Self(Mutex::new(seed))
    }

    /// A number in `[0, 1)`.
    pub(crate) fn next_unit(&self) -> f64 {
        let mut s = lock(&self.0);
        *s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        // 53 random bits → [0, 1).
        ((z >> 11) as f64) / ((1u64 << 53) as f64)
    }
}
