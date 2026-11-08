//! The error envelope: one `error` object whose code is the status and whose token names the cause.

use nico_core::ErrorCategory;
use nico_llm::{CanonicalError, ClientCodec};

use crate::support::codec;

/// Encodes one failure and parses the envelope.
fn envelope(category: ErrorCategory, status: u16) -> serde_json::Value {
    let error = CanonicalError::try_new(category, "a_stable_code", "something went wrong", status)
        .expect("a valid error");
    let body = codec().encode_error(&error).expect("the envelope encodes");
    serde_json::from_slice(&body).expect("a JSON envelope")
}

/// Every category has its vendor token, and the numeric code is the status the caller sees.
#[test]
fn every_category_has_its_token_and_the_code_is_the_status() {
    for (category, status, token) in [
        (ErrorCategory::Client, 422, "INVALID_ARGUMENT"),
        (ErrorCategory::Limit, 429, "RESOURCE_EXHAUSTED"),
        (ErrorCategory::Upstream, 502, "UNAVAILABLE"),
        (ErrorCategory::Internal, 500, "INTERNAL"),
        (ErrorCategory::Cancelled, 499, "CANCELLED"),
    ] {
        let envelope = envelope(category, status);
        assert_eq!(envelope["error"]["status"], token, "{category:?}");
        assert_eq!(envelope["error"]["code"], status, "{category:?}");
        assert_eq!(envelope["error"]["message"], "something went wrong");
    }
}

/// The canonical error's stable code has no field in this envelope, so it is not written.
#[test]
fn the_stable_code_has_no_spelling_here() {
    let envelope = envelope(ErrorCategory::Client, 422);
    assert!(
        envelope["error"].get("details").is_none() && envelope["error"].get("errors").is_none(),
        "no invented member: {}",
        serde_json::to_string(&envelope).expect("serializable")
    );
    let members: Vec<String> = envelope["error"]
        .as_object()
        .expect("an object")
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        members,
        ["code", "message", "status"],
        "exactly the members the protocol's own failures carry"
    );
}
