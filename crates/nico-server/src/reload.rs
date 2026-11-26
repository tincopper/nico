//! The configuration reload pipeline: one source, one snapshot, one publication point.
//!
//! The pipeline owns every policy decision a source refuses to make:
//!
//! - **De-bounce with a hard ceiling.** A burst of changes is coalesced for `debounce`, but a
//!   publish is never postponed for longer than `max_wait` while changes keep arriving, so a
//!   continuous stream of edits cannot starve the reload.
//! - **Single flight.** A candidate is compiled and published by one task, one document at a time.
//!   Events that arrive while a document is in flight only replace the pending candidate.
//! - **De-duplication and ordering.** The same digest is never compiled twice, and a revision that
//!   a totals-ordered source proves older is dropped. Equality-only sources are never sorted.
//! - **Failure keeps the last known good.** A rejected document, an unreachable source or a broken
//!   watch leaves the published snapshot in service, marks the pipeline degraded and retries with
//!   backoff. The generation counter never moves during a failure.
//! - **Publication is atomic, and one generation is one value.** A complete [`RuntimeSnapshot`](nico_core::RuntimeSnapshot) is
//!   built off the request path, together with the plugin plan and the LLM plan compiled from the same
//!   document; `publish_state` hands all of them to the request path and to the status handle operators
//!   read. A reader holding the previous `Arc` keeps a consistent view for its whole lifetime.
//!
//! Everything that can be cancelled is cancellable: the watch, the debounce timer, the backoff wait
//! and the initial load all select on the shutdown token, so [`Reloader::run`] returns promptly and
//! leaves no task behind.
//!
//! # Modules
//!
//! - `pacing` — the pure parts of the loop: coalescing, deadlines, backoff.
//! - `publish` — validation, compilation and the one publication step.
//! - `startup` — the first publish, and the last-known-good cache fallback.
//! - `settings` — the reloader's inputs: the publication readers and the run settings.
//! - `error` — the pipeline's public failure vocabulary.
//!
//! This module keeps the reloader's state, the consuming loop and the retry loop: what survives one
//! document is exactly what is declared here.

use std::sync::Arc;

pub use error::ReloadError;
#[cfg(not(feature = "llm"))]
pub use llm::NoLlmCompiler;
use llm::builtin_llm;
pub use llm::{LlmCompileError, LlmCompiler, LlmFrontEnds};
use nico_config::{CacheStore, ConfigSource, ConfigSourceError, ConfigVersion, ConfigWatch};
use nico_core::{ConfigDigest, ErrorCategory, Generation, ParseLimits, Revision, SnapshotHandle};
use nico_http::{HttpStateHandle, MemberResolver};
use nico_plugins::PluginRuntime;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

mod error;
mod llm;
mod pacing;
mod publish;
mod settings;
mod startup;

pub use settings::{Publication, ReloadLimits, ReloadSettings};

use self::pacing::{Wake, WatchStep, coalesce, earliest, next_item, sleep_until};

/// Loads, validates and publishes one configuration source until shutdown.
///
/// The reloader is the only component that writes to a [`SnapshotHandle`]: a data plane reads the
/// handle and never the source.
pub struct Reloader {
    source: Arc<dyn ConfigSource>,
    handle: Arc<SnapshotHandle>,
    /// The one value the request path reads. Written in the same step as `handle`, so a request never
    /// observes one generation's snapshot with another's policies.
    state: Arc<HttpStateHandle>,
    /// The plugin compiler, which also keeps the state that outlives a publication — a rate limiter's
    /// buckets. It is created by the process assembly, so every generation of one process shares it.
    runtime: Arc<PluginRuntime>,
    /// The AI role's compiler, shared by every generation for the same reason: a codec and an adapter
    /// hold configuration, and a key is re-read on each publication. The traffic role supplies the
    /// no-op implementation, which refuses every provider route.
    llm: Arc<dyn LlmCompiler>,
    /// The DNS the member expansion consults, shared by every generation for the same reason: a
    /// publication resolves its hostname members through the same resolver the last one did. The
    /// production value is Tokio's blocking-pool lookup; a test injects one that answers with the
    /// addresses it states, because a resolver that wildcard-answers every name would make a
    /// real-DNS refusal undeterminable.
    member_dns: MemberResolver,
    cache: Option<CacheStore>,
    limits: ReloadLimits,
    parse_limits: ParseLimits,
    allow_bootstrap_cache: bool,
    shutdown: CancellationToken,
    /// Generation of the published snapshot, or [`Generation::ZERO`] before the first publish.
    generation: Generation,
    /// Revision of the published snapshot, used to drop events a source proves are older.
    revision: Option<Revision>,
    /// Digest of the published document, used to skip a document that is already in service.
    digest: Option<ConfigDigest>,
}

