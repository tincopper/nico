//! Versioned configuration document model: generation, revision, digest and parsing.
//!
//! A document is an untrusted input boundary: it is size-checked, decoded as UTF-8, parsed with
//! `deny_unknown_fields`, and range-checked before it can become part of a published snapshot.

use std::fmt;

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::error::ErrorCategory;

/// The only `apiVersion` this build understands.
pub const API_VERSION: &str = "gateway/v1";

/// Largest `limits.max_request_body_bytes` accepted by [`Document::parse`], in bytes (8 MiB).
const MAX_REQUEST_BODY_BYTES: u32 = 8 * 1024 * 1024;
/// Largest `limits.request_timeout_ms` accepted by [`Document::parse`], in milliseconds.
const MAX_REQUEST_TIMEOUT_MS: u32 = 600_000;

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

/// Why a configuration document was rejected before it could be published.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DocumentError {
    /// The document is larger than the configured bound.
    #[error("document exceeds the size limit: {size} bytes > {limit} bytes")]
    TooLarge {
        /// Actual document size in bytes.
        size: usize,
        /// Configured maximum size in bytes.
        limit: usize,
    },
    /// The document is not valid UTF-8.
    #[error("document is not valid UTF-8")]
    NotUtf8,
    /// The document declares an `apiVersion` this build does not understand.
    #[error("unsupported apiVersion: {found}")]
    WrongApiVersion {
        /// The `apiVersion` value found in the document.
        found: String,
    },
    /// The document is not valid YAML, or violates the document schema.
    #[error("document is not valid configuration YAML")]
    Yaml {
        /// Parser or schema error, kept as the source for logging.
        #[source]
        source: serde_norway::Error,
    },
    /// A numeric limit is outside the range this build accepts.
    #[error("limit is out of range: {field} = {value}")]
    LimitOutOfRange {
        /// Dotted field path, for example `limits.request_timeout_ms`.
        field: &'static str,
        /// Rejected value.
        value: u64,
    },
}

impl DocumentError {
    /// Classifies the failure for the edge layer.
    ///
    /// A bad document is the caller's fault, except for bounds: an oversized document or an
    /// out-of-range limit is a resource limit decision.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::TooLarge { .. } | Self::LimitOutOfRange { .. } => ErrorCategory::Limit,
            Self::NotUtf8 | Self::WrongApiVersion { .. } | Self::Yaml { .. } => {
                ErrorCategory::Client
            }
        }
    }
}

// Compiled-in proof that a document error can cross `.await` and `tokio::spawn` boundaries.
const _: fn() = || {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<DocumentError>();
};

/// Bounds applied while parsing a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseLimits {
    max_bytes: usize,
}

impl ParseLimits {
    /// Creates parse limits from the maximum accepted document size.
    #[must_use]
    pub const fn new(max_bytes: usize) -> Self {
        Self { max_bytes }
    }

    /// Returns the maximum accepted document size in bytes.
    #[must_use]
    pub const fn max_bytes(self) -> usize {
        self.max_bytes
    }
}

/// A validated gateway resources document.
///
/// Fields stay private and the type implements neither [`serde::Deserialize`] nor a public
/// constructor: a `Document` can only be built by [`Document::parse`], so every instance has passed
/// size, encoding, schema and range validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    api_version: String,
    limits: Limits,
}

/// The document exactly as written, before any validation.
///
/// Deserialization goes through this private type so that no public `Deserialize` implementation can
/// build a [`Document`] while bypassing [`Document::parse`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDocument {
    #[serde(rename = "apiVersion")]
    api_version: String,
    limits: RawLimits,
}

