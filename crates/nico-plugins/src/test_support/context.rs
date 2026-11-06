//! A [`PluginContext`] a unit test can drive.
//!
//! The production implementation belongs to the data plane and is reached through that crate's own
//! private types, so a plugin's tests use this double instead. It implements the whole contract — the
//! method set is closed, so a partial double would not compile — and the parts a built-in kind uses
//! behave like the real implementation: a header lookup is case-insensitive, the first identity wins,
//! and the client address is a hash the test sets rather than an address.
//!
//! Everything a built-in kind does *not* call answers [`PluginError::PhaseViolation`], which is what
//! the real context answers for a stage that does not offer the method. A double that silently
//! succeeded would let a plugin call something its phase does not have and still pass here.

use std::sync::Arc;

use nico_http::{LogLevel, PluginContext, PluginError};

/// One stage of one test request, as a plugin sees it.
pub(crate) struct TestContext {
    plugin: Arc<str>,
    config: Arc<str>,
    route: Option<String>,
    headers: Vec<(String, String)>,
    identity: Option<String>,
    ip_hash: Option<u64>,
    log_lines: usize,
}

impl TestContext {
    /// Builds a context for the plugin instance `plugin`, with no route, header or identity.
    pub(crate) fn new(plugin: &str) -> Self {
        Self {
            plugin: Arc::from(plugin),
            config: Arc::from("{}"),
            route: None,
            headers: Vec::new(),
            identity: None,
            ip_hash: None,
            log_lines: 0,
        }
    }

    /// Returns a copy reporting `route` as the matched route.
    ///
    /// The route id is part of a rate-limit key, so a fixture that skipped it could not exercise the
    /// key at all.
    pub(crate) fn for_route(mut self, route: &str) -> Self {
        self.route = Some(route.to_owned());
        self
    }

    /// Returns a copy carrying one more request header.
    pub(crate) fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Returns a copy already authenticated as `subject` by this plugin.
    pub(crate) fn with_identity(mut self, subject: &str) -> Self {
        self.identity = Some(subject.to_owned());
        self
    }

    /// Returns a copy whose caller hashes to `hash`.
    ///
    /// A test never holds an address: the contract exposes only the hash, so the fixture supplies one
    /// and the cases that must differ differ by a value the test chose.
    pub(crate) fn with_client_ip_hash(mut self, hash: u64) -> Self {
        self.ip_hash = Some(hash);
        self
    }

    /// Returns the first value of `name`, matched without regard to case, as a header map does.
    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(found, _value)| found.eq_ignore_ascii_case(name))
            .map(|(_name, value)| value.clone())
    }
}

impl PluginContext for TestContext {
    fn plugin_name(&self) -> &str {
        &self.plugin
    }

    fn trace_id(&self) -> &str {
        "fixture-trace"
    }

    fn method(&self) -> String {
        "GET".to_owned()
    }

    fn uri(&self) -> &[u8] {
        b"/api/items"
    }

    fn set_uri(&mut self, _uri: &[u8]) -> Result<(), PluginError> {
        Err(PluginError::PhaseViolation(
            "the test double does not support rewriting the request target",
        ))
    }

    fn is_sse(&self) -> bool {
        false
    }

    fn is_websocket(&self) -> bool {
        false
    }

    fn request_header(&self, name: &str) -> Result<Option<String>, PluginError> {
        Ok(self.header(name))
    }

    fn set_request_header(&mut self, name: &str, value: &str) -> Result<(), PluginError> {
        self.headers
            .retain(|(found, _value)| !found.eq_ignore_ascii_case(name));
        self.headers.push((name.to_owned(), value.to_owned()));
        Ok(())
    }

    fn append_request_header(&mut self, name: &str, value: &str) -> Result<(), PluginError> {
        self.headers.push((name.to_owned(), value.to_owned()));
        Ok(())
    }

    fn remove_request_header(&mut self, name: &str) -> Result<(), PluginError> {
        self.headers
            .retain(|(found, _value)| !found.eq_ignore_ascii_case(name));
        Ok(())
    }

    fn status(&self) -> Result<u16, PluginError> {
        Err(PluginError::PhaseViolation("this stage has no response"))
    }

    fn response_header(&self, _name: &str) -> Result<Option<String>, PluginError> {
        Err(PluginError::PhaseViolation(
            "this stage has no response headers",
        ))
    }

    fn set_response_header(&mut self, _name: &str, _value: &str) -> Result<(), PluginError> {
        Err(PluginError::PhaseViolation(
            "this stage has no response headers",
        ))
    }

    fn append_response_header(&mut self, _name: &str, _value: &str) -> Result<(), PluginError> {
        Err(PluginError::PhaseViolation(
            "this stage has no response headers",
        ))
    }

    fn remove_response_header(&mut self, _name: &str) -> Result<(), PluginError> {
        Err(PluginError::PhaseViolation(
            "this stage has no response headers",
        ))
    }

    fn request_body(&self) -> Result<Vec<u8>, PluginError> {
        Err(PluginError::PhaseViolation(
            "this stage has no request body",
        ))
    }

    fn set_request_body(&mut self, _body: Vec<u8>) -> Result<(), PluginError> {
        Err(PluginError::PhaseViolation(
            "this stage has no request body",
        ))
    }

    fn response_body(&self) -> Result<Vec<u8>, PluginError> {
        Err(PluginError::PhaseViolation(
            "this stage has no response body",
        ))
    }

    fn set_response_body(&mut self, _body: Vec<u8>) -> Result<(), PluginError> {
        Err(PluginError::PhaseViolation(
            "this stage has no response body",
        ))
    }

    fn body_complete(&self) -> Result<bool, PluginError> {
        Err(PluginError::PhaseViolation("this stage has no body"))
    }

    fn route_id(&self) -> Option<&str> {
        self.route.as_deref()
    }

    fn target(&self) -> Option<&str> {
        None
    }

    fn config(&self) -> &str {
        &self.config
    }

    fn set_identity(&mut self, subject: &str) -> Result<(), PluginError> {
        // First identity wins, exactly as the data plane does it: a later plugin may not replace the
        // subject an earlier one already established.
        if self.identity.is_none() {
            self.identity = Some(subject.to_owned());
        }
        Ok(())
    }

    fn identity(&self) -> Option<&str> {
        self.identity.as_deref()
    }

    fn client_ip_hash(&self) -> Option<u64> {
        self.ip_hash
    }

    fn log(&mut self, _level: LogLevel, _message: &str) {
        self.log_lines = self.log_lines.saturating_add(1);
    }
}