impl Reloader {
    /// Creates a reloader for one source, its two publication readers and one plugin compiler.
    ///
    /// `publication` carries both readers the pipeline writes — the configuration-state fact source
    /// and the one value the request path reads — because `publish_state` writes them together.
    /// `runtime` is shared with the process rather than created here, because the bucket tables inside
    /// it must survive every reload this reloader performs and nothing else in the process knows them.
    ///
    /// The compiler is the build's own: the `llm` feature supplies the real registries and credential
    /// resolver, and a traffic-only build supplies the refusal. It is created here so a pipeline that
    /// does not care can be built without naming them; [`Reloader::with_llm`] is the seam that
    /// injects another one.
    #[must_use]
    pub fn new(
        source: Arc<dyn ConfigSource>,
        publication: Publication,
        runtime: Arc<PluginRuntime>,
        settings: ReloadSettings,
    ) -> Self {
        Self::with_llm(source, publication, runtime, builtin_llm(), settings)
    }

    /// Sets the DNS the member expansion consults.
    ///
    /// The one seam a test needs to make a publication's resolution a stated fact: a resolver that
    /// answers, refuses, or answers twice with one address, all without depending on what the
    /// host's resolver does with a name nobody owns.
    #[must_use]
    pub fn with_member_dns(mut self, member_dns: MemberResolver) -> Self {
        self.member_dns = member_dns;
        self
    }

    /// Creates a reloader that compiles LLM routes with `llm`.
    ///
    /// The registries and the resolver are process-wide state, so the process creates them once and
    /// hands the same value to the reloader it builds. A test uses this constructor to compile a
    /// document against a resolver it owns, instead of mutating the environment.
    #[must_use]
    pub fn with_llm(
        source: Arc<dyn ConfigSource>,
        Publication { status, requests }: Publication,
        runtime: Arc<PluginRuntime>,
        llm: Arc<dyn LlmCompiler>,
        settings: ReloadSettings,
    ) -> Self {
        let limits = settings.limits();
        let ReloadSettings {
            cache,
            parse_limits,
            allow_bootstrap_cache,
            shutdown,
            ..
        } = settings;
        Self {
            source,
            handle: status,
            state: requests,
            runtime,
            llm,
            member_dns: nico_http::resolver(),
            cache,
            limits,
            parse_limits,
            allow_bootstrap_cache,
            shutdown,
            generation: Generation::ZERO,
            revision: None,
            digest: None,
        }
    }

