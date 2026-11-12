//! The member a request selected: which member of which upstream, and what the request reported.
//!
//! Selection happens once, in [`destination`](crate::proxy::forward), and the two facts it produces
//! travel together: the member list this generation published (so a retry inside one request cannot
//! select from another generation) and the one member the request chose (so the `least_conn` count it
//! holds survives exactly as long as the request does).
//!
//! Keeping this out of `context.rs` is not cosmetic. The context module answers "what does the data
//! plane remember about one request"; this one answers "which machine served it and how did that go",
//! and only these three methods may write the answer. Everything else — metrics, the ejection log, the
//! record's `upstream.endpoint` — reads it through them.

use std::time::Instant;

use super::HttpContext;
use crate::upstream::pools::{ConnectionGuard, MemberFailure, MemberOutcome, SelectionTable};

impl HttpContext {
    /// Records the members this request may select from.
    ///
    /// Called once, from the request hook that read the publication, so the selection below uses the
    /// same generation's document the request was routed with. The value is an `Arc` into the
    /// published state, not a copy of it.
    pub(crate) fn set_selection(&mut self, selection: std::sync::Arc<SelectionTable>) {
        self.selection = Some(selection);
    }

    /// Returns the members this request may select from, once a generation is recorded.
    pub(crate) fn selection(&self) -> Option<&SelectionTable> {
        self.selection.as_deref()
    }

    /// Holds one more in-flight count against the member this request uses.
    ///
    /// Only the destination resolver calls this, and only when it selected a member. The guard is
    /// released when the context is dropped, which is what makes every exit path — a response, a
    /// refusal, a cancellation — decrement exactly once.
    pub(crate) fn hold_connection(&mut self, guard: ConnectionGuard) {
        self.connection = Some(guard);
    }

    /// Returns the selected member's name, when its upstream published more than one.
    ///
    /// The request record's `upstream.endpoint` field: absent for a single-member upstream, an LLM
    /// route, a plugin-chosen destination or a request refused before a member was selected. `None`
    /// and an empty name are different facts, so an absent field is what a deployment that never
    /// configured members keeps.
    pub(crate) fn upstream_endpoint(&self) -> Option<&str> {
        self.connection.as_ref().and_then(ConnectionGuard::endpoint)
    }

    /// Reports that the selected member answered the request with anything but a 5xx.
    ///
    /// A no-op when no member was selected — an LLM route, a destination a filter recorded, a request
    /// refused before selection — which is why both hooks can call it unconditionally instead of
    /// asking whether this route selects members.
    pub(crate) fn member_answered(&self, now: Instant) {
        self.report_member(MemberOutcome::Answered, now);
    }

    /// Reports that the selected member failed the request.
    pub(crate) fn member_failed(&self, failure: MemberFailure, now: Instant) {
        self.report_member(MemberOutcome::Failed(failure), now);
    }

    /// Hands one outcome to the guard, which owns the state machine and its observability.
    fn report_member(&self, outcome: MemberOutcome, now: Instant) {
        if let Some(guard) = self.connection.as_ref() {
            guard.record(outcome, now);
        }
    }
}
