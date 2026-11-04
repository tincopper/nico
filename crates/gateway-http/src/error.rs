//! The data plane's failure vocabulary and the only place it becomes a wire response.
//!
//! Every failure on the request path — routing, limits, filters, upstream connection — is expressed
//! as one [`HttpError`]. [`HttpError::respond`] maps each variant to exactly one status code, one
//! stable machine-readable `code` and one caller-safe `message`; nothing else in this crate builds
//! an error response, so a filter cannot invent a status and two call sites cannot disagree.
//!
//! Two properties are deliberate:
//!
//! - **The response never carries internal detail.** The `code` is a snake_case string owned by
//!   this crate and renaming it is a breaking change for callers; the `message` is a fixed
//!   sentence. Upstream names, addresses, credentials and upstream bodies appear in logs through
//!   the error's [`Display`](std::fmt::Display) implementation, never in the response.
//! - **Cancellation is not an error.** A client disconnect, a shutdown and a deadline that expires
//!   after bytes were committed are outcomes, not responses: they produce no body and no 5xx.

use std::{borrow::Cow, error::Error, fmt::Write as _};

use bytes::Bytes;
use gateway_core::ErrorCategory;
use pingora::http::StatusCode;

/// How one failure is rendered, if at all.
///
/// Fields are private because the triple (`status`, `code`, `message`) is a contract: a caller may
/// read it, never assemble a variant of its own. `code` and `message` are compile-time constants, so
/// the only value that needs escaping when the body is built is the request's trace id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Respond {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
}

