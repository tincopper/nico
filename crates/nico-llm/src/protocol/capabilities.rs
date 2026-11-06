//! What one side of an exchange can express.
//!
//! A capability is a *semantic* feature, not a vendor branch: the set is small and closed, so a
//! request that needs something a `(client protocol, provider)` pair does not share is refused once,
//! with the feature named, instead of being silently dropped on the way through.
//!
//! Plain text needs no capability; it is what a chat protocol does by definition. Everything beyond
//! it — a stream, tools, images, files, hidden reasoning, a structured response format — is a flag
//! one side either has or lacks. Publication intersects the two declared sets and keeps the result;
//! the request path compares the needs it derives from the canonical request against it.

use std::fmt;

/// One optional feature a client protocol or a provider adapter may support.
///
/// The set is deliberately small and semantic: these are the features a request can require, so a
/// capability the pair does not share is a refusal with a stable name instead of a silently dropped
/// field. A new capability is added here only when the canonical model gains a way to express it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Capability {
    /// Incremental, event-streamed responses.
    Streaming,
    /// Tool declarations, tool calls and tool results.
    Tools,
    /// Image content parts.
    Images,
    /// File content parts.
    Files,
    /// Hidden reasoning parts.
    Reasoning,
    /// A structured response format.
    ResponseFormat,
}

/// Number of kinds in [`Capability::ALL`]; the storage width of [`Capabilities`].
const CAPABILITY_COUNT: usize = 6;

// The bit set has one bit per capability, so the table must fit the `u8` it is stored in.
const _: () = assert!(CAPABILITY_COUNT <= u8::BITS as usize);

impl Capability {
    /// Every capability, in bit order.
    pub const ALL: [Self; CAPABILITY_COUNT] = [
        Self::Streaming,
        Self::Tools,
        Self::Images,
        Self::Files,
        Self::Reasoning,
        Self::ResponseFormat,
    ];

    /// Returns the capability's position in [`Capability::ALL`].
    const fn index(self) -> usize {
        match self {
            Self::Streaming => 0,
            Self::Tools => 1,
            Self::Images => 2,
            Self::Files => 3,
            Self::Reasoning => 4,
            Self::ResponseFormat => 5,
        }
    }

    /// Returns the bit this capability occupies.
    const fn mask(self) -> u8 {
        1 << self.index()
    }

    /// Returns the stable name used in logs and diagnostics.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Streaming => "streaming",
            Self::Tools => "tools",
            Self::Images => "images",
            Self::Files => "files",
            Self::Reasoning => "reasoning",
            Self::ResponseFormat => "response_format",
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// A set of [`Capability`] flags.
///
/// One value answers "what can this side express". Publication intersects the client codec's set with
/// the provider adapter's and the exchange keeps the result, so a request that needs something the
/// pair does not share is refused once, with the capability named, instead of failing later inside a
/// vendor encoder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Capabilities {
    bits: u8,
}

impl Capabilities {
    /// Builds a set with no capability.
    #[must_use]
    pub const fn none() -> Self {
        Self { bits: 0 }
    }

    /// Builds a set with every capability.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            bits: (1 << CAPABILITY_COUNT) - 1,
        }
    }

    /// Reports whether `capability` is set.
    #[must_use]
    pub const fn supports(self, capability: Capability) -> bool {
        self.bits & capability.mask() != 0
    }

    /// Returns a copy with `capability` set.
    #[must_use]
    pub const fn with(mut self, capability: Capability) -> Self {
        self.bits |= capability.mask();
        self
    }

    /// Returns a copy with `capability` cleared.
    #[must_use]
    pub const fn without(mut self, capability: Capability) -> Self {
        self.bits &= !capability.mask();
        self
    }

    /// Returns the capabilities both sets have.
    #[must_use]
    pub const fn intersection(self, other: Self) -> Self {
        Self {
            bits: self.bits & other.bits,
        }
    }

    /// Reports whether no capability is set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.bits == 0
    }

    /// Iterates the capabilities that are set, in [`Capability::ALL`] order.
    pub fn iter(self) -> impl Iterator<Item = Capability> {
        Capability::ALL
            .into_iter()
            .filter(move |capability| self.supports(*capability))
    }
}
