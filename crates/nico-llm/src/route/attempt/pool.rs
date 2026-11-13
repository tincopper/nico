//! The pooled half of a request's walk: which member an attempt goes to, and what an exclusion means.
//!
//! A pool changes *order* and *retirement*; it changes nothing about what happens after a failure. That
//! split is why this file exists beside `attempt.rs`: budgets, breakers and the fallback chain keep the
//! rules they had, while everything that is only true of a route with several providers lives here.
//!
//! Two properties are load-bearing and stated once, here:
//!
//! - **The pool is a prefix of the chain.** Members `0..pool.len()` are the pool and the rest is the
//!   fallback chain, so every rule that reads the member list keeps its meaning without knowing pools
//!   exist.
//! - **A request never tries the same pooled member twice.** The strategy orders the members this
//!   request has not used, so the walk cannot draw one it already gave up on — which is also what makes
//!   a pool of one terminate instead of retrying itself until its budget runs out.

use std::time::Duration;

use tokio::time::Instant;

use super::AttemptSequence;
use crate::route::exclusion::{Entry as ExclusionEntry, Reason as ExclusionReason};

impl AttemptSequence {
    /// Retires the current member for `cooldown` without reporting it to the breaker.
    ///
    /// The two tables stay apart on purpose (design §6): a 429 says "this member's window is spent",
    /// which is a routing decision for the next few seconds, while the breaker counts "this target
    /// cannot serve at all". Reporting one as the other would let a rate limit open a breaker for the
    /// cooldown a real outage earns.
    pub(super) fn exclude_current(
        &mut self,
        reason: ExclusionReason,
        cooldown: Duration,
        now: Instant,
    ) {
        let name = self.chain.members()[self.current]
            .identity()
            .provider()
            .to_owned();
        tracing::warn!(
            llm.pool = %self.route_id(),
            llm.excluded = reason.as_str(),
            llm.target = %name,
            cooldown_ms = cooldown.as_millis(),
            "provider member excluded from distribution"
        );
        metrics::counter!(
            "gateway_llm_pool_excluded_total",
            "route_id" => self.route_id().to_owned(),
            "provider" => name.clone(),
            "reason" => reason.as_str(),
        )
        .increment(1);
        if let Some(exclusions) = self.exclusions.as_ref() {
            let entry = ExclusionEntry::new(self.route_id(), name.as_str());
            let until = exclusions.exclude(entry, reason, now);
            tracing::info!(
                llm.pool = %self.route_id(),
                llm.target = %name,
                llm.excluded = reason.as_str(),
                until_ms = until.saturating_duration_since(now).as_millis(),
                "provider member retired across requests"
            );
        }
        self.note_tried(self.current);
        self.excluded.push((name, reason));
    }

    /// Returns the chain indices to try, starting from `first`.
    ///
    /// A route without a pool answers the list in document order — the behaviour every route had
    /// before pools existed. A pooled route answers its pool in the strategy's order for this request,
    /// then the fallback chain in document order.
    ///
    /// `first` applies to the fallback part alone. The pool's position comes from `tried`, not from a
    /// chain index: a pool member is "next" because the request has not used it, not because its
    /// index is larger than the one just left. Applying `first` to the pool would skip the members
    /// after the current index and end the walk after the first failure.
    pub(super) fn walk_order(&mut self, first: usize, now: Instant) -> (Vec<usize>, bool) {
        let Some(pool) = self.chain.pool() else {
            return ((first..self.chain.members().len()).collect(), false);
        };
        let pooled = pool.len();
        // Model filtering is a property of the request, not of a walk step: each pooled member
        // resolves the logical name through its OWN `model_map` and checks its OWN allowlist before
        // the strategy sees the member. A member that would answer `403 model_not_allowed` is not a
        // candidate, so the strategy cannot waste an attempt on it.
        let servable: Vec<usize> = (0..pooled)
            .filter(|&index| self.serves_requested_model(index))
            .collect();
        let model_empty = servable.is_empty();
        let mut order = Vec::with_capacity(self.chain.members().len());
        // The pool's order is drawn once per request, so a random strategy does not produce a
        // different first choice every time the walk restarts mid-request. The draw covers every
        // model-servable pooled member; what the walk may still try is decided per call from `tried`,
        // so a member the request already gave up on is skipped rather than redrawn.
        // The order is drawn once per request so a random strategy cannot produce a different first
        // choice mid-request — but the candidate SET it was drawn from can change: the first round
        // selects before the client's body arrives (no model to filter on), so a later round must
        // redraw if the filter has since become meaningful. `frozen_for_model` records which of the
        // two worlds the draw belongs to.
        let filter_is_active = self.requested_model().is_some();
        if self.pool_order.is_none() || self.pool_order_model != Some(filter_is_active) {
            let names: Vec<&str> = servable
                .iter()
                .map(|&index| self.pool_name(index))
                .collect();
            self.pool_order = Some(pool.order(&servable, &names));
            self.pool_order_model = Some(filter_is_active);
        }
        if let Some(ordered) = self.pool_order.as_ref() {
            order.extend(
                ordered
                    .iter()
                    .copied()
                    .filter(|&index| !self.tried.contains(&index))
                    .filter(|&index| self.admits_now(index, now)),
            );
        }
        // The fallback chain keeps document order, and `first` still advances it: a fallback member
        // that was left is not asked again by a later round. A route that serves no member for this
        // model does NOT reach here — `select_from` answers before appending the chain.
        if !model_empty {
            order.extend(pooled.max(first)..self.chain.members().len());
        }
        (order, model_empty)
    }

