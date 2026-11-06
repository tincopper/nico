//! The endpoint: how a route's base URL becomes a destination, a path and headers.

use nico_core::Scheme;
use nico_llm::{EndpointError, Provider, ProviderError};

use crate::support::{endpoint, header, provider, route, route_with};

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
}

/// Returns the refusal of a route the adapter must not address.
fn invalid(base_url: &str) -> ProviderError {
    provider()
        .endpoint(&route(base_url))
        .expect_err("the base URL must not form an endpoint")
}

#[test]
fn an_origin_base_url_addresses_the_chat_endpoint() {
    assert_endpoint(
        "http://127.0.0.1:8080",
        Scheme::Http,
        "127.0.0.1",
        8080,
        "/v1/chat/completions",
    );
}

#[test]
fn a_scheme_without_a_port_uses_its_default() {
    assert_endpoint(
        "http://api.example.test",
        Scheme::Http,
        "api.example.test",
        80,
        "/v1/chat/completions",
    );
    assert_endpoint(
        "https://api.example.test",
        Scheme::Https,
        "api.example.test",
        443,
        "/v1/chat/completions",
    );
    assert_endpoint(
        "https://api.example.test:8443/",
        Scheme::Https,
        "api.example.test",
        8443,
        "/v1/chat/completions",
    );
}

#[test]
fn the_scheme_is_case_insensitive() {
    assert_endpoint(
        "HTTP://api.example.test:8080/v1",
        Scheme::Http,
        "api.example.test",
        8080,
        "/v1/chat/completions",
    );
    assert_endpoint(
        "HtTpS://api.example.test",
        Scheme::Https,
        "api.example.test",
        443,
        "/v1/chat/completions",
    );
}

#[test]
fn a_path_prefix_is_kept_and_the_chat_path_appended() {
    assert_endpoint(
        "http://gateway.internal:8080/v1",
        Scheme::Http,
        "gateway.internal",
        8080,
        "/v1/chat/completions",
    );
    assert_endpoint(
        "http://gateway.internal:8080/openai/v1",
        Scheme::Http,
        "gateway.internal",
        8080,
        "/openai/v1/chat/completions",
    );
}

#[test]
fn trailing_slashes_never_produce_a_double_slash() {
    for base_url in [
        "http://gateway.internal:8080/",
        "http://gateway.internal:8080//",
        "http://gateway.internal:8080/v1/",
        "http://gateway.internal:8080/v1//",
    ] {
        let endpoint = endpoint(&route(base_url));
        assert!(
            !endpoint.path().contains("//"),
            "the joined path of {base_url} is {}",
            endpoint.path()
        );
    }
    assert_eq!(
        endpoint(&route("http://gateway.internal:8080//")).path(),
        "/v1/chat/completions",
        "a base URL whose whole path is slashes is an API root"
    );
    assert_eq!(
        endpoint(&route("http://gateway.internal:8080/openai/")).path(),
        "/openai/chat/completions"
    );
}

#[test]
fn an_ipv6_literal_is_refused_because_a_destination_carries_a_bare_name() {
    // The host rule (nico_core::validate_host) has no place for a colon-bearing host, and the data
    // plane writes `host:port` without re-bracketing: accepting `[::1]` would put an unreadable
    // `Host` on the wire. The refusal names the rule, not the value.
    for base_url in ["http://[::1]:9000/v1", "https://[2001:db8::1]"] {
        let ProviderError::InvalidEndpoint { source, .. } = invalid(base_url) else {
            panic!("`{base_url}` must be refused as a base URL");
        };
        let EndpointError::BaseUrl { reason, .. } = &source else {
            panic!("`{base_url}` must be a base-URL failure: {source:?}");
        };
        assert!(
            reason.contains("IPv6"),
            "the reason names the rule: {reason}"
        );
        assert!(!reason.contains(base_url), "the reason quotes no value");
    }
}

#[test]
fn a_host_longer_than_the_dns_name_limit_is_refused() {
    let long_host = "a".repeat(254);
    let ProviderError::InvalidEndpoint { source, .. } = invalid(&format!("http://{long_host}/v1"))
    else {
        panic!("a 254-byte host must be refused");
    };
    let EndpointError::BaseUrl { reason, .. } = &source else {
        panic!("the refusal is a base-URL failure: {source:?}");
    };
    assert!(
        reason.contains("253"),
        "the reason quotes the bound the check applied: {reason}"
    );
}

