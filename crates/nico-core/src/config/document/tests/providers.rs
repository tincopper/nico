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
