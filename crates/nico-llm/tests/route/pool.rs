//! A provider pool's ordering: which member one attempt tries first, and what is never repeated.
//!
//! The pool decides *order* and nothing else — budgets, breakers and the fallback chain keep the rules
//! they had. Every test here therefore states an order: which member the walk reaches, and that one
//! request never draws a member it already gave up on.

use std::{
    net::{IpAddr, Ipv4Addr},
    sync::Arc,
};

use nico_llm::{
    AttemptChain, AttemptSequence, BreakerParams, ChainMember, PoolOrder, TargetIdentity,
    route::select::{IpHash, RoundRobin, Source, WeightedRandom},
};

use crate::support::{BASE_URL, breakers, exchange};

/// A source that returns a fixed sequence, cycling when it runs out.
#[derive(Debug)]
struct Scripted(Vec<u64>);

impl Source for Scripted {
    fn below(&self, total: u64) -> u64 {
        // Every fixture draw is a single value; the position is the request count, which each test
        // states by building its own sequence.
        self.0.first().copied().unwrap_or_default() % total
    }
}

/// The member names a fixture pool uses, so each identity is a `&'static str`.
const NAMES: [&str; 3] = ["provider-0", "provider-1", "provider-2"];

/// Builds a chain whose first `weights.len()` members form a pool.
fn pooled(weights: &[u16], picker: Arc<dyn nico_llm::route::select::Picker>) -> Arc<AttemptChain> {
    let members: Vec<ChainMember> = weights
        .iter()
        .enumerate()
        .map(|(index, _)| {
            let identity = TargetIdentity::new(
                NAMES[index],
                BASE_URL,
                nico_llm::ProviderKind::new("openai"),
                BreakerParams::new(5, 30_000),
            );
            ChainMember::new(exchange(), identity)
        })
        .collect();
    let order = PoolOrder::new(picker, weights.to_vec());
    Arc::new(AttemptChain::with_pool(members, Some(order)).expect("the pool fits the chain"))
}

#[test]
fn a_pool_orders_its_members_by_the_strategy() {
    // A single draw of 0 over a 3:1 range lands on the heavy member, which is the first arm of the
    // order; the rest follow in pool order because a double-draw per member is what a *sequence* of
    // draws would be, and this fixture states one step.
    let chain = pooled(
        &[3, 1],
        Arc::new(WeightedRandom::new(Arc::new(Scripted(vec![0])))),
    );

    let pool = chain.pool().expect("the chain has a pool");
    let order = pool.order(&[0, 1], &["provider-0", "provider-1"], None);

    assert_eq!(order.len(), 2, "every untried member appears once");
    assert_eq!(
        order[0], 0,
        "a draw of 0 lands on the member with three shares"
    );
    assert_eq!(order[1], 1, "and the remaining member follows");
}

#[test]
fn round_robin_pool_order_follows_the_document() {
    let chain = pooled(&[1, 1, 1], Arc::new(RoundRobin::default()));
    let pool = chain.pool().expect("the chain has a pool");

    let first = pool.order(&[0, 1, 2], &["a", "b", "c"], None);
    let second = pool.order(&[0, 1, 2], &["a", "b", "c"], None);

    assert_eq!(
        first,
        [0, 1, 2],
        "the first request starts at the first member"
    );
    assert_eq!(
        second,
        [1, 0, 2],
        "the next request starts one further and keeps pool order after it"
    );
}

#[test]
fn a_pool_never_names_a_member_it_was_not_offered() {
    // The containment rule: the untried set shrinks as attempts fail, and the order may only contain
    // what is left.
    let chain = pooled(&[1, 1, 1], Arc::new(RoundRobin::default()));
    let pool = chain.pool().expect("the chain has a pool");

    let order = pool.order(&[1, 2], &["a", "b", "c"], None);

    assert_eq!(order.len(), 2);
    assert!(order.iter().all(|index| [1, 2].contains(index)));
    assert!(!order.contains(&0), "member 0 was already tried");
}

#[test]
fn a_request_does_not_attempt_the_same_pooled_member_twice() {
    // The walk records every pooled member it selected, so a second walk asks the strategy for a
    // smaller set. Without that, a pool of one would retry the same member until its budget ran out
    // even though the contract says a request never repeats a member.
    let chain = pooled(&[1, 1], Arc::new(RoundRobin::default()));
    let mut sequence = AttemptSequence::new(Arc::clone(&chain), breakers(), 1);

    sequence
        .select_first(tokio::time::Instant::now(), None)
        .expect("the first member admits");
    let first = sequence.current();
    sequence
        .select_first(tokio::time::Instant::now(), None)
        .expect("the walk still has a member left");

    assert_ne!(
        sequence.current(),
        first,
        "the second walk must not draw the member the first one used"
    );
}

#[test]
fn a_zero_weight_member_is_ordered_last() {
    // Zero means "not a distribution candidate": a member with no share contributes no width, so the
    // draw cannot reach it while any weighted member is still untried.
    let chain = pooled(
        &[1, 0],
        Arc::new(WeightedRandom::new(Arc::new(Scripted(vec![0])))),
    );
    let pool = chain.pool().expect("the chain has a pool");

    let order = pool.order(&[0, 1], &["weighted", "fallback-only"], None);

    assert_eq!(order[0], 0, "the weighted member is drawn first");
    assert_eq!(
        order[1], 1,
        "a member with no share is still reachable once nothing else is left"
    );
}

#[test]
fn a_chain_without_a_pool_reports_none() {
    // The compatibility direction: a single-target route has no pool, and the walk uses document
    // order exactly as it did before pools existed.
    let chain = crate::support::chain_of(exchange());

    assert!(chain.pool().is_none());
}

#[test]
fn a_pool_claiming_more_members_than_the_chain_has_is_refused() {
    let member = ChainMember::new(
        exchange(),
        TargetIdentity::new(
            "only",
            BASE_URL,
            nico_llm::ProviderKind::new("openai"),
            BreakerParams::new(5, 30_000),
        ),
    );
    let order = PoolOrder::new(Arc::new(RoundRobin::default()), vec![1, 1]);

    assert!(
        AttemptChain::with_pool(vec![member], Some(order)).is_err(),
        "a pool of two members over a chain of one would silently be short of members"
    );
}

#[test]
fn an_ip_hash_pool_keeps_the_same_caller_first() {
    // The draw keys on the caller's address: the same caller meets the same member first, however
    // many times the order is asked. A provider pool without a caller keeps its address-less
    // behaviour, which the other tests already cover.
    let pool = PoolOrder::new(Arc::new(IpHash::default()), vec![1, 1]);
    let caller = Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)));
    let names = &NAMES[..2];

    let first = pool.order(&[0, 1], names, caller)[0];
    for _ in 0..9 {
        assert_eq!(
            pool.order(&[0, 1], names, caller)[0],
            first,
            "the same caller meets the same member first, however many times the order is asked"
        );
    }
}
