//! Model filtering in a provider pool: a member that cannot serve the requested model is not a
//! candidate, and a route with no such member answers `403 model_not_allowed` without trying the
//! fallback chain.
//!
//! These tests drive a real `AttemptSequence` over real exchanges. `decode` must run first: before the
//! body arrives there is no model name to filter on, and the pre-body behaviour is deliberately
//! unchanged.

use std::sync::Arc;

use nico_llm::{
    AttemptChain, AttemptSequence, BreakerParams, ChainMember, Exchange, PoolOrder, ProviderKind,
    ProviderRoute, TargetIdentity,
    protocol::openai_chat::{OpenAiChatCodec, PROTOCOL},
    provider::openai::{OpenAiProvider, PROVIDER_KIND},
};
use tokio::time::Instant;

use crate::{
    mocks::request_facts,
    support::{BASE_URL, MODEL, breakers},
};

/// A Chat Completions client body naming `model`, in the wire the real OpenAI codec decodes.
fn client_body(model: &str) -> Vec<u8> {
    format!(r#"{{"model":"{model}","messages":[{{"role":"user","content":"hello"}}]}}"#)
        .into_bytes()
}

/// Static member names for the identity table.
const MEMBER_NAMES: [&str; 3] = ["provider-0", "provider-1", "provider-2"];

/// Builds an exchange whose route only serves `allowed`, via a `model_allowlist`.
fn restricted(allowed: &str) -> Arc<Exchange> {
    let route = ProviderRoute::try_new("chat", PROTOCOL, PROVIDER_KIND, BASE_URL)
        .expect("a valid route")
        .with_model_allowlist(vec![allowed.to_owned()])
        .expect("a valid allowlist");
    Arc::new(
        Exchange::try_new(
            Arc::new(OpenAiChatCodec::new()),
            Arc::new(OpenAiProvider::new()),
            Arc::new(route),
        )
        .expect("the codec, adapter and route pair"),
    )
}

/// Builds a chain whose first `exchanges.len()` members form a pool, optionally with a fallback.
fn pooled(exchanges: Vec<Arc<Exchange>>, fallback: Option<Arc<Exchange>>) -> Arc<AttemptChain> {
    let mut members: Vec<ChainMember> = exchanges
        .into_iter()
        .enumerate()
        .map(|(index, exchange)| {
            let identity = TargetIdentity::new(
                MEMBER_NAMES[index],
                BASE_URL,
                PROVIDER_KIND,
                BreakerParams::new(5, 30_000),
            );
            ChainMember::new(exchange, identity)
        })
        .collect();
    let pooled = members.len();
    if let Some(exchange) = fallback {
        let identity = TargetIdentity::new(
            "fallback",
            BASE_URL,
            ProviderKind::new("openai"),
            BreakerParams::new(5, 30_000),
        );
        members.push(ChainMember::new(exchange, identity));
    }
    let order = PoolOrder::round_robin(pooled);
    Arc::new(AttemptChain::with_pool(members, Some(order)).expect("the pool fits the chain"))
}

/// Decodes a request for `model` into a fresh sequence over `chain`.
fn decoded(chain: Arc<AttemptChain>, model: &str) -> AttemptSequence {
    let mut sequence = AttemptSequence::new(chain, breakers(), 1);
    sequence
        .decode(&request_facts(), &client_body(model))
        .expect("the fixture body decodes");
    sequence
}

#[test]
fn only_members_that_serve_the_model_are_offered() {
    // Member 0 serves only "alpha", member 1 only "beta". A "beta" request must reach member 1 on
    // the first attempt even though round robin would have started at 0.
    let chain = pooled(vec![restricted("alpha"), restricted("beta")], None);

    let mut sequence = decoded(chain, "beta");
    sequence
        .select_first(Instant::now(), None)
        .expect("a member serves");

    assert_eq!(
        sequence.current(),
        1,
        "the member that cannot serve the model was filtered out before the strategy ran"
    );
}

#[test]
fn a_filtered_member_is_never_selected_on_a_later_round() {
    let chain = pooled(vec![restricted("alpha"), restricted("beta")], None);
    let mut sequence = decoded(chain, "beta");

    sequence
        .select_first(Instant::now(), None)
        .expect("member 1 serves");
    assert_eq!(sequence.current(), 1);
    // The walk restarts after a failure; member 0 is still not a candidate, so no second member is
    // available and the sequence must refuse rather than send the request to a model it cannot serve.
    let refusal = sequence
        .select_first(Instant::now(), None)
        .expect_err("no other member serves beta");
    assert_eq!(
        refusal.error().code(),
        "upstream_error",
        "the pool was exhausted by an attempt, not by a model mismatch: the two exhaustions must not
         be reported the same way"
    );
    assert_ne!(refusal.error().http_status(), 403);
}

#[test]
fn a_route_with_no_member_serving_the_model_refuses_with_model_not_allowed() {
    let chain = pooled(vec![restricted("alpha"), restricted("beta")], None);
    let mut sequence = decoded(chain, "gamma");

    let refusal = sequence
        .select_first(Instant::now(), None)
        .expect_err("no member serves gamma");

    let error = refusal.error();
    assert_eq!(error.code(), "model_not_allowed");
    assert_eq!(error.http_status(), 403);
    assert_eq!(error.category(), nico_core::ErrorCategory::Client);
}

#[test]
fn a_model_mismatch_does_not_reach_the_fallback_chain() {
    // The fallback member is unrestricted, so it WOULD serve gamma. The point of the test is that
    // the route's mismatch is answered before the chain is even considered.
    let fallback = Arc::new(
        Exchange::try_new(
            Arc::new(OpenAiChatCodec::new()),
            Arc::new(OpenAiProvider::new()),
            Arc::new(
                ProviderRoute::try_new("chat", PROTOCOL, PROVIDER_KIND, BASE_URL)
                    .expect("a valid route"),
            ),
        )
        .expect("the fallback exchange pairs"),
    );
    let chain = pooled(
        vec![restricted("alpha"), restricted("beta")],
        Some(fallback),
    );
    let mut sequence = decoded(chain, "gamma");

    let refusal = sequence
        .select_first(Instant::now(), None)
        .expect_err("no member serves gamma");

    assert_eq!(refusal.error().code(), "model_not_allowed");
    assert!(
        sequence.take_skipped().is_empty(),
        "no member was skipped by a breaker: the walk stopped at the model filter"
    );
}

#[test]
fn an_unrestricted_member_keeps_serving_every_model() {
    // The compatibility direction: a document that declares no `model_allowlist` at all must behave
    // exactly as it did before filtering existed.
    let unrestricted = Arc::new(
        Exchange::try_new(
            Arc::new(OpenAiChatCodec::new()),
            Arc::new(OpenAiProvider::new()),
            Arc::new(
                ProviderRoute::try_new("chat", PROTOCOL, PROVIDER_KIND, BASE_URL)
                    .expect("a valid route"),
            ),
        )
        .expect("the exchange pairs"),
    );
    let chain = pooled(vec![unrestricted], None);
    let mut sequence = decoded(chain, "any-model-name");

    sequence
        .select_first(Instant::now(), None)
        .expect("the member serves");
    assert_eq!(sequence.current(), 0);
}

#[test]
fn a_member_with_its_own_model_map_is_filtered_by_the_physical_name() {
    // Filtering uses each member's resolved PHYSICAL name. A member that maps "fast" -> "alpha" and
    // allows only "alpha" serves a "fast" request; a member that maps "fast" -> "beta" but allows
    // only "alpha" does not.
    let mapped = |physical: &str, allowed: &str| -> Arc<Exchange> {
        let route = ProviderRoute::try_new("chat", PROTOCOL, PROVIDER_KIND, BASE_URL)
            .expect("a valid route")
            .with_model_map(
                vec![(MODEL.to_owned(), physical.to_owned())]
                    .into_iter()
                    .collect(),
            )
            .expect("a valid model map")
            .with_model_allowlist(vec![allowed.to_owned()])
            .expect("a valid allowlist");
        Arc::new(
            Exchange::try_new(
                Arc::new(OpenAiChatCodec::new()),
                Arc::new(OpenAiProvider::new()),
                Arc::new(route),
            )
            .expect("the exchange pairs"),
        )
    };
    let chain = pooled(
        vec![mapped("beta", "alpha"), mapped("alpha", "alpha")],
        None,
    );
    let mut sequence = decoded(chain, MODEL);

    sequence
        .select_first(Instant::now(), None)
        .expect("one member serves");
    assert_eq!(
        sequence.current(),
        1,
        "the filter resolved each member's own model_map before checking its allowlist"
    );
}
