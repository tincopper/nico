//! The adapter contracts' own invariants: capabilities, the two facts views, the endpoint checks and
//! the compiled route facts. Nothing here needs a codec or a provider.

#![expect(
    clippy::expect_used,
    reason = "a test that cannot build its fixture has nothing useful to return"
)]

use std::collections::BTreeMap;

use nico_core::Scheme;
use nico_http::Destination;
use nico_llm::{
    Capabilities, Capability, Credential, Endpoint, EndpointError, ExchangeMode, ModelId,
    ProtocolId, ProviderKind, ProviderRoute, RequestFacts, ResponseFacts, RouteError,
};

fn model(name: &str) -> ModelId {
    ModelId::try_new(name).expect("a valid model name")
}

fn destination() -> Destination {
    Destination::new(Scheme::Http, "127.0.0.1", 8080)
}

fn route() -> ProviderRoute {
    ProviderRoute::try_new(
        "chat-route",
        ProtocolId::new("mock.chat"),
        ProviderKind::new("mock"),
        "http://127.0.0.1:8080",
    )
    .expect("a valid route")
}

#[test]
fn capabilities_answer_membership_and_intersection() {
    let codec = Capabilities::none()
        .with(Capability::Streaming)
        .with(Capability::Tools);
    let provider = Capabilities::none()
        .with(Capability::Tools)
        .with(Capability::Images);
    let shared = codec.intersection(provider);

    assert!(shared.supports(Capability::Tools));
    assert!(!shared.supports(Capability::Streaming));
    assert_eq!(shared.iter().collect::<Vec<_>>(), vec![Capability::Tools]);
    assert_eq!(
        codec.without(Capability::Tools),
        Capabilities::none().with(Capability::Streaming)
    );
    assert!(Capabilities::none().is_empty());
    assert!(Capabilities::default().is_empty());
    assert_eq!(Capabilities::all().iter().count(), Capability::ALL.len());
    assert!(Capabilities::all().supports(Capability::Files));
}

#[test]
fn a_capability_names_itself_for_logs_and_diagnostics() {
    assert_eq!(Capability::Streaming.name(), "streaming");
    assert_eq!(Capability::ResponseFormat.name(), "response_format");
    assert_eq!(Capability::Tools.to_string(), "tools");
    assert_eq!(ProtocolId::new("mock.chat").to_string(), "mock.chat");
    assert_eq!(ProviderKind::new("mock").as_str(), "mock");
}

#[test]
fn request_facts_read_headers_case_insensitively() {
    let facts = RequestFacts::new(
        "POST",
        "/v1/mock/chat?stream=1",
        vec![("Content-Type".to_owned(), "application/json".to_owned())],
    );
    assert_eq!(facts.method(), "POST");
    assert_eq!(facts.path(), "/v1/mock/chat?stream=1");
    assert_eq!(facts.header("content-type"), Some("application/json"));
    assert_eq!(facts.header("missing"), None);
    assert_eq!(facts.headers().len(), 1);
}

#[test]
fn response_facts_classify_a_status_and_read_headers() {
    let success = ResponseFacts::new(200, Vec::new());
    assert!(success.is_success());
    assert_eq!(success.status(), 200);

    let failure = ResponseFacts::new(429, vec![("Retry-After".to_owned(), "2".to_owned())]);
    assert!(!failure.is_success());
    assert_eq!(failure.header("retry-after"), Some("2"));
    assert!(!ResponseFacts::new(199, Vec::new()).is_success());
    assert!(!ResponseFacts::new(300, Vec::new()).is_success());
}

#[test]
fn an_endpoint_accepts_an_absolute_path_and_valid_headers() {
    let endpoint = Endpoint::try_new(
        destination(),
        "/v1/chat/completions",
        vec![
            ("authorization".to_owned(), "Bearer secret".to_owned()),
            ("content-type".to_owned(), "application/json".to_owned()),
        ],
    )
    .expect("a valid endpoint");
    assert_eq!(endpoint.destination(), &destination());
    assert_eq!(endpoint.path(), "/v1/chat/completions");
    assert_eq!(endpoint.headers().len(), 2);
}

#[test]
fn an_endpoint_rejects_a_path_that_is_not_absolute() {
    let error = Endpoint::try_new(destination(), "v1/chat", Vec::new())
        .expect_err("a relative path is not a request target");
    assert_eq!(error, EndpointError::Path);
    assert_eq!(
        Endpoint::try_new(destination(), "/v1 chat", Vec::new())
            .expect_err("a space is not a path"),
        EndpointError::Path
    );
}

#[test]
fn an_endpoint_rejects_a_header_that_could_split_the_request() {
    let error = Endpoint::try_new(
        destination(),
        "/v1/chat",
        vec![("authorization".to_owned(), "Bearer a\r\nx: b".to_owned())],
    )
    .expect_err("a line break in a value would inject a header");
    assert_eq!(
        error,
        EndpointError::HeaderValue {
            name: "authorization".to_owned()
        }
    );
    assert_eq!(
        Endpoint::try_new(
            destination(),
            "/v1/chat",
            vec![("bad name".to_owned(), "value".to_owned())]
        )
        .expect_err("a space is not a token"),
        EndpointError::HeaderName {
            name: "bad name".to_owned()
        }
    );
}