    /// Reports whether one pooled member can serve the model this request asked for.
    ///
    /// `None` as the requested model means the body has not been decoded yet; the filter is a no-op
    /// then, which keeps the pre-body selection behaviour exactly as it was.
    fn serves_requested_model(&self, index: usize) -> bool {
        let Some(logical) = self.requested_model() else {
            return true;
        };
        let route = self.chain.members()[index].exchange().route();
        let physical = route.model(logical).physical().clone();
        route.allows(&physical)
    }

    /// Returns the name a pooled member is keyed by.
    pub(super) fn pool_name(&self, index: usize) -> &str {
        // INVARIANT: the caller only asks for indices below the pool's length, which `with_pool`
        // bounded by the member list.
        self.chain.members()[index].identity().provider()
    }

    /// Returns whether any member may still be walked after `current`.
    ///
    /// A pooled route answers "yes" while an untried pooled member exists, whatever their chain
    /// indexes are; after the pool it answers by index like every route did before pools existed.
    pub(super) fn has_member_after(&self, current: usize) -> bool {
        let Some(pool) = self.chain.pool() else {
            return current + 1 < self.chain.members().len();
        };
        let untried_pooled = (0..pool.len()).any(|index| !self.tried.contains(&index));
        untried_pooled || pool.len() < self.chain.members().len()
    }

    /// Reports whether the process-level table lets a pooled member take traffic at `now`.
    ///
    /// A member the *previous* request retired is not drawn by this one: that is the whole point of
    /// keeping the table across requests. A sequence without a table (a unit test) answers "yes", which
    /// is the behaviour before the table existed.
    fn admits_now(&self, index: usize, now: Instant) -> bool {
        let Some(exclusions) = self.exclusions.as_ref() else {
            return true;
        };
        let entry = ExclusionEntry::new(self.route_id(), self.pool_name(index));
        exclusions.admits(&entry, now)
    }

    /// Records that this request attempted `index`, so a later walk does not draw it again.
    pub(super) fn note_tried(&mut self, index: usize) {
        if !self.chain.pool().is_some_and(|pool| index < pool.len()) {
            return;
        }
        if !self.tried.contains(&index) {
            self.tried.push(index);
            // The one emission point of the selection counter, on the same rule the traffic layer
            // uses: the labels are the document's own names, so the cardinality is the configuration's
            // and not the traffic's.
            metrics::counter!(
                "gateway_llm_pool_selected_total",
                "route_id" => self.route_id().to_owned(),
                "provider" => self.chain.members()[index].identity().provider().to_owned(),
            )
            .increment(1);
        }
    }

    /// Returns the route this sequence serves, as the document names it.
    pub(super) fn route_id(&self) -> &str {
        self.chain.primary().exchange().route().route_id()
    }
}
