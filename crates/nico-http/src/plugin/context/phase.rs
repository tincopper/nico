//! Which of the five stages an implementation of [`PluginContext`] is serving.
//!
//! One value decides everything the context allows: not a per-plugin flag, not a per-configuration
//! decision. That is what makes "you may not read the body in `on_request`" a property of the type
//! rather than a rule each method has to remember — and it is why the driver builds one context per
//! stage instead of reusing one.
//!
//! [`PluginContext`]: crate::PluginContext

/// The stage a plugin is running in.
///
/// The set is exactly the trait's five stages, so "which methods are allowed" is a function of this
/// value and nothing else — no per-plugin flags, no per-configuration decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// `on_request`: request headers, request target, no body.
    Request,
    /// `on_request_body`: one chunk of the request body, plus request headers.
    RequestBody,
    /// `on_response`: the response status and headers, before anything is written.
    Response,
    /// `on_response_body`: one chunk of the response body.
    ResponseBody,
    /// `on_logging`: the request is over; everything is read-only.
    Logging,
}

impl Phase {
    /// Returns whether request headers and the request target are available.
    pub(super) const fn offers_request(self) -> bool {
        matches!(self, Self::Request | Self::RequestBody)
    }

    /// Returns whether this stage may establish facts the rest of the request reads.
    ///
    /// Identity is an admission fact: the rate limit keys on it and the request record attributes
    /// authentication to the plugin that wrote it. A response or logging stage running after the head
    /// was sent can only corrupt that attribution, so those stages are read-only for identity. The
    /// phase owns the rule, not each context method.
    pub(super) const fn offers_identity(self) -> bool {
        self.offers_request()
    }

    /// Returns whether response facts are available.
    pub(super) const fn offers_response(self) -> bool {
        matches!(self, Self::Response | Self::ResponseBody | Self::Logging)
    }

    /// Returns whether a body chunk is available.
    pub(super) const fn offers_body(self) -> bool {
        matches!(self, Self::RequestBody | Self::ResponseBody)
    }
}
