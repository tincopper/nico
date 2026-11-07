//! The endpoint: the route's base URL, the path, the headers and the credential.

use nico_core::Scheme;
use nico_llm::Provider;

use crate::support::{BASE_URL, endpoint, header, provider, route, route_with};

/// Asserts the endpoint's destination and path.
fn assert_endpoint(base_url: &str, scheme: Scheme, host: &str, port: u16, path: &str) {
    let endpoint = endpoint(&route(base_url));
    assert_eq!(
        endpoint.destination().scheme(),
        scheme,
        "scheme of {base_url}"
    );
    assert_eq!(endpoint.destination().host(), host, "host of {base_url}");
    assert_eq!(endpoint.destination().port(), port, "port of {base_url}");
    assert_eq!(endpoint.path(), path, "path of {base_url}");
    assert!(
        endpoint.model().is_none(),
        "the model travels in the body of this dialect, so nothing is pinned to the path"
    );
}

#[test]
fn an_origin_base_url_addresses_the_responses_endpoint() {
    assert_endpoint(
        "http://127.0.0.1:8080",
        Scheme::Http,
        "127.0.0.1",
        8080,
        "/v1/responses",
    );
    assert_endpoint(
        "https://api.example.test",
        Scheme::Https,
        "api.example.test",
        443,
        "/v1/responses",
    );
    assert_endpoint(
        "https://api.example.test:8443/",
        Scheme::Https,
        "api.example.test",
        8443,
        "/v1/responses",
    );
}

#[test]
fn a_path_prefix_is_kept_and_the_responses_path_appended() {
    // The base URL is an API prefix: a route that already ends in `/v1` is not given a second one.
    for (base_url, expected) in [
        ("http://gateway.internal:8080/v1", "/v1/responses"),
        ("http://gateway.internal:8080/openai", "/openai/responses"),
        (
            "http://gateway.internal:8080/openai/v1/",
            "/openai/v1/responses",
        ),
        ("http://gateway.internal:8080//", "/v1/responses"),
    ] {
        assert_eq!(endpoint(&route(base_url)).path(), expected, "{base_url}");
    }
}

#[test]
fn the_headers_name_the_wire_shape() {
    let endpoint = endpoint(&route(BASE_URL));
    assert_eq!(header(&endpoint, "content-type"), Some("application/json"));
    let accept = header(&endpoint, "accept").expect("the endpoint advertises its media types");
    assert!(accept.contains("application/json"));
    assert!(
        accept.contains("text/event-stream"),
        "the same path answers a stream depending on a body field the endpoint may not read"
    );
    assert_eq!(
        header(&endpoint, "anthropic-version"),
        None,
        "this dialect has no revision header"
    );
    assert_eq!(
        header(&endpoint, "x-api-key"),
        None,
        "the credential belongs in `authorization` here"
    );
}

#[test]
fn a_credential_is_presented_as_a_bearer_token() {
    let endpoint = endpoint(&route_with(BASE_URL, Some("sk-responses-secret")));
    assert_eq!(
        header(&endpoint, "authorization"),
        Some("Bearer sk-responses-secret")
    );
}

#[test]
fn a_route_without_a_credential_sends_no_authorization_header() {
    // A local compatible endpoint needs none, and an empty credential is a publication concern.
    assert_eq!(
        header(&endpoint(&route(BASE_URL)), "authorization"),
        None,
        "an empty authorization header would be a value the provider has to interpret"
    );
}

#[test]
fn the_adapter_never_sets_a_header_the_data_plane_owns() {
    let endpoint = endpoint(&route_with(BASE_URL, Some("sk-responses-secret")));
    for owned in [
        "host",
        "content-length",
        "transfer-encoding",
        "connection",
        "keep-alive",
    ] {
        assert_eq!(
            header(&endpoint, owned),
            None,
            "`{owned}` is derived from the destination and the body the data plane writes"
        );
    }
}

#[test]
fn an_unaddressable_base_url_is_refused_without_quoting_it() {
    for base_url in [
        "api.example.test",
        "ftp://api.example.test",
        "http://user:sk-secret@api.example.test",
        "http://api.example.test/v1?key=1",
        "http://api.example.test:0",
    ] {
        let error = provider()
            .endpoint(&route(base_url))
            .expect_err("the route must not form an endpoint");
        assert!(
            matches!(
                error,
                nico_llm::ProviderError::InvalidEndpoint {
                    source: nico_llm::EndpointError::BaseUrl { .. },
                    ..
                }
            ),
            "`{base_url}` must be a base-URL refusal: {error:?}"
        );
        let record = format!("{error:?} {error}");
        assert!(
            !record.contains("sk-secret"),
            "a diagnostic must not quote user information: {record}"
        );
    }
}

#[test]
fn the_endpoint_record_names_the_headers_and_not_their_values() {
    // The credential travels in a header value, so a record that printed values would print the key.
    let endpoint = endpoint(&route_with(BASE_URL, Some("sk-responses-secret")));
    let record = format!("{endpoint:?}");
    assert!(
        record.contains("authorization"),
        "the header names are what an operator needs: {record}"
    );
    assert!(
        !record.contains("sk-responses-secret"),
        "the credential must not be printed: {record}"
    );
}
