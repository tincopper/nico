//! One request's attempt sequence over its chain: what the data plane sends, and when it stops.
//!
//! The data plane drives the retry loop (see the Pingora research: it owns the session mechanics and
//! the "no client byte has been committed" gate); this module owns every **decision** inside it —
//! which target is next, whether the failure is final, and what the client is answered with. It is
//! the one place that knows both the chain and the breaker table, so the order of a fallback, the
//! budget of one target and the skip of an open breaker cannot disagree with each other.
//!
//! # The order (design §3)
//!
//! 1. **Budget first.** A target absorbs up to the route's `retry_budget` attempts; a retryable
//!    failure within budget re-encodes the same canonical request for the same target. Retrying the
//!    target that just answered is the cheapest next step — network jitter heals, and no new
//!    endpoint's uncertainty is introduced.
//! 2. **Then the breaker.** When a target's budget is exhausted by failures — or its failure was not
//!    retryable at all — the sequence leaves it and the breaker records one failure. The count is
//!    per *target outcome*, not per attempt: a chain must not amplify one request's failures into
//!    several breaker failures, which is the same isolation the design calls "熔断统计按目标隔离".
//! 3. **Then the next member.** The next chain member is asked the same way — and an `open` breaker
//!    means no attempt plan is produced for it at all: the target is skipped without a connection,
//!    and the sequence moves on.
//! 4. **The end.** A chain that served nothing (every member skipped) answers with the last
//!    canonical error the breaker recorded — no response exists to translate, so the answer comes
//!    from the state the failures left behind. A chain that ran out (the last member's failures
//!    exhausted) answers with that member's own response, which the data plane translates as today.
//!
//! The canonical request is decoded **once** ([`AttemptSequence::decode`]), before the first attempt
//! is encoded; every attempt re-encodes it for its own target. Nothing here touches the client's
//! bytes again. The walk is split from the encode because the data plane must know where attempt 0
//! is *sent* before the client's body has finished arriving — target selection needs only the
//! breaker and the clock, while the encode needs the decoded body.

use std::sync::Arc;

use tokio::time::Instant;

use crate::{
    ExchangeError,
    breaker::{Admission, AttemptOutcome, BreakerTable},
    filter::required,
    protocol::RequestFacts,
    provider::{Endpoint, WireRequest},
    route::{AttemptChain, TargetIdentity, disposition::Exclusion},
    schema::{CanonicalError, ChatRequest, ModelRef},
};

mod failure;
mod pool;

/// What follows one attempt's response head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Verdict {
    /// The current response is the client's answer: a final (non-retryable) failure the data plane
    /// translates exactly as it does today.
    Answer,
    /// The chain ran out: the current response — the last target's failure — is the client's answer,
    /// translated exactly as [`Verdict::Answer`].
    Exhausted,
    /// Another attempt is prepared: the sequence now holds its plan, and the data plane sends it.
    Retry,
}

/// Why a request's chain could serve nothing.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("every target of the chain was skipped")]
pub struct SelectionRefused(pub(crate) Box<CanonicalError>);

impl SelectionRefused {
    /// Returns the canonical error the failures left behind, which the client is answered with.
    #[must_use]
    pub fn error(&self) -> &CanonicalError {
        &self.0
    }
}

/// One attempt's byte-level facts: where it goes and what it carries.
///
/// Everything here is what the data plane needs to send one request and nothing more — no breaker,
/// no chain, no canonical value. The endpoint is the member's own, so a fallback's destination,
/// path and headers are the ones its provider entry compiled.
#[derive(Debug)]
pub struct AttemptPlan {
    /// The member this attempt targets, as the chain orders them.
    member: usize,
    wire: WireRequest,
    streaming: bool,
    model: ModelRef,
}

impl AttemptPlan {
    /// Returns the chain member this attempt targets.
    #[must_use]
    pub const fn member(&self) -> usize {
        self.member
    }

    /// Returns the endpoint the attempt is sent to.
    #[must_use]
    pub const fn wire(&self) -> &WireRequest {
        &self.wire
    }

    /// Consumes the plan into the wire request to send.
    #[must_use]
    pub fn into_wire(self) -> WireRequest {
        self.wire
    }

    /// Returns whether the client asked for a stream.
    #[must_use]
    pub const fn streaming(&self) -> bool {
        self.streaming
    }

