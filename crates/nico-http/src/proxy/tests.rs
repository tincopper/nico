//! Unit tests for the pure parts of the hook contract.
//!
//! The hooks themselves need a live `Session`, so they are exercised by the integration tests; what
//! can be tested here is the logic they delegate to: which destination a request goes to, the
//! deadline verdict, the classification of Pingora's own errors, the framing guard, and the header
//! forwarding helpers.

use std::{
    net::IpAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use nico_core::{
    ConfigDigest, Document, ErrorCategory, Generation, ParseLimits, Revision, RuntimeSnapshot,
};
use pingora::{Error, ErrorSource, ErrorType, http::RequestHeader};

mod llm;
mod member;

use super::{
    failure::{classify, is_downstream_gone},
    forward::{Framing, destination, forwarded_for},
};
use crate::{
    context::HttpContext,
    error::HttpError,
    test_support::{document_with, request_to, snapshot},
    upstream::{self, Destination, pools::UpstreamPools},
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
);

/// A route whose target this crate does not understand: only a filter can resolve it.
const PROVIDER_ROUTE: &str = concat!(
    "  - id: ai\n",
    "    path_prefix: /v1/\n",
    "    provider: openai-primary\n",
    // The document model requires a client protocol on a provider route; this crate still refuses
    // the target because no filter resolved it.
    "    protocol: openai.chat\n",
);

/// Starts a context for the fixture's route, returning it with the instant it began at.
fn begin(routes: &str, path: &str) -> (HttpContext, Instant) {
    started(snapshot(routes), path)
}

/// Starts a context on `snapshot`'s first route.
fn started(snapshot: Arc<RuntimeSnapshot>, path: &str) -> (HttpContext, Instant) {
    let route = &snapshot.document().routes()[0];
    let now = Instant::now();
    let mut ctx = HttpContext::default();
    ctx.begin(&snapshot, &request_to("example.test", path), now);
    // The publication a request reads also carries the members its upstreams select from; a test
    // context built from a snapshot alone gets the one-member list that document describes.
    ctx.set_selection(UpstreamPools::default().publish(snapshot.document()));
    ctx.bind_route(route);
    (ctx, now)
}

/// Builds a snapshot with one provider and no upstream, so a route may target the provider.
fn provider_snapshot() -> Arc<RuntimeSnapshot> {
    let yaml = format!(
        concat!(
            "apiVersion: gateway/v1\n",
            "request:\n",
            "  max_body_bytes: 1048576\n",
            "  timeout_ms: 30000\n",
            "plugins: []\n",
            "providers:\n",
            "  - name: openai-primary\n",
            "    kind: openai\n",
            "    base_url: https://api.example.test\n",
            "    credential_ref: env:FIXTURE_KEY\n",
            "upstreams: []\n",
            "routes:\n",
            "{}",
        ),
        PROVIDER_ROUTE
    );
    let document = Document::parse(yaml.as_bytes(), ParseLimits::new(64 * 1024))
        .expect("the provider fixture is a valid document");
    Arc::new(RuntimeSnapshot::new(
        Generation::new(1),
        Revision::comparable(1),
        ConfigDigest::of(yaml.as_bytes()),
        document,
        "test",
    ))
}

#[test]
fn a_document_upstream_becomes_the_destination_the_route_names() {
    let (mut ctx, _now) = begin(ROUTE, "/api/items");

    let destination = destination(&mut ctx).expect("the fixture upstream exists");

    assert_eq!(destination.host(), "127.0.0.1");
    assert_eq!(destination.port(), 8080);
}

#[test]
fn a_destination_recorded_by_a_filter_wins_over_the_document() {
    let (mut ctx, _now) = begin(ROUTE, "/api/items");
    let chosen = Destination::new(nico_core::Scheme::Http, "10.0.0.9", 9000);

    // This is the seam `nico-llm` uses: it resolves the provider target the route named.
    ctx.set_destination(chosen.clone());

    assert_eq!(
        destination(&mut ctx).expect("a recorded destination is used"),
        chosen,
        "a policy decision must not be replaced by the fallback it exists to replace"
    );
    assert_eq!(ctx.destination(), Some(&chosen));
}

#[test]
fn a_provider_target_no_filter_resolved_is_refused_rather_than_guessed() {
    let (mut ctx, _now) = started(provider_snapshot(), "/v1/chat/completions");

    let error = destination(&mut ctx).expect_err("this crate does not know provider semantics");

    assert!(
        matches!(&error, HttpError::ProviderNotConfigured { name } if name == "openai-primary"),
        "expected ProviderNotConfigured, got {error:?}"
    );
    let respond = error.respond().expect("the refusal is renderable");
    assert_eq!(respond.status().as_u16(), 502);
    assert_eq!(respond.code(), "provider_unavailable");
    assert_eq!(error.category(), ErrorCategory::Upstream);
}

#[test]
fn an_upstream_with_no_published_member_is_refused_rather_than_guessed() {
    let (mut ctx, _now) = begin(ROUTE, "/api/items");
    // A generation that published no member for the upstream the route names. Unreachable through a
    // document — an empty `endpoints` list is refused — and the answer is still the refusal, not an
    // attempt against an address nothing named.
    ctx.set_selection(UpstreamPools::default().publish(&document_with("  []", "  []")));

    let error = destination(&mut ctx).expect_err("nothing is available to select");

    assert!(
        matches!(error, HttpError::UpstreamFailed { .. }),
        "expected UpstreamFailed, got {error:?}"
    );
    let respond = error.respond().expect("the refusal is renderable");
    assert_eq!(respond.status().as_u16(), 502);
    assert_eq!(respond.code(), "upstream_unreachable");
}

#[test]
fn a_context_that_never_began_has_no_destination() {
    let error = destination(&mut HttpContext::default()).expect_err("nothing is routable");

    assert!(matches!(error, HttpError::NotReady));
}

#[tokio::test(flavor = "current_thread")]
async fn a_matched_upstream_becomes_a_peer_with_the_remaining_budget() {
    let (mut ctx, now) = begin(ROUTE, "/api/items");
    let destination = destination(&mut ctx).expect("the fixture upstream exists");
    let budget = ctx.remaining(now).expect("the request has a budget");

    let peer = upstream::peer(&destination, budget)
        .await
        .expect("127.0.0.1 resolves");

    assert!(
        peer.to_string().contains("127.0.0.1:8080"),
        "the peer must carry the document's address: {peer}"
    );
    assert_eq!(
        peer.options.connection_timeout,
        Some(Duration::from_millis(30_000)),
        "the connect budget is the request's remaining time"
    );
    assert_eq!(
        peer.options.read_timeout,
        Some(Duration::from_millis(30_000)),
        "a slow-dripping upstream may not outlive the request"
    );
}

#[test]
fn a_context_that_never_began_has_no_deadline() {
    assert!(
        !HttpContext::default().expired(Instant::now()),
        "an unrouted request is rejected by routing, not by a clock"
    );
}

#[test]
fn a_fresh_request_has_not_spent_its_budget() {
    let (ctx, now) = begin(TIGHT_ROUTE, "/");

    assert!(!ctx.expired(now));
    // The same context, observed after the budget: this is the only clock the data plane trusts.
    assert!(
        ctx.remaining(now + Duration::from_millis(101))
            .is_some_and(|left| left.is_zero())
    );
    assert!(ctx.expired(now + Duration::from_millis(101)));
}

#[test]
fn an_upstream_error_is_classified_as_an_upstream_failure() {
    let error = Error::new_up(ErrorType::ConnectRefused);

    let classified =
        classify(&HttpContext::default(), &error).expect("an upstream failure is renderable");

    assert!(matches!(classified, HttpError::UpstreamFailed { .. }));
}

#[test]
fn a_peer_that_runs_out_of_the_request_budget_is_a_gateway_timeout() {
    let (ctx, _now) = begin(TIGHT_ROUTE, "/");

    // Pingora reports a peer timeout with an upstream source, which the source alone would map to
    // "unreachable" — the wrong answer for a peer that accepted the request and then went quiet.
    for etype in [
        ErrorType::ReadTimedout,
        ErrorType::WriteTimedout,
        ErrorType::ConnectTimedout,
    ] {
        let classified =
            classify(&ctx, &Error::new_up(etype)).expect("a peer timeout is renderable");
        let respond = classified.respond().expect("a peer timeout is renderable");
        assert_eq!(classified.category(), ErrorCategory::Upstream);
        assert_eq!(
            respond.status().as_u16(),
            504,
            "a peer timeout must be a gateway timeout, not an unreachable peer"
        );
        assert_eq!(respond.code(), "upstream_timeout");
    }
}

#[test]
fn a_downstream_disconnect_has_no_response_to_write() {
    let error = Error::new_down(ErrorType::ConnectionClosed);

    assert!(
        classify(&HttpContext::default(), &error).is_none(),
        "there is nobody to answer"
    );
    assert!(is_downstream_gone(&error));
}

#[test]
fn a_malformed_request_is_classified_as_a_client_error() {
    let error = Error::new_down(ErrorType::InvalidHTTPHeader);

    let classified =
        classify(&HttpContext::default(), &error).expect("a malformed request is answerable");

    assert!(matches!(classified, HttpError::InvalidRequest { .. }));
    assert_eq!(classified.category(), ErrorCategory::Client);
}

#[test]
fn an_internal_error_is_classified_as_an_internal_failure() {
    let error = Error::new_in(ErrorType::InternalError);

    let classified =
        classify(&HttpContext::default(), &error).expect("an internal failure is answerable");

    assert!(matches!(classified, HttpError::Internal { .. }));
    assert_eq!(classified.category(), ErrorCategory::Internal);
}

#[test]
fn an_error_with_no_source_is_not_reported_as_a_client_fault() {
    let classified = classify(
        &HttpContext::default(),
        &Error::new(ErrorType::UnknownError),
    )
    .expect("an unclassified failure still needs a response");

    assert_eq!(classified.category(), ErrorCategory::Internal);
}

#[test]
fn a_downstream_timeout_counts_as_the_client_leaving() {
    for etype in [ErrorType::ReadTimedout, ErrorType::WriteTimedout] {
        assert!(is_downstream_gone(&Error::new_down(etype.clone())));
    }
}

#[test]
fn the_forwarded_for_chain_appends_the_client_address() {
    let request = request_with(&[("x-forwarded-for", "203.0.113.7")]);
    let client = IpAddr::from([10, 0, 0, 1]);

    assert_eq!(
        forwarded_for(&request, Some(client)).as_deref(),
        Some("203.0.113.7, 10.0.0.1")
    );
}

#[test]
fn a_first_hop_forwarded_for_is_just_the_client_address() {
    let request = request_with(&[]);
    let client = IpAddr::from([10, 0, 0, 1]);

    assert_eq!(
        forwarded_for(&request, Some(client)).as_deref(),
        Some("10.0.0.1")
    );
}

#[test]
fn an_existing_forwarded_for_survives_when_the_peer_address_is_unknown() {
    let request = request_with(&[("x-forwarded-for", "203.0.113.7")]);

    assert_eq!(
        forwarded_for(&request, None).as_deref(),
        Some("203.0.113.7")
    );
}

#[test]
fn no_forwarded_for_is_invented_when_there_is_nothing_to_forward() {
    assert_eq!(forwarded_for(&request_with(&[]), None), None);
}

#[test]
fn pingora_reports_its_error_source_as_the_classifier_expects() {
    // The classifier keys off `esource`, so the constructors must keep setting it.
    assert_eq!(
        *Error::new_up(ErrorType::ConnectError).esource(),
        ErrorSource::Upstream
    );
    assert_eq!(
        *Error::new_down(ErrorType::ReadError).esource(),
        ErrorSource::Downstream
    );
    assert_eq!(
        *Error::new_in(ErrorType::InternalError).esource(),
        ErrorSource::Internal
    );
}

/// Builds an otherwise empty request carrying the given headers.
fn request_with(headers: &[(&'static str, &str)]) -> RequestHeader {
    let mut request = request_to("example.test", "/api/items");
    for (name, value) in headers {
        request
            .insert_header(*name, *value)
            .expect("the fixture header is valid");
    }
    request
}

#[test]
fn the_framing_guard_restores_the_headers_a_plugin_rewrote() {
    let mut request = request_with(&[("content-length", "12"), ("transfer-encoding", "chunked")]);
    let framing = Framing::snapshot(&request);

    // A plugin that changes the framing must not change how the body is sent on.
    request
        .insert_header("content-length", "0")
        .expect("the fixture header is valid");
    request.remove_header("transfer-encoding");
    framing.restore(&mut request);

    assert_eq!(
        request
            .headers
            .get("content-length")
            .and_then(|value| value.to_str().ok()),
        Some("12")
    );
    assert_eq!(
        request
            .headers
            .get("transfer-encoding")
            .and_then(|value| value.to_str().ok()),
        Some("chunked"),
        "a header the client declared is restored, not left to the plugin"
    );
}

#[test]
fn the_framing_guard_leaves_a_header_absent_when_the_client_declared_none() {
    let mut request = request_with(&[]);
    let framing = Framing::snapshot(&request);

    request
        .insert_header("content-length", "99")
        .expect("the fixture header is valid");
    framing.restore(&mut request);

    assert!(
        request.headers.get("content-length").is_none(),
        "a plugin must not be able to decide the framing by adding a length"
    );
}

#[test]
fn the_framing_guard_restores_every_value_of_a_repeated_header() {
    let mut request = request_with(&[("transfer-encoding", "gzip")]);
    request
        .append_header("transfer-encoding", "chunked")
        .expect("the fixture header is valid");
    let framing = Framing::snapshot(&request);

    request.remove_header("transfer-encoding");
    framing.restore(&mut request);

    let values: Vec<&str> = request
        .headers
        .get_all("transfer-encoding")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect();
    assert_eq!(values, ["gzip", "chunked"]);
}
