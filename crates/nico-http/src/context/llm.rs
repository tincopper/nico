//! What one request's LLM front-end decided, and the transform installed for its response.
//!
//! The fields are the request's own facts, so they live with the request rather than with the
//! published front-end: the route is a pointer, but the stream flag and the model were decoded from
//! *this* caller's body, and the transform belongs to *this* response. A shared value could not carry
//! either without letting two concurrent requests observe each other.
//!
//! Nothing here is printed: a prompt, an endpoint and a response body have no `Debug` spelling. What a
//! log line may report is the mode, the two protocol names, the stream flag and the model label.

use std::{
    fmt,
    sync::{Arc, Mutex, PoisonError},
};

use super::HttpContext;
use crate::llm::{
    AttemptTarget, ConvertingRoute, LlmAttempt, LlmPrepared, LlmRoute, ResponseTransform,
};

/// The attempt a round started, as the round that started it recorded it.
///
/// When the attempt fails, the walk has already moved on to its next choice, so the failure's log
/// line needs the facts from here — the target's name, the attempt's number and the round's clock —
/// and not from a walk that no longer holds them.
pub(crate) struct AttemptInFlight {
    pub(crate) target: String,
    pub(crate) number: u32,
    pub(crate) started: std::time::Instant,
}

/// The LLM facts of one request, plus the per-response transform it installed.
pub(crate) struct LlmExchange {
    route: Arc<dyn LlmRoute>,
    /// This request's walk over the route's targets; `None` for a passthrough route, which has no
    /// canonical value and therefore no attempts to make.
    ///
    /// Behind its own lock for the same reason the transform is: the walk is `Send` but not `Sync`,
    /// and the framework requires the request context to be both. The lock is never contended — one
    /// hook drives the walk, reached through `&mut HttpContext` — so the accessor uses `get_mut`.
    attempt: Option<Mutex<Box<dyn LlmAttempt>>>,
    streaming: Option<bool>,
    model: Option<String>,
    /// A per-response translator, which is `Send` but not `Sync`: the framework requires the request
    /// context to be both, so the value sits behind its own lock. The lock is never contended — one
    /// hook drives the transform, and it is reached through `&mut HttpContext` — which is why the
    /// accessor uses `get_mut` and never takes it.
    transform: Option<Mutex<Box<dyn ResponseTransform>>>,
    /// The head facts of the attempt in flight, set by the selection.
    target: Option<AttemptTarget>,
    /// The attempt a round started, for the failure line a replaced attempt reports.
    in_flight: Option<AttemptInFlight>,
    /// Whether the client's body was decoded and encoded already, so a replay round must not
    /// accumulate it a second time.
    body_prepared: bool,
}

impl LlmExchange {
    /// Starts the exchange for the front-end the route published.
    ///
    /// The stream flag and the model stay empty until [`LlmExchange::record_prepared`]: they are facts
    /// of the client's body, and a passthrough route never decodes one.
    pub(crate) fn new(route: Arc<dyn LlmRoute>) -> Self {
        // The walk starts here, when the route starts serving the request: it is the per-request
        // half of a converting route, and a passthrough route has no answer for "decode this".
        let attempt = route
            .converting()
            .map(|converting| Mutex::new(converting.begin()));
        Self {
            route,
            attempt,
            streaming: None,
            model: None,
            transform: None,
            target: None,
            in_flight: None,
            body_prepared: false,
        }
    }

    /// Returns this request's walk over the route's targets, for mutation.
    ///
    /// The walk has no read-only half worth exposing: every question the data plane asks it — select,
    /// prepare, classify, record — changes it. `get_mut` reaches it without taking the lock, because
    /// the caller holds the request context exclusively.
    pub(crate) fn attempt_mut(&mut self) -> Option<&mut (dyn LlmAttempt + 'static)> {
        let slot = self.attempt.as_mut()?;
        let attempt = slot.get_mut().unwrap_or_else(PoisonError::into_inner);
        Some(&mut **attempt)
    }

    /// Returns the head facts of the attempt the walk is on.
    ///
    /// Recorded when the walk selected it, so the hooks that write the upstream head read one value
    /// instead of re-asking the walk.
    pub(crate) fn target(&self) -> Option<&AttemptTarget> {
        self.target.as_ref()
    }

    /// Returns whether the walk already selected the target of the attempt in flight.
    pub(crate) fn has_target(&self) -> bool {
        self.target.is_some()
    }

    /// Records that a round started serving the attempt its selection chose.
    ///
    /// Called once per round, when the walk's head facts are recorded: the value it overwrites
    /// belonged to the attempt before, whose fate — if it failed — was already reported.
    pub(crate) fn note_attempt_started(&mut self, target: &str, number: u32) {
        self.in_flight = Some(AttemptInFlight {
            target: target.to_owned(),
            number,
            started: std::time::Instant::now(),
        });
    }