impl Document {
    /// Parses and validates a complete resources document.
    ///
    /// Validation order is fixed: size limit, UTF-8, YAML syntax and schema (unknown fields are
    /// rejected), `apiVersion`, then limit ranges.
    ///
    /// # Errors
    /// - [`DocumentError::TooLarge`] when `bytes` is longer than `limits.max_bytes`.
    /// - [`DocumentError::NotUtf8`] when `bytes` is not valid UTF-8.
    /// - [`DocumentError::Yaml`] when the document is not YAML or has unknown or missing fields.
    /// - [`DocumentError::WrongApiVersion`] when `apiVersion` is not [`API_VERSION`].
    /// - [`DocumentError::LimitOutOfRange`] when a limit is outside its accepted range.
    ///
    /// # Examples
    ///
    /// ```
    /// use gateway_core::{Document, ParseLimits};
    ///
    /// let bytes = "\
    /// apiVersion: gateway/v1
    /// limits:
    ///   max_request_body_bytes: 1048576
    ///   request_timeout_ms: 30000
    /// ";
    /// let document = Document::parse(bytes.as_bytes(), ParseLimits::new(1024 * 1024))
    ///     .expect("fixture is valid");
    /// assert_eq!(document.limits().request_timeout_ms(), 30_000);
    /// ```
    ///
    /// A wrong `apiVersion` is rejected rather than guessed:
    ///
    /// ```
    /// use gateway_core::{Document, ErrorCategory, ParseLimits};
    ///
    /// let bytes = "\
    /// apiVersion: gateway/v2
    /// limits:
    ///   max_request_body_bytes: 1024
    ///   request_timeout_ms: 1000
    /// ";
    /// let error = Document::parse(bytes.as_bytes(), ParseLimits::new(1024))
    ///     .expect_err("apiVersion is unknown");
    /// assert_eq!(error.category(), ErrorCategory::Client);
    /// ```
    pub fn parse(bytes: &[u8], limits: ParseLimits) -> Result<Self, DocumentError> {
        if bytes.len() > limits.max_bytes {
            return Err(DocumentError::TooLarge {
                size: bytes.len(),
                limit: limits.max_bytes,
            });
        }

        let text = std::str::from_utf8(bytes).map_err(|_| DocumentError::NotUtf8)?;
        let raw: RawDocument =
            serde_norway::from_str(text).map_err(|source| DocumentError::Yaml { source })?;

        if raw.api_version != API_VERSION {
            return Err(DocumentError::WrongApiVersion {
                found: raw.api_version,
            });
        }

        let limits = Limits {
            max_request_body_bytes: raw.limits.max_request_body_bytes,
            request_timeout_ms: raw.limits.request_timeout_ms,
        };
        limits.validate()?;

        Ok(Self {
            api_version: raw.api_version,
            limits,
        })
    }

    /// Returns the declared `apiVersion`, always [`API_VERSION`] for a parsed document.
    #[must_use]
    pub fn api_version(&self) -> &str {
        &self.api_version
    }

    /// Returns the validated global limits.
    #[must_use]
    pub const fn limits(&self) -> Limits {
        self.limits
    }
}

/// Global limits applied to every request.
///
/// Obtainable only through [`Document::limits`], so the values it carries have already passed range
/// validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    max_request_body_bytes: u32,
    request_timeout_ms: u32,
}

/// The `limits` section exactly as written, before range validation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLimits {
    max_request_body_bytes: u32,
    request_timeout_ms: u32,
}

