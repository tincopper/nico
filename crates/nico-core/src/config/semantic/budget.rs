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
