//! Fallback-chain reference rules: members exist, are unique, and never re-list the target.
//!
//! The chain is the order the gateway tries after its target fails, so these tests pin the three
//! meanings an operator can express with it: who may be named, how often, and what the head is.

use super::*;

/// A provider route with a fallback chain, whose members the document declares.
fn provider_route(fallback: &str) -> String {
    format!(
        concat!(
            "  - id: llm-chat\n",
            "    path_prefix: /v1/chat/\n",
            "    provider: openai-primary\n",
            "    protocol: openai.chat\n",
            "{}"
        ),
        fallback
    )
}

#[test]
fn an_unknown_fallback_member_is_rejected() {
    let document = document(
        PROVIDER,
        NO_ENTRIES,
        &provider_route("    fallback: [missing-provider]\n"),
    );
    let error = parse(&document).expect_err("unknown fallback member");
    assert!(
        matches!(
            error,
            DocumentError::ReferenceUnknown {
                kind: "provider",
                ref name
            } if name == "missing-provider"
        ),
        "expected ReferenceUnknown for the member, got {error:?}"
    );
    assert_eq!(error.category(), ErrorCategory::Client);
}

#[test]
fn a_fallback_member_listed_twice_is_rejected() {
    let document = document(
        format!("{PROVIDER}{SECOND_PROVIDER}").as_str(),
        NO_ENTRIES,
        &provider_route("    fallback: [openai-secondary, openai-secondary]\n"),
    );
    let error = parse(&document).expect_err("a chain cannot visit one member twice");
    assert!(
        matches!(
            error,
            DocumentError::DuplicateId {
                kind: "route-fallback",
                ref id
            } if id == "openai-secondary"
        ),
        "expected DuplicateId for the repeated member, got {error:?}"
    );
}

#[test]
fn a_fallback_that_repeats_the_target_is_rejected() {
    // The target is the chain's head by construction: re-listing it asks the gateway to try the
    // provider it just gave up on, which is not a meaning an operator could have asked for.
    let document = document(
        format!("{PROVIDER}{SECOND_PROVIDER}").as_str(),
        NO_ENTRIES,
        &provider_route("    fallback: [openai-primary, openai-secondary]\n"),
    );
    let error = parse(&document).expect_err("the target is not its own fallback");
    assert!(
        matches!(
            error,
            DocumentError::DuplicateId {
                kind: "route-fallback",
                ref id
            } if id == "openai-primary"
        ),
        "expected DuplicateId for the re-listed target, got {error:?}"
    );
}

#[test]
fn a_fallback_on_an_upstream_route_is_rejected() {
    // An upstream route forwards bytes without classifying a failure, so it has nothing to fall
    // back from; ignoring the chain would publish a route that lies about its own resilience.
    let document = document(
        NO_ENTRIES,
        UPSTREAM,
        concat!(
            "  - id: api-public-v1\n",
            "    path_prefix: /api/\n",
            "    upstream: api-backend\n",
            "    fallback: [openai-primary]\n",
        ),
    );
    let error = parse(&document).expect_err("an upstream route owns no chain");
    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "routes[].fallback",
                ..
            }
        ),
        "expected InvalidField for routes[].fallback, got {error:?}"
    );
}

#[test]
fn a_retry_budget_on_an_upstream_route_bounds_its_member_walk() {
    let document = document(
        NO_ENTRIES,
        UPSTREAM,
        concat!(
            "  - id: api-public-v1\n",
            "    path_prefix: /api/\n",
            "    upstream: api-backend\n",
            "    retry_budget: 2\n",
        ),
    );
    let document = parse(&document).expect("the failover budget is an upstream route's own field");
    let route = &document.routes()[0];
    assert_eq!(
        route.retry_budget(),
        2,
        "the budget bounds the members one request may walk, including the first"
    );
}

#[test]
fn a_declared_chain_parses_with_its_members_in_document_order() {
    let document = document(
        format!("{PROVIDER}{SECOND_PROVIDER}").as_str(),
        NO_ENTRIES,
        &provider_route("    retry_budget: 2\n    fallback: [openai-secondary]\n"),
    );
    let document = parse(&document).expect("the chain resolves");
    let route = &document.routes()[0];
    assert_eq!(route.fallback(), ["openai-secondary"]);
    assert_eq!(route.retry_budget(), 2);
}
