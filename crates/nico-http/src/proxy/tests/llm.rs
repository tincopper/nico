//! The LLM route's hooks, asserted without a session.
//!
//! Each function under test takes the framework header it needs instead of a `Session`, which is what
//! makes the translation assertable here: what the provider receives (path, headers, framework), what
//! the front-end is handed, and what the caller's head and body become.

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use nico_core::ErrorCategory;
use pingora::http::{RequestHeader, ResponseHeader};

use super::super::{forward::destination, hooks::llm};
use crate::{
    Destination,
    context::HttpContext,
    error::HttpError,
    test_support::{FakeLlmRoute, LlmCalls, llm_context, llm_snapshot, request},
};

/// Builds a front-end and the call log a test asserts against.
fn front_end(calls: &Arc<Mutex<LlmCalls>>) -> Arc<FakeLlmRoute> {
    Arc::new(FakeLlmRoute::new(calls))
}

/// Builds a request that carries a client credential and a framing header.
fn client_request() -> RequestHeader {
    let mut request = request("POST", "/v1/chat/completions");
    request
        .insert_header("authorization", "Bearer client-key")
        .expect("the fixture header is valid");
    request
        .insert_header("content-length", "42")
        .expect("the fixture header is valid");
    request
        .insert_header("x-client-tag", "kept")
        .expect("the fixture header is valid");
    request
}

/// Decodes the fixture's client body, so the response half has the facts a transform needs.
fn prepare(ctx: &mut HttpContext) {
    llm::request_body(
        ctx,
        &client_request(),
        &mut Some(Bytes::from_static(b"{}")),
        true,
    )
    .expect("the fixture prepares");
}

/// Builds a provider response head with a declared length.
fn provider_response(status: u16) -> ResponseHeader {
    let mut response = ResponseHeader::build(status, None).expect("the status is valid");
    response
        .insert_header("content-type", "application/json")
        .expect("the fixture header is valid");
    response
        .insert_header("x-provider-only", "not-for-the-caller")
        .expect("the fixture header is valid");
    response
}

#[test]
fn a_provider_route_with_a_published_front_end_has_a_destination() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let snapshot = llm_snapshot();
    let mut ctx = llm_context(&snapshot, front_end(&calls));

    let chosen = destination(&mut ctx).expect("the front-end resolved the provider");

    assert_eq!(
        chosen,
        Destination::new(nico_core::Scheme::Http, "127.0.0.1", 9000)
    );
}

#[test]
fn a_converted_body_is_held_back_until_the_stream_ends() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let snapshot = llm_snapshot();
    let mut ctx = llm_context(&snapshot, front_end(&calls));
    let mut body = Some(Bytes::from_static(b"{\"model\":"));

    let owned = llm::request_body(&mut ctx, &client_request(), &mut body, false)
        .expect("holding a chunk back cannot fail");

    assert!(owned, "the LLM route owns the body");
    assert_eq!(
        body.as_deref(),
        Some(&b""[..]),
        "an empty chunk with the end flag unset is what the framework skips"
    );
    assert!(
        calls
            .lock()
            .expect("the fake lock is not poisoned")
            .prepared
            .is_empty(),
        "nothing is decoded before the client has finished sending"
    );
}

#[test]
fn the_front_end_sees_the_whole_client_request_once() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let snapshot = llm_snapshot();
    let mut ctx = llm_context(&snapshot, front_end(&calls));
    let header = client_request();
    let mut body = Some(Bytes::from_static(b"{\"model\":\"fast\"}"));

    llm::request_body(&mut ctx, &header, &mut body, true).expect("the fixture prepares");

    let calls = calls.lock().expect("the fake lock is not poisoned");
    assert_eq!(calls.prepared.len(), 1, "the body is decoded exactly once");
    let prepared = &calls.prepared[0];
    assert_eq!(prepared.method, "POST");
    assert_eq!(prepared.path, "/v1/chat/completions");
    assert_eq!(prepared.body, b"{\"model\":\"fast\"}");
    assert!(
        prepared
            .headers
            .iter()
            .any(|(name, value)| name == "x-client-tag" && value == "kept"),
        "the codec sees the caller's headers"
    );
    assert_eq!(
        body.as_deref(),
        Some(&b"{\"model\":\"gpt-physical\"}"[..]),
        "the bytes written upstream are the front-end's"
    );
}

#[test]
fn the_front_end_sees_the_query_the_client_sent() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let snapshot = llm_snapshot();
    let mut ctx = llm_context(&snapshot, front_end(&calls));
    let header = request("POST", "/v1beta/models/fast:streamGenerateContent?alt=sse");
    let mut body = Some(Bytes::from_static(b"{}"));

    llm::request_body(&mut ctx, &header, &mut body, true).expect("the fixture prepares");

    let calls = calls.lock().expect("the fake lock is not poisoned");
    assert_eq!(
        calls.prepared[0].path, "/v1beta/models/fast:streamGenerateContent?alt=sse",
        "the facts carry the target with its query, which a protocol may read a fact from"
    );
}

#[test]
fn the_front_end_reports_the_stream_flag_and_the_model() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let mut fake = FakeLlmRoute::new(&calls);
    fake.streaming = true;
    let snapshot = llm_snapshot();
    let mut ctx = llm_context(&snapshot, Arc::new(fake));

    llm::request_body(
        &mut ctx,
        &client_request(),
        &mut Some(Bytes::from_static(b"{}")),
        true,
    )
    .expect("the fixture prepares");

    let exchange = ctx.llm().expect("the route is an LLM route");
    assert_eq!(exchange.streaming(), Some(true));
    assert_eq!(exchange.model(), Some("fast -> gpt-physical"));
}

