//! Error classification shared across data planes.

use serde::{Deserialize, Serialize};

/// Classifies a failure without deciding how a protocol represents it.
///
/// Data planes map a category to their own status code or reason code; the kernel never carries a
/// wire-level status. The enum is `Copy` because it is a fieldless tag, and `#[non_exhaustive]` so a
/// new category does not break downstream matches.
///
/// The serde form is the variant's lowercase name. It exists so a canonical value and a golden
/// fixture can record the category (the canonical LLM error carries it that way); it is a
/// diagnostic representation, never a protocol status. Renaming a variant therefore changes a
/// serialized representation, which is why the spellings are pinned by a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

    /// Returns the category's stable spelling, which is also its serde representation.
    ///
    /// One spelling, two consumers: the serde form a golden fixture records and the `error_category`
    /// metric label a dashboard groups by are the same string, and neither may drift from the other.
    /// The test beside this impl asserts the two agree for every variant.
    ///
    /// The match is exhaustive and carries **no** fallback, on purpose. `#[non_exhaustive]` protects
    /// crates that match on this enum, not the crate that defines it: adding a variant here is a
    /// compile error until it is given a spelling, so no "unknown category" value can ever reach a
    /// dashboard. A wildcard would let a new variant ship as a generic label instead.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Upstream => "upstream",
            Self::Limit => "limit",
            Self::Internal => "internal",
            Self::Cancelled => "cancelled",
        }
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

    /// The serde spelling is a representation contract: a fixture, a diagnostic record and a
    /// canonical value all read the category by this name.
    #[test]
    fn every_category_keeps_its_spelling() {
        for (category, spelling) in [
            (ErrorCategory::Client, "client"),
            (ErrorCategory::Upstream, "upstream"),
            (ErrorCategory::Limit, "limit"),
            (ErrorCategory::Internal, "internal"),
            (ErrorCategory::Cancelled, "cancelled"),
        ] {
            let serialized = serde_json::to_string(&category).expect("a category serializes");
            assert_eq!(
                serialized,
                format!("\"{spelling}\""),
                "the serialized spelling of {category:?} must not drift"
            );
            let parsed: ErrorCategory =
                serde_json::from_str(&serialized).expect("a category parses back");
            assert_eq!(
                parsed, category,
                "the spelling must map back to its variant"
            );
            // The serde form and the label form are one contract, not two: a dashboard groups by
            // `error_category` and a fixture records the serde string, so a drift between them would
            // make the two disagree about what a category is called.
            assert_eq!(
                category.as_str(),
                spelling,
                "the metric label spelling of {category:?} must equal its serde spelling"
            );
        }
    }

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
