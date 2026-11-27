//! The process-local store: the bounded table behind the standing single-replica deployment.
//!
//! One `Mutex<LruCache>` owns every window, so a check and a charge are atomic by construction —
//! the property the shared backends must reproduce with their own transactions. The table
//! survives a reload on purpose: spend already recorded belongs to the identity, not to the
//! document that happened to be published when it was spent.

use std::{
    num::NonZeroUsize,
    sync::{Mutex, MutexGuard},
};

use async_trait::async_trait;
use lru::LruCache;

use super::{BudgetKey, BudgetPolicy, BudgetStore, BudgetStoreError, Charge, CheckResult};

/// One identity's window: what it has spent and when that window opened.
#[derive(Debug, Clone, Copy)]
struct Window {
    used: u64,
    started: std::time::SystemTime,
}

/// The standing implementation: a bounded LRU of fixed windows.
pub struct InProcessBudgetStore {
    entries: Mutex<LruCache<BudgetKey, Window>>,
}

impl InProcessBudgetStore {
    /// Builds an empty table holding at most `max_keys` identities.
    #[must_use]
    pub fn new(max_keys: usize) -> Self {
        Self {
            entries: Mutex::new(LruCache::new(
                NonZeroUsize::new(max_keys).unwrap_or(NonZeroUsize::MAX),
            )),
        }
    }

    /// Takes the table's lock, recovering a poisoned one.
    fn lock(&self) -> MutexGuard<'_, LruCache<BudgetKey, Window>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl std::fmt::Debug for InProcessBudgetStore {
    /// Reports the table's shape, never a subject: the keys are hashed and the spend is a number.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InProcessBudgetStore")
            .finish_non_exhaustive()
    }
}

/// Reports whether `window` has run past `policy`'s duration at `now`.
fn expired(window: &Window, policy: &BudgetPolicy, now: std::time::SystemTime) -> bool {
    let elapsed = now
        .duration_since(window.started)
        .unwrap_or(std::time::Duration::ZERO);
    elapsed >= std::time::Duration::from_secs(u64::from(policy.window_seconds))
}

/// Returns the window for `key`, reset when it has expired.
fn current(
    entries: &mut LruCache<BudgetKey, Window>,
    key: &BudgetKey,
    policy: &BudgetPolicy,
    now: std::time::SystemTime,
) -> Window {
    match entries.get(key).copied() {
        Some(window) if !expired(&window, policy, now) => window,
        _ => Window {
            used: 0,
            started: now,
        },
    }
}

#[async_trait]
impl BudgetStore for InProcessBudgetStore {
    async fn check(
        &self,
        key: &BudgetKey,
        policy: &BudgetPolicy,
        now: std::time::SystemTime,
    ) -> Result<CheckResult, BudgetStoreError> {
        let mut entries = self.lock();
        let window = current(&mut entries, key, policy, now);
        if window.used >= policy.limit_tokens {
            return Ok(CheckResult::Exceeded(super::Exceeded {
                limit: policy.limit_tokens,
                window_seconds: policy.window_seconds,
            }));
        }
        // Touch the entry so a live identity is not evicted while an idle one grows old.
        entries.put(*key, window);
        Ok(CheckResult::Allowed)
    }

    async fn charge(
        &self,
        key: &BudgetKey,
        policy: &BudgetPolicy,
        charge: &Charge,
    ) -> Result<(), BudgetStoreError> {
        let mut entries = self.lock();
        let mut window = current(&mut entries, key, policy, charge.at);
        window.used = window.used.saturating_add(charge.tokens());
        entries.put(*key, window);
        Ok(())
    }
}
