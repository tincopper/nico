//! The front-end's error document: what a converted LLM route answers with.
//!
//! The assertions are the ones a caller would make: the status and the stable code are the
//! front-end's, the body is the document its codec encoded byte for byte, and the category is the
//! one the front-end declared rather than one derived from a status it chose.

use std::sync::Arc;

use nico_core::ErrorCategory;
use pingora::http::StatusCode;

use super::llm_failure;
use crate::error::{HttpError, LlmRejection, LlmRejectionError};

/// The error as a converted route reports it: the presentation, shared, with no second mapping.
fn failure(status: u16, code: &str, category: ErrorCategory) -> HttpError {
    HttpError::LlmRejected {
        rejection: Arc::new(llm_failure(status, code, category)),
    }
}

#[test]
fn a_refused_request_keeps_the_codecs_own_document() {
    // The body is the codec's, not this crate's envelope: a client that speaks the provider's
    // protocol must receive that protocol's failure document.
    let error = failure(422, "unsupported_capability", ErrorCategory::Client);
    let respond = error
        .respond()
        .expect("a front-end rejection is renderable");

    assert_eq!(respond.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(respond.code(), "unsupported_capability");
    assert_eq!(respond.content_type(), "application/json");
    assert_eq!(
        respond.body("trace-1"),
        llm_failure(422, "unsupported_capability", ErrorCategory::Client)
            .body()
            .to_vec(),
        "the codec's bytes are written unchanged, with no trace id substituted"
    );
    assert_eq!(error.category(), ErrorCategory::Client);
    assert!(
        respond.challenge().is_none(),
        "a client protocol's rejection carries no challenge"
    );
    assert!(respond.retry_after().is_none());
}

#[test]
fn a_client_side_failure_keeps_its_own_status() {
    let respond = failure(400, "invalid_request", ErrorCategory::Client)
        .respond()
        .expect("a front-end rejection is renderable");

    assert_eq!(respond.status(), StatusCode::BAD_REQUEST);
    assert_eq!(respond.code(), "invalid_request");
}

#[test]
fn a_gateway_defect_keeps_the_status_the_front_end_chose() {
    let respond = failure(500, "client_encode_failed", ErrorCategory::Internal)
        .respond()
        .expect("a front-end rejection is renderable");

    assert_eq!(respond.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        HttpError::LlmRejected {
            rejection: Arc::new(llm_failure(
                500,
                "client_encode_failed",
                ErrorCategory::Internal
            ))
        }
        .category(),
        ErrorCategory::Internal
    );
}

#[test]
fn an_upstream_failure_is_reported_with_the_front_ends_status_and_category() {
    let respond = failure(502, "upstream_protocol_error", ErrorCategory::Upstream)
        .respond()
        .expect("a front-end rejection is renderable");

    assert_eq!(respond.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(respond.code(), "upstream_protocol_error");
}

#[test]
fn a_status_outside_the_failure_range_is_refused() {
    for status in [200, 299, 399, 600, 999] {
        let rejection = LlmRejection::new(
            status,
            "invalid_request",
            "the request is not valid",
            Vec::new(),
            ErrorCategory::Client,
        );
        assert!(
            matches!(rejection, Err(LlmRejectionError::Status { .. })),
            "{status} is not a failure status: {rejection:?}"
        );
    }
}

#[test]
fn a_code_is_a_bounded_stable_identifier() {
    let cases = [
        ("", LlmRejectionError::Empty { field: "code" }),
        (
            &"a".repeat(65),
            LlmRejectionError::TooLong {
                field: "code",
                limit: 64,
            },
        ),
        ("Unsupported_Capability", LlmRejectionError::NotSnakeCase),
        ("unsupported-capability", LlmRejectionError::NotSnakeCase),
        ("unsupported capability", LlmRejectionError::NotSnakeCase),
    ];

    for (code, expected) in cases {
        let rejection = LlmRejection::new(
            400,
            code,
            "the request is not valid",
            Vec::new(),
            ErrorCategory::Client,
        );
        assert!(
            matches!(&rejection, Err(error) if *error == expected),
            "`{code}` must be refused: {rejection:?}"
        );
    }
}

#[test]
fn a_message_is_a_bounded_sentence() {
    let empty = LlmRejection::new(
        400,
        "invalid_request",
        "",
        Vec::new(),
        ErrorCategory::Client,
    );
    assert!(
        matches!(&empty, Err(LlmRejectionError::Empty { field: "message" })),
        "{empty:?}"
    );

    let long = LlmRejection::new(
        400,
        "invalid_request",
        "a".repeat(257),
        Vec::new(),
        ErrorCategory::Client,
    );
    assert!(
        matches!(
            &long,
            Err(LlmRejectionError::TooLong {
                field: "message",
                limit: 256
            })
        ),
        "a longer text is a payload, not a diagnosis"
    );
}

#[test]
fn a_rejection_prints_its_identity_and_never_its_body() {
    let rejection = llm_failure(422, "unsupported_capability", ErrorCategory::Client);
    let printed = format!("{rejection:?}");

    assert!(printed.contains("unsupported_capability"), "{printed}");
    assert!(printed.contains("body_len"), "{printed}");
    assert!(
        !printed.contains("BODY-MARKER"),
        "the codec's document has no Debug spelling: {printed}"
    );
}

#[test]
fn the_error_carries_the_same_presentation_to_every_clone() {
    // The rejection is shared, not copied: the body reaches the writer without a second allocation,
    // and two failures built from one presentation cannot disagree about the status behind it.
    let rejection = Arc::new(llm_failure(
        429,
        "upstream_rate_limited",
        ErrorCategory::Limit,
    ));
    let first = HttpError::LlmRejected {
        rejection: Arc::clone(&rejection),
    };
    let second = HttpError::LlmRejected {
        rejection: Arc::clone(&rejection),
    };

    assert_eq!(rejection.status(), 429);
    assert_eq!(
        second.respond().expect("renderable").status(),
        first.respond().expect("renderable").status()
    );
    assert_eq!(
        Arc::strong_count(&rejection),
        3,
        "both errors share one value"
    );
}
