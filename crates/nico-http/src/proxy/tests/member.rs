//! What one request reports about the member that served it, driven through the hooks' own decisions.
//!
//! The state machine has its own tests next to it (`upstream::pools::tests::health`); these are about
//! the two narrow seams between the machine and the request path:
//!
//! - the status classification — a 5xx counts and everything else clears — which lives in the response
//!   hook because that is the only hook that sees a 5xx at all;
//! - what the request record's `upstream.endpoint` says, and what it must not say.
//!
//! Both are asserted through the published member list rather than through the machine's internals, so
//! a test here fails when the *request path* stops reporting, which is the fact an operator depends on.

use std::time::Instant;

use nico_core::{ConfigDigest, Generation, Revision, RuntimeSnapshot};

use super::*;
use crate::proxy::hooks::response::record_member_answer;

const THREE_MEMBERS: &str = concat!(
    "  - name: api-backend\n",
    "    scheme: http\n",
    "    host: api.example.test\n",
    "    port: 8080\n",
    "    max_fails: 2\n",
    "    endpoints:\n",
    "      - name: one\n",
    "        address: 10.0.0.1\n",
    "      - name: two\n",
    "        address: 10.0.0.2\n",
    "      - name: three\n",
    "        address: 10.0.0.3\n",
);

/// [`THREE_MEMBERS`] whose budget ejects on the first failure, for the all-members-out refusal.
const FRAGILE_MEMBERS: &str = concat!(
    "  - name: api-backend\n",
    "    scheme: http\n",
    "    host: api.example.test\n",
    "    port: 8080\n",
    "    max_fails: 1\n",
    "    endpoints:\n",
    "      - name: one\n",
    "        address: 10.0.0.1\n",
    "      - name: two\n",
    "        address: 10.0.0.2\n",
);

/// A one-generation snapshot whose single upstream is `upstreams`.
fn snapshot_with(upstreams: &str) -> Arc<RuntimeSnapshot> {
    let document = document_with(upstreams, ROUTE);
    Arc::new(RuntimeSnapshot::new(
        Generation::new(1),
        Revision::comparable(1),
        ConfigDigest::of(upstreams.as_bytes()),
        document,
        "test",
    ))
}

/// Starts a context on `upstreams`'s route, which the fixture documents all call `api`.
fn started_on(upstreams: &str) -> (HttpContext, Instant) {
    started(snapshot_with(upstreams), "/api/items")
}

/// Selects the next member of the fixture upstream.
///
/// Each call is one request, so the walk the previous request opened ends here — the same death the
/// production context dies with.
fn select(ctx: &mut HttpContext) -> Destination {
    ctx.end_walk();
    destination(ctx).expect("at least one member is available")
}

/// Selects `requests` times, returning the addresses reached.
fn reachable(ctx: &mut HttpContext, requests: usize) -> Vec<String> {
    (0..requests)
        .map(|_| select(ctx).host().to_owned())
        .collect()
}

/// Selects until the member named `endpoint` is the one this request would report about.
fn select_until(ctx: &mut HttpContext, endpoint: &str) {
    for _ in 0..8 {
        if ctx.upstream_endpoint() == Some(endpoint) {
            return;
        }
        select(ctx);
    }
    panic!("the fixture never selected `{endpoint}`");
}

#[test]
fn a_server_error_counts_against_the_member_and_a_client_error_clears_the_streak() {
    let (mut ctx, now) = started_on(THREE_MEMBERS);

    assert_eq!(select(&mut ctx).host(), "10.0.0.1");
    record_member_answer(&ctx, 503, now);
    // A 404 is the caller's fault: it must not count, and it breaks the streak the 5xx started.
    record_member_answer(&ctx, 404, now);
    assert!(
        reachable(&mut ctx, 3).contains(&"10.0.0.1".to_owned()),
        "the cleared streak means the member is still a candidate"
    );

    // Two 5xx in a row reach the fixture's budget of two.
    select_until(&mut ctx, "one");
    record_member_answer(&ctx, 502, now);
    record_member_answer(&ctx, 500, now);
    let reached = reachable(&mut ctx, 3);
    assert!(
        !reached.contains(&"10.0.0.1".to_owned()),
        "a member that answered 5xx twice must stop receiving requests, reached {reached:?}"
    );
}

#[test]
fn an_upstream_with_every_member_ejected_is_refused_as_unreachable() {
    let (mut ctx, now) = started_on(FRAGILE_MEMBERS);

    for expected in ["10.0.0.1", "10.0.0.2"] {
        assert_eq!(select(&mut ctx).host(), expected);
        record_member_answer(&ctx, 503, now);
    }

    let error = destination(&mut ctx).expect_err("no member may be contacted");

    assert!(
        matches!(error, HttpError::UpstreamFailed { .. }),
        "the refusal is the existing unreachable pair, not a new code, got {error:?}"
    );
    let respond = error.respond().expect("the refusal is renderable");
    assert_eq!(respond.status().as_u16(), 502);
    assert_eq!(respond.code(), "upstream_unreachable");
}

#[test]
fn a_multi_member_upstream_names_the_member_in_the_record() {
    let (mut ctx, _now) = started_on(THREE_MEMBERS);

    select(&mut ctx);

    assert_eq!(
        ctx.upstream_endpoint(),
        Some("one"),
        "a deployment that configured members can tell them apart in the record"
    );
}

#[test]
fn a_single_member_upstream_keeps_the_log_shape_it_had() {
    let (mut ctx, _now) = begin(ROUTE, "/api/items");

    select(&mut ctx);

    assert_eq!(
        ctx.upstream_endpoint(),
        None,
        "the only machine is already named by `upstream`; an absent field is the honest answer"
    );
}

#[test]
fn a_route_that_selected_no_member_reports_nothing() {
    let (mut ctx, _now) = started(provider_snapshot(), "/v1/chat/completions");
    let now = Instant::now();

    // A provider route reaches its peer through the front-end, so there is no member to judge. Both
    // hooks call the recording methods unconditionally, which is why they must be no-ops here.
    ctx.member_answered(now);
    ctx.member_failed(crate::upstream::pools::MemberFailure::Connect, now);

    assert_eq!(ctx.upstream_endpoint(), None);
    assert!(
        destination(&mut ctx).is_err(),
        "and this crate still refuses a provider target no filter resolved"
    );
}
