//! Token usage, kept in two explicit buckets.
//!
//! A number a *provider reported* and a number the *gateway measured* are different facts. They
//! may disagree (a provider reports what it billed; the gateway can only count what crossed it), so
//! [`Usage`] keeps them apart instead of merging them into one struct whose provenance a reader has
//! to guess. Phase 1 defines the shape only; normalization and billing arrive later.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de, ser::SerializeMap};

/// The five token counts a provider can report and the gateway can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TokenKind {
    /// Tokens sent to the provider.
    Input,
    /// Tokens produced by the model.
    Output,
    /// Input tokens served from the provider's prompt cache.
    CacheRead,
    /// Input tokens written to the provider's prompt cache.
    CacheWrite,
    /// Output tokens spent on hidden reasoning.
    Reasoning,
}

/// Number of kinds in [`TokenKind::ALL`]; the storage width of [`UsageCounts`].
const TOKEN_KIND_COUNT: usize = 5;

impl TokenKind {
    /// Every kind, in serialization order.
    pub const ALL: [Self; TOKEN_KIND_COUNT] = [
        Self::Input,
        Self::Output,
        Self::CacheRead,
        Self::CacheWrite,
        Self::Reasoning,
    ];

    /// Returns the kind's position in [`TokenKind::ALL`].
    const fn index(self) -> usize {
        match self {
            Self::Input => 0,
            Self::Output => 1,
            Self::CacheRead => 2,
            Self::CacheWrite => 3,
            Self::Reasoning => 4,
        }
    }

    /// Returns the field name used by the canonical JSON representation.
    const fn name(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
            Self::CacheRead => "cache_read",
            Self::CacheWrite => "cache_write",
            Self::Reasoning => "reasoning",
        }
    }

    /// Returns the kind a canonical JSON field name denotes.
    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }
}

/// A set of token counts.
///
/// The counts are addressed through [`TokenKind`] rather than five accessors: the kinds are one
/// table, and a new count is one variant plus one match arm with the compiler pointing at every
/// consumer. Storage is fixed-size, so a count set is `Copy` and allocates nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageCounts {
    counts: [Option<u64>; TOKEN_KIND_COUNT],
}

impl UsageCounts {
    /// Builds an empty set of counts.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            counts: [None; TOKEN_KIND_COUNT],
        }
    }

    /// Returns the count for one kind.
    #[must_use]
    pub const fn get(&self, kind: TokenKind) -> Option<u64> {
        self.counts[kind.index()]
    }

    /// Sets the count for one kind.
    pub fn set(&mut self, kind: TokenKind, value: u64) {
        self.counts[kind.index()] = Some(value);
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
        self.counts.iter().all(Option::is_none)
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
