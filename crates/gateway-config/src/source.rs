//! The `ConfigSource` contract: how a source loads and watches one complete document.
//!
//! A source is deliberately narrow. It loads bytes, it reports changes, and it reports failures.
//! It never parses routes, never decides when to reload, and never touches a runtime snapshot:
//! de-duplication, debouncing, validation, compilation and publication all live in one pipeline in
//! `gateway-server`.
//!
//! Two rules shape every implementation:
//!
//! - **Whole documents only.** A watch emits [`ConfigEvent::Snapshot`] with the complete document
//!   or [`ConfigEvent::ResyncRequired`]; there is no incremental patch. A source that can only offer
//!   partial updates synthesizes a full document or asks for a resync.
//! - **Dropping the stream cancels the subscription.** No background task, file watcher or
//!   connection outlives the returned [`ConfigWatch`].

use std::{error::Error, pin::Pin};

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;
use gateway_core::{ErrorCategory, Revision};
use thiserror::Error;

/// A complete configuration document together with the revision it was read at.
///
/// The bytes are exactly what the source returned; no source normalizes, reformats or trims them.
/// The pipeline computes the digest with [`gateway_core::ConfigDigest::of`] so two sources can never
/// disagree about whether two documents are the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigVersion {
    revision: Revision,
    bytes: Bytes,
}

impl ConfigVersion {
    /// Creates a version from a source-scoped revision and the complete document bytes.
    #[must_use]
    pub fn new(revision: Revision, bytes: Bytes) -> Self {
        Self { revision, bytes }
    }

    /// Returns the source-scoped revision of this document.
    #[must_use]
    pub const fn revision(&self) -> Revision {
        self.revision
    }

    /// Returns the complete document bytes.
    #[must_use]
    pub const fn bytes(&self) -> &Bytes {
        &self.bytes
    }
}

/// One event from a configuration watch.
///
/// The enum is `#[non_exhaustive]` so a future capability (for example a source-reported staleness
/// flag) does not break exhaustive matches downstream. It carries no patch variant on purpose.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigEvent {
    /// A new complete document.
    Snapshot(ConfigVersion),
    /// The source cannot continue the current subscription; reload the whole document.
    ///
    /// Emitted after a compaction or gap, when a lagging consumer dropped events, or when the source
    /// reconnected in a way that cannot be proven incremental.
    ResyncRequired,
}

/// The stream a watch produces.
///
/// Bounded and cancellable: an implementation must not buffer unbounded history when the consumer
/// falls behind, and dropping the stream must stop whatever the source started.
pub type ConfigWatch =
    Pin<Box<dyn Stream<Item = Result<ConfigEvent, ConfigSourceError>> + Send + 'static>>;

/// Why a configuration source could not provide a document.
///
/// One variant per decision the caller makes, and never a message that carries document content:
/// the pipeline treats every variant as "no new snapshot", but it distinguishes the cause for logs,
/// alerting and readiness.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConfigSourceError {
    /// The source could not be reached or read: missing file, permission denied, disconnected.
    #[error("config source `{source_name}` is unavailable")]
    Unavailable {
        /// Source name, as returned by [`ConfigSource::name`].
        source_name: &'static str,
        /// Underlying cause, kept for logs. Never contains the document itself.
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
    /// The document is larger than the source's configured bound.
    #[error("config document exceeds the source limit: {size} bytes > {limit} bytes")]
    TooLarge {
        /// Actual document size in bytes.
        size: usize,
        /// Configured maximum size in bytes.
        limit: usize,
    },
    /// The document is not valid UTF-8.
    #[error("config document is not valid UTF-8")]
    NotUtf8,
    /// A revision arrived that is older than the one already accepted from this source.
    #[error("config revision went backwards: current {current}, incoming {incoming}")]
    RevisionRegression {
        /// Revision currently accepted.
        current: Revision,
        /// Revision reported by the source.
        incoming: Revision,
    },
    /// The watch cannot continue incrementally and needs a full reload.
    #[error("config watch requires a full document reload")]
    ResyncRequired,
}

