//! Parse-level tests for one provider's own fields.
//!
//! The provider model is validated in two places: shape while the document is assembled, and the
//! per-field rules in `semantic`. These tests assert the assembly half — what a document's provider
//! entry looks like once parsed — for the fields that are endpoint policy rather than routing.

use super::*;

#[test]
fn parse_defaults_an_omitted_max_output_tokens_to_none() {
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
routes: []
";
    let document =
        Document::parse(document.as_bytes(), ParseLimits::new(4096)).expect("fixture is valid");
    assert_eq!(
        document.providers()[0].max_output_tokens(),
        None,
        "an endpoint that names no ceiling imposes none"
    );
}

#[test]
fn parse_preserves_an_explicit_max_output_tokens() {
    // `u32::MAX` is a legal ceiling: the field's rule is "a positive count", not "a small one",
    // and the largest value must survive the document as written.
    for written in [4096, u32::MAX] {
        let document = format!(
            "\
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
    max_output_tokens: {written}
upstreams: []
routes: []
"
        );
        let document =
            Document::parse(document.as_bytes(), ParseLimits::new(4096)).expect("fixture is valid");
        assert_eq!(
            document.providers()[0].max_output_tokens(),
            Some(written),
            "the endpoint's ceiling is the document's value, not a gateway default"
        );
    }
}

#[test]
fn a_provider_without_a_breaker_section_takes_the_built_in_defaults() {
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
routes: []
";
    let document =
        Document::parse(document.as_bytes(), ParseLimits::new(4096)).expect("fixture is valid");
    let breaker = document.providers()[0].circuit_breaker();
    assert_eq!(breaker.failure_threshold(), 5);
    assert_eq!(breaker.cooldown_ms(), 30_000);
}

#[test]
fn a_breaker_section_fills_only_the_fields_the_document_left_out() {
    // One knob tightened must not restate the other: the defaults are per field, which is what
    // makes `failure_threshold: 0` (disable) usable without freezing the cooldown too.
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
    circuit_breaker:
      failure_threshold: 0
upstreams: []
routes: []
";
    let document =
        Document::parse(document.as_bytes(), ParseLimits::new(4096)).expect("fixture is valid");
    let breaker = document.providers()[0].circuit_breaker();
    assert_eq!(breaker.failure_threshold(), 0);
    assert_eq!(breaker.cooldown_ms(), 30_000);
}

#[test]
fn a_cooldown_only_breaker_keeps_the_default_threshold() {
    // The symmetric half of the per-field defaults: tuning the wait must not restate the count.
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
    circuit_breaker:
      cooldown_ms: 60000
upstreams: []
routes: []
";
    let document =
        Document::parse(document.as_bytes(), ParseLimits::new(4096)).expect("fixture is valid");
    let breaker = document.providers()[0].circuit_breaker();
    assert_eq!(breaker.failure_threshold(), 5);
    assert_eq!(breaker.cooldown_ms(), 60_000);
}
