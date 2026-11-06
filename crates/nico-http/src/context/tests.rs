use std::time::{Duration, Instant};

use nico_core::{Generation, RouteTarget, Scheme};
use pingora::http::Method;

use super::{HttpContext, Identity};
use crate::{
    error::HttpError,
    test_support::{request_to, snapshot},
    upstream::Destination,
};

const ROUTE: &str = concat!(
    "  - id: api\n",
    "    path_prefix: /api\n",
    "    upstream: api-backend\n",
);

const TIGHT_ROUTE: &str = concat!(
    "  - id: tight\n",
    "    path_prefix: /\n",
    "    upstream: api-backend\n",
    "    timeout_ms: 100\n",
    "    max_body_bytes: 512\n",
);

/// Starts a context for `routes`'s only route, returning it with the instant it began at.
fn begin(routes: &str) -> (HttpContext, Instant) {
    let snapshot = snapshot(routes);
    let route = &snapshot.document().routes()[0];
    let now = Instant::now();
    let mut ctx = HttpContext::default();
    ctx.begin(&snapshot, &request_to("example.test", "/api/items"), now);
    ctx.bind_route(route);
    (ctx, now)
}

#[test]
fn a_context_that_never_began_reports_nothing() {
    let ctx = HttpContext::default();

    assert!(ctx.snapshot().is_none());
    assert!(ctx.route_id().is_none());
    assert!(ctx.target().is_none());
    assert!(ctx.upstream().is_none());
    assert!(ctx.method().is_none());
    assert!(ctx.path().is_none());
    assert!(ctx.deadline().is_none());
    assert!(ctx.destination().is_none());
    assert!(ctx.client_ip().is_none());
    assert!(ctx.body_limit().is_none());
    assert!(ctx.response_limit().is_none());
    assert_eq!(ctx.body_bytes(), 0);
    assert!(ctx.error().is_none());
    assert!(ctx.identity().is_none());
    assert!(!ctx.is_cancelled());
    assert_eq!(ctx.trace_id(), "");
}

#[test]
fn begin_records_the_route_the_target_and_the_generation() {
    let (ctx, _) = begin(ROUTE);

    assert_eq!(ctx.route_id(), Some("api"));
    assert_eq!(
        ctx.target(),
        Some(&RouteTarget::Upstream("api-backend".to_owned()))
    );
    assert_eq!(ctx.upstream(), Some("api-backend"));
    assert_eq!(ctx.generation(), Some(Generation::new(1)));
}

#[test]
fn begin_records_the_method_and_the_path_without_its_query() {
    let snapshot = snapshot(ROUTE);
    let route = &snapshot.document().routes()[0];
    let mut ctx = HttpContext::default();

    ctx.begin(
        &snapshot,
        &request_to("example.test", "/api/items?page=2"),
        Instant::now(),
    );
    ctx.bind_route(route);

    assert_eq!(ctx.method(), Some(&Method::GET));
    assert_eq!(ctx.path(), Some("/api/items"));
}

#[test]
fn begin_derives_the_deadline_from_the_document_budget() {
    let (ctx, now) = begin(ROUTE);

    assert_eq!(ctx.deadline(), Some(now + Duration::from_millis(30_000)));
    assert_eq!(ctx.remaining(now), Some(Duration::from_millis(30_000)));
    assert_eq!(
        ctx.remaining(now + Duration::from_millis(10_000)),
        Some(Duration::from_millis(20_000))
    );
}

#[test]
fn a_route_override_tightens_the_body_limit_and_the_deadline() {
    let (ctx, now) = begin(TIGHT_ROUTE);

    assert_eq!(ctx.body_limit(), Some(512));
    assert_eq!(
        ctx.response_limit(),
        Some(nico_core::DEFAULT_MAX_RESPONSE_BYTES),
        "a route that sets no response ceiling inherits the document's"
    );
    assert_eq!(ctx.deadline(), Some(now + Duration::from_millis(100)));
}

