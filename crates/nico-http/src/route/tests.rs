use super::match_route;
use crate::test_support::{document, request, request_to, route};

/// Three routes of increasing prefix length, none of them host-specific.
const NESTED_PREFIXES: &str = concat!(
    "  - id: root\n",
    "    path_prefix: /\n",
    "    upstream: api-backend\n",
    "  - id: api\n",
    "    path_prefix: /api\n",
    "    upstream: api-backend\n",
    "  - id: api-v1\n",
    "    path_prefix: /api/v1/\n",
    "    upstream: api-backend\n",
);

#[test]
fn the_longest_matching_prefix_wins() {
    let document = document(NESTED_PREFIXES);
    let request = request_to("example.test", "/api/v1/items");

    let matched = match_route(&request, document.routes()).expect("a route matches");

    assert_eq!(matched.id(), "api-v1");
}

#[test]
fn a_shorter_prefix_still_matches_a_longer_path() {
    let document = document(NESTED_PREFIXES);
    let request = request_to("example.test", "/api/v2/items");

    let matched = match_route(&request, document.routes()).expect("a route matches");

    assert_eq!(matched.id(), "api");
}

#[test]
fn a_prefix_matches_on_bytes_without_path_segment_normalisation() {
    let document = document(NESTED_PREFIXES);

    let matched = match_route(&request_to("example.test", "/apix"), document.routes());

    assert_eq!(
        matched.map(|route| route.id()),
        Some("api"),
        "`/api` is a byte prefix of `/apix`"
    );
}

#[test]
fn a_prefix_ending_in_a_slash_does_not_match_the_bare_path() {
    let document = document(concat!(
        "  - id: api-slash\n",
        "    path_prefix: /api/\n",
        "    upstream: api-backend\n",
    ));

    let matched = match_route(&request_to("example.test", "/api"), document.routes());

    assert!(matched.is_none(), "`/api/` does not match `/api`");
}

#[test]
fn a_host_specific_route_beats_a_longer_agnostic_prefix() {
    let document = document(concat!(
        "  - id: tenant\n",
        "    host: api.example.com\n",
        "    path_prefix: /\n",
        "    upstream: api-backend\n",
        "  - id: wildcard\n",
        "    path_prefix: /api/v1/\n",
        "    upstream: api-backend\n",
    ));
    let request = request_to("api.example.com", "/api/v1/items");

    let matched = match_route(&request, document.routes()).expect("a route matches");

    assert_eq!(matched.id(), "tenant");
}

#[test]
fn a_host_agnostic_route_serves_every_other_host() {
    let document = document(concat!(
        "  - id: tenant\n",
        "    host: api.example.com\n",
        "    path_prefix: /\n",
        "    upstream: api-backend\n",
        "  - id: wildcard\n",
        "    path_prefix: /api/v1/\n",
        "    upstream: api-backend\n",
    ));
    let request = request_to("other.test", "/api/v1/items");

    let matched = match_route(&request, document.routes()).expect("a route matches");

    assert_eq!(matched.id(), "wildcard");
}

#[test]
fn a_host_specific_route_never_matches_another_host() {
    let document = document(concat!(
        "  - id: tenant\n",
        "    host: api.example.com\n",
        "    path_prefix: /\n",
        "    upstream: api-backend\n",
    ));

    let matched = match_route(&request_to("other.test", "/"), document.routes());

    assert!(matched.is_none());
}

#[test]
fn the_request_host_is_compared_case_insensitively_and_without_its_port() {
    let document = document(concat!(
        "  - id: tenant\n",
        "    host: api.example.com\n",
        "    path_prefix: /\n",
        "    upstream: api-backend\n",
    ));
    let request = request_to("API.Example.COM:8443", "/");

    let matched = match_route(&request, document.routes()).expect("a route matches");

    assert_eq!(matched.id(), "tenant");
}

#[test]
fn an_authority_in_the_request_target_is_used_when_no_host_header_is_sent() {
    let document = document(concat!(
        "  - id: tenant\n",
        "    host: api.example.com\n",
        "    path_prefix: /\n",
        "    upstream: api-backend\n",
    ));
    let request = request("GET", "http://api.example.com/");

    let matched = match_route(&request, document.routes()).expect("a route matches");

    assert_eq!(matched.id(), "tenant");
}

#[test]
fn a_route_host_matches_whatever_its_case() {
    // The validator and the router compare a host with one rule (`nico_core::hosts_match`), so a
    // document cannot carry two entries the router would serve as one. That leaves the earlier
    // document-order tie-break unreachable for a validated document — which is the point: the refusal
    // is what protects it, not the ranking.
    let document = document(concat!(
        "  - id: first\n",
        "    host: api.example.com\n",
        "    path_prefix: /\n",
        "    upstream: api-backend\n",
    ));
    let request = request_to("API.Example.COM", "/items");

    let matched = match_route(&request, document.routes()).expect("a route matches");

    assert_eq!(matched.id(), "first");
}

#[test]
fn a_document_without_routes_matches_nothing() {
    let document = document("  []");
    let request = request_to("example.test", "/");

    assert!(match_route(&request, document.routes()).is_none());
}

#[test]
fn the_returned_route_is_the_one_the_fixture_defined() {
    let document = document(NESTED_PREFIXES);
    let request = request_to("example.test", "/api/v1/");

    let matched = match_route(&request, document.routes()).expect("a route matches");

    assert_eq!(matched.id(), route(&document, "api-v1").id());
    assert_eq!(matched.path_prefix(), "/api/v1/");
}

#[test]
fn an_absolute_form_target_wins_over_the_host_header() {
    // A proxy receives absolute-form targets; Pingora forwards them with the target's authority,
    // so routing has to agree with the authority rather than with a conflicting `Host` header.
    let document = document(concat!(
        "  - id: target-host\n",
        "    host: api.example.com\n",
        "    path_prefix: /\n",
        "    upstream: api-backend\n",
        "  - id: header-host\n",
        "    host: other.test\n",
        "    path_prefix: /\n",
        "    upstream: api-backend\n",
    ));
    let mut request = request("GET", "http://api.example.com/items");
    request
        .insert_header("host", "other.test")
        .expect("host header");

    let matched = match_route(&request, document.routes()).expect("a route matches");

    assert_eq!(matched.id(), "target-host");
}
