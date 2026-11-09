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
