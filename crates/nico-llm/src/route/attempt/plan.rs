//! What one attempt produces: its verdict, the refusal a request gets instead, and the bytes to send.
//!
//! These are the values a walk hands back, kept in their own file so `attempt.rs` holds the walk
//! itself. None of them is mutable or holds a lock: an `AttemptPlan` is written once per attempt and
//! read by the data plane, and both `Verdict` and `SelectionRefused` are `Copy`/tiny answers.

use crate::{
    provider::WireRequest,
    schema::{CanonicalError, ModelRef},
};

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
    /// Builds the plan one encoded attempt produces.
    pub(super) const fn new(
        member: usize,
        wire: WireRequest,
        streaming: bool,
        model: ModelRef,
    ) -> Self {
        Self {
            member,
            wire,
            streaming,
            model,
        }
    }

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