    /// Runs the pipeline until `shutdown` is cancelled.
    ///
    /// # Errors
    /// - [`ReloadError::NotReady`] when no snapshot could be published and no cache fallback was
    ///   allowed or usable.
    /// - [`ReloadError::GenerationOverflow`] when the generation counter is exhausted.
    ///
    /// A source that fails *after* a snapshot is in service is not an error: the pipeline keeps
    /// serving the last-known-good snapshot, records the failure in [`SnapshotHandle::status`] and
    /// retries with backoff, which is what a gateway must do while its control plane is down.
    pub async fn run(mut self) -> Result<(), ReloadError> {
        if !self.startup().await? {
            return Ok(());
        }

        loop {
            match self.subscribe().await? {
                Subscribe::Shutdown => return Ok(()),
                Subscribe::Failed(category) => {
                    self.handle.mark_degraded(category);
                    tracing::warn!(
                        source = self.source.name(),
                        error.category = ?category,
                        "configuration watch could not be established; serving the last-known-good snapshot"
                    );
                    if !self.recover(1).await? {
                        return Ok(());
                    }
                }
                Subscribe::Watch(watch) => {
                    if self.consume(watch).await? == Session::Shutdown {
                        return Ok(());
                    }
                    // The subscription ended without an error. Re-read the whole document before
                    // resubscribing, so nothing published in the gap can be missed.
                    tracing::debug!(
                        source = self.source.name(),
                        "configuration watch ended; reloading before resubscribing"
                    );
                    if !self.recover(1).await? {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Establishes a watch, or reports why it could not be established.
    async fn subscribe(&mut self) -> Result<Subscribe, ReloadError> {
        let result = tokio::select! {
            biased;
            () = self.shutdown.cancelled() => return Ok(Subscribe::Shutdown),
            result = self.source.watch(self.revision) => result,
        };

        match result {
            Ok(watch) => {
                tracing::debug!(
                    source = self.source.name(),
                    config.revision = ?self.revision.map(Revision::seq),
                    "configuration watch established"
                );
                Ok(Subscribe::Watch(watch))
            }
            Err(error) => Ok(Subscribe::Failed(error.category())),
        }
    }

    /// Reads the watch until it ends, publishing candidates as their debounce window closes.
    async fn consume(&mut self, mut watch: ConfigWatch) -> Result<Session, ReloadError> {
        let mut pending: Option<ConfigVersion> = None;
        let mut quiet_until: Option<Instant> = None;
        let mut max_wait_until: Option<Instant> = None;

        loop {
            let deadline = earliest(quiet_until, max_wait_until);
            let wake = tokio::select! {
                biased;
                () = self.shutdown.cancelled() => Wake::Shutdown,
                () = sleep_until(deadline) => Wake::Deadline,
                item = next_item(&mut watch) => Wake::Item(item),
            };

            match wake {
                Wake::Shutdown => return Ok(Session::Shutdown),
                Wake::Deadline => {
                    quiet_until = None;
                    max_wait_until = None;
                    if let Some(version) = pending.take() {
                        self.publish(version).await?;
                    }
                }
                Wake::Item(None) => return Ok(Session::Disconnected),
                Wake::Item(Some(item)) => {
                    match coalesce(&mut watch, item) {
                        WatchStep::Candidate(version) => {
                            let now = Instant::now();
                            pending = Some(version);
                            quiet_until = Some(now + self.limits.debounce());
                            // The ceiling counts from the *first* pending change, so it must
                            // survive the burst that keeps resetting the quiet window.
                            if max_wait_until.is_none() {
                                max_wait_until = Some(now + self.limits.max_wait());
                            }
                        }
                        WatchStep::Resync => {
                            // The watch's continuity is lost, so the only recovery is a full re-read.
                            // This counter increments once per `ResyncRequired` event, attributed to
                            // the source that lost it; a file source has no \"reconnect\", but the
                            // event is the same fact.
                            metrics::counter!(
                                "config_watch_reconnects_total",
                                "source" => self.source.name(),
                            )
                            .increment(1);
                            pending = None;
                            quiet_until = None;
                            max_wait_until = None;
                            let loaded = self.load_or_shutdown().await;
                            match loaded {
                                None => return Ok(Session::Shutdown),
                                Some(Ok(version)) => {
                                    self.publish(version).await?;
                                }
                                Some(Err(error)) => {
                                    self.handle.mark_degraded(error.category());
                                    tracing::warn!(
                                        source = self.source.name(),
                                        error = %error,
                                        error.category = ?error.category(),
                                        "configuration reload failed; serving the last-known-good snapshot"
                                    );
                                    if !self.recover(1).await? {
                                        return Ok(Session::Shutdown);
                                    }
                                }
                            }
                        }
                        WatchStep::Failed(error) => {
                            pending = None;
                            quiet_until = None;
                            max_wait_until = None;
                            self.handle.mark_degraded(error.category());
                            tracing::warn!(
                                source = self.source.name(),
                                error = %error,
                                error.category = ?error.category(),
                                "configuration source failed; serving the last-known-good snapshot"
                            );
                            if !self.recover(1).await? {
                                return Ok(Session::Shutdown);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Re-reads the whole document after a failure, backing off between attempts.
    ///
    /// Returns `Ok(false)` when shutdown interrupted the retry loop. The loop is unbounded on
    /// purpose: a gateway must keep serving its last-known-good snapshot and keep trying, and every
    /// iteration is separated by a backoff wait so a dead source cannot spin.
    async fn recover(&mut self, mut attempt: u32) -> Result<bool, ReloadError> {
        loop {
            if self.wait_backoff(attempt).await {
                return Ok(false);
            }
            match self.load_or_shutdown().await {
                None => return Ok(false),
                Some(Ok(version)) => {
                    self.publish(version).await?;
                    return Ok(true);
                }
                Some(Err(error)) => {
                    self.handle.mark_degraded(error.category());
                    tracing::warn!(
                        source = self.source.name(),
                        error = %error,
                        error.category = ?error.category(),
                        attempt,
                        "configuration reload failed; serving the last-known-good snapshot"
                    );
                    attempt = attempt.saturating_add(1);
                }
            }
        }
    }

    /// Waits out the reconnect delay, returning `true` when shutdown interrupted the wait.
    async fn wait_backoff(&self, attempt: u32) -> bool {
        let delay = pacing::backoff_delay(attempt);
        tracing::debug!(
            source = self.source.name(),
            attempt,
            delay = ?delay,
            "waiting before reloading the configuration"
        );
        tokio::select! {
            biased;
            () = self.shutdown.cancelled() => true,
            () = tokio::time::sleep(delay) => false,
        }
    }

    /// Reads the whole document, or returns `None` when shutdown wins the race.
    async fn load_or_shutdown(&self) -> Option<Result<ConfigVersion, ConfigSourceError>> {
        tokio::select! {
            biased;
            () = self.shutdown.cancelled() => None,
            result = self.source.load() => {
                // This counter increments once per source read, whatever its outcome: startup,
                // a resync and a retry all read through here.
                let outcome = if result.is_ok() { "ok" } else { "error" };
                metrics::counter!(
                    "config_load_total",
                    "source" => self.source.name(),
                    "result" => outcome,
                )
                .increment(1);
                Some(result)
            }
        }
    }
}

/// The state of one subscription attempt.
enum Subscribe {
    /// The watch is live.
    Watch(ConfigWatch),
    /// The source refused the subscription.
    Failed(ErrorCategory),
    /// Shutdown was requested while subscribing.
    Shutdown,
}

/// Why the pipeline stopped consuming a watch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Session {
    /// Shutdown was requested.
    Shutdown,
    /// The watch ended on its own.
    Disconnected,
}

#[cfg(test)]
mod tests;
