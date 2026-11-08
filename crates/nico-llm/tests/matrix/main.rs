//! The client-protocol × provider matrix: sixteen pairs, four native ones, one conversation.
//!
//! The matrix answers the composition question the per-protocol tests cannot: not "does this codec
//! speak its protocol", but "do any codec and any adapter translate the *same* canonical values, so
//! any route of this gateway is a composition of checked halves". The sixteen rows live in
//! [`rows`], the one conversation they all translate lives in [`support`] with its wire fixtures in
//! `tests/fixtures/matrix/`, and the fixture checks live in [`fixtures`].
//!
//! The table is explicit and never derived from the registries: a registry that contains four codecs
//! and four adapters does not say they may be paired, and a row that is added without both halves
//! must fail here, before any request can reach it.

#![expect(
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    reason = "a test that cannot build its fixture has nothing to return; one item kind differs               from another by panicking with both in hand; the fixture generator prints the               documents a deliberate canonical change regenerates"
)]

mod fixtures;
mod rows;
mod support;

use std::{num::NonZeroU32, sync::Arc};

use nico_http::llm::LlmRoute;
use nico_llm::{
    ClientCodec, EndpointError, Exchange, ExchangeBuildError, ExchangeMode, PassthroughRoute,
    Provider, ProviderError, ProviderRoute, builtin_registry,
};

/// The base URL every matrix route names: a name and port only, since no row contacts a peer.
const BASE_URL: &str = "http://127.0.0.1:8080";

/// The output ceiling the matrix routes compile with.
///
/// One dialect requires it at publication and the others ignore it, so every row carries the same
/// fact instead of special-casing one.
fn ceiling() -> NonZeroU32 {
    NonZeroU32::new(64).expect("a positive ceiling")
}

/// Every declared pair has both halves, pairs into one exchange, and reports exactly the capability
/// set its two sides share.
#[test]
fn the_whole_matrix_is_served() {
    let (protocols, providers) = builtin_registry();

    for row in rows::MATRIX {
        let codec: Arc<dyn ClientCodec> = protocols
            .codec(row.protocol.as_str())
            .unwrap_or_else(|| panic!("row `{}`: no client codec for `{}`", row.name, row.protocol))
            .clone();
        let adapter: Arc<dyn Provider> = providers
            .provider(row.kind.as_str())
            .unwrap_or_else(|| panic!("row `{}`: no provider adapter for `{}`", row.name, row.kind))
            .clone();
        let route = Arc::new(
            ProviderRoute::try_new("matrix", row.protocol, row.kind, BASE_URL)
                .expect("the matrix route facts are valid")
                .with_model_map(
                    [(support::LOGICAL.to_owned(), support::PHYSICAL.to_owned())]
                        .into_iter()
                        .collect(),
                )
                .expect("a valid model map")
                .with_max_output_tokens(ceiling()),
        );

        let exchange = Exchange::try_new(codec, adapter, route)
            .unwrap_or_else(|error| panic!("row `{}` does not compile: {error}", row.name));
        assert_eq!(
            exchange.route().protocol(),
            row.protocol,
            "row `{}`: the exchange preserves the protocol half",
            row.name
        );
        assert_eq!(
            exchange.route().provider_kind(),
            row.kind,
            "row `{}`: the exchange preserves the provider half",
            row.name
        );
        assert_eq!(
            exchange.capabilities(),
            row.capabilities,
            "row `{}`: the pair serves exactly what both sides declare",
            row.name
        );
    }

    assert_eq!(
        rows::MATRIX.len(),
        16,
        "four client codecs against four provider adapters: a new half adds its rows or fails here"
    );
}

/// The four native pairs align for passthrough: three publish, and the one whose request target
/// carries the model is refused at publication for that reason, not for an unparseable endpoint.
#[test]
fn the_native_pairs_publish_as_passthrough_or_refuse_for_the_target() {
    let (_, providers) = builtin_registry();

    for pair in rows::NATIVE {
        let adapter = providers
            .provider(pair.kind.as_str())
            .unwrap_or_else(|| panic!("pair `{}`: no adapter for `{}`", pair.name, pair.kind))
            .clone();
        let route = Arc::new(
            ProviderRoute::try_new("matrix", pair.protocol, pair.kind, BASE_URL)
                .expect("the matrix route facts are valid")
                .with_mode(ExchangeMode::Passthrough)
                .with_max_output_tokens(ceiling()),
        );

        if pair.publishable {
            let published = PassthroughRoute::try_new(Arc::clone(&route), adapter.as_ref())
                .unwrap_or_else(|error| panic!("pair `{}` does not publish: {error}", pair.name));
            assert_eq!(
                published.protocol(),
                pair.protocol.as_str(),
                "pair `{}`: the published front-end serves the route's protocol",
                pair.name
            );
        } else {
            let error = PassthroughRoute::try_new(Arc::clone(&route), adapter.as_ref())
                .expect_err("the pair whose target carries the model refuses passthrough");
            let ExchangeBuildError::Endpoint { source } = error else {
                panic!(
                    "pair `{}`: expected an endpoint refusal, got {error:?}",
                    pair.name
                );
            };
            let ProviderError::InvalidEndpoint {
                source: endpoint, ..
            } = source
            else {
                panic!(
                    "pair `{}`: expected an invalid endpoint, got {source:?}",
                    pair.name
                );
            };
            assert!(
                matches!(endpoint, EndpointError::RouteFact { field, .. } if field == "routes[].mode"),
                "pair `{}`: the refusal names the route fact, not an endpoint parse: {endpoint:?}",
                pair.name
            );
        }
    }
}
