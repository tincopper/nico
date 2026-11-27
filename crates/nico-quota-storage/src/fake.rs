//! The deterministic backend: the atomicity real adapters must reproduce, driven by injected
//! time and failures.
//!
//! The fake is the contract suite's subject and an adapter test's environment. It holds one map
//! of windows behind a mutex — the same atomicity-by-construction the local store has — and it
//! runs the seam's pure semantics directly, so a backend that disagrees with the seam fails the
//! contract suite rather than disagreeing only in production.

use std::{collections::HashMap, sync::Mutex};

use async_trait::async_trait;
use nico_http::{BudgetKey, BudgetPolicy, BudgetStore, BudgetStoreError, Charge, CheckResult};

use crate::seam::{BackendError, CheckOutcome, WindowRecord, apply_charge, apply_check};

/// One backend's worth of windows, with the failure to inject.
pub struct FakeBackend {
    windows: Mutex<HashMap<BudgetKey, WindowRecord>>,
    /// When set, every operation fails with this error instead of touching the map.
    failure: Mutex<Option<BackendError>>,
}

impl FakeBackend {
    /// Builds an empty backend.
    #[must_use]
    pub fn new() -> Self {
        Self {
            windows: Mutex::new(HashMap::new()),
            failure: Mutex::new(None),
        }
    }

    /// Makes every subsequent operation fail with the given error, until cleared.
    pub fn fail_with(&self, error: BackendError) {
        *self.lock_failure() = Some(error);
    }

    /// Clears the injected failure.
    pub fn clear_failure(&self) {
        *self.lock_failure() = None;
    }

    /// Takes the failure slot, recovering a poisoned lock.
    fn lock_failure(&self) -> std::sync::MutexGuard<'_, Option<BackendError>> {
        self.failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Takes the window map, recovering a poisoned lock.
    fn lock_windows(&self) -> std::sync::MutexGuard<'_, HashMap<BudgetKey, WindowRecord>> {
        self.windows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Reads the stored record for one key, or `None`.
    #[must_use]
    pub fn stored(&self, key: &BudgetKey) -> Option<WindowRecord> {
        self.lock_windows().get(key).cloned()
    }
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for FakeBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FakeBackend")
            .finish_non_exhaustive()
    }
}

/// Takes the injected failure, if any.
fn injected(backend: &FakeBackend) -> Result<(), nico_http::BudgetStoreError> {
    if let Some(error) = backend.lock_failure().take() {
        // The failure is consumed: one injected error answers one operation, so a test that
        // fails the backend for two calls injects twice.
        return Err(error.into_domain());
    }
    Ok(())
}

#[async_trait]
impl BudgetStore for FakeBackend {
    async fn check(
        &self,
        key: &BudgetKey,
        policy: &BudgetPolicy,
        now: std::time::SystemTime,
    ) -> Result<CheckResult, BudgetStoreError> {
        injected(self)?;
        let mut windows = self.lock_windows();
        let stored = windows.get(key).cloned();
        let applied = apply_check(stored, policy, now)
            .map_err(|corrupt| BackendError::Corrupt(corrupt).into_domain())?;
        if let Some(record) = &applied.stored {
            windows.insert(*key, record.clone());
        }
        Ok(match applied.outcome {
            CheckOutcome::Allowed => CheckResult::Allowed,
            CheckOutcome::Spent => CheckResult::Exceeded(nico_http::Exceeded {
                limit: policy.limit_tokens,
                window_seconds: policy.window_seconds,
            }),
        })
    }

    async fn charge(
        &self,
        key: &BudgetKey,
        policy: &BudgetPolicy,
        charge: &Charge,
    ) -> Result<(), BudgetStoreError> {
        injected(self)?;
        let mut windows = self.lock_windows();
        let stored = windows.get(key).cloned();
        let applied = apply_charge(stored, policy, &charge.id, charge.tokens(), charge.at)
            .map_err(|corrupt| BackendError::Corrupt(corrupt).into_domain())?;
        windows.insert(*key, applied.stored);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
