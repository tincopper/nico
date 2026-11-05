use std::{io, sync::Arc};

use nico_core::ErrorCategory;
use pingora::http::StatusCode;

use super::{BodyTemplate, HttpError, Rejection};

/// One upstream failure whose cause names a host and a port, so a leak would be visible.
fn upstream_failure() -> HttpError {
    HttpError::UpstreamFailed {
        source: Box::new(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "connect refused to 10.0.0.1:8080",
        )),
    }
}

/// A rejection a policy compiled: a 429 with the built-in envelope.
fn policy_rejection(category: ErrorCategory) -> Rejection {
    Rejection::new(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        "too many requests",
        "application/json",
        category,
        BodyTemplate::error_envelope(),
    )
    .expect("the fixture is a valid rejection")
}

/// One failure this data plane cannot attribute to either side of the request.
fn internal_failure() -> HttpError {
    HttpError::Internal {
        source: Box::new(io::Error::other(
            "no upstream was selected for a routed request",
        )),
    }
}

#[test]
fn every_failing_variant_maps_to_its_documented_status_code_and_category() {
    let cases = [
        (
            HttpError::NotReady,
            StatusCode::SERVICE_UNAVAILABLE,
            "not_ready",
            ErrorCategory::Upstream,
        ),
        (
            HttpError::RouteNotFound,
            StatusCode::NOT_FOUND,
            "route_not_found",
            ErrorCategory::Client,
        ),
        (
            HttpError::InvalidRequest {
                reason: "host header is missing".into(),
            },
            StatusCode::BAD_REQUEST,
            "invalid_request",
            ErrorCategory::Client,
        ),
        (
            HttpError::BodyTooLarge { limit: 1024 },
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
            ErrorCategory::Limit,
        ),
        (
            HttpError::DeadlineExceeded { elapsed_ms: 30_000 },
            StatusCode::GATEWAY_TIMEOUT,
            "upstream_timeout",
            ErrorCategory::Upstream,
        ),
        (
            HttpError::RequestTimeout,
            StatusCode::REQUEST_TIMEOUT,
            "request_timeout",
            ErrorCategory::Client,
        ),
        (
            upstream_failure(),
            StatusCode::BAD_GATEWAY,
            "upstream_unreachable",
            ErrorCategory::Upstream,
        ),
        (
            HttpError::ProviderNotConfigured {
                name: "openai-primary".into(),
            },
            StatusCode::BAD_GATEWAY,
            "provider_unavailable",
            ErrorCategory::Upstream,
        ),
        (
            internal_failure(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            ErrorCategory::Internal,
        ),
        (
            HttpError::Rejected {
                rejection: Arc::new(policy_rejection(ErrorCategory::Limit)),
                detail: "the bucket is empty",
                retry_after: Some(std::time::Duration::from_millis(900)),
            },
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            ErrorCategory::Limit,
        ),
    ];

    for (error, status, code, category) in cases {
        let respond = error
            .respond()
            .unwrap_or_else(|| panic!("{code} must be renderable as a response"));
        assert_eq!(respond.status(), status, "status for {code}");
        assert_eq!(respond.code(), code);
        assert_eq!(error.category(), category, "category for {code}");
    }
}

#[test]
fn a_committed_response_has_neither_a_response_nor_a_client_fault() {
    let error = HttpError::ResponseCommitted;

    assert!(error.respond().is_none(), "there is nowhere to write it");
    assert_eq!(error.category(), ErrorCategory::Internal);
}

#[test]
fn the_body_has_the_documented_envelope() {
    let respond = HttpError::NotReady
        .respond()
        .expect("not_ready is renderable");

    assert_eq!(
        respond.body("trace-1"),
        r#"{"error":{"code":"not_ready","message":"the gateway has no configuration to serve yet","trace_id":"trace-1"}}"#
    );
}

#[test]
fn the_body_never_repeats_the_upstream_cause() {
    let respond = upstream_failure()
        .respond()
        .expect("an upstream failure is renderable");

    let body = respond.body("trace-2");
    let body = std::str::from_utf8(&body).expect("body is UTF-8");
    assert!(body.contains("upstream_unreachable"));
    assert!(
        !body.contains("10.0.0.1"),
        "the upstream address must stay in logs: {body}"
    );
    assert!(
        !body.contains("connect refused"),
        "the source text must stay in logs: {body}"
    );
}

#[test]
fn the_body_never_repeats_an_invalid_request_reason() {
    let error = HttpError::InvalidRequest {
        reason: "header x-api-key is malformed".into(),
    };
    let respond = error.respond().expect("an invalid request is renderable");

    let body = respond.body("trace-3");
    let body = std::str::from_utf8(&body).expect("body is UTF-8");
    assert!(
        !body.contains("x-api-key"),
        "a rejected credential name must not be echoed: {body}"
    );
}

#[test]
fn a_trace_id_from_a_request_header_is_json_escaped() {
    let respond = HttpError::RouteNotFound.respond().expect("renderable");

    let body = respond.body("a\"b\\c\nd\u{1}");
    let body = std::str::from_utf8(&body).expect("body is UTF-8");

    assert!(
        body.contains(r#""trace_id":"a\"b\\c\nd\u0001""#),
        "attacker-controlled trace id must be escaped: {body}"
    );
}

#[test]
fn the_exported_escaper_is_the_one_the_renderer_uses() {
    // A layer that compiles a JSON template from configuration needs exactly this escaping, and it
    // must not grow a second escaper: the same input has to produce the same bytes here as inside a
    // rendered body.
    let respond = HttpError::RouteNotFound.respond().expect("renderable");
    let value = "a\"b\\c\nd\u{1}";

    let escaped = super::escape_json_string(value);
    let body = respond.body(value);
    let body = std::str::from_utf8(&body).expect("body is UTF-8");

    assert_eq!(escaped, r#"a\"b\\c\nd\u0001"#);
    assert!(
        body.contains(&format!(r#""trace_id":"{escaped}""#)),
        "one escaper, one spelling: {body}"
    );
}

#[test]
fn the_exported_escaper_leaves_plain_text_unchanged() {
    assert_eq!(super::escape_json_string("auth_invalid"), "auth_invalid");
    assert_eq!(super::escape_json_string(""), "");
}

#[test]
fn the_body_never_repeats_an_internal_cause() {
    let respond = internal_failure().respond().expect("renderable");

    let body = respond.body("trace-4");
    let body = std::str::from_utf8(&body).expect("body is UTF-8");
    assert!(body.contains("internal_error"));
    assert!(
        !body.contains("no upstream was selected"),
        "internal detail stays in logs: {body}"
    );
}

#[test]
fn a_display_message_quotes_the_limit_the_check_applied() {
    let error = HttpError::BodyTooLarge { limit: 1_048_576 };

    assert_eq!(
        error.to_string(),
        "request body exceeds the 1048576 byte limit"
    );
}

#[test]
fn a_rejection_reports_the_category_its_plugin_declared_not_the_status_it_renders() {
    // A document may override the status; the log category must keep describing the plugin, so a rate
    // limit configured as a 403 is still a limit and not a client error.
    let error = HttpError::Rejected {
        rejection: Arc::new(
            Rejection::new(
                StatusCode::FORBIDDEN,
                "rate_limited",
                "too many requests",
                "application/json",
                ErrorCategory::Limit,
                BodyTemplate::error_envelope(),
            )
            .expect("the fixture is a valid rejection"),
        ),
        detail: "the bucket is empty",
        retry_after: None,
    };

    assert_eq!(error.category(), ErrorCategory::Limit);
    assert_eq!(
        error.respond().map(|respond| respond.status()),
        Some(StatusCode::FORBIDDEN)
    );
}

#[test]
fn a_rejection_carries_its_retry_after_to_the_writer() {
    let error = HttpError::Rejected {
        rejection: Arc::new(policy_rejection(ErrorCategory::Limit)),
        detail: "the bucket is empty",
        retry_after: Some(std::time::Duration::from_millis(900)),
    };

    assert_eq!(
        error.respond().and_then(|respond| respond.retry_after()),
        Some(std::time::Duration::from_millis(900))
    );
}
