//! The exchange contract: mocks prove the orchestration, the capability gate and the failure mapping
//! without either vendor's wire format.
//!
//! The mocks live in `exchange/mocks.rs`; the assertions here are about ordering (decode once, then
//! encode), about the one place a capability is enforced, and about the fact that every failure
//! leaves as a value a client codec can render.

#![expect(
    clippy::expect_used,
    reason = "a test that cannot build its fixture has nothing useful to return"
)]

mod mocks;
mod response;
mod stream;

use std::sync::Arc;

use mocks::{
    KIND, MockCodec, MockProvider, PROTOCOL, StreamBehaviour, Trace, body, exchange, request_facts,
    route, streaming_request_facts,
};
use nico_core::ErrorCategory;
use nico_llm::{
    CanonicalError, Capabilities, Capability, ClientReply, CodecError, EndpointError, Exchange,
    ExchangeBuildError, ExchangeError, ExchangeMode, PreparedExchange, ProtocolId,
    ProtocolRegistry, ProviderError, ProviderKind, ProviderRegistry, ProviderRoute, RegistryError,
    ResponseFacts, SchemaError, StreamEncoder,
};

#[test]
fn a_codec_registry_refuses_a_duplicate_protocol() {
    let mut registry = ProtocolRegistry::new();
    assert!(registry.is_empty());
    registry
        .register(MockCodec::new(Capabilities::all()))
        .expect("the first registration of a protocol succeeds");
    assert_eq!(registry.len(), 1);
    assert!(registry.codec("mock.chat").is_some());
    assert!(registry.codec("other.chat").is_none());

    let duplicate = registry
        .register(MockCodec::new(Capabilities::all()))
        .expect_err("a second codec for the same protocol is refused");
    assert_eq!(
        duplicate,
        RegistryError::DuplicateKind {
            kind: "mock.chat".to_owned()
        }
    );
    assert!(
        format!("{registry:?}").contains("mock.chat"),
        "the registry must name its protocols when logged"
    );
}

#[test]
fn a_provider_registry_refuses_a_duplicate_kind() {
    let mut registry = ProviderRegistry::new();
    assert!(registry.is_empty());
    registry
        .register(MockProvider::new(
            Capabilities::all(),
            StreamBehaviour::Lines,
        ))
        .expect("the first registration of a kind succeeds");
    assert_eq!(registry.len(), 1);
    assert!(registry.provider("mock").is_some());
    assert!(registry.provider("other").is_none());

    let duplicate = registry
        .register(MockProvider::new(
            Capabilities::all(),
            StreamBehaviour::Lines,
        ))
        .expect_err("a second adapter for the same kind is refused");
    assert_eq!(
        duplicate,
        RegistryError::DuplicateKind {
            kind: "mock".to_owned()
        }
    );
    assert!(format!("{registry:?}").contains("mock"));
}

#[test]
fn an_exchange_refuses_a_codec_for_another_protocol() {
    let other = Arc::new(
        ProviderRoute::try_new(
            "chat-route",
            ProtocolId::new("other.chat"),
            KIND,
            "http://127.0.0.1:8080",
        )
        .expect("a valid route"),
    );
    let error = Exchange::try_new(
        MockCodec::new(Capabilities::all()),
        MockProvider::new(Capabilities::all(), StreamBehaviour::Lines),
        other,
    )
    .expect_err("the codec speaks a protocol the route does not name");
    assert_eq!(
        error,
        ExchangeBuildError::ProtocolMismatch {
            codec: PROTOCOL,
            route: ProtocolId::new("other.chat")
        }
    );
}

#[test]
fn an_exchange_refuses_a_provider_for_another_kind() {
    let other = Arc::new(
        ProviderRoute::try_new(
            "chat-route",
            PROTOCOL,
            ProviderKind::new("other"),
            "http://127.0.0.1:8080",
        )
        .expect("a valid route"),
    );
    let error = Exchange::try_new(
        MockCodec::new(Capabilities::all()),
        MockProvider::new(Capabilities::all(), StreamBehaviour::Lines),
        other,
    )
    .expect_err("the adapter serves a kind the route does not name");
    assert_eq!(
        error,
        ExchangeBuildError::ProviderMismatch {
            provider: KIND,
            route: ProviderKind::new("other")
        }
    );
}

#[test]
fn an_exchange_refuses_a_passthrough_route() {
    let error = Exchange::try_new(
        MockCodec::new(Capabilities::all()),
        MockProvider::new(Capabilities::all(), StreamBehaviour::Lines),
        route(ExchangeMode::Passthrough),
    )
    .expect_err("a passthrough route has no canonical exchange");
    assert_eq!(
        error,
        ExchangeBuildError::NotConverted {
            route_id: "chat-route".to_owned()
        }
    );
}