#[test]
fn the_remaining_budget_saturates_at_zero_instead_of_going_negative() {
    let (ctx, now) = begin(ROUTE);
    let long_after = now + Duration::from_secs(60);

    assert_eq!(ctx.remaining(long_after), Some(Duration::ZERO));
}

#[test]
fn the_elapsed_time_is_zero_before_the_context_begins() {
    assert_eq!(HttpContext::default().elapsed_ms(Instant::now()), 0);
}

#[test]
fn the_elapsed_time_is_measured_from_the_instant_the_request_began() {
    let (ctx, now) = begin(ROUTE);

    assert_eq!(ctx.elapsed_ms(now + Duration::from_millis(45)), 45);
}

#[test]
fn nothing_decides_the_destination_until_something_does() {
    let (ctx, _) = begin(ROUTE);

    assert!(
        ctx.destination().is_none(),
        "matching a route names a target; it does not resolve it"
    );
}

#[test]
fn a_recorded_destination_is_what_the_request_goes_to() {
    let (mut ctx, _) = begin(ROUTE);
    let destination = Destination::new(Scheme::Http, "10.0.0.9", 9000);

    ctx.set_destination(destination.clone());

    assert_eq!(ctx.destination(), Some(&destination));
}

#[test]
fn an_expired_budget_produces_the_renderable_deadline_failure() {
    let (ctx, now) = begin(ROUTE);
    let late = now + Duration::from_secs(60);

    assert!(ctx.expired(late), "the budget must run out eventually");
    let error = ctx.deadline_error();

    assert!(
        matches!(error, HttpError::DeadlineExceeded { .. }),
        "expected the deadline failure, got {error:?}"
    );
    assert_eq!(
        error.respond().expect("renderable").status().as_u16(),
        504,
        "the caller is told the gateway ran out of time"
    );
}

#[test]
fn counted_body_bytes_accumulate_across_chunks() {
    let (mut ctx, _) = begin(TIGHT_ROUTE);

    ctx.count_body(200);
    ctx.count_body(100);

    assert_eq!(ctx.body_bytes(), 300);
    assert_eq!(
        ctx.exceeded_body_limit(),
        None,
        "300 bytes is under the 512 limit"
    );
}

#[test]
fn a_body_exactly_at_the_limit_is_not_over_it() {
    let (mut ctx, _) = begin(TIGHT_ROUTE);

    ctx.count_body(512);

    assert_eq!(ctx.exceeded_body_limit(), None);
}

#[test]
fn a_body_one_byte_over_the_limit_is_over_it() {
    let (mut ctx, _) = begin(TIGHT_ROUTE);

    ctx.count_body(513);

    assert_eq!(
        ctx.exceeded_body_limit(),
        Some(512),
        "the verdict carries the limit it was measured against, so no caller has to look it up again"
    );
}

#[test]
fn the_deadline_guard_returns_the_failure_only_when_the_clock_ran_out() {
    let (ctx, now) = begin(TIGHT_ROUTE);

    assert!(ctx.deadline_failure(now).is_none());
    let error = ctx
        .deadline_failure(now + Duration::from_millis(101))
        .expect("the 100 ms route budget is spent");
    assert!(matches!(error, HttpError::DeadlineExceeded { .. }));
}

#[test]
fn a_zero_body_limit_rejects_the_first_byte() {
    let (mut ctx, _) = begin(concat!(
        "  - id: api\n",
        "    path_prefix: /api\n",
        "    upstream: api-backend\n",
        "    max_body_bytes: 0\n",
    ));

    assert_eq!(ctx.exceeded_body_limit(), None, "an empty body is allowed");
    ctx.count_body(1);
    assert_eq!(ctx.exceeded_body_limit(), Some(0), "0 accepts no body");
}

#[test]
fn the_first_plugin_attribution_wins() {
    let (mut ctx, _) = begin(ROUTE);

    assert_eq!(ctx.failed_plugin(), None);
    ctx.set_failed_plugin("edge-firewall");
    ctx.set_failed_plugin("a-later-symptom");
    assert_eq!(ctx.failed_plugin(), Some("edge-firewall"));
}

