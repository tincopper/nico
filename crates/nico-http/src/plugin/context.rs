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

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "step 5b drives the body and logging stages; until then their constructors and the log counter have no caller"
    )
)]

use std::{net::IpAddr, sync::Arc};

use pingora::http::{HMap, RequestHeader, ResponseHeader};

use super::{LogLevel, PluginContext, PluginError, header};
use crate::context::{HttpContext, Identity};

/// Offset basis of FNV-1a, 64 bit.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// Prime of FNV-1a, 64 bit.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

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
    const fn offers_request(self) -> bool {
        matches!(self, Self::Request | Self::RequestBody)
    }

    /// Returns whether response facts are available.
    const fn offers_response(self) -> bool {
        matches!(self, Self::Response | Self::ResponseBody | Self::Logging)
    }

    /// Returns whether a body chunk is available.
    const fn offers_body(self) -> bool {
        matches!(self, Self::RequestBody | Self::ResponseBody)
    }
}

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
    body: Vec<u8>,
    body_complete: bool,
    plugin: Arc<str>,
    config: Arc<str>,
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
            body: Vec::new(),
            body_complete: false,
            plugin,
            config,
            log_lines: 0,
        }
    }

    /// Builds the context of a response stage, whose caller has no request header to attach.
    ///
    /// The request is then absent rather than invented: a response stage may not read request
    /// headers anyway, and answering it with a fabricated one would make `method` and `uri` describe
    /// a request nobody sent.
    pub(crate) fn for_response(
        http: &'a mut HttpContext,
        response: &'a mut ResponseHeader,
        plugin: Arc<str>,
        config: Arc<str>,
    ) -> Self {
        Self {
            phase: Phase::Response,
            http,
            request: None,
            response: Some(response),
            body: Vec::new(),
            body_complete: false,
            plugin,
            config,
            log_lines: 0,
        }
    }

    /// Attaches the response the stage runs against.
    ///
    /// Required by `on_response`, `on_response_body` and `on_logging`; a stage without one answers
    /// [`PluginError::PhaseViolation`] to any response read, which is what a caller that forgot to
    /// attach it deserves.
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

    /// Returns how many lines the plugin logged through this context, for tests and diagnostics.
    pub(crate) fn log_lines(&self) -> usize {
        self.log_lines
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

    /// Requires the stage to be a request stage, and returns its request header mutably.
    fn as_request_mut(&mut self) -> Result<&mut RequestHeader, PluginError> {
        if !self.phase.offers_request() {
            return Err(PluginError::PhaseViolation(
                "request headers are only available in on_request and on_request_body",
            ));
        }
        self.request
            .as_deref_mut()
            .ok_or(PluginError::PhaseViolation(
                "no request is attached to this stage",
            ))
    }

    /// Requires the stage to be a response stage, and returns its response header.
    fn as_response(&mut self) -> Result<&mut ResponseHeader, PluginError> {
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

    /// Requires the stage to be a response stage, and returns its response header immutably.
    fn response_ref(&self) -> Result<&ResponseHeader, PluginError> {
        if !self.phase.offers_response() {
            return Err(PluginError::PhaseViolation(
                "the response status is only available in on_response, on_response_body and on_logging",
            ));
        }
        self.response.as_deref().ok_or(PluginError::PhaseViolation(
            "no response is attached to this stage",
        ))
    }
}

impl PluginContext for ContextState<'_> {
    fn plugin_name(&self) -> &str {
        self.plugin.as_ref()
    }

    fn trace_id(&self) -> &str {
        self.http.trace_id()
    }

    fn method(&self) -> String {
        // Read from the header rather than from `HttpContext`: a global plugin runs before routing,
        // so the context has not `begin`-ed yet, while the method is known from the first byte. A
        // response stage may have no request attached, and the context still knows it by then.
        match self.request.as_deref() {
            Some(request) => request.method.as_str().to_owned(),
            None => self
                .http
                .method()
                .map_or_else(String::new, |method| method.as_str().to_owned()),
        }
    }

    fn uri(&self) -> String {
        match self.request.as_deref() {
            Some(request) => String::from_utf8_lossy(request.raw_path()).into_owned(),
            // Without a request header the context still has the path routing matched; it does not
            // keep the query string, so that is what a response stage gets to see.
            None => self.http.path().unwrap_or_default().to_owned(),
        }
    }

    fn set_uri(&mut self, uri: &str) -> Result<(), PluginError> {
        if self.phase != Phase::Request {
            return Err(PluginError::PhaseViolation(
                "set_uri is only available in on_request",
            ));
        }
        self.as_request_mut()?
            .set_raw_path(uri.as_bytes())
            .map_err(|_| PluginError::Execute("the URI is not a valid request target"))
    }

    fn is_sse(&self) -> bool {
        let value = self
            .response
            .as_ref()
            .and_then(|response| read_header(&response.headers, "content-type"))
            .or_else(|| {
                self.request
                    .as_deref()
                    .and_then(|request| read_header(&request.headers, "accept"))
            });
        value.is_some_and(|value| value.to_ascii_lowercase().contains("text/event-stream"))
    }

    fn is_websocket(&self) -> bool {
        self.request
            .as_deref()
            .and_then(|request| read_header(&request.headers, "upgrade"))
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
    }

    fn request_header(&self, name: &str) -> Result<Option<String>, PluginError> {
        Ok(read_header(&self.as_request()?.headers, name))
    }

    fn set_request_header(&mut self, name: &str, value: &str) -> Result<(), PluginError> {
        let (name, value) = checked(name, value)?;
        self.as_request_mut()?
            .insert_header(name, value)
            .map_err(|_| PluginError::Execute("the request header cannot be stored"))
    }

    fn append_request_header(&mut self, name: &str, value: &str) -> Result<(), PluginError> {
        let (name, value) = checked(name, value)?;
        self.as_request_mut()?
            .append_header(name, value)
            .map(|_appended| ())
            .map_err(|_| PluginError::Execute("the request header cannot be stored"))
    }

    fn remove_request_header(&mut self, name: &str) -> Result<(), PluginError> {
        if !header::is_header_name(name) {
            return Err(PluginError::Execute(
                "a header name must be an RFC 9110 token",
            ));
        }
        self.as_request_mut()?.remove_header(name);
        Ok(())
    }

    fn status(&self) -> Result<u16, PluginError> {
        Ok(self.response_ref()?.status.as_u16())
    }

    fn response_header(&self, name: &str) -> Result<Option<String>, PluginError> {
        Ok(read_header(&self.response_ref()?.headers, name))
    }

    fn set_response_header(&mut self, name: &str, value: &str) -> Result<(), PluginError> {
        let (name, value) = checked(name, value)?;
        self.as_response()?
            .insert_header(name, value)
            .map_err(|_| PluginError::Execute("the response header cannot be stored"))
    }

    fn append_response_header(&mut self, name: &str, value: &str) -> Result<(), PluginError> {
        let (name, value) = checked(name, value)?;
        self.as_response()?
            .append_header(name, value)
            .map(|_appended| ())
            .map_err(|_| PluginError::Execute("the response header cannot be stored"))
    }

    fn remove_response_header(&mut self, name: &str) -> Result<(), PluginError> {
        if !header::is_header_name(name) {
            return Err(PluginError::Execute(
                "a header name must be an RFC 9110 token",
            ));
        }
        self.as_response()?.remove_header(name);
        Ok(())
    }

    fn request_body(&self) -> Result<Vec<u8>, PluginError> {
        if self.phase != Phase::RequestBody {
            return Err(PluginError::PhaseViolation(
                "the request body is only available in on_request_body",
            ));
        }
        Ok(self.body.clone())
    }

    fn set_request_body(&mut self, body: Vec<u8>) -> Result<(), PluginError> {
        if self.phase != Phase::RequestBody {
            return Err(PluginError::PhaseViolation(
                "the request body is only available in on_request_body",
            ));
        }
        self.body = body;
        Ok(())
    }

    fn response_body(&self) -> Result<Vec<u8>, PluginError> {
        if self.phase != Phase::ResponseBody {
            return Err(PluginError::PhaseViolation(
                "the response body is only available in on_response_body",
            ));
        }
        Ok(self.body.clone())
    }

    fn set_response_body(&mut self, body: Vec<u8>) -> Result<(), PluginError> {
        if self.phase != Phase::ResponseBody {
            return Err(PluginError::PhaseViolation(
                "the response body is only available in on_response_body",
            ));
        }
        self.body = body;
        Ok(())
    }

    fn body_complete(&self) -> Result<bool, PluginError> {
        if !self.phase.offers_body() {
            return Err(PluginError::PhaseViolation(
                "body completeness is only available in a body stage",
            ));
        }
        Ok(self.body_complete)
    }

    fn route_id(&self) -> Option<&str> {
        self.http.route_id()
    }

    fn target(&self) -> Option<&str> {
        self.http.upstream()
    }

    fn config(&self) -> &str {
        self.config.as_ref()
    }

    fn set_identity(&mut self, subject: &str) -> Result<(), PluginError> {
        // First identity wins, and a later write is ignored rather than refused: an authentication
        // plugin that authenticated *after* another one cannot replace the subject a rate limit is
        // already charged to.
        self.http
            .set_identity(Identity::new(subject, Arc::clone(&self.plugin)));
        Ok(())
    }

    fn identity(&self) -> Option<&str> {
        self.http.identity().map(Identity::subject)
    }

    fn client_ip_hash(&self) -> Option<u64> {
        self.http
            .client_ip()
            .map(|address| hash_address(address.to_canonical()))
    }

    fn log(&mut self, level: LogLevel, message: &str) {
        self.log_lines = self.log_lines.saturating_add(1);
        match level {
            LogLevel::Debug => tracing::debug!(plugin = %self.plugin, "{message}"),
            LogLevel::Info => tracing::info!(plugin = %self.plugin, "{message}"),
            LogLevel::Warn => tracing::warn!(plugin = %self.plugin, "{message}"),
            LogLevel::Error => tracing::error!(plugin = %self.plugin, "{message}"),
        }
    }
}

