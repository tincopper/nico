//! What the client sees for one provider answer: the status, the body, and the failure mapping.
//!
//! The split from `main.rs` is by scenario, not by line count: these tests all drive
//! [`Exchange::respond`], while the request side (`prepare`) lives next to the ordering and
//! capability tests.

use nico_core::ErrorCategory;
use nico_llm::{Capabilities, ResponseFacts};

use super::mocks::exchange;

#[test]
fn a_successful_response_is_encoded_for_the_client() {
    let exchange = exchange(Capabilities::all(), Capabilities::all());
    let facts = ResponseFacts::new(200, Vec::new());
    let reply = exchange
        .respond(&facts, b"resp-1|hi")
        .expect("a well-formed response decodes");
    assert_eq!(reply.status(), 200);
    assert_eq!(
        std::str::from_utf8(reply.body()).expect("the encoded body is UTF-8"),
        "resp-1|hi"
    );
}

#[test]
fn the_client_status_comes_from_the_codec_not_the_provider() {
    let exchange = exchange(Capabilities::all(), Capabilities::all());
    // A provider's own 2xx variant describes how it framed its body; once the body is re-encoded it
    // says nothing about the caller's request, so only the codec's status may reach the client.
    let reply = exchange
        .respond(&ResponseFacts::new(201, Vec::new()), b"resp-1|hi")
        .expect("a 2xx provider body decodes");
    assert_eq!(reply.status(), 200, "the codec owns a success status");
    assert_eq!(
        std::str::from_utf8(reply.body()).expect("the encoded body is UTF-8"),
        "resp-1|hi"
    );
}

#[test]
fn a_provider_failure_status_is_answered_in_the_client_protocol() {
    let exchange = exchange(Capabilities::all(), Capabilities::all());
    let facts = ResponseFacts::new(429, Vec::new());
    let reply = exchange
        .respond(&facts, br#"{"error":"quota"}"#)
        .expect("a provider failure is still a client answer");
    assert_eq!(reply.status(), 429);
    assert_eq!(
        std::str::from_utf8(reply.body()).expect("the encoded error is UTF-8"),
        "error:429:mock_refused\n",
        "the client sees the canonical status and code, never the provider body"
    );
}

#[test]
fn a_malformed_provider_body_is_an_upstream_failure() {
    let exchange = exchange(Capabilities::all(), Capabilities::all());
    let error = exchange
        .respond(&ResponseFacts::new(200, Vec::new()), b"not-a-response")
        .expect_err("the mock provider needs `id|text`");
    assert_eq!(error.category(), ErrorCategory::Upstream);
    assert_eq!(error.canonical().http_status(), 502);
    assert_eq!(error.canonical().code(), "upstream_protocol_error");
}