#[test]
fn an_exchange_keeps_only_the_shared_capabilities() {
    let exchange = exchange(
        Capabilities::none()
            .with(Capability::Streaming)
            .with(Capability::Tools),
        Capabilities::none()
            .with(Capability::Tools)
            .with(Capability::Images),
    );
    let shared = exchange.capabilities();
    assert!(shared.supports(Capability::Tools));
    assert!(!shared.supports(Capability::Streaming));
    assert_eq!(shared.iter().collect::<Vec<_>>(), vec![Capability::Tools]);
    assert_eq!(exchange.route().route_id(), "chat-route");
    assert_eq!(exchange.route().provider_kind(), KIND);
}

#[test]
fn debug_records_never_print_a_prompt_a_credential_or_an_answer() {
    let exchange = exchange(Capabilities::all(), Capabilities::all());
    let prepared = exchange
        .prepare(&request_facts(), &body("chat-small", "top-secret-prompt"))
        .expect("a well-formed request prepares");

    let printed = format!("{prepared:?}");
    assert!(printed.contains("chat-small-2026-01-01"), "{printed}");
    assert!(!printed.contains("top-secret-prompt"), "{printed}");
    assert!(!printed.contains("sk-mock-secret"), "{printed}");

    let printed = format!("{:?}", prepared.wire());
    assert!(printed.contains("body_len"), "{printed}");
    assert!(!printed.contains("top-secret-prompt"), "{printed}");
    assert!(!printed.contains("sk-mock-secret"), "{printed}");

    let reply = exchange
        .respond(
            &ResponseFacts::new(200, Vec::new()),
            b"resp-1|top-secret-answer",
        )
        .expect("a well-formed response decodes");
    let printed = format!("{reply:?}");
    assert!(printed.contains("body_len"), "{printed}");
    assert!(!printed.contains("top-secret-answer"), "{printed}");
}

#[test]
fn the_endpoint_exists_before_the_body_does() {
    let exchange = exchange(Capabilities::all(), Capabilities::all());
    let endpoint = exchange.endpoint();
    assert_eq!(endpoint.path(), "/v1/mock");
    assert_eq!(endpoint.destination().host(), "127.0.0.1");
    assert_eq!(
        endpoint.headers(),
        &[(
            "authorization".to_owned(),
            "Bearer sk-mock-secret".to_owned()
        )]
    );
}

#[test]
fn the_endpoint_is_compiled_once_at_publication() {
    let trace = Trace::default();
    let exchange = Exchange::try_new(
        MockCodec::traced(Capabilities::all(), trace.clone()),
        MockProvider::traced(Capabilities::all(), StreamBehaviour::Lines, trace.clone()),
        route(ExchangeMode::Convert),
    )
    .expect("the mocks pair on the route");
    assert_eq!(
        trace.steps(),
        ["endpoint"],
        "pairing the route resolves the endpoint it will send to"
    );

    for _ in 0..2 {
        exchange
            .prepare(&request_facts(), &body("chat-small", "hello"))
            .expect("a well-formed request prepares");
    }
    assert_eq!(
        trace.steps(),
        ["endpoint", "decode", "encode", "decode", "encode"],
        "preparing a request never re-reads the route's base URL"
    );
}

#[test]
fn prepare_decodes_once_and_encodes_for_the_provider() {
    let exchange = exchange(Capabilities::all(), Capabilities::all());
    let prepared = exchange
        .prepare(&request_facts(), &body("chat-small", "hello"))
        .expect("a well-formed request prepares");

    assert_eq!(prepared.request().model().as_str(), "chat-small");
    assert_eq!(prepared.model().logical().as_str(), "chat-small");
    assert_eq!(
        prepared.model().physical().as_str(),
        "chat-small-2026-01-01"
    );
    assert_eq!(prepared.wire().endpoint().path(), "/v1/mock");
    assert_eq!(
        std::str::from_utf8(prepared.wire().body()).expect("the encoded body is UTF-8"),
        "chat-small-2026-01-01\nhello\n",
        "the provider receives the physical model name"
    );
}

#[test]
fn a_malformed_client_body_is_a_client_error_in_the_client_envelope() {
    let exchange = exchange(Capabilities::all(), Capabilities::all());
    let error = exchange
        .prepare(&request_facts(), b"no newline")
        .expect_err("the mock codec needs `model\\ntext`");
    assert_eq!(error.category(), ErrorCategory::Client);
    assert_eq!(error.canonical().http_status(), 400);

    let reply = exchange
        .error_reply(&error)
        .expect("the codec renders its own error envelope");
    assert_eq!(reply.status(), 400);
    assert_eq!(
        std::str::from_utf8(reply.body()).expect("the encoded error is UTF-8"),
        "error:400:invalid_request\n"
    );
}

