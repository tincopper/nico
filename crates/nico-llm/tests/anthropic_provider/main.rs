//! The Anthropic provider adapter as a whole: its identity, its capabilities and its statelessness.

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
    Capability, Provider, ProviderKind,
    provider::anthropic::{
        AnthropicProvider, MAX_CONTENT_BLOCKS, NATIVE_PROTOCOL, PROVIDER_KIND, VERSION,
    },
};

#[test]
fn the_adapter_names_its_kind_native_protocol_and_revision() {
    let provider = AnthropicProvider::new();
    assert_eq!(provider.kind(), PROVIDER_KIND);
    assert_eq!(provider.native_protocol(), NATIVE_PROTOCOL);
    assert_eq!(PROVIDER_KIND.as_str(), "anthropic");
    assert_eq!(
        NATIVE_PROTOCOL.as_str(),
        "anthropic.messages",
        "the registry key is the wire dialect, which a route's `protocol` must name"
    );
    assert_eq!(
        provider.kind(),
        ProviderKind::new("anthropic"),
        "the registry key is the kind's own name"
    );
    assert_eq!(
        provider.version(),
        Some(VERSION),
        "the revision the endpoint sends is the one the assembly reports"
    );
    assert_eq!(VERSION, "2023-06-01");
}

#[test]
fn a_passthrough_route_pairs_the_adapters_protocol_with_a_codec() {
    // Passthrough legality is equality: the route's `protocol` must be the provider's native one.
    // The client codec for this dialect names itself `anthropic.messages` for that reason.
    assert_eq!(NATIVE_PROTOCOL.as_str(), "anthropic.messages");
    assert_eq!(NATIVE_PROTOCOL, support::PROTOCOL);
}

#[test]
fn the_adapter_declares_the_capabilities_it_actually_maps() {
    let capabilities = AnthropicProvider::new().capabilities();
    for capability in [Capability::Streaming, Capability::Tools, Capability::Images] {
        assert!(
            capabilities.supports(capability),
            "`{capability}` is mapped and must be declared"
        );
    }
    for absent in [
        Capability::Files,
        Capability::Reasoning,
        Capability::ResponseFormat,
    ] {
        assert!(
            !capabilities.supports(absent),
            "`{absent}` is not mapped: a file has no field, a thinking block cannot be replayed \
             without a budget this request cannot ask for, and the dialect has no JSON mode"
        );
    }
}

#[test]
fn the_adapter_is_a_stateless_singleton() {
    // The adapter is `Copy`: copying it is the compile-time proof that a request carries no state,
    // and that one instance may serve every route of this kind.
    let first = AnthropicProvider::new();
    let second = first;
    assert_eq!(first.kind(), second.kind());
    assert_eq!(first.capabilities(), second.capabilities());
}

#[test]
fn the_adapter_and_its_stream_cross_thread_boundaries() {
    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_send<T: Send>() {}
    assert_send_sync::<AnthropicProvider>();
    // A decoder is created per response and moves with the response task, so it is `Send` and need
    // not be `Sync`.
    let decoder = AnthropicProvider::new().stream_decoder();
    assert_send::<Box<dyn nico_llm::ProviderStream>>();
    drop(decoder);
}

#[test]
fn the_content_block_bound_is_a_bound() {
    // The peer picks every block index, so the decoder retains at most this many entries.
    const { assert!(MAX_CONTENT_BLOCKS > 0) };
}