#[test]
fn the_headers_name_the_wire_shape_and_both_answer_kinds() {
    let endpoint = endpoint(&route(BASE_URL));
    assert_eq!(header(&endpoint, "content-type"), Some("application/json"));
    let accept = header(&endpoint, "accept").expect("the endpoint advertises its media types");
    assert!(accept.contains("application/json"));
    assert!(
        accept.contains("text/event-stream"),
        "the same path answers a stream depending on a body field the endpoint may not read"
    );
}

/// The base URL most tests use.
const BASE_URL: &str = "http://127.0.0.1:8080";

#[test]
fn a_credential_is_presented_as_a_bearer() {
    let endpoint = endpoint(&route_with(Some("sk-test-secret"), &[]));
    assert_eq!(
        header(&endpoint, "authorization"),
        Some("Bearer sk-test-secret")
    );
}

#[test]
fn a_route_without_a_credential_sends_no_authorization() {
    let endpoint = endpoint(&route(BASE_URL));
    assert_eq!(header(&endpoint, "authorization"), None);
}

#[test]
fn the_adapter_never_sets_a_header_the_data_plane_owns() {
    let endpoint = endpoint(&route_with(Some("sk-test-secret"), &[]));
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
    let endpoint = endpoint(&route_with(Some("sk-test-secret"), &[]));
    let record = format!("{endpoint:?}");
    assert!(
        !record.contains("sk-test-secret"),
        "the credential must not be printed: {record}"
    );
    assert!(
        record.contains("authorization"),
        "the header name is what makes the record useful: {record}"
    );
}

#[test]
fn a_url_the_adapter_does_not_interpret_is_refused() {
    for base_url in [
        "http://user:secret@api.example.test",
        "http://api.example.test/v1?key=1",
        "http://api.example.test/v1#fragment",
        "http://",
        "http:///v1",
        "http://:8080",
        "http://api.example.test:0",
        "http://api.example.test:not-a-port",
        "http://api.example.test:",
        "http://api.example.test:70000",
        "http://api.example.test :8080",
        " http://api.example.test",
        "api.example.test",
        "ftp://api.example.test",
        "http://[::1",
        "http://[::1]suffix",
        "http://::1",
        "http://api.example.test/v1 with space",
        "http://api.example.test\u{1}",
    ] {
        let error = invalid(base_url);
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
fn a_refused_url_does_not_quote_its_user_information() {
    let ProviderError::InvalidEndpoint { source, .. } =
        invalid("http://user:sk-test-secret@api.example.test")
    else {
        panic!("the user information must be refused as a base URL");
    };
    let record = format!("{source:?} {source}");
    assert!(
        !record.contains("sk-test-secret"),
        "a diagnostic must not repeat the credential: {record}"
    );
}

#[test]
fn a_prefix_with_an_empty_segment_is_refused() {
    // The join never produces `//`, and a prefix that already carries one is not passed to the
    // origin to interpret: a leading `//` reads as a network-path reference to some peers.
    for base_url in [
        "http://gateway.internal:8080//v1",
        "http://gateway.internal:8080/openai//v1",
        "http://gateway.internal:8080/v1//chat",
    ] {
        let ProviderError::InvalidEndpoint { source, .. } = invalid(base_url) else {
            panic!("`{base_url}` must be refused as a base URL");
        };
        assert!(
            matches!(
                source,
                EndpointError::BaseUrl {
                    field: "provider.base_url",
                    ..
                }
            ),
            "`{base_url}` must name the field it broke: {source:?}"
        );
        assert!(
            !format!("{source:?} {source}").contains(base_url),
            "a diagnostic must not quote the URL: {source}"
        );
    }
}

#[test]
fn a_relative_path_is_not_a_base_url() {
    // The adapter's only path check is `Endpoint`'s absolute-path rule; a relative base URL never
    // reaches it because it fails the scheme check first.
    assert!(matches!(
        invalid("/v1/chat/completions"),
        ProviderError::InvalidEndpoint {
            source: EndpointError::BaseUrl { .. },
            ..
        }
    ));
}
