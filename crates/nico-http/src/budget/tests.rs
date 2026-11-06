use std::time::Duration;

use pingora::http::ResponseHeader;

use super::{
    body_limit, declared_length, declared_response_length, over_limit, request_budget,
    response_limit,
};
use crate::test_support::{document, llm_snapshot, llm_snapshot_with, request, route};

const UNBOUNDED: &str = concat!(
    "  - id: loose\n",
    "    path_prefix: /\n",
    "    upstream: api-backend\n",
);

const TIGHTER_THAN_DOCUMENT: &str = concat!(
    "  - id: tight\n",
    "    path_prefix: /\n",
    "    upstream: api-backend\n",
    "    timeout_ms: 5000\n",
    "    max_body_bytes: 512\n",
);

const LOOSER_THAN_THE_DOCUMENT_TIMEOUT: &str = concat!(
    "  - id: patient\n",
    "    path_prefix: /\n",
    "    upstream: api-backend\n",
    "    timeout_ms: 60000\n",
);

#[test]
fn an_unbounded_route_uses_the_document_body_limit() {
    let document = document(UNBOUNDED);

    let limit = body_limit(route(&document, "loose"), document.request());

    assert_eq!(limit, 1_048_576);
}

#[test]
fn a_route_body_bound_lowers_the_document_limit() {
    let document = document(TIGHTER_THAN_DOCUMENT);

    let limit = body_limit(route(&document, "tight"), document.request());

    assert_eq!(limit, 512);
}

#[test]
fn a_declared_content_length_is_reported() {
    let mut header = request("POST", "/api");
    header
        .insert_header("content-length", "1024")
        .expect("the fixture header is valid");

    assert_eq!(declared_length(&header), Some(1024));
}

#[test]
fn a_request_without_content_length_declares_nothing() {
    assert_eq!(declared_length(&request("POST", "/api")), None);
}

#[test]
fn a_malformed_content_length_reads_as_no_declaration() {
    let mut header = request("POST", "/api");
    header
        .insert_header("content-length", "12kb")
        .expect("the fixture header is valid");

    assert_eq!(declared_length(&header), None);
}

#[test]
fn a_body_exactly_at_the_limit_is_accepted() {
    assert!(!over_limit(512, 512));
}

#[test]
fn one_byte_over_the_limit_is_rejected() {
    assert!(over_limit(512, 513));
}

#[test]
fn a_body_far_above_the_limit_is_rejected() {
    assert!(over_limit(u32::MAX, u64::MAX));
}

#[test]
fn an_unbounded_route_uses_the_document_budget() {
    let document = document(UNBOUNDED);

    let budget = request_budget(route(&document, "loose"), document.request());

    assert_eq!(budget, Duration::from_millis(30_000));
}

#[test]
fn a_route_budget_lowers_the_document_budget() {
    let document = document(TIGHTER_THAN_DOCUMENT);

    let budget = request_budget(route(&document, "tight"), document.request());

    assert_eq!(budget, Duration::from_millis(5_000));
}

#[test]
fn a_route_budget_above_the_document_budget_is_clamped_to_it() {
    let document = document(LOOSER_THAN_THE_DOCUMENT_TIMEOUT);

    let budget = request_budget(route(&document, "patient"), document.request());

    assert_eq!(
        budget,
        Duration::from_millis(30_000),
        "a route may tighten a document bound, never widen it"
    );
}

#[test]
fn an_unbounded_route_uses_the_document_response_limit() {
    let snapshot = llm_snapshot();
    let document = snapshot.document();

    let limit = response_limit(route(document, "ai"), document.request());

    assert_eq!(limit, 4096);
}

#[test]
fn a_route_response_bound_lowers_the_document_limit() {
    let snapshot = llm_snapshot_with("    max_response_bytes: 512\n");
    let document = snapshot.document();

    let limit = response_limit(route(document, "ai"), document.request());

    assert_eq!(
        limit, 512,
        "a route may tighten the buffered-response ceiling, never widen it"
    );
}

#[test]
fn a_declared_response_length_is_reported() {
    let mut header = ResponseHeader::build(200, None).expect("the status is valid");
    header
        .insert_header("content-length", "2048")
        .expect("the fixture header is valid");

    assert_eq!(declared_response_length(&header), Some(2048));
}

#[test]
fn a_chunked_response_declares_no_length() {
    let header = ResponseHeader::build(200, None).expect("the status is valid");

    assert_eq!(declared_response_length(&header), None);
}
