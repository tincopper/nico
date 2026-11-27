//! The backend-facing seam: the atomic operations every storage adapter must perform.
//!
//! One window is one record: the total spent, when the window opened, and the charge ids already
//! recorded (bounded — see [`WindowRecord`]). An adapter translates [`apply_check`] and
//! [`apply_charge`] onto its backend's atomic primitive — an etcd transaction, a Redis script —
//! so the read-modify-write happens inside the storage and two replicas cannot interleave half
//! of it.
//!
//! The functions here are the *semantics*, deliberately pure: they take the record currently
//! stored (or none), the operation's inputs, and return the record to store plus the operation's
//! verdict. An adapter wraps them in its wire call; the fake runs them directly against a map.
//! Sharing the semantics in one place is what lets the contract suite prove all backends agree.

use std::time::{Duration, SystemTime};

use nico_http::{BudgetPolicy, ChargeId};

/// How many charge ids one window record keeps for deduplication.
///
/// The ids exist to absorb retries of *recent* charges — the only window in which a network can
/// make a caller unsure whether a write landed. A window's full history is not a budget fact, so
/// the record keeps the most recent ids and forgets older ones; a retry of a forgotten charge is
/// a charge, which matches the bounded at-least-once contract.
pub const RETAINED_CHARGE_IDS: usize = 64;

/// One stored window: the spend, the opening, and the recent charge ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowRecord {
    /// The total full-price tokens recorded in this window.
    pub used: u64,
    /// When this window opened (its first charge's time).
    pub started: SystemTime,
    /// The most recent charge ids, newest last, for retry deduplication.
    pub charges: Vec<ChargeId>,
}

/// A stored record that could not be interpreted.
#[derive(Debug, thiserror::Error)]
#[error("the stored window record is not one this build can read: {reason}")]
pub struct CorruptRecord {
    /// Why the record was refused. Names the shape, never a key or an identity.
    pub reason: &'static str,
}

/// The verdict of one atomic admission check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckOutcome {
    /// The window has room; store the (possibly reset) record and allow.
    Allowed,
    /// The window is spent; store nothing new and refuse.
    Spent,
}

/// The result of applying a check's semantics to the currently stored record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckApplied {
    /// The verdict.
    pub outcome: CheckOutcome,
    /// The record to store. `None` when the verdict is [`CheckOutcome::Spent`] and nothing about
    /// the stored record changed (an expired window is reset only by a charge or an allowed
    /// check, which touches it).
    pub stored: Option<WindowRecord>,
}

/// Applies the admission-check semantics: expiry, reset-if-expired, limit comparison.
///
/// A stored window whose duration has passed is treated as absent. An absent window (or an
/// expired one) admits with a fresh empty record, which the caller stores so the next replica
/// sees it. A live window compares its spend against the limit and admits only with room.
///
/// # Errors
/// Returns [`CorruptRecord`] when `stored` cannot be interpreted; the caller decides what a
/// corrupt window means (fail-closed, never a silent reset).
pub fn apply_check(
    stored: Option<WindowRecord>,
    policy: &BudgetPolicy,
    now: SystemTime,
) -> Result<CheckApplied, CorruptRecord> {
    let window = match stored {
        None => None,
        Some(record) if expired(&record, policy, now) => None,
        Some(record) => Some(record),
    };
    match window {
        None => Ok(CheckApplied {
            outcome: CheckOutcome::Allowed,
            stored: Some(WindowRecord {
                used: 0,
                started: now,
                charges: Vec::new(),
            }),
        }),
        Some(record) => {
            if record.used >= policy.limit_tokens {
                Ok(CheckApplied {
                    outcome: CheckOutcome::Spent,
                    stored: None,
                })
            } else {
                Ok(CheckApplied {
                    outcome: CheckOutcome::Allowed,
                    stored: Some(record),
                })
            }
        }
    }
}

