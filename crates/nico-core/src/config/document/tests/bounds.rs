//! Tests for the document-level byte bounds: `request.max_response_bytes`.
//!
//! The body bound is covered by the older parse tests; this module covers the response bound, whose
//! distinguishing property is that it has a default instead of being required.

use super::*;

/// A complete document whose `request` section carries `max_response_bytes`.
fn document_with_response_bound(max_response_bytes: u32) -> String {
    format!(
        "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1048576
  max_response_bytes: {max_response_bytes}
  timeout_ms: 1000
plugins: []
providers: []
upstreams: []
routes: []
"
    )
}

#[test]
fn an_omitted_response_bound_takes_the_eight_mebibyte_default() {
    // `VALID` predates the field: a document written before it existed must keep parsing, and it
    // must get the default rather than an unbounded or zero value.
    let document = parse_valid();
    assert_eq!(
        document.request().max_response_bytes(),
        DEFAULT_MAX_RESPONSE_BYTES
    );
    assert_eq!(DEFAULT_MAX_RESPONSE_BYTES, 8 * 1024 * 1024);
}

#[test]
fn an_explicit_response_bound_overrides_the_default() {
    let document = document_with_response_bound(1024 * 1024);
    let document =
        Document::parse(document.as_bytes(), ParseLimits::new(4096)).expect("fixture is valid");
    assert_eq!(document.request().max_response_bytes(), 1_048_576);
}

#[test]
fn a_zero_response_bound_is_legal() {
    let document = document_with_response_bound(0);
    let parsed = Document::parse(document.as_bytes(), ParseLimits::new(4096))
        .expect("0 means: only an empty response body fits");
    assert_eq!(parsed.request().max_response_bytes(), 0);
}

#[test]
fn the_largest_response_bound_is_legal() {
    // `u32` is the only boundary of the field, exactly as it is for `max_body_bytes`: the operator
    // who writes the ceiling also accepts the memory it allows.
    let document = document_with_response_bound(u32::MAX);
    let parsed = Document::parse(document.as_bytes(), ParseLimits::new(4096))
        .expect("u32 is the only boundary of max_response_bytes");
    assert_eq!(parsed.request().max_response_bytes(), u32::MAX);
}
