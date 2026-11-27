//! Token budgets: how much one identity may spend on one route, and when that resets.
//!
//! The contract is a *deployment* choice, not a permanent one: a single process keeps its own
//! table, but a deployment running several replicas shares its windows through external storage.
//! Both implement [`BudgetStore`], and the check and charge call sites do not change between them.
//! The trait is the boundary that makes that swap possible, so it is `pub`: an external storage
//! crate implements it, `nico-http` never learns which backend holds the windows.
//!
//! The semantics are **fixed window, charged after the fact**:
//!
//! - a window opens when the identity's first tokens are charged, and closes `window_seconds` later;
//! - the usage a request produces is known only after the provider answered, so a request may push
//!   its identity past the limit — the *next* request is the one refused. This is the standard
//!   post-hoc accounting every LLM gateway uses, because a gateway has no tokenizer and cannot
//!   predict what an answer will cost. The two call sites are two business moments; each backend
//!   operation must be atomic on its own, and the gap between them carries this documented
//!   overshoot rather than a reservation of tokens nobody can count yet;
//! - every charge carries a stable [`Charge::id`], so a retried write cannot double-bill: the
//!   backend deduplicates on `(namespace, key, charge id)`;
//! - the store's errors are its own vocabulary. "The window is spent" and "the storage behind the
//!   window is unreachable" are different facts, and conflating them would answer a healthy
//!   caller with a quota refusal that is really an outage.
//!
//! The policy (limit, window) travels with each call from the route's document rather than living
//! in the store: a reload that retunes a budget changes what the *next* request sees while the
//! windows already open keep their accumulated spend.

use std::{
    fmt,
    sync::Arc,
    time::{Duration, SystemTime},
};

use async_trait::async_trait;

mod local;

pub use local::InProcessBudgetStore;

/// The budget one route declares, as its document states it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetPolicy {
    /// The most tokens one identity may spend in one window.
    pub limit_tokens: u64,
    /// How long one window lasts.
    pub window_seconds: u32,
}

/// Why a request was refused: the identity's window is spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exceeded {
    /// The limit the document set, reported so the answer names it.
    pub limit: u64,
    /// The window the limit applies to, for the same reason.
    pub window_seconds: u32,
}

/// The verdict of one admission check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckResult {
    /// The identity has room; forward the request.
    Allowed,
    /// The identity's window is spent; refuse with 429.
    Exceeded(Exceeded),
}

/// The stable identity of one charge.
///
/// One successful provider exchange produces exactly one id, fixed for the exchange's lifetime:
/// a logging hook that fires twice, or a write the backend cannot confirm and that the caller
/// retries, must land as one charge, not two. The value is opaque to the backend — it only needs
/// byte equality — and it must not carry credentials or anything caller-controlled that could
/// collide across identities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChargeId(pub Arc<str>);

impl fmt::Display for ChargeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One successful exchange's usage, ready to be recorded.
#[derive(Debug, Clone)]
pub struct Charge {
    /// The deduplication identity of this charge.
    pub id: ChargeId,
    /// Provider-reported input tokens (the full-price buckets only: cache and reasoning are
    /// deliberately excluded from budgets).
    pub input: u64,
    /// Provider-reported output tokens.
    pub output: u64,
    /// When the provider exchange completed, as a wall-clock instant every replica and backend
    /// can agree on. Process-local `Instant`s do not cross a network.
    pub at: SystemTime,
}

impl Charge {
    /// The full-price total this charge adds to the window.
    #[must_use]
    pub const fn tokens(&self) -> u64 {
        self.input.saturating_add(self.output)
    }
}

/// The fixed-width identity of one budget window.
///
/// The bytes are opaque to every store — local or shared — and encode
/// `(route, kind, quota identity)` behind the same domain separation the request path derives.
/// A shared backend prefixes its namespace at the storage layer; it never reinterprets these bytes.
pub type BudgetKey = [u8; 32];

/// Where token budgets live.
///
/// Implementations: the process-local table (the standing default), and the shared backends a
/// multi-replica deployment selects. Every operation is one backend-atomic decision: a remote
/// read-modify-write done in the client is not an implementation of this trait.
///
/// Time is carried as [`SystemTime`], not `tokio::time::Instant`: a shared store compares
/// wall-clock instants written by other processes, and a monotonic process clock cannot survive
/// that comparison.
#[async_trait]
pub trait BudgetStore: Send + Sync + fmt::Debug {
    /// Returns whether `key` may spend another request under `policy` at `now`.
    ///
    /// The check must be atomic on the backend: read the window, reset it if `now` is past its
    /// expiry, create it if absent, and compare the spend against the limit — as one operation,
    /// so two replicas checking concurrently cannot each observe a half-updated window.
    ///
    /// # Errors
    /// A [`BudgetStoreError`] when the storage behind the window cannot answer; the caller decides
    /// what an admission that cannot be confirmed means.
    async fn check(
        &self,
        key: &BudgetKey,
        policy: &BudgetPolicy,
        now: SystemTime,
    ) -> Result<CheckResult, BudgetStoreError>;

