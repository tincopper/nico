//! The shape of an [`Upstream`](super::Upstream) as the document model produces it.
//!
//! Everything here is about the single/multi-member split: a document that writes no `endpoints` has
//! one member, and the parser expands it, so the assertions live next to the expansion rules they
//! describe rather than inside the validation tests.

use super::*;
use crate::config::{
    Document, ParseLimits,
    test_support::{NO_ENTRIES, UPSTREAM, document},
};

/// Parses a document whose only upstream is `upstreams`.
fn parse(upstreams: &str) -> Document {
    Document::parse(
        document(NO_ENTRIES, upstreams, NO_ENTRIES).as_bytes(),
        ParseLimits::new(64 * 1024),
    )
    .expect("the fixture is a valid document")
}

const EXPLICIT_MEMBERS: &str = concat!(
    "  - name: api-backend\n",
    "    scheme: http\n",
    "    host: api.example.test\n",
    "    port: 8080\n",
    "    strategy: least_conn\n",
    "    endpoints:\n",
    "      - name: one\n",
    "        address: 10.0.0.1\n",
    "      - name: two\n",
    "        address: 10.0.0.2\n",
    "        port: 9090\n",
    "        weight: 3\n",
);

#[test]
fn a_single_instance_entry_expands_into_one_named_member() {
    let document = parse(UPSTREAM);
    let upstream = &document.upstreams()[0];

    assert_eq!(
        upstream.endpoints().len(),
        1,
        "a document that writes no `endpoints` still has one member"
    );
    let member = &upstream.endpoints()[0];
    assert_eq!(member.name(), DEFAULT_ENDPOINT_NAME);
    assert_eq!(
        member.address(),
        upstream.host(),
        "the one member connects to the logical host, so a single-address document keeps working"
    );
    assert_eq!(member.port(), upstream.port());
    assert_eq!(member.weight(), DEFAULT_ENDPOINT_WEIGHT);
    assert_eq!(upstream.strategy(), Strategy::RoundRobin);
}

#[test]
fn an_explicit_member_list_is_kept_in_document_order_with_the_upstream_defaults() {
    let document = parse(EXPLICIT_MEMBERS);
    let upstream = &document.upstreams()[0];
    let members = upstream.endpoints();

    assert_eq!(members.len(), 2);
    assert_eq!(members[0].name(), "one");
    assert_eq!(members[0].address(), "10.0.0.1");
    assert_eq!(
        members[0].port(),
        upstream.port(),
        "a member without a port inherits the upstream's"
    );
    assert_eq!(members[0].weight(), DEFAULT_ENDPOINT_WEIGHT);
    assert_eq!(members[1].name(), "two");
    assert_eq!(members[1].port(), 9090);
    assert_eq!(members[1].weight(), 3);
    assert_eq!(
        upstream.host(),
        "api.example.test",
        "the logical host is not a member address"
    );
    assert_eq!(upstream.strategy(), Strategy::LeastConn);
}

#[test]
fn every_strategy_spelling_is_one_strategy() {
    for (spelling, expected) in [
        ("round_robin", Strategy::RoundRobin),
        ("weighted_round_robin", Strategy::WeightedRoundRobin),
        ("least_conn", Strategy::LeastConn),
        ("ip_hash", Strategy::IpHash),
    ] {
        let entry = format!("{UPSTREAM}    strategy: {spelling}\n");
        assert_eq!(
            parse(&entry).upstreams()[0].strategy(),
            expected,
            "`{spelling}` must name {expected:?}"
        );
    }
}
