//! Token budgets: how much one identity may spend on one route, and when that resets.
//!
//! The interface exists because the standing implementation is a *deployment* choice, not a
//! permanent one: a single process can keep its own table, but a deployment running several replicas
//! needs the windows shared through external storage. Both implement [`BudgetStore`], and the check
//! and charge call sites do not change between them.
//!
//! The semantics are **fixed window, charged after the fact**:
//!
//! - a window opens when the identity's first tokens are charged, and closes `window_seconds` later;
//! - the usage a request produces is known only after the provider answered, so a request may push
//!   its identity past the limit — the *next* request is the one refused. This is the standard
//!   post-hoc accounting every LLM gateway uses, because a gateway has no tokenizer and cannot
//!   predict what an answer will cost.
//! - the producer never blocks and the table never grows without bound: the key space is the set of
//!   authenticated identities, bounded by an LRU like every other per-caller table in this
//!   workspace.
//!
//! The policy (limit, window) travels with each call from the route's document rather than living in
//! the store: a reload that retunes a budget changes what the *next* request sees while the windows
//! already open keep their accumulated spend.

use std::{
    num::NonZeroUsize,
    sync::{Mutex, MutexGuard},
};

use lru::LruCache;
use tokio::time::Instant;

/// The budget one route declares, as its document states it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BudgetPolicy {
    /// The most tokens one identity may spend in one window.
    limit_tokens: u64,
    /// How long one window lasts.
    window_seconds: u32,
}

impl BudgetPolicy {
    /// Builds a policy from the route's declared budget.
    pub(crate) fn from_route(budget: &nico_core::TokenBudget) -> Self {
        Self {
            limit_tokens: budget.limit_tokens(),
            window_seconds: budget.window_seconds(),
        }
    }
}

/// Why a request was refused: the identity's window is spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Exceeded {
    /// The limit the document set, reported so the answer names it.
    limit: u64,
    /// The window the limit applies to, for the same reason.
    window_seconds: u32,
}

/// Where token budgets live.
///
/// The key is opaque (the caller hashes `(route, subject)` into it), so an implementation may store
/// whatever it wants — a process-local map today, a shared external table when the deployment runs
/// several replicas.
pub(crate) trait BudgetStore: Send + Sync + std::fmt::Debug {
    /// Returns whether `key` may spend another request under `policy` at `now`.
    fn check(&self, key: &[u8; 32], policy: &BudgetPolicy, now: Instant) -> Result<(), Exceeded>;

    /// Adds `tokens` to `key`'s current window at `now`.
    fn charge(&self, key: &[u8; 32], tokens: u64, policy: &BudgetPolicy, now: Instant);
}

/// One identity's window: what it has spent and when that window opened.
#[derive(Debug, Clone, Copy)]
struct Window {
    used: u64,
    started: Instant,
}

/// The standing implementation: a bounded LRU of fixed windows.
///
/// The table survives a reload on purpose — spend already recorded belongs to the identity, not to
/// the document that happened to be published when it was spent.
pub(crate) struct InProcessBudgetStore {
    entries: Mutex<LruCache<[u8; 32], Window>>,
}

impl InProcessBudgetStore {
    /// Builds an empty table holding at most `max_keys` identities.
    #[must_use]
    pub(crate) fn new(max_keys: usize) -> Self {
        Self {
            entries: Mutex::new(LruCache::new(
                NonZeroUsize::new(max_keys).unwrap_or(NonZeroUsize::MAX),
            )),
        }
    }

    /// Takes the table's lock, recovering a poisoned one.
    fn lock(&self) -> MutexGuard<'_, LruCache<[u8; 32], Window>> {
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
fn expired(window: &Window, policy: &BudgetPolicy, now: Instant) -> bool {
    now.saturating_duration_since(window.started).as_secs() >= u64::from(policy.window_seconds)
}

/// Returns the window for `key`, reset when it has expired.
fn current(
    entries: &mut LruCache<[u8; 32], Window>,
    key: &[u8; 32],
    policy: &BudgetPolicy,
    now: Instant,
) -> Window {
    match entries.get(key).copied() {
        Some(window) if !expired(&window, policy, now) => window,
        _ => Window {
            used: 0,
            started: now,
        },
    }
}

impl BudgetStore for InProcessBudgetStore {
    fn check(&self, key: &[u8; 32], policy: &BudgetPolicy, now: Instant) -> Result<(), Exceeded> {
        let mut entries = self.lock();
        let window = current(&mut entries, key, policy, now);
        if window.used >= policy.limit_tokens {
            return Err(Exceeded {
                limit: policy.limit_tokens,
                window_seconds: policy.window_seconds,
            });
        }
        // Touch the entry so a live identity is not evicted while an idle one grows old.
        entries.put(*key, window);
        Ok(())
    }

