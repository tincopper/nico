//! The response direction: the head a client protocol answers with, and the body under it.
//!
//! Both functions take the framework header they need instead of a `Session`, so the translation is
//! assertable here: what the caller's head becomes, what the transform is handed, and what the bytes
//! written under it are.

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use nico_core::ErrorCategory;
use pingora::http::Method;

use super::{client_request, front_end, prepare, provider_response};
use crate::{
    context::HttpContext,
    error::HttpError,
    proxy::hooks::llm,
    test_support::{FakeLlmRoute, LlmCalls, llm_context, llm_snapshot, llm_snapshot_with, request},
};

#[test]
fn a_converted_head_replaces_the_provider_head() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let snapshot = llm_snapshot();
    let mut ctx = llm_context(&snapshot, front_end(&calls));
    let request = client_request();
    let mut response = provider_response(200);

    prepare(&mut ctx);
    llm::install_transform(&mut ctx, &request, &mut response).expect("the head translates");

    assert_eq!(response.status.as_u16(), 200);
    assert_eq!(
        response
            .headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    assert!(
        response.headers.get("x-provider-only").is_none(),
        "a provider header describes bytes the caller never receives"
    );
    assert!(
        response.headers.get("content-length").is_none(),
        "the data plane frames the body it writes"
    );
    assert_eq!(
        response
            .headers
            .get("x-nico-trace-id")
            .and_then(|value| value.to_str().ok()),
        Some("trace-llm")
    );
    assert!(ctx.llm().is_some_and(|llm| llm.has_transform()));
}

#[test]
fn the_transform_is_created_with_the_stream_flag_and_the_route_ceiling() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let mut fake = FakeLlmRoute::new(&calls);
    fake.streaming = true;
    let snapshot = llm_snapshot_with("    max_response_bytes: 512\n");
    let mut ctx = llm_context(&snapshot, Arc::new(fake));
    llm::request_body(
        &mut ctx,
        &client_request(),
        &mut Some(Bytes::from_static(b"{}")),
        true,
    )
    .expect("the fixture prepares");

    llm::install_transform(&mut ctx, &client_request(), &mut provider_response(200))
        .expect("the head translates");

    assert_eq!(
        calls
            .lock()
            .expect("the fake lock is not poisoned")
            .transforms,
        vec![(true, 512)],
        "a route may tighten the document ceiling, never widen it"
    );
}

#[test]
fn a_declared_length_over_the_ceiling_is_refused_before_the_head_is_written() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let snapshot = llm_snapshot_with("    max_response_bytes: 1024\n");
    let mut ctx = llm_context(&snapshot, front_end(&calls));
    // A non-streaming answer: the front-end buffers it, so the declared size is the size.
    llm::request_body(
        &mut ctx,
        &client_request(),
        &mut Some(Bytes::from_static(b"{}")),
        true,
    )
    .expect("the fixture prepares");
    let mut response = provider_response(200);
    response
        .insert_header("content-length", "4096")
        .expect("the fixture header is valid");

    let error = llm::install_transform(&mut ctx, &client_request(), &mut response)
        .expect_err("a declared size over the ceiling is refused");

    let respond = error.respond().expect("the refusal is renderable");
    assert_eq!(respond.status().as_u16(), 502);
    assert_eq!(respond.code(), "upstream_response_too_large");
    assert_eq!(error.category(), ErrorCategory::Limit);
    assert!(
        !ctx.llm().is_some_and(|llm| llm.has_transform()),
        "a refused response installs nothing"
    );
    assert_eq!(
        response
            .headers
            .get("content-length")
            .and_then(|value| value.to_str().ok()),
        Some("4096"),
        "the head is untouched, so the failure can still be written in its place"
    );
}

#[test]
fn an_event_stream_may_declare_any_length() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let mut fake = FakeLlmRoute::new(&calls);
    fake.streaming = true;
    let snapshot = llm_snapshot_with("    max_response_bytes: 1024\n");
    let mut ctx = llm_context(&snapshot, Arc::new(fake));
    llm::request_body(
        &mut ctx,
        &client_request(),
        &mut Some(Bytes::from_static(b"{}")),
        true,
    )
    .expect("the fixture prepares");
    let mut response = provider_response(200);
    response
        .insert_header("content-length", "4096")
        .expect("the fixture header is valid");

    llm::install_transform(&mut ctx, &client_request(), &mut response)
        .expect("an event stream is translated chunk by chunk, never buffered");

    assert!(ctx.llm().is_some_and(|llm| llm.has_transform()));
}

