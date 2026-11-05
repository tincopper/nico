//! The data plane's failure vocabulary and the only place it becomes a wire response.
//!
//! Every failure on the request path — routing, limits, plugins, upstream connection — is expressed
//! as one [`HttpError`]. [`HttpError::respond`] maps each variant to exactly one status code, one
//! stable machine-readable `code` and one caller-safe `message`; nothing else in this crate builds
//! an error response, so a plugin cannot invent a status and two call sites cannot disagree.
//!
//! Two properties are deliberate:
//!
//! - **The response never carries internal detail.** The `code` is a snake_case string owned by
//!   this crate and renaming it is a breaking change for callers; the `message` is a fixed
//!   sentence. Upstream names, addresses, credentials and upstream bodies appear in logs through
//!   the error's [`Display`](std::fmt::Display) implementation, never in the response.
//! - **Cancellation is not an error.** A client disconnect, a shutdown and a deadline that expires
//!   after bytes were committed are outcomes, not responses: they produce no body and no 5xx.
//!
//! A policy that refuses a request with its own status and body does not get a second mapper: it
//! supplies a compiled [`Rejection`] (see the `rejection` module), which [`HttpError::respond`] turns
//! into the same [`Respond`] a built-in failure produces. `code` stays stable and always travels in
//! `x-nico-error-code`, and the category is carried by the rejection instead of being guessed from
//! the status a document may have overridden.

mod rejection;

use std::{borrow::Cow, error::Error, fmt::Write as _, sync::Arc, time::Duration};

use bytes::Bytes;
use nico_core::ErrorCategory;
use pingora::http::StatusCode;
pub use rejection::{BodyFormat, BodyTemplate, MAX_LITERAL_BYTES, Part, Rejection, RejectionError};

use crate::plugin::PluginError;

/// How one failure is rendered, if at all.
///
/// Fields are private because the set is a contract: a caller may read it, never assemble a variant
/// of its own. `code` and `message` are compile-time constants, so the only value that needs escaping
/// when the body is built is the request's trace id.
///
/// This is no longer `Copy`: a policy rejection holds an `Arc<Rejection>` shared by every request it
/// refuses, and cloning that `Arc` is cheaper than copying the template it points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Respond {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    challenge: Option<&'static str>,
    retry_after: Option<Duration>,
    /// `None` for a built-in failure, whose body is the fixed envelope and whose content type is
    /// [`Self::JSON`]; `Some` when a policy compiled its own presentation, which then owns the body
    /// *and* the content type it is written with (a document configures both).
    rejection: Option<Arc<Rejection>>,
}

impl Respond {
    /// The content type of a built-in envelope.
    const JSON: &'static str = "application/json";

    const fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
            challenge: None,
            retry_after: None,
            rejection: None,
        }
    }

    /// Builds the response a compiled rejection describes.
    fn from_rejection(rejection: Arc<Rejection>, retry_after: Option<Duration>) -> Self {
        Self {
            status: rejection.status(),
            code: rejection.code(),
            message: rejection.message(),
            challenge: rejection.challenge(),
            retry_after,
            rejection: Some(rejection),
        }
    }

    /// Returns the HTTP status code to send.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.status
    }

    /// Returns the stable snake_case error code callers may match on.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    /// Returns the caller-safe message, a fixed sentence with no internal detail.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        self.message
    }

    /// Returns the content type the body is written with.
    ///
    /// Built-in failures answer JSON; a policy's rejection carries whatever its document configured.
    #[must_use]
    pub fn content_type(&self) -> &str {
        match &self.rejection {
            Some(rejection) => rejection.content_type(),
            None => Self::JSON,
        }
    }

    /// Returns the `www-authenticate` challenge, when the rejection answers an authentication
    /// failure.
    #[must_use]
    pub const fn challenge(&self) -> Option<&'static str> {
        self.challenge
    }

    /// Returns how long the caller should wait before retrying, when the rejection is a limit.
    #[must_use]
    pub const fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    /// Renders the error body.
    ///
    /// A built-in failure renders `{"error":{"code","message","trace_id"}}` with `trace_id` escaped;
    /// a policy rejection renders the template it compiled, which escapes the variables it
    /// substitutes for the same reason.
    #[must_use]
    pub fn body(&self, trace_id: &str) -> Bytes {
        if let Some(rejection) = &self.rejection {
            return rejection.render(trace_id);
        }
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
    /// A policy refused the request and compiled how the refusal is presented.
    ///
    /// This is the one variant a policy decides: a plugin returns
    /// [`PluginError::Refused`], and the status, the stable code, the
    /// headers and the body all come from the [`Rejection`] compiled when the document was
    /// published. `category` is carried by the rejection, so a document that overrides the status
    /// cannot change how the failure is logged.
    #[error("request rejected by policy: {detail}")]
    Rejected {
        /// The compiled presentation, shared by every request this policy refuses.
        rejection: Arc<Rejection>,
        /// Why the request was rejected, for logs only. It is a constant sentence, so it can neither
        /// quote a credential nor repeat anything the caller sent.
        detail: &'static str,
        /// How long the caller should wait before retrying, when the policy knows.
        retry_after: Option<Duration>,
    },
    /// A response was already committed, so no error response can replace it.
    #[error("response already committed; the failure cannot be rendered as a response")]
    ResponseCommitted,
}

