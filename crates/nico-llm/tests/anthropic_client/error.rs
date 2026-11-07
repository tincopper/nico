//! The error envelope a Messages caller parses.

use nico_core::ErrorCategory;
use nico_llm::{protocol::ClientCodec, schema::CanonicalError};

/// Encodes one failure and returns the parsed document.
fn envelope(category: ErrorCategory) -> serde_json::Value {
    let error = CanonicalError::try_new(category, "some_stable_code", "a safe message", 400)
        .expect("a valid error");
    let body = super::support::codec()
        .encode_error(&error)
        .expect("the envelope is encodable");
    serde_json::from_slice(&body).expect("the envelope is a JSON document")
}

#[test]
fn the_envelope_names_its_own_top_level_type() {
    let document = envelope(ErrorCategory::Client);
    assert_eq!(document["type"], "error");
    assert_eq!(document["error"]["type"], "invalid_request_error");
    assert_eq!(document["error"]["message"], "a safe message");
    assert!(
        document["error"].get("code").is_none(),
        "this protocol has no member for a stable code, and inventing one would be a document no \
         caller's parser expects"
    );
}

#[test]
fn every_category_has_a_word_and_the_status_is_not_written_here() {
    for (category, expected) in [
        (ErrorCategory::Client, "invalid_request_error"),
        (ErrorCategory::Limit, "rate_limit_error"),
        (ErrorCategory::Upstream, "api_error"),
        (ErrorCategory::Internal, "api_error"),
        (ErrorCategory::Cancelled, "api_error"),
    ] {
        assert_eq!(
            envelope(category)["error"]["type"],
            expected,
            "{category:?}"
        );
    }
}

#[test]
fn the_envelope_never_carries_the_provider_code() {
    let error = CanonicalError::try_new(
        ErrorCategory::Upstream,
        "upstream_error",
        "the provider failed",
        502,
    )
    .expect("a valid error")
    .with_provider_code("invalid_api_key")
    .expect("a bounded token");
    let body = super::support::codec()
        .encode_error(&error)
        .expect("the envelope is encodable");
    let printed = String::from_utf8(body).expect("the envelope is UTF-8");
    assert!(!printed.contains("invalid_api_key"), "{printed}");
}