    /// Adds `charge`'s tokens to `key`'s window.
    ///
    /// The write must be atomic on the backend and idempotent on `(key, charge.id)`: a retry
    /// after an unclear answer lands once. Recording is never refused for being over the limit —
    /// the spend already happened at the provider; what the total refuses is the *next* admission.
    ///
    /// # Errors
    /// A [`BudgetStoreError`] when the charge could not be confirmed. The caller must not
    /// silently drop it: usage that was served but not recorded is a budget that lies.
    async fn charge(
        &self,
        key: &BudgetKey,
        policy: &BudgetPolicy,
        charge: &Charge,
    ) -> Result<(), BudgetStoreError>;
}

/// Why the store could not answer.
///
/// One variant per decision the caller makes. None of them is an `Exceeded`: the window's verdict
/// travels through [`CheckResult`], and a store failure must never be rendered as a 429.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BudgetStoreError {
    /// The backend could not be reached, or the operation timed out.
    ///
    /// The cause is interpolated: the request record renders this error's `Display` and never
    /// walks `.source()`, so a message that omits its cause hides it from every operator. What a
    /// *response* quotes is decided by `HttpError`, not here.
    #[error("the budget store is unavailable: {source}")]
    Unavailable {
        /// The underlying cause, kept for logs and never rendered to the caller.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The backend refused the credentials.
    #[error("the budget store refused the credentials: {source}")]
    Unauthorized {
        /// The underlying cause, kept for logs.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// A conflicting write lost the race beyond what the backend's atomicity can absorb.
    #[error("the budget store lost a conflicting write: {source}")]
    Conflict {
        /// The underlying cause, kept for logs.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// A stored window could not be interpreted.
    ///
    /// A corrupt record must not be silently reset to zero: that would reopen a spent budget.
    #[error("a stored budget window could not be read: {source}")]
    Corrupt {
        /// The underlying cause, kept for logs.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The store was asked for something its configuration cannot serve (an unknown namespace
    /// shape, a policy beyond the backend's bounds).
    #[error("the budget store cannot serve this request: {source}")]
    Invalid {
        /// The underlying cause, kept for logs.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl BudgetStoreError {
    /// Builds an [`Unavailable`] from anything that converts to a boxed cause.
    ///
    /// The one constructor for "the storage did not answer": adapters keep their own wire types
    /// and hand them over typed rather than flattened into prose.
    ///
    /// [`Unavailable`]: BudgetStoreError::Unavailable
    #[must_use]
    pub fn unavailable(cause: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self::Unavailable {
            source: cause.into(),
        }
    }
}

/// The duration a wait may be bounded by, re-exported so adapters state their timeouts in the
/// contract's own vocabulary without each re-deriving it.
pub type StoreTimeout = Duration;

/// Hashes `(route, kind, quota identity)` into the store's fixed-width key.
///
/// The kind tag prevents a tenant label from colliding semantically with an untenantized subject
/// carrying the same bytes. The route remains part of the key, so a tenant spends separate windows
/// on separate routes.
#[must_use]
pub fn budget_key(route_id: &str, identity: &str, tenant: bool) -> BudgetKey {
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

/// The request path's one admission decision against the identity's route budget.
///
/// Three conditions skip the check, each with its own reason: the route declares no budget (most
/// routes), the request has no authenticated identity (a deployment that wants full coverage puts
/// an auth plugin on the route), and the store allows the request. A store error is returned as
/// its own fact — the caller decides fail-open or fail-closed — and never folded into a refusal.
pub(crate) async fn check_request(
    store: &dyn BudgetStore,
    ctx: &crate::context::HttpContext,
    now: SystemTime,
) -> Result<(), Result<crate::error::HttpError, BudgetStoreError>> {
    let Some(policy) = ctx.token_budget().map(|budget| BudgetPolicy {
        limit_tokens: budget.limit_tokens(),
        window_seconds: budget.window_seconds(),
    }) else {
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
    match store.check(&key, &policy, now).await {
        Ok(CheckResult::Allowed) => Ok(()),
        Ok(CheckResult::Exceeded(exceeded)) => {
            Err(Ok(crate::error::HttpError::TokenBudgetExceeded {
                limit: exceeded.limit,
                window_seconds: exceeded.window_seconds,
            }))
        }
        Err(error) => Err(Err(error)),
    }
}

/// The request path's one recording of a served exchange's usage.
///
/// Only `input` and `output` count: the two full-price buckets a bill is made of. The discount
/// buckets (cache reads and writes) are deliberately excluded, so the budget tracks what the caller
/// is actually driving up. A request with no reported usage charges nothing, and one without an
/// identity has no window to charge.
///
/// The charge carries `id` so a retried write bills once; `now` is the wall-clock completion the
/// exchange produced, which a shared backend can compare with every other replica's write.
pub(crate) async fn charge_request(
    store: &dyn BudgetStore,
    ctx: &crate::context::HttpContext,
    id: ChargeId,
    input: u64,
    output: u64,
    now: SystemTime,
) -> Result<(), BudgetStoreError> {
    let Some(policy) = ctx.token_budget().map(|budget| BudgetPolicy {
        limit_tokens: budget.limit_tokens(),
        window_seconds: budget.window_seconds(),
    }) else {
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
    store
        .charge(
            &key,
            &policy,
            &Charge {
                id,
                input,
                output,
                at: now,
            },
        )
        .await
}

#[cfg(test)]
mod tests;
