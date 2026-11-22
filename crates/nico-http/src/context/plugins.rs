//! The route's compiled plugin plan, as one request carries it: the two chains, the framing facts
//! the plan decides, and the plugin instance a failure is attributed to.
//!
//! These methods are [`HttpContext`]'s plugin face. The chains come from the publication — the same
//! plan that decided the framing flags — and the failed instance is set by the only place that knows
//! both the instance name and the error.

use std::sync::Arc;

use super::HttpContext;
use crate::plugin::PluginChain;

impl HttpContext {
    /// Records the two chains this request was admitted by, and how it is framed.
    ///
    /// The response and logging stages read them back instead of resolving the route again, so a
    /// reload that publishes while a request is in flight cannot run a *different* generation's
    /// plugins against a response the request's own plugins shaped. Both chains come from the same
    /// `HttpState`, so they are one generation by construction.
    ///
    /// `body` and `response` are the framing decisions made while that generation was compiled: the
    /// body hook runs per chunk and may read them, but may not re-derive them from configuration.
    /// Only the hooks call this.
    pub(crate) fn set_chains(&mut self, global: PluginChain, route: Option<PluginChain>) {
        self.global = Some(global);
        self.route_chain = route;
    }

    /// Records that this route's request body is delivered to plugins whole.
    pub(crate) fn set_buffered(&mut self, buffered: bool) {
        self.buffered = buffered;
    }

    /// Records that a plugin rewrites this route's response body.
    pub(crate) fn set_response_rewrite(&mut self, rewrites: bool) {
        self.rewrites_response = rewrites;
    }

    /// Returns the chain every request runs, once the request has been admitted.
    pub(crate) fn global(&self) -> Option<&PluginChain> {
        self.global.as_ref()
    }

    /// Returns the chain this request's route runs, when the route has plugins.
    pub(crate) fn route_chain(&self) -> Option<&PluginChain> {
        self.route_chain.as_ref()
    }

    /// Returns whether this route's request body is buffered whole.
    pub(crate) fn buffers_body(&self) -> bool {
        self.buffered
    }

    /// Returns whether a plugin rewrites this route's response body.
    pub(crate) fn rewrites_response(&self) -> bool {
        self.rewrites_response
    }

    /// Appends one received chunk to the buffered request body.
    pub(crate) fn push_body(&mut self, chunk: &[u8]) {
        self.request_buffer.push(chunk);
    }

    /// Takes the buffered request body, leaving the buffer empty.
    pub(crate) fn take_body(&mut self) -> Vec<u8> {
        self.request_buffer.take()
    }

    /// Records the plugin instance whose stage failed, for the request's one log record.
    ///
    /// The chain is the only place that knows both the instance name and the error; the record is the
    /// only place that reports it. The first failure wins, so a later symptom cannot hide the cause.
    pub(crate) fn set_failed_plugin(&mut self, name: &str) {
        if self.failed_plugin.is_none() {
            self.failed_plugin = Some(Arc::from(name));
        }
    }

    /// Returns the plugin instance a failure is attributed to, when one produced it.
    #[must_use]
    pub(crate) fn failed_plugin(&self) -> Option<&str> {
        self.failed_plugin.as_deref()
    }
}
