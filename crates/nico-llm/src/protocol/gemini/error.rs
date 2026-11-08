//! Encoding a canonical failure as the error envelope a `generateContent` caller expects.
//!
//! This protocol's envelope is `{"error": {"code", "message", "status"}}`: the numeric `code` is the
//! HTTP status the caller is answered with, and `status` is the vendor's `google.rpc` token. The
//! canonical error's stable code has no field here — the numeric member repeats the status line, it
//! does not carry a gateway code — so it is not written; inventing a member the protocol does not
//! have would produce a document no caller's parser expects. The status itself belongs to the
//! canonical error, and the exchange passes it to the data plane unchanged.
//!
//! As in every other envelope here, the provider's own status, body and credential have no spelling:
//! they were never in the canonical error.

use nico_core::ErrorCategory;
use serde::Serialize;

use super::PROTOCOL;
use crate::{protocol::CodecError, schema::CanonicalError, stream::sse};

/// Encodes one failure as this envelope's JSON body.
pub(super) fn encode(error: &CanonicalError) -> Result<Vec<u8>, CodecError> {
    Ok(document(error)?.into_bytes())
}

/// Encodes one failure as a single server-sent event, for a stream that failed after it started.
pub(super) fn frame(error: &CanonicalError) -> Result<Vec<u8>, CodecError> {
    Ok(sse::encode_event(&document(error)?))
}

/// Builds the envelope document.
fn document(error: &CanonicalError) -> Result<String, CodecError> {
    let envelope = Envelope {
        error: Body {
            code: error.http_status(),
            message: error.message(),
            status: status(error.category()),
        },
    };
    serde_json::to_string(&envelope).map_err(|failure| CodecError::Encode {
        protocol: PROTOCOL,
        reason: failure.to_string(),
    })
}

/// Returns the envelope's status token: the vendor's vocabulary, not the canonical category.
///
/// The mapping is closed over the five categories because the categories are: a new one arrives with
/// a decision about which vendor word it becomes, not with a wildcard that silently widens the
/// envelope. A caller failure keeps the protocol's own word for a request it refused; a limit keeps
/// the one callers back off on; an upstream failure is the peer's, which is `UNAVAILABLE` here — the
/// token that says the dependency, not this gateway, is what failed.
fn status(category: ErrorCategory) -> &'static str {
    match category {
        ErrorCategory::Client => "INVALID_ARGUMENT",
        ErrorCategory::Limit => "RESOURCE_EXHAUSTED",
        ErrorCategory::Upstream => "UNAVAILABLE",
        ErrorCategory::Cancelled => "CANCELLED",
        ErrorCategory::Internal => "INTERNAL",
        _ => "INTERNAL",
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
    /// The HTTP status the caller is answered with; this envelope's spelling for it.
    code: u16,
    message: &'a str,
    status: &'static str,
}
