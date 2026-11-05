//! Tests for the parts of the writer that decide a value rather than write it.
//!
//! The header set itself is asserted end to end (`nico-server/tests/plugins_*`): a `Session` needs a
//! connection, so a unit test can only reach the arithmetic and the mapping that feed it.

use std::time::Duration;

use super::{retry_after_seconds, synthetic_head};
use crate::ResponseData;

#[test]
fn a_wait_rounds_up_to_whole_seconds() {
    assert_eq!(retry_after_seconds(Duration::from_millis(1)), 1);
    assert_eq!(retry_after_seconds(Duration::from_millis(900)), 1);
    assert_eq!(retry_after_seconds(Duration::from_millis(1_000)), 1);
    assert_eq!(retry_after_seconds(Duration::from_millis(1_500)), 2);
    assert_eq!(retry_after_seconds(Duration::from_secs(7)), 7);
}

#[test]
fn a_zero_wait_stays_zero() {
    assert_eq!(
        retry_after_seconds(Duration::ZERO),
        0,
        "a bucket that has a token available needs no wait, and `0` says exactly that"
    );
}

#[test]
fn the_largest_wait_saturates_instead_of_overflowing() {
    // A public `PluginError::Refused` may carry any duration. `Duration::MAX` is all whole seconds
    // plus a nanosecond, so rounding up must not wrap to `0` — that would invite an immediate retry.
    assert_eq!(retry_after_seconds(Duration::MAX), u64::MAX);
}

/// Returns one header value as text, for the assertions below.
fn value(header: &pingora::http::ResponseHeader, name: &str) -> Option<String> {
    header
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

#[test]
fn a_synthesised_response_recomputes_its_length_and_keeps_its_headers() {
    let data = ResponseData::new(
        200,
        vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            ("content-length".to_owned(), "9999".to_owned()),
            ("set-cookie".to_owned(), "a=1".to_owned()),
            ("set-cookie".to_owned(), "b=2".to_owned()),
        ],
        b"{\"ok\":true}".to_vec(),
    )
    .expect("the fixture response is valid");

    let (header, body) = synthetic_head(data, "trace-7").expect("the head builds");

    assert_eq!(
        value(&header, "content-length").as_deref(),
        Some("11"),
        "the length the plugin wrote is replaced by the one it produced"
    );
    assert_eq!(
        value(&header, "content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(
        value(&header, "x-nico-trace-id").as_deref(),
        Some("trace-7")
    );
    let cookies: Vec<String> = header
        .headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok().map(str::to_owned))
        .collect();
    assert_eq!(cookies, ["a=1", "b=2"], "repeated headers survive cleaning");
    assert_eq!(body.len(), 11);
}

#[test]
fn a_synthesised_response_drops_the_challenge_and_the_wait() {
    // Both are consequences of a typed refusal; a plugin that wants a `www-authenticate` challenge
    // refuses instead of answering, which is also the only way to keep the stable code contract.
    let data = ResponseData::new(
        200,
        vec![
            ("www-authenticate".to_owned(), "Bearer".to_owned()),
            ("retry-after".to_owned(), "5".to_owned()),
        ],
        Vec::new(),
    )
    .expect("the fixture response is valid");

    let (header, body) = synthetic_head(data, "trace-8").expect("the head builds");

    assert!(value(&header, "www-authenticate").is_none());
    assert!(value(&header, "retry-after").is_none());
    assert!(body.is_empty());
    assert!(
        value(&header, "x-nico-error-code").is_none(),
        "a response that is not a failure has no code to report unless its plugin supplied one"
    );
}

#[test]
fn a_status_that_cannot_carry_a_body_is_written_without_one() {
    // `ResponseData::new` refuses a body for these statuses; the writer must agree even if a future
    // constructor did not, so the cleaning step is what this asserts.
    let data = ResponseData::new(204, Vec::new(), Vec::new()).expect("204 carries no body");

    let (header, body) = synthetic_head(data, "trace-9").expect("the head builds");

    assert_eq!(header.status.as_u16(), 204);
    assert_eq!(value(&header, "content-length").as_deref(), Some("0"));
    assert!(body.is_empty());
}
