//! The data-plane side of [`PluginContext`]: one live request, one phase, one plugin.
//!
//! [`ContextState`] is what a phase driver hands to a plugin. It is deliberately not public: it
//! carries [`HttpContext`] — the data plane's per-request state, which a plugin must only see through
//! the contract — and the live header structures, neither of which is part of the loading seam. The
//! trait is the contract; this type is one implementation of it, and the WASM host will be another.
//!
//! Two rules are enforced here rather than trusted to plugins:
//!
//! - **A phase only offers its own facts.** Reading the request body during `on_request`, or a status
//!   before a response exists, returns [`PluginError::PhaseViolation`]: not a panic, and not a silent
//!   no-op that would let a plugin believe a rewrite happened.
//! - **The caller of a header write validates it.** A header map stores a name as opaque bytes, so an
//!   invalid name would reach the request line; the checks live in [`super::header`] and every write
//!   applies them before touching the request.
//!
//! The client's address is not exposed, only [`PluginContext::client_ip_hash`]: a rate limit needs a
//! stable bucket key, and a WASM guest has no business holding a caller's address.

mod caller;
mod contract;
mod phase;

use std::sync::Arc;

// Re-exported so the phase tests assert the same hash the context reports, without a second copy of
// the caller-address rule.
pub(crate) use caller::hash_address;
pub(crate) use phase::Phase;
use pingora::http::{RequestHeader, ResponseHeader};

use super::PluginError;
use crate::context::HttpContext;

/// The plugin context for one stage of one request.
///
/// Built by the phase driver, used for exactly one plugin call, then dropped: it holds a mutable
/// borrow of the request and the per-request state, so the driver cannot touch either until the
/// plugin returned.
pub(crate) struct ContextState<'a> {
    phase: Phase,
    http: &'a mut HttpContext,
    // A response stage is driven by a hook that has no request header to hand over, so the request
    // side is optional: `method`/`uri` then answer from the context, and every mutator that only a
    // request stage offers is refused by `phase` before the `None` could matter.
    request: Option<&'a mut RequestHeader>,
    response: Option<&'a mut ResponseHeader>,
    // The response head as it was written downstream, for the stages that run after it was sent.
    sent: Option<&'a ResponseHeader>,
    body: Vec<u8>,
    body_complete: bool,
    plugin: Arc<str>,
    config: Arc<str>,
    // Only the tests need to count a plugin's own lines; production attributes them by name through
    // the `plugin` field of the `tracing` event itself.
    #[cfg(test)]
    log_lines: usize,
}

impl<'a> ContextState<'a> {
    /// Builds the context of one stage.
    ///
    /// `plugin` is the instance name — the same string the document wrote, which the refusal, the
    /// log line and the rate-limit bucket are all keyed by. `config` is the JSON text the factory
    /// validated at publication; it is never re-parsed on the request path.
    pub(crate) fn new(
        phase: Phase,
        http: &'a mut HttpContext,
        request: &'a mut RequestHeader,
        plugin: Arc<str>,
        config: Arc<str>,
    ) -> Self {
        Self {
            phase,
            http,
            request: Some(request),
            response: None,
            sent: None,
            body: Vec::new(),
            body_complete: false,
            plugin,
            config,
            #[cfg(test)]
            log_lines: 0,
        }
    }

    /// Builds the context of a response stage, whose caller has no request header to attach.
    ///
    /// The request is then absent rather than invented: a response stage may not read request
    /// headers anyway, and answering it with a fabricated one would make `method` and `uri` describe
    /// a request nobody sent.
    pub(crate) fn for_response(
        phase: Phase,
        http: &'a mut HttpContext,
        response: &'a mut ResponseHeader,
        plugin: Arc<str>,
        config: Arc<str>,
    ) -> Self {
        Self {
            phase,
            http,
            request: None,
            response: Some(response),
            sent: None,
            body: Vec::new(),
            body_complete: false,
            plugin,
            config,
            #[cfg(test)]
            log_lines: 0,
        }
    }

    /// Builds the context of a stage that runs against a response head which has already been sent.
    ///
    /// The stages after `on_response` describe a head the client is already reading: the status and the
    /// headers are facts, and a write to them would change nothing on the wire. The context therefore
    /// holds the written header immutably, so a `set_response_header` there is a
    /// [`PluginError::PhaseViolation`] — a plugin bug worth reporting — rather than an edit that
    /// silently disappears.
    pub(crate) fn sent(
        phase: Phase,
        http: &'a mut HttpContext,
        response: &'a ResponseHeader,
        plugin: Arc<str>,
        config: Arc<str>,
    ) -> Self {
        Self {
            phase,
            http,
            request: None,
            response: None,
            sent: Some(response),
            body: Vec::new(),
            body_complete: false,
            plugin,
            config,
            #[cfg(test)]
            log_lines: 0,
        }
    }

