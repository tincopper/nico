//! The strategy seam: what "one of the available members" means, and the four implementations.
//!
//! A [`Picker`] answers one question — *which candidate serves this request* — and owns everything it
//! needs to answer it. Each part of that shape removes one way the module could drift:
//!
//! - **The picker owns its state.** A rotation position, a smooth-weight vector, a tie-break counter:
//!   each lives in the implementation that reads it, never in the pool. So the pool knows no
//!   strategy's layout, and a strategy that needs no state costs neither an allocation nor a lock for
//!   the strategies that do.
//! - **The input is one [`View`], not one signature per strategy.** A rule reads the accessors it
//!   needs and nothing else, so adding an input is a new accessor rather than a new argument on every
//!   implementation. [`Members`] is the same seam from the other side: it is what a rule may know
//!   about a member, and a second kind of pool (a provider pool, say) can implement it without the
//!   rules changing.
//! - **A publication carries state forward by identity, not by index.** [`Picker::inherit`] receives
//!   the member names of both generations, so a member that kept its identity keeps its accumulated
//!   weight while a member that changed is a different member and starts from nothing.
//!
//! The ordinals here answer *in which order*, and nothing about distribution: [`IpHash`] and
//! [`WeightedRandom`] live in [`weighted`](crate::weighted), because they promise a mapping or a
//! proportion rather than a sequence.
//!
//! One rule is easy to implement plausibly and still get it wrong, which is why it has its own tests:
//! [`SmoothWeighted`] is nginx's smooth weighted round robin, not "draw a random member with
//! probability `weight / total`". The drawn form produces bursts that a small upstream notices, and it
//! is not reproducible, so a test cannot state the sequence it should produce.

use std::{
    net::IpAddr,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicUsize, Ordering},
    },
};

/// What a rule may know about the members it chooses among.
///
/// An accessor trait rather than a pre-computed array so a rule pays only for what it reads: `ip_hash`
/// never asks for a weight and `round_robin` never asks for a name. It is also the seam a second pool
/// reuses — a provider pool implements the same four questions and gets the same four strategies.
pub trait Members {
    /// Returns how many members the pool has, candidates or not.
    fn len(&self) -> usize;

    /// Returns whether the pool has no members at all.
    ///
    /// Part of the contract rather than a default: a pool that reports `len() == 0` must say so here
    /// too, and a caller that asks "is there anything to choose from" without a candidate list needs
    /// an answer that means the same thing.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the weight of the member at `index`.
    fn weight(&self, index: usize) -> u16;

    /// Returns how many requests are using the member at `index` right now.
    fn active(&self, index: usize) -> u32;

    /// Returns the name of the member at `index`, which is unique within its upstream.
    fn name(&self, index: usize) -> &str;
}

/// One selection's input: the candidates, and the facts a rule may key on.
///
/// The candidates are member indices in document order, already filtered to the members the pool
/// judged available. A rule answers with one of *those* indices or `None`; it never names a member
/// outside the set it was handed, which is what keeps a health verdict from being overridden by a
/// strategy that happens to prefer a member that was just ejected.
#[derive(Clone, Copy)]
pub struct View<'a> {
    members: &'a dyn Members,
    candidates: &'a [usize],
    client: Option<IpAddr>,
}

impl<'a> View<'a> {
    /// Builds the input of one selection.
    pub fn new(members: &'a dyn Members, candidates: &'a [usize], client: Option<IpAddr>) -> Self {
        Self {
            members,
            candidates,
            client,
        }
    }

    /// Returns the candidates a rule may choose from, in document order.
    pub fn candidates(&self) -> &'a [usize] {
        self.candidates
    }

    /// Returns the weight of the member at `index`.
    pub fn weight(&self, index: usize) -> u16 {
        self.members.weight(index)
    }

    /// Returns how many requests are using the member at `index` right now.
    pub fn active(&self, index: usize) -> u32 {
        self.members.active(index)
    }

    /// Returns the name of the member at `index`.
    pub fn name(&self, index: usize) -> &'a str {
        self.members.name(index)
    }

    /// Returns how many members the pool has, candidates or not.
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Returns whether the pool has no members at all.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Returns the caller's address, when the transport reported one.
    pub fn client(&self) -> Option<IpAddr> {
        self.client
    }
}

impl std::fmt::Debug for View<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("View")
            .field("candidates", &self.candidates)
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

/// One way of choosing a member among the available ones.
///
/// `Send + Sync` because one generation's state is shared by every worker thread through its
/// [`Arc`]; the trait takes `&self` so a selection never needs the pool to be mutable. Interior state,
/// when a rule has any, is atomic or behind a short lock — nothing inside may allocate, wait or touch
/// the network.
pub trait Picker: Send + Sync + std::fmt::Debug {
    /// Returns the member this request should use, or `None` when there is nothing to choose from.
    fn pick(&self, view: &View<'_>) -> Option<usize>;

    /// Builds the picker the next publication serves from, carrying what it can across the change.
    ///
    /// `previous` and `current` are the member names of the two generations, in document order, and
    /// only the names are passed because a rule never connects to anything — the address and port are
    /// the pool's business. A rule whose state is *about a member* (a smooth weight) remaps it by
    /// identity; a rule whose state is *about the list* (a rotation position) keeps it as it is; a
    /// stateless rule ignores the call. The result is a new instance: the generation in flight keeps
    /// reading the old one, so a remap cannot be observed by a request that already selected.
    fn inherit(&self, previous: &[Arc<str>], current: &[Arc<str>]) -> Arc<dyn Picker>;
}

/// Returns the candidates in document order, one request each.
///
/// The default because it needs no configuration and no key: with one member it is the only possible
/// answer, and with several it spreads requests evenly without keeping state about the caller.
#[derive(Debug, Default)]
pub struct RoundRobin {
    /// The next slot to hand out, wrapped by the candidate count.
    position: AtomicUsize,
}

impl Picker for RoundRobin {
    fn pick(&self, view: &View<'_>) -> Option<usize> {
        let candidates = view.candidates();
        if candidates.is_empty() {
            return None;
        }
        // `fetch_add` rather than a read-then-write: two requests selecting at once must not receive
        // the same slot. The ordering is relaxed because the counter orders nothing but itself.
        let slot = self.position.fetch_add(1, Ordering::Relaxed);
        candidates.get(slot % candidates.len()).copied()
    }