impl ConfigSourceError {
    /// Classifies the failure for the edge layer.
    ///
    /// A source that cannot answer, or cannot keep streaming, is a failed dependency. An oversized
    /// document is a limit decision, malformed content is the writer's fault, and a regressing
    /// revision means a source broke its own ordering promise.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::Unavailable { .. } | Self::ResyncRequired => ErrorCategory::Upstream,
            Self::TooLarge { .. } => ErrorCategory::Limit,
            Self::NotUtf8 => ErrorCategory::Client,
            Self::RevisionRegression { .. } => ErrorCategory::Internal,
        }
    }
}

/// Errors compare by value so tests can assert the exact variant.
///
/// [`ConfigSourceError::Unavailable`] compares by source name alone: the boxed cause is an opaque
/// foreign error and is deliberately not part of the value's identity.
impl PartialEq for ConfigSourceError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Unavailable {
                    source_name: left, ..
                },
                Self::Unavailable {
                    source_name: right, ..
                },
            ) => left == right,
            (
                Self::TooLarge {
                    size: left_size,
                    limit: left_limit,
                },
                Self::TooLarge {
                    size: right_size,
                    limit: right_limit,
                },
            ) => left_size == right_size && left_limit == right_limit,
            (Self::NotUtf8, Self::NotUtf8) => true,
            (
                Self::RevisionRegression {
                    current: left_current,
                    incoming: left_incoming,
                },
                Self::RevisionRegression {
                    current: right_current,
                    incoming: right_incoming,
                },
            ) => left_current == right_current && left_incoming == right_incoming,
            (Self::ResyncRequired, Self::ResyncRequired) => true,
            _ => false,
        }
    }
}

impl Eq for ConfigSourceError {}

// Compiled-in proof that a source error can cross `.await` and `tokio::spawn` boundaries.
const _: fn() = || {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<ConfigSourceError>();
    assert_send_sync_static::<ConfigVersion>();
};

// A watch stream crosses task boundaries but is polled by one owner, so it is `Send` and not `Sync`.
const _: fn() = || {
    const fn assert_send_static<T: Send + 'static>() {}
    assert_send_static::<ConfigWatch>();
};

/// One authoritative configuration source in a process.
///
/// The trait is object-safe on purpose: the pipeline holds `Arc<dyn ConfigSource>` and never names a
/// concrete adapter, so adding a source does not change the reload pipeline.
///
/// # Contract
///
/// - [`ConfigSource::load`] returns the complete document together with the revision that follows it.
/// - [`ConfigSource::watch`] starts after the revision passed in `from`; an implementation whose
///   protocol resumes *at* a revision (etcd, for example) increments it inside the adapter.
///   `from = None` means "from now on", which is what a freshly loaded process wants.
/// - Only [`ConfigEvent::Snapshot`] and [`ConfigEvent::ResyncRequired`] are emitted. No patches.
/// - Output is bounded: a consumer that falls behind is told to resync instead of receiving
///   unbounded history.
/// - Dropping the stream cancels the subscription and leaves no task, watcher or connection behind.
/// - A source failure is reported as [`ConfigSourceError`], never as an empty document.
///
/// # Examples
///
/// A source is used behind a trait object, so the pipeline never depends on a concrete adapter:
///
/// ```
/// use std::sync::Arc;
///
/// use gateway_config::{ConfigSource, FakeConfigSource};
///
/// let source: Arc<dyn ConfigSource> = Arc::new(
///     FakeConfigSource::new("demo")
///         .push_load(Ok(FakeConfigSource::version_at(1, b"apiVersion: gateway/v1\n"))),
/// );
///
/// let runtime = tokio::runtime::Builder::new_current_thread()
///     .build()
///     .expect("runtime builds");
/// let version = runtime.block_on(source.load()).expect("scripted load succeeds");
///
/// assert_eq!(source.name(), "demo");
/// assert_eq!(version.revision().seq(), 1);
/// assert_eq!(version.bytes().as_ref(), b"apiVersion: gateway/v1\n");
/// ```
#[async_trait]
pub trait ConfigSource: Send + Sync {
    /// Returns the source name used in logs and in [`ConfigSourceError::Unavailable`].
    fn name(&self) -> &'static str;

    /// Reads the complete document as of this call.
    ///
    /// # Errors
    /// Returns [`ConfigSourceError::Unavailable`] when the source cannot be read,
    /// [`ConfigSourceError::TooLarge`] when the document exceeds the configured bound, or
    /// [`ConfigSourceError::NotUtf8`] when the document is not valid UTF-8.
    async fn load(&self) -> Result<ConfigVersion, ConfigSourceError>;

