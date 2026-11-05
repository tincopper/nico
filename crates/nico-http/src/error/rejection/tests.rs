//! Tests for the compiled rejection presentation: what a constructor refuses, and how the body is
//! rendered.
//!
//! The wire-level header set is `proxy::failure`'s subject; what can be asserted here is the value a
//! writer is handed: status, stable code, content type, challenge, and the bytes of the body.

use std::sync::Arc;

use nico_core::ErrorCategory;
use pingora::http::StatusCode;

use super::{BodyFormat, BodyTemplate, MAX_LITERAL_BYTES, Part, Rejection, RejectionError};
use crate::error::HttpError;

/// The built-in JSON body with `message` substituted, as a policy compiles it.
fn json_body() -> BodyTemplate {
    BodyTemplate::error_envelope()
}

/// The default authentication rejection: 401, a challenge, the built-in envelope.
fn unauthorized(code: &'static str) -> Rejection {
    Rejection::new(
        StatusCode::UNAUTHORIZED,
        code,
        "the request is not authenticated",
        "application/json",
        ErrorCategory::Client,
        json_body(),
    )
    .expect("the fixture is a valid rejection")
    .with_challenge("Bearer")
    .expect("`Bearer` is a valid challenge")
}

#[test]
fn the_default_authentication_refusal_renders_both_codes() {
    for code in ["auth_missing", "auth_invalid"] {
        let rejection = unauthorized(code);
        let respond = HttpError::Rejected {
            rejection: Arc::new(rejection),
            detail: "the token was rejected",
            retry_after: None,
        }
        .respond()
        .expect("a rejection is renderable");

        assert_eq!(
            respond.status(),
            StatusCode::UNAUTHORIZED,
            "status of {code}"
        );
        assert_eq!(respond.code(), code);
        assert_eq!(respond.content_type(), "application/json");
        assert_eq!(
            respond.challenge(),
            Some("Bearer"),
            "a 401 tells the caller how to authenticate"
        );
        assert_eq!(respond.retry_after(), None);
        assert_eq!(
            respond.body("trace-1"),
            format!(
                r#"{{"error":{{"code":"{code}","message":"the request is not authenticated","trace_id":"trace-1"}}}}"#
            )
        );
    }
}

