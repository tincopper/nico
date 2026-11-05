//! The bounded token-bucket table a rate-limit plugin charges callers against.
//!
//! A bucket is `burst` tokens that refill at `per_second`; one request costs one token. The table is
//! an LRU bounded by `max_keys`, because the key is derived from something the caller controls: an
//! unbounded map would let one client grow process memory without limit, and a plain map that never
//! evicts would do the same for a client that keeps changing its own key.
//!
//! Three invariants are visible in the types:
//!
//! - **The clock is `tokio::time::Instant`.** It is the clock a test can pause and advance, and it is
//!   the same clock the rest of the data plane measures a request against. Reading the wall clock
//!   here would make a refill depend on an operator's NTP step.
//! - **The key is a hash.** The table therefore owns one fixed-width key per bucket, so a request
//!   allocates nothing to look one up, and no client address is stored in plaintext.
//! - **The lock is never held across an `await`.** [`Limiter::acquire`] is synchronous by signature:
//!   the short critical section cannot be widened by a caller.

use std::{
    num::{NonZeroU32, NonZeroUsize},
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use lru::LruCache;
use tokio::time::Instant;

use crate::{PluginError, spec::nonzero};

/// The bucket's capacity, refill rate and table bound.
///
/// `Copy` and comparable so the compiler can decide whether a published limiter may be reused after a
/// reload: the same parameters mean the same buckets, different parameters mean the quota changed and
/// the buckets must start over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LimitParameters {
    burst: NonZeroU32,
    per_second: NonZeroU32,
    max_keys: NonZeroUsize,
}

impl LimitParameters {
    /// Compiles a `rate_limit` entry's numbers.
    ///
    /// # Errors
    /// Returns [`PluginError::InvalidParameter`] for a zero `burst` or `per_second`, or a `max_keys`
    /// that is zero or cannot be a table bound on this platform.
    pub(crate) fn new(
        plugin: &str,
        burst: u32,
        per_second: u32,
        max_keys: u32,
    ) -> Result<Self, PluginError> {
        let max_keys = usize::try_from(max_keys)
            .ok()
            .and_then(NonZeroUsize::new)
            .ok_or_else(|| PluginError::InvalidParameter {
                plugin: plugin.to_owned(),
                field: "max_keys",
                reason: "must be greater than zero".into(),
            })?;
        Ok(Self {
            burst: nonzero(plugin, "burst", burst)?,
            per_second: nonzero(plugin, "per_second", per_second)?,
            max_keys,
        })
    }

    /// Returns how many requests may arrive back to back.
    pub(crate) const fn burst(self) -> NonZeroU32 {
        self.burst
    }

    /// Returns how many tokens one second restores.
    pub(crate) const fn per_second(self) -> NonZeroU32 {
        self.per_second
    }

    /// Returns how many buckets the table holds before the least recently used is evicted.
    pub(crate) const fn max_keys(self) -> NonZeroUsize {
        self.max_keys
    }
}

/// One caller's bucket: how many tokens it has and when that was last computed.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    updated: Instant,
}

/// A bounded table of token buckets, shared by every request one plugin instance serves.
///
/// The table outlives a reload on purpose: it is runtime state, not configuration, so republishing a
/// document must not hand every caller a fresh quota. The compiler that rebuilds filters keeps the
/// `Arc<Limiter>` it already has while the parameters are unchanged.
pub(crate) struct Limiter {
    parameters: LimitParameters,
    buckets: Mutex<LruCache<[u8; 32], Bucket>>,
}

impl Limiter {
    /// Builds an empty table for `parameters`.
    #[must_use]
    pub(crate) fn new(parameters: LimitParameters) -> Self {
        Self {
            parameters,
            buckets: Mutex::new(LruCache::new(parameters.max_keys())),
        }
    }

    /// Returns the parameters this limiter was built with.
    pub(crate) const fn parameters(&self) -> LimitParameters {
        self.parameters
    }

    /// Spends one token from `key`'s bucket.
    ///
    /// A key seen for the first time starts with a full bucket, so a caller's first request always
    /// succeeds unless its own burst is zero (which the parameters make unrepresentable).
    ///
    /// # Errors
    /// Returns how long until the bucket holds a token again when it is empty. The duration is always
    /// positive and never longer than the time one token takes, so a caller that waits exactly that
    /// long succeeds.
    pub(crate) fn acquire(&self, key: [u8; 32], now: Instant) -> Result<(), Duration> {
        let burst = f64::from(self.parameters.burst().get());
        let rate = f64::from(self.parameters.per_second().get());
        let mut buckets = self.lock();
        let mut bucket = buckets.get(&key).copied().unwrap_or(Bucket {
            tokens: burst,
            updated: now,
        });

        let elapsed = now.saturating_duration_since(bucket.updated);
        bucket.tokens = (bucket.tokens + elapsed.as_secs_f64() * rate).min(burst);
        bucket.updated = now;

        let outcome = if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            Err(wait_for_one_token(bucket.tokens, rate))
        };
        // The bucket is written back even when the request was refused: the refill it just received
        // must not be computed a second time on the next request.
        buckets.put(key, bucket);
        outcome
    }

    /// Returns how many buckets the table currently holds.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }

    /// Takes the table's lock, recovering from a poisoned one.
    ///
    /// No code that runs while the lock is held can panic, so a poisoned guard still describes a
    /// consistent map. Recovering keeps the request path alive; replacing the table instead would
    /// reset every bucket, and a limit that disappears under load is the one failure it must not
    /// have.
    fn lock(&self) -> MutexGuard<'_, LruCache<[u8; 32], Bucket>> {
        match self.buckets.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Returns how long a bucket with `tokens` needs to hold one token again.
///
/// `rate` is a non-zero integer, so the quotient is finite and positive; the fallback exists only
/// because `Duration` cannot represent a non-finite number, and answering "one second" is a
/// fail-closed answer where "no wait" would invite an immediate retry loop.
fn wait_for_one_token(tokens: f64, rate: f64) -> Duration {
    Duration::try_from_secs_f64((1.0 - tokens) / rate).unwrap_or(Duration::from_secs(1))
}

#[cfg(test)]
mod tests;
