//! The error envelope the two OpenAI client protocols share.
//!
//! Chat Completions and Responses failed in the same shape long before they answered in the same
//! shape: one `{"error": {"message", "type", "param", "code"}}` object, whose `type` is the vendor's
//! own vocabulary rather than the canonical category. Both codecs therefore build the document here,
//! from [`CanonicalError`] alone: the status belongs to the canonical error and the exchange passes it
//! to the data plane unchanged, so this module returns a body and never a status.
//!
//! # What is not written
//!
//! - The provider's status code, body and credential. They were never in the canonical error; the
//!   adapter that read them kept them in its own log record.
//! - A `param`. The canonical error does not name a request member, and guessing one would point the
//!   caller at a field that may not be at fault.

use nico_core::ErrorCategory;
use serde::Serialize;

use crate::{
    protocol::{CodecError, ProtocolId},
    schema::CanonicalError,
    stream::sse,
};

/// Encodes one failure as this envelope's JSON body.
pub(crate) fn document(
    protocol: ProtocolId,
    error: &CanonicalError,
) -> Result<Vec<u8>, CodecError> {
    Ok(serialize(protocol, error)?.into_bytes())
}

/// Encodes one failure as a single server-sent event, for a stream that failed after it started.
pub(crate) fn frame(protocol: ProtocolId, error: &CanonicalError) -> Result<Vec<u8>, CodecError> {
    Ok(sse::encode_event(&serialize(protocol, error)?))
}

/// Builds the document text.
fn serialize(protocol: ProtocolId, error: &CanonicalError) -> Result<String, CodecError> {
    let envelope = Envelope {
        error: Body {
            message: error.message(),
            kind: kind(error.category()),
            param: None,
            code: error.code(),
        },
    };
    serde_json::to_string(&envelope).map_err(|failure| CodecError::Encode {
        protocol,
        reason: failure.to_string(),
    })
}

/// Returns the envelope's failure type: the vendor's vocabulary, not the canonical category.
///
/// The mapping is closed over the five categories because the categories are: a new one arrives with
/// a decision about which vendor word it becomes, not with a wildcard that silently widens the
/// envelope.
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
