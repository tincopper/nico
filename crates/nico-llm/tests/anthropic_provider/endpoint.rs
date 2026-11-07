//! The endpoint: the route's base URL, the required output ceiling, the path and the headers.

use std::num::NonZeroU32;

use nico_core::Scheme;
use nico_llm::{EndpointError, Provider, ProviderError, provider::anthropic::VERSION};

use crate::support::{BASE_URL, CEILING, endpoint, header, provider, route, route_with};

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

/// Returns the refusal of a route the adapter must not address.
fn invalid(route: &nico_llm::ProviderRoute) -> ProviderError {
    provider()
        .endpoint(route)
        .expect_err("the route must not form an endpoint")
}

#[test]
fn an_origin_base_url_addresses_the_messages_endpoint() {
    assert_endpoint(
        "http://127.0.0.1:8080",
        Scheme::Http,
        "127.0.0.1",
        8080,
        "/v1/messages",
    );
    assert_endpoint(
        "https://api.example.test",
        Scheme::Https,
        "api.example.test",
        443,
        "/v1/messages",
    );
    assert_endpoint(
        "https://api.example.test:8443/",
        Scheme::Https,
        "api.example.test",
        8443,
        "/v1/messages",
    );
}

#[test]
fn a_path_prefix_is_kept_and_the_messages_path_appended() {
    // The base URL is an API prefix: a route that already ends in `/v1` is not given a second one.
    for (base_url, expected) in [
        ("http://gateway.internal:8080/v1", "/v1/messages"),
        (
            "http://gateway.internal:8080/anthropic",
            "/anthropic/messages",
        ),
        (
            "http://gateway.internal:8080/anthropic/v1/",
            "/anthropic/v1/messages",
        ),
        ("http://gateway.internal:8080//", "/v1/messages"),
    ] {
        assert_eq!(endpoint(&route(base_url)).path(), expected, "{base_url}");
    }
}

#[test]
fn the_headers_name_the_wire_shape_and_the_revision() {
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
        Some(VERSION),
        "the dialect requires its revision on every request"
    );
}

#[test]
fn a_credential_is_presented_as_an_api_key() {
    let endpoint = endpoint(&route_with(BASE_URL, Some("sk-ant-secret"), Some(CEILING)));
    assert_eq!(
        header(&endpoint, "x-api-key"),
        Some("sk-ant-secret"),
        "this dialect carries the key in its own header, not in `authorization`"
    );
    assert_eq!(
        header(&endpoint, "authorization"),
        None,
        "a second credential header would be a second, unlogged secret"
    );
}

#[test]
fn a_route_without_a_credential_sends_no_api_key() {
    assert_eq!(header(&endpoint(&route(BASE_URL)), "x-api-key"), None);
}

#[test]
fn the_adapter_never_sets_a_header_the_data_plane_owns() {
    let endpoint = endpoint(&route_with(BASE_URL, Some("sk-ant-secret"), Some(CEILING)));
    for owned in [
        "host",
        "content-length",
        "transfer-encoding",
        "connection",
        "keep-alive",
        "te",
        "trailer",
        "upgrade",
        "proxy-authorization",
    ] {
        assert_eq!(
            header(&endpoint, owned),
            None,
            "`{owned}` is derived from the destination and the body by the data plane"
        );
    }
}

#[test]
fn a_debug_record_names_the_headers_and_never_the_credential() {
    let endpoint = endpoint(&route_with(BASE_URL, Some("sk-ant-secret"), Some(CEILING)));
    let record = format!("{endpoint:?}");
    assert!(
        !record.contains("sk-ant-secret"),
        "the credential must not be printed: {record}"
    );
    assert!(
        record.contains("x-api-key"),
        "the header name is what makes the record useful: {record}"
    );
}

#[test]
fn a_route_without_an_output_ceiling_is_refused_at_publication() {
    // `/v1/messages` requires `max_tokens`; a route that answers nothing for a request that omits one
    // cannot be served, and inventing a constant would silently cap the caller's answer.
    let error = invalid(&route_with(BASE_URL, None, None));
    let ProviderError::InvalidEndpoint { source, .. } = error else {
        panic!("a missing ceiling must be an endpoint failure, got {error:?}");
    };
    assert_eq!(
        source,
        EndpointError::RouteFact {
            field: "providers[].max_output_tokens",
            reason: "`/v1/messages` requires `max_tokens` and the route names no default for a \
                     request that omits it"
                .into(),
        },
        "the operator reads which route fact is missing"
    );
    assert!(
        !source.to_string().contains(BASE_URL),
        "a refusal quotes no value: {source}"
    );
}

#[test]
fn the_missing_ceiling_is_reported_before_the_base_url_is_parsed() {
    // A route can be broken in two ways at once; reporting the fact specific to this adapter makes
    // the operator's next edit the one that matters.
    let error = invalid(&route_with("ftp://api.example.test", None, None));
    assert!(
        matches!(
            error,
            ProviderError::InvalidEndpoint {
                source: EndpointError::RouteFact { .. },
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn a_base_url_the_adapter_does_not_interpret_is_refused() {
    // The parsing rules are shared with the OpenAI adapter; a couple of representative refusals are
    // asserted here so this adapter cannot silently lose the rule by not routing through it.
    for base_url in [
        "ftp://api.example.test",
        "http://user:secret@api.example.test",
        "http://api.example.test/v1?key=1",
        "http://api.example.test/v1#fragment",
        "http://",
        "http://api.example.test:0",
        "http://api.example.test :8080",
        "api.example.test",
        "http://api.example.test//v1",
    ] {
        let error = invalid(&route(base_url));
        assert!(
            matches!(
                error,
                ProviderError::InvalidEndpoint {
                    source: EndpointError::BaseUrl {
                        field: "provider.base_url",
                        ..
                    },
                    ..
                }
            ),
            "`{base_url}` must be refused as a base URL, got {error:?}"
        );
    }
}

#[test]
fn the_ceiling_is_a_route_fact_and_does_not_change_the_path() {
    for ceiling in 1..=16 {
        let route = route_with(BASE_URL, None, Some(ceiling));
        assert_eq!(
            route.max_output_tokens(),
            NonZeroU32::new(ceiling),
            "the route carries the configured ceiling"
        );
        assert_eq!(
            endpoint(&route).path(),
            "/v1/messages",
            "the ceiling is body policy, not part of the request target"
        );
    }
}

#[test]
fn an_https_base_url_is_a_valid_endpoint() {
    // TLS policy belongs to the data plane; refusing `https` here would make an adapter the place
    // that decides what the gateway can dial.
    assert_eq!(
        endpoint(&route("https://api.example.test"))
            .destination()
            .scheme(),
        Scheme::Https
    );
}
