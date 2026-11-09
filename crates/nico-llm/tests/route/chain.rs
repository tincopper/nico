//! The compiled attempt chain: member order, the capability intersection, and the one-target proof.
//!
//! The chain is what a retry and a fallback move across, so its construction is the publication's
//! statement about the route: which targets it serves, in which order, and what every one of them
//! can do. The one-target chain must also prepare a request byte-identically to the bare exchange a
//! phase-1 publication held — a route that names no fallback changed nothing.

use std::sync::Arc;

use nico_http::llm::{ConvertingRoute, LlmRoute};
use nico_llm::{
    AttemptChain, BreakerParams, Capabilities, Capability, ChainBuildError, ChainMember, Exchange,
    ExchangeMode, TargetIdentity, protocol::RequestFacts, route::ConvertRoute,
};

use crate::{
    mocks::{KIND as MOCK_KIND, MockCodec, MockProvider, StreamBehaviour},
    support::{chain_of, client_body, exchange, view},
};

/// A mock exchange on the mocks' shared route, for chain shapes the real pair cannot express.
fn mock_exchange(codec: Capabilities, adapter: Capabilities) -> Arc<Exchange> {
    Arc::new(
        Exchange::try_new(
            MockCodec::new(codec),
            MockProvider::new(adapter, StreamBehaviour::Lines),
            crate::mocks::route(ExchangeMode::Convert),
        )
        .expect("the mocks pair on the route"),
    )
}

/// Names a chain member after its position, so a test can assert the order it was compiled in.
fn member_of(name: &str, exchange: Arc<Exchange>, threshold: u32) -> ChainMember {
    ChainMember::new(
        exchange,
        TargetIdentity::new(
            name,
            "http://127.0.0.1:8080",
            MOCK_KIND,
            BreakerParams::new(threshold, 30_000),
        ),
    )
}

#[test]
fn an_empty_member_list_is_refused_not_panicked() {
    let error = AttemptChain::try_new(Vec::new()).expect_err("no member cannot serve");
    assert_eq!(error, ChainBuildError);
}

#[test]
fn members_keep_their_compilation_order() {
    let primary = member_of(
        "primary",
        mock_exchange(Capabilities::none(), Capabilities::none()),
        5,
    );
    let fallback = member_of(
        "secondary",
        mock_exchange(Capabilities::none(), Capabilities::none()),
        3,
    );
    let chain = AttemptChain::try_new(vec![primary, fallback]).expect("two members are a chain");
    let names: Vec<&str> = chain
        .members()
        .iter()
        .map(|member| member.identity().provider())
        .collect();
    assert_eq!(names, ["primary", "secondary"]);
    assert_eq!(chain.primary().identity().provider(), "primary");
}

#[test]
fn the_chain_capability_set_is_the_intersection_of_its_members() {
    // The fallback serves less than the primary: a request that needs Tools would pass the primary's
    // own gate and fail inside the fallback's encoder, which is a gateway defect. The intersection
    // is the chain's promise, computed once at publication.
    let wide = Capabilities::none()
        .with(Capability::Streaming)
        .with(Capability::Tools);
    let narrow = Capabilities::none().with(Capability::Streaming);
    let primary = member_of("primary", mock_exchange(wide, wide), 5);
    let fallback = member_of("secondary", mock_exchange(narrow, narrow), 5);
    let chain = AttemptChain::try_new(vec![primary, fallback]).expect("two members are a chain");
    assert_eq!(chain.capabilities(), narrow);
}

#[test]
fn identities_carry_the_breaker_parameters_the_member_runs_with() {
    let primary = member_of(
        "primary",
        mock_exchange(Capabilities::none(), Capabilities::none()),
        7,
    );
    let chain = AttemptChain::try_new(vec![primary]).expect("one member is a chain");
    let identity = chain.primary().identity();
    assert_eq!(identity.provider(), "primary");
    assert_eq!(identity.kind(), MOCK_KIND);
    assert_eq!(identity.params().threshold(), 7);
    assert_eq!(identity.params().cooldown_ms(), 30_000);
}

#[test]
fn a_one_target_chain_prepares_identically_to_the_bare_exchange() {
    // The zero-behavior proof: a route that declares no attempts publishes a chain whose primary is
    // the exchange a phase-1 publication held, and the data plane reads the front-end — so the bytes
    // a request produces must not differ between the two shapes.
    let exchange = exchange();
    let route = ConvertRoute::new(
        chain_of(Arc::clone(&exchange)),
        crate::support::breakers(),
        1,
    );

    let body = client_body(false);
    let request = view(&body);
    let via_route = route.prepare(&request).expect("the route prepares");

    // The codec reads owned facts, exactly as the front-end builds them.
    let facts = RequestFacts::new(
        request.method(),
        request.path(),
        request
            .headers()
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
    );
    let direct = exchange
        .prepare(&facts, request.body())
        .expect("the bare exchange prepares");

    assert_eq!(
        via_route.body(),
        direct.into_wire().into_body().as_slice(),
        "the one-target chain changes no byte a request produces"
    );
}

#[test]
fn the_one_target_chain_reports_the_front_end_facts_of_its_primary() {
    let exchange = exchange();
    let route = ConvertRoute::new(chain_of(exchange), crate::support::breakers(), 1);
    assert_eq!(route.protocol(), "openai.chat");
    assert_eq!(route.provider(), "openai");
    assert_eq!(route.path(), "/v1/chat/completions");
    let destination = route.destination();
    assert_eq!(destination.host(), "127.0.0.1");
    assert_eq!(destination.port(), 8080);
}
