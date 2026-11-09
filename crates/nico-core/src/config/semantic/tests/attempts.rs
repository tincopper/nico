//! Range tests for the route attempt fields: the retry budget a target may absorb.
//!
//! The budget is materialized to `1` when the document writes nothing, so the range's job is only to
//! refuse values that would mean more attempts than a route can hold in flight.

use super::*;

/// A provider route that may declare attempt fields, with `extra` appended to it.
fn provider_route(extra: &str) -> String {
    format!(
        concat!(
            "  - id: llm-chat\n",
            "    path_prefix: /v1/chat/\n",
            "    provider: openai-primary\n",
            "    protocol: openai.chat\n",
            "{}"
        ),
        extra
    )
}

#[test]
fn a_retry_budget_of_zero_is_rejected() {
    let routes = provider_route("    retry_budget: 0\n");
    let error = with_routes(&routes).expect_err("no attempts cannot serve a request");
    assert!(
        matches!(
            error,
            DocumentError::LimitOutOfRange {
                field: "routes[].retry_budget",
                value: 0
            }
        ),
        "expected LimitOutOfRange for routes[].retry_budget, got {error:?}"
    );
    assert_eq!(error.category(), ErrorCategory::Limit);
}

#[test]
fn a_retry_budget_above_four_is_rejected() {
    let routes = provider_route("    retry_budget: 5\n");
    let error = with_routes(&routes).expect_err("the budget is a memory posture, not a knob");
    assert!(
        matches!(
            error,
            DocumentError::LimitOutOfRange {
                field: "routes[].retry_budget",
                value: 5
            }
        ),
        "expected LimitOutOfRange for routes[].retry_budget, got {error:?}"
    );
}

#[test]
fn the_default_budget_is_legal_and_so_is_the_ceiling() {
    for written in ["", "    retry_budget: 1\n", "    retry_budget: 4\n"] {
        let routes = provider_route(written);
        parse(&document(PROVIDER, UPSTREAM, &routes))
            .unwrap_or_else(|error| panic!("a budget of `{written}` is legal: {error:?}"));
    }
}
