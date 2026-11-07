//! Encoding a canonical failure as the error envelope a Messages caller expects.
//!
//! This protocol's envelope is `{"type": "error", "error": {"type", "message"}}`: the failure type is
//! the vendor's vocabulary, and there is no field for a stable machine code. The canonical code is
//! therefore *not* written — inventing a member the protocol does not have would produce a document
//! no caller's parser expects. The status belongs to the canonical error and the exchange passes it
//! to the data plane unchanged.
//!
//! As in every other envelope here, the provider's own status, body and credential have no spelling:
//! they were never in the canonical error.

use nico_core::ErrorCategory;
use serde::Serialize;

use super::PROTOCOL;
use crate::{protocol::CodecError, schema::CanonicalError};

/// The HTTP body of one failure.
pub(super) fn encode(error: &CanonicalError) -> Result<Vec<u8>, CodecError> {
    Ok(document(error)?.into_bytes())
}

/// The same envelope framed as one event, for a stream that fails after it started.
pub(super) fn frame(error: &CanonicalError) -> Result<Vec<u8>, CodecError> {
    Ok(event(&document(error)?))
}

/// Builds the envelope document.
fn document(error: &CanonicalError) -> Result<String, CodecError> {
    let envelope = Envelope {
        kind: "error",
        error: Body {
            kind: kind(error.category()),
            message: error.message(),
        },
    };
    serde_json::to_string(&envelope).map_err(|failure| CodecError::Encode {
        protocol: PROTOCOL,
        reason: failure.to_string(),
    })
}

/// Frames one document as an `error` event, which is the name this protocol gives it.
///
/// The frame is written here rather than through the shared event helper because this protocol names
/// its events: an `event:` line the shared helper does not write is the one part of the framing that
/// is not shared.
fn event(document: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(document.len() + 16);
    bytes.extend_from_slice(b"event: error\ndata: ");
    bytes.extend_from_slice(document.as_bytes());
    bytes.extend_from_slice(b"\n\n");
    bytes
}

/// Returns the envelope's failure type: the vendor's vocabulary, not the canonical category.
///
/// The mapping is closed over the five categories because the categories are. A client failure keeps
/// the protocol's own word for a request it refused; a limit keeps the one callers back off on; an
/// upstream, internal or cancelled failure is the gateway's, which is `api_error` here.
fn kind(category: ErrorCategory) -> &'static str {
    match category {
        ErrorCategory::Client => "invalid_request_error",
        ErrorCategory::Limit => "rate_limit_error",
        ErrorCategory::Upstream | ErrorCategory::Internal | ErrorCategory::Cancelled => "api_error",
        _ => "api_error",
    }
}

/// The document a caller parses for a failure.
#[derive(Serialize)]
struct Envelope<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    error: Body<'a>,
}

/// The failure itself.
#[derive(Serialize)]
struct Body<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    message: &'a str,
}
