//! Why the reload pipeline stopped.
//!
//! The type is the pipeline's whole public failure vocabulary, so it lives in its own module: its set
//! is what a process exit path and every caller match on, while `reload` owns the state machine that
//! produces it.

use nico_config::ConfigSourceError;
use nico_core::{ErrorCategory, GenerationOverflow};

/// Why the pipeline could not keep a configuration in service.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReloadError {
    /// No snapshot could be published, so the process must not serve traffic.
    #[error("no configuration snapshot is available")]
    NotReady {
        /// Category of the failure that prevented the first publish.
        category: ErrorCategory,
    },
    /// The generation counter is exhausted, so the next publish could not be ordered.
    #[error("the configuration generation counter is exhausted")]
    GenerationOverflow {
        /// The overflow reported by `Generation::next`.
        #[source]
        source: GenerationOverflow,
    },
    /// A configuration source error that was surfaced instead of being retried.
    #[error("configuration source failed")]
    Source {
        /// The source failure.
        #[source]
        source: ConfigSourceError,
    },
}

impl ReloadError {
    /// Classifies the failure for the process exit path.
    ///
    /// A missing snapshot is classified by whatever prevented it: an unreachable source is an
    /// upstream failure, a rejected document is the writer's fault, and an exhausted counter is an
    /// internal invariant being hit.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::NotReady { category } => *category,
            Self::Source { source } => source.category(),
            Self::GenerationOverflow { .. } => ErrorCategory::Internal,
        }
    }
}

impl From<ConfigSourceError> for ReloadError {
    fn from(source: ConfigSourceError) -> Self {
        Self::Source { source }
    }
}

impl From<GenerationOverflow> for ReloadError {
    fn from(source: GenerationOverflow) -> Self {
        Self::GenerationOverflow { source }
    }
}

// Compiled-in proof that a reload error can cross `.await` and `tokio::spawn` boundaries.
const _: fn() = || {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<ReloadError>();
};