#[test]
fn a_request_with_no_body_still_reaches_the_front_end() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let snapshot = llm_snapshot();
    let mut ctx = llm_context(&snapshot, front_end(&calls));
    let mut body = None;

    let owned = llm::request_body(&mut ctx, &client_request(), &mut body, true)
        .expect("the fixture prepares an empty body");

    assert!(owned);
    let calls = calls.lock().expect("the fake lock is not poisoned");
    assert_eq!(calls.prepared.len(), 1);
    assert!(calls.prepared[0].body.is_empty());
    assert_eq!(
        body.as_deref(),
        Some(&b"{\"model\":\"gpt-physical\"}"[..]),
        "an empty client body is still replaced by what the front-end encoded"
    );
}

#[test]
fn a_provider_path_a_peer_cannot_parse_is_this_sides_defect() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let mut fake = FakeLlmRoute::new(&calls);
    // A space is not a request target. The path comes from the published front-end, so this is the
    // front-end's defect — but the refusal must still be a typed failure, never a panic and never a
    // request written with half a target.
    fake.path = "/v1/chat completions".to_owned();
    let snapshot = llm_snapshot();
    let ctx = llm_context(&snapshot, Arc::new(fake));
    let mut upstream = client_request();

    let error = llm::apply_upstream_request(&mut upstream, &ctx)
        .expect_err("a target a peer cannot parse is refused");

    assert!(matches!(error, HttpError::Internal { .. }));
    assert_eq!(
        upstream.raw_path(),
        b"/v1/chat/completions",
        "a rejected target leaves the one the client sent intact"
    );
}

#[test]
fn every_chunk_is_held_back_and_the_whole_is_prepared_once() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let snapshot = llm_snapshot();
    let mut ctx = llm_context(&snapshot, front_end(&calls));
    let header = client_request();

    for chunk in [&b"{\"model\":"[..], &b"\"fast\"}"[..]] {
        let mut body = Some(Bytes::copy_from_slice(chunk));
        llm::request_body(&mut ctx, &header, &mut body, false).expect("a chunk is held back");
        assert_eq!(
            body.as_deref(),
            Some(&b""[..]),
            "the client's bytes stop here, not at the provider"
        );
    }
    assert!(
        calls
            .lock()
            .expect("the fake lock is not poisoned")
            .prepared
            .is_empty(),
        "nothing is decoded before the client has finished sending"
    );

    // The framework's final task for an empty tail: no bytes, but the body ends here.
    let mut body = None;
    llm::request_body(&mut ctx, &header, &mut body, true).expect("the fixture prepares");

    let calls = calls.lock().expect("the fake lock is not poisoned");
    assert_eq!(calls.prepared.len(), 1, "the body is decoded exactly once");
    assert_eq!(
        calls.prepared[0].body, b"{\"model\":\"fast\"}",
        "the chunks are accumulated, not decoded one by one"
    );
    assert_eq!(
        body.as_deref(),
        Some(&b"{\"model\":\"gpt-physical\"}"[..]),
        "the bytes written upstream are the front-end's"
    );
}

#[test]
fn a_prepare_failure_reaches_the_hook() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let mut fake = FakeLlmRoute::new(&calls);
    fake.fail_prepare = Some("the document is not a chat request");
    let snapshot = llm_snapshot();
    let mut ctx = llm_context(&snapshot, Arc::new(fake));

    let error = llm::request_body(
        &mut ctx,
        &client_request(),
        &mut Some(Bytes::from_static(b"{}")),
        true,
    )
    .expect_err("the front-end refused the document");

    assert!(
        matches!(&error, HttpError::InvalidRequest { .. }),
        "the caller's document is a 400, got {error:?}"
    );
    assert_eq!(error.category(), ErrorCategory::Client);
}

#[test]
fn a_passthrough_route_forwards_the_body_untouched() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let mut fake = FakeLlmRoute::new(&calls);
    fake.converting_route = false;
    let snapshot = llm_snapshot();
    let mut ctx = llm_context(&snapshot, Arc::new(fake));
    let mut body = Some(Bytes::from_static(b"{\"model\":\"fast\"}"));

    let owned = llm::request_body(&mut ctx, &client_request(), &mut body, false)
        .expect("passthrough cannot fail");

    assert!(owned);
    assert_eq!(
        body.as_deref(),
        Some(&b"{\"model\":\"fast\"}"[..]),
        "passthrough never buffers and never prepares"
    );
    assert!(
        calls
            .lock()
            .expect("the fake lock is not poisoned")
            .prepared
            .is_empty(),
        "there is no canonical value in passthrough"
    );
}

#[test]
fn an_upstream_route_does_not_own_the_body() {
    let snapshot = llm_snapshot();
    let route = &snapshot.document().routes()[0];
    let mut ctx = HttpContext::default();
    ctx.begin(
        &snapshot,
        &request("POST", "/api"),
        std::time::Instant::now(),
    );
    ctx.bind_route(route);
    let mut body = Some(Bytes::from_static(b"chunk"));

    let owned = llm::request_body(&mut ctx, &client_request(), &mut body, false)
        .expect("a route without a front-end is not rewritten");

    assert!(!owned);
    assert_eq!(body.as_deref(), Some(&b"chunk"[..]));
}

mod request;
mod response;
