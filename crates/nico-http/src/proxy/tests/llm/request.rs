//! The request-head half of the hook: what reaches the provider, and what never does.
//!
//! Split from [`super`] by direction, because the two halves answer different questions: the head is
//! decided once, before any body exists, while the body hooks run per chunk. A reader looking for
//! "what does the caller's head become" should not have to scroll past the streaming cases.

use std::sync::{Arc, Mutex};

use nico_core::ErrorCategory;

use super::{client_request, front_end};
use crate::{
    context::HttpContext,
    error::HttpError,
    proxy::hooks::llm,
    test_support::{FakeLlmRoute, LlmCalls, llm_context, llm_snapshot, request},
};

#[test]
fn an_upstream_route_leaves_the_request_alone() {
    let snapshot = llm_snapshot();
    let route = &snapshot.document().routes()[0];
    let mut ctx = HttpContext::default();
    ctx.begin(
        &snapshot,
        &request("POST", "/api"),
        std::time::Instant::now(),
    );
    ctx.bind_route(route);
    let mut upstream = client_request();

    llm::apply_upstream_request(&mut upstream, &ctx)
        .expect("an unrouted exchange is not rewritten");

    assert_eq!(upstream.raw_path(), b"/v1/chat/completions");
    assert_eq!(
        upstream
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer client-key"),
        "a filter's route must keep the caller's own headers"
    );
}

#[test]
fn a_converted_route_writes_the_provider_path_and_headers() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let snapshot = llm_snapshot();
    let ctx = llm_context(&snapshot, front_end(&calls));
    let mut upstream = client_request();

    llm::apply_upstream_request(&mut upstream, &ctx).expect("the provider head is valid");

    assert_eq!(upstream.raw_path(), b"/v1/chat/completions");
    assert_eq!(
        upstream
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer provider-key"),
        "the route's credential replaces the caller's"
    );
    assert_eq!(
        upstream
            .headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json"),
        "the provider's content type describes the encoded body"
    );
}

#[test]
fn a_converted_route_drops_the_callers_credential_even_without_one_of_its_own() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let mut fake = FakeLlmRoute::new(&calls);
    // A route with no credential: a local compatible endpoint needs none.
    fake.headers = vec![("content-type".to_owned(), "application/json".to_owned())];
    let snapshot = llm_snapshot();
    let ctx = llm_context(&snapshot, Arc::new(fake));
    let mut upstream = client_request();

    llm::apply_upstream_request(&mut upstream, &ctx).expect("the provider head is valid");

    assert!(
        upstream.headers.get("authorization").is_none(),
        "the caller's key must not travel to a provider that did not issue it"
    );
}

#[test]
fn a_passthrough_route_keeps_the_callers_credential_when_the_route_presents_none() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let mut fake = FakeLlmRoute::new(&calls);
    fake.converting_route = false;
    fake.headers = vec![("content-type".to_owned(), "application/json".to_owned())];
    let snapshot = llm_snapshot();
    let ctx = llm_context(&snapshot, Arc::new(fake));
    let mut upstream = client_request();

    llm::apply_upstream_request(&mut upstream, &ctx).expect("the provider head is valid");

    assert_eq!(
        upstream
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer client-key"),
        "passthrough forwards the caller's own protocol, credential included"
    );
}

#[test]
fn a_route_header_the_data_plane_owns_is_ignored() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let mut fake = FakeLlmRoute::new(&calls);
    fake.headers = vec![
        ("host".to_owned(), "wrong.example".to_owned()),
        ("content-length".to_owned(), "1".to_owned()),
        ("transfer-encoding".to_owned(), "gzip".to_owned()),
        ("connection".to_owned(), "close".to_owned()),
        ("x-provider-tag".to_owned(), "kept".to_owned()),
    ];
    let snapshot = llm_snapshot();
    let ctx = llm_context(&snapshot, Arc::new(fake));
    let mut upstream = client_request();

    llm::apply_upstream_request(&mut upstream, &ctx).expect("the provider head is valid");

    for name in ["host", "transfer-encoding", "connection"] {
        assert!(
            upstream.headers.get(name).is_none(),
            "`{name}` describes a message this crate frames"
        );
    }
    assert_eq!(
        upstream
            .headers
            .get("content-length")
            .and_then(|value| value.to_str().ok()),
        Some("42"),
        "the client's framing is the data plane's, not the route's"
    );
    assert_eq!(
        upstream
            .headers
            .get("x-provider-tag")
            .and_then(|value| value.to_str().ok()),
        Some("kept"),
        "an end-to-end provider header is still written"
    );
}

