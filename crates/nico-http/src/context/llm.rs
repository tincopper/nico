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
use crate::llm::{ConvertingRoute, LlmPrepared, LlmRoute, ResponseTransform};

/// The LLM facts of one request, plus the per-response transform it installed.
pub(crate) struct LlmExchange {
    route: Arc<dyn LlmRoute>,
    streaming: Option<bool>,
    model: Option<String>,
    /// A per-response translator, which is `Send` but not `Sync`: the framework requires the request
    /// context to be both, so the value sits behind its own lock. The lock is never contended — one
    /// hook drives the transform, and it is reached through `&mut HttpContext` — which is why the
    /// accessor uses `get_mut` and never takes it.
    transform: Option<Mutex<Box<dyn ResponseTransform>>>,
}

impl LlmExchange {
    /// Starts the exchange for the front-end the route published.
    ///
    /// The stream flag and the model stay empty until [`LlmExchange::record_prepared`]: they are facts
    /// of the client's body, and a passthrough route never decodes one.
    pub(crate) fn new(route: Arc<dyn LlmRoute>) -> Self {
        Self {
            route,
            streaming: None,
            model: None,
            transform: None,
        }
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
