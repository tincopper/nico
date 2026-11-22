//! The order a route's provider pool is walked in, and the strategy that decides it.
//!
//! A pool changes *which* member one attempt tries first; it changes nothing about what happens after
//! a failure. That split is the whole file: the walk over attempts still belongs to
//! [`AttemptSequence`](super::AttemptSequence) — budgets, breakers and the fallback chain keep their
//! rules — while the order among the pooled members comes from here.
//!
//! Two properties are load-bearing:
//!
//! - **One request never tries the same member twice.** The walk hands the picker the members it has
//!   not tried yet, so the strategy orders a shrinking set. Without that, `weighted_random` would
//!   happily draw the member that just failed, and a pool of one would loop until its budget ran out.
//! - **The pool is a prefix of the chain.** Members `0..len` are the pool and the rest is the fallback
//!   chain, so every existing rule that reads the list — "the primary's history decides the refusal",
//!   "the chain is walked in order" — keeps its meaning without knowing pools exist.

use std::sync::Arc;

use nico_lb::{Members, Picker, RoundRobin, View, WeightedRandom};

/// How a route's pool members are ordered for one attempt.
///
/// Holds the strategy and the number of members it chooses among. The member facts a strategy reads
/// are the ones the chain already compiled: a weight from the document, a provider name for the
/// rendezvous key, and an in-flight count that is always zero — a provider attempt has no connection
/// this crate is counting, and `least_conn` is not a model-layer strategy for exactly that reason.
pub struct PoolOrder {
    picker: Arc<dyn Picker>,
    len: usize,
    /// Each pooled member's share, in pool order.
    weights: Vec<u16>,
}

impl PoolOrder {
    /// Builds the order for `len` pooled members.
    ///
    /// The strategy is a closed document vocabulary, so the mapping lives with the caller that owns
    /// that vocabulary; this type only holds what it was given.
    #[must_use]
    pub fn new(picker: Arc<dyn Picker>, weights: Vec<u16>) -> Self {
        Self {
            picker,
            len: weights.len(),
            weights,
        }
    }

    /// Returns the share the document gave the `index`-th pooled member.
    #[must_use]
    pub fn weight(&self, index: usize) -> u16 {
        self.weights.get(index).copied().unwrap_or_default()
    }

    /// Builds the order for a pool of `len` members weighted as `weights`.
    ///
    /// A convenience for the two strategies the model layer serves, so a caller without a document
    /// vocabulary of its own (a test) can still state a pool.
    #[must_use]
    pub fn weighted(weights: &[u16], source: Arc<dyn nico_lb::Source>) -> Self {
        Self::new(Arc::new(WeightedRandom::new(source)), weights.to_vec())
    }

    /// Builds a round-robin order for `len` members.
    #[must_use]
    pub fn round_robin(len: usize) -> Self {
        Self::new(Arc::new(RoundRobin::default()), vec![1; len])
    }

    /// Returns how many leading members of the chain form the pool.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns whether the pool has no members.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the order `untried` members are tried in, as chain indices.
    ///
    /// The strategy answers **once**, for the whole set: which member goes first. The rest follow in
    /// pool order. Asking it repeatedly to build a full permutation would be asking a different
    /// question — a rotation advances its position on every call, so the second pick of one request
    /// would skip the member the first pick named and produce an order no document asked for. A
    /// strategy's contract is one choice per attempt, and one attempt is one member.
    ///
    /// `untried` is what the walk has not attempted yet, in pool order, so the strategy cannot name a
    /// member the request already gave up on. When it answers with nothing — every remaining member
    /// has no share, which a pool of zero weights reaches — the rest stay in pool order rather than
    /// being dropped: the document named them, so they are still members the route may use.
    /// `client` is the address an `ip_hash` strategy keys its rendezvous on; `None` means the
    /// strategy's own address-less behaviour applies.
    #[must_use]
    pub fn order(
        &self,
        untried: &[usize],
        names: &[&str],
        client: Option<std::net::IpAddr>,
    ) -> Vec<usize> {
        if untried.is_empty() {
            return Vec::new();
        }
        let facts = Facts {
            weights: &self.weights,
            names,
        };
        let chosen = self.picker.pick(&View::new(&facts, untried, client));
        let Some(chosen) = chosen.filter(|index| untried.contains(index)) else {
            return untried.to_vec();
        };
        let mut order = Vec::with_capacity(untried.len());
        order.push(chosen);
        order.extend(untried.iter().copied().filter(|&index| index != chosen));
        order
    }
}

impl std::fmt::Debug for PoolOrder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PoolOrder")
            .field("picker", &self.picker)
            .field("len", &self.len)
            .finish()
    }
}

/// The member facts a strategy reads, built from what the chain already compiled.
struct Facts<'a> {
    weights: &'a [u16],
    names: &'a [&'a str],
}

impl Members for Facts<'_> {
    fn len(&self) -> usize {
        self.weights.len()
    }

    fn weight(&self, index: usize) -> u16 {
        self.weights.get(index).copied().unwrap_or_default()
    }

    fn active(&self, _index: usize) -> u32 {
        // A provider attempt holds no connection this crate counts. Reporting zero for everyone is the
        // honest value, and it is why `least_conn` is not offered as a model-layer strategy.
        0
    }

    fn name(&self, index: usize) -> &str {
        self.names.get(index).copied().unwrap_or("")
    }
}
