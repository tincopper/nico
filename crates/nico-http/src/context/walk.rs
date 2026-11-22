//! One traffic request's walk over its upstream's members: who it may still try, and what it spent.
//!
//! The walk mirrors the llm attempt walk's contract — one verdict per failed round, a bounded
//! budget — with traffic's own vocabulary: no canonical refusals, no model filters, no fallback
//! chain. It holds three facts and answers one question:
//!
//! - which upstream the request's route pool chose, **once per request**: a retry moves to another
//!   member, not another upstream, because the pool's availability filter answered "which upstreams
//!   can serve" when the draw was made;
//! - which members the request already tried, so a round cannot be sent to the member that just
//!   failed;
//! - how many attempts the route's `retry_budget` still allows, including the first.
//!
//! The failure classification is the hooks' (`fail_to_connect` / `error_while_proxy`): only the
//! member-fault failures reach [`TrafficWalk::failed`], and its answer is whether the budget holds
//! another member. An upstream `5xx` is the origin's own answer and is forwarded — it counts against
//! the member's health, never against the walk.

use std::sync::Arc;

use crate::upstream::pools::MemberId;

/// One traffic request's walk over its upstream's members.
#[derive(Debug)]
pub(crate) struct TrafficWalk {
    upstream: Arc<str>,
    tried: Vec<MemberId>,
    /// Attempts made, including the one in flight.
    spent: u8,
    budget: u8,
}

impl TrafficWalk {
    /// Begins the walk over `upstream`'s members, with `budget` as the attempt total.
    ///
    /// Created when the request selects its first member: the first attempt is spent from that
    /// moment, which is what makes the budget "including the first".
    pub(crate) fn new(upstream: Arc<str>, budget: u8) -> Self {
        Self {
            upstream,
            tried: Vec::new(),
            spent: 1,
            budget,
        }
    }

    /// Returns the upstream this request's pool chose, sticky for the walk's lifetime.
    pub(crate) fn upstream(&self) -> &Arc<str> {
        &self.upstream
    }

    /// Returns the members this request already tried, in attempt order.
    pub(crate) fn tried(&self) -> &[MemberId] {
        &self.tried
    }

    /// Records the member this round selected, so no later round may repeat it.
    pub(crate) fn record_tried(&mut self, member: MemberId) {
        self.tried.push(member);
    }

    /// Reports whether the walk holds another attempt, and spends it.
    ///
    /// Called once per failed round: `true` means the loop takes another member, `false` that the
    /// budget is spent and the failure is the request's outcome.
    pub(crate) fn failed(&mut self) -> bool {
        if self.spent >= self.budget {
            return false;
        }
        self.spent += 1;
        true
    }

    /// Reports whether another attempt is held, without spending it.
    pub(crate) fn can_retry(&self) -> bool {
        self.spent < self.budget
    }
}

#[cfg(test)]
mod tests;
