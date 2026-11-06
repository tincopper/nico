//! Mapping a provider failure status into the canonical error, without its body.
//!
//! The status decides the category, the stable code and the status the caller sees. The mapping is a
//! closed table so that the same provider failure always reads the same way, and so that an operator
//! can name the code an alert fired on:
//!
//! | Provider status | Category | Code | Caller status | Retryable |
//! |---|---|---|---|---|
//! | 400 | `Client` | `upstream_rejected` | 400 | no |
//! | 401, 403 | `Upstream` | `upstream_auth_failed` | 502 | no |
//! | 404 | `Client` | `upstream_model_not_found` | 404 | no |
//! | 408 | `Upstream` | `upstream_timeout` | 504 | yes |
//! | 429 | `Limit` | `upstream_rate_limited` | 429 | yes |
//! | other 4xx | `Client` | `upstream_rejected` | the provider's status | no |
//! | 5xx | `Upstream` | `upstream_error` | 502 | yes |
//! | anything else | `Upstream` | `upstream_error` | 502 | no |
//!
//! A credential failure is a gateway problem, not the caller's: the caller presented a valid document
//! and a route whose key the provider refuses must not look like a malformed request. A missing model
//! is the caller's — it named something the route cannot serve — and keeps its `404`. A rate limit
//! keeps `429` so a client's own retry policy still works.
//!
//! The provider's own `error.code` is kept as [`CanonicalError::provider_code`] when it is a bounded
//! printable token, because that is what makes a log line correlate with the provider's status page.
//! Anything else is dropped rather than sanitized: the value reaches a log field, and a provider that
//! could put a line break or a multi-byte sentence there could shape the record itself. Its message,
//! type, headers and body are not read into the error: the provider's text can quote the prompt or an
//! internal host, and none of it belongs in a caller's answer.

use nico_core::ErrorCategory;
use serde::Deserialize;

use crate::{
    diagnostic,
    schema::{CanonicalError, MAX_PROVIDER_CODE_BYTES, RawJson},
};

/// Largest error body this module parses to find the provider's code.
///
/// An error envelope is small; a multi-megabyte failure body is not a document whose code is worth
/// scanning, and the status mapping already decided the answer.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Builds the canonical error a failure status describes.
pub(super) fn failure(status: u16, body: &[u8]) -> CanonicalError {
    let shape = shape(status);
    let code = envelope_code(body);
    decorate(
        CanonicalError::from_static_parts(shape.category, shape.code, shape.message, shape.status)
            .with_retryable(shape.retryable),
        code.as_deref(),
    )
}

/// Builds the canonical error a failed stream frame describes.
///
/// A stream that fails after it started is never retryable: the caller already received bytes, and a
/// second attempt would be a second answer. The frame's `code` is kept for the log for the same reason
/// a status error's code is.
pub(super) fn stream_failure(code: Option<&RawJson>) -> CanonicalError {
    decorate(
        CanonicalError::from_static_parts(
            ErrorCategory::Upstream,
            "upstream_error",
            "the upstream failed while streaming the answer",
            502,
        ),
        code.and_then(as_string),
    )
}

/// One row of the status table.
struct Shape {
    category: ErrorCategory,
    code: &'static str,
    message: &'static str,
    status: u16,
    retryable: bool,
}

/// Returns the row one provider status maps to.
const fn shape(status: u16) -> Shape {
    match status {
        400 => Shape {
            category: ErrorCategory::Client,
            code: "upstream_rejected",
            message: "the upstream rejected the request",
            status: 400,
            retryable: false,
        },
        401 | 403 => Shape {
            category: ErrorCategory::Upstream,
            code: "upstream_auth_failed",
            message: "the upstream rejected this gateway's credential",
            status: 502,
            retryable: false,
        },
        404 => Shape {
            category: ErrorCategory::Client,
            code: "upstream_model_not_found",
            message: "the upstream does not serve the requested model",
            status: 404,
            retryable: false,
        },
        408 => Shape {
            category: ErrorCategory::Upstream,
            code: "upstream_timeout",
            message: "the upstream did not answer in time",
            status: 504,
            retryable: true,
        },
        429 => Shape {
            category: ErrorCategory::Limit,
            code: "upstream_rate_limited",
            message: "the upstream is rate limiting this gateway",
            status: 429,
            retryable: true,
        },
        500..=599 => Shape {
            category: ErrorCategory::Upstream,
            code: "upstream_error",
            message: "the upstream failed to serve the request",
            status: 502,
            retryable: true,
        },
        other if other >= 400 && other <= 499 => Shape {
            category: ErrorCategory::Client,
            code: "upstream_rejected",
            message: "the upstream rejected the request",
            status: other,
            retryable: false,
        },
        _ => Shape {
            category: ErrorCategory::Upstream,
            code: "upstream_error",
            message: "the upstream failed to serve the request",
            status: 502,
            retryable: false,
        },
    }
}

/// Attaches the provider's own code when it is present and safe to log.
///
/// The code is a log field, never part of the answer, so it must be a bounded printable token: a
/// value with a line break or non-ASCII text would let the provider shape the record itself.
fn decorate(error: CanonicalError, code: Option<&str>) -> CanonicalError {
    let Some(code) = code.and_then(|text| diagnostic::quoted(text, MAX_PROVIDER_CODE_BYTES)) else {
        return error;
    };
    // `quoted` screens with exactly the bound `with_provider_code` validates, so attaching cannot
    // fail; if it ever does, the undecorated failure is kept rather than lost.
    error.clone().with_provider_code(code).unwrap_or(error)
}

/// Reads the `error.code` of a provider failure body, when it is a bounded string.
fn envelope_code(body: &[u8]) -> Option<String> {
    if body.len() > MAX_ERROR_BODY_BYTES {
        return None;
    }
    let envelope: Envelope = serde_json::from_slice(body).ok()?;
    let error = envelope.error?;
    error.code.as_ref().and_then(as_string).map(str::to_owned)
}

/// Returns the string a JSON value holds, when it is a non-empty one.
fn as_string(value: &RawJson) -> Option<&str> {
    value.as_value().as_str().filter(|text| !text.is_empty())
}

/// The part of a provider failure body this adapter reads.
#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    error: Option<Body>,
}

/// The provider's failure object; only its machine-readable code is used.
#[derive(Deserialize)]
struct Body {
    #[serde(default)]
    code: Option<RawJson>,
}