#[test]
fn the_first_failure_wins_and_is_taken_exactly_once() {
    let (mut ctx, _) = begin(ROUTE);

    ctx.fail(HttpError::BodyTooLarge { limit: 512 });
    ctx.fail(HttpError::RouteNotFound);

    assert!(
        matches!(ctx.error(), Some(HttpError::BodyTooLarge { limit: 512 })),
        "the first failure is the one a caller acts on: {:?}",
        ctx.error()
    );
}

#[test]
fn a_cancelled_request_is_recorded_as_such() {
    let (mut ctx, _) = begin(ROUTE);

    ctx.mark_cancelled();

    assert!(ctx.is_cancelled());
}

#[test]
fn a_trace_id_is_replaced_rather_than_appended_to() {
    let (mut ctx, _) = begin(ROUTE);

    ctx.set_trace_id("first");
    ctx.set_trace_id("second");

    assert_eq!(ctx.trace_id(), "second");
}

mod client_ip {
    use std::net::IpAddr;

    use super::*;

    #[test]
    fn the_peer_address_is_recorded_as_the_transport_reported_it() {
        let (mut ctx, _now) = begin(ROUTE);
        let address: IpAddr = "203.0.113.7"
            .parse()
            .expect("the fixture is an IPv4 address");

        ctx.set_client_ip(address);

        assert_eq!(
            ctx.client_ip(),
            Some(address),
            "a policy that keys on the caller reads the connection's address, never a header"
        );
    }

    #[test]
    fn the_first_address_wins_so_a_filter_cannot_choose_the_bucket() {
        let (mut ctx, _now) = begin(ROUTE);
        let peer: IpAddr = "203.0.113.7"
            .parse()
            .expect("the fixture is an IPv4 address");
        let forged: IpAddr = "198.51.100.9"
            .parse()
            .expect("the fixture is an IPv4 address");

        // The data plane records the peer before the chain runs, so the second write can only be a
        // policy trying to key itself on something the caller supplied.
        ctx.set_client_ip(peer);
        ctx.set_client_ip(forged);

        assert_eq!(
            ctx.client_ip(),
            Some(peer),
            "a rate-limit bucket must be keyed by the connection, not by whatever ran later"
        );
    }
}

mod identity {
    use super::*;

    #[test]
    fn a_context_starts_without_an_identity() {
        let (ctx, _now) = begin(ROUTE);

        assert!(
            ctx.identity().is_none(),
            "not being authenticated is a fact of its own, not an empty subject"
        );
    }

    #[test]
    fn an_identity_records_the_subject_and_the_filter_that_claimed_it() {
        let (mut ctx, _now) = begin(ROUTE);

        ctx.set_identity(Identity::new("user-42", "public-jwt"));

        let identity = ctx.identity().expect("the identity is recorded");
        assert_eq!(identity.subject(), "user-42");
        assert_eq!(
            identity.source(),
            "public-jwt",
            "the source is what makes a subject attributable in the log"
        );
    }

    #[test]
    fn an_identity_source_can_be_a_configured_plugin_name() {
        let (mut ctx, _now) = begin(ROUTE);
        // A plugin instance name comes from the document, so it is not a `'static` string: two
        // entries of one kind on one route must be distinguishable here, or they would share a
        // rate-limit bucket and one `authn` value.
        let instance = ["public", "jwt"].join("-");

        ctx.set_identity(Identity::new("user-42", instance.clone()));

        let identity = ctx.identity().expect("the identity is recorded");
        assert_eq!(identity.source(), "public-jwt");
        assert_eq!(identity.source(), instance.as_str());
    }

    #[test]
    fn the_first_identity_wins_so_a_later_filter_cannot_change_who_is_charged() {
        let (mut ctx, _now) = begin(ROUTE);

        ctx.set_identity(Identity::new("user-42", "public-jwt"));
        ctx.set_identity(Identity::new("someone-else", "second-filter"));

        assert_eq!(
            ctx.identity().map(Identity::subject),
            Some("user-42"),
            "a policy must not be able to rewrite the subject a rate limit is keyed by"
        );
    }
}