#[test]
fn a_converted_route_offers_the_provider_no_content_coding() {
    // The answer's head is written from the front-end's own headers with the provider's removed, and
    // nothing here decompresses. Offering a coding would make a compressed answer arrive as JSON and
    // fail after the status line was committed, which is a `200` with no body rather than an error.
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let fake = FakeLlmRoute::new(&calls);
    let snapshot = llm_snapshot();
    let ctx = llm_context(&snapshot, Arc::new(fake));
    let mut upstream = client_request();
    upstream
        .insert_header("accept-encoding", "gzip, br")
        .expect("the fixture header is valid");
    upstream
        .insert_header("content-encoding", "identity")
        .expect("the fixture header is valid");

    llm::apply_upstream_request(&mut upstream, &ctx).expect("the provider head is valid");

    assert!(
        upstream.headers.get("accept-encoding").is_none(),
        "the data plane re-frames the answer, so it decides the coding"
    );
    assert!(
        upstream.headers.get("content-encoding").is_some(),
        "`identity` describes the bytes that travel, so it is allowed"
    );
}

#[test]
fn a_converted_route_refuses_a_content_encoded_body() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let fake = FakeLlmRoute::new(&calls);
    let snapshot = llm_snapshot();
    let ctx = llm_context(&snapshot, Arc::new(fake));
    let mut upstream = client_request();
    upstream
        .insert_header("content-encoding", "gzip")
        .expect("the fixture header is valid");

    let error = llm::apply_upstream_request(&mut upstream, &ctx)
        .expect_err("a converted route decodes the body itself");

    assert!(
        matches!(error, HttpError::InvalidRequest { .. }),
        "the caller's request is what cannot be served, got {error:?}"
    );
    assert_eq!(error.category(), ErrorCategory::Client);
}

#[test]
fn a_passthrough_route_keeps_the_callers_coding() {
    // Nothing is decoded, so the coding describes the bytes the provider receives and must travel.
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let mut fake = FakeLlmRoute::new(&calls);
    fake.converting_route = false;
    let snapshot = llm_snapshot();
    let ctx = llm_context(&snapshot, Arc::new(fake));
    let mut upstream = client_request();
    upstream
        .insert_header("accept-encoding", "gzip")
        .expect("the fixture header is valid");

    llm::apply_upstream_request(&mut upstream, &ctx).expect("the provider head is valid");

    assert_eq!(
        upstream
            .headers
            .get("accept-encoding")
            .and_then(|value| value.to_str().ok()),
        Some("gzip"),
        "passthrough forwards the caller's coding with its bytes"
    );
}

#[test]
fn a_route_cannot_offer_a_content_coding() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let mut fake = FakeLlmRoute::new(&calls);
    fake.headers = vec![
        ("accept-encoding".to_owned(), "gzip".to_owned()),
        ("content-encoding".to_owned(), "br".to_owned()),
    ];
    let snapshot = llm_snapshot();
    let ctx = llm_context(&snapshot, Arc::new(fake));
    let mut upstream = client_request();

    llm::apply_upstream_request(&mut upstream, &ctx).expect("the provider head is valid");

    for name in ["accept-encoding", "content-encoding"] {
        assert!(
            upstream.headers.get(name).is_none(),
            "`{name}` negotiates bytes the data plane writes"
        );
    }
}