    fn inherit(&self, _previous: &[Arc<str>], _current: &[Arc<str>]) -> Arc<dyn Picker> {
        // A position is a place in a list, not a fact about a machine: a document that only added a
        // member must not restart the rotation at the first one.
        Arc::new(Self {
            position: AtomicUsize::new(self.position.load(Ordering::Relaxed)),
        })
    }
}

/// Returns the candidates in proportion to their weights, interleaved rather than in bursts.
///
/// This is nginx's smooth weighted round robin: every step adds each candidate's weight to its running
/// weight, picks the largest one, then takes the candidates' total back off the winner. A `3:1` pair
/// therefore interleaves instead of arriving as three requests and then one.
///
/// The total is the sum of the *candidates'* weights, not of every member's: a member that is not
/// available must not consume its share, or the remaining members would under-serve by exactly the
/// weight of the excluded one.
#[derive(Debug, Default)]
pub struct SmoothWeighted {
    /// Each member's running weight, by member index.
    ///
    /// A short critical section rather than one atomic per member: the step reads every candidate's
    /// weight and writes one, and a per-member atomic version of that is hard to prove correct for
    /// three or more members. Nothing inside the lock allocates or awaits.
    current: Mutex<Vec<i64>>,
}

impl Picker for SmoothWeighted {
    fn pick(&self, view: &View<'_>) -> Option<usize> {
        let candidates = view.candidates();
        if candidates.is_empty() {
            return None;
        }
        let mut current = self.lock();
        if current.len() < view.len() {
            current.resize(view.len(), 0);
        }
        let mut total = 0i64;
        for &index in candidates {
            if let Some(slot) = current.get_mut(index) {
                *slot += i64::from(view.weight(index));
                total += i64::from(view.weight(index));
            }
        }
        let mut best: Option<(usize, i64)> = None;
        for &index in candidates {
            let Some(&value) = current.get(index) else {
                continue;
            };
            if best.is_none_or(|(_, best_value)| value > best_value) {
                best = Some((index, value));
            }
        }
        let (chosen, _) = best?;
        if let Some(slot) = current.get_mut(chosen) {
            *slot -= total;
        }
        Some(chosen)
    }

    fn inherit(&self, previous: &[Arc<str>], current: &[Arc<str>]) -> Arc<dyn Picker> {
        // The running weights are indexed by member position, so they are remapped onto the new list
        // by identity: a member that kept its name keeps what it accumulated, a new member starts at
        // zero and a member that left is dropped. Carrying them by index instead would hand one
        // machine's accumulated weight to whichever machine took its slot.
        let snapshot = self.lock().clone();
        let mut carried = vec![0i64; current.len()];
        for (index, name) in current.iter().enumerate() {
            let kept = previous
                .iter()
                .position(|previous_name| previous_name == name);
            if let Some(value) = kept.and_then(|old| snapshot.get(old).copied()) {
                carried[index] = value;
            }
        }
        Arc::new(Self {
            current: Mutex::new(carried),
        })
    }
}

impl SmoothWeighted {
    /// Takes the running-weight lock, recovering a poisoned one.
    ///
    /// Nothing inside the lock can panic, so a poisoned guard still describes a consistent vector, and
    /// recovering keeps one panic elsewhere from making the strategy unusable for the process.
    fn lock(&self) -> MutexGuard<'_, Vec<i64>> {
        match self.current.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Returns the candidate with the fewest requests in flight, breaking ties in turn.
///
/// The count is the data plane's own: Pingora's connection pool is process-wide and its connections
/// belong to no request, so it cannot answer "how many requests are using this member right now".
#[derive(Debug, Default)]
pub struct LeastConn {
    /// The tie-break position, advanced on every call.
    ///
    /// Advanced even when there is no tie: a burst that finds every count equal would otherwise break
    /// the same way for the whole burst, which is the rotation this field exists to prevent.
    tie: AtomicUsize,
}

impl Picker for LeastConn {
    fn pick(&self, view: &View<'_>) -> Option<usize> {
        let candidates = view.candidates();
        if candidates.is_empty() {
            return None;
        }
        let minimum = candidates.iter().map(|&index| view.active(index)).min()?;
        let tied: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|&index| view.active(index) == minimum)
            .collect();
        let slot = self.tie.fetch_add(1, Ordering::Relaxed);
        tied.get(slot % tied.len()).copied()
    }

    fn inherit(&self, _previous: &[Arc<str>], _current: &[Arc<str>]) -> Arc<dyn Picker> {
        // Like the rotation, the tie-break is a position in a list rather than a fact about a machine.
        Arc::new(Self {
            tie: AtomicUsize::new(self.tie.load(Ordering::Relaxed)),
        })
    }
}

#[cfg(test)]
mod tests;