impl Limits {
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
    fn validate(self) -> Result<(), DocumentError> {
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

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const VALID: &str = "\
apiVersion: gateway/v1
limits:
  max_request_body_bytes: 1048576
  request_timeout_ms: 30000
";

    fn parse_valid() -> Document {
        Document::parse(VALID.as_bytes(), ParseLimits::new(1024 * 1024)).expect("fixture is valid")
    }

    /// Builds a schema-valid document with the requested values.
    ///
    /// The literal starts each line in column 0 on purpose: a `\`-continued literal would strip
    /// the indentation that makes `max_request_body_bytes` a child of `limits`.
    fn yaml_document(api_version: &str, max_request_body_bytes: u32, timeout_ms: u32) -> String {
        format!(
            "\
apiVersion: {api_version}
limits:
  max_request_body_bytes: {max_request_body_bytes}
  request_timeout_ms: {timeout_ms}
"
        )
    }

    mod parse {
        use super::*;

        #[test]
        fn parse_accepts_a_valid_document() {
            let document = parse_valid();
            assert_eq!(document.api_version(), API_VERSION);
            assert_eq!(document.limits().max_request_body_bytes(), 1_048_576);
            assert_eq!(document.limits().request_timeout_ms(), 30_000);
        }

        #[test]
        fn parse_rejects_unknown_fields() {
            let mut document = yaml_document(API_VERSION, 1024, 1000);
            document.push_str("routes: []\n");
            let error = Document::parse(document.as_bytes(), ParseLimits::new(4096))
                .expect_err("unknown field");
            assert!(
                matches!(error, DocumentError::Yaml { .. }),
                "expected Yaml error, got {error:?}"
            );
            assert_eq!(error.category(), ErrorCategory::Client);
        }

        #[test]
        fn parse_rejects_unknown_limit_fields() {
            let document = "\
apiVersion: gateway/v1
limits:
  max_request_body_bytes: 1024
  request_timeout_ms: 1000
  max_connections: 10
";
            let error = Document::parse(document.as_bytes(), ParseLimits::new(4096))
                .expect_err("unknown field");
            assert!(
                matches!(error, DocumentError::Yaml { .. }),
                "expected Yaml error, got {error:?}"
            );
        }

        #[test]
        fn parse_rejects_wrong_api_version() {
            let document = yaml_document("gateway/v2", 1024, 1000);
            match Document::parse(document.as_bytes(), ParseLimits::new(4096)) {
                Err(DocumentError::WrongApiVersion { found }) => {
                    assert_eq!(found, "gateway/v2");
                }
                other => panic!("expected WrongApiVersion, got {other:?}"),
            }
        }

        #[test]
        fn parse_rejects_document_larger_than_the_limit() {
            let bytes = VALID.as_bytes();
            match Document::parse(bytes, ParseLimits::new(bytes.len() - 1)) {
                Err(DocumentError::TooLarge { size, limit }) => {
                    assert_eq!(size, bytes.len());
                    assert_eq!(limit, bytes.len() - 1);
                }
                other => panic!("expected TooLarge, got {other:?}"),
            }
        }

        #[test]
        fn parse_rejects_non_utf8_bytes() {
            let bytes = [b'a', 0xff, b'b'];
            let error = Document::parse(&bytes, ParseLimits::new(16)).expect_err("not utf-8");
            assert!(
                matches!(error, DocumentError::NotUtf8),
                "expected NotUtf8, got {error:?}"
            );
            assert_eq!(error.category(), ErrorCategory::Client);
        }

        #[test]
        fn parse_rejects_request_body_limit_below_one() {
            let document = yaml_document(API_VERSION, 0, 1000);
            match Document::parse(document.as_bytes(), ParseLimits::new(4096)) {
                Err(DocumentError::LimitOutOfRange { field, value }) => {
                    assert_eq!(field, "limits.max_request_body_bytes");
                    assert_eq!(value, 0);
                }
                other => panic!("expected LimitOutOfRange, got {other:?}"),
            }
        }

        #[test]
        fn parse_rejects_request_body_limit_above_eight_mib() {
            let document = yaml_document(API_VERSION, 8_388_609, 1000);
            let error = Document::parse(document.as_bytes(), ParseLimits::new(4096))
                .expect_err("limit too big");
            assert!(
                matches!(
                    error,
                    DocumentError::LimitOutOfRange {
                        field: "limits.max_request_body_bytes",
                        value: 8_388_609
                    }
                ),
                "expected LimitOutOfRange, got {error:?}"
            );
            assert_eq!(error.category(), ErrorCategory::Limit);
        }

        #[test]
        fn parse_rejects_timeout_above_ten_minutes() {
            let document = yaml_document(API_VERSION, 1024, 600_001);
            match Document::parse(document.as_bytes(), ParseLimits::new(4096)) {
                Err(DocumentError::LimitOutOfRange { field, value }) => {
                    assert_eq!(field, "limits.request_timeout_ms");
                    assert_eq!(value, 600_001);
                }
                other => panic!("expected LimitOutOfRange, got {other:?}"),
            }
        }

        #[test]
        fn parse_rejects_an_empty_document() {
            let error = Document::parse(b"", ParseLimits::new(16)).expect_err("empty document");
            assert!(
                matches!(error, DocumentError::Yaml { .. }),
                "expected Yaml error, got {error:?}"
            );
        }

        #[test]
        fn parse_rejects_a_missing_limits_section() {
            let error = Document::parse(b"apiVersion: gateway/v1\n", ParseLimits::new(4096))
                .expect_err("missing limits");
            assert!(
                matches!(error, DocumentError::Yaml { .. }),
                "expected Yaml error, got {error:?}"
            );
        }

        proptest! {
            #[test]
            fn parse_never_panics_on_arbitrary_input(
                bytes in proptest::collection::vec(any::<u8>(), 0..4096),
                max_bytes in 0usize..8192,
            ) {
                // The point of the property is that no input makes the parser panic.
                let _ = Document::parse(&bytes, ParseLimits::new(max_bytes));
            }
        }
    }

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
