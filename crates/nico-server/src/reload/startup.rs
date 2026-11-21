//! Reaching a first publish: the source, or the last-known-good cache when the operator allowed it.
//!
//! Startup is the one phase where "no snapshot" is a legitimate state to give up on: a pipeline that
//! could not publish once has nothing to serve, and [`ReloadError::NotReady`] is how the process is
//! told to stop instead of answering `503` forever. Once a snapshot is in service every failure keeps
//! that snapshot, so this module is reached exactly once.
//!
//! The cache is a memory of a previously published document, not a second configuration source: it is
//! compiled through the same document model and the same plugin compiler as anything else, so a
//! remembered document can never be served with a policy set it did not compile.

use std::sync::Arc;

use nico_core::{ConfigDigest, Document, ErrorCategory, Generation, Revision, RuntimeSnapshot};
use nico_http::expand_members_with;

use super::{ReloadError, Reloader, publish::PublishOutcome};

impl Reloader {
    /// Publishes the first snapshot, or falls back to the cache when the operator allowed it.
    ///
    /// Returns `Ok(false)` when shutdown won the race before a snapshot could be published.
    pub(super) async fn startup(&mut self) -> Result<bool, ReloadError> {
        let outcome = match self.load_or_shutdown().await {
            None => return Ok(false),
            Some(Ok(version)) => self.publish(version).await?,
            Some(Err(error)) => {
                let category = error.category();
                tracing::warn!(
                    source = self.source.name(),
                    error = %error,
                    error.category = ?category,
                    "configuration source unavailable at startup"
                );
                self.handle.mark_degraded(category);
                PublishOutcome::Rejected(category)
            }
        };

        match outcome {
            PublishOutcome::Published | PublishOutcome::Skipped => Ok(true),
            PublishOutcome::Rejected(category) => self.start_from_cache(category).await,
        }
    }

    /// Serves the last-known-good cache, marked stale, when nothing else is available.
    ///
    /// A stale snapshot is servable — that is the point of the flag — but it stays `degraded`, so
    /// an operator can see that the document in service is a memory rather than an observation.
    async fn start_from_cache(&mut self, category: ErrorCategory) -> Result<bool, ReloadError> {
        if !self.allow_bootstrap_cache {
            return Err(ReloadError::NotReady { category });
        }
        let Some(cache) = self.cache.clone() else {
            return Err(ReloadError::NotReady { category });
        };

        let cached = match cache.read().await {
            Ok(cached) => cached,
            Err(error) => {
                tracing::warn!(
                    source = self.source.name(),
                    error = %error,
                    error.category = ?error.category(),
                    "last-known-good cache is not usable"
                );
                return Err(ReloadError::NotReady { category });
            }
        };

        let document = match Document::parse(cached.bytes(), self.parse_limits) {
            Ok(document) => document,
            Err(error) => {
                tracing::warn!(
                    source = self.source.name(),
                    error = %error,
                    error.category = ?error.category(),
                    "cached configuration document is not usable"
                );
                return Err(ReloadError::NotReady {
                    category: error.category(),
                });
            }
        };

        // A remembered document is compiled like any other: its plugins and its LLM routes are what
        // make it servable, so a credential that has since disappeared has to stop startup rather than
        // publish a generation whose routes would all answer `500`.
        let plan = match self.runtime.compile(&document) {
            Ok(plan) => plan,
            Err(error) => {
                let category = error.category();
                tracing::warn!(
                    source = self.source.name(),
                    error = %error,
                    error.category = ?category,
                    "cached configuration document has no usable plugin plan"
                );
                return Err(ReloadError::NotReady { category });
            }
        };
        let llm = match self.llm.compile(&document, &plan) {
            Ok(llm) => llm,
            Err(error) => {
                let category = error.category();
                tracing::warn!(
                    source = self.source.name(),
                    error = %error,
                    error.category = ?category,
                    "cached configuration document has no usable LLM plan"
                );
                return Err(ReloadError::NotReady { category });
            }
        };

        // The cached document's hostname members are resolved like any other publication's: a
        // remembered document can never be served with a member list the resolver cannot produce.
        // The failure has no degraded form — startup is the one phase with nothing to fall back on —
        // so the field the resolver could not answer stops the process, named.
        let members = match expand_members_with(&document, &self.member_dns).await {
            Ok(members) => members,
            Err(error) => {
                let category = error.category();
                tracing::warn!(
                    source = self.source.name(),
                    error = %error,
                    error.category = ?category,
                    "cached configuration document has an unresolvable member address"
                );
                return Err(ReloadError::NotReady { category });
            }
        };

        // The generation continues where the cache left off, so a restart does not look like a
        // rollback to every observer of the status.
        let generation = Generation::new(cached.meta().generation());
        // The cached revision is restored as equality-only: the sequence belongs to the *previous*
        // process (a local source restarts its counter at one), so no order can be proven across
        // the restart. De-duplication still works, because it uses the digest.
        let revision = Revision::equality_only(cached.meta().revision_seq());
        let digest = ConfigDigest::of(cached.bytes());

        self.generation = generation;
        self.revision = Some(revision);
        self.digest = Some(digest);

        self.publish_state(
            Arc::new(RuntimeSnapshot::new(
                generation,
                revision,
                digest,
                document,
                self.source.name(),
            )),
            plan,
            llm,
            &members,
        );
        self.handle.mark_stale();
        self.handle.mark_degraded(category);
        // `mark_ready` would clear `degraded`, which is exactly the signal an operator needs here.
        self.handle.update_status(|status| status.ready = true);

        tracing::warn!(
            source = self.source.name(),
            config.generation = generation.get(),
            config.revision = %revision,
            digest = %digest,
            size = cached.bytes().len(),
            error.category = ?category,
            "serving the last-known-good snapshot; the configuration source is unavailable"
        );
        Ok(true)
    }
}
