//! Pool rules: how `providers` relates to `provider`, and what a member must be.
//!
//! The pool is the one place a route names more than one provider, so the tests here state the
//! decisions a document can get wrong: naming two target kinds at once, naming a member nobody
//! declared, repeating a member, and setting a weight outside the range. Each case asserts the error
//! variant rather than a message, because the variant is what a caller matches on.

use super::*;
use crate::{
    config::{
        bounds::{Bound, ParseLimits, SchemaBounds},
        document::Document,
        test_support::{NO_ENTRIES, PROVIDER, SECOND_PROVIDER, document},
    },
    error::ErrorCategory,
};

/// A document with both provider fixtures and a route body `routes`.
fn with_pool(routes: &str) -> String {
    document(&format!("{PROVIDER}{SECOND_PROVIDER}"), NO_ENTRIES, routes)
}

fn parse(rendered: &str) -> Result<Document, DocumentError> {
    Document::parse(rendered.as_bytes(), ParseLimits::new(16 * 1024))
}

/// The route of a document that parsed, for asserting on the resolved pool.
fn route(document: &Document) -> &crate::config::Route {
    document
        .routes()
        .first()
        .expect("the fixture has one route")
}

#[test]
fn a_pool_route_resolves_every_member_in_document_order() {
    let rendered = with_pool(concat!(
        "  - id: llm-chat\n",
        "    path_prefix: /v1/chat/\n",
        "    protocol: openai.chat\n",
        "    providers:\n",
        "      - name: openai-primary\n",
        "        weight: 3\n",
        "      - name: openai-secondary\n",
    ));

    let document = parse(&rendered).expect("the pool is valid");
    let pool = route(&document).pool().expect("the route has a pool");

    assert_eq!(pool.members().len(), 2);
    assert_eq!(pool.members()[0].name(), "openai-primary");
    assert_eq!(pool.members()[0].weight(), 3);
    assert_eq!(
        pool.members()[1].weight(),
        1,
        "an omitted weight is one share"
    );
    assert_eq!(pool.strategy(), crate::config::PoolStrategy::WeightedRandom);
}

#[test]
fn a_pool_route_keeps_its_first_member_as_the_route_target() {
    // Single-target callers still work on a pooled route: the target is the head of the
    // distribution, and the fallback rule that refuses re-listing the target keeps its meaning.
    let rendered = with_pool(concat!(
        "  - id: llm-chat\n",
        "    path_prefix: /v1/chat/\n",
        "    protocol: openai.chat\n",
        "    providers:\n",
        "      - name: openai-primary\n",
        "      - name: openai-secondary\n",
    ));

    let document = parse(&rendered).expect("the pool is valid");

    assert_eq!(
        route(&document).target(),
        &crate::config::RouteTarget::Provider("openai-primary".to_owned())
    );
}

#[test]
fn a_route_naming_a_provider_and_a_pool_is_rejected() {
    let rendered = with_pool(concat!(
        "  - id: llm-chat\n",
        "    path_prefix: /v1/chat/\n",
        "    protocol: openai.chat\n",
        "    provider: openai-primary\n",
        "    providers:\n",
        "      - name: openai-secondary\n",
    ));

    let error = parse(&rendered).expect_err("both are targets");

    assert!(
        matches!(error, DocumentError::RouteTargetAmbiguous { ref id } if id == "llm-chat"),
        "expected RouteTargetAmbiguous, got {error:?}"
    );
    assert_eq!(error.category(), ErrorCategory::Client);
}

#[test]
fn a_route_naming_an_upstream_and_a_pool_is_rejected() {
    let rendered = document(
        &format!("{PROVIDER}{SECOND_PROVIDER}"),
        crate::config::test_support::UPSTREAM,
        concat!(
            "  - id: llm-chat\n",
            "    path_prefix: /v1/chat/\n",
            "    protocol: openai.chat\n",
            "    upstream: api-backend\n",
            "    providers:\n",
            "      - name: openai-primary\n",
        ),
    );

    let error = parse(&rendered).expect_err("both are targets");

    assert!(
        matches!(error, DocumentError::RouteTargetAmbiguous { ref id } if id == "llm-chat"),
        "expected RouteTargetAmbiguous, got {error:?}"
    );
}

#[test]
fn an_empty_pool_is_rejected() {
    let rendered = with_pool(concat!(
        "  - id: llm-chat\n",
        "    path_prefix: /v1/chat/\n",
        "    protocol: openai.chat\n",
        "    providers: []\n",
    ));

    let error = parse(&rendered).expect_err("a pool with no member");

    assert!(
        matches!(error, DocumentError::PoolEmpty { ref id } if id == "llm-chat"),
        "expected PoolEmpty, got {error:?}"
    );
    assert_eq!(error.category(), ErrorCategory::Client);
}

#[test]
fn an_undeclared_pool_member_is_rejected() {
    let rendered = with_pool(concat!(
        "  - id: llm-chat\n",
        "    path_prefix: /v1/chat/\n",
        "    protocol: openai.chat\n",
        "    providers:\n",
        "      - name: openai-primary\n",
        "      - name: nobody-declared-this\n",
    ));

    let error = parse(&rendered).expect_err("unknown member");

    assert!(
        matches!(
            error,
            DocumentError::ReferenceUnknown { kind: "provider", ref name }
                if name == "nobody-declared-this"
        ),
        "expected ReferenceUnknown, got {error:?}"
    );
}

