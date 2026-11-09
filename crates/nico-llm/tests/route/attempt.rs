//! The attempt sequence: the order a request walks its chain, and where it stops.
//!
//! These tests drive a real sequence over mock exchanges — the orchestration is what is under test,
//! and the mocks' trivial wire (`model\ntext`) makes every decision observable in the bytes. The
//! sequence is asked the way the data plane asks it: `decode` when the client body ends,
//! `select_first` before the first attempt, `encode_current` for each attempt, `classify` at every
//! non-2xx head, `succeeded` at every 2xx one.

use std::{collections::BTreeMap, sync::Arc};

use nico_llm::{
    Admission, AttemptOutcome, AttemptSequence, BreakerParams, BreakerPhase, BreakerTable,
    Capabilities, Capability, Exchange, ExchangeError, ExchangeMode, ProviderRoute, TargetIdentity,
    Verdict, route::AttemptChain,
};
use tokio::time::Instant;

use crate::mocks::{
    KIND, MockCodec, MockProvider, PROTOCOL, StreamBehaviour, Trace, body, request_facts,
};

/// The budget most tests use: one attempt per target, the document's default.
const NO_RETRY: u8 = 1;

/// A route whose map renames the model to `physical`, so two members' wires differ observably.
fn route_renaming(physical: &str) -> Arc<ProviderRoute> {
    let mut map = BTreeMap::new();
    map.insert("chat-small".to_owned(), physical.to_owned());
    Arc::new(
        ProviderRoute::try_new("chat-route", PROTOCOL, KIND, "http://127.0.0.1:8080")
            .expect("a valid route")
            .with_mode(ExchangeMode::Convert)
            .with_model_map(map)
            .expect("a valid map"),
    )
}

/// An exchange whose failure statuses answer retryable, over a route that renames to `physical`.
fn exchange_with(codec: Capabilities, provider: Capabilities, physical: &str) -> Arc<Exchange> {
    Arc::new(
        Exchange::try_new(
            MockCodec::new(codec),
            MockProvider::retrying(provider, StreamBehaviour::Lines),
            route_renaming(physical),
        )
        .expect("the mocks pair on the route"),
    )
}

/// An exchange whose closed table marks no status retryable, over a route that renames to
/// `physical`.
fn exchange_strict(codec: Capabilities, provider: Capabilities, physical: &str) -> Arc<Exchange> {
    Arc::new(
        Exchange::try_new(
            MockCodec::new(codec),
            MockProvider::new(provider, StreamBehaviour::Lines),
            route_renaming(physical),
        )
        .expect("the mocks pair on the route"),
    )
}

/// Names a chain member, the way a publication would.
fn member(name: &str, exchange: Arc<Exchange>) -> nico_llm::ChainMember {
    nico_llm::ChainMember::new(
        exchange,
        TargetIdentity::new(
            name,
            "http://127.0.0.1:8080",
            KIND,
            BreakerParams::new(1, 30_000),
        ),
    )
}

/// A chain of two members, both capable of everything.
fn two_members() -> Arc<AttemptChain> {
    Arc::new(
        AttemptChain::try_new(vec![
            member(
                "primary",
                exchange_with(Capabilities::none(), Capabilities::none(), "primary-model"),
            ),
            member(
                "secondary",
                exchange_with(
                    Capabilities::none(),
                    Capabilities::none(),
                    "secondary-model",
                ),
            ),
        ])
        .expect("two members are a chain"),
    )
}

/// A chain of two members whose codecs report every decode to `trace`.
///
/// The test that pins "one decode, however many attempts" needs the *sequence* to be the thing that
/// decodes: an untraced mock could not tell a sequence that decodes once from one that decodes per
/// attempt, because the only observed decode would be the test's own.
fn traced_members(trace: &Trace) -> Arc<AttemptChain> {
    let member_of = |name: &str, physical: &str| {
        let exchange = Arc::new(
            Exchange::try_new(
                MockCodec::traced(Capabilities::none(), trace.clone()),
                MockProvider::retrying(Capabilities::none(), StreamBehaviour::Lines),
                route_renaming(physical),
            )
            .expect("the mocks pair on the route"),
        );
        member(name, exchange)
    };
    Arc::new(
        AttemptChain::try_new(vec![
            member_of("primary", "primary-model"),
            member_of("secondary", "secondary-model"),
        ])
        .expect("two members are a chain"),
    )
}

/// A sequence over `chain` that has decoded the fixture body once.
fn sequence(chain: Arc<AttemptChain>, budget: u8) -> AttemptSequence {
    let mut state = AttemptSequence::new(chain, Arc::new(BreakerTable::new()), budget);
    state
        .decode(&request_facts(), &body("chat-small", "hello"))
        .expect("the chain serves the body");
    state
}

/// Runs the sequence to its first attempt.
fn started(sequence: &mut AttemptSequence) {
    sequence
        .select_first(Instant::now())
        .expect("the chain serves");
    sequence.encode_current().expect("the member encodes");
}

