//! Mapping a provider failure status into the canonical error, without its body.
//!
//! The status decides the category, the stable code and the status the caller sees. The mapping is the
//! same closed table every adapter uses, because a provider status means the same thing whoever
//! answered it:
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
//! This dialect spells its machine-readable failure reason as `error.status` (a token such as
//! `INVALID_ARGUMENT`) rather than `error.code` (a number that repeats the HTTP status), so that is the
//! value kept as [`CanonicalError::provider_code`] — for the structured log only, and only when it is a
//! bounded printable token. The provider's message, request id, headers and body are never read into
//! the error: its text can quote the prompt or an internal host, and none of it belongs in a caller's
//! answer.

use nico_core::ErrorCategory;
use serde::Deserialize;

use crate::{
    diagnostic,
    schema::{CanonicalError, MAX_PROVIDER_CODE_BYTES, RawJson},
};

/// Largest error body this module parses to find the provider's status token.
///
/// An error envelope is small; a multi-megabyte failure body is not a document whose token is worth
/// scanning, and the status mapping already decided the answer.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Builds the canonical error a failure status describes.
pub(super) fn failure(status: u16, body: &[u8]) -> CanonicalError {
    let shape = shape(status);
    decorate(
        CanonicalError::from_static_parts(shape.category, shape.code, shape.message, shape.status)
            .with_retryable(shape.retryable),
        envelope_status(body).as_deref(),
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

/// Attaches the provider's own status token when it is present and safe to log.
fn decorate(error: CanonicalError, code: Option<&str>) -> CanonicalError {
    let Some(code) = code.and_then(|text| diagnostic::quoted(text, MAX_PROVIDER_CODE_BYTES)) else {
        return error;
    };
    // `quoted` screens with exactly the bound `with_provider_code` validates, so attaching cannot
    // fail; if it ever does, the undecorated failure is kept rather than lost.
    error.clone().with_provider_code(code).unwrap_or(error)
}

/// Reads the `error.status` of a provider failure body, when it is a bounded string.
fn envelope_status(body: &[u8]) -> Option<String> {
    if body.len() > MAX_ERROR_BODY_BYTES {
        return None;
    }
    let envelope: Envelope = serde_json::from_slice(body).ok()?;
    let error = envelope.error?;
    error.status.as_ref().and_then(as_string).map(str::to_owned)
}

/// Returns the string a JSON value holds, when it is a non-empty one.
fn as_string(value: &RawJson) -> Option<&str> {
    value.as_value().as_str().filter(|text| !text.is_empty())
}

/// The part of a provider failure body this module reads.
#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    error: Option<Body>,
}

/// The provider's failure object; only its machine-readable status is used.
#[derive(Deserialize)]
struct Body {
    #[serde(default)]
    status: Option<RawJson>,
}