#[test]
fn a_request_needing_an_unshared_capability_is_refused() {
    let exchange = exchange(Capabilities::all(), Capabilities::none());
    exchange
        .prepare(&request_facts(), &body("chat-small", "hello"))
        .expect("plain text needs no capability");

    let error = exchange
        .prepare(&streaming_request_facts(), &body("chat-small", "hello"))
        .expect_err("the provider does not stream");
    assert_eq!(
        error,
        ExchangeError::Unsupported {
            capability: Capability::Streaming
        }
    );
    assert_eq!(error.canonical().http_status(), 422);
    assert_eq!(error.canonical().code(), "unsupported_capability");
}

#[test]
fn an_image_request_needs_the_images_capability() {
    let exchange = exchange(Capabilities::all(), Capabilities::none());
    let error = exchange
        .prepare(&request_facts(), &body("chat-small", "image:cat"))
        .expect_err("the provider does not accept images");
    assert_eq!(
        error,
        ExchangeError::Unsupported {
            capability: Capability::Images
        }
    );
}

#[test]
fn the_exchange_types_cross_await_and_task_boundaries() {
    fn assert_send_sync<T: Send + Sync + 'static>() {}
    assert_send_sync::<Exchange>();
    assert_send_sync::<PreparedExchange>();
    assert_send_sync::<ClientReply>();
    assert_send_sync::<ExchangeError>();
    assert_send_sync::<ExchangeBuildError>();

    // One decoder per response owned by the encoder: `Send`, but deliberately not `Sync`, because
    // its buffer is mutated by the request that drives it.
    fn assert_send<T: Send + 'static>() {}
    assert_send::<StreamEncoder>();
}

#[test]
fn every_failure_maps_to_a_valid_canonical_error() {
    let upstream = CanonicalError::try_new(
        ErrorCategory::Upstream,
        "mock_refused",
        "the mock provider refused the request",
        429,
    )
    .expect("a valid canonical error");

    let failures = [
        (
            ExchangeError::Unsupported {
                capability: Capability::Files,
            },
            ("unsupported_capability", 422),
        ),
        (
            ExchangeError::Codec(CodecError::Unsupported {
                protocol: PROTOCOL,
                feature: Capability::Tools,
            }),
            ("unsupported_capability", 422),
        ),
        (
            ExchangeError::Codec(CodecError::UnsupportedField {
                protocol: PROTOCOL,
                field: "n".into(),
            }),
            ("unsupported_capability", 422),
        ),
        (
            ExchangeError::Provider(ProviderError::Unsupported {
                kind: KIND,
                feature: Capability::Images,
            }),
            ("unsupported_capability", 422),
        ),
        (
            ExchangeError::Provider(ProviderError::UnsupportedField {
                kind: KIND,
                field: "message.content".into(),
            }),
            ("unsupported_capability", 422),
        ),
        (
            ExchangeError::Codec(CodecError::InvalidRequest {
                protocol: PROTOCOL,
                reason: "a diagnostic".to_owned(),
            }),
            ("invalid_request", 400),
        ),
        (
            ExchangeError::Codec(CodecError::Encode {
                protocol: PROTOCOL,
                reason: "a diagnostic".to_owned(),
            }),
            ("client_encode_failed", 500),
        ),
        (
            ExchangeError::Provider(ProviderError::InvalidRequest {
                kind: KIND,
                reason: "a diagnostic".to_owned(),
            }),
            ("provider_encode_failed", 500),
        ),
        (
            ExchangeError::Provider(ProviderError::InvalidEndpoint {
                kind: KIND,
                source: EndpointError::Path,
            }),
            ("upstream_endpoint_invalid", 500),
        ),
        (
            ExchangeError::Provider(ProviderError::Malformed {
                kind: KIND,
                reason: "a diagnostic".to_owned(),
            }),
            ("upstream_protocol_error", 502),
        ),
        (
            ExchangeError::Provider(ProviderError::Upstream { error: upstream }),
            ("mock_refused", 429),
        ),
        (
            ExchangeError::InvalidCanonical {
                source: SchemaError::EmptyConversation,
            },
            ("internal_error", 500),
        ),
    ];

    for (failure, (code, status)) in failures {
        let canonical = failure.canonical();
        canonical
            .validate()
            .expect("every mapped constant satisfies the canonical error contract");
        assert_eq!(canonical.code(), code);
        assert_eq!(canonical.http_status(), status);
        assert_eq!(failure.category(), canonical.category());
        assert!(
            !canonical.message().is_empty(),
            "a client answer always carries a message"
        );
    }
}