#[test]
fn the_first_attempt_is_the_first_member() {
    let mut sequence = sequence(two_members(), NO_RETRY);
    started(&mut sequence);
    assert_eq!(sequence.current(), 0);
    assert_eq!(
        sequence.plan().expect("the attempt was encoded").member(),
        0
    );
}

#[test]
fn a_retryable_failure_retries_the_same_target_within_the_budget() {
    // Budget 3: the first attempt, then two retries against the same member — the cheapest next
    // step, because network jitter heals and no new endpoint's uncertainty is introduced.
    let mut sequence = sequence(two_members(), 3);
    started(&mut sequence);
    let now = Instant::now();
    for _ in 0..2 {
        assert_eq!(
            sequence.classify(502, now).expect("the failure classifies"),
            Verdict::Retry,
            "within budget, the same target"
        );
        assert_eq!(sequence.current(), 0);
    }
}

#[test]
fn an_exhausted_budget_moves_to_the_next_member() {
    let mut sequence = sequence(two_members(), NO_RETRY);
    started(&mut sequence);
    let now = Instant::now();
    assert_eq!(
        sequence.classify(502, now).expect("the failure classifies"),
        Verdict::Retry
    );
    assert_eq!(
        sequence.current(),
        1,
        "the budget of one is spent, the chain moves"
    );
    assert_eq!(
        sequence
            .plan()
            .expect("the new attempt was encoded")
            .member(),
        1
    );
}

#[test]
fn the_chain_end_answers_with_the_last_response() {
    let chain = Arc::new(
        AttemptChain::try_new(vec![member(
            "primary",
            exchange_with(Capabilities::none(), Capabilities::none(), "primary-model"),
        )])
        .expect("one member is a chain"),
    );
    let mut sequence = sequence(chain, NO_RETRY);
    started(&mut sequence);
    let now = Instant::now();
    assert_eq!(
        sequence.classify(502, now).expect("the failure classifies"),
        Verdict::Exhausted
    );
}

#[test]
fn a_non_retryable_failure_serves_immediately_without_moving_the_chain() {
    // 401 is final: the caller is refused with it, no budget is burned, and no other target is
    // asked — a fallback would answer the same misconfigured credential somewhere else. The
    // members here use the strict posture: their closed table marks no status retryable.
    let chain = Arc::new(
        AttemptChain::try_new(vec![
            member(
                "primary",
                exchange_strict(Capabilities::none(), Capabilities::none(), "primary-model"),
            ),
            member(
                "secondary",
                exchange_strict(
                    Capabilities::none(),
                    Capabilities::none(),
                    "secondary-model",
                ),
            ),
        ])
        .expect("two members are a chain"),
    );
    let mut sequence = sequence(chain, 3);
    started(&mut sequence);
    let now = Instant::now();
    assert_eq!(
        sequence.classify(401, now).expect("the failure classifies"),
        Verdict::Answer
    );
    assert_eq!(sequence.current(), 0, "no other target was asked");
}

#[test]
fn leaving_a_target_counts_one_breaker_failure_not_one_per_attempt() {
    // Budget 3, all failing: the target leaves the chain once, so the breaker counts one failure —
    // a chain must not amplify one request's failures into several breaker events.
    let chain = two_members();
    let table = Arc::new(BreakerTable::new());
    let mut state = AttemptSequence::new(Arc::clone(&chain), Arc::clone(&table), 3);
    state
        .decode(&request_facts(), &body("chat-small", "hello"))
        .expect("the chain serves the body");
    started(&mut state);
    let now = Instant::now();
    for _ in 0..3 {
        let _ = state.classify(502, now).expect("classifies");
    }
    assert!(
        matches!(
            table.admit(chain.primary().identity(), now),
            Admission::Skip(BreakerPhase::Open)
        ),
        "one exhausted target is one breaker failure, and one is enough to open at threshold one"
    );
}

#[test]
fn a_success_resets_the_target_breaker() {
    let chain = two_members();
    let table = Arc::new(BreakerTable::new());
    let mut state = AttemptSequence::new(Arc::clone(&chain), table.clone(), NO_RETRY);
    state
        .decode(&request_facts(), &body("chat-small", "hello"))
        .expect("the chain serves the body");
    started(&mut state);
    let now = Instant::now();
    state.succeeded(now);
    assert!(
        matches!(
            table.admit(chain.primary().identity(), now),
            Admission::Admit
        ),
        "a target that answered stays servable"
    );
}

#[test]
fn an_open_target_is_skipped_without_a_plan() {
    let chain = two_members();
    let table = Arc::new(BreakerTable::new());
    let now = Instant::now();
    // The primary's breaker opened: the sequence starts at the fallback without contacting it.
    table.record(
        chain.primary().identity(),
        AttemptOutcome::Failure,
        None,
        now,
    );
    let mut state = AttemptSequence::new(chain, table, NO_RETRY);
    state
        .decode(&request_facts(), &body("chat-small", "hello"))
        .expect("the chain serves the body");
    state.select_first(now).expect("the fallback serves");
    state.encode_current().expect("the fallback encodes");
    assert_eq!(state.current(), 1);
    assert_eq!(
        state.take_skipped(),
        ["primary"],
        "the walk records what it skipped"
    );
}