    /// Subscribes to changes after `from`, or from now on when `from` is `None`.
    ///
    /// # Errors
    /// Returns [`ConfigSourceError::Unavailable`] when the subscription cannot be established, for
    /// example because the watched entity no longer exists or the backend refuses the subscription.
    async fn watch(&self, from: Option<Revision>) -> Result<ConfigWatch, ConfigSourceError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    mod error {
        use super::*;

        #[test]
        fn too_large_maps_to_the_limit_category() {
            let error = ConfigSourceError::TooLarge { size: 10, limit: 5 };
            assert_eq!(error.category(), ErrorCategory::Limit);
            assert_eq!(
                error.to_string(),
                "config document exceeds the source limit: 10 bytes > 5 bytes"
            );
        }

        #[test]
        fn not_utf8_maps_to_the_client_category() {
            assert_eq!(ConfigSourceError::NotUtf8.category(), ErrorCategory::Client);
            assert_eq!(
                ConfigSourceError::NotUtf8.to_string(),
                "config document is not valid UTF-8"
            );
        }

        #[test]
        fn unavailable_maps_to_the_upstream_category_without_leaking_the_cause() {
            let error = ConfigSourceError::Unavailable {
                source_name: "local",
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "secret path",
                )),
            };
            assert_eq!(error.category(), ErrorCategory::Upstream);
            assert_eq!(error.to_string(), "config source `local` is unavailable");
            assert!(
                !error.to_string().contains("secret path"),
                "the display text must not repeat the underlying cause"
            );
        }

        #[test]
        fn revision_regression_maps_to_internal() {
            let error = ConfigSourceError::RevisionRegression {
                current: Revision::comparable(7),
                incoming: Revision::comparable(6),
            };
            assert_eq!(error.category(), ErrorCategory::Internal);
            assert_eq!(
                error.to_string(),
                "config revision went backwards: current 7/comparable, incoming 6/comparable"
            );
        }

        #[test]
        fn resync_required_maps_to_upstream() {
            assert_eq!(
                ConfigSourceError::ResyncRequired.category(),
                ErrorCategory::Upstream
            );
        }

        #[test]
        fn errors_compare_by_variant_and_payload() {
            assert_eq!(ConfigSourceError::NotUtf8, ConfigSourceError::NotUtf8);
            assert_eq!(
                ConfigSourceError::TooLarge { size: 10, limit: 5 },
                ConfigSourceError::TooLarge { size: 10, limit: 5 }
            );
            assert_ne!(
                ConfigSourceError::TooLarge { size: 10, limit: 5 },
                ConfigSourceError::TooLarge { size: 11, limit: 5 }
            );
            assert_ne!(
                ConfigSourceError::NotUtf8,
                ConfigSourceError::ResyncRequired
            );
        }

        #[test]
        fn unavailable_errors_compare_by_source_name() {
            let first = ConfigSourceError::Unavailable {
                source_name: "local",
                source: Box::new(std::io::Error::other("first")),
            };
            let second = ConfigSourceError::Unavailable {
                source_name: "local",
                source: Box::new(std::io::Error::other("second")),
            };
            let other_source = ConfigSourceError::Unavailable {
                source_name: "etcd",
                source: Box::new(std::io::Error::other("first")),
            };
            assert_eq!(first, second);
            assert_ne!(first, other_source);
        }
    }

    mod version {
        use super::*;

        #[test]
        fn version_exposes_the_revision_and_the_exact_bytes() {
            let version = ConfigVersion::new(Revision::comparable(3), Bytes::from_static(b"doc"));
            assert_eq!(version.revision(), Revision::comparable(3));
            assert_eq!(version.bytes().as_ref(), b"doc");
        }
    }

    mod object_safety {
        use std::sync::Arc;

        use super::*;
        use crate::{LocalConfig, LocalFileSource};

        #[test]
        fn every_source_is_usable_behind_a_trait_object() {
            // The pipeline stores one `Arc<dyn ConfigSource>`; this test is the compile-time proof
            // that no implementation depends on being named.
            let _fake: Arc<dyn ConfigSource> = Arc::new(crate::FakeConfigSource::new("fake"));
            let _local: Arc<dyn ConfigSource> = Arc::new(
                LocalFileSource::new(LocalConfig::new("resources.yaml"))
                    .expect("configuration is valid"),
            );
        }
    }
}
