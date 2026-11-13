//! One request's walk over a converted route's targets, as the data plane drives it.
//!
//! A converting route is published once and shared by every request, so the *walk* — which target is
//! next, whether the last failure ends the request — cannot live on it. This module is the per-request
//! half: [`ConvertingRoute::begin`](super::ConvertingRoute::begin) creates one, the data plane holds
//! it in the request context, and every hook that needs to know "where does this attempt go" or "what
//! does this failure mean" asks it.
//!
//! # What this crate decides, and what it does not
//!
//! The front-end owns every *decision*: which target is next, whether a status is retryable, whether a
//! skipped target leaves the request with an answer. This crate owns the *mechanics*: it selects before
//! the body arrives, encodes once the body ends, and classifies at the response head. The vocabulary
//! here is therefore order and bytes only — `Retry`, `Answer`, a destination, a body — and never a
//! reason. That is what keeps the data plane free of the LLM crate's policy while still driving it.
//!
//! # The one data-plane rule
//!
//! Pingora replays a request body on a retry only from its own buffer, which is capped at
//! [`REPLAY_BODY_LIMIT`] and cannot be configured (see `research/pingora-retry.md` §2.3). A request
//! whose raw body crossed that cap cannot be retried once its body was dispatched: the next attempt
//! would be sent with no body at all. The hooks consult [`retry_after_dispatch_allowed`] before
//! letting a retry through — it is this crate's fact about its own runtime, not a question the
//! front-end could answer.

use std::time::Duration;

use crate::{error::HttpError, upstream::Destination};

/// The largest request body Pingora can replay on a retry.
///
/// `pingora-core`'s `BODY_BUF_LIMIT` (64 KiB): the buffer is truncated above it, and a truncated
/// buffer is not offered to the next attempt at all. Re-deriving the number here rather than
/// importing it keeps the dependency surface unchanged; the e2e suite pins the behavior.
pub const REPLAY_BODY_LIMIT: u64 = 64 * 1024;

/// Returns whether a retry may be attempted after the client's body was dispatched.
///
/// `body_bytes` is what the request received. Above the replay cap the next attempt would be sent
/// with no body, so the failure must be answered instead of retried. A failure *before* the body was
/// dispatched — a connect that never established — does not consult this: the body is still pending.
#[must_use]
pub const fn retry_after_dispatch_allowed(body_bytes: u64) -> bool {
    body_bytes <= REPLAY_BODY_LIMIT
}

/// Where one attempt's request head goes, and what it carries.
///
/// The values are per-attempt, not per-route: a fallback target has its own host, its own path and its
/// own headers, and the head written for it must be that target's — the route's primary values would
/// address the target the request just left.
#[derive(Debug, Clone)]
pub struct AttemptTarget {
    destination: Destination,
    path: String,
    headers: Vec<(String, String)>,
    /// The target's name, for the attempt log. A name, never an address.
    name: String,
    /// The logical → physical model label, for the request record.
    model: Option<String>,
    /// How long a streamed attempt may wait for its first byte, when the route states one.
    ///
    /// Travels with the target rather than with the peer because the peer is built per attempt: a
    /// route that moved to another member builds another peer, and the budget belongs to the attempt.
    ttft: Option<Duration>,
}

impl AttemptTarget {
    /// Builds one attempt's head facts.
    #[must_use]
    pub fn new(
        destination: Destination,
        path: impl Into<String>,
        headers: Vec<(String, String)>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            destination,
            path: path.into(),
            headers,
            name: name.into(),
            model: None,
            ttft: None,
        }
    }

    /// Attaches the route's first-byte budget for a streamed answer.
    #[must_use]
    pub const fn with_ttft(mut self, ttft: Option<Duration>) -> Self {
        self.ttft = ttft;
        self
    }

    /// Returns the first-byte budget, when the route states one.
    #[must_use]
    pub const fn ttft(&self) -> Option<Duration> {
        self.ttft
    }

    /// Attaches the model label the request record reports.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Returns the peer this attempt is sent to.
    #[must_use]
    pub const fn destination(&self) -> &Destination {
        &self.destination
    }

    /// Returns the absolute path this attempt requests.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns the headers this attempt presents, in the order they are written.
    #[must_use]
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// Returns the target's name, for the attempt log.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the model label the request record reports, when the front-end supplied one.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }
}

/// What a response head means for the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AttemptVerdict {
    /// This response is the client's answer: translate it and finish.
    Answer,
    /// The chain ran out: this response — the last target's failure — is the client's answer.
    Exhausted,
    /// Another attempt is ready: the walker now holds its head facts and body.
    Retry,
}