    /// Returns the logical and physical model names, for the request record.
    #[must_use]
    pub const fn model(&self) -> &ModelRef {
        &self.model
    }
}

/// One request's walk over its chain.
///
/// Built once per request, when the data plane starts serving a converting route; the data plane
/// holds it for the request's lifetime and asks it at every decision point. All of its answers are
/// synchronous — the clock travels in, never a lock across an `.await`.
pub struct AttemptSequence {
    chain: Arc<AttemptChain>,
    breakers: Arc<BreakerTable>,
    /// The canonical request, decoded once from the client's body; `None` until it has arrived.
    canonical: Option<ChatRequest>,
    /// Attempts one target may absorb, from the route's `retry_budget`.
    budget: u8,
    /// The member in flight, as the chain orders it.
    current: usize,
    /// Attempts the current member has left, including the one in flight.
    remaining: u8,
    /// The plan of the current attempt, once the canonical was encoded for it.
    plan: Option<AttemptPlan>,
    /// The targets the most recent walk skipped, in walk order, for the data plane's log.
    skipped: Vec<String>,
    /// How many attempts have been encoded, so the attempt log can number them.
    attempts: u32,
    /// The pooled members this request has already tried, in attempt order.
    ///
    /// Only a pooled route fills this. It is what keeps one request from drawing the same member twice:
    /// the order is asked for the members *not* in this set, so a strategy ordering a shrinking list
    /// cannot hand back a member the request already gave up on.
    tried: Vec<usize>,
    /// Whether the response head's verdict is already this attempt's outcome.
    ///
    /// A head that was classified is the attempt's answer. A later "this round failed" signal — the
    /// data plane reports one for every non-2xx response it did not keep — must not classify the same
    /// attempt a second time as a *transport* failure: a `400` would become retryable, and one request
    /// would be sent to every provider. Reset when a new attempt is available.
    classified: bool,
    /// The members this request retired, and why, in attempt order.
    ///
    /// A member excluded for a rate limit or a credential problem must not be drawn again by this
    /// request, so it is recorded here *and* pushed onto `tried`: the set the order is drawn from is
    /// what enforces it, and this record is what the request log reports.
    excluded: Vec<(String, Exclusion)>,
    /// The pooled members in the order this request draws them.
    ///
    /// Built once, before the first attempt: the order is a property of the request's draw, not of each
    /// step, so asking the strategy again after every failure would let a random draw produce a
    /// different first choice for the same request. The members the request has already tried are
    /// filtered out of it on every walk, which is what keeps one request from repeating a member.
    pool_order: Option<Vec<usize>>,
}

impl AttemptSequence {
    /// Builds the sequence over its chain, before the client's body has arrived.
    #[must_use]
    pub fn new(chain: Arc<AttemptChain>, breakers: Arc<BreakerTable>, budget: u8) -> Self {
        Self {
            chain,
            breakers,
            canonical: None,
            budget,
            current: 0,
            remaining: budget,
            plan: None,
            skipped: Vec::new(),
            attempts: 0,
            tried: Vec::new(),
            excluded: Vec::new(),
            classified: false,
            pool_order: None,
        }
    }

    /// Decodes the client's body once into the canonical request, and gates it against the chain's
    /// capability intersection.
    ///
    /// The gate runs here, not per target: a request the chain cannot serve is refused before any
    /// attempt is encoded, however many members it has.
    ///
    /// # Errors
    /// Returns the client codec's refusal when the body is not a document of the client protocol,
    /// [`ExchangeError::InvalidCanonical`] when the decoded value breaks a canonical invariant, and
    /// [`ExchangeError::Unsupported`] when the request needs a capability the chain does not carry.
    pub fn decode(&mut self, facts: &RequestFacts, body: &[u8]) -> Result<(), ExchangeError> {
        let member = self.chain.primary();
        let canonical = member.exchange().decode_request(facts, body)?;
        let needed = required(&canonical);
        let capabilities = self.chain.capabilities();
        if let Some(capability) = needed
            .iter()
            .find(|capability| !capabilities.supports(*capability))
        {
            return Err(ExchangeError::Unsupported { capability });
        }
        self.canonical = Some(canonical);
        Ok(())
    }

