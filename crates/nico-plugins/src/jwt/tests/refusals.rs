//! The token shapes the verifier refuses, one reason at a time.
//!
//! Each case names the fixed sentence the refusal logs, because that sentence is the only place the
//! cause exists: the error code a caller sees is the same for all of them.

use super::*;

#[test]
fn an_expired_token_is_refused() {
    let expired = token(Some("k1"), NOW as i64 - 31, "");

    assert_eq!(refusal(&one_key(), &expired), "the token has expired");
}

#[test]
fn a_token_without_exp_is_refused() {
    let forever = sign(
        r#"{"alg":"HS256","kid":"k1"}"#,
        r#"{"sub":"user-42"}"#,
        SECRET,
    );

    assert_eq!(refusal(&one_key(), &forever), "the token carries no expiry");
}

#[test]
fn a_token_that_is_not_valid_yet_is_refused() {
    let future = token(
        Some("k1"),
        NOW as i64 + 3600,
        &format!(r#","nbf":{}"#, NOW + 60),
    );

    assert_eq!(refusal(&one_key(), &future), "the token is not valid yet");
}

#[test]
fn a_bad_signature_is_refused() {
    let forged = sign(
        r#"{"alg":"HS256","kid":"k1"}"#,
        &format!(r#"{{"exp":{},"sub":"user-42"}}"#, NOW + 60),
        b"another-secret",
    );

    assert_eq!(
        refusal(&one_key(), &forged),
        "the token signature does not verify"
    );
}

#[test]
fn alg_none_is_refused_before_any_key_is_considered() {
    // The classic algorithm-confusion token: `none` with an empty signature.
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none","kid":"k1"}"#);
    let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{},"sub":"user-42"}}"#, NOW + 60));

    assert_eq!(
        refusal(&one_key(), &format!("{header}.{payload}.")),
        "the token does not use HS256"
    );
}

#[test]
fn another_algorithm_is_refused_even_with_a_valid_hmac_signature() {
    // A verifier that selected a key before checking `alg` would accept this: the HMAC is valid,
    // only the declared algorithm is not one this build implements.
    let substituted = sign(
        r#"{"alg":"RS256","kid":"k1"}"#,
        &format!(r#"{{"exp":{},"sub":"user-42"}}"#, NOW + 60),
        SECRET,
    );

    assert_eq!(
        refusal(&one_key(), &substituted),
        "the token does not use HS256"
    );
}

#[test]
fn a_multi_key_set_refuses_a_token_without_a_kid() {
    let filter = filter(&entry_with_keys(""), &[SECRET, b"the-second-secret"]);

    assert_eq!(
        refusal(&filter, &token(None, NOW as i64 + 60, "")),
        "the token names no key this plugin accepts"
    );
}

#[test]
fn an_unknown_kid_is_refused() {
    assert_eq!(
        refusal(&one_key(), &token(Some("k9"), NOW as i64 + 60, "")),
        "the token names no key this plugin accepts"
    );
    let two = filter(&entry_with_keys(""), &[SECRET, b"the-second-secret"]);
    assert_eq!(
        refusal(&two, &token(Some("k9"), NOW as i64 + 60, "")),
        "the token names no key this plugin accepts"
    );
}

#[test]
fn a_single_key_set_refuses_a_kid_it_does_not_pin() {
    let filter = filter(&entry_with(""), &[SECRET]);
    let keyless = sign(
        r#"{"alg":"HS256"}"#,
        &format!(r#"{{"exp":{},"sub":"user-42"}}"#, NOW + 60),
        SECRET,
    );

    assert!(
        filter.authenticate(&keyless, now()).is_ok(),
        "the keyless entry accepts a token that names no key"
    );
    assert_eq!(
        refusal(&filter, &token(Some("k2"), NOW as i64 + 60, "")),
        "the token names no key this plugin accepts"
    );
}

#[test]
fn an_oversized_token_is_refused_without_decoding_it() {
    let huge = format!("{}.{}.{}", "a".repeat(8192), "b", "c");

    assert_eq!(
        refusal(&one_key(), &huge),
        "the token is too large to be accepted"
    );
}

#[test]
fn a_token_that_is_not_three_segments_is_refused() {
    for shape in ["", "one", "one.two", "one.two.three.four", "..."] {
        assert_eq!(
            refusal(&one_key(), shape),
            "the token is not a well-formed JWT",
            "`{shape}` must be refused as malformed"
        );
    }
}

#[test]
fn malformed_base64_is_refused() {
    for shape in ["not base64.not base64.not base64", "e30.e30.!!!", "e30.e30"] {
        assert_eq!(
            refusal(&one_key(), shape),
            "the token is not a well-formed JWT",
            "`{shape}` must be refused as malformed"
        );
    }
}

#[test]
fn a_token_whose_segments_are_not_json_is_refused() {
    let header = URL_SAFE_NO_PAD.encode("not json");
    let payload = URL_SAFE_NO_PAD.encode("not json");
    let signature = URL_SAFE_NO_PAD.encode("signature");

    assert_eq!(
        refusal(&one_key(), &format!("{header}.{payload}.{signature}")),
        "the token is not a well-formed JWT"
    );
}

#[test]
fn a_token_without_a_usable_subject_is_refused() {
    let missing = sign(
        r#"{"alg":"HS256","kid":"k1"}"#,
        &format!(r#"{{"exp":{}}}"#, NOW + 60),
        SECRET,
    );
    assert_eq!(
        refusal(&one_key(), &missing),
        "the token carries no subject"
    );

    let empty = sign(
        r#"{"alg":"HS256","kid":"k1"}"#,
        &format!(r#"{{"exp":{},"sub":""}}"#, NOW + 60),
        SECRET,
    );
    assert_eq!(refusal(&one_key(), &empty), "the token carries no subject");

    let long = sign(
        r#"{"alg":"HS256","kid":"k1"}"#,
        &format!(r#"{{"exp":{},"sub":"{}"}}"#, NOW + 60, "u".repeat(257)),
        SECRET,
    );
    assert_eq!(refusal(&one_key(), &long), "the token subject is too large");
}
