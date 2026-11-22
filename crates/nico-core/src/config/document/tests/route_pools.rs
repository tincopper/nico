//! Tests for a traffic route's `upstreams` pool: the schema, the reference rules, and the weight
//! range.
//!
//! The rules mirror a provider route's `providers` pool, with the two differences the domains force:
//! the pool is refused on a converting route (that domain pools providers), and a member's weight
//! starts at one — a traffic route has one attempt and no fallback chain, so a zero-weight member
//! would be a name without a behaviour.

use super::*;
use crate::config::{
    PoolStrategy,
    test_support::{PROVIDER, SECOND_UPSTREAM, UPSTREAM, document},
};

/// A traffic route that distributes across the fixture's two upstreams, with `extra` appended.
fn pooled_route(extra: &str) -> String {
    format!(
        concat!(
            "  - id: api-canary\n",
            "    path_prefix: /\n",
            "    upstreams:\n",
            "      - upstream: api-backend\n",
            "        weight: 9\n",
            "      - upstream: api-backend-two\n",
            "        weight: 1\n",
            "{}",
        ),
        extra
    )
}

/// Parses a document holding the fixture upstreams and one traffic route.
fn parse_route(route: &str) -> Result<Document, DocumentError> {
    let yaml = document(NO_ENTRIES, &format!("{UPSTREAM}{SECOND_UPSTREAM}"), route);
    Document::parse(yaml.as_bytes(), ParseLimits::new(16 * 1024))
}

/// Parses a provider-route document, for the cross-domain refusals.
fn parse_provider_route(route: &str) -> Result<Document, DocumentError> {
    let yaml = document(PROVIDER, &format!("{UPSTREAM}{SECOND_UPSTREAM}"), route);
    Document::parse(yaml.as_bytes(), ParseLimits::new(16 * 1024))
}

#[test]
fn a_pooled_route_carries_the_pool_and_its_first_member_as_target() {
    let document = parse_route(&pooled_route("")).expect("a complete pool is valid");
    let route = &document.routes()[0];

    let pool = route.upstream_pool().expect("the pool is carried");
    let names: Vec<&str> = pool.members().iter().map(|m| m.upstream()).collect();
    assert_eq!(names, ["api-backend", "api-backend-two"]);
    let weights: Vec<u16> = pool.members().iter().map(|m| m.weight()).collect();
    assert_eq!(weights, [9, 1]);
    assert_eq!(pool.strategy(), PoolStrategy::WeightedRandom);
    assert_eq!(
        route.target(),
        &RouteTarget::Upstream("api-backend".to_owned()),
        "the route's target is the pool's first member: rules that read a target keep their meaning"
    );
}

#[test]
fn member_weights_default_to_one() {
    let route = concat!(
        "  - id: api-canary\n",
        "    path_prefix: /\n",
        "    upstreams:\n",
        "      - upstream: api-backend\n",
        "      - upstream: api-backend-two\n",
    );
    let document = parse_route(route).expect("weights are optional");
    let pool = document.routes()[0]
        .upstream_pool()
        .expect("the pool is carried");
    let weights: Vec<u16> = pool.members().iter().map(|m| m.weight()).collect();
    assert_eq!(weights, [1, 1]);
}

#[test]
fn ip_hash_is_a_legal_pool_strategy() {
    let document = parse_route(&pooled_route("    strategy: ip_hash\n"))
        .expect("the sticky canary is a legal strategy");
    assert_eq!(
        document.routes()[0]
            .upstream_pool()
            .expect("the pool is carried")
            .strategy(),
        PoolStrategy::IpHash
    );
}

#[test]
fn a_pool_and_a_single_upstream_are_ambiguous() {
    let body = format!("{}    upstream: api-backend\n", pooled_route(""));
    let error = parse_route(&body).expect_err("two ways to name the target");

    assert!(
        matches!(error, DocumentError::RouteTargetAmbiguous { .. }),
        "expected RouteTargetAmbiguous, got {error:?}"
    );
}