    /// Selects the first target whose breaker admits, starting at the chain's head.
    ///
    /// Called before the client's body arrives: target selection needs only the breaker and the
    /// clock, so the data plane learns where attempt 0 is sent while the body is still streaming.
    ///
    /// # Errors
    /// Returns [`SelectionRefused`] when every member was skipped: the canonical error to answer the
    /// client with is the one the failures left behind (the primary target's history, because the
    /// route is *for* it).
    pub fn select_first(&mut self, now: Instant) -> Result<(), SelectionRefused> {
        self.select_from(0, now)
    }

    /// Encodes the canonical request for the current target, producing the attempt the data plane
    /// sends.
    ///
    /// Called once the body is decoded — for attempt 0, and again after every [`Verdict::Retry`],
    /// because a target's own model map and endpoint may rename and re-frame the request.
    ///
    /// # Errors
    /// Returns the current member's encoding errors: a model its allowlist or pinned endpoint
    /// refuses, or an adapter that cannot encode the canonical value.
    pub fn encode_current(&mut self) -> Result<(), ExchangeError> {
        let Some(canonical) = self.canonical.clone() else {
            // Deferred: the client's body has not been decoded yet — the walk moved to another
            // target because this attempt failed before any bytes arrived (a connect that never
            // established, a read that broke during the upload). There is nothing to encode, and
            // the round that replays the body will decode it and call here again.
            self.plan = None;
            return Ok(());
        };
        let member = &self.chain.members()[self.current];
        let prepared = member.exchange().encode_request(canonical)?;
        let streaming = prepared.request().stream();
        let model = prepared.model().clone();
        self.attempts += 1;
        self.classified = false;
        self.plan = Some(AttemptPlan {
            member: self.current,
            wire: prepared.into_wire(),
            streaming,
            model,
        });
        Ok(())
    }

    /// Records that the current attempt's response head was a success.
    ///
    /// The breaker learns the target answered — a half-open probe closes its breaker, a closed
    /// breaker's failure counter resets.
    pub fn succeeded(&self, now: Instant) {
        let member = &self.chain.members()[self.current];
        self.breakers
            .record(member.identity(), AttemptOutcome::Success, None, now);
    }

    /// Returns whether the client's body has been decoded into the canonical request.
    #[must_use]
    pub const fn is_decoded(&self) -> bool {
        self.canonical.is_some()
    }

    /// Returns the attempts one target may absorb.
    #[must_use]
    pub const fn budget(&self) -> u8 {
        self.budget
    }

    /// Returns the 1-based number of the attempt in flight, for the attempt log.
    ///
    /// The count behind it is the number of encodes, and an attempt's encode may run before or
    /// after the round that sends it learns its number: the first round selects while the body is
    /// still streaming and encodes later, a retry round encodes first and selects after. The
    /// in-flight attempt is therefore one past the finished encodes when its own encode has not
    /// run yet, and the newest encode once it has.
    #[must_use]
    pub const fn attempt_number(&self) -> u32 {
        if self.plan.is_some() {
            self.attempts
        } else {
            self.attempts + 1
        }
    }

    /// Returns the model label of the current plan, when the canonical was encoded for it.
    #[must_use]
    pub fn plan_model_label(&self) -> Option<String> {
        self.plan.as_ref().map(|plan| {
            let model = plan.model();
            format!("{} -> {}", model.logical(), model.physical())
        })
    }

    /// Returns the plan of the current attempt, once the canonical was encoded for it.
    #[must_use]
    pub fn plan(&self) -> Option<&AttemptPlan> {
        self.plan.as_ref()
    }

    /// Returns the encoded body of the current attempt.
    ///
    /// The plan is re-encoded for every attempt, so its bytes are the ones this round writes and
    /// reading them does not consume them: a round that fails before its body is dispatched asks
    /// again, and finds the same encoding.
    #[must_use]
    pub fn plan_body(&self) -> Option<&[u8]> {
        self.plan.as_ref().map(|plan| plan.wire.body())
    }

    /// Returns the member index the sequence is on.
    #[must_use]
    pub const fn current(&self) -> usize {
        self.current
    }

    /// Returns the identity of the target the current attempt addresses.
    #[must_use]
    pub fn current_identity(&self) -> &TargetIdentity {
        self.chain.members()[self.current].identity()
    }

