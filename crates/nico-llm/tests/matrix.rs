//! The client-protocol × provider matrix, as a table the protocol work extends.
//!
//! 1.4a freezes the shape with one row: an OpenAI Chat client and an OpenAI provider. The end-to-end
//! behaviour of that row lives in `nico-server/tests/llm_convert.rs`; this test answers the other
//! half of the contract — every declared pair has a codec and an adapter in the built-in registry,
//! and the two can be paired into one [`Exchange`] without a socket.
//!
//! 1.4b appends its Anthropic, Responses and Gemini rows to [`MATRIX`]. A row that is added without
//! both halves fails here, before any request can reach it.

use std::sync::Arc;

use nico_llm::{
    ClientCodec, Exchange, ProtocolId, Provider, ProviderKind, ProviderRoute, builtin_registry,
    protocol::openai_chat::PROTOCOL, provider::openai::PROVIDER_KIND,
};

/// The `(client protocol, provider kind)` pairs 1.4a serves.
///
/// The table is deliberately explicit instead of derived from the registries: a registry that
/// contains two kinds does not say they may be paired, and a pair that is missing a codec or an
/// adapter must be a test failure rather than a silently skipped combination.
const MATRIX: &[(ProtocolId, ProviderKind)] = &[(PROTOCOL, PROVIDER_KIND)];

#[test]
fn every_matrix_row_has_both_halves_and_pairs_into_an_exchange() {
    let (protocols, providers) = builtin_registry();

    for &(protocol, kind) in MATRIX {
        let codec: Arc<dyn ClientCodec> = protocols
            .codec(protocol.as_str())
            .unwrap_or_else(|| panic!("no client codec registered for `{protocol}`"))
            .clone();
        let adapter: Arc<dyn Provider> = providers
            .provider(kind.as_str())
            .unwrap_or_else(|| panic!("no provider adapter registered for `{kind}`"))
            .clone();
        let route = Arc::new(
            ProviderRoute::try_new("matrix", protocol, kind, "http://127.0.0.1:8080")
                .expect("the matrix route facts are valid"),
        );

        let exchange = Exchange::try_new(codec, adapter, route)
            .expect("a declared pair can be compiled into one exchange");
        assert_eq!(
            exchange.route().protocol(),
            protocol,
            "the exchange preserves the protocol half of the pair"
        );
        assert_eq!(
            exchange.route().provider_kind(),
            kind,
            "the exchange preserves the provider half of the pair"
        );
    }

    assert!(
        !MATRIX.is_empty(),
        "1.4a must declare at least the OpenAI Chat × OpenAI row"
    );
}
