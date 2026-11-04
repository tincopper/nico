//! Identifiers for a published configuration: generation, revision and digest.
//!
//! The three types answer three different questions and must not be confused: [`Generation`] is a
//! process-local counter owned by the publisher, [`Revision`] is an opaque version scoped to one
//! configuration source, and [`ConfigDigest`] proves whether two documents are byte-identical.
//! Keeping them here separates authorship of the document model from versioning of the snapshots
//! built from it.

use std::fmt;

use sha2::{Digest as _, Sha256};

/// A monotonic snapshot generation assigned by the publisher.
///
/// Generation is local to one process: it starts at [`Generation::ZERO`] and increases by exactly
/// one per successful publish. It never follows a source revision backwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Generation(u64);

impl Generation {
    /// The generation before any snapshot has been published.
    pub const ZERO: Self = Self(0);

    /// Wraps a raw counter value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the raw counter value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Returns the generation that follows this one.
    ///
    /// # Errors
    /// Returns [`GenerationOverflow`] when this generation is `u64::MAX`, so a publisher can fail
    /// instead of wrapping to a generation that looks older than the current one.
    pub const fn next(self) -> Result<Self, GenerationOverflow> {
        match self.0.checked_add(1) {
            Some(value) => Ok(Self(value)),
            None => Err(GenerationOverflow { current: self.0 }),
        }
    }
}

/// Reported when [`Generation::next`] is called on `u64::MAX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("generation counter cannot advance past {current}")]
pub struct GenerationOverflow {
    current: u64,
}

impl GenerationOverflow {
    /// Returns the counter value that could not be incremented.
    #[must_use]
    pub const fn current(self) -> u64 {
        self.current
    }
}

/// How a [`Revision`]'s sequence number may be compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RevisionOrdering {
    /// Sequence numbers from one source are totally ordered; `seq` may be compared.
    Comparable,
    /// Sequence numbers carry no ordering; only equality is meaningful.
    EqualityOnly,
}

/// A source-scoped, opaque configuration revision.
///
/// A revision is only meaningful together with the source that produced it, and two revisions from
/// different sources must never be compared. Within one source, [`RevisionOrdering`] states whether
/// the sequence numbers can prove "older" or only "different"; for example a content hash is
/// equality-only and must not be sorted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Revision {
    seq: u64,
    ordering: RevisionOrdering,
}

impl Revision {
    /// Creates a revision whose sequence numbers are comparable within one source.
    #[must_use]
    pub const fn comparable(seq: u64) -> Self {
        Self {
            seq,
            ordering: RevisionOrdering::Comparable,
        }
    }

    /// Creates a revision whose sequence numbers are only comparable for equality.
    #[must_use]
    pub const fn equality_only(seq: u64) -> Self {
        Self {
            seq,
            ordering: RevisionOrdering::EqualityOnly,
        }
    }

    /// Returns the source-scoped sequence number.
    #[must_use]
    pub const fn seq(self) -> u64 {
        self.seq
    }

    /// Returns how the sequence number may be compared.
    #[must_use]
    pub const fn ordering(self) -> RevisionOrdering {
        self.ordering
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ordering = match self.ordering {
            RevisionOrdering::Comparable => "comparable",
            RevisionOrdering::EqualityOnly => "equality-only",
        };
        write!(formatter, "{}/{ordering}", self.seq)
    }
}

/// A SHA-256 digest of a complete configuration document.
///
/// The digest is computed by the pipeline, not by individual sources, so two adapters can never
/// disagree about whether two documents are the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConfigDigest([u8; 32]);

impl ConfigDigest {
    /// Computes the digest of a complete document.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        Self(hasher.finalize().into())
    }

    /// Returns the digest as 64 lowercase hexadecimal characters.
    #[must_use]
    pub fn hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut hex = String::with_capacity(64);
        for byte in self.0 {
            hex.push(char::from(HEX[usize::from(byte >> 4)]));
            hex.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        hex
    }
}

impl fmt::Display for ConfigDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Arbitrary stable bytes: the digest tests only need one fixed input.
    const VALID: &str = "\
apiVersion: gateway/v1
limits:
  max_request_body_bytes: 1048576
  request_timeout_ms: 30000
";

    mod digest {
        use super::*;

        #[test]
        fn digest_is_stable_for_the_same_bytes() {
            assert_eq!(
                ConfigDigest::of(VALID.as_bytes()),
                ConfigDigest::of(VALID.as_bytes())
            );
        }

        #[test]
        fn digest_differs_for_different_bytes() {
            assert_ne!(ConfigDigest::of(b"a"), ConfigDigest::of(b"b"));
        }

        #[test]
        fn digest_hex_is_64_lowercase_characters() {
            let hex = ConfigDigest::of(VALID.as_bytes()).hex();
            assert_eq!(hex.len(), 64);
            assert!(
                hex.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            );
        }

        #[test]
        fn digest_display_matches_hex() {
            let digest = ConfigDigest::of(VALID.as_bytes());
            assert_eq!(digest.to_string(), digest.hex());
        }
    }

    mod generation {
        use super::*;

        #[test]
        fn next_increments_the_counter() {
            let next = Generation::ZERO.next().expect("zero can advance");
            assert_eq!(next, Generation::new(1));
            assert_eq!(next.get(), 1);
        }

        #[test]
        fn next_reports_overflow_instead_of_wrapping() {
            let error = Generation::new(u64::MAX)
                .next()
                .expect_err("counter is exhausted");
            assert_eq!(error.current(), u64::MAX);
        }
    }

    mod revision {
        use super::*;

        #[test]
        fn comparable_revision_exposes_its_sequence_and_ordering() {
            let revision = Revision::comparable(7);
            assert_eq!(revision.seq(), 7);
            assert_eq!(revision.ordering(), RevisionOrdering::Comparable);
        }

        #[test]
        fn equality_only_revision_does_not_claim_order() {
            let revision = Revision::equality_only(7);
            assert_eq!(revision.seq(), 7);
            assert_eq!(revision.ordering(), RevisionOrdering::EqualityOnly);
        }

        #[test]
        fn revisions_from_different_sources_only_compare_by_value() {
            let comparable = Revision::comparable(7);
            let equality_only = Revision::equality_only(7);
            assert_ne!(comparable, equality_only);
            assert_eq!(comparable, Revision::comparable(7));
        }
    }
}
