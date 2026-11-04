use std::time::{Duration, Instant};

use nico_core::{Generation, RouteTarget, Scheme};
use pingora::http::Method;

use super::HttpContext;
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
    ctx.begin(
        &snapshot,
        route,
        &request_to("example.test", "/api/items"),
        now,
    );
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
    assert!(ctx.body_limit().is_none());
    assert_eq!(ctx.body_bytes(), 0);
    assert!(ctx.error().is_none());
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
        route,
        &request_to("example.test", "/api/items?page=2"),
        Instant::now(),
    );

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
    assert!(!ctx.body_over_limit(), "300 bytes is under the 512 limit");
}

#[test]
fn a_body_exactly_at_the_limit_is_not_over_it() {
    let (mut ctx, _) = begin(TIGHT_ROUTE);

    ctx.count_body(512);

    assert!(!ctx.body_over_limit());
}

#[test]
fn a_body_one_byte_over_the_limit_is_over_it() {
    let (mut ctx, _) = begin(TIGHT_ROUTE);

    ctx.count_body(513);

    assert!(ctx.body_over_limit());
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