impl Respond {
    const fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
        }
    }

    /// Returns the HTTP status code to send.
    #[must_use]
    pub const fn status(self) -> StatusCode {
        self.status
    }

    /// Returns the stable snake_case error code callers may match on.
    #[must_use]
    pub const fn code(self) -> &'static str {
        self.code
    }

    /// Returns the caller-safe message, a fixed sentence with no internal detail.
    #[must_use]
    pub const fn message(self) -> &'static str {
        self.message
    }

    /// Renders the error body as `{"error":{"code","message","trace_id"}}`.
    ///
    /// `trace_id` is caller-supplied, so it is JSON-escaped; the code and message are constants and
    /// are written verbatim.
    #[must_use]
    pub fn body(self, trace_id: &str) -> Bytes {
        let mut json = String::with_capacity(96 + trace_id.len());
        json.push_str(r#"{"error":{"code":""#);
        json.push_str(self.code);
        json.push_str(r#"","message":""#);
        json.push_str(self.message);
        json.push_str(r#"","trace_id":""#);
        push_json_string(&mut json, trace_id);
        json.push_str(r#""}}"#);
        Bytes::from(json)
    }
}

/// A request-path failure, classified once and mapped once.
///
/// The set may grow, so matches outside this crate must handle the variants they know plus a
/// wildcard. Every variant is constructible by other crates: that is how an HTTP filter reports
/// *why* it rejected a request without writing a response itself.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HttpError {
    /// No runtime snapshot has been published, or the pipeline is not ready to serve.
    #[error("no runtime snapshot is available to serve")]
    NotReady,
    /// No route in the published document matches the request.
    #[error("no route matches the request")]
    RouteNotFound,
    /// The request is malformed in a way the data plane must reject before routing.
    ///
    /// `reason` borrows when the rule is a fixed sentence and owns only when it quotes a bound, so
    /// a message can never drift from the constant the check applied. It is for logs; the response
    /// body never repeats it.
    #[error("invalid request: {reason}")]
    InvalidRequest {
        /// Why the request is rejected, derived from the rule that rejected it.
        reason: Cow<'static, str>,
    },
    /// The request body exceeds the bytes this route allows.
    #[error("request body exceeds the {limit} byte limit")]
    BodyTooLarge {
        /// The effective limit: the smaller of the route override and the document limit.
        limit: u32,
    },
    /// The whole-request deadline expired before the response was committed.
    ///
    /// Two sources produce it: a peer that ran out of the request's budget (Pingora reports it as a
    /// read/write/connect timeout) and a budget that was already spent before a peer could be
    /// contacted. The status and code stay `504 upstream_timeout` — a proxy that ran out of time
    /// while answering is exactly what that pair means, and a stable code may not be renamed once
    /// callers match on it.
    ///
    /// A deadline that expires while the *caller* is still sending is not this variant: nothing is
    /// known to be wrong with the peer, so it is [`Self::RequestTimeout`].
    #[error("request deadline exceeded after {elapsed_ms} ms")]
    DeadlineExceeded {
        /// Time spent on the request before the deadline was observed.
        elapsed_ms: u64,
    },
    /// The caller did not finish sending the request within its budget.
    ///
    /// Only reachable while the body is still streaming, where the data plane knows the request is
    /// incomplete: the peer is not at fault, the caller is slow. A `504` here would send an operator
    /// to look at a healthy upstream.
    #[error("the request body was still incomplete when the deadline expired")]
    RequestTimeout,
    /// The upstream could not be reached, or failed before a response was committed.
    #[error("upstream request failed")]
    UpstreamFailed {
        /// The underlying cause, kept for logs and never rendered to the caller.
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
    /// A route targets a provider and no filter replaced the target with a real peer.
    ///
    /// This crate does not know provider semantics, so it refuses to guess an address.
    #[error("no provider front-end is configured for {name}")]
    ProviderNotConfigured {
        /// The provider name the route referenced.
        name: String,
    },
    /// The data plane failed in a way it cannot attribute to the caller or to an upstream.
    ///
    /// Pingora reports failures from its own layers (a broken downstream connection that is still
    /// writable, an internal invariant) that do not fit the caller's or the upstream's vocabulary.
    #[error("internal data-plane failure")]
    Internal {
        /// The underlying cause, kept for logs and never rendered to the caller.
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
    /// A response was already committed, so no error response can replace it.
    #[error("response already committed; the failure cannot be rendered as a response")]
    ResponseCommitted,
}

impl HttpError {
    /// Returns how this failure is rendered, or `None` when a response is no longer possible.
    ///
    /// This is the single mapping point: status, stable code and safe message are decided here and
    /// nowhere else.
    #[must_use]
    pub const fn respond(&self) -> Option<Respond> {
        match self {
            Self::NotReady => Some(Respond::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "not_ready",
                "the gateway has no configuration to serve yet",
            )),
            Self::RouteNotFound => Some(Respond::new(
                StatusCode::NOT_FOUND,
                "route_not_found",
                "no route matches the request",
            )),
            Self::InvalidRequest { .. } => Some(Respond::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "the request is not valid",
            )),
            Self::BodyTooLarge { .. } => Some(Respond::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_too_large",
                "the request body is too large",
            )),
            Self::DeadlineExceeded { .. } => Some(Respond::new(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_timeout",
                "the request did not complete in time",
            )),
            Self::RequestTimeout => Some(Respond::new(
                StatusCode::REQUEST_TIMEOUT,
                "request_timeout",
                "the request was not received in time",
            )),
            Self::UpstreamFailed { .. } => Some(Respond::new(
                StatusCode::BAD_GATEWAY,
                "upstream_unreachable",
                "the upstream is unavailable",
            )),
            Self::ProviderNotConfigured { .. } => Some(Respond::new(
                StatusCode::BAD_GATEWAY,
                "provider_unavailable",
                "no provider is configured for this route",
            )),
            Self::Internal { .. } => Some(Respond::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "the gateway failed to process the request",
            )),
            Self::ResponseCommitted => None,
        }
    }

    /// Classifies the failure for the layer that logs it.
    ///
    /// `ResponseCommitted` is [`ErrorCategory::Internal`] because reaching it means the data plane
    /// tried to replace a response it had already sent; a plain cancellation is not an `HttpError`,
    /// so it can never be misreported as a 5xx here.
    #[must_use]
    pub const fn category(&self) -> ErrorCategory {
        match self {
            Self::RouteNotFound | Self::InvalidRequest { .. } | Self::RequestTimeout => {
                ErrorCategory::Client
            }
            Self::BodyTooLarge { .. } => ErrorCategory::Limit,
            Self::NotReady
            | Self::DeadlineExceeded { .. }
            | Self::UpstreamFailed { .. }
            | Self::ProviderNotConfigured { .. } => ErrorCategory::Upstream,
            Self::Internal { .. } | Self::ResponseCommitted => ErrorCategory::Internal,
        }
    }
}

/// Writes `value` as a JSON string body, escaping what would otherwise break the document.
///
/// Only the trace id needs this: it can come from a request header, so it is attacker-controlled.
fn push_json_string(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Control characters are not legal unescaped in JSON strings.
            ch if ch < ' ' => {
                // Writing to a `String` cannot fail, so the result is discarded deliberately.
                let _ = write!(out, "\\u{:04x}", u32::from(ch));
            }
            ch => out.push(ch),
        }
    }
}

// Compiled-in proof that an HTTP error can cross `.await` and `tokio::spawn` boundaries.
const _: fn() = || {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<HttpError>();
};

#[cfg(test)]
mod tests;
