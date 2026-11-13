//! Parse-level tests for the route attempt fields: what a route that declares none gets.

use super::*;

#[test]
fn a_route_without_attempt_fields_defaults_to_one_attempt_and_no_fallback() {
    let document = "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1048576
  timeout_ms: 30000
plugins: []
providers:
  - name: openai-primary
    kind: openai
    base_url: https://api.openai.com
    credential_ref: env:OPENAI_API_KEY
upstreams: []
routes:
  - id: llm-chat
    path_prefix: /v1/chat/
    provider: openai-primary
    protocol: openai.chat
";
    let document =
        Document::parse(document.as_bytes(), ParseLimits::new(4096)).expect("fixture is valid");
    let route = &document.routes()[0];
    assert_eq!(
        route.retry_budget(),
        1,
        "one attempt, no retry, is the behavior of a route that never names the field"
    );
    assert!(
        route.fallback().is_empty(),
        "a route without a chain has no fallback"
    );
}

#[test]
fn a_route_with_declared_attempts_carries_them_as_written() {
    let document = "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1048576
  timeout_ms: 30000
plugins: []
providers:
  - name: openai-primary
    kind: openai
    base_url: https://api.openai.com
    credential_ref: env:OPENAI_API_KEY
  - name: openai-secondary
    kind: openai
    base_url: https://api.openai.com
    credential_ref: env:OPENAI_API_KEY
upstreams: []
routes:
  - id: llm-chat
    path_prefix: /v1/chat/
    provider: openai-primary
    protocol: openai.chat
    retry_budget: 3
    fallback: [openai-secondary]
";
    let document =
        Document::parse(document.as_bytes(), ParseLimits::new(4096)).expect("fixture is valid");
    let route = &document.routes()[0];
    assert_eq!(route.retry_budget(), 3);
    assert_eq!(route.fallback(), ["openai-secondary"]);
}

/// A document whose only route is a converting provider route carrying `extra` lines.
fn with_route_fields(extra: &str) -> String {
    format!(
        "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1048576
  timeout_ms: 30000
plugins: []
providers:
  - name: primary
    kind: openai
    base_url: https://api.example.test
    credential_ref: env:KEY
upstreams: []
routes:
  - id: llm-chat
    path_prefix: /v1/chat/
    provider: primary
    protocol: openai.chat
{extra}"
    )
}

#[test]
fn a_route_without_a_ttft_budget_has_none() {
    let document = Document::parse(with_route_fields("").as_bytes(), ParseLimits::new(4096))
        .expect("fixture is valid");

    assert_eq!(document.routes()[0].ttft_timeout_ms(), None);
}

#[test]
fn a_ttft_budget_is_read_from_the_document() {
    let document = Document::parse(
        with_route_fields("    ttft_timeout_ms: 2500\n").as_bytes(),
        ParseLimits::new(4096),
    )
    .expect("fixture is valid");

    assert_eq!(document.routes()[0].ttft_timeout_ms(), Some(2500));
}

#[test]
fn a_zero_ttft_budget_is_refused() {
    let error = Document::parse(
        with_route_fields("    ttft_timeout_ms: 0\n").as_bytes(),
        ParseLimits::new(4096),
    )
    .expect_err("a zero first-byte budget would fire before any attempt");

    assert!(
        matches!(
            error,
            DocumentError::LimitOutOfRange {
                field: "routes[].ttft_timeout_ms",
                value: 0
            }
        ),
        "expected LimitOutOfRange, got {error:?}"
    );
}

#[test]
fn a_ttft_budget_above_its_ceiling_is_refused() {
    // The ceiling is half the whole-request one: a first-byte wait that approaches the request's own
    // deadline could never fire.
    let error = Document::parse(
        with_route_fields("    ttft_timeout_ms: 300001\n").as_bytes(),
        ParseLimits::new(4096),
    )
    .expect_err("above the first-byte ceiling");

    assert!(
        matches!(
            error,
            DocumentError::LimitOutOfRange {
                field: "routes[].ttft_timeout_ms",
                value: 300_001
            }
        ),
        "expected LimitOutOfRange, got {error:?}"
    );
}

#[test]
fn a_ttft_budget_on_an_upstream_route_is_refused() {
    // An upstream route forwards bytes and has no provider stream to cut, so the field would be read
    // as configuration that does nothing.
    let document = "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1048576
  timeout_ms: 30000
plugins: []
providers: []
upstreams:
  - name: api-backend
    scheme: http
    host: 127.0.0.1
    port: 8080
routes:
  - id: api
    path_prefix: /api/
    upstream: api-backend
    ttft_timeout_ms: 1000
";
    let error = Document::parse(document.as_bytes(), ParseLimits::new(4096))
        .expect_err("an upstream route owns no stream");

    assert!(
        matches!(
            error,
            DocumentError::InvalidField {
                field: "routes[].ttft_timeout_ms",
                ..
            }
        ),
        "expected InvalidField, got {error:?}"
    );
}