#[test]
fn all_targets_skipped_refuses_with_the_last_recorded_error() {
    let chain = two_members();
    let table = Arc::new(BreakerTable::new());
    let now = Instant::now();
    // The primary failed last with a 429; the secondary's breaker is open but never produced an
    // error the sequence could reach. The refusal is the primary's history: the route is *for* it.
    let failure = chain.primary().exchange().error_for_status(429);
    table.record(
        chain.primary().identity(),
        AttemptOutcome::Failure,
        Some(&failure),
        now,
    );
    table.record(
        chain.members()[1].identity(),
        AttemptOutcome::Failure,
        None,
        now,
    );
    let mut state = AttemptSequence::new(chain, table, NO_RETRY);
    state
        .decode(&request_facts(), &body("chat-small", "hello"))
        .expect("the chain serves the body");
    let refusal = state
        .select_first(now)
        .expect_err("the sequence refuses what it cannot serve");
    assert_eq!(
        refusal.error().http_status(),
        429,
        "the refusal is the failure the target actually produced"
    );
}

#[test]
fn a_request_beyond_the_chain_capabilities_is_refused_before_any_target() {
    // The fallback serves less than the request needs: the gate is the chain's intersection, so the
    // refusal happens once, before any target is contacted or encoded.
    let chain = Arc::new(
        AttemptChain::try_new(vec![
            member(
                "primary",
                exchange_with(
                    Capabilities::none().with(Capability::Images),
                    Capabilities::none().with(Capability::Images),
                    "primary-model",
                ),
            ),
            member(
                "secondary",
                exchange_with(
                    Capabilities::none(),
                    Capabilities::none(),
                    "secondary-model",
                ),
            ),
        ])
        .expect("two members are a chain"),
    );
    let mut state = AttemptSequence::new(chain, Arc::new(BreakerTable::new()), NO_RETRY);
    let error = state
        .decode(&request_facts(), &body("chat-small", "image:payload"))
        .expect_err("the request needs more than the chain serves");
    assert!(
        matches!(error, ExchangeError::Unsupported { .. }),
        "expected the capability gate, got {error:?}"
    );
}

#[test]
fn the_canonical_request_is_decoded_once_and_encoded_per_target() {
    // One decode builds the sequence; every attempt re-encodes the same canonical value for its own
    // target, and the wires differ because the targets' model maps do. The members' codecs report
    // their decodes, so a sequence that decoded once per attempt would show two here.
    let trace = Trace::default();
    let mut sequence = sequence(traced_members(&trace), NO_RETRY);
    started(&mut sequence);
    let now = Instant::now();
    let first = sequence.plan().expect("encoded").wire().body().to_vec();
    assert_eq!(
        sequence
            .classify(502, now)
            .expect("the failure moves the chain"),
        Verdict::Retry
    );
    let second = sequence.plan().expect("encoded").wire().body().to_vec();
    assert_eq!(
        trace
            .steps()
            .iter()
            .filter(|step| **step == "decode")
            .count(),
        1,
        "one decode, however many attempts"
    );
    assert_ne!(
        first, second,
        "the same canonical encoded for two targets names each target's own model"
    );
}

#[test]
fn a_transport_failure_moves_the_chain_like_a_provider_502() {
    // A connect that never established or a read that broke produces no response head; the sequence
    // still treats the target as failed, with the closed table's own transport row.
    let mut sequence = sequence(two_members(), NO_RETRY);
    started(&mut sequence);
    let now = Instant::now();
    assert_eq!(
        sequence
            .transport_failed(now)
            .expect("the failure classifies"),
        Verdict::Retry
    );
    assert_eq!(sequence.current(), 1);
    assert_eq!(sequence.current_identity().provider(), "secondary");
}

#[test]
fn a_transport_failure_retries_the_same_target_within_the_budget() {
    // A connect failure is the "network jitter heals" case the budget-first rule exists for: with
    // budget 3 the sequence retries the same member twice before it spends the chain.
    let mut sequence = sequence(two_members(), 3);
    started(&mut sequence);
    let now = Instant::now();
    for _ in 0..2 {
        assert_eq!(
            sequence
                .transport_failed(now)
                .expect("the failure classifies"),
            Verdict::Retry,
            "within budget, the same target"
        );
        assert_eq!(sequence.current(), 0);
    }
    // The budget spent, the transport failure moves the chain exactly as a provider 502 would.
    assert_eq!(
        sequence
            .transport_failed(now)
            .expect("the failure classifies"),
        Verdict::Retry,
        "the next member takes over"
    );
    assert_eq!(sequence.current(), 1);
    assert_eq!(sequence.current_identity().provider(), "secondary");
}
