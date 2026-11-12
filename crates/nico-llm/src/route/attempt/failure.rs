//! What one attempt's failure costs: the retry budget, the member, or nothing.
//!
//! The classification runs **before** any counter moves, and that order is the whole design. A
//! failure's category decides three things at once — whether it consumes the route's retry budget,
//! whether the member is tried again this request, and whether the breaker hears about it — so a
//! classification made after the fact would let one number answer for three different events.
//!
//! The table itself lives in [`disposition`](crate::route::disposition); this file is where its answer
//! is applied to a walk: which member is left, whether the round is replaced, and what the client is
//! finally told.

use tokio::time::Instant;

use super::AttemptSequence;
use crate::{
    ExchangeError, Verdict,
    breaker::AttemptOutcome,
    route::disposition::{self, Disposition},
};

impl AttemptSequence {
    /// Classifies a non-2xx response head from the current target.
    ///
    /// The provider status becomes the canonical error through the member's own closed table — the
    /// same one its body decoding uses — and the failure is what the breaker remembers.
    ///
    /// # Errors
    /// Returns the encoding error of a member the sequence moved to.
    pub fn classify(&mut self, status: u16, now: Instant) -> Result<Verdict, ExchangeError> {
        let member = &self.chain.members()[self.current];
        let failure = member.exchange().error_for_status(status);
        // From here the head is this attempt's outcome, whatever the round reports afterwards.
        self.classified = true;
        // The category decides what the failure costs *before* any counter moves: a caller's mistake is
        // final, a rate limit or a credential problem retires the member for a while without spending
        // the route's retry budget, and only "the member failed to serve" reaches the budget and the
        // breaker. Classifying after the counters would let one number answer for three events.
        match disposition::classify(status, &failure) {
            Disposition::Final => {
                // The client is refused with the member's own answer and no other target is asked.
                // Leaving the target is still a failure the breaker should count — a target that 401s
                // every request is exactly the one a breaker exists to stop trying first.
                self.leave_current(AttemptOutcome::Failure, Some(&failure), now);
                Ok(Verdict::Answer)
            }
            Disposition::Exclude { reason, cooldown } => {
                self.exclude_current(reason, cooldown, now);
                self.advance(now)
            }
            Disposition::Failed => {
                if self.remaining > 1 {
                    // Budget left: retry the target that just failed. No breaker movement — a jittery
                    // target within its budget has not earned one.
                    self.remaining -= 1;
                    self.encode_current()?;
                    return Ok(Verdict::Retry);
                }
                // The budget is exhausted: leave the target, counting one failure for it, and move on.
                self.leave_current(AttemptOutcome::Failure, Some(&failure), now);
                self.advance(now)
            }
        }
    }

    /// Classifies an attempt that failed without producing a response head — a connection that never
    /// established, a read that broke before the head arrived.
    ///
    /// The failure is the transport row of the current target's own closed table (a `502`, which the
    /// table marks retryable), so the budget and the breaker move exactly as they would for a
    /// provider that answered one.
    ///
    /// # Errors
    /// Returns the encoding error of a member the sequence moved to.
    pub fn transport_failed(&mut self, now: Instant) -> Result<Verdict, ExchangeError> {
        if self.classified {
            // The head was already classified: this round's failure is that verdict, not a transport
            // failure. Answering `Answer` keeps the classification the provider's own table produced —
            // a 400 stays final, a 429 stays an exclusion — instead of turning both into a retry.
            return Ok(Verdict::Answer);
        }
        let member = &self.chain.members()[self.current];
        let failure = member.exchange().error_for_status(502);
        if self.remaining > 1 {
            // Budget left: retry the target that just failed. No breaker movement — a jittery
            // target within its budget has not earned one. A connect that never established is
            // exactly the "network jitter heals" case the budget-first rule exists for.
            self.remaining -= 1;
            self.encode_current()?;
            return Ok(Verdict::Retry);
        }
        // The budget is exhausted: leave the target, counting one failure for it, and move on.
        self.leave_current(AttemptOutcome::Failure, Some(&failure), now);
        self.advance(now)
    }
}
