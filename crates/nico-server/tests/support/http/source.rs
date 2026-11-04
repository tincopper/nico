//! The configuration source the HTTP scenarios publish into.
//!
//! A test needs two things a real source cannot give it: a document that is served *before* the
//! first request (the usual case), and a source that keeps its answer so "no snapshot published
//! yet" is a state the test can observe instead of a race against the pipeline's startup. Both are
//! the same type here, and publishing a second document is how a scenario reaches a reload.

use std::{
    io,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use bytes::Bytes;
use nico_config::{ConfigEvent, ConfigSource, ConfigSourceError, ConfigVersion, ConfigWatch};
use nico_core::Revision;
use tokio::sync::{mpsc, watch};

/// A configuration source the test drives directly.
///
/// `load` answers with the published document, or stays pending until one exists; the watch emits a
/// [`ConfigEvent::Snapshot`] for every later publish. A source that holds its answer is what makes
/// "a request before any snapshot" reproducible instead of a race against the pipeline's startup.
#[derive(Debug)]
pub(crate) struct TestSource {
    current: watch::Sender<Option<ConfigVersion>>,
    watchers: Mutex<Vec<mpsc::Sender<WatchItem>>>,
    sequence: AtomicU64,
}

/// One item of a [`TestSource`] watch, mirrored from `nico_config`.
type WatchItem = Result<ConfigEvent, ConfigSourceError>;

impl TestSource {
    /// A source that already serves `document`.
    pub(crate) fn with_document(document: &str) -> Arc<Self> {
        let source = Self::holding();
        source.publish(document);
        source
    }

    /// A source with nothing to serve: `load` stays pending until [`Self::publish`] is called.
    pub(crate) fn holding() -> Arc<Self> {
        let (current, _receiver) = watch::channel(None);
        Arc::new(Self {
            current,
            watchers: Mutex::new(Vec::new()),
            sequence: AtomicU64::new(0),
        })
    }

    /// Makes `document` the current one and reports it on the watch.
    pub(crate) fn publish(&self, document: &str) {
        let seq = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let version = ConfigVersion::new(
            Revision::comparable(seq),
            Bytes::copy_from_slice(document.as_bytes()),
        );
        self.current.send_replace(Some(version.clone()));
        let event = ConfigEvent::Snapshot(version);
        let mut watchers = lock(&self.watchers);
        watchers.retain(|watcher| match watcher.try_send(Ok(event.clone())) {
            Ok(()) => true,
            // A lagging fixture is a fixture bug, not a source behaviour worth modelling: the
            // pipeline always drains between two publishes in these tests.
            Err(mpsc::error::TrySendError::Full(_item)) => true,
            Err(mpsc::error::TrySendError::Closed(_item)) => false,
        });
    }
}

#[async_trait]
impl ConfigSource for TestSource {
    fn name(&self) -> &'static str {
        "test-source"
    }

    async fn load(&self) -> Result<ConfigVersion, ConfigSourceError> {
        let mut current = self.current.subscribe();
        loop {
            let published = current.borrow_and_update().clone();
            if let Some(version) = published {
                return Ok(version);
            }
            if current.changed().await.is_err() {
                return Err(ConfigSourceError::Unavailable {
                    source_name: self.name(),
                    source: Box::new(io::Error::other("the test source was dropped")),
                });
            }
        }
    }

    async fn watch(&self, from: Option<Revision>) -> Result<ConfigWatch, ConfigSourceError> {
        // Bounded on purpose: a test that stops draining its own source must block, not grow.
        let (watcher, receiver) = mpsc::channel(16);
        {
            let mut watchers = lock(&self.watchers);
            watchers.retain(|existing| !existing.is_closed());
            watchers.push(watcher);
        }
        // Register first, then compare: a publish in between is delivered twice, which the pipeline
        // de-duplicates by digest, while a publish missed entirely would be lost.
        let current = self.current.borrow().clone();
        let pending = match current {
            Some(version) if from.is_none_or(|from| version.revision().seq() > from.seq()) => {
                Some(Ok(ConfigEvent::Snapshot(version)))
            }
            _ => None,
        };
        Ok(Box::pin(TestWatch { receiver, pending }))
    }
}

/// The receiving half of a [`TestSource`] watch.
struct TestWatch {
    receiver: mpsc::Receiver<WatchItem>,
    pending: Option<WatchItem>,
}

impl futures_core::Stream for TestWatch {
    type Item = WatchItem;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        if let Some(item) = this.pending.take() {
            return std::task::Poll::Ready(Some(item));
        }
        this.receiver.poll_recv(context)
    }
}

/// Locks the watcher list, recovering it when a previous holder panicked.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
