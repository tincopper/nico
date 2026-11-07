//! The endpoint: the pinned model in the path, the operation, the headers and the route refusals.

use nico_core::Scheme;
use nico_llm::{
    EndpointError, ExchangeMode, Provider, ProviderError, ProviderRoute, provider::gemini::VERSION,
};

use crate::support::{
    BASE_URL, LOGICAL, PHYSICAL, endpoint, endpoint_err, header, provider, route, route_with,
};

/// Returns the route-fact refusal a route must produce, as its field and reason.
fn route_fact(route: &ProviderRoute) -> (String, String) {
    route_fact_or_none(route).expect("the route must be refused by a route fact")
}

/// Returns the route-fact refusal, when the adapter reported one.
fn route_fact_or_none(route: &ProviderRoute) -> Option<(String, String)> {
    match endpoint_err(route) {
        ProviderError::InvalidEndpoint {
            source: EndpointError::RouteFact { field, reason },
            ..
        } => Some((field.to_owned(), reason.to_string())),
        _ => None,
    }
}

/// Returns the endpoint refusal a route must produce, when the adapter reported one.
fn base_url_refusal(route: &ProviderRoute) -> Option<EndpointError> {
    match endpoint_err(route) {
        ProviderError::InvalidEndpoint { source, .. } => Some(source),
        _ => None,
    }
}

#[test]
fn an_origin_base_url_addresses_the_pinned_model_and_the_streaming_operation() {
    let endpoint = endpoint(&route(BASE_URL));
    assert_eq!(endpoint.destination().scheme(), Scheme::Http);
    assert_eq!(endpoint.destination().host(), "127.0.0.1");
    assert_eq!(endpoint.destination().port(), 8080);
    assert_eq!(
        endpoint.path(),
        format!("/{VERSION}/models/{PHYSICAL}:streamGenerateContent?alt=sse"),
        "the path names both body-derived facts, so both are publication-time values"
    );
    assert_eq!(
        endpoint.model().map(nico_llm::ModelId::as_str),
        Some(PHYSICAL),
        "the endpoint records the model it already wrote into the path"
    );
}

#[test]
fn a_secure_origin_uses_the_default_port_and_its_own_one_when_named() {
    for (base_url, port) in [
        ("https://api.example.test", 443),
        ("https://api.example.test:8443/", 8443),
    ] {
        let endpoint = endpoint(&route(base_url));
        assert_eq!(endpoint.destination().scheme(), Scheme::Https);
        assert_eq!(endpoint.destination().host(), "api.example.test");
        assert_eq!(endpoint.destination().port(), port, "{base_url}");
    }
}

#[test]
fn a_prefix_precedes_the_whole_operation_path() {
    // The base URL is an API prefix, and this dialect's operation path carries its own revision
    // segment: the prefix is written in front of it, not merged with it.
    for (base_url, expected) in [
        (
            "http://gateway.internal:8080/proxy",
            "/proxy/v1beta/models/gemini-2.0-flash:streamGenerateContent?alt=sse",
        ),
        (
            "http://gateway.internal:8080/gemini/",
            "/gemini/v1beta/models/gemini-2.0-flash:streamGenerateContent?alt=sse",
        ),
    ] {
        assert_eq!(endpoint(&route(base_url)).path(), expected, "{base_url}");
    }
}

#[test]
fn a_route_with_no_model_map_is_refused() {
    // The path names the model, so a route that maps nothing would address a request with no model.
    let (field, reason) = route_fact(&route_with(BASE_URL, None, &[]));
    assert_eq!(field, "providers[].model_map");
    assert!(
        reason.contains("path"),
        "the reason names the fact that made the map necessary: {reason}"
    );
}

#[test]
fn a_map_naming_two_models_is_refused() {
    let (field, _) = route_fact(&route_with(
        BASE_URL,
        None,
        &[(LOGICAL, PHYSICAL), ("other", "gemini-2.0-pro")],
    ));
    assert_eq!(field, "providers[].model_map");
}

#[test]
fn two_aliases_for_one_model_are_constant_and_accepted() {
    // "Constant" is the rule, not "one entry": both aliases describe one endpoint, so the path names
    // that one model and nothing is ambiguous.
    let endpoint = endpoint(&route_with(
        BASE_URL,
        None,
        &[("chat", PHYSICAL), ("chat-latest", PHYSICAL)],
    ));
    assert_eq!(
        endpoint.model().map(nico_llm::ModelId::as_str),
        Some(PHYSICAL)
    );
    assert!(
        endpoint.path().contains(&format!("/models/{PHYSICAL}:")),
        "{}",
        endpoint.path()
    );
}

#[test]
fn a_model_that_is_not_a_literal_path_segment_is_refused() {
    // The name is written into the request line, so a value needing an escape — or one carrying a
    // query, a fragment or a path separator — would address a different resource than the operator
    // wrote. It is refused at publication instead of escaped into something else.
    for physical in [
        "gemini/../../admin",
        "gemini?alt=json",
        "gemini#frag",
        "gemini 2",
        "gemini:2",
        "models/gemini-2.0-flash",
    ] {
        let (field, reason) = route_fact(&route_with(BASE_URL, None, &[(LOGICAL, physical)]));
        assert_eq!(field, "providers[].model_map", "{physical}");
        assert!(
            !reason.contains(physical),
            "a diagnostic never quotes the value: {reason}"
        );
    }
}