#[test]
fn a_repeated_pool_member_is_rejected() {
    // Two members naming one provider are one service counted twice: its share would be split across
    // two entries, and a failure would be recorded against a member that names the same target.
    let rendered = with_pool(concat!(
        "  - id: llm-chat\n",
        "    path_prefix: /v1/chat/\n",
        "    protocol: openai.chat\n",
        "    providers:\n",
        "      - name: openai-primary\n",
        "      - name: openai-primary\n",
    ));

    let error = parse(&rendered).expect_err("repeated member");

    assert!(
        matches!(
            error,
            DocumentError::DuplicateId { kind: "route-pool-member", ref id }
                if id == "openai-primary"
        ),
        "expected DuplicateId, got {error:?}"
    );
}

#[test]
fn a_zero_weight_member_is_kept_but_never_distributed() {
    // Zero is legal in a provider pool, unlike a traffic-layer endpoint list: the member stays
    // addressable by an explicit fallback, it just does not take part in the distribution.
    let rendered = with_pool(concat!(
        "  - id: llm-chat\n",
        "    path_prefix: /v1/chat/\n",
        "    protocol: openai.chat\n",
        "    providers:\n",
        "      - name: openai-primary\n",
        "        weight: 0\n",
        "      - name: openai-secondary\n",
    ));

    let document = parse(&rendered).expect("zero weight is a legal member");
    let pool = route(&document).pool().expect("the route has a pool");

    assert_eq!(pool.members()[0].weight(), 0);
}

#[test]
fn a_weight_above_the_bound_is_rejected() {
    let rendered = with_pool(concat!(
        "  - id: llm-chat\n",
        "    path_prefix: /v1/chat/\n",
        "    protocol: openai.chat\n",
        "    providers:\n",
        "      - name: openai-primary\n",
        "        weight: 1001\n",
    ));

    let error = parse(&rendered).expect_err("weight out of range");

    assert!(
        matches!(
            error,
            DocumentError::LimitOutOfRange {
                field: "routes[].providers[].weight",
                value: 1001
            }
        ),
        "expected LimitOutOfRange, got {error:?}"
    );
}

#[test]
fn a_pool_strategy_is_read_from_the_document() {
    let rendered = with_pool(concat!(
        "  - id: llm-chat\n",
        "    path_prefix: /v1/chat/\n",
        "    protocol: openai.chat\n",
        "    strategy: round_robin\n",
        "    providers:\n",
        "      - name: openai-primary\n",
        "      - name: openai-secondary\n",
    ));

    let document = parse(&rendered).expect("the pool is valid");

    assert_eq!(
        route(&document)
            .pool()
            .expect("the route has a pool")
            .strategy(),
        crate::config::PoolStrategy::RoundRobin
    );
}

#[test]
fn a_strategy_without_a_pool_is_rejected() {
    // The value would choose among one member, so nothing reads it. Refusing it keeps a typo from
    // looking like configuration.
    let rendered = with_pool(concat!(
        "  - id: llm-chat\n",
        "    path_prefix: /v1/chat/\n",
        "    protocol: openai.chat\n",
        "    strategy: round_robin\n",
        "    provider: openai-primary\n",
    ));

    let error = parse(&rendered).expect_err("strategy without a pool");

    assert!(
        matches!(error, DocumentError::StrategyWithoutPool { ref id } if id == "llm-chat"),
        "expected StrategyWithoutPool, got {error:?}"
    );
}

#[test]
fn a_pool_larger_than_the_member_bound_is_rejected() {
    // The bound is per route and is also a factor of the attempt ceiling, so a document that raises
    // it changes how many attempts the data plane must be sized for.
    let schema = SchemaBounds::default()
        .with(Bound::PoolMembers, 1)
        .expect("one is a legal bound");
    let rendered = with_pool(concat!(
        "  - id: llm-chat\n",
        "    path_prefix: /v1/chat/\n",
        "    protocol: openai.chat\n",
        "    providers:\n",
        "      - name: openai-primary\n",
        "      - name: openai-secondary\n",
    ));

    let error = Document::parse(
        rendered.as_bytes(),
        ParseLimits::new(16 * 1024).with_schema(schema),
    )
    .expect_err("the pool exceeds the bound");

    assert!(
        matches!(
            error,
            DocumentError::TooMany {
                kind: "routes[].providers",
                count: 2,
                limit: 1
            }
        ),
        "expected TooMany, got {error:?}"
    );
}

#[test]
fn a_single_provider_route_has_no_pool() {
    // The compatibility direction: a document written before pools existed resolves to exactly what
    // it did, and the data plane's single-target path is chosen by `pool()` being `None`.
    let rendered = with_pool(concat!(
        "  - id: llm-chat\n",
        "    path_prefix: /v1/chat/\n",
        "    protocol: openai.chat\n",
        "    provider: openai-primary\n",
    ));

    let document = parse(&rendered).expect("the single-target route is unchanged");

    assert!(route(&document).pool().is_none());
}

#[test]
fn the_attempt_ceiling_scales_with_the_pool_bound() {
    // The fuse a data plane installs must cover the document it validated, so the ceiling is derived
    // from that document's bound rather than from the built-in default.
    let raised = SchemaBounds::default()
        .with(Bound::PoolMembers, 16)
        .expect("sixteen is a legal bound");
    let (budget, chain) = (4, 3);

    assert_eq!(
        crate::config::attempt_ceiling(raised),
        budget * 16 * (1 + chain),
        "the ceiling is the product of all three independent bounds"
    );
    assert!(
        crate::config::attempt_ceiling(raised) > crate::config::MAX_ATTEMPTS_PER_ROUTE,
        "raising the pool bound must widen the fuse, not keep the default one"
    );
}
