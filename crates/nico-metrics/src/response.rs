//! One self-reporting answer, and the routing from a request line to it.
//!
//! The path constants are the surface's contract: the transport layer names them only through this
//! module, so a path cannot be spelled twice. [`Response::allow`] is the other half of that
//! contract — the legal method set is a fact about the surface, so the surface declares it and the
//! transport writes exactly what it is told.

use crate::{Metrics, health::Readiness};

/// The liveness endpoint, answered without reading shared state.
pub const LIVE_PATH: &str = "/health/live";

/// The readiness endpoint, answered from the caller's servability verdict.
pub const READY_PATH: &str = "/health/ready";

/// The Prometheus text endpoint.
pub const METRICS_PATH: &str = "/metrics";

impl Metrics {
    /// Answers one self-reporting request.
    ///
    /// `method` and `path` are the request line's own text. A known path answers `405` with
    /// [`Response::allow`] when the method is not `GET`; an unknown path answers `404`. Neither
    /// reads shared state, so a probe cannot be refused because a configuration failure happened.
    #[must_use]
    pub fn respond(&self, method: &str, path: &str, readiness: Readiness<'_>) -> Response {
        let get = method == "GET";
        match path {
            LIVE_PATH if get => readiness.live(),
            READY_PATH if get => readiness.ready(),
            METRICS_PATH if get => Response::metrics(self.render(readiness.status())),
            LIVE_PATH | READY_PATH | METRICS_PATH => Response::method_not_allowed(),
            _ => Response::not_found(),
        }
    }
}

/// One self-reporting answer: status, content type, optional `Allow` and the body text.
///
/// The fields are private and read through accessors, so a caller states what it was told instead
/// of rebuilding a response. The body is owned because the metrics endpoint renders a `String`.
pub struct Response {
    status: u16,
    content_type: &'static str,
    allow: Option<&'static str>,
    body: String,
}

impl Response {
    /// Builds a response with no `Allow` header.
    pub(crate) fn text(status: u16, content_type: &'static str, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type,
            allow: None,
            body: body.into(),
        }
    }

    /// Builds a Prometheus text response.
    pub(crate) fn metrics(body: String) -> Self {
        Self::text(200, "text/plain; version=0.0.4", body)
    }

    /// Builds the `405` answer, whose `Allow` header declares the one method this surface serves.
    pub(crate) fn method_not_allowed() -> Self {
        Self {
            status: 405,
            content_type: "text/plain",
            allow: Some("GET"),
            body: "method not allowed".to_owned(),
        }
    }

    /// Builds the `404` answer.
    pub(crate) fn not_found() -> Self {
        Self {
            status: 404,
            content_type: "text/plain",
            allow: None,
            body: "not found".to_owned(),
        }
    }

    /// Returns the HTTP status line.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.status
    }

    /// Returns the response's `Content-Type`.
    #[must_use]
    pub fn content_type(&self) -> &'static str {
        self.content_type
    }

    /// Returns the `Allow` header value for a `405`, or `None` for every other status.
    #[must_use]
    pub fn allow(&self) -> Option<&'static str> {
        self.allow
    }

    /// Returns the response body.
    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }
}

#[cfg(test)]
mod tests;