#[test]
fn a_passthrough_route_is_refused_by_its_mode() {
    // The target carries the model and the operation, so forwarding the caller's bytes to a target
    // fixed at publication would send them to whichever model this route pinned.
    let route = route(BASE_URL).with_mode(ExchangeMode::Passthrough);
    let (field, reason) = route_fact(&route);
    assert_eq!(field, "routes[].mode");
    assert!(
        reason.contains("model"),
        "the reason says what would be sent to another model: {reason}"
    );
}

#[test]
fn the_route_facts_are_checked_before_the_address() {
    // A route with two problems reports the one specific to this adapter, because that is the answer
    // the operator has to act on: the address is not even consulted.
    let route = route_with("not-a-url", None, &[]).with_mode(ExchangeMode::Passthrough);
    assert_eq!(route_fact(&route).0, "routes[].mode");
}

#[test]
fn an_addressable_url_that_is_not_a_base_url_is_refused_by_the_shared_rules() {
    for base_url in [
        "h.test",
        "ftp://h.test",
        "http://user:secret@h.test",
        "http://h.test:0",
    ] {
        let error = base_url_refusal(&route(base_url)).expect("a base-URL refusal");
        assert!(
            matches!(
                error,
                EndpointError::BaseUrl {
                    field: "provider.base_url",
                    ..
                }
            ),
            "`{base_url}` must be a base-URL refusal: {error:?}"
        );
        assert!(
            !format!("{error:?} {error}").contains(base_url),
            "a diagnostic must not quote the URL: {error}"
        );
    }
}

#[test]
fn the_headers_name_the_event_stream_and_never_a_second_credential_header() {
    let endpoint = endpoint(&route_with(
        BASE_URL,
        Some("goog-secret"),
        &[(LOGICAL, PHYSICAL)],
    ));
    assert_eq!(header(&endpoint, "content-type"), Some("application/json"));
    assert_eq!(
        header(&endpoint, "accept"),
        Some("text/event-stream"),
        "the endpoint only ever asks for the streaming shape"
    );
    assert_eq!(
        header(&endpoint, "x-goog-api-key"),
        Some("goog-secret"),
        "this dialect carries the key in its own header"
    );
    assert_eq!(
        header(&endpoint, "authorization"),
        None,
        "a second credential header would be a second, unlogged secret"
    );
}

#[test]
fn a_route_without_a_credential_sends_no_api_key() {
    assert_eq!(header(&endpoint(&route(BASE_URL)), "x-goog-api-key"), None);
}

#[test]
fn the_adapter_never_sets_a_header_the_data_plane_owns() {
    let endpoint = endpoint(&route_with(
        BASE_URL,
        Some("goog-secret"),
        &[(LOGICAL, PHYSICAL)],
    ));
    for owned in [
        "host",
        "content-length",
        "transfer-encoding",
        "connection",
        "accept-encoding",
    ] {
        assert_eq!(
            header(&endpoint, owned),
            None,
            "the data plane owns `{owned}`"
        );
    }
}

#[test]
fn neither_the_endpoint_nor_its_refusals_print_the_credential_or_the_model_value() {
    let endpoint = endpoint(&route_with(
        BASE_URL,
        Some("goog-secret"),
        &[(LOGICAL, PHYSICAL)],
    ));
    let record = format!("{endpoint:?}");
    assert!(!record.contains("goog-secret"), "{record}");
    assert!(
        record.contains("x-goog-api-key"),
        "the header's name is printed, its value is not: {record}"
    );

    let error = endpoint_err(&route_with(BASE_URL, None, &[(LOGICAL, "bad name")]));
    assert!(!format!("{error:?} {error}").contains("bad name"));
}

#[test]
fn the_adapter_has_one_endpoint_per_route_whatever_the_request_will_say() {
    // The endpoint is a function of the route only: nothing here reads a request, which is what lets
    // the data plane write the upstream head before the body is decoded.
    let route = route(BASE_URL);
    assert_eq!(endpoint(&route), endpoint(&route));
    assert_eq!(provider().kind().as_str(), "gemini");
}

#[test]
fn the_pinned_model_is_the_one_the_route_resolves_for_every_alias() {
    let route = route_with(
        BASE_URL,
        None,
        &[(LOGICAL, PHYSICAL), ("alias-2", PHYSICAL)],
    );
    assert_eq!(
        route.model(&nico_llm::ModelId::try_new("alias-2").expect("a name")),
        nico_llm::ModelRef::new(
            nico_llm::ModelId::try_new("alias-2").expect("a name"),
            nico_llm::ModelId::try_new(PHYSICAL).expect("a name"),
        )
    );
    assert_eq!(
        endpoint(&route).model().map(nico_llm::ModelId::as_str),
        Some(PHYSICAL)
    );
}
