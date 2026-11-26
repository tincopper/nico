//! The reloader's inputs: what it publishes into, and how a pipeline is paced and bounded.
//!
//! These three types are the configuration a caller states before a [`Reloader`](super::Reloader)
//! runs; none of them hold reloader state. They live apart from the loop so the loop's file keeps
//! only the state machine, and a caller that only builds settings never reads the pipeline.
//!
//! - [`Publication`] — the two readers one generation is published to, handed over together.
//! - [`ReloadLimits`] — the de-bounce window and the postponement ceiling.
//! - [`ReloadSettings`] — the cache, the parse bounds, the trust policy and the shutdown token,
//!   constructed as one value so a cache cannot be paired with the wrong trust policy unnoticed.

use std::{sync::Arc, time::Duration};

use nico_config::CacheStore;
use nico_core::{ParseLimits, SnapshotHandle};
use nico_http::HttpStateHandle;
use tokio_util::sync::CancellationToken;

/// The two readers one generation is published to.
///
/// They are handed over together because they are written together: `status` is the
/// configuration-state fact source an operator and the readiness check read, `requests` is the one
/// value the request path reads. A reloader given only one of them could publish a generation the
/// other reader never sees, and for an authentication plugin that is a fail-open request path.
pub struct Publication {
    pub(super) status: Arc<SnapshotHandle>,
    pub(super) requests: Arc<HttpStateHandle>,
}

impl Publication {
    /// Creates the pair a reloader publishes into.
    #[must_use]
    pub const fn new(status: Arc<SnapshotHandle>, requests: Arc<HttpStateHandle>) -> Self {
        Self { status, requests }
    }
}

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

/// How one pipeline runs: what it may do with a document, and when it stops.
///
/// These are the values a process derives from its bootstrap configuration and a test states
/// directly. They travel as one value because they are one decision — how a document is paced,
/// bounded and persisted — and because a constructor per knob would let a caller pair a cache with the
/// wrong trust policy without anything noticing.
#[derive(Debug, Clone)]
pub struct ReloadSettings {
    pub(super) cache: Option<CacheStore>,
    limits: ReloadLimits,
    pub(super) parse_limits: ParseLimits,
    pub(super) allow_bootstrap_cache: bool,
    pub(super) shutdown: CancellationToken,
}

impl ReloadSettings {
    /// Creates the settings one pipeline runs with.
    ///
    /// `cache` is written after every successful publish; it is only *read* when startup fails and
    /// `allow_bootstrap_cache` is set, so passing a store never changes the running behaviour of a
    /// healthy process by itself.
    #[must_use]
    pub const fn new(
        cache: Option<CacheStore>,
        limits: ReloadLimits,
        parse_limits: ParseLimits,
        allow_bootstrap_cache: bool,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            cache,
            limits,
            parse_limits,
            allow_bootstrap_cache,
            shutdown,
        }
    }

    /// Returns the pacing limits the pipeline runs with.
    #[must_use]
    pub const fn limits(&self) -> ReloadLimits {
        self.limits
    }
}
