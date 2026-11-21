//! The store one mock server serves, driven directly by the test that owns the handle.
//!
//! It keeps the full revision history of one key because that is what a watch replays from, and a
//! live broadcast because that is how a watcher learns about a change that lands after it subscribed.
//! Nothing here knows the wire: the services read this state and translate it.

use std::sync::{Mutex, MutexGuard, PoisonError};

use tokio::sync::broadcast;

/// One revision of the watched key: the document bytes stored at that revision.
///
/// An empty value is the tombstone a delete leaves behind.
pub(super) type History = Vec<(i64, Vec<u8>)>;

/// Unlocks a poisoned lock by keeping the state: a test harness with a panicked test is already a
/// failed test, so the data is safe to read.
pub(super) fn held<'a, T>(
    lock: Result<MutexGuard<'a, T>, PoisonError<MutexGuard<'a, T>>>,
) -> MutexGuard<'a, T> {
    lock.unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One live change the store broadcasts to its watch streams.
#[derive(Debug, Clone)]
pub(super) struct LiveChange {
    pub(super) revision: i64,
    pub(super) value: Vec<u8>,
}

/// The shared state one server instance serves, driven directly by its tests.
#[derive(Debug)]
pub struct StoreState {
    /// The full history of the key, ascending by revision. An empty value is a delete tombstone.
    history: Mutex<History>,
    /// The revision the store hands out next; one past the newest history entry.
    next_revision: Mutex<i64>,
    /// When set, every operation is refused with `permission_denied`.
    require_auth: Mutex<bool>,
    /// When set, every watch starting at or before this revision is canceled naming it.
    compact_to: Mutex<Option<i64>>,
    /// The live tail every watch stream subscribes to.
    live: broadcast::Sender<LiveChange>,
}

impl StoreState {
    /// Builds an empty store: no history, nothing compacted, no auth required.
    pub(super) fn new() -> Self {
        let (live, _initial) = broadcast::channel(64);
        Self {
            history: Mutex::new(History::new()),
            next_revision: Mutex::new(0),
            require_auth: Mutex::new(false),
            compact_to: Mutex::new(None),
            live,
        }
    }

    /// Stores a new revision of the key, returns its mod revision, and broadcasts the change.
    pub fn put(&self, document: &[u8]) -> i64 {
        self.append(document.to_vec())
    }

    /// Deletes the key: a tombstone revision, broadcast like any other change.
    pub fn delete(&self) -> i64 {
        self.append(Vec::new())
    }

    fn append(&self, value: Vec<u8>) -> i64 {
        let revision = {
            let mut next = held(self.next_revision.lock());
            *next += 1;
            *next
        };
        held(self.history.lock()).push((revision, value.clone()));
        // No receivers is the normal case before a watch is established; the change is still in
        // the history for replay.
        let _ = self.live.send(LiveChange { revision, value });
        revision
    }

    /// Sets the auth requirement. Every operation afterwards is refused until cleared.
    ///
    /// This is a *server-side* refusal — `permission_denied`, as a cluster with authentication
    /// enabled answers a client that presented no token — and it is independent of the credentials a
    /// server is started with, which refuse at the token check instead.
    pub fn set_require_auth(&self, required: bool) {
        *held(self.require_auth.lock()) = required;
    }

    /// Compacts history at or below `revision`: every watch starting there is canceled naming it,
    /// exactly as the roadmap's semantics state.
    pub fn compact(&self, revision: i64) {
        *held(self.compact_to.lock()) = Some(revision);
    }

    pub(super) fn auth_refused(&self) -> bool {
        *held(self.require_auth.lock())
    }

    pub(super) fn current_revision(&self) -> i64 {
        *held(self.next_revision.lock())
    }

    pub(super) fn history(&self) -> History {
        held(self.history.lock()).clone()
    }

    pub(super) fn compact_to(&self) -> Option<i64> {
        *held(self.compact_to.lock())
    }

    pub(super) fn subscribe(&self) -> broadcast::Receiver<LiveChange> {
        self.live.subscribe()
    }
}
