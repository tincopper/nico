//! One converted response: the head a client answers with, and the bytes under it.

use std::sync::Arc;

use nico_core::ErrorCategory;
use nico_http::{
    error::HttpError,
    llm::{ConvertingRoute, LlmRoute, TokenKind},
};
use nico_llm::{protocol::openai_chat::OpenAiChatCodec, route::ConvertRoute};
use serde_json::Value;

use crate::support::{
    LIMIT, SuccessOnFailure, buffered, chain_of, completion, completion_with_usage, convert_route,
    exchange_with, frame, owned, provider_stream, streamed, success_document,
};

/// The provider headers most fixture answers arrive with.
const JSON_HEADERS: &[(&str, &str)] = &[("content-type", "application/json")];

/// Parses a document a fixture produced.
fn document(body: &[u8]) -> Value {
    serde_json::from_slice(body).expect("the fixture body is JSON")
}

/// Returns the value of one header in a translated head.
fn header<'a>(headers: &'a [String], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find_map(|header| header.strip_prefix(&format!("{name}: ")))
}

#[test]
fn a_buffered_answer_is_reencoded_in_the_client_protocol() {
    let (status, headers, body) =
        buffered(&convert_route(), 201, JSON_HEADERS, &completion("hello"))
            .expect("a complete answer converts");

    assert_eq!(
        status, 200,
        "the client protocol, not the provider's 201, answers a complete request"
    );
    assert_eq!(header(&headers, "content-type"), Some("application/json"));
    assert_eq!(
        header(&headers, "x-nico-error-code"),
        None,
        "a success carries no failure code"
    );
    assert_eq!(document(&body)["object"], "chat.completion");
    assert_eq!(document(&body)["choices"][0]["message"]["content"], "hello");
}

#[test]
fn a_buffered_answer_produces_nothing_until_the_body_ends() {
    let route = convert_route();
    let mut transform = route.transform(false, LIMIT).expect("a transform is built");
    transform
        .head(200, &owned(JSON_HEADERS))
        .expect("the head converts");

    assert!(
        transform
            .push(&completion("hello"))
            .expect("the bytes are buffered")
            .is_empty(),
        "a prefix of a document is not a document"
    );
    assert!(
        !transform.finish().expect("the body completes").is_empty(),
        "the whole answer is written once the body ends"
    );
}

