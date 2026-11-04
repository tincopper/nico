//! The global request limits declared by a document.
//!
//! [`Limits`] is a small plain-data type, so it is `Copy`: only validated values ever exist, which
//! keeps it usable by value on the request path. It owns its own range validation because the
//! ranges are a property of the data, not of the parser.

use super::error::DocumentError;

/// Largest `limits.max_request_body_bytes` accepted by `Document::parse`, in bytes (8 MiB).
const MAX_REQUEST_BODY_BYTES: u32 = 8 * 1024 * 1024;
/// Largest `limits.request_timeout_ms` accepted by `Document::parse`, in milliseconds.
const MAX_REQUEST_TIMEOUT_MS: u32 = 600_000;

/// Global limits applied to every request.
///
/// Obtainable only through [`Document`](super::Document::limits), so the values it carries have already passed range
/// validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    max_request_body_bytes: u32,
    request_timeout_ms: u32,
}

impl Limits {
    /// Builds limits for range validation.
    pub(super) const fn new(max_request_body_bytes: u32, request_timeout_ms: u32) -> Self {
        Self {
            max_request_body_bytes,
            request_timeout_ms,
        }
    }

    /// Returns the maximum request body size in bytes.
    #[must_use]
    pub const fn max_request_body_bytes(self) -> u32 {
        self.max_request_body_bytes
    }

    /// Returns the whole-request timeout in milliseconds.
    #[must_use]
    pub const fn request_timeout_ms(self) -> u32 {
        self.request_timeout_ms
    }

    /// Checks both fields against their accepted ranges.
    ///
    /// # Errors
    /// Returns [`DocumentError::LimitOutOfRange`] naming the first field outside its range.
    pub(super) fn validate(self) -> Result<(), DocumentError> {
        if !(1..=MAX_REQUEST_BODY_BYTES).contains(&self.max_request_body_bytes) {
            return Err(DocumentError::LimitOutOfRange {
                field: "limits.max_request_body_bytes",
                value: u64::from(self.max_request_body_bytes),
            });
        }
        if !(1..=MAX_REQUEST_TIMEOUT_MS).contains(&self.request_timeout_ms) {
            return Err(DocumentError::LimitOutOfRange {
                field: "limits.request_timeout_ms",
                value: u64::from(self.request_timeout_ms),
            });
        }
        Ok(())
    }
}