/// One request's walk over its route's targets.
///
/// Created by the front-end at the start of the request and held by the request context. Every method
/// takes the clock, so the implementation never reads one itself and a test can drive time forward
/// without sleeping.
/// The walk is `Send` but not `Sync`: it is this request's own value, and the framework requires the
/// request context to be both, so [`LlmExchange`](crate::context) keeps it behind a lock the way it
/// keeps the response transform.
pub trait LlmAttempt: Send {
    /// Selects the first target whose state admits the request.
    ///
    /// Called before the client's body arrives: target selection needs only the clock and the target's
    /// own state, so the data plane learns where attempt 0 is sent while the body is still streaming.
    ///
    /// # Errors
    /// Returns the refusal that answers the client when no target could be served at all — an
    /// [`HttpError::LlmRejected`] carrying the client protocol's error document, or the generic failure
    /// the front-end chose.
    fn select(&mut self, now: tokio::time::Instant) -> Result<AttemptTarget, HttpError>;

    /// Decodes the buffered client body and encodes the current attempt for its target.
    ///
    /// Called once the client body has ended — for attempt 0 and after every [`AttemptVerdict::Retry`].
    /// The returned value is what the data plane writes upstream.
    ///
    /// # Errors
    /// Returns the failure that replaces the provider request.
    fn prepare(
        &mut self,
        request: &super::LlmRequest<'_>,
        now: tokio::time::Instant,
    ) -> Result<super::LlmPrepared, HttpError>;

    /// Classifies a non-2xx response head from the current target.
    ///
    /// # Errors
    /// Returns the failure that replaces the response, when the front-end cannot prepare the next
    /// attempt it decided on.
    fn classify(
        &mut self,
        status: u16,
        now: tokio::time::Instant,
    ) -> Result<AttemptVerdict, HttpError>;

    /// Classifies an attempt that failed without producing a response head.
    ///
    /// A connection that never established and a read that broke before the head arrived are both
    /// failures of the current target, and both move the walk exactly as a provider's `502` would.
    ///
    /// # Errors
    /// Returns the failure that replaces the response, when the front-end cannot prepare the next
    /// attempt it decided on.
    fn failed(&mut self, now: tokio::time::Instant) -> Result<AttemptVerdict, HttpError>;

    /// Records that the current attempt's response head was a success.
    fn succeeded(&mut self, now: tokio::time::Instant);

    /// Returns whether this route can retry at all, which is false for a route that declared none.
    ///
    /// A route with one target, one attempt and no fallback never retries: the data plane skips the
    /// retry plumbing entirely, which is what makes an undeclared route behave exactly as it did
    /// before attempts existed.
    fn can_retry(&self) -> bool;

    /// Returns the name of the target the current attempt addresses.
    fn current_target(&self) -> &str;

    /// Returns the route's first-byte budget, when it states one.
    fn ttft(&self) -> Option<Duration>;

    /// Returns whether the caller asked for a streamed answer, once its body was decoded.
    ///
    /// `None` before the body arrives — the peer is built before the decode, so a route that has not
    /// read its body yet cannot say. A route that is not converting answers `None` for its whole life.
    fn streaming(&self) -> Option<bool>;

    /// Returns the 1-based number of the attempt in flight, for the attempt log.
    fn attempt_number(&self) -> u32;

    /// Returns the head facts of the member the walk is on, with the model label when that
    /// member's canonical encoding already produced one.
    ///
    /// Valid after [`Self::select`] or after a classification moved the walk; the values are the
    /// member's own endpoint, so a fallback never writes its successor's address.
    fn target_view(&self) -> AttemptTarget;

    /// Returns the body encoded for the attempt in flight.
    ///
    /// The replay round writes these bytes at its end of stream. They are not consumed: a round that
    /// failed before its body was dispatched asks again and finds the same encoding. `None` means
    /// this attempt encoded no body — a failure that arrived before the client's body was decoded,
    /// which the round that replays it will encode instead.
    fn body(&self) -> Option<&[u8]>;

    /// Returns the failure the walk's history holds, rendered in the client's protocol.
    ///
    /// The answer a request needs when the loop ends without a response of its own: the last target
    /// failure the walk recorded, which is the same canonical error an all-skipped selection
    /// answers with.
    fn last_refusal(&self) -> Option<HttpError>;

    /// Returns the targets the most recent selection skipped, in walk order, and clears the record.
    fn take_skipped(&mut self) -> Vec<String>;
}
