//! Encoding a canonical failure as the error envelope a Responses caller expects.
//!
//! The document is the one both OpenAI client protocols answer with, so it lives in
//! [`super::openai_error`]; this module keeps the protocol's own name on the two entry points.

use super::PROTOCOL;
use crate::{protocol::CodecError, schema::CanonicalError};

/// The HTTP body of one failure.
pub(super) fn encode(error: &CanonicalError) -> Result<Vec<u8>, CodecError> {
    super::super::openai_error::document(PROTOCOL, error)
}

/// The same envelope framed as one event, for a stream that fails after it started.
pub(super) fn frame(error: &CanonicalError) -> Result<Vec<u8>, CodecError> {
    super::super::openai_error::frame(PROTOCOL, error)
}