#[test]
fn a_route_resolves_a_logical_model_through_its_map() {
    let mut model_map = BTreeMap::new();
    model_map.insert("chat-small".to_owned(), "chat-small-2026-01-01".to_owned());
    let route = route()
        .with_model_map(model_map)
        .expect("the map names valid models");

    let resolved = route.model(&model("chat-small"));
    assert_eq!(resolved.logical().as_str(), "chat-small");
    assert_eq!(resolved.physical().as_str(), "chat-small-2026-01-01");

    let unmapped = route.model(&model("chat-large"));
    assert_eq!(unmapped.logical().as_str(), "chat-large");
    assert_eq!(
        unmapped.physical().as_str(),
        "chat-large",
        "an unmapped model passes through unchanged"
    );
    assert_eq!(route.model_map().len(), 1);
}

#[test]
fn a_route_defaults_to_convert_and_never_prints_its_credential() {
    let route = route()
        .with_credential(Credential::new("sk-mock-secret"))
        .with_mode(ExchangeMode::Passthrough);
    assert_eq!(route.mode(), ExchangeMode::Passthrough);
    assert_eq!(ExchangeMode::default(), ExchangeMode::Convert);
    assert_eq!(
        route.credential().map(Credential::expose),
        Some("sk-mock-secret")
    );
    assert_eq!(
        format!("{:?}", Credential::new("sk-mock-secret")),
        "Credential(<redacted>)"
    );
    let printed = format!("{route:?}");
    assert!(
        !printed.contains("sk-mock-secret"),
        "a logged route must not leak the credential: {printed}"
    );
    assert!(printed.contains("credential: true"));
    assert_eq!(route.protocol(), ProtocolId::new("mock.chat"));
    assert_eq!(route.provider_kind(), ProviderKind::new("mock"));
    assert_eq!(route.base_url(), "http://127.0.0.1:8080");
}

#[test]
fn a_route_rejects_an_empty_identity_field() {
    let empty_id = ProviderRoute::try_new(
        "  ",
        ProtocolId::new("mock.chat"),
        ProviderKind::new("mock"),
        "http://127.0.0.1:8080",
    )
    .expect_err("an empty route id addresses nothing");
    assert_eq!(empty_id, RouteError::Empty { field: "route.id" });

    let empty_url = ProviderRoute::try_new(
        "chat-route",
        ProtocolId::new("mock.chat"),
        ProviderKind::new("mock"),
        "",
    )
    .expect_err("an empty base URL addresses nothing");
    assert_eq!(
        empty_url,
        RouteError::Empty {
            field: "provider.base_url"
        }
    );
}

#[test]
fn a_route_rejects_a_model_map_entry_that_is_not_a_model() {
    let mut model_map = BTreeMap::new();
    model_map.insert("chat-small".to_owned(), "  ".to_owned());
    let error = route()
        .with_model_map(model_map)
        .expect_err("an empty physical name resolves nothing");
    assert_eq!(
        error.to_string(),
        "`model_map` entry `  ` is not a valid model"
    );
    assert!(matches!(error, RouteError::InvalidModel { .. }));
}

#[test]
fn debug_records_name_headers_and_never_print_their_values() {
    let endpoint = Endpoint::try_new(
        destination(),
        "/v1/chat/completions",
        vec![
            (
                "authorization".to_owned(),
                "Bearer sk-live-secret".to_owned(),
            ),
            ("content-type".to_owned(), "application/json".to_owned()),
        ],
    )
    .expect("a valid endpoint");
    let printed = format!("{endpoint:?}");
    assert!(printed.contains("authorization"), "{printed}");
    assert!(!printed.contains("sk-live-secret"), "{printed}");
    assert!(!printed.contains("application/json"), "{printed}");

    let request = RequestFacts::new(
        "POST",
        "/v1/chat/completions",
        vec![(
            "authorization".to_owned(),
            "Bearer sk-live-secret".to_owned(),
        )],
    );
    let printed = format!("{request:?}");
    assert!(printed.contains("authorization"), "{printed}");
    assert!(!printed.contains("sk-live-secret"), "{printed}");

    let response = ResponseFacts::new(
        200,
        vec![("set-cookie".to_owned(), "session=secret".to_owned())],
    );
    let printed = format!("{response:?}");
    assert!(printed.contains("set-cookie"), "{printed}");
    assert!(!printed.contains("session=secret"), "{printed}");
}

#[test]
fn the_contracts_cross_await_and_task_boundaries() {
    fn assert_send_sync<T: Send + Sync + 'static>() {}
    assert_send_sync::<ProviderRoute>();
    assert_send_sync::<Endpoint>();
    assert_send_sync::<Capabilities>();
    assert_send_sync::<RequestFacts>();
    assert_send_sync::<ResponseFacts>();
}
