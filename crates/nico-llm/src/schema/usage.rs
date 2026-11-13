//! Token usage, kept in two explicit buckets.
//!
//! A number a *provider reported* and a number the *gateway measured* are different facts. They
//! may disagree (a provider reports what it billed; the gateway can only count what crossed it), so
//! [`Usage`] keeps them apart instead of merging them into one struct whose provenance a reader has
//! to guess. Phase 1 defines the shape only; normalization and billing arrive later.

use std::collections::BTreeMap;

/// The five token counts a provider can report and the gateway can carry.
///
/// The vocabulary lives in `nico_http::llm` because the request path emits a token counter without
/// depending on this crate. Re-exported here so a provider adapter — which does depend on it — names
/// the kinds through the crate that owns the canonical model.
pub use nico_http::llm::TokenKind;
pub(crate) use nico_http::llm::TokenUsage;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de, ser::SerializeMap};

/// A set of token counts, in the canonical model's serializable form.
///
/// The storage and the accessors are [`TokenUsage`]'s — the same shape the request path reads when it
/// emits a counter — and this type adds the two things a *document* needs: a serde implementation that
/// writes a map of names, and the `Copy`-friendly builder the adapters use. One set of counts, two
/// views of it, no second definition to drift.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageCounts {
    counts: TokenUsage,
}

impl UsageCounts {
    /// Builds an empty set of counts.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            counts: TokenUsage::new(),
        }
    }

    /// Returns the count for one kind.
    #[must_use]
    pub const fn get(&self, kind: TokenKind) -> Option<u64> {
        self.counts.get(kind)
    }

    /// Sets the count for one kind.
    pub fn set(&mut self, kind: TokenKind, value: u64) {
        self.counts.set(kind, value);
    }

    /// Returns a copy with one count set.
    #[must_use]
    pub fn with(mut self, kind: TokenKind, value: u64) -> Self {
        self.set(kind, value);
        self
    }

    /// Reports whether no count is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }

    /// Returns the shared shape a consumer outside this crate reads.
    #[must_use]
    pub const fn as_usage(&self) -> &TokenUsage {
        &self.counts
    }

    /// Returns every reported kind and its count, in [`TokenKind::ALL`] order.
    #[must_use]
    pub fn reported_pairs(&self) -> Vec<(TokenKind, u64)> {
        self.counts.reported()
    }
}

impl From<TokenUsage> for UsageCounts {
    fn from(counts: TokenUsage) -> Self {
        Self { counts }
    }
}

impl Serialize for UsageCounts {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        for kind in TokenKind::ALL {
            if let Some(value) = self.get(kind) {
                map.serialize_entry(kind.name(), &value)?;
            }
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for UsageCounts {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = BTreeMap::<String, u64>::deserialize(deserializer)?;
        let mut counts = Self::new();
        for (name, value) in raw {
            let Some(kind) = TokenKind::from_name(&name) else {
                return Err(de::Error::custom(format_args!(
                    "unknown usage token kind `{name}`"
                )));
            };
            counts.set(kind, value);
        }
        Ok(counts)
    }
}

/// Token usage split by provenance.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reported: Option<UsageCounts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    measured: Option<UsageCounts>,
}

impl Usage {
    /// Builds a usage value from provider-reported counts.
    #[must_use]
    pub const fn reported(counts: UsageCounts) -> Self {
        Self {
            reported: Some(counts),
            measured: None,
        }
    }

    /// Builds a usage value from gateway-measured counts.
    #[must_use]
    pub const fn measured(counts: UsageCounts) -> Self {
        Self {
            reported: None,
            measured: Some(counts),
        }
    }

    /// Builds a usage value carrying both provenance buckets.
    #[must_use]
    pub const fn both(reported: UsageCounts, measured: UsageCounts) -> Self {
        Self {
            reported: Some(reported),
            measured: Some(measured),
        }
    }

    /// Returns the counts the provider reported, when it reported any.
    #[must_use]
    pub const fn reported_counts(&self) -> Option<&UsageCounts> {
        self.reported.as_ref()
    }

    /// Returns the counts the gateway measured, when it measured any.
    #[must_use]
    pub const fn measured_counts(&self) -> Option<&UsageCounts> {
        self.measured.as_ref()
    }

    /// Reports whether neither bucket is present.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.reported.is_none() && self.measured.is_none()
    }
}
