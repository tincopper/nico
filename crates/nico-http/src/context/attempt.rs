//! The request's attempt state: what the retry gate reads, and what a finished loop answers with.
//!
//! These methods sit on [`HttpContext`] but are about the converted route's attempt walk: how much of
//! the client's body a round owns, whether another attempt may be sent, which failure an exhausted
//! request answers with, and whether the peer a previous round recorded still describes the target in
//! flight. They live here so the context's own module stays about the request's facts rather than the
//! attempt machinery's.

use super::{BodyBuffer, HttpContext};
use crate::error::HttpError;

/// How far a failed round got, which decides how much of the retry gate applies.
///
/// The set is closed — a round can fail before it dispatched the request body or after — so this is an
/// `enum` rather than a boolean at each call site, and a caller cannot pass the wrong one silently.
///
/// A failure before dispatch needs no replay buffer: the client's bytes never left the data plane, so
/// the next attempt reads them again whatever their size. A failure after dispatch may already have
/// put them on the wire, and then only Pingora's fixed replay buffer can produce them a second time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureStage {
    /// The peer could not be resolved or connected: no request byte reached it.
    BeforeDispatch,
    /// A response head arrived, so the request body may already be on the wire.
    AfterDispatch,
}

impl HttpContext {
    /// Starts one round's view of the client's body.
    ///
    /// Pingora accumulates the raw downstream bytes it reads and offers them to the body filter again
    /// on the next attempt, so a round's body stream starts at zero: the buffer holds this round's
    /// material and the count holds this round's bytes. Rewinding both here is what makes every client
    /// byte accumulate and count exactly once across a retry, instead of the replay being added to the
    /// previous round's totals.
    ///
    /// Called from `upstream_request_filter`, which runs once per round, before that round's head is
    /// written and before any of its body tasks.
    pub(crate) fn begin_attempt_body(&mut self) {
        self.request_buffer = BodyBuffer::default();
        self.body_bytes = 0;
    }

    /// Returns whether the request may send another attempt after a failure at `stage`.
    ///
    /// The data plane's half of the retry decision, in one place so no hook can forget a fact. Three
    /// are its own, and none of them belongs to the walk or to the front-end:
    ///
    /// - the client is still there: a cancelled request has nobody to answer;
    /// - no response byte was committed: a committed response can never be replaced;
    /// - the body can be produced a second time, which for a dispatched body means it still fits the
    ///   framework's replay buffer (`REPLAY_BODY_LIMIT`) — above it the next attempt would be sent
    ///   with no body at all, so the failure is answered instead.
    ///
    /// Whether the *walk* holds another attempt is a different question, asked by the caller: it moves
    /// the walk, and the answer depends on how far this request already got.
    pub(crate) fn may_replace_attempt(&mut self, committed: bool, stage: FailureStage) -> bool {
        if committed || self.cancelled {
            return false;
        }
        let replayable = match stage {
            FailureStage::BeforeDispatch => true,
            FailureStage::AfterDispatch => {
                crate::llm::retry_after_dispatch_allowed(self.body_bytes)
            }
        };
        replayable
            && self
                .llm_mut()
                .and_then(|llm| llm.attempt_mut())
                .is_some_and(|attempt| attempt.can_retry())
    }

    /// Reports whether the attempt in flight carried a first-byte budget of its own.
    ///
    /// It decides what a read timeout means: with a budget the bound was the route's opinion about this
    /// member, so the member failed and another may answer; without one the bound was the request's own
    /// deadline, and replacing the attempt would only spend another member's time on a request that is
    /// out of time anyway.
    pub(crate) fn attempt_has_first_byte_budget(&self) -> bool {
        self.llm()
            .and_then(|llm| llm.target())
            .and_then(|target| target.ttft())
            .is_some()
    }

    /// Replaces an `AttemptRetry` signal with the failure the walk's history holds.
    ///
    /// A retry signal is not a client-visible failure: it exists so the loop takes another round. When
    /// the loop ends instead — the budget is spent, the body cannot be replayed, the response was
    /// already committed — the request still needs an answer, and the canonical error the failures
    /// left behind is it.
    ///
    /// Only a signal is replaced. Any other failure is already an answer — a plugin's refusal, a
    /// deadline, or the data plane's own classification of a failure the walk never saw — and a round
    /// the walk never classified has no recorded failure to install in its place.
    pub(crate) fn replace_attempt_failure_with_last(&mut self) {
        if !matches!(self.error, Some(HttpError::AttemptRetry)) {
            return;
        }
        let Some(rejection) = self
            .llm_mut()
            .and_then(|llm| llm.attempt_mut())
            .and_then(|attempt| attempt.last_refusal())
        else {
            // No history to answer with: the walk never failed a target, which cannot happen for a
            // signal it produced. Leaving the signal is the honest state, and `fail_to_proxy` maps
            // it to its generic failure rather than inventing a status.
            return;
        };
        self.error = Some(rejection);
    }

    /// Clears the stored failure, because the attempt that produced it is being replaced.
    ///
    /// A retried attempt is not the request's outcome: keeping its failure would make `fail_to_proxy`
    /// write the response of an attempt the next round never made.
    pub(crate) fn clear_failure(&mut self) {
        self.error = None;
    }

    /// Marks the request as cancelled: the client disconnected or shutdown began.
    ///
    /// A cancelled request produces no response. Recording it keeps the logging hook from reporting
    /// a disconnect as a server error.
    pub fn mark_cancelled(&mut self) {
        self.cancelled = true;
    }

    /// Returns whether the request was cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// Returns whether the recorded destination belongs to the attempt in flight.
    ///
    /// True for a route with no walk: an upstream route's destination is its own, resolved once, and no
    /// later round can invalidate it. False for every converting route, whose destination is the
    /// attempt's rather than the route's — the walk's recorded target supplies it instead, and a
    /// resolver query is cheaper than a head written to the endpoint the request just failed on.
    pub(crate) fn destination_still_describes_the_attempt(&self) -> bool {
        self.llm.is_none()
    }
}
