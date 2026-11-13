//! Token counts, by the five kinds a provider bills.
//!
//! This is the one piece of accounting vocabulary the data plane holds, and it holds it for a
//! structural reason: the front-end that produces the counts is `nico-llm`'s, while the place that
//! records them (the request record and its metrics) is this crate's. Keeping the kind list here means
//! the request path can emit a token counter without depending on a crate that understands providers.
//!
//! The list is **closed** on purpose. A provider reports tokens it billed, and these are the buckets a
//! bill is made of; a sixth kind is a change to what "usage" means, not a new field. The names are the
//! metric label values and the log field suffixes, so one spelling serves both.
//!
//! The counts are `Option<u64>` per kind rather than a default of zero: "the provider said nothing"
//! and "the provider said zero" are different facts, and a consumer that averaged over them would be
//! computing something nobody asked for.

/// One bucket of a provider's bill.
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

/// Number of kinds in [`TokenKind::ALL`]; the storage width of [`TokenUsage`].
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

    /// Returns the name this kind is logged and counted under.
    ///
    /// One spelling for the metric label and the log field suffix: a dashboard and a log line name a
    /// bucket the same way.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
            Self::CacheRead => "cache_read",
            Self::CacheWrite => "cache_write",
            Self::Reasoning => "reasoning",
        }
    }

    /// Returns the kind a name denotes.
    ///
    /// The inverse of [`TokenKind::name`], used where a document carries the names — the canonical
    /// usage shape is serialized as a map keyed by them.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }
}

/// A set of provider-reported counts.
///
/// Addressed through [`TokenKind`] rather than five accessors: the kinds are one table, and a new
/// count is one variant plus one match arm with the compiler pointing at every consumer. Storage is
/// fixed-size, so a set is `Copy` and allocates nothing on the request path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    counts: [Option<u64>; TOKEN_KIND_COUNT],
}

impl TokenUsage {
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

    /// Returns every reported kind and its count, in [`TokenKind::ALL`] order.
    ///
    /// The iteration a consumer that emits one sample per bucket needs, so none of them re-implements
    /// the "skip the unreported kinds" rule and gets it wrong.
    #[must_use]
    pub fn reported(&self) -> Vec<(TokenKind, u64)> {
        TokenKind::ALL
            .into_iter()
            .filter_map(|kind| self.get(kind).map(|count| (kind, count)))
            .collect()
    }
}

#[cfg(test)]
mod tests;
