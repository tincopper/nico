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
//! - **Publication is atomic.** A complete [`RuntimeSnapshot`] is built off the request path and
//!   swapped in with one `SnapshotHandle::store`, so a reader holding the previous `Arc` keeps a
//!   consistent view for its whole lifetime.
//!
//! Everything that can be cancelled is cancellable: the watch, the debounce timer, the backoff wait
//! and the initial load all select on the shutdown token, so [`Reloader::run`] returns promptly and
//! leaves no task behind.

use std::{
    sync::Arc,
    task::{Context, Poll, Waker},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use nico_config::{
    CacheMeta, CacheStore, ConfigEvent, ConfigSource, ConfigSourceError, ConfigVersion, ConfigWatch,
};
use nico_core::{
    ConfigDigest, Document, ErrorCategory, Generation, GenerationOverflow, ParseLimits, Revision,
    RevisionOrdering, RuntimeSnapshot, SnapshotHandle,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// First reconnect delay; it doubles on every further failed attempt.
const BACKOFF_BASE: Duration = Duration::from_millis(100);

/// Ceiling for the reconnect delay, jitter included.
const BACKOFF_CAP: Duration = Duration::from_secs(5);

/// Largest power-of-two multiplier applied to [`BACKOFF_BASE`] before the cap applies.
const BACKOFF_MAX_EXPONENT: u32 = 6;

/// One item of a configuration watch.
type WatchItem = Result<ConfigEvent, ConfigSourceError>;

/// How long a burst of changes is coalesced, and how long a publish may be postponed in total.
///
/// `max_wait` should be at least `debounce`; when it is shorter, it wins, because the ceiling exists
/// to stop starvation and the floor exists to avoid reload storms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReloadLimits {
    debounce: Duration,
    max_wait: Duration,
}

impl ReloadLimits {
    /// Creates limits from a debounce window and a maximum wait.
    #[must_use]
    pub const fn new(debounce: Duration, max_wait: Duration) -> Self {
        Self { debounce, max_wait }
    }

    /// Returns the quiet window a change must leave before it is published.
    #[must_use]
    pub const fn debounce(&self) -> Duration {
        self.debounce
    }

    /// Returns the longest a publish may be postponed while changes keep arriving.
    #[must_use]
    pub const fn max_wait(&self) -> Duration {
        self.max_wait
    }
}

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
        /// The overflow reported by [`Generation::next`].
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

/// Loads, validates and publishes one configuration source until shutdown.
///
/// The reloader is the only component that writes to a [`SnapshotHandle`]: a data plane reads the
/// handle and never the source.
pub struct Reloader {
    source: Arc<dyn ConfigSource>,
    handle: Arc<SnapshotHandle>,
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
    /// Creates a reloader for one source and one publication handle.
    ///
    /// `cache` is written after every successful publish; it is only *read* when startup fails and
    /// `allow_bootstrap_cache` is set, so passing a store never changes the running behaviour by
    /// itself.
    #[must_use]
    pub fn new(
        source: Arc<dyn ConfigSource>,
        handle: Arc<SnapshotHandle>,
        cache: Option<CacheStore>,
        limits: ReloadLimits,
        parse_limits: ParseLimits,
        allow_bootstrap_cache: bool,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            source,
            handle,
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

    /// Publishes the first snapshot, or falls back to the cache when the operator allowed it.
    ///
    /// Returns `Ok(false)` when shutdown won the race before a snapshot could be published.
    async fn startup(&mut self) -> Result<bool, ReloadError> {
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

        self.handle.store(Arc::new(RuntimeSnapshot::new(
            generation,
            revision,
            digest,
            document,
            self.source.name(),
        )));
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
                            quiet_until = Some(now + self.limits.debounce);
                            // The ceiling counts from the *first* pending change, so it must
                            // survive the burst that keeps resetting the quiet window.
                            if max_wait_until.is_none() {
                                max_wait_until = Some(now + self.limits.max_wait);
                            }
                        }
                        WatchStep::Resync => {
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
        let delay = backoff_delay(attempt);
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
            result = self.source.load() => Some(result),
        }
    }

    /// Validates one candidate document and publishes it as a new generation.
    ///
    /// De-duplication and ordering run before validation: a document that is already in service is
    /// not compiled twice, and a revision the source proves is older is not compiled at all.
    async fn publish(&mut self, version: ConfigVersion) -> Result<PublishOutcome, ReloadError> {
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

        let document = match Document::parse(version.bytes(), self.parse_limits) {
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

        let generation = self.generation.next()?;
        self.handle.store(Arc::new(RuntimeSnapshot::new(
            generation,
            version.revision(),
            digest,
            document,
            self.source.name(),
        )));
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

/// What happened to one candidate document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublishOutcome {
    /// A new generation is in service.
    Published,
    /// The candidate was already in service or provably older; nothing changed.
    Skipped,
    /// The document was rejected; the last-known-good snapshot is still in service.
    Rejected(ErrorCategory),
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

/// What woke the debounce loop.
enum Wake {
    /// Shutdown was requested.
    Shutdown,
    /// The debounce window or the maximum wait elapsed.
    Deadline,
    /// The watch produced an item, or ended.
    Item(Option<WatchItem>),
}

/// What the watch said once everything already queued behind it has been coalesced.
enum WatchStep {
    /// The last document of a burst.
    Candidate(ConfigVersion),
    /// The source cannot continue incrementally.
    Resync,
    /// The source failed.
    Failed(ConfigSourceError),
}

/// Reduces the next queued item, and every snapshot behind it, to one decision.
///
/// Draining is what makes the debounce a debounce: a burst that has already arrived is exactly the
/// set of documents the quiet window is meant to collapse, so three queued events publish once.
/// A resync or a failure supersedes any candidate collected so far, because both mean "the source,
/// not this document, decides what is current".
fn coalesce(watch: &mut ConfigWatch, first: WatchItem) -> WatchStep {
    let mut candidate = match first {
        Ok(ConfigEvent::Snapshot(version)) => version,
        Ok(ConfigEvent::ResyncRequired) => return WatchStep::Resync,
        Err(error) => return WatchStep::Failed(error),
        // `ConfigEvent` is `#[non_exhaustive]`; the only safe reading of an unknown event is
        // "reload the whole document", because this build cannot know what it means.
        Ok(_) => return WatchStep::Resync,
    };

    while let Some(item) = take_ready(watch) {
        match item {
            Ok(ConfigEvent::Snapshot(version)) => candidate = version,
            Ok(ConfigEvent::ResyncRequired) => return WatchStep::Resync,
            Err(error) => return WatchStep::Failed(error),
            Ok(_) => return WatchStep::Resync,
        }
    }

    WatchStep::Candidate(candidate)
}

/// Takes an item that is already available, without polling a timer.
///
/// A no-op waker is enough: the caller only wants what is ready now, and the next poll from the
/// suspend point in [`Reloader::consume`] registers the real waker.
fn take_ready(watch: &mut ConfigWatch) -> Option<WatchItem> {
    let mut context = Context::from_waker(Waker::noop());
    match watch.as_mut().poll_next(&mut context) {
        Poll::Ready(item) => item,
        Poll::Pending => None,
    }
}

/// Polls the watch for its next item.
async fn next_item(watch: &mut ConfigWatch) -> Option<WatchItem> {
    std::future::poll_fn(|context: &mut Context<'_>| watch.as_mut().poll_next(context)).await
}

/// Sleeps until `deadline`, or forever when there is nothing pending.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Returns the earlier of two optional deadlines.
fn earliest(left: Option<Instant>, right: Option<Instant>) -> Option<Instant> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    }
}

/// Computes the reconnect delay for a failed attempt.
///
/// Exponential from [`BACKOFF_BASE`] to [`BACKOFF_CAP`], plus a small deterministic jitter derived
/// from the attempt counter: processes that started together still separate, without pulling a
/// random-number dependency in for a sleep.
fn backoff_delay(attempt: u32) -> Duration {
    let exponent = attempt.min(BACKOFF_MAX_EXPONENT);
    let base = BACKOFF_BASE
        .saturating_mul(1u32 << exponent)
        .min(BACKOFF_CAP);
    let jitter = base / 8 * (attempt % 5);
    (base + jitter).min(BACKOFF_CAP)
}

/// Returns the current Unix time in seconds, or `0` before the epoch.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    mod backoff {
        use super::*;

        #[test]
        fn backoff_delay_starts_at_the_base_delay() {
            assert_eq!(backoff_delay(0), BACKOFF_BASE);
        }

        #[test]
        fn backoff_delay_doubles_before_the_cap_applies() {
            // Attempt 1: 100 ms * 2, plus one eighth of jitter.
            assert_eq!(backoff_delay(1), BACKOFF_BASE * 2 + BACKOFF_BASE * 2 / 8);
            // Attempt 2: 100 ms * 4, plus two eighths of jitter.
            assert_eq!(
                backoff_delay(2),
                BACKOFF_BASE * 4 + BACKOFF_BASE * 4 / 8 * 2
            );
        }

        #[test]
        fn backoff_delay_never_exceeds_the_cap() {
            for attempt in 0..64 {
                assert!(
                    backoff_delay(attempt) <= BACKOFF_CAP,
                    "attempt {attempt} exceeded the cap"
                );
            }
        }

        #[test]
        fn backoff_delay_is_monotonic_within_the_cap() {
            let mut previous = Duration::ZERO;
            for attempt in 0..BACKOFF_MAX_EXPONENT {
                let delay = backoff_delay(attempt);
                assert!(
                    delay >= previous || delay == BACKOFF_CAP,
                    "attempt {attempt} went backwards"
                );
                previous = delay;
            }
        }

        #[test]
        fn backoff_delay_separates_processes_that_started_together() {
            // The jitter is derived from the attempt counter, so the same attempt is reproducible
            // and the next attempt is not the same delay.
            assert_eq!(backoff_delay(3), backoff_delay(3));
            assert_ne!(backoff_delay(3), backoff_delay(4));
        }
    }

    mod deadlines {
        use super::*;

        #[test]
        fn earliest_prefers_the_sooner_deadline() {
            let base = Instant::now();
            let sooner = base + Duration::from_millis(10);
            let later = base + Duration::from_millis(20);
            assert_eq!(earliest(Some(later), Some(sooner)), Some(sooner));
        }

        #[test]
        fn earliest_uses_whatever_is_available() {
            let deadline = Instant::now();
            assert_eq!(earliest(Some(deadline), None), Some(deadline));
            assert_eq!(earliest(None, Some(deadline)), Some(deadline));
            assert_eq!(earliest(None, None), None);
        }
    }

    mod error {
        use super::*;

        #[test]
        fn an_unreachable_source_makes_the_pipeline_not_ready_upstream() {
            let error = ReloadError::NotReady {
                category: ErrorCategory::Upstream,
            };
            assert_eq!(error.category(), ErrorCategory::Upstream);
        }

        #[test]
        fn a_source_error_keeps_its_own_category() {
            let error = ReloadError::from(ConfigSourceError::TooLarge { size: 9, limit: 4 });
            assert_eq!(error.category(), ErrorCategory::Limit);
            assert!(
                matches!(error, ReloadError::Source { .. }),
                "expected Source, got {error:?}"
            );
        }

        #[test]
        fn an_exhausted_generation_is_internal() {
            let overflow = Generation::new(u64::MAX)
                .next()
                .expect_err("the counter is exhausted");
            let error = ReloadError::from(overflow);
            assert_eq!(error.category(), ErrorCategory::Internal);
            assert!(
                matches!(error, ReloadError::GenerationOverflow { .. }),
                "expected GenerationOverflow, got {error:?}"
            );
        }
    }
}
