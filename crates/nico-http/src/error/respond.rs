//! How one failure is rendered: the status, the stable code, the headers and the body a caller sees.
//!
//! The value is produced only by [`HttpError::respond`](super::HttpError::respond), so the set of
//! presentations is closed: this crate's fixed envelope, a policy's compiled template, or a client
//! codec's error document. A caller reads; it never assembles one.
//!
//! The body is decided by the producer rather than by the status: a policy owns the content type its
//! document configured, a front-end owns the bytes its protocol parses, and only this crate's own
//! failures render the built-in JSON envelope. That is why the renderer is a total match over
//! producers instead of a branch on a status code.

use std::{fmt, sync::Arc, time::Duration};

use bytes::Bytes;
use pingora::http::StatusCode;

use super::{LlmRejection, Rejection, push_json_string};

/// How one failure is rendered, if at all.
///
/// Fields are private because the set is a contract: a caller may read it, never assemble a variant
/// of its own. A built-in failure's `code` and `message` are compile-time constants, so the only value
/// that needs escaping when its body is built is the request's trace id; a policy or a front-end
/// supplies its own presentation instead, and the code, message and body are read from it.
///
/// This is no longer `Copy`: a rejection holds the compiled presentation shared by every request it
/// refuses, and cloning that `Arc` is cheaper than copying the template or the body it points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Respond {
    status: StatusCode,
    challenge: Option<&'static str>,
    retry_after: Option<Duration>,
    presentation: Presentation,
}

/// Where a rendered failure's code, message and body come from.
///
/// One variant per producer, so the renderer is a total match instead of a pair of optional fields a
/// later edit could combine into an unreachable state.
#[derive(Clone, PartialEq, Eq)]
enum Presentation {
    /// This crate's fixed envelope, built from the code and message.
    Envelope {
        code: &'static str,
        message: &'static str,
    },
    /// A policy's compiled template, which owns the body and the content type it is written with.
    Template(Arc<Rejection>),
    /// A client codec's error document, already encoded in the client's protocol.
    Llm(Arc<LlmRejection>),
}

impl fmt::Debug for Presentation {
    /// Prints the presentation's identity and never a body: the bytes may be the client's error
    /// document, and no log line needs them.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Envelope { code, message } => formatter
                .debug_struct("Envelope")
                .field("code", code)
                .field("message", message)
                .finish(),
            Self::Template(rejection) => {
                formatter.debug_tuple("Template").field(rejection).finish()
            }
            Self::Llm(rejection) => formatter.debug_tuple("Llm").field(rejection).finish(),
        }
    }
}

impl Respond {
    /// The content type of a built-in envelope and of a client codec's error document.
    const JSON: &'static str = "application/json";

    pub(super) const fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            challenge: None,
            retry_after: None,
            presentation: Presentation::Envelope { code, message },
        }
    }

    /// Builds the response a compiled rejection describes.
    pub(super) fn from_rejection(rejection: Arc<Rejection>, retry_after: Option<Duration>) -> Self {
        Self {
            status: rejection.status(),
            challenge: rejection.challenge(),
            retry_after,
            presentation: Presentation::Template(rejection),
        }
    }

    /// Builds the response a client codec's error document describes.
    ///
    /// The status, code and category are the front-end's; this crate only writes them with the closed
    /// header set, so a converted route's failure looks like every other refusal to a caller that
    /// matches on `x-nico-error-code`.
    pub(super) fn from_llm_rejection(rejection: Arc<LlmRejection>) -> Self {
        Self {
            status: rejection.status_code(),
            challenge: None,
            retry_after: None,
            presentation: Presentation::Llm(rejection),
        }
    }

    /// Returns the HTTP status code to send.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.status
    }

    /// Returns the stable snake_case error code callers may match on.
    ///
    /// A built-in failure's code is a constant; a policy's and a front-end's are their own, which is
    /// why the spelling is not a `&'static str` here — the stable part is the value, not the lifetime.
    #[must_use]
    pub fn code(&self) -> &str {
        match &self.presentation {
            Presentation::Envelope { code, .. } => code,
            Presentation::Template(rejection) => rejection.code(),
            Presentation::Llm(rejection) => rejection.code(),
        }
    }

    /// Returns the caller-safe message, a sentence with no internal detail.
    #[must_use]
    pub fn message(&self) -> &str {
        match &self.presentation {
            Presentation::Envelope { message, .. } => message,
            Presentation::Template(rejection) => rejection.message(),
            Presentation::Llm(rejection) => rejection.message(),
        }
    }

    /// Returns the content type the body is written with.
    ///
    /// Built-in failures and client error documents answer JSON; a policy's rejection carries
    /// whatever its document configured.
    #[must_use]
    pub fn content_type(&self) -> &str {
        match &self.presentation {
            Presentation::Envelope { .. } | Presentation::Llm(_) => Self::JSON,
            Presentation::Template(rejection) => rejection.content_type(),
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
    /// substitutes for the same reason; a front-end's rejection returns the document its codec
    /// encoded, byte for byte, because this crate cannot and must not re-encode another protocol.
    #[must_use]
    pub fn body(&self, trace_id: &str) -> Bytes {
        match &self.presentation {
            Presentation::Template(rejection) => rejection.render(trace_id),
            Presentation::Llm(rejection) => rejection.body_bytes(),
            Presentation::Envelope { code, message } => {
                let mut json = String::with_capacity(96 + trace_id.len());
                json.push_str(r#"{"error":{"code":""#);
                json.push_str(code);
                json.push_str(r#"","message":""#);
                json.push_str(message);
                json.push_str(r#"","trace_id":""#);
                push_json_string(&mut json, trace_id);
                json.push_str(r#""}}"#);
                Bytes::from(json)
            }
        }
    }
}