/// The result of applying a charge's semantics to the currently stored record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChargeApplied {
    /// The record to store: deduped, accumulated, TTL-refreshed.
    pub stored: WindowRecord,
    /// Whether this charge was new (`true`) or a retry of an already-recorded id (`false`).
    /// The caller records the difference as a metric: a high retry rate is an operation signal.
    pub first_write: bool,
}

/// Applies the charge semantics: dedupe on the charge id, expiry/reset, accumulate, retain ids.
///
/// A charge against an expired or absent window opens a fresh one and records the full amount —
/// the spend already happened at the provider, whatever the window boundary did while the
/// request was in flight. A duplicate id records nothing new and reports [`ChargeApplied::first_write']
/// as `false`, which is what makes a retried write bill once.
///
/// Recording is never refused for ending above the limit: post-hoc accounting records what the
/// provider served; what the total refuses is the next admission.
///
/// # Errors
/// Returns [`CorruptRecord`] when `stored` cannot be interpreted.
pub fn apply_charge(
    stored: Option<WindowRecord>,
    policy: &BudgetPolicy,
    charge_id: &ChargeId,
    tokens: u64,
    now: SystemTime,
) -> Result<ChargeApplied, CorruptRecord> {
    let fresh = WindowRecord {
        used: 0,
        started: now,
        charges: Vec::new(),
    };
    let record = match stored {
        None => fresh,
        Some(record) if expired(&record, policy, now) => fresh,
        Some(record) => record,
    };

    if record
        .charges
        .iter()
        .any(|recorded| recorded.0.as_ref() == charge_id.0.as_ref())
    {
        return Ok(ChargeApplied {
            stored: record,
            first_write: false,
        });
    }

    let mut charges = record.charges;
    charges.push(charge_id.clone());
    if charges.len() > RETAINED_CHARGE_IDS {
        let excess = charges.len() - RETAINED_CHARGE_IDS;
        charges.drain(..excess);
    }
    Ok(ChargeApplied {
        stored: WindowRecord {
            used: record.used.saturating_add(tokens),
            started: record.started,
            charges,
        },
        first_write: true,
    })
}

/// Returns whether the record's window has run past `policy`'s duration at `now`.
fn expired(record: &WindowRecord, policy: &BudgetPolicy, now: SystemTime) -> bool {
    let elapsed = now.duration_since(record.started).unwrap_or(Duration::ZERO);
    elapsed >= Duration::from_secs(u64::from(policy.window_seconds))
}

/// The wire-level failure an adapter folds its transport errors into before they cross into the
/// domain vocabulary. One variant per backend decision; the adapter keeps its own cause typed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BackendError {
    /// The backend could not be reached, or the operation timed out.
    #[error("the backend could not be reached: {0}")]
    Unavailable(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The backend refused the credentials.
    #[error("the backend refused the credentials: {0}")]
    Unauthorized(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The operation could not complete atomically and the backend refused to guess.
    #[error("the backend lost a conflicting write: {0}")]
    Conflict(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// A stored record could not be interpreted.
    #[error("the backend returned an unreadable record: {0}")]
    Corrupt(#[from] CorruptRecord),
}

impl BackendError {
    /// Folds any backend failure into the domain's [`BudgetStoreError`] vocabulary.
    ///
    /// [`BudgetStoreError`]: nico_http::BudgetStoreError
    #[must_use]
    pub fn into_domain(self) -> nico_http::BudgetStoreError {
        match self {
            Self::Unavailable(source) => nico_http::BudgetStoreError::Unavailable { source },
            Self::Unauthorized(source) => nico_http::BudgetStoreError::Unauthorized { source },
            Self::Conflict(source) => nico_http::BudgetStoreError::Conflict { source },
            Self::Corrupt(error) => nico_http::BudgetStoreError::Corrupt {
                source: Box::new(error),
            },
        }
    }
}

#[cfg(test)]
mod tests;
