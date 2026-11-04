//! The configuration pipeline state that operators read.
//!
//! [`ConfigStatus`] is the observability surface of the reload pipeline until a metrics exporter
//! exists: every field maps to an operator question ("is a config live?", "is it stale?", "what
//! failed last?").

use crate::error::ErrorCategory;

/// Operator-visible state of the configuration pipeline.
///
/// The struct is `#[non_exhaustive]` so new fields can be added without breaking readers. All fields
/// are plain values: a status is a snapshot of counters and flags, never a handle to live state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ConfigStatus {
    /// Generation of the currently published snapshot; `0` before the first publish.
    pub generation: u64,
    /// Source-scoped revision sequence of the published snapshot.
    pub revision: Option<u64>,
    /// Hex digest of the published document.
    pub digest: Option<String>,
    /// Whether a snapshot is published and the pipeline can serve traffic.
    pub ready: bool,
    /// Whether the published snapshot came from the last-known-good cache.
    pub stale: bool,
    /// Whether the most recent reload failed while a last-known-good snapshot is still served.
    pub degraded: bool,
    /// Error category of the most recent failed reload.
    pub last_error_category: Option<ErrorCategory>,
    /// Unix timestamp, in seconds, of the most recent successful publish.
    pub last_reload_unix_secs: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_status_is_not_ready_and_not_stale() {
        let status = ConfigStatus::default();
        assert_eq!(status.generation, 0);
        assert_eq!(status.revision, None);
        assert_eq!(status.digest, None);
        assert!(!status.ready);
        assert!(!status.stale);
        assert!(!status.degraded);
        assert_eq!(status.last_error_category, None);
        assert_eq!(status.last_reload_unix_secs, None);
    }
}