#[test]
fn a_response_that_cannot_carry_a_body_is_not_translated() {
    for status in [204, 304] {
        let calls = Arc::new(Mutex::new(LlmCalls::default()));
        let snapshot = llm_snapshot();
        let mut ctx = llm_context(&snapshot, front_end(&calls));
        let mut response = provider_response(status);

        llm::install_transform(&mut ctx, &client_request(), &mut response)
            .expect("there is nothing to decode");

        assert!(
            !ctx.llm().is_some_and(|llm| llm.has_transform()),
            "a bodiless answer has no client envelope to invent"
        );
        assert_eq!(response.status.as_u16(), status);
        assert!(response.headers.get("x-provider-only").is_some());
    }

    // A declared length of zero is the same case, and so is a `HEAD` request.
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let snapshot = llm_snapshot();
    let mut ctx = llm_context(&snapshot, front_end(&calls));
    let mut response = provider_response(200);
    response
        .insert_header("content-length", "0")
        .expect("the fixture header is valid");
    llm::install_transform(&mut ctx, &client_request(), &mut response)
        .expect("there is nothing to decode");
    assert!(!ctx.llm().is_some_and(|llm| llm.has_transform()));

    let mut head_request = client_request();
    head_request.set_method(Method::HEAD);
    llm::install_transform(&mut ctx, &head_request, &mut provider_response(200))
        .expect("a HEAD answer has no body by definition");
    assert!(!ctx.llm().is_some_and(|llm| llm.has_transform()));
}

#[test]
fn a_passthrough_response_keeps_the_provider_head() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let mut fake = FakeLlmRoute::new(&calls);
    fake.converting_route = false;
    let snapshot = llm_snapshot();
    let mut ctx = llm_context(&snapshot, Arc::new(fake));
    let mut response = provider_response(200);

    llm::install_transform(&mut ctx, &client_request(), &mut response)
        .expect("passthrough installs nothing");

    assert!(!ctx.llm().is_some_and(|llm| llm.has_transform()));
    assert!(
        response.headers.get("x-provider-only").is_some(),
        "the caller's protocol already is the provider's"
    );
}

#[test]
fn a_transform_head_failure_is_reported_and_the_head_is_left_alone() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let mut fake = FakeLlmRoute::new(&calls);
    fake.head = None;
    let snapshot = llm_snapshot();
    let mut ctx = llm_context(&snapshot, Arc::new(fake));
    let mut response = provider_response(200);

    let error = llm::install_transform(&mut ctx, &client_request(), &mut response)
        .expect_err("the fake transform refuses the head");

    assert!(matches!(error, HttpError::Internal { .. }));
    assert!(response.headers.get("x-provider-only").is_some());
    assert!(!ctx.llm().is_some_and(|llm| llm.has_transform()));
}

#[test]
fn a_response_that_precedes_the_client_body_is_translated_as_a_buffered_one() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let mut fake = FakeLlmRoute::new(&calls);
    // The provider refused the request from its head alone, before the caller finished sending.
    fake.head = Some((
        429,
        vec![("content-type".to_owned(), "application/json".to_owned())],
    ));
    let snapshot = llm_snapshot();
    let mut ctx = llm_context(&snapshot, Arc::new(fake));
    let mut response = provider_response(429);

    llm::install_transform(&mut ctx, &client_request(), &mut response)
        .expect("an early refusal is still a response the client protocol can render");

    assert_eq!(
        calls
            .lock()
            .expect("the fake lock is not poisoned")
            .transforms,
        vec![(false, 4096)],
        "no stream begins before its request ends, so the answer is buffered"
    );
    assert_eq!(
        response.status.as_u16(),
        429,
        "the refusal keeps the status the client protocol chose for it"
    );
    assert!(ctx.llm().is_some_and(|llm| llm.has_transform()));
}

