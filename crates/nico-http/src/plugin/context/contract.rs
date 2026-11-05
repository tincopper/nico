//! The data-plane implementation of the [`PluginContext`] contract.
//!
//! `plugin.rs` owns the contract and `plugin/context.rs` owns the state and its phase guards. This
//! module is the seam between them: every method here either reads a request fact through the phase
//! guard, or refuses with [`PluginError::PhaseViolation`]. Keeping the trait implementation in its
//! own file is what lets both sides stay within the file budget without moving a rule away from its
//! owner.

use std::sync::Arc;

use super::{ContextState, Phase, hash_address};
use crate::{
    context::Identity,
    plugin::{LogLevel, PluginContext, PluginError, header},
};

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
            .and_then(|response| header::first_value(&response.headers, "content-type"))
            .or_else(|| {
                self.request
                    .as_deref()
                    .and_then(|request| header::first_value(&request.headers, "accept"))
            });
        value.is_some_and(|value| value.to_ascii_lowercase().contains("text/event-stream"))
    }

    fn is_websocket(&self) -> bool {
        self.request
            .as_deref()
            .and_then(|request| header::first_value(&request.headers, "upgrade"))
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
    }

    fn request_header(&self, name: &str) -> Result<Option<String>, PluginError> {
        Ok(header::first_value(&self.as_request()?.headers, name))
    }

    fn set_request_header(&mut self, name: &str, value: &str) -> Result<(), PluginError> {
        let (name, value) = header::validated(name, value)?;
        header::reject_framing(name.as_str())?;
        self.as_request_mut()?
            .insert_header(name, value)
            .map_err(|_| PluginError::Execute("the request header cannot be stored"))
    }

    fn append_request_header(&mut self, name: &str, value: &str) -> Result<(), PluginError> {
        let (name, value) = header::validated(name, value)?;
        header::reject_framing(name.as_str())?;
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
        header::reject_framing(name)?;
        self.as_request_mut()?.remove_header(name);
        Ok(())
    }

    fn status(&self) -> Result<u16, PluginError> {
        Ok(self.response_ref()?.status.as_u16())
    }

    fn response_header(&self, name: &str) -> Result<Option<String>, PluginError> {
        Ok(header::first_value(&self.response_ref()?.headers, name))
    }

    fn set_response_header(&mut self, name: &str, value: &str) -> Result<(), PluginError> {
        let (name, value) = header::validated(name, value)?;
        header::reject_framing(name.as_str())?;
        self.as_response()?
            .insert_header(name, value)
            .map_err(|_| PluginError::Execute("the response header cannot be stored"))
    }

    fn append_response_header(&mut self, name: &str, value: &str) -> Result<(), PluginError> {
        let (name, value) = header::validated(name, value)?;
        header::reject_framing(name.as_str())?;
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
        header::reject_framing(name)?;
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
        if !self.phase.offers_identity() {
            return Err(PluginError::PhaseViolation(
                "identity is an admission fact; it cannot be set after the response is written",
            ));
        }
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
        #[cfg(test)]
        {
            self.log_lines = self.log_lines.saturating_add(1);
        }
        match level {
            LogLevel::Debug => tracing::debug!(plugin = %self.plugin, "{message}"),
            // The logging spec reserves `info!` for lifecycle milestones. A plugin's `Info` on the
            // request path is emitted at `debug!` by the data plane, which owns the level: at gateway
            // volume one `info!` per request would flood the logging pipeline.
            LogLevel::Info => tracing::debug!(plugin = %self.plugin, "{message}"),
            LogLevel::Warn => tracing::warn!(plugin = %self.plugin, "{message}"),
            LogLevel::Error => tracing::error!(plugin = %self.plugin, "{message}"),
        }
    }
}
