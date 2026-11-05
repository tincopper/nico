//! What a plugin may say, and what the data plane actually writes.

use pingora::http::StatusCode;

use super::{ResponseData, ResponseError, clean_response};

/// Builds a response whose headers are `headers`, with a two-byte body.
fn response(status: u16, headers: &[(&str, &str)]) -> ResponseData {
    ResponseData::new(
        status,
        headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
        b"ok".to_vec(),
    )
    .expect("the fixture response is valid")
}

/// Returns the names of the headers that survived cleaning, in order.
fn kept_names(data: ResponseData) -> Vec<String> {
    clean_response(data)
        .headers()
        .iter()
        .map(|(name, _)| name.to_ascii_lowercase())
        .collect()
}

#[test]
fn a_well_formed_response_is_accepted_as_written() {
    let data = response(200, &[("content-type", "application/json")]);

    assert_eq!(data.status(), 200);
    assert_eq!(
        data.headers(),
        [("content-type".to_owned(), "application/json".to_owned())]
    );
    assert_eq!(data.body(), b"ok");
}

#[test]
fn a_status_outside_the_http_range_is_rejected() {
    for status in [0, 99, 600, u16::MAX] {
        let error = ResponseData::new(status, Vec::new(), Vec::new())
            .expect_err("an out-of-range status is not an HTTP status");
        assert_eq!(error, ResponseError::Status { status });
    }
}

#[test]
fn a_status_that_carries_no_body_refuses_one() {
    for status in [100, 101, 103, 204, 304] {
        let error = ResponseData::new(status, Vec::new(), b"body".to_vec())
            .expect_err("this status cannot carry a body");
        assert_eq!(error, ResponseError::BodyNotAllowed { status });

        ResponseData::new(status, Vec::new(), Vec::new())
            .expect("the same status without a body is a valid response");
    }
}

#[test]
fn a_header_name_must_be_a_token_and_a_value_must_be_a_field_value() {
    let error = ResponseData::new(
        200,
        vec![("bad name".to_owned(), "1".to_owned())],
        Vec::new(),
    )
    .expect_err("a name with a space cannot reach the wire");
    assert!(
        matches!(error, ResponseError::InvalidHeader { reason, .. } if reason.contains("token"))
    );

    let error = ResponseData::new(
        200,
        vec![("x-note".to_owned(), "a\r\nb".to_owned())],
        Vec::new(),
    )
    .expect_err("a CR LF in a value is a response-splitting bug");
    assert!(matches!(error, ResponseError::InvalidHeader { name, .. } if name == "x-note"));
}

#[test]
fn the_data_plane_recomputes_framing_and_drops_hop_by_hop_headers() {
    let kept = kept_names(response(
        200,
        &[
            ("content-type", "application/json"),
            ("content-length", "2"),
            ("transfer-encoding", "chunked"),
            ("connection", "keep-alive"),
            ("location", "/elsewhere"),
            ("etag", "\"v1\""),
        ],
    ));

    assert_eq!(kept, ["content-type", "location", "etag"]);
}

#[test]
fn a_connection_header_nominates_the_headers_it_names() {
    let kept = kept_names(response(
        200,
        &[
            ("connection", "x-private, Keep-Out"),
            ("x-private", "1"),
            ("keep-out", "2"),
            ("x-public", "3"),
        ],
    ));

    assert_eq!(
        kept,
        ["x-public"],
        "both nominating spellings drop the header they name"
    );
}

#[test]
fn a_plugin_cannot_forge_the_refusal_headers() {
    let kept = kept_names(response(
        401,
        &[
            ("www-authenticate", "Bearer"),
            ("retry-after", "3"),
            ("x-nico-error-code", "auth_invalid"),
        ],
    ));

    assert_eq!(
        kept,
        ["x-nico-error-code"],
        "a challenge and a wait belong to a typed refusal, not to a returned response"
    );
}

#[test]
fn repeated_values_survive_in_order() {
    let cleaned = clean_response(response(
        200,
        &[("set-cookie", "a=1"), ("set-cookie", "b=2")],
    ));

    let cookies: Vec<&str> = cleaned
        .headers()
        .iter()
        .map(|(_, value)| value.as_str())
        .collect();
    assert_eq!(cookies, ["a=1", "b=2"]);
}

#[test]
fn cleaning_keeps_the_status_and_the_body_it_was_given() {
    let cleaned = clean_response(response(503, &[("retry-after", "1")]));

    assert_eq!(cleaned.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(cleaned.body(), &b"ok"[..]);
}

#[test]
fn a_body_less_status_writes_no_bytes_even_if_one_reached_cleaning() {
    // `ResponseData::new` already refuses this pair, so the invariant is checked where the wire body
    // is decided rather than only where the response is built.
    let cleaned = clean_response(ResponseData {
        status: 204,
        headers: Vec::new(),
        body: b"stale".to_vec(),
    });

    assert_eq!(cleaned.status(), StatusCode::NO_CONTENT);
    assert!(cleaned.body().is_empty(), "a 204 never carries a body");
}
