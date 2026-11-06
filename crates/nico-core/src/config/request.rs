//! The per-request resource bounds declared by a document.
//!
//! [`RequestBounds`] is small plain data, so it is `Copy`: only validated values ever exist, which
//! keeps it usable by value on the request path. It owns its own range validation because the range
//! is a property of the data, not of the parser.
//!
//! Only `timeout_ms` is range-checked. `max_body_bytes` and `max_response_bytes` are whatever the
//! document says — `u32` is their only boundary, and `0` is a legal value meaning "accept no body" —
//! because the operator who writes a byte ceiling also accepts what honouring it costs: a buffered
//! route may hold that many bytes per in-flight request.

use super::error::DocumentError;

/// Largest `request.timeout_ms` accepted by `Document::parse`, in milliseconds.
const MAX_REQUEST_TIMEOUT_MS: u32 = 600_000;

/// The `request.max_response_bytes` a document gets when it does not write one: 8 MiB.
///
/// It is a default rather than a limit because the bound only applies to a buffered (non-streaming)
/// response: a document that serves nothing but event streams never pays for it, so a document whose
/// answers are larger or smaller writes the bound explicitly instead of inheriting a number chosen
/// for someone else.
pub const DEFAULT_MAX_RESPONSE_BYTES: u32 = 8 * 1024 * 1024;

/// The per-request bounds applied to every request.
///
/// Obtainable only through [`Document`](super::Document::request), so the values it carries have
/// already passed range validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestBounds {
    max_body_bytes: u32,
    max_response_bytes: u32,
    timeout_ms: u32,
}

impl RequestBounds {
    /// Builds bounds for range validation.
    pub(super) const fn new(max_body_bytes: u32, max_response_bytes: u32, timeout_ms: u32) -> Self {
        Self {
            max_body_bytes,
            max_response_bytes,
            timeout_ms,
        }
    }

    /// Returns the maximum request body size in bytes.
    #[must_use]
    pub const fn max_body_bytes(self) -> u32 {
        self.max_body_bytes
    }

    /// Returns the ceiling on a buffered (non-streaming) response body, in bytes.
    ///
    /// A response that streams is never buffered, so the bound does not apply to it. This is the
    /// document-wide ceiling a route may tighten: a route can lower it, never raise it. `u32` is its
    /// only boundary, so `0` is legal and means only an empty body fits.
    #[must_use]
    pub const fn max_response_bytes(self) -> u32 {
        self.max_response_bytes
    }

    /// Returns the whole-request timeout in milliseconds.
    #[must_use]
    pub const fn timeout_ms(self) -> u32 {
        self.timeout_ms
    }

    /// Checks the fields that have an accepted range.
    ///
    /// # Errors
    /// Returns [`DocumentError::LimitOutOfRange`] naming `request.timeout_ms` when it is `0` or
    /// above ten minutes.
    pub(super) fn validate(self) -> Result<(), DocumentError> {
        if !(1..=MAX_REQUEST_TIMEOUT_MS).contains(&self.timeout_ms) {
            return Err(DocumentError::LimitOutOfRange {
                field: "request.timeout_ms",
                value: u64::from(self.timeout_ms),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_body_bound_is_legal() {
        let bounds = RequestBounds::new(0, DEFAULT_MAX_RESPONSE_BYTES, 1000);
        assert!(bounds.validate().is_ok(), "0 means: accept no request body");
    }

    #[test]
    fn the_largest_body_bound_is_legal() {
        let bounds = RequestBounds::new(u32::MAX, DEFAULT_MAX_RESPONSE_BYTES, 1000);
        assert!(
            bounds.validate().is_ok(),
            "u32 is the only boundary of max_body_bytes"
        );
    }

    #[test]
    fn a_response_bound_has_no_range() {
        for max_response_bytes in [0, u32::MAX] {
            let bounds = RequestBounds::new(1024, max_response_bytes, 1000);
            assert!(
                bounds.validate().is_ok(),
                "u32 is the only boundary of max_response_bytes"
            );
            assert_eq!(bounds.max_response_bytes(), max_response_bytes);
        }
    }

    #[test]
    fn a_timeout_without_a_range_is_rejected() {
        for timeout_ms in [0, MAX_REQUEST_TIMEOUT_MS + 1] {
            let error = RequestBounds::new(1024, DEFAULT_MAX_RESPONSE_BYTES, timeout_ms)
                .validate()
                .expect_err("timeout must be in range");
            assert_eq!(error.to_string(), {
                let field = "request.timeout_ms";
                format!("limit is out of range: {field} = {timeout_ms}")
            });
        }
    }
}
