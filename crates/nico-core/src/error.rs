//! Error classification shared across data planes.

/// Classifies a failure without deciding how a protocol represents it.
///
/// Data planes map a category to their own status code or reason code; the kernel never carries a
/// wire-level value. The enum is `Copy` because it is a fieldless tag, and `#[non_exhaustive]` so a
/// new category does not break downstream matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorCategory {
    /// The caller is at fault: malformed input, failed authentication or authorization.
    Client,
    /// A dependency failed: upstream peer, LLM provider, configuration source.
    Upstream,
    /// A resource or rate limit was reached.
    Limit,
    /// An invariant was violated inside the gateway.
    Internal,
    /// A deadline expired, shutdown began, or the caller disconnected.
    Cancelled,
}

impl ErrorCategory {
    /// Reports whether this failure is the caller's fault.
    #[must_use]
    pub const fn is_client(self) -> bool {
        matches!(self, Self::Client)
    }
}

// Compiled-in proof that a category can cross `.await` and `tokio::spawn` boundaries.
const _: fn() = || {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<ErrorCategory>();
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_category_is_the_only_caller_fault() {
        assert!(ErrorCategory::Client.is_client());
        for category in [
            ErrorCategory::Upstream,
            ErrorCategory::Limit,
            ErrorCategory::Internal,
            ErrorCategory::Cancelled,
        ] {
            assert!(!category.is_client(), "expected non-client category");
        }
    }
}