#[test]
fn a_streamed_answer_is_translated_chunk_by_chunk() {
    // Every possible chunk boundary must produce the same client stream: the provider's framing and
    // the client's are independent of how the transport sliced the bytes.
    let route = convert_route();
    let provider = provider_stream();
    let whole = streamed(&route, &provider, provider.len()).expect("the stream converts");
    let text = String::from_utf8(whole.clone()).expect("client SSE is UTF-8");

    assert!(
        text.contains(r#""object":"chat.completion.chunk""#),
        "{text}"
    );
    assert!(text.contains(r#""content":"Hi""#), "{text}");
    assert!(
        text.contains("✓"),
        "a multi-byte token survives every boundary: {text}"
    );
    assert!(text.contains(r#""finish_reason":"stop""#), "{text}");
    assert!(
        text.ends_with("data: [DONE]\n\n"),
        "the client always receives its terminator: {text}"
    );

    for size in 1..=provider.len() {
        let split = streamed(&route, &provider, size).expect("the stream converts");
        assert_eq!(
            split, whole,
            "a chunk size of {size} must not change the client stream"
        );
    }
}

#[test]
fn a_streamed_head_is_an_event_stream() {
    let route = convert_route();
    let mut transform = route.transform(true, LIMIT).expect("a transform is built");
    let head = transform
        .head(200, &owned(JSON_HEADERS))
        .expect("the head converts");
    let names: Vec<&str> = head
        .headers()
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();

    assert_eq!(head.status(), 200);
    assert!(names.contains(&"content-type"), "{names:?}");
    assert_eq!(
        head.headers()
            .iter()
            .find(|(name, _)| name == "content-type")
            .map(|(_, value)| value.as_str()),
        Some("text/event-stream")
    );
    assert_eq!(
        head.headers()
            .iter()
            .find(|(name, _)| name == "cache-control")
            .map(|(_, value)| value.as_str()),
        Some("no-cache")
    );
}

#[test]
fn a_provider_failure_is_answered_in_the_client_envelope_without_its_body() {
    let provider_error = br#"{"error":{"message":"slow down, customer","type":"rate_limit","code":"rate_limit_exceeded"}}"#;
    let (status, headers, body) = buffered(&convert_route(), 429, JSON_HEADERS, provider_error)
        .expect("a failure status still produces a client answer");

    assert_eq!(status, 429);
    assert_eq!(
        header(&headers, "x-nico-error-code"),
        Some("upstream_rate_limited"),
        "the stable code reaches the caller like every other refusal"
    );
    assert_eq!(
        header(&headers, "content-type"),
        Some("application/json"),
        "a failure document is not an event stream"
    );
    assert_eq!(document(&body)["error"]["code"], "upstream_rate_limited");
    assert!(
        !String::from_utf8_lossy(&body).contains("slow down, customer"),
        "the provider's message must not be forwarded"
    );
}

#[test]
fn a_failure_is_buffered_even_when_the_client_asked_for_a_stream() {
    // No event stream can begin before its request finishes; a provider refusing the request answers
    // a document. The transform must render that document, not start an SSE body that never ends.
    let route = convert_route();
    let mut transform = route.transform(true, LIMIT).expect("a transform is built");
    let head = transform
        .head(401, &owned(JSON_HEADERS))
        .expect("the head converts");
    assert_eq!(
        head.status(),
        502,
        "a credential failure is the gateway's, not the caller's"
    );
    assert_eq!(
        head.headers()
            .iter()
            .find(|(name, _)| name == "content-type")
            .map(|(_, value)| value.as_str()),
        Some("application/json")
    );
    assert!(
        transform
            .push(br#"{"error":{"code":"invalid_api_key"}}"#)
            .expect("the failure document is buffered")
            .is_empty(),
        "a failure document produces its bytes when it is complete"
    );
    let body = transform.finish().expect("the failure converts");
    assert_eq!(document(&body)["error"]["code"], "upstream_auth_failed");
}

#[test]
fn an_unknown_status_becomes_the_default_upstream_failure() {
    for status in [199, 302, 599] {
        let (client_status, headers, body) =
            buffered(&convert_route(), status, &[], b"{}").expect("a status is always answerable");

        assert_eq!(client_status, 502, "status {status} is not a chat answer");
        assert_eq!(
            header(&headers, "x-nico-error-code"),
            Some("upstream_error")
        );
        assert_eq!(document(&body)["error"]["code"], "upstream_error");
    }
}

#[test]
fn a_declared_length_over_the_ceiling_is_refused_before_the_head_is_written() {
    let route = convert_route();
    let mut transform = route.transform(false, 128).expect("a transform is built");
    let error = transform
        .head(200, &owned(&[("content-length", "999")]))
        .expect_err("the answer declares more than the route allows");

    assert!(
        matches!(error, HttpError::UpstreamResponseTooLarge { limit: 128 }),
        "{error:?}"
    );
    assert_eq!(error.category(), ErrorCategory::Limit);
}

#[test]
fn a_failure_document_over_the_ceiling_is_refused_too() {
    // The client asked for a stream, but the answer is a failure document and therefore buffered.
    // Its declared length is still known before the head is written, so it is refused the same way.
    let route = convert_route();
    let mut transform = route.transform(true, 128).expect("a transform is built");
    let error = transform
        .head(429, &owned(&[("content-length", "999")]))
        .expect_err("the failure document declares more than the route allows");

    assert!(
        matches!(error, HttpError::UpstreamResponseTooLarge { limit: 128 }),
        "{error:?}"
    );
}

#[test]
fn a_chunked_answer_is_metered_while_it_arrives() {
    let route = convert_route();
    let mut transform = route.transform(false, 16).expect("a transform is built");
    transform
        .head(200, &owned(JSON_HEADERS))
        .expect("the head converts");
    transform
        .push(b"0123456789")
        .expect("the prefix is under the ceiling");
    let error = transform
        .push(b"0123456789")
        .expect_err("the second half crosses the ceiling");

    assert!(
        matches!(error, HttpError::UpstreamResponseTooLarge { limit: 16 }),
        "{error:?}"
    );
}

#[test]
fn an_event_stream_is_never_metered() {
    // An event stream is translated chunk by chunk and never held whole, so the buffered ceiling does
    // not apply to it — not even to a streamed answer that declares a total length.
    let route = convert_route();
    let provider = provider_stream();
    let mut transform = route.transform(true, 8).expect("a transform is built");
    transform
        .head(200, &owned(&[("content-length", "999999")]))
        .expect("a declared length does not bound a stream");
    transform
        .push(&provider)
        .expect("a chunk larger than the ceiling is still translated");
    transform.finish().expect("the stream ends");
}

#[test]
fn a_stream_that_ends_before_its_finish_reason_ends_the_client_stream() {
    // The provider stopped mid-generation. The transform reports an upstream failure instead of
    // writing the client's terminator: the caller must not be told the model finished when nobody
    // said it did, and the bytes already written are not retracted.
    let route = convert_route();
    let mut transform = route.transform(true, LIMIT).expect("a transform is built");
    transform
        .head(200, &owned(JSON_HEADERS))
        .expect("the head converts");
    let truncated = frame(
        r#"{"id":"chatcmpl-1","model":"chat-small-2026-01-01","choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]}"#,
    );
    let written = transform
        .push(truncated.as_bytes())
        .expect("the completed chunk converts");
    assert!(
        String::from_utf8_lossy(&written).contains(r#""content":"Hi""#),
        "the bytes of the events that did complete are written"
    );

    let error = transform
        .finish()
        .expect_err("the provider never announced an end");

    assert!(
        matches!(error, HttpError::UpstreamFailed { .. }),
        "a peer that broke its own protocol is an upstream failure: {error:?}"
    );
    assert_eq!(error.category(), ErrorCategory::Upstream);
}

#[test]
fn a_malformed_provider_body_is_an_upstream_failure() {
    let route = convert_route();
    let mut transform = route.transform(false, LIMIT).expect("a transform is built");
    transform
        .head(200, &owned(JSON_HEADERS))
        .expect("the head converts");
    transform
        .push(b"not a document")
        .expect("the bytes are buffered");
    let error = transform
        .finish()
        .expect_err("the provider did not return its own document");

    assert!(
        matches!(error, HttpError::UpstreamFailed { .. }),
        "the peer broke its own protocol: {error:?}"
    );
    assert_eq!(error.category(), ErrorCategory::Upstream);
}

#[test]
fn a_reply_that_disagrees_with_the_head_ends_the_stream() {
    // A provider adapter that answers success to a failure status would make the body's envelope
    // contradict the status line already written. The head is the contract; the disagreement is a
    // front-end defect and the stream ends.
    let exchange = exchange_with(
        Arc::new(OpenAiChatCodec::new()),
        Arc::new(SuccessOnFailure::new()),
    );
    let route = ConvertRoute::new(chain_of(exchange), crate::support::breakers(), 1);
    let mut transform = route.transform(false, LIMIT).expect("a transform is built");
    let head = transform
        .head(429, &owned(JSON_HEADERS))
        .expect("the head converts");
    assert_eq!(head.status(), 429);
    transform
        .push(&success_document())
        .expect("the body is buffered");
    let error = transform
        .finish()
        .expect_err("the reply disagrees with the written head");

    assert!(matches!(error, HttpError::Internal { .. }), "{error:?}");
}

#[test]
fn a_body_that_arrives_before_its_head_is_refused() {
    let route = convert_route();
    let mut transform = route.transform(false, LIMIT).expect("a transform is built");
    let error = transform
        .push(b"bytes")
        .expect_err("the data plane always translates the head first");
    assert!(matches!(error, HttpError::Internal { .. }), "{error:?}");

    let mut transform = route.transform(false, LIMIT).expect("a transform is built");
    let error = transform
        .finish()
        .expect_err("a body cannot end before its head exists");
    assert!(matches!(error, HttpError::Internal { .. }), "{error:?}");
}

#[test]
fn an_empty_chunk_writes_nothing_and_does_not_end_the_body() {
    let route = convert_route();
    let mut transform = route.transform(true, LIMIT).expect("a transform is built");
    transform
        .head(200, &owned(JSON_HEADERS))
        .expect("the head converts");
    transform
        .push(&provider_stream())
        .expect("the stream converts");

    assert!(
        transform
            .push(b"")
            .expect("an empty task is not a failure")
            .is_empty(),
        "a transport may deliver an empty task; it carries no event"
    );
    let body = transform.finish().expect("the stream still ends");
    assert!(
        String::from_utf8_lossy(&body).ends_with("data: [DONE]\n\n"),
        "the terminator follows the events the body completed"
    );
}

#[test]
fn the_route_hands_out_a_fresh_transform_per_response() {
    // The stream encoder is per response: reusing one would repeat the first response's id into the
    // second one's chunks.
    let route = convert_route();
    let first = streamed(&route, &provider_stream(), 7).expect("the stream converts");
    let second = streamed(&route, &provider_stream(), 7).expect("the stream converts");

    assert_eq!(first, second);
    assert!(
        route.converting().is_some(),
        "a converting route reports its exchange"
    );
}

#[test]
fn a_buffered_answer_reports_the_providers_token_counts() {
    let mut transform = convert_route()
        .transform(false, LIMIT)
        .expect("a transform is built");
    transform
        .head(200, &owned(JSON_HEADERS))
        .expect("the head converts");

    assert!(
        transform.usage().is_none(),
        "nothing is known before the body ends"
    );

    transform
        .push(&completion_with_usage("hello", 11, 7))
        .expect("the bytes are buffered");
    transform.finish().expect("the body completes");

    let usage = transform.usage().expect("the provider reported usage");
    assert_eq!(usage.get(TokenKind::Input), Some(11));
    assert_eq!(usage.get(TokenKind::Output), Some(7));
    assert_eq!(
        usage.get(TokenKind::CacheRead),
        None,
        "a kind the provider did not report has no count"
    );
}

#[test]
fn an_answer_without_usage_reports_none_rather_than_zeros() {
    // The distinction the request record depends on: a provider that said nothing produces no sample,
    // and writing a zero instead would pollute every average computed downstream.
    let mut transform = convert_route()
        .transform(false, LIMIT)
        .expect("a transform is built");
    transform
        .head(200, &owned(JSON_HEADERS))
        .expect("the head converts");
    transform
        .push(&completion("hello"))
        .expect("the bytes are buffered");
    transform.finish().expect("the body completes");

    assert!(transform.usage().is_none());
}
