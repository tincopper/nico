//! Compiling one candidate document and publishing it as a generation.
//!
//! This is the only place a generation reaches service, and it does so in one step: the snapshot
//! operators read and the value the request path reads are written together, so no reader can
//! observe one generation's document with another's policies.
//!
//! The rules it enforces are the pipeline's whole safety argument:
//!
//! - **De-duplication and ordering run before compilation.** A document already in service is not
//!   compiled twice, and a revision a source proves older is not compiled at all.
//! - **A rejected document leaves the last known good one serving.** Whether the document model
//!   refused the bytes or the plugin compiler refused the plan, the outcome is the same: nothing is
//!   published, the pipeline is marked degraded, and the caller retries.
//! - **One generation number per publish.** The counter is advanced once and stored only after both
//!   readers were written.

use std::sync::Arc;

use nico_config::{CacheMeta, ConfigVersion};
use nico_core::{
    ConfigDigest, ErrorCategory, Generation, Revision, RevisionOrdering, RuntimeSnapshot,
};
use nico_http::PublishedChains;

use super::{Reloader, pacing::unix_now};

/// What happened to one candidate document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PublishOutcome {
    /// A new generation is in service.
    Published,
    /// The candidate was already in service or provably older; nothing changed.
    Skipped,
    /// The document was rejected; the last-known-good snapshot is still in service.
    Rejected(ErrorCategory),
}

impl Reloader {
    /// Validates one candidate document and publishes it as a new generation.
    ///
    /// De-duplication and ordering run before validation: a document that is already in service is
    /// not compiled twice, and a revision the source proves is older is not compiled at all.
    pub(super) async fn publish(
        &mut self,
        version: ConfigVersion,
    ) -> Result<PublishOutcome, super::ReloadError> {
        let digest = ConfigDigest::of(version.bytes());

        if self.digest == Some(digest) {
            self.confirm_published(digest, &version);
            return Ok(PublishOutcome::Skipped);
        }

        if self.is_regression(version.revision()) {
            tracing::debug!(
                source = self.source.name(),
                config.revision = %version.revision(),
                digest = %digest,
                "discarding a configuration revision that is not newer than the published one"
            );
            return Ok(PublishOutcome::Skipped);
        }

        let document = match nico_core::Document::parse(version.bytes(), self.parse_limits) {
            Ok(document) => document,
            Err(error) => {
                let category = error.category();
                self.handle.mark_degraded(category);
                tracing::warn!(
                    source = self.source.name(),
                    error = %error,
                    error.category = ?category,
                    config.revision = %version.revision(),
                    size = version.bytes().len(),
                    "configuration document rejected; serving the last-known-good snapshot"
                );
                return Ok(PublishOutcome::Rejected(category));
            }
        };

        // Compiling the policies is part of validating the document, not a separate step after it: a
        // published generation must be one whose plugins can actually run, or "published" would mean
        // "the routes are current and the credentials are not". A refusal here keeps the previous
        // generation in service exactly like a refused document does.
        let plan = match self.runtime.compile(&document) {
            Ok(plan) => plan,
            Err(error) => {
                let category = error.category();
                self.handle.mark_degraded(category);
                tracing::warn!(
                    source = self.source.name(),
                    error = %error,
                    error.category = ?category,
                    config.revision = %version.revision(),
                    "plugin compilation failed; serving the last-known-good snapshot"
                );
                return Ok(PublishOutcome::Rejected(category));
            }
        };

        let generation = self.generation.next()?;
        self.publish_state(
            Arc::new(RuntimeSnapshot::new(
                generation,
                version.revision(),
                digest,
                document,
                self.source.name(),
            )),
            plan,
        );
        self.handle.mark_ready();
        // `mark_ready` clears `degraded`, which a stale snapshot must keep; it does not clear
        // `stale`, which a freshly compiled document must not keep. Both flags describe something
        // different, so they are set on their own lines.
        self.handle.update_status(|status| status.stale = false);
        self.generation = generation;
        self.revision = Some(version.revision());
        self.digest = Some(digest);

        tracing::info!(
            source = self.source.name(),
            config.generation = generation.get(),
            config.revision = %version.revision(),
            digest = %digest,
            size = version.bytes().len(),
            "runtime snapshot published"
        );

        self.cache_published(&version, generation, digest).await;
        Ok(PublishOutcome::Published)
    }

    /// Publishes one generation to both readers, in one step.
    ///
    /// The status handle comes first so an operator polling it never sees a generation the request
    /// path has not loaded; a request path that reads slightly earlier keeps serving the previous
    /// generation, which is a complete and consistent pair.
    ///
    /// `plan` is compiled from `snapshot`'s document; passing a plan from another generation would
    /// pair a route table with policies that do not belong to it, which is the one thing
    /// [`HttpStateHandle`] exists to prevent. It carries the global chain, the per-route chains and the
    /// buffered route set together, so the request path can never read one of them from an older
    /// publication.
    ///
    /// [`HttpStateHandle`]: nico_http::HttpStateHandle
    pub(super) fn publish_state(&self, snapshot: Arc<RuntimeSnapshot>, plan: PublishedChains) {
        self.handle.store(Arc::clone(&snapshot));
        self.state.publish(snapshot, plan);
    }

    /// Handles the case where the document in service is exactly the document just received.
    ///
    /// Nothing is recompiled, but the event is still a health signal: the source has just produced
    /// the bytes that are already in service, so the pipeline is neither degraded nor serving a
    /// guess. Without this, a `stale` or `degraded` flag would outlive the outage that set it as
    /// long as the source kept serving an unchanged document.
    fn confirm_published(&self, digest: ConfigDigest, version: &ConfigVersion) {
        let status = self.handle.status();
        if !status.stale && !status.degraded {
            tracing::debug!(
                source = self.source.name(),
                config.revision = %version.revision(),
                digest = %digest,
                "configuration unchanged; skipping the publish"
            );
            return;
        }

        self.handle.mark_ready();
        self.handle.update_status(|status| status.stale = false);
        tracing::info!(
            source = self.source.name(),
            config.revision = %version.revision(),
            digest = %digest,
            config.generation = self.generation.get(),
            was_stale = status.stale,
            was_degraded = status.degraded,
            "configuration confirmed by the source; the published snapshot is current"
        );
    }

    /// Reports whether `incoming` is provably older than the revision already published.
    ///
    /// Both revisions must come from a source that declares a total order. An equality-only source
    /// can only distinguish "different", so its events are never dropped here.
    fn is_regression(&self, incoming: Revision) -> bool {
        let Some(current) = self.revision else {
            return false;
        };
        incoming.ordering() == RevisionOrdering::Comparable
            && current.ordering() == RevisionOrdering::Comparable
            && incoming.seq() <= current.seq()
    }

    /// Writes the published document to the last-known-good cache.
    ///
    /// Best effort: the snapshot is already live, and losing the cache costs one restart's worth of
    /// freshness, not correctness.
    async fn cache_published(
        &self,
        version: &ConfigVersion,
        generation: Generation,
        digest: ConfigDigest,
    ) {
        let Some(cache) = self.cache.as_ref() else {
            return;
        };
        let meta = CacheMeta::new(
            version.revision().seq(),
            digest.hex(),
            generation.get(),
            unix_now(),
            self.source.name(),
        );
        if let Err(error) = cache.write(version.bytes(), &meta).await {
            tracing::warn!(
                source = self.source.name(),
                error = %error,
                error.category = ?error.category(),
                "last-known-good cache write failed"
            );
        }
    }
}
