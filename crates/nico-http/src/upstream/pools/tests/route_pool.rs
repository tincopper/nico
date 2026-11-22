//! The route pool: which upstream one request is sent to.
//!
//! The pool chooses between services; the upstream's member pool chooses between machines, and its
//! rules have their own tests. What is asserted here is the pool's own wiring — the order its
//! strategies produce, the upstreams it excludes from the candidates, and the picker state that
//! survives a reload. Distribution *proportions* belong to `nico-lb`'s pickers, which are tested
//! where they are defined.

use std::{
    net::{IpAddr, Ipv4Addr},
    time::Instant,
};

use super::*;
use crate::upstream::{
    dns::ExpandedMembers,
    pools::{MemberFailure, MemberOutcome},
};

/// Two upstreams with a short ejection budget, so a test can step past a cooldown without
/// arithmetic on the default thirty seconds.
const TWO_UPSTREAMS: &str = concat!(
    "  - name: api-v1\n",
    "    scheme: http\n",
    "    host: api.example.test\n",
    "    port: 8080\n",
    "    max_fails: 2\n",
    "    fail_timeout_ms: 1000\n",
    "    endpoints:\n",
    "      - name: one\n",
    "        address: 10.0.0.1\n",
    "  - name: api-v2\n",
    "    scheme: http\n",
    "    host: api.example.test\n",
    "    port: 8080\n",
    "    max_fails: 2\n",
    "    fail_timeout_ms: 1000\n",
    "    endpoints:\n",
    "      - name: one\n",
    "        address: 10.0.0.2\n",
);

/// A pooled route in document order, one draw per pick.
const ROUND_ROBIN_ROUTE: &str = concat!(
    "  - id: api-canary\n",
    "    path_prefix: /\n",
    "    upstreams:\n",
    "      - upstream: api-v1\n",
    "        weight: 1\n",
    "      - upstream: api-v2\n",
    "        weight: 1\n",
    "    strategy: round_robin\n",
);

/// A pooled route that keys its draw on the caller's address.
const IP_HASH_ROUTE: &str = concat!(
    "  - id: api-canary\n",
    "    path_prefix: /\n",
    "    upstreams:\n",
    "      - upstream: api-v1\n",
    "        weight: 1\n",
    "      - upstream: api-v2\n",
    "        weight: 1\n",
    "    strategy: ip_hash\n",
);

/// A pooled route with the default strategy.
const WEIGHTED_ROUTE: &str = concat!(
    "  - id: api-canary\n",
    "    path_prefix: /\n",
    "    upstreams:\n",
    "      - upstream: api-v1\n",
    "        weight: 1\n",
    "      - upstream: api-v2\n",
    "        weight: 1\n",
);

/// Publishes the fixture with its route pool, once.
fn published(routes: &str) -> Arc<SelectionTable> {
    UpstreamPools::default().publish(
        &document_with(TWO_UPSTREAMS, routes),
        &ExpandedMembers::default(),
    )
}

/// Reports one connection failure against the only member of `upstream`.
fn fail(table: &SelectionTable, upstream: &str, now: Instant) {
    let state = table
        .upstream(upstream)
        .expect("the fixture upstream exists");
    state.members()[0].state().record(
        state.ejection(),
        MemberOutcome::Failed(MemberFailure::Connect),
        now,
    );
}

#[test]
fn a_round_robin_pool_rotates_in_document_order() {
    let table = published(ROUND_ROBIN_ROUTE);
    let pool = table
        .route_pool("api-canary")
        .expect("the pool is published");

    let mut seen = Vec::new();
    for _ in 0..4 {
        let picked = table
            .pick_upstream(pool, None, Instant::now())
            .expect("a candidate exists");
        seen.push(picked.to_string());
    }
    assert_eq!(
        seen,
        ["api-v1", "api-v2", "api-v1", "api-v2"],
        "the members rotate in document order, wrapping"
    );
}

#[test]
fn an_ip_hash_pool_keeps_the_same_caller_on_the_same_upstream() {
    let table = published(IP_HASH_ROUTE);
    let pool = table
        .route_pool("api-canary")
        .expect("the pool is published");
    let caller = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));

    let assigned = table
        .pick_upstream(pool, Some(caller), Instant::now())
        .expect("a candidate exists")
        .to_string();
    for _ in 0..9 {
        let picked = table
            .pick_upstream(pool, Some(caller), Instant::now())
            .expect("a candidate exists");
        assert_eq!(
            picked.to_string(),
            assigned,
            "the same caller meets the same upstream, however many requests it makes"
        );
    }
}

#[test]
fn a_weighted_pool_reaches_every_member_over_a_sample() {
    let table = published(WEIGHTED_ROUTE);
    let pool = table
        .route_pool("api-canary")
        .expect("the pool is published");

    let mut reached = std::collections::BTreeSet::new();
    for _ in 0..40 {
        let picked = table
            .pick_upstream(pool, None, Instant::now())
            .expect("a candidate exists");
        reached.insert(picked.to_string());
    }
    let expected: std::collections::BTreeSet<String> = ["api-v1", "api-v2"]
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    assert_eq!(
        reached, expected,
        "equal weights over forty draws reach both sides: a member that never draws is miswired"
    );
}

#[test]
fn an_upstream_with_every_member_ejected_is_not_a_candidate() {
    let pools = UpstreamPools::default();
    let table = pools.publish(
        &document_with(TWO_UPSTREAMS, ROUND_ROBIN_ROUTE),
        &ExpandedMembers::default(),
    );
    let pool = table
        .route_pool("api-canary")
        .expect("the pool is published");
    let now = Instant::now();

    // `api-v2`'s only member fails its budget: the upstream leaves the candidates, and the whole
    // share lands on the one that can still serve.
    fail(&table, "api-v2", now);
    fail(&table, "api-v2", now);
    for _ in 0..3 {
        let picked = table
            .pick_upstream(pool, None, now)
            .expect("the other upstream can serve");
        assert_eq!(
            picked.to_string(),
            "api-v1",
            "a fully ejected upstream is not a candidate"
        );
    }

    // Both sides down: nothing to select, which the request path answers as its usual refusal.
    fail(&table, "api-v1", now);
    fail(&table, "api-v1", now);
    assert!(
        table.pick_upstream(pool, None, now).is_none(),
        "no upstream in the pool can serve"
    );
}

#[test]
fn a_pool_keeps_its_rotation_across_a_reload() {
    let pools = UpstreamPools::default();
    let document = document_with(TWO_UPSTREAMS, ROUND_ROBIN_ROUTE);
    let first = pools.publish(&document, &ExpandedMembers::default());
    let first_pool = first
        .route_pool("api-canary")
        .expect("the pool is published");
    let first_pick = first
        .pick_upstream(first_pool, None, Instant::now())
        .expect("a candidate exists")
        .to_string();

    // The same route republished keeps the rotation the first generation left: a reload is not a
    // restart, and the picker's state is inherited by route identity the way a member's counters are.
    let second = pools.publish(&document, &ExpandedMembers::default());
    let second_pool = second
        .route_pool("api-canary")
        .expect("the pool is republished");
    let second_pick = second
        .pick_upstream(second_pool, None, Instant::now())
        .expect("a candidate exists")
        .to_string();

    assert_eq!(first_pick, "api-v1");
    assert_eq!(
        second_pick, "api-v2",
        "the draw continues where the previous generation left it"
    );
}
