//! Tests for the strict JSON reader the token parser uses.
//!
//! Each case is a shape a token could carry, and the reader's job is to accept exactly the ones RFC
//! 8259 defines: an ambiguous document (a repeated key, a leading zero, a YAML document) has no
//! second reading here, it has none at all.

use super::{MAX_DEPTH, Value, parse};

/// Parses a fixture that must be accepted.
fn object(text: &str) -> Value {
    parse(text.as_bytes()).expect("the fixture is valid JSON")
}

/// Asserts that a fixture is refused.
fn refused(text: &str) {
    assert!(
        parse(text.as_bytes()).is_err(),
        "`{text}` must not be accepted"
    );
}

/// Returns the string member `key`, or `None` when the value is another shape.
fn string_at(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

#[test]
fn reads_the_header_a_token_carries() {
    let header = object(r#"{"alg":"HS256","typ":"JWT","kid":"k1"}"#);

    assert_eq!(string_at(&header, "alg").as_deref(), Some("HS256"));
    assert_eq!(string_at(&header, "kid").as_deref(), Some("k1"));
    assert!(header.get("crit").is_none(), "an absent claim is absent");
}

#[test]
fn reads_the_claim_shapes_a_token_carries() {
    let claims = object(
        r#"{"exp":1700000000,"nbf":1699999000.5,"sub":"user","aud":["api","other"],"iss":true}"#,
    );

    assert_eq!(
        claims.get("exp").and_then(Value::as_f64),
        Some(1_700_000_000.0)
    );
    assert_eq!(
        claims.get("nbf").and_then(Value::as_f64),
        Some(1_699_999_000.5)
    );
    let audience = claims
        .get("aud")
        .and_then(Value::as_array)
        .expect("an array audience");
    assert_eq!(
        audience
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>(),
        vec!["api", "other"]
    );
    assert!(
        claims.get("iss").and_then(Value::as_str).is_none(),
        "a boolean is not a string, so a claim never silently reads as one"
    );
}

#[test]
fn resolves_every_escape() {
    let value = object(r#"{"text":"a\"b\\c\/d\be\ff\ng\rh\ti\u00e9\u0041\ud83d\ude00"}"#);

    assert_eq!(
        string_at(&value, "text").as_deref(),
        Some("a\"b\\c/d\u{8}e\u{c}f\ng\rh\ti\u{e9}A\u{1f600}")
    );
}

#[test]
fn an_escaped_key_is_the_claim_it_spells() {
    let value = object(r#"{"\u0061lg":"HS256"}"#);

    assert_eq!(
        string_at(&value, "alg").as_deref(),
        Some("HS256"),
        "an escape in a key must not hide the claim it names"
    );
}

#[test]
fn a_repeated_key_is_refused() {
    // The document has two readings, so it has none: neither `HS256` nor `none` is "the" algorithm.
    refused(r#"{"alg":"HS256","alg":"none"}"#);
}

#[test]
fn an_unpaired_surrogate_is_refused() {
    refused(r#"{"sub":"\ud83d"}"#);
    refused(r#"{"sub":"\ude00"}"#);
    refused(r#"{"sub":"\ud83d\u0041"}"#);
}

#[test]
fn an_unescaped_control_character_is_refused() {
    refused("{\"sub\":\"a\tb\"}");
    refused("{\"sub\":\"a\nb\"}");
}

#[test]
fn invalid_utf8_is_refused() {
    let bytes = b"{\"sub\":\"\xff\"}";
    assert!(
        parse(bytes).is_err(),
        "a token segment that is not UTF-8 has no claim to read"
    );
}

#[test]
fn trailing_data_is_refused() {
    refused("{} {}");
    refused("{\"alg\":\"HS256\"}x");
    refused("  ");
    refused("");
}

#[test]
fn a_document_that_is_not_json_is_refused() {
    // YAML is a superset of JSON; this reader is deliberately not.
    refused("alg: HS256");
    refused("---\n{}");
    refused("{'alg': 'HS256'}");
    refused(r#"{"alg":"HS256",}"#);
    refused(r#"{"alg":"HS256"} # comment"#);
    refused("[1, 2,]");
}

#[test]
fn a_number_must_be_canonical_and_finite() {
    refused(r#"{"exp":01}"#);
    refused(r#"{"exp":+1}"#);
    refused(r#"{"exp":1e999}"#);
    refused(r#"{"exp":.5}"#);
    refused(r#"{"exp":1.}"#);
    refused(r#"{"exp":1e}"#);

    assert_eq!(
        object(r#"{"exp":-1.5e3}"#)
            .get("exp")
            .and_then(Value::as_f64),
        Some(-1500.0)
    );
    assert_eq!(
        object(r#"{"exp":0}"#).get("exp").and_then(Value::as_f64),
        Some(0.0)
    );
}

#[test]
fn nesting_beyond_the_bound_is_refused() {
    let accepted = format!(
        "{{\"a\":{}1{}}}",
        "[".repeat(usize::from(MAX_DEPTH) - 1),
        "]".repeat(usize::from(MAX_DEPTH) - 1)
    );
    assert!(
        parse(accepted.as_bytes()).is_ok(),
        "the deepest accepted shape must still parse"
    );

    let refused = format!(
        "{{\"a\":{}1{}}}",
        "[".repeat(usize::from(MAX_DEPTH)),
        "]".repeat(usize::from(MAX_DEPTH))
    );
    assert!(
        parse(refused.as_bytes()).is_err(),
        "one level deeper must be refused rather than recurse the stack"
    );
}

#[test]
fn whitespace_around_values_is_ignored() {
    let value = object(" \t\r\n{ \"alg\" : \"HS256\" , \"kid\" : \"k1\" }\n");

    assert_eq!(string_at(&value, "alg").as_deref(), Some("HS256"));
    assert_eq!(string_at(&value, "kid").as_deref(), Some("k1"));
}