impl HttpError {
    /// Returns how this failure is rendered, or `None` when a response is no longer possible.
    ///
    /// This is the single mapping point: status, stable code, headers and body are decided here and
    /// nowhere else. A compiled [`Rejection`] is passed through, which is why a policy can present its
    /// own refusal without a second mapping function existing.
    #[must_use]
    pub fn respond(&self) -> Option<Respond> {
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
            Self::Rejected {
                rejection,
                retry_after,
                ..
            } => Some(Respond::from_rejection(Arc::clone(rejection), *retry_after)),
            Self::ResponseCommitted => None,
        }
    }

    /// Classifies the failure for the layer that logs it.
    ///
    /// `ResponseCommitted` is [`ErrorCategory::Internal`] because reaching it means the data plane
    /// tried to replace a response it had already sent; a plain cancellation is not an `HttpError`,
    /// so it can never be misreported as a 5xx here. A policy rejection reports the category its
    /// plugin belongs to — authentication is the caller's fault, a rate limit is a limit — and never
    /// one derived from the status code a document may have overridden.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::RouteNotFound | Self::InvalidRequest { .. } | Self::RequestTimeout => {
                ErrorCategory::Client
            }
            Self::BodyTooLarge { .. } => ErrorCategory::Limit,
            Self::NotReady
            | Self::DeadlineExceeded { .. }
            | Self::UpstreamFailed { .. }
            | Self::ProviderNotConfigured { .. } => ErrorCategory::Upstream,
            Self::Rejected { rejection, .. } => rejection.category(),
            Self::Internal { .. } | Self::ResponseCommitted => ErrorCategory::Internal,
        }
    }
}

/// Why a plugin stage failed, kept as the cause of an internal failure.
///
/// A plugin that cannot do its job is this process's defect, so it is answered as
/// [`HttpError::Internal`] with the plugin layer named in the cause. The plugin *instance* is a
/// per-request fact: the chain records it on [`HttpContext`](crate::HttpContext) at the one place
/// that knows both the entry and the error, and the request's single log record emits it as `plugin`.
/// Keeping it out of the error type is what preserves one conversion and a caller-safe `Display`.
#[derive(Debug, thiserror::Error)]
enum PluginFault {
    /// The plugin failed to do its own work.
    #[error("plugin execution failed: {reason}")]
    Execute {
        /// The plugin's own fixed sentence.
        reason: &'static str,
    },
    /// The plugin called a context method its phase does not offer.
    #[error("plugin violated its phase contract: {reason}")]
    PhaseViolation {
        /// The method and the stage it belongs to.
        reason: &'static str,
    },
}

impl From<PluginError> for HttpError {
    /// Converts a plugin failure into the data plane's failure vocabulary.
    ///
    /// This is the only conversion. A refusal keeps its compiled [`Rejection`] — status, stable code,
    /// category and body — because a document configured that presentation, and the data plane must
    /// not second-guess it. Everything else is a `500`: a plugin that cannot execute, or that calls a
    /// method its phase does not offer, is a defect on this side of the connection.
    fn from(error: PluginError) -> Self {
        match error {
            PluginError::Refused {
                rejection,
                detail,
                retry_after,
            } => Self::Rejected {
                rejection,
                detail,
                retry_after,
            },
            PluginError::Execute(reason) => Self::Internal {
                source: Box::new(PluginFault::Execute { reason }),
            },
            PluginError::PhaseViolation(reason) => Self::Internal {
                source: Box::new(PluginFault::PhaseViolation { reason }),
            },
        }
    }
}

/// Returns `value` escaped for use inside a JSON string.
///
/// This is the crate's one JSON escaper, exported for the layer that compiles a rejection body from
/// configuration: [`Part::Literal`] in a [`BodyFormat::Json`] template must already be valid JSON
/// syntax, and a literal built from a document (an operator's message, an object key) needs exactly
/// this escaping. Exporting it is what keeps a second escaper from growing in the policy crate and
/// drifting from the one the renderer uses for variables.
#[must_use]
pub fn escape_json_string(value: &str) -> String {
    // Escaping only ever grows the byte length, so the input's own length is the starting capacity;
    // the common case (an identifier or a short message) does not reallocate.
    let mut out = String::with_capacity(value.len());
    push_json_string(&mut out, value);
    out
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