    /// Returns the endpoint of the current target, whose destination, path and headers the upstream
    /// head needs — available before the canonical was encoded for it.
    #[must_use]
    pub fn current_endpoint(&self) -> &Endpoint {
        self.chain.members()[self.current].exchange().endpoint()
    }

    /// Returns whether the client asked for a streamed answer, once its body was decoded.
    #[must_use]
    pub fn streaming(&self) -> Option<bool> {
        self.canonical.as_ref().map(ChatRequest::stream)
    }

    /// Returns the canonical error the walk's history holds, for a request that ran out of attempts.
    ///
    /// The primary target's, because the route is *for* it: its failures are the ones an operator
    /// compares the route's health against, and an open breaker always has at least one.
    #[must_use]
    pub fn last_refusal(&self) -> Option<CanonicalError> {
        Some(self.primary_failure())
    }

    /// Returns the members this request retired and why, in attempt order, and clears the record.
    ///
    /// The data plane logs each one; the sequence only records what its own classification decided.
    pub fn take_excluded(&mut self) -> Vec<(String, Exclusion)> {
        std::mem::take(&mut self.excluded)
    }

    /// Returns the targets the most recent walk skipped, in walk order, and clears the record.
    ///
    /// The data plane logs each one; the sequence only collects what its own walk decided.
    pub fn take_skipped(&mut self) -> Vec<String> {
        std::mem::take(&mut self.skipped)
    }

    /// Records the outcome of leaving the current target.
    fn leave_current(
        &self,
        outcome: AttemptOutcome,
        failure: Option<&CanonicalError>,
        now: Instant,
    ) {
        let member = &self.chain.members()[self.current];
        self.breakers
            .record(member.identity(), outcome, failure, now);
    }

    /// Returns the canonical error the primary target's history holds, or the closed table's
    /// transport row when it has none.
    ///
    /// The primary's, because the route is *for* it: its failures are the ones an operator compares
    /// the route's health against. An open breaker has recorded at least one failure, so the
    /// history is there; the transport row stands in for an open breaker with no history, which
    /// cannot happen.
    fn primary_failure(&self) -> CanonicalError {
        let primary = self.chain.primary();
        self.breakers
            .last_error(primary.identity())
            .unwrap_or_else(|| primary.exchange().error_for_status(502))
    }

    /// Walks the chain from `first`, taking the first member whose breaker admits.
    ///
    /// A pooled route walks its pool in the strategy's order and then the fallback chain in document
    /// order. The two are one list — the pool is its prefix — so the only difference is *which* index
    /// comes next: the strategy's for the pool, `index + 1` after it.
    fn select_from(&mut self, first: usize, now: Instant) -> Result<(), SelectionRefused> {
        self.skipped.clear();
        for index in self.walk_order(first) {
            let member = &self.chain.members()[index];
            match self.breakers.admit(member.identity(), now) {
                Admission::Admit => {
                    self.current = index;
                    self.remaining = self.budget;
                    self.note_tried(index);
                    return Ok(());
                }
                // An open target produces no attempt plan at all: no connection is made and the
                // sequence moves on. The data plane logs the skip from the record this walk keeps.
                Admission::Skip(_) => {
                    self.skipped.push(member.identity().provider().to_owned());
                }
            }
        }
        // Nothing could be served. No response exists to translate, so the answer is the canonical
        // error the failures left behind (see `primary_failure`).
        Err(SelectionRefused(Box::new(self.primary_failure())))
    }

    /// Leaves the current member for the next one that admits.
    ///
    /// A pooled route asks its pool before its fallback chain, and "the next member" is whoever the
    /// order has not used — not the member with the next index. Only the fallback part advances by
    /// index, which is what `select_from` does with `first`.
    fn advance(&mut self, now: Instant) -> Result<Verdict, ExchangeError> {
        if self.has_member_after(self.current) {
            if self.select_from(self.current + 1, now).is_ok() {
                self.encode_current()?;
                return Ok(Verdict::Retry);
            }
            // The rest of the chain was skipped, but the current member *did* answer: its response
            // is the freshest truth about the route, so the client is answered with it. A refusal
            // from the failure history is only for a walk that never served anything at all.
            return Ok(Verdict::Exhausted);
        }
        Ok(Verdict::Exhausted)
    }
}