/// Reads the first value of one header, or `None` when it is absent or not UTF-8.
///
/// A value that is not valid UTF-8 reads as absent on purpose: the contract's value type is a
/// `String`, and handing a plugin a mangled copy of a credential is worse than telling it there is
/// none. An authentication plugin then answers "missing", which is the closed answer.
fn read_header(headers: &HMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Validates one header name and value, and returns them in the owned form a header map stores.
fn checked(name: &str, value: &str) -> Result<(String, String), PluginError> {
    if !header::is_header_name(name) {
        return Err(PluginError::Execute(
            "a header name must be an RFC 9110 token",
        ));
    }
    if !header::is_header_value(value) {
        return Err(PluginError::Execute(
            "a header value must be visible ASCII, space or tab",
        ));
    }
    Ok((name.to_owned(), value.to_owned()))
}

/// Hashes one canonical address with FNV-1a.
///
/// This is a bucketing function, not a cryptographic hash: a rate-limit key needs to be stable within
/// a generation and cheap to compute, and the value never leaves the process or reaches a guest as an
/// address. IPv4 and its IPv4-mapped IPv6 spelling hash the same, so one caller cannot hold two
/// buckets by changing how it connects.
fn hash_address(address: IpAddr) -> u64 {
    match address {
        IpAddr::V4(address) => fnv1a(&address.octets()),
        IpAddr::V6(address) => fnv1a(&address.octets()),
    }
}

/// FNV-1a over a byte slice.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests;
