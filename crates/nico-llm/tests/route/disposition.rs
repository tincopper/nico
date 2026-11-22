//! What a failed attempt costs: the retry budget, the member, or nothing.
//!
//! The classification is the difference between "one request billed once" and "one request billed
//! once per provider", so every test here states one row of the table and asserts the *observable*
//! consequence: how many encodes happened, and which member the walk ended on.

use std::sync::Arc;

use nico_llm::{
    AttemptSequence, BreakerParams, BreakerTable, Capabilities, ChainMember, Exchange,
    ProviderRoute, TargetIdentity, Verdict,
    route::{AttemptChain, PoolOrder},
};
use tokio::time::Instant;

use crate::mocks::{KIND, MockCodec, MockProvider, PROTOCOL, StreamBehaviour, body, request_facts};

/// The mock adapter kind the fixtures use.
const HTTP: &str = "http://127.0.0.1:8080";

/// A route whose model map renames the request, so two members' wires differ.
fn route() -> Arc<ProviderRoute> {
    Arc::new(ProviderRoute::try_new("chat-route", PROTOCOL, KIND, HTTP).expect("a valid route"))
}

/// An exchange over a provider that answers `status`'s classification.
///
/// `retryable` is the provider's own table verdict, which the classification treats as authoritative
/// for statuses it does not name.
fn exchange(retryable: bool) -> Arc<Exchange> {
    let provider = if retryable {
        MockProvider::retrying(Capabilities::none(), StreamBehaviour::Lines)
    } else {
        MockProvider::new(Capabilities::none(), StreamBehaviour::Lines)
    };
    Arc::new(
        Exchange::try_new(MockCodec::new(Capabilities::none()), provider, route())
            .expect("the mocks pair on the route"),
    )
}

/// A two-member chain whose first two members form a pool.
fn pooled_chain(retryable: bool) -> Arc<AttemptChain> {
    let members: Vec<ChainMember> = ["one", "two"]
        .into_iter()
        .map(|name| {
            ChainMember::new(
                exchange(retryable),
                TargetIdentity::new(name, HTTP, KIND, BreakerParams::new(5, 30_000)),
            )
        })
        .collect();
    Arc::new(
        AttemptChain::with_pool(members, Some(PoolOrder::round_robin(2)))
            .expect("two pooled members are a chain"),
    )
}

/// A sequence over `chain` with `budget` attempts per member, already decoded and selected.
fn ready(chain: Arc<AttemptChain>, budget: u8) -> AttemptSequence {
    let mut sequence = AttemptSequence::new(chain, Arc::new(BreakerTable::new()), budget);
    let facts = request_facts();
    sequence
        .decode(&facts, &body("chat-small", "hello"))
        .expect("the fixture body decodes");
    sequence
        .select_first(Instant::now(), None)
        .expect("the first member admits");
    sequence
        .encode_current()
        .expect("the first attempt encodes");
    sequence
}

#[test]
fn a_rate_limit_moves_to_another_member_without_spending_the_budget() {
    // 429 is about the member's window, not the route's allowance: the walk must leave it at once and
    // the next member must serve the *first* attempt of its own budget.
    let mut sequence = ready(pooled_chain(true), 2);
    let first = sequence.current();

    let verdict = sequence
        .classify(429, Instant::now())
        .expect("moving to the next member encodes");

    assert_eq!(verdict, Verdict::Retry);
    assert_ne!(sequence.current(), first, "the throttled member is left");
    assert_eq!(
        sequence.attempt_number(),
        2,
        "one attempt was encoded so far; the next is the second"
    );
    assert_eq!(
        sequence.budget(),
        2,
        "the route's budget is a document fact and does not move"
    );
}

#[test]
fn a_credential_failure_moves_to_another_member() {
    for status in [401, 403] {
        let mut sequence = ready(pooled_chain(true), 1);
        let first = sequence.current();

        let verdict = sequence
            .classify(status, Instant::now())
            .expect("the next member encodes");

        assert_eq!(verdict, Verdict::Retry, "{status} leaves the member");
        assert_ne!(sequence.current(), first, "{status} moves the walk on");
    }
}

#[test]
fn a_caller_mistake_answers_immediately() {
    // 400 is the request's fault. Asking another provider would bill the same fault twice and return
    // the same answer, so the attempt is final and the walk stops where it is.
    for status in [400, 404, 422] {
        let mut sequence = ready(pooled_chain(true), 1);
        let current = sequence.current();

        let verdict = sequence
            .classify(status, Instant::now())
            .expect("classification is synchronous and cannot fail");

        assert_eq!(verdict, Verdict::Answer, "{status} is the caller's answer");
        assert_eq!(
            sequence.current(),
            current,
            "{status} must not move to another provider"
        );
    }
}

#[test]
fn a_retryable_server_failure_spends_the_budget_first() {
    // 5xx is the member's problem, so the budget applies: with two attempts allowed, the same member
    // is retried before the walk leaves it.
    let mut sequence = ready(pooled_chain(true), 2);
    let first = sequence.current();

    let verdict = sequence
        .classify(502, Instant::now())
        .expect("the retry encodes");

    assert_eq!(verdict, Verdict::Retry);
    assert_eq!(
        sequence.current(),
        first,
        "a retryable failure within budget retries the same member"
    );

    // The second failure exhausts the budget and moves the walk on.
    let verdict = sequence
        .classify(502, Instant::now())
        .expect("the next member encodes");

    assert_eq!(verdict, Verdict::Retry);
    assert_ne!(
        sequence.current(),
        first,
        "an exhausted budget leaves the member"
    );
}

#[test]
fn a_non_retryable_server_failure_is_answered() {
    // The provider table says this status cannot succeed on another attempt, and the classification
    // trusts it: the client is answered instead of the request being retried.
    let mut sequence = ready(pooled_chain(false), 4);

    let verdict = sequence
        .classify(500, Instant::now())
        .expect("classification is synchronous");

    assert_eq!(verdict, Verdict::Answer);
}

#[test]
fn an_excluded_member_is_named_for_the_request_log() {
    let mut sequence = ready(pooled_chain(true), 1);

    let _ = sequence.classify(429, Instant::now());
    let excluded = sequence.take_excluded();

    assert_eq!(excluded.len(), 1);
    assert_eq!(excluded[0].1.as_str(), "rate_limited");
}

#[test]
fn an_excluded_member_is_not_drawn_again_by_the_same_request() {
    // A rate-limited member must not be handed the next attempt: it is recorded as tried, and the
    // order is drawn from what is left.
    let mut sequence = ready(pooled_chain(true), 1);
    let throttled = sequence.current();

    let _ = sequence.classify(429, Instant::now());
    let next = sequence.current();

    assert_ne!(next, throttled, "the excluded member is skipped");
}