#[test]
fn a_custom_json_refusal_renders_a_structured_body() {
    let body = BodyTemplate::new(
        BodyFormat::Json,
        vec![
            Part::Literal(r#"{"error":{"code":""#.into()),
            Part::Code,
            Part::Literal(r#"","detail":"denied","trace_id":""#.into()),
            Part::TraceId,
            Part::Literal(r#""}}"#.into()),
        ],
    )
    .expect("the fixture template is small enough");
    let rejection = Rejection::new(
        StatusCode::FORBIDDEN,
        "forbidden_by_policy",
        "the policy refused the request",
        "application/json",
        ErrorCategory::Client,
        body,
    )
    .expect("the fixture is a valid rejection");

    let respond = HttpError::Rejected {
        rejection: Arc::new(rejection),
        detail: "a policy refused the request",
        retry_after: None,
    }
    .respond()
    .expect("a rejection is renderable");

    assert_eq!(respond.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        respond.body("trace-2"),
        r#"{"error":{"code":"forbidden_by_policy","detail":"denied","trace_id":"trace-2"}}"#
    );
}

#[test]
fn a_custom_text_refusal_renders_literals_and_variables_unquoted() {
    let body = BodyTemplate::new(
        BodyFormat::Text,
        vec![
            Part::Literal("<h1>".into()),
            Part::Code,
            Part::Literal("</h1><p>trace ".into()),
            Part::TraceId,
            Part::Literal("</p>".into()),
        ],
    )
    .expect("the fixture template is small enough");
    let rejection = Rejection::new(
        StatusCode::FORBIDDEN,
        "blocked",
        "blocked by policy",
        "text/html",
        ErrorCategory::Client,
        body,
    )
    .expect("the fixture is a valid rejection");

    assert_eq!(
        rejection.render("trace-3"),
        "<h1>blocked</h1><p>trace trace-3</p>"
    );
}

#[test]
fn a_non_4xx_status_is_refused() {
    for status in [
        StatusCode::OK,
        StatusCode::MOVED_PERMANENTLY,
        StatusCode::INTERNAL_SERVER_ERROR,
    ] {
        let error = Rejection::new(
            status,
            "code",
            "message",
            "application/json",
            ErrorCategory::Client,
            json_body(),
        )
        .expect_err("only a 4xx describes a rejection");

        assert_eq!(
            error,
            RejectionError::StatusNotClientError {
                status: status.as_u16()
            },
            "status {status} must be refused"
        );
    }
}

#[test]
fn every_4xx_status_is_accepted() {
    for status in 400..=499_u16 {
        let status = StatusCode::from_u16(status).expect("the range holds only valid codes");
        assert!(
            Rejection::new(
                status,
                "code",
                "message",
                "application/json",
                ErrorCategory::Client,
                json_body(),
            )
            .is_ok(),
            "status {status} must be accepted"
        );
    }
}

#[test]
fn an_empty_or_unprintable_header_value_is_refused() {
    for (field, code, content_type) in [
        ("code", "", "application/json"),
        ("code", "auth\ninjected", "application/json"),
        ("content_type", "code", ""),
        ("content_type", "code", "text/html\r\nx-injected: 1"),
    ] {
        let error = Rejection::new(
            StatusCode::UNAUTHORIZED,
            code,
            "message",
            content_type,
            ErrorCategory::Client,
            json_body(),
        )
        .expect_err("a header value must be printable and non-empty");

        assert!(
            matches!(error, RejectionError::Empty { field: broken } | RejectionError::NotPrintable { field: broken } if broken == field),
            "expected a {field} rejection, got {error:?}"
        );
    }
}

#[test]
fn a_challenge_cannot_smuggle_a_second_header() {
    let error = unauthorized("auth_missing")
        .with_challenge("Bearer\r\nx-nico-error-code: forged")
        .expect_err("a challenge is one header value");

    assert_eq!(error, RejectionError::NotPrintable { field: "challenge" });
}

#[test]
fn a_template_literal_is_text_so_it_cannot_name_a_variable() {
    // The whitelist is the `Part` enum: there is no syntax a template could use to name a variable
    // outside it, so `{unknown}` stays exactly what the operator wrote.
    let body = BodyTemplate::new(
        BodyFormat::Text,
        vec![Part::Literal(
            "{unknown} and {code} are not both variables".into(),
        )],
    )
    .expect("the fixture template is small enough");

    assert_eq!(
        body.render("code", "message", "trace"),
        "{unknown} and {code} are not both variables",
        "nothing in a literal is substituted"
    );
}

#[test]
fn a_template_over_the_literal_bound_is_refused() {
    // Ten bytes per part, one part more than the bound allows: the total is what is bounded, not one
    // literal, so a compiled template cannot dodge the limit by being split up.
    let parts = vec![Part::Literal("0123456789".into()); MAX_LITERAL_BYTES / 10 + 1];

    let error = BodyTemplate::new(BodyFormat::Text, parts)
        .expect_err("the literal bound exists so a rejection cannot allocate without limit");

    assert_eq!(
        error,
        RejectionError::TemplateTooLarge {
            limit: MAX_LITERAL_BYTES
        }
    );
}

#[test]
fn a_json_template_escapes_every_variable() {
    let rejection = unauthorized("auth_invalid");

    let body = rejection.render("a\"b\\c\nd\u{1}");
    let body = std::str::from_utf8(&body).expect("the body is UTF-8");

    assert!(
        body.contains(r#""trace_id":"a\"b\\c\nd\u0001""#),
        "a caller-controlled value must be escaped: {body}"
    );
}

#[test]
fn the_category_travels_with_the_rejection_rather_than_the_status() {
    let limit = Rejection::new(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        "too many requests",
        "application/json",
        ErrorCategory::Limit,
        json_body(),
    )
    .expect("the fixture is a valid rejection");
    // A document may override the status; the category must still describe the *plugin*, so a rate
    // limit configured as a 403 is still a limit and an authentication failure is still the caller's
    // fault.
    let limit_as_forbidden = Rejection::new(
        StatusCode::FORBIDDEN,
        "rate_limited",
        "too many requests",
        "application/json",
        ErrorCategory::Limit,
        json_body(),
    )
    .expect("the fixture is a valid rejection");

    assert_eq!(limit.category(), ErrorCategory::Limit);
    assert_eq!(limit_as_forbidden.category(), ErrorCategory::Limit);
    assert_eq!(
        HttpError::Rejected {
            rejection: Arc::new(unauthorized("auth_missing")),
            detail: "no credential",
            retry_after: None,
        }
        .category(),
        ErrorCategory::Client
    );
}

#[test]
fn a_display_message_never_renders_the_rejection_body_or_a_credential() {
    let error = HttpError::Rejected {
        rejection: Arc::new(unauthorized("auth_invalid")),
        detail: "the token was rejected",
        retry_after: None,
    };
    let rendered = error.to_string();

    assert!(
        rendered.contains("the token was rejected"),
        "got {rendered}"
    );
    assert!(
        !rendered.contains("trace") && !rendered.contains("Bearer"),
        "the detail is the only text a rejection adds: {rendered}"
    );
}
