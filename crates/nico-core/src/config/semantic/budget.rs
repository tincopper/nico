//! Semantic validation of `routes[].token_budget` — the per-identity token quota one route enforces.

use super::{DocumentError, Route};

/// Inclusive upper bound for `routes[].token_budget.limit_tokens`.
///
/// A billion tokens is far past any real budget: the bound exists so a typo cannot express
/// "unlimited" while pretending to be a number.
const MAX_TOKEN_BUDGET: u64 = 1_000_000_000;

/// Inclusive upper bound for `routes[].token_budget.window_seconds`.
///
/// One year. A longer window is a lifetime quota, which an operator expresses by not limiting at
/// all rather than by a number nobody recomputes.
const MAX_TOKEN_BUDGET_WINDOW: u32 = 31_536_000;

/// Checks the budget a converting route declares.
pub(super) fn validate(route: &Route) -> Result<(), DocumentError> {
    let Some(budget) = route.token_budget() else {
        return Ok(());
    };
    if budget.limit_tokens() == 0 || budget.limit_tokens() > MAX_TOKEN_BUDGET {
        return Err(DocumentError::LimitOutOfRange {
            field: "routes[].token_budget.limit_tokens",
            value: budget.limit_tokens(),
        });
    }
    if budget.window_seconds() == 0 || budget.window_seconds() > MAX_TOKEN_BUDGET_WINDOW {
        return Err(DocumentError::LimitOutOfRange {
            field: "routes[].token_budget.window_seconds",
            value: u64::from(budget.window_seconds()),
        });
    }
    Ok(())
}

/// Inclusive upper bound for a route-level `timeout_ms`.
const MAX_TIMEOUT_MS: u32 = 600_000;

/// Inclusive upper bound for `routes[].ttft_timeout_ms`.
///
/// Half the whole-request ceiling on purpose: this budget bounds the wait *before* an answer starts,
/// so a value approaching the request's own deadline would never fire — the request would time out
/// first, and the field would look like configuration that does nothing.
const MAX_TTFT_TIMEOUT_MS: u32 = 300_000;

/// Checks a route-level timeout.
pub(crate) fn validate_timeout(field: &'static str, timeout_ms: u32) -> Result<(), DocumentError> {
    validate_timeout_bound(field, timeout_ms, MAX_TIMEOUT_MS)
}

/// Checks a first-byte budget stays inside its own ceiling.
///
/// The first-byte ceiling is half the whole-request one on purpose: the wait *before* an answer
/// starts cannot approach the request's own deadline without the request timing out first, which
/// would make the field look like configuration that does nothing.
pub(crate) fn validate_ttft_timeout(
    field: &'static str,
    timeout_ms: u32,
) -> Result<(), DocumentError> {
    validate_timeout_bound(field, timeout_ms, MAX_TTFT_TIMEOUT_MS)
}

/// Checks a timeout stays inside its own ceiling.
///
/// Split from [`validate_timeout`] because the two ceilings differ: a whole-request budget and a
/// first-byte budget are not the same posture, and one constant for both would let a document state a
/// first-byte wait longer than the request it belongs to.
pub(crate) fn validate_timeout_bound(
    field: &'static str,
    timeout_ms: u32,
    maximum: u32,
) -> Result<(), DocumentError> {
    if !(1..=maximum).contains(&timeout_ms) {
        return Err(DocumentError::LimitOutOfRange {
            field,
            value: u64::from(timeout_ms),
        });
    }
    Ok(())
}
