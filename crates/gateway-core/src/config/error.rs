//! Reasons a configuration document is rejected before it can be published.
//!
//! [`DocumentError`] is the only error type produced by [`Document::parse`](super::Document::parse)
//! and it classifies every failure through [`DocumentError::category`], so the edge maps a category
//! instead of matching each variant.

use std::borrow::Cow;

use crate::error::ErrorCategory;

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
    /// The document uses a YAML anchor or alias, which would let one byte range expand into an
    /// arbitrarily large graph after the size check has already passed.
    #[error("document must not use YAML anchors or aliases")]
    AliasForbidden,
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
    /// The decoded document has more YAML nodes than the schema bound allows.
    #[error("document exceeds the node bound: {count} nodes > {limit} nodes")]
    TooManyNodes {
        /// Counted nodes, at least `limit + 1`.
        count: usize,
        /// Configured maximum node count.
        limit: usize,
    },
    /// The decoded document nests deeper than the schema bound allows.
    #[error("document exceeds the depth bound: depth {depth} > {limit}")]
    TooDeep {
        /// Depth at which the bound was crossed.
        depth: usize,
        /// Configured maximum depth, where the root sits at depth `1`.
        limit: usize,
    },
    /// A collection holds more entries than the schema bound allows.
    #[error("too many {kind}: {count} > {limit}")]
    TooMany {
        /// Collection kind: `routes`, `upstreams` or `providers`.
        kind: &'static str,
        /// Number of entries found.
        count: usize,
        /// Configured maximum entry count.
        limit: usize,
    },
    /// A route names neither an upstream nor a provider.
    #[error("route {id} has no target: set exactly one of `upstream` or `provider`")]
    RouteTargetMissing {
        /// The route that is missing a target.
        id: String,
    },
    /// A route names both an upstream and a provider.
    #[error("route {id} has two targets: set exactly one of `upstream` or `provider`")]
    RouteTargetAmbiguous {
        /// The route that names two targets.
        id: String,
    },
    /// A route references a name that no entry defines.
    #[error("unknown {kind} reference: {name}")]
    ReferenceUnknown {
        /// Referenced collection kind: `upstream` or `provider`.
        kind: &'static str,
        /// The name that resolved to no entry.
        name: String,
    },
    /// A name that must be unique appears twice.
    #[error("duplicate {kind}: {id}")]
    DuplicateId {
        /// Identifier kind: `route`, `upstream`, `provider` or `route-match`.
        kind: &'static str,
        /// The repeated identifier, or the colliding match combination.
        id: String,
    },
    /// A field is syntactically or structurally invalid.
    ///
    /// `reason` borrows when the rule is a fixed sentence and owns only when it quotes a limit, so
    /// a message can never drift from the constant the check applied.
    #[error("invalid field {field}: {reason}")]
    InvalidField {
        /// Dotted field path, for example `routes[].path_prefix`.
        field: &'static str,
        /// Why the value is rejected, derived from the rule that rejected it.
        reason: Cow<'static, str>,
    },
}

impl DocumentError {
    /// Classifies the failure for the edge layer.
    ///
    /// A bad document is the caller's fault, except for bounds: an oversized document, an
    /// out-of-range limit, too many entries or too much nesting is a resource limit decision.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::TooLarge { .. }
            | Self::LimitOutOfRange { .. }
            | Self::TooManyNodes { .. }
            | Self::TooDeep { .. }
            | Self::TooMany { .. } => ErrorCategory::Limit,
            Self::NotUtf8
            | Self::AliasForbidden
            | Self::WrongApiVersion { .. }
            | Self::Yaml { .. }
            | Self::RouteTargetMissing { .. }
            | Self::RouteTargetAmbiguous { .. }
            | Self::ReferenceUnknown { .. }
            | Self::DuplicateId { .. }
            | Self::InvalidField { .. } => ErrorCategory::Client,
        }
    }
}

// Compiled-in proof that a document error can cross `.await` and `tokio::spawn` boundaries.
const _: fn() = || {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<DocumentError>();
};
