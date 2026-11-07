//! The Responses provider adapter as a whole: identity, capabilities and statelessness.

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
    provider::openai_responses::{
        MAX_OUTPUT_ITEMS, NATIVE_PROTOCOL, OpenAiResponsesProvider, PROVIDER_KIND,
    },
};

#[test]
fn the_adapter_names_its_kind_native_protocol_and_revision() {
    let provider = OpenAiResponsesProvider::new();
    assert_eq!(provider.kind(), PROVIDER_KIND);
    assert_eq!(provider.native_protocol(), NATIVE_PROTOCOL);
    assert_eq!(PROVIDER_KIND.as_str(), "openai_responses");
    assert_eq!(
        NATIVE_PROTOCOL.as_str(),
        "openai.responses",
        "the registry key is the wire dialect, which a route's `protocol` must name"
    );
    assert_eq!(
        provider.kind(),
        ProviderKind::new("openai_responses"),
        "the kind is the vendor plus the dialect: Chat Completion is `openai`, this one is distinct"
    );
    assert_eq!(
        provider.version(),
        None,
        "this vendor publishes no versioned spelling of the dialect"
    );
}

#[test]
fn the_two_openai_dialects_are_two_kinds() {
    // One kind names one document shape. A single kind that picked a dialect from the client
    // protocol would make passthrough legality a set membership instead of equality.
    assert_ne!(
        PROVIDER_KIND.as_str(),
        nico_llm::provider::openai::PROVIDER_KIND.as_str(),
        "the two OpenAI wire formats are registered separately"
    );
    assert_ne!(
        NATIVE_PROTOCOL,
        nico_llm::provider::openai::NATIVE_PROTOCOL,
        "and they speak different protocols"
    );
}

#[test]
fn a_passthrough_route_pairs_the_adapters_protocol_with_a_codec() {
    // Passthrough legality is equality: the route's `protocol` must be the provider's native one.
    assert_eq!(NATIVE_PROTOCOL, support::PROTOCOL);
}

#[test]
fn the_adapter_declares_the_capabilities_it_actually_maps() {
    let capabilities = OpenAiResponsesProvider::new().capabilities();
    for capability in [
        Capability::Streaming,
        Capability::Tools,
        Capability::Images,
        Capability::ResponseFormat,
    ] {
        assert!(
            capabilities.supports(capability),
            "`{capability}` is mapped and must be declared"
        );
    }
    for absent in [Capability::Files, Capability::Reasoning] {
        assert!(
            !capabilities.supports(absent),
            "`{absent}` is not mapped: a file has no field here, and replaying a reasoning item \
             needs two handles the canonical model has no place for"
        );
    }
}

#[test]
fn the_adapter_is_a_stateless_singleton() {
    // The adapter is `Copy`: copying it is the compile-time proof that a request carries no state,
    // and that one instance may serve every route of this kind.
    let first = OpenAiResponsesProvider::new();
    let second = first;
    assert_eq!(first.kind(), second.kind());
    assert_eq!(first.capabilities(), second.capabilities());
}

#[test]
fn the_adapter_and_its_stream_cross_thread_boundaries() {
    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_send<T: Send>() {}
    assert_send_sync::<OpenAiResponsesProvider>();
    // A decoder is created per response and moves with the response task, so it is `Send` and need
    // not be `Sync`.
    let decoder = OpenAiResponsesProvider::new().stream_decoder();
    assert_send::<Box<dyn nico_llm::ProviderStream>>();
    drop(decoder);
}

#[test]
fn the_output_item_bound_is_a_bound() {
    // The peer picks every output index, so the decoder retains at most this many entries.
    const { assert!(MAX_OUTPUT_ITEMS > 0) };
}