#[test]
fn a_pool_on_a_converting_route_is_refused() {
    let body = concat!(
        "  - id: llm-chat\n",
        "    path_prefix: /v1/chat/\n",
        "    provider: openai-primary\n",
        "    protocol: openai.chat\n",
        "    upstreams:\n",
        "      - upstream: api-backend\n",
        "        weight: 1\n",
    );
    let error = parse_provider_route(body).expect_err("an upstream pool is traffic vocabulary");

    assert!(
        matches!(error, DocumentError::RouteTargetAmbiguous { .. }),
        "`upstreams` beside `provider` is two ways to name a target, got {error:?}"
    );
}

#[test]
fn an_empty_pool_is_refused() {
    let body = concat!(
        "  - id: api-canary\n",
        "    path_prefix: /\n",
        "    upstreams: []\n",
    );
    let error = parse_route(body).expect_err("a pool with no member is a route with no target");

    assert!(matches!(error, DocumentError::PoolEmpty { .. }));
}

#[test]
fn a_duplicate_pool_member_is_refused() {
    let body = concat!(
        "  - id: api-canary\n",
        "    path_prefix: /\n",
        "    upstreams:\n",
        "      - upstream: api-backend\n",
        "        weight: 1\n",
        "      - upstream: api-backend\n",
        "        weight: 1\n",
    );
    let error = parse_route(body).expect_err("one machine counted twice splits its failures");

    assert!(
        matches!(
            error,
            DocumentError::DuplicateId {
                kind: "route-upstream-pool-member",
                ..
            }
        ),
        "expected DuplicateId for the pool member, got {error:?}"
    );
}

#[test]
fn a_zero_weight_member_is_refused() {
    let body = concat!(
        "  - id: api-canary\n",
        "    path_prefix: /\n",
        "    upstreams:\n",
        "      - upstream: api-backend\n",
        "        weight: 0\n",
    );
    let error = parse_route(body).expect_err(
        "a traffic route has one attempt: a member that can never serve is not a weight",
    );

    assert!(
        matches!(
            error,
            DocumentError::LimitOutOfRange {
                field: "routes[].upstreams[].weight",
                ..
            }
        ),
        "expected LimitOutOfRange naming the member's weight, got {error:?}"
    );
}

#[test]
fn an_oversized_weight_member_is_refused() {
    let body = concat!(
        "  - id: api-canary\n",
        "    path_prefix: /\n",
        "    upstreams:\n",
        "      - upstream: api-backend\n",
        "        weight: 1001\n",
    );
    let error = parse_route(body).expect_err("a weight above the ceiling is arithmetic gone wrong");

    assert!(matches!(error, DocumentError::LimitOutOfRange { .. }));
}

#[test]
fn an_undeclared_pool_member_is_refused() {
    let body = concat!(
        "  - id: api-canary\n",
        "    path_prefix: /\n",
        "    upstreams:\n",
        "      - upstream: api-ghost\n",
        "        weight: 1\n",
    );
    let error = parse_route(body).expect_err("a member nobody declared cannot serve anything");

    assert!(matches!(error, DocumentError::ReferenceUnknown { .. }));
}

#[test]
fn a_pool_above_the_member_bound_is_refused() {
    // The default `max_pool_members` is 8, so nine declared-and-referenced members exceed it before
    // any document raises the bound. The members must reference declared upstreams: the reference
    // rules run first, and an undeclared name would fail them instead of reaching this bound.
    let upstreams: String = (0..9)
        .map(|i| {
            format!(
                "  - name: api-{i}\n    scheme: http\n    host: 127.0.0.1\n    port: {}\n",
                8080 + i
            )
        })
        .collect();
    let members: String = (0..9)
        .map(|i| format!("      - upstream: api-{i}\n        weight: 1\n"))
        .collect();
    let route = format!("  - id: api-canary\n    path_prefix: /\n    upstreams:\n{members}");

    let yaml = document(NO_ENTRIES, &upstreams, &route);
    let error = Document::parse(yaml.as_bytes(), ParseLimits::new(16 * 1024))
        .expect_err("nine members exceed the default bound of eight");

    assert!(
        matches!(
            error,
            DocumentError::TooMany {
                kind: "routes[].upstreams",
                ..
            }
        ),
        "expected TooMany for the upstream pool, got {error:?}"
    );
}
