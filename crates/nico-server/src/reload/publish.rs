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
//!   refused the bytes, the plugin compiler refused the plan, or the LLM compiler refused a provider
//!   route, the outcome is the same: nothing is published, the pipeline is marked degraded, and the
//!   caller retries.
//! - **One generation number per publish.** The counter is advanced once and stored only after both
//!   readers were written.
//!
//! The LLM plan is compiled here rather than lazily on the first AI request: a document whose provider
//! route cannot be served is a document this process must not publish, because the alternative is a
//! request path that answers `502` for every request of a route the operator believes is live.

use std::{fmt, sync::Arc, time::Instant};

use nico_config::{CacheMeta, ConfigVersion};
use nico_core::{
    ConfigDigest, ErrorCategory, Generation, Revision, RevisionOrdering, RuntimeSnapshot,
};
use nico_http::{ExpandedMembers, PublishedChains, expand_members_with};

use super::{Reloader, pacing::unix_now};
use crate::reload::LlmFrontEnds;

/// The three values one compiled document produces, which a publication stores together.
///
/// A struct rather than a tuple because the three are named facts at the call site. It is not an
/// enum with a "refused" variant: the caller's `Result<CompiledDocument, PublishOutcome>` already
/// says "compiled, or refused", and a refusal is an outcome rather than an error — the `Err` arm
/// reads as one because the alternative is a second type spelling the same two cases.
struct CompiledDocument {
    /// The parsed document, stored in the snapshot unchanged.
    document: nico_core::Document,
    /// The chains compiled from that document.
    plan: PublishedChains,
    /// The provider front-ends compiled from that document and that plan.
    llm: Arc<dyn LlmFrontEnds>,
}

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
    /// Validates one candidate document and publishes it as a new generation, recording the two
    /// publication metrics once per attempt.
    ///
    /// This is the one place every outcome of an attempt is known — published, skipped as a
    /// duplicate or regression, or refused — so `config_reload_total` counts each attempt exactly
    /// once here, and `config_compile_seconds` measures the compile phase rather than a stage that
    /// a later refactor could move.
    ///
    /// The two measure different things on purpose. `config_compile_seconds` is CPU work — parse,
    /// plugin compile, LLM compile — and is what an operator optimizes when reloads are slow. The
    /// last-known-good cache flush is file I/O and is deliberately **outside** it: folding a slow
    /// disk into a metric named `compile` would send that operator to the wrong place. The attempt
    /// counter is recorded after everything, because an attempt is one unit of work.
    pub(super) async fn publish(
        &mut self,
        version: ConfigVersion,
    ) -> Result<PublishOutcome, super::ReloadError> {
        let outcome = self.publish_candidate(version).await;
        let result = match outcome {
            Ok(PublishOutcome::Rejected(_)) => "error",
            _ => "ok",
        };
        metrics::counter!("config_reload_total", "result" => result).increment(1);
        outcome
    }

    /// Compiles one parsed document into the three values a publication stores.
    ///
    /// One function because the three are one decision: a generation whose routes are current but
    /// whose plugin plan or LLM front-ends are not would serve a document it cannot describe.
    ///
    /// The return type is `Result<CompiledDocument, PublishOutcome>` and the `Err` arm carries the
    /// refusal **outcome**, not an error. A refused document is a normal result: the previous
    /// generation keeps serving and the next source event retries, which is exactly
    /// [`PublishOutcome::Rejected`]. Every stage refuses the same way, so collapsing them into one
    /// outcome here keeps the caller to a single `match` instead of three error types it would only
    /// turn back into that same outcome.
    ///
    /// The caller times this call: the plugin and LLM compilations are the CPU work
    /// `config_compile_seconds` measures, and keeping the timer at the call site is what keeps that
    /// metric's name honest — the cache flush that follows is I/O, not compile.
    fn compile_document(
        &self,
        document: nico_core::Document,
        version: &ConfigVersion,
    ) -> Result<CompiledDocument, PublishOutcome> {
        // Compiling the policies is part of validating the document, not a separate step after it: a
        // published generation must be one whose plugins can actually run, or "published" would mean
        // "the routes are current and the credentials are not".
        let plan = match self.runtime.compile(&document) {
            Ok(plan) => plan,
            Err(error) => {
                return Err(self.refuse(&error, error.category(), "plugin", version));
            }
        };

        // The LLM plan is compiled from the same document and the same plugin plan: a provider route's
        // body ownership is one decision, so the two cannot be built from different generations.
        let llm = match self.llm.compile(&document, &plan) {
            Ok(llm) => llm,
            Err(error) => {
                return Err(self.refuse(&error, error.category(), "llm", version));
            }
        };

        Ok(CompiledDocument {
            document,
            plan,
            llm,
        })
    }

    /// Validates one candidate document and publishes it as a new generation.
    ///
    /// De-duplication and ordering run before validation: a document that is already in service is
    /// not compiled twice, and a revision the source proves is older is not compiled at all.
    async fn publish_candidate(
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
            Err(error) => return Ok(self.refuse(&error, error.category(), "document", &version)),
        };

        // The compile phase is timed as a phase, not as the whole attempt: this is the CPU work an
        // operator acts on when reloads are slow, and the cache flush below is I/O that belongs to
        // a different diagnosis. Parse is deliberately outside the window — it is the document
        // model's cost, and a document that fails to parse never reaches a compile at all — so the
        // histogram answers "how expensive is turning a valid document into a servable plan".
        let compiled = {
            let started = Instant::now();
            let compiled = self.compile_document(document, &version);
            metrics::histogram!("config_compile_seconds").record(started.elapsed().as_secs_f64());
            compiled
        };
        let CompiledDocument {
            document,
            plan,
            llm,
        } = match compiled {
            Ok(compiled) => compiled,
            Err(outcome) => return Ok(outcome),
        };

        // The member addresses are resolved outside the compile histogram: DNS is I/O, not CPU, and
        // an operator tuning `config_compile_seconds` should not be pointed at a resolver. A
        // hostname that cannot be answered refuses the candidate with its field named, and the
        // last-known-good snapshot keeps serving — the same shape as every other stage refusal.
        let members = match expand_members_with(&document, &self.member_dns).await {
            Ok(members) => members,
            Err(error) => {
                return Ok(self.refuse(&error, error.category(), "members", &version));
            }
        };

        let generation = self.generation.next()?;
        let llm_routes = llm.route_count();
        self.publish_state(
            Arc::new(RuntimeSnapshot::new(
                generation,
                version.revision(),
                digest,
                document,
                self.source.name(),
            )),
            plan,
            llm,
            &members,
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
            llm.routes = llm_routes,
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
    /// `plan` and `llm` are compiled from `snapshot`'s document; passing a plan from another generation
    /// would pair a route table with policies that do not belong to it, which is the one thing
    /// [`HttpStateHandle`] exists to prevent. `plan` carries the global chain, the per-route chains and
    /// the buffered route set together, `llm` the front-ends of every provider route, and `members`
    /// the document's resolved hostname members, so the request path can never read one of them from
    /// an older publication.
    ///
    /// [`HttpStateHandle`]: nico_http::HttpStateHandle
    pub(super) fn publish_state(
        &self,
        snapshot: Arc<RuntimeSnapshot>,
        plan: PublishedChains,
        llm: Arc<dyn LlmFrontEnds>,
        members: &ExpandedMembers,
    ) {
        self.handle.store(Arc::clone(&snapshot));
        // Keep a handle on the plan so the report can be emitted after the store, which is the only
        // point at which "published" is true.
        let report = Arc::clone(&llm);
        self.state
            .publish_with_llm(snapshot, plan, llm.routes(), members);
        report.log_published();
    }

    /// Refuses a candidate document at whichever stage refused it.
    ///
    /// The three stages answer the same way — mark the generation degraded, report which stage and
    /// which revision, keep the last known good snapshot serving — so they share one path. A fourth
    /// stage cannot forget the degradation, and `error.stage` is what tells an operator which check
    /// said no; the message is one sentence for the same reason.
    ///
    /// A refusal is an **outcome**, not an error: the pipeline keeps serving the previous generation
    /// and retries on the next source event, which is exactly [`PublishOutcome::Rejected`]. Returning
    /// the outcome rather than a `Result` is what lets the compile stage hand it straight back
    /// instead of wrapping it in a `Result` it would never populate with `Err`.
    fn refuse(
        &self,
        error: &dyn fmt::Display,
        category: ErrorCategory,
        stage: &'static str,
        version: &ConfigVersion,
    ) -> PublishOutcome {
        self.handle.mark_degraded(category);
        tracing::warn!(
            source = self.source.name(),
            error = %error,
            error.category = ?category,
            error.stage = stage,
            config.revision = %version.revision(),
            size = version.bytes().len(),
            "configuration refused; serving the last-known-good snapshot"
        );
        PublishOutcome::Rejected(category)
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
