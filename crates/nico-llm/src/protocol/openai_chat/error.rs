//! Encoding a canonical failure as the error envelope a Chat Completions caller expects.
//!
//! The envelope carries the canonical error's stable code and its caller-safe message. The provider's
//! own status code, body and credential are not part of it: they were never in the canonical error,
//! and the adapter that read them kept them in its own log record.

use nico_core::ErrorCategory;
use serde::Serialize;

use super::PROTOCOL;
use crate::{protocol::CodecError, schema::CanonicalError, stream::sse};

/// The HTTP body of one failure.
///
/// The status is not written here: it belongs to the canonical error, and the exchange passes it to
/// the data plane unchanged.
pub(super) fn encode(error: &CanonicalError) -> Result<Vec<u8>, CodecError> {
    Ok(document(error)?.into_bytes())
}

/// The same envelope framed as one event, for a stream that fails after it started.
pub(super) fn frame(error: &CanonicalError) -> Result<Vec<u8>, CodecError> {
    Ok(sse::encode_event(&document(error)?))
}

/// Builds the envelope document.
fn document(error: &CanonicalError) -> Result<String, CodecError> {
    let envelope = Envelope {
        error: Body {
            message: error.message(),
            kind: kind(error.category()),
            // The canonical error does not name a request member, and guessing one would point the
            // caller at a field that may not be at fault.
            param: None,
            code: error.code(),
        },
    };
    serde_json::to_string(&envelope).map_err(|error| CodecError::Encode {
        protocol: PROTOCOL,
        reason: error.to_string(),
    })
}

/// Returns the envelope's failure type: the protocol's own vocabulary, not the canonical category.
fn kind(category: ErrorCategory) -> &'static str {
    match category {
        ErrorCategory::Client => "invalid_request_error",
        ErrorCategory::Limit => "rate_limit_error",
        ErrorCategory::Upstream | ErrorCategory::Internal | ErrorCategory::Cancelled => {
            "server_error"
        }
        _ => "server_error",
    }
}

/// The document a caller parses for a failure.
#[derive(Serialize)]
struct Envelope<'a> {
    error: Body<'a>,
}

/// The failure itself.
#[derive(Serialize)]
struct Body<'a> {
    message: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    param: Option<&'a str>,
    code: &'a str,
}