#[test]
fn a_response_body_is_translated_chunk_by_chunk_and_completed() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let snapshot = llm_snapshot();
    let mut ctx = llm_context(&snapshot, front_end(&calls));
    let mut response = provider_response(200);
    prepare(&mut ctx);
    llm::install_transform(&mut ctx, &client_request(), &mut response)
        .expect("the head translates");

    let mut first = Some(Bytes::from_static(b"ab"));
    llm::transform_response_body(&mut ctx, &mut first, false).expect("the chunk translates");
    let mut second = Some(Bytes::from_static(b"cd"));
    llm::transform_response_body(&mut ctx, &mut second, true).expect("the body completes");

    assert_eq!(first.as_deref(), Some(&b"AB"[..]));
    assert_eq!(
        second.as_deref(),
        Some(&b"CD<end>"[..]),
        "the completion bytes are written with the final chunk"
    );
    let calls = calls.lock().expect("the fake lock is not poisoned");
    assert_eq!(calls.chunks, vec![b"ab".to_vec(), b"cd".to_vec()]);
    assert_eq!(calls.finishes, 1);
}

#[test]
fn an_empty_final_task_still_completes_the_body() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let snapshot = llm_snapshot();
    let mut ctx = llm_context(&snapshot, front_end(&calls));
    prepare(&mut ctx);
    llm::install_transform(&mut ctx, &client_request(), &mut provider_response(200))
        .expect("the head translates");
    let mut body = None;

    llm::transform_response_body(&mut ctx, &mut body, true).expect("the body completes");

    assert_eq!(
        body.as_deref(),
        Some(&b"<end>"[..]),
        "the terminator is written even when the provider's last task carried no bytes"
    );
}

#[test]
fn a_task_with_no_bytes_writes_nothing() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let snapshot = llm_snapshot();
    let mut ctx = llm_context(&snapshot, front_end(&calls));
    prepare(&mut ctx);
    llm::install_transform(&mut ctx, &client_request(), &mut provider_response(200))
        .expect("the head translates");
    let mut body = None;

    llm::transform_response_body(&mut ctx, &mut body, false).expect("nothing to translate");

    assert!(body.is_none());
    assert_eq!(
        calls
            .lock()
            .expect("the fake lock is not poisoned")
            .finishes,
        0
    );
}

#[test]
fn a_transform_that_exceeds_the_ceiling_ends_the_stream() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let snapshot = llm_snapshot_with("    max_response_bytes: 4\n");
    let mut ctx = llm_context(&snapshot, front_end(&calls));
    prepare(&mut ctx);
    llm::install_transform(&mut ctx, &client_request(), &mut provider_response(200))
        .expect("the head translates");
    let mut body = Some(Bytes::from_static(b"too much"));

    let error = llm::transform_response_body(&mut ctx, &mut body, false)
        .expect_err("the buffered answer crossed the route ceiling");

    assert!(matches!(
        error,
        HttpError::UpstreamResponseTooLarge { limit: 4 }
    ));
}

#[test]
fn a_response_body_without_a_transform_is_left_alone() {
    let snapshot = llm_snapshot();
    let route = &snapshot.document().routes()[0];
    let mut ctx = HttpContext::default();
    ctx.begin(
        &snapshot,
        &request("GET", "/api"),
        std::time::Instant::now(),
    );
    ctx.bind_route(route);
    let mut body = Some(Bytes::from_static(b"untouched"));

    llm::transform_response_body(&mut ctx, &mut body, true).expect("no transform is a no-op");

    assert_eq!(body.as_deref(), Some(&b"untouched"[..]));
}

#[test]
fn the_context_debug_reports_the_llm_facts_and_never_a_body() {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let mut fake = FakeLlmRoute::new(&calls);
    fake.body = b"a prompt that must not appear".to_vec();
    let snapshot = llm_snapshot();
    let mut ctx = llm_context(&snapshot, Arc::new(fake));
    llm::request_body(
        &mut ctx,
        &client_request(),
        &mut Some(Bytes::from_static(b"a prompt that must not appear")),
        true,
    )
    .expect("the fixture prepares");

    let printed = format!("{ctx:?}");

    assert!(printed.contains("LlmExchange"), "{printed}");
    assert!(
        printed.contains("openai.chat"),
        "the protocol is the report"
    );
    assert!(
        printed.contains("fast -> gpt-physical"),
        "the model is reported"
    );
    assert!(
        !printed.contains("a prompt"),
        "a prompt must have no Debug spelling: {printed}"
    );
    assert!(
        !printed.contains("provider-key"),
        "a credential must have no Debug spelling: {printed}"
    );
}