    /// Takes the record of the attempt in flight, for the failure line that reports it.
    pub(crate) fn take_attempt_in_flight(&mut self) -> Option<AttemptInFlight> {
        self.in_flight.take()
    }

    /// Returns the published front-end serving this request.
    pub(crate) fn route(&self) -> &dyn LlmRoute {
        &*self.route
    }

    /// Returns the exchange this route converts with, or `None` when it forwards bytes through.
    pub(crate) fn converting(&self) -> Option<&dyn ConvertingRoute> {
        self.route.converting()
    }

    /// Returns the client protocol the published front-end serves.
    ///
    /// A publication fact, not a fact of this request: the value is recorded in the request log and
    /// never matched on by this crate.
    pub(crate) fn protocol(&self) -> &str {
        self.route.protocol()
    }

    /// Returns the provider kind the published front-end speaks to.
    ///
    /// Reported alongside [`Self::protocol`] so one request record names both sides of the exchange.
    pub(crate) fn provider(&self) -> &str {
        self.route.provider()
    }

    /// Returns whether the client asked for a streamed answer, once its body has been decoded.
    ///
    /// `None` also means the response head arrived before the client finished sending — a provider can
    /// refuse a credential from the request head alone — so no answer of this exchange is a stream.
    pub(crate) fn streaming(&self) -> Option<bool> {
        self.streaming
    }

    /// Returns the model label the front-end reported, once the body has been decoded.
    pub(crate) fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Records what the front-end decoded from the client's body.
    pub(crate) fn record_prepared(&mut self, prepared: &LlmPrepared) {
        self.streaming = Some(prepared.streaming());
        self.model = prepared.model().map(str::to_owned);
    }

    /// Replaces the target the walk is on, keeping its model label in the record.
    pub(crate) fn advance_target(&mut self, target: AttemptTarget) {
        if let Some(model) = target.model() {
            self.model = Some(model.to_owned());
        }
        self.target = Some(target);
    }

    /// Records that the client's body was decoded and encoded for an attempt.
    pub(crate) fn mark_body_prepared(&mut self) {
        self.body_prepared = true;
    }

    /// Returns whether the client's body was already decoded and encoded.
    ///
    /// A retry round replays the request body through the same hook, and the walk's canonical value
    /// is what the next attempt encodes; accumulating the replay again would decode it twice.
    pub(crate) fn body_already_prepared(&self) -> bool {
        self.body_prepared
    }

    /// Stores the transform installed for this response.
    pub(crate) fn install(&mut self, transform: Box<dyn ResponseTransform>) {
        self.transform = Some(Mutex::new(transform));
    }

    /// Returns the response transform, once the response head installed one.
    pub(crate) fn transform_mut(&mut self) -> Option<&mut (dyn ResponseTransform + 'static)> {
        let slot = self.transform.as_mut()?;
        // Exclusive access, so the lock is not taken; a poison left by an earlier panic still yields
        // the value, which is what the caller needs to finish or refuse the body.
        let transform = slot.get_mut().unwrap_or_else(PoisonError::into_inner);
        Some(&mut **transform)
    }

    /// Returns whether a transform was installed, which is what tells a converted response from one
    /// that is forwarded as it arrived.
    pub(crate) fn has_transform(&self) -> bool {
        self.transform.is_some()
    }
}

impl fmt::Debug for LlmExchange {
    /// Prints the published facts and never a body, a header value or a credential.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LlmExchange")
            .field("mode", &self.route.mode_name())
            .field("protocol", &self.protocol())
            .field("provider", &self.provider())
            .field("streaming", &self.streaming)
            .field("model", &self.model())
            .field("transform", &self.has_transform())
            .finish()
    }
}

impl HttpContext {
    /// Records the LLM front-end this route was published with.
    ///
    /// Called once, after routing, for a route that targets a provider. A provider route without one
    /// is refused before this would be called, so an [`LlmExchange`] exists exactly when the route has
    /// a published front-end.
    pub(crate) fn set_llm(&mut self, route: Arc<dyn LlmRoute>) {
        self.llm = Some(LlmExchange::new(route));
    }

    /// Returns the LLM facts of this request, when its route targets a provider.
    pub(crate) fn llm(&self) -> Option<&LlmExchange> {
        self.llm.as_ref()
    }

    /// Returns the LLM facts of this request for mutation, when its route targets a provider.
    pub(crate) fn llm_mut(&mut self) -> Option<&mut LlmExchange> {
        self.llm.as_mut()
    }

    /// Returns the exchange this request's route converts with, or `None` when it does not convert.
    ///
    /// `None` covers two cases that behave the same way here: the route is not an LLM route, or it is
    /// one that forwards bytes through. Both leave the body and the response to whatever else owns
    /// them, and a converting route is the only one with an answer for "decode this".
    pub(crate) fn converting(&self) -> Option<&dyn ConvertingRoute> {
        self.llm.as_ref()?.converting()
    }
}
