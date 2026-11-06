//! The OpenAI provider adapter as a whole: its identity, its capabilities and its statelessness.

#![expect(
    clippy::expect_used,
    reason = "a test that cannot build its fixture has nothing to assert"
)]

mod endpoint;
mod exchange;
mod request;
mod response;
mod stream;
mod support;

use nico_llm::{
    Capability, ClientCodec, Provider, ProviderKind,
    protocol::openai_chat::{OpenAiChatCodec, PROTOCOL},
    provider::openai::{NATIVE_PROTOCOL, OpenAiProvider, PROVIDER_KIND},
};

#[test]
fn the_adapter_names_its_kind_and_native_protocol() {
    let provider = OpenAiProvider::new();
    assert_eq!(provider.kind(), PROVIDER_KIND);
    assert_eq!(PROVIDER_KIND.as_str(), "openai");
    assert_eq!(NATIVE_PROTOCOL.as_str(), "openai.chat");
    assert_eq!(
        provider.kind(),
        ProviderKind::new("openai"),
        "the registry key is the kind's own name"
    );
    assert_eq!(
        NATIVE_PROTOCOL,
        OpenAiChatCodec::new().protocol(),
        "a passthrough route is legal only when the provider's native protocol is the codec's"
    );
    assert_eq!(PROTOCOL, NATIVE_PROTOCOL);
}

#[test]
fn the_adapter_declares_the_capabilities_it_actually_maps() {
    let capabilities = OpenAiProvider::new().capabilities();
    for capability in [
        Capability::Streaming,
        Capability::Tools,
        Capability::Images,
        Capability::Reasoning,
        Capability::ResponseFormat,
    ] {
        assert!(
            capabilities.supports(capability),
            "`{capability}` is mapped and must be declared"
        );
    }
    assert!(
        !capabilities.supports(Capability::Files),
        "a canonical file part has no wire form, so the capability must not be declared"
    );
}

#[test]
fn the_adapter_is_a_stateless_singleton() {
    // The adapter is `Copy`: copying it is the compile-time proof that a request carries no state,
    // and that one instance may serve every route of this kind.
    let first = OpenAiProvider::new();
    let second = first;
    assert_eq!(first.kind(), second.kind());
    assert_eq!(
        first.capabilities(),
        second.capabilities(),
        "two instances declare the same capabilities"
    );
}

#[test]
fn the_adapter_and_its_stream_cross_thread_boundaries() {
    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_send<T: Send>() {}
    assert_send_sync::<OpenAiProvider>();
    // A decoder is created per response and moves with the response task, so it is `Send` and need
    // not be `Sync`.
    let decoder = OpenAiProvider::new().stream_decoder();
    assert_send::<Box<dyn nico_llm::ProviderStream>>();
    drop(decoder);
}
