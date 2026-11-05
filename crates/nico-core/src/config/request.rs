//! The per-request resource bounds declared by a document.
//!
//! [`RequestBounds`] is small plain data, so it is `Copy`: only validated values ever exist, which
//! keeps it usable by value on the request path. It owns its own range validation because the range
//! is a property of the data, not of the parser.
//!
//! Only `timeout_ms` is range-checked. `max_body_bytes` is whatever the document says — `u32` is its
//! only boundary, and `0` is a legal value meaning "accept no request body" — because the operator
//! who writes a byte ceiling also accepts what honouring it costs: a buffered route may hold that
//! many bytes per in-flight request.

use super::error::DocumentError;

/// Largest `request.timeout_ms` accepted by `Document::parse`, in milliseconds.
const MAX_REQUEST_TIMEOUT_MS: u32 = 600_000;

/// The per-request bounds applied to every request.
///
/// Obtainable only through [`Document`](super::Document::request), so the values it carries have
/// already passed range validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestBounds {
    max_body_bytes: u32,
    timeout_ms: u32,
}

impl RequestBounds {
    /// Builds bounds for range validation.
    pub(super) const fn new(max_body_bytes: u32, timeout_ms: u32) -> Self {
        Self {
            max_body_bytes,
            timeout_ms,
        }
    }

    /// Returns the maximum request body size in bytes.
    #[must_use]
    pub const fn max_body_bytes(self) -> u32 {
        self.max_body_bytes
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
        let bounds = RequestBounds::new(0, 1000);
        assert!(bounds.validate().is_ok(), "0 means: accept no request body");
    }

    #[test]
    fn the_largest_body_bound_is_legal() {
        let bounds = RequestBounds::new(u32::MAX, 1000);
        assert!(
            bounds.validate().is_ok(),
            "u32 is the only boundary of max_body_bytes"
        );
    }

    #[test]
    fn a_timeout_without_a_range_is_rejected() {
        for timeout_ms in [0, MAX_REQUEST_TIMEOUT_MS + 1] {
            let error = RequestBounds::new(1024, timeout_ms)
                .validate()
                .expect_err("timeout must be in range");
            assert_eq!(error.to_string(), {
                let field = "request.timeout_ms";
                format!("limit is out of range: {field} = {timeout_ms}")
            });
        }
    }
}