    /// Builds the context of a response-side stage with no response attached.
    ///
    /// A request that never got a response still reaches `on_logging`; every response read then
    /// answers [`PluginError::PhaseViolation`] instead of inventing a status.
    pub(crate) fn detached(
        phase: Phase,
        http: &'a mut HttpContext,
        plugin: Arc<str>,
        config: Arc<str>,
    ) -> Self {
        Self {
            phase,
            http,
            request: None,
            response: None,
            sent: None,
            body: Vec::new(),
            body_complete: false,
            plugin,
            config,
            #[cfg(test)]
            log_lines: 0,
        }
    }

    /// Attaches the response the stage runs against.
    ///
    /// Used by the phase tests and by the fixtures: the two constructors above cover the stages the
    /// driver runs, and this builder lets a test attach a response to any of them.
    #[cfg(test)]
    pub(crate) fn with_response(mut self, response: &'a mut ResponseHeader) -> Self {
        self.response = Some(response);
        self
    }

    /// Attaches the body chunk the stage runs against and whether the stream ended with it.
    pub(crate) fn with_body(mut self, body: Vec<u8>, body_complete: bool) -> Self {
        self.body = body;
        self.body_complete = body_complete;
        self
    }

    /// Returns how many lines the plugin logged through this context, for tests.
    ///
    /// Production attributes a line by the `plugin` field of its own `tracing` event, so a counter
    /// would be a second source of truth nobody reads: it exists only where a test cannot install a
    /// subscriber to observe the events themselves.
    #[cfg(test)]
    pub(crate) fn log_lines(&self) -> usize {
        self.log_lines
    }

    /// Takes the body the stage ran against, as the plugin left it.
    ///
    /// The driver calls this after the stage returns: `set_request_body` and `set_response_body`
    /// write here, and what the plugin left is what travels on.
    pub(crate) fn take_body(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.body)
    }

    /// Requires the stage to be a request stage, and returns its request header.
    fn as_request(&self) -> Result<&RequestHeader, PluginError> {
        if !self.phase.offers_request() {
            return Err(PluginError::PhaseViolation(
                "request headers are only available in on_request and on_request_body",
            ));
        }
        self.request.as_deref().ok_or(PluginError::PhaseViolation(
            "no request is attached to this stage",
        ))
    }

    /// Requires the stage to be `on_request`, and returns its request header mutably.
    ///
    /// Not `on_request_body`: the upstream head is already sent by then, so refusing the write is
    /// honest where accepting it would be a silent no-op.
    fn as_request_mut(&mut self) -> Result<&mut RequestHeader, PluginError> {
        if self.phase != Phase::Request {
            return Err(PluginError::PhaseViolation(
                "request headers are only writable in on_request: the upstream head is already sent",
            ));
        }
        self.request
            .as_deref_mut()
            .ok_or(PluginError::PhaseViolation(
                "no request is attached to this stage",
            ))
    }

    /// Requires the stage to be able to write the response head, and returns it.
    ///
    /// `on_response` is the only stage that can: it runs before anything is written downstream. The two
    /// stages after it describe a head the client already has, so a write there is a contract
    /// violation rather than an edit with no effect.
    fn as_response(&mut self) -> Result<&mut ResponseHeader, PluginError> {
        if self.sent.is_some() {
            return Err(PluginError::PhaseViolation(
                "the response head has already been sent; it can only be changed in on_response",
            ));
        }
        if !self.phase.offers_response() {
            return Err(PluginError::PhaseViolation(
                "response headers are only available in on_response, on_response_body and on_logging",
            ));
        }
        let Some(response) = self.response.as_mut() else {
            return Err(PluginError::PhaseViolation(
                "no response is attached to this stage",
            ));
        };
        Ok(response)
    }

    /// Requires the stage to have a response, and returns its head.
    fn response_ref(&self) -> Result<&ResponseHeader, PluginError> {
        if !self.phase.offers_response() {
            return Err(PluginError::PhaseViolation(
                "the response status is only available in on_response, on_response_body and on_logging",
            ));
        }
        // Exactly one of the two is set: a live head before it is written, or the written one after.
        match (self.response.as_deref(), self.sent) {
            (Some(response), _) => Ok(response),
            (None, Some(response)) => Ok(response),
            (None, None) => Err(PluginError::PhaseViolation(
                "no response is attached to this stage",
            )),
        }
    }
}

#[cfg(test)]
mod tests;
