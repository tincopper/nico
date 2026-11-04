//! Immutable runtime snapshots and their atomic publication handle.
//!
//! A publisher builds a complete [`RuntimeSnapshot`] off the request path and swaps it in with
//! [`SnapshotHandle::store`]. A reader calls [`SnapshotHandle::current`] once per request and keeps
//! the returned `Arc` for its whole lifetime, so a reload can never mutate state under an in-flight
//! request.

use std::{
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

use arc_swap::ArcSwapOption;

use crate::{
    config::Document,
    error::ErrorCategory,
    status::ConfigStatus,
    version::{ConfigDigest, Generation, Revision},
};

/// A fully validated, immutable configuration snapshot.
///
/// The snapshot owns everything a request needs for one generation; nothing in it is mutated after
/// construction.
#[derive(Debug)]
pub struct RuntimeSnapshot {
    generation: Generation,
    revision: Revision,
    digest: ConfigDigest,
    document: Document,
    source: &'static str,
}

impl RuntimeSnapshot {
    /// Creates a snapshot from a document that has already passed [`Document::parse`].
    #[must_use]
    pub fn new(
        generation: Generation,
        revision: Revision,
        digest: ConfigDigest,
        document: Document,
        source: &'static str,
    ) -> Self {
        Self {
            generation,
            revision,
            digest,
            document,
            source,
        }
    }

    /// Returns the generation this snapshot was published as.
    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    /// Returns the source-scoped revision this snapshot was built from.
    #[must_use]
    pub const fn revision(&self) -> Revision {
        self.revision
    }

    /// Returns the digest of the document bytes.
    #[must_use]
    pub const fn digest(&self) -> ConfigDigest {
        self.digest
    }

    /// Returns the validated document.
    #[must_use]
    pub const fn document(&self) -> &Document {
        &self.document
    }

    /// Returns the name of the configuration source that produced the snapshot.
    #[must_use]
    pub const fn source(&self) -> &'static str {
        self.source
    }
}

/// The single publication point for the current runtime snapshot.
///
/// Reads are lock-free; the status mutex is only held for the duration of a synchronous status
/// update, never across an `.await`.
#[derive(Debug)]
pub struct SnapshotHandle {
    current: ArcSwapOption<RuntimeSnapshot>,
    status: Mutex<ConfigStatus>,
}

impl SnapshotHandle {
    /// Creates an empty handle: no snapshot, not ready, not stale.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            current: ArcSwapOption::empty(),
            status: Mutex::new(ConfigStatus::default()),
        })
    }

    /// Returns the published snapshot, or `None` before the first successful publish.
    #[must_use]
    pub fn current(&self) -> Option<Arc<RuntimeSnapshot>> {
        self.current.load_full()
    }

    /// Publishes `snapshot` atomically and records its metadata in the status.
    ///
    /// `ready` is not changed here: readiness is a pipeline decision, set with [`Self::mark_ready`]
    /// once the first snapshot is live.
    pub fn store(&self, snapshot: Arc<RuntimeSnapshot>) {
        let generation = snapshot.generation();
        let revision = snapshot.revision();
        let digest = snapshot.digest();
        self.current.store(Some(snapshot));
        self.update_status(|status| {
            status.generation = generation.get();
            status.revision = Some(revision.seq());
            status.digest = Some(digest.hex());
            status.last_reload_unix_secs = unix_now();
        });
    }

    /// Returns a copy of the current status.
    #[must_use]
    pub fn status(&self) -> ConfigStatus {
        self.lock_status().clone()
    }

    /// Applies `update` to the current status.
    pub fn update_status(&self, update: impl FnOnce(&mut ConfigStatus)) {
        let mut guard = self.lock_status();
        update(&mut guard);
    }

    /// Marks the pipeline ready and clears the degraded flag.
    pub fn mark_ready(&self) {
        self.update_status(|status| {
            status.ready = true;
            status.degraded = false;
            status.last_error_category = None;
        });
    }

    /// Marks the pipeline degraded, keeping the last-known-good snapshot in service.
    pub fn mark_degraded(&self, category: ErrorCategory) {
        self.update_status(|status| {
            status.degraded = true;
            status.last_error_category = Some(category);
        });
    }

    /// Marks the published snapshot as loaded from the last-known-good cache.
    pub fn mark_stale(&self) {
        self.update_status(|status| {
            status.stale = true;
        });
    }

    fn lock_status(&self) -> MutexGuard<'_, ConfigStatus> {
        // A panic elsewhere while the status lock was held cannot corrupt the published snapshot,
        // so recovering the guard is safer than turning it into a second panic.
        self.status.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn unix_now() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ParseLimits;

    const DOCUMENT: &str = "\
apiVersion: gateway/v1
limits:
  max_request_body_bytes: 1048576
  request_timeout_ms: 30000
providers: []
upstreams: []
routes: []
";

    fn document() -> Document {
        Document::parse(DOCUMENT.as_bytes(), ParseLimits::new(4096)).expect("fixture is valid")
    }

    fn snapshot(generation: u64) -> RuntimeSnapshot {
        let bytes = DOCUMENT.as_bytes();
        RuntimeSnapshot::new(
            Generation::new(generation),
            Revision::comparable(generation),
            ConfigDigest::of(bytes),
            document(),
            "test",
        )
    }

    #[test]
    fn handle_has_no_snapshot_before_the_first_store() {
        let handle = SnapshotHandle::new();
        assert!(handle.current().is_none());
        assert_eq!(handle.status().generation, 0);
    }

    #[test]
    fn store_publishes_the_snapshot_and_its_metadata() {
        let handle = SnapshotHandle::new();
        let published = Arc::new(snapshot(1));
        let digest = published.digest().hex();

        handle.store(Arc::clone(&published));

        let current = handle.current().expect("snapshot is published");
        assert_eq!(current.generation(), Generation::new(1));
        assert_eq!(current.source(), "test");

        let status = handle.status();
        assert_eq!(status.generation, 1);
        assert_eq!(status.revision, Some(1));
        assert_eq!(status.digest, Some(digest));
        assert!(status.last_reload_unix_secs.is_some());
        assert!(
            !status.ready,
            "store alone must not make the pipeline ready"
        );
    }

    #[test]
    fn store_leaves_a_previously_read_snapshot_unchanged() {
        let handle = SnapshotHandle::new();
        handle.store(Arc::new(snapshot(1)));
        let held = handle.current().expect("first snapshot is published");

        handle.store(Arc::new(snapshot(2)));

        assert_eq!(held.generation(), Generation::new(1));
        assert_eq!(
            handle.current().map(|current| current.generation()),
            Some(Generation::new(2))
        );
    }

    #[test]
    fn mark_ready_clears_degraded_and_the_last_error() {
        let handle = SnapshotHandle::new();
        handle.store(Arc::new(snapshot(1)));
        handle.mark_degraded(ErrorCategory::Client);
        assert!(handle.status().degraded);

        handle.mark_ready();

        let status = handle.status();
        assert!(status.ready);
        assert!(!status.degraded);
        assert_eq!(status.last_error_category, None);
    }

    #[test]
    fn mark_degraded_records_the_error_category() {
        let handle = SnapshotHandle::new();
        handle.mark_degraded(ErrorCategory::Upstream);

        let status = handle.status();
        assert!(status.degraded);
        assert_eq!(status.last_error_category, Some(ErrorCategory::Upstream));
    }

    #[test]
    fn mark_stale_flags_a_cache_bootstrap() {
        let handle = SnapshotHandle::new();
        assert!(!handle.status().stale);

        handle.mark_stale();

        assert!(handle.status().stale);
    }
}