    fn charge(&self, key: &[u8; 32], tokens: u64, policy: &BudgetPolicy, now: Instant) {
        let mut entries = self.lock();
        let mut window = current(&mut entries, key, policy, now);
        window.used = window.used.saturating_add(tokens);
        entries.put(*key, window);
    }
}

/// Checks one request against its route's budget, before anything upstream happens.
///
/// Three conditions skip the check, each with its own reason: the route declares no budget (most
/// routes), the request has no authenticated identity (a deployment that wants full coverage puts an
/// auth plugin on the route), and the identity still has room.
pub(crate) fn check_request(
    store: &dyn BudgetStore,
    ctx: &crate::context::HttpContext,
    now: Instant,
) -> Result<(), crate::error::HttpError> {
    let Some(policy) = ctx.token_budget().map(BudgetPolicy::from_route) else {
        return Ok(());
    };
    let Some(identity) = ctx.identity() else {
        return Ok(());
    };
    let quota_identity = identity.tenant().unwrap_or_else(|| identity.subject());
    let key = budget_key(
        ctx.route_id().unwrap_or_default(),
        quota_identity,
        identity.tenant().is_some(),
    );
    store.check(&key, &policy, now).map_err(|exceeded| {
        crate::error::HttpError::TokenBudgetExceeded {
            limit: exceeded.limit,
            window_seconds: exceeded.window_seconds,
        }
    })
}

/// Charges one served request's tokens to its identity, from the numbers the provider reported.
///
/// Only `input` and `output` count: the two full-price buckets a bill is made of. The discount
/// buckets (cache reads and writes) are deliberately excluded, so the budget tracks what the caller
/// is actually driving up. A request with no reported usage charges nothing, and one without an
/// identity has no window to charge.
pub(crate) fn charge_request(
    store: &dyn BudgetStore,
    ctx: &crate::context::HttpContext,
    input: u64,
    output: u64,
    now: Instant,
) {
    let Some(policy) = ctx.token_budget().map(BudgetPolicy::from_route) else {
        return;
    };
    let Some(identity) = ctx.identity() else {
        return;
    };
    let quota_identity = identity.tenant().unwrap_or_else(|| identity.subject());
    let key = budget_key(
        ctx.route_id().unwrap_or_default(),
        quota_identity,
        identity.tenant().is_some(),
    );
    store.charge(&key, input.saturating_add(output), &policy, now);
}

/// Hashes `(route, kind, quota identity)` into the store's fixed-width key.
///
/// The kind tag prevents a tenant label from colliding semantically with an untenantized subject
/// carrying the same bytes. The route remains part of the key, so a tenant spends separate windows
/// on separate routes.
pub(crate) fn budget_key(route_id: &str, identity: &str, tenant: bool) -> [u8; 32] {
    let mix = |bytes: &[u8], seed: u64| -> u64 {
        let mut hash = seed;
        for &byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    };
    let domain = if tenant { 0x54 } else { 0x53 };
    let first = mix(
        identity.as_bytes(),
        mix(route_id.as_bytes(), 0xcbf2_9ce4_8422_2325 ^ domain),
    );
    let second = mix(identity.as_bytes(), first ^ 0x9e37_79b9_7f4a_7c15 ^ domain);
    let mut key = [0u8; 32];
    key[..8].copy_from_slice(&first.to_be_bytes());
    key[8..16].copy_from_slice(&second.to_be_bytes());
    key[16..24].copy_from_slice(&first.swap_bytes().to_le_bytes());
    key[24..].copy_from_slice(&second.to_le_bytes());
    key
}

#[cfg(test)]
mod tests;
