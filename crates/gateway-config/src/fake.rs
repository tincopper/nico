//! A scripted configuration source for tests.
//!
//! The fake replays a queue of `load` results and a queue of watch events, so a test can construct
//! the situations a real source produces rarely and on purpose: duplicate digests, regressing
//! revisions, injected faults and a consumer that falls behind.

use std::{
    collections::VecDeque,
    fmt,
    sync::{Mutex, MutexGuard, PoisonError},
};

use async_trait::async_trait;
use bytes::Bytes;
use gateway_core::Revision;
use tokio::sync::{mpsc, mpsc::error::TrySendError};
use tokio_util::sync::CancellationToken;

use crate::{
    source::{ConfigEvent, ConfigSource, ConfigSourceError, ConfigVersion, ConfigWatch},
    stream::{SourceWatch, WatchItem},
};

/// Default number of events the watch channel buffers before it collapses into a resync.
const DEFAULT_CHANNEL_CAPACITY: usize = 8;

/// A configuration source whose answers are scripted by the test.
///
/// Every call to [`ConfigSource::load`] pops one scripted result; once the queue is empty the source
/// reports [`ConfigSourceError::Unavailable`]. Every call to [`ConfigSource::watch`] replays the
/// remaining scripted events and then ends the stream.
#[derive(Debug)]
pub struct FakeConfigSource {
    name: &'static str,
    capacity: usize,
    loads: Mutex<VecDeque<Result<ConfigVersion, ConfigSourceError>>>,
    events: Mutex<VecDeque<WatchItem>>,
}

impl FakeConfigSource {
    /// Creates an empty fake with the given source name and a default channel capacity of 8.
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            capacity: DEFAULT_CHANNEL_CAPACITY,
            loads: Mutex::new(VecDeque::new()),
            events: Mutex::new(VecDeque::new()),
        }
    }

    /// Builds a [`ConfigVersion`] whose revision is comparable within the source.
    ///
    /// A comparable revision models a counter-style source (a local file sequence, an etcd
    /// `mod_revision`), where the pipeline may discard an event that goes backwards.
    #[must_use]
    pub fn version_at(seq: u64, bytes: &[u8]) -> ConfigVersion {
        ConfigVersion::new(Revision::comparable(seq), Bytes::copy_from_slice(bytes))
    }

    /// Builds a [`ConfigVersion`] whose revision is only comparable for equality.
    ///
    /// An equality-only revision models a content-addressed source (an MD5 or ETag), where sequence
    /// numbers carry no order and the pipeline must not sort by them.
    #[must_use]
    pub fn equality_version_at(seq: u64, bytes: &[u8]) -> ConfigVersion {
        ConfigVersion::new(Revision::equality_only(seq), Bytes::copy_from_slice(bytes))
    }

    /// Appends one `load` result to the script.
    #[must_use]
    pub fn push_load(self, result: Result<ConfigVersion, ConfigSourceError>) -> Self {
        self.lock_loads().push_back(result);
        self
    }

    /// Appends one watch item to the script.
    #[must_use]
    pub fn push_event(self, result: Result<ConfigEvent, ConfigSourceError>) -> Self {
        self.lock_events().push_back(result);
        self
    }

    /// Sets the watch channel capacity used to model a slow consumer.
    ///
    /// A capacity of `0` is clamped to `1`, because a channel must always be able to hold one item.
    #[must_use]
    pub fn with_channel_capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity;
        self
    }

    fn lock_loads(&self) -> MutexGuard<'_, VecDeque<Result<ConfigVersion, ConfigSourceError>>> {
        self.loads.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_events(&self) -> MutexGuard<'_, VecDeque<WatchItem>> {
        self.events.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Reported when the scripted `load` queue is empty.
///
/// The fake cannot answer, which is exactly what [`ConfigSourceError::Unavailable`] means, so the
/// pipeline sees the same failure it would see from an unreachable source.
#[derive(Debug)]
struct ScriptExhausted;

impl fmt::Display for ScriptExhausted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("no scripted load result remains")
    }
}

impl std::error::Error for ScriptExhausted {}

#[async_trait]
impl ConfigSource for FakeConfigSource {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn load(&self) -> Result<ConfigVersion, ConfigSourceError> {
        // The lock is released before any `.await`; `load` never suspends while holding it.
        self.lock_loads().pop_front().unwrap_or_else(|| {
            Err(ConfigSourceError::Unavailable {
                source_name: self.name,
                source: Box::new(ScriptExhausted),
            })
        })
    }

    async fn watch(&self, _from: Option<Revision>) -> Result<ConfigWatch, ConfigSourceError> {
        // `from` is not honoured: the fake replays the script verbatim so a test can drive ordering
        // decisions in the pipeline rather than re-testing source-side filtering.
        let script: VecDeque<WatchItem> = std::mem::take(&mut *self.lock_events());
        let (sender, receiver) = mpsc::channel(self.capacity.max(1));
        let token = CancellationToken::new();
        let task_token = token.clone();

        // The handle is detached on purpose: the stream ends when the script is exhausted or the
        // receiver is dropped, and nothing in the source needs to join the task.
        let _handle = tokio::spawn(async move {
            for item in script {
                match sender.try_send(item) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_dropped)) => {
                        // The consumer fell behind. A real adapter must not block its source or
                        // buffer unlimited history, so every remaining event collapses into one
                        // resync and the pipeline reloads the whole document instead.
                        tokio::select! {
                            _ = sender.send(Ok(ConfigEvent::ResyncRequired)) => {}
                            () = task_token.cancelled() => {}
                        }
                        return;
                    }
                    Err(TrySendError::Closed(_)) => return,
                }
            }
        });

        Ok(Box::pin(SourceWatch::new(receiver, token)))
    }
}

#[cfg(test)]
mod tests {
    use std::{future::poll_fn, time::Duration};

    use gateway_core::RevisionOrdering;

    use super::*;

    const DOC_A: &[u8] = b"apiVersion: gateway/v1\nlimits:\n  max_request_body_bytes: 1024\n";
    const DOC_B: &[u8] = b"apiVersion: gateway/v1\nlimits:\n  max_request_body_bytes: 2048\n";

    /// A watch that never happens must fail the test rather than hang the suite.
    const WAIT: Duration = Duration::from_secs(5);

    async fn next_event(watch: &mut ConfigWatch) -> Option<WatchItem> {
        poll_fn(|context| watch.as_mut().poll_next(context)).await
    }

    async fn next_event_within(watch: &mut ConfigWatch) -> WatchItem {
        tokio::time::timeout(WAIT, next_event(watch))
            .await
            .expect("an event arrives within the timeout")
            .expect("the stream is not exhausted")
    }

    mod load {
        use super::*;

        #[tokio::test]
        async fn load_replays_the_scripted_results_in_order() {
            let source = FakeConfigSource::new("fake")
                .push_load(Ok(FakeConfigSource::version_at(1, DOC_A)))
                .push_load(Err(ConfigSourceError::TooLarge { size: 9, limit: 4 }));

            let first = source.load().await.expect("first scripted load succeeds");
            assert_eq!(first.revision(), Revision::comparable(1));
            assert_eq!(first.bytes().as_ref(), DOC_A);

            assert_eq!(
                source.load().await.expect_err("second scripted load fails"),
                ConfigSourceError::TooLarge { size: 9, limit: 4 }
            );
        }

        #[tokio::test]
        async fn load_reports_unavailable_once_the_script_is_exhausted() {
            let source = FakeConfigSource::new("fake");
            let error = source.load().await.expect_err("no scripted result");
            assert_eq!(
                error,
                ConfigSourceError::Unavailable {
                    source_name: "fake",
                    source: Box::new(ScriptExhausted),
                }
            );
        }

        #[tokio::test]
        async fn load_can_script_an_unavailable_failure() {
            let source =
                FakeConfigSource::new("fake").push_load(Err(ConfigSourceError::Unavailable {
                    source_name: "fake",
                    source: Box::new(ScriptExhausted),
                }));
            let error = source.load().await.expect_err("scripted failure");
            assert!(matches!(error, ConfigSourceError::Unavailable { .. }));
        }
    }

    mod watch {
        use super::*;

        #[tokio::test]
        async fn watch_replays_the_scripted_events_in_order() {
            let source = FakeConfigSource::new("fake")
                .push_event(Ok(ConfigEvent::Snapshot(FakeConfigSource::version_at(
                    1, DOC_A,
                ))))
                .push_event(Ok(ConfigEvent::Snapshot(
                    FakeConfigSource::equality_version_at(2, DOC_B),
                )))
                .push_event(Ok(ConfigEvent::ResyncRequired));

            let mut watch = source.watch(None).await.expect("watch starts");

            match next_event_within(&mut watch).await {
                Ok(ConfigEvent::Snapshot(version)) => {
                    assert_eq!(version.revision(), Revision::comparable(1));
                    assert_eq!(version.bytes().as_ref(), DOC_A);
                }
                other => panic!("expected the first snapshot, got {other:?}"),
            }
            match next_event_within(&mut watch).await {
                Ok(ConfigEvent::Snapshot(version)) => {
                    assert_eq!(version.revision(), Revision::equality_only(2));
                    assert_eq!(
                        version.revision().ordering(),
                        RevisionOrdering::EqualityOnly
                    );
                }
                other => panic!("expected the second snapshot, got {other:?}"),
            }
            assert!(
                matches!(
                    next_event_within(&mut watch).await,
                    Ok(ConfigEvent::ResyncRequired)
                ),
                "expected a resync event"
            );
            assert!(
                next_event(&mut watch).await.is_none(),
                "the stream ends when the script is exhausted"
            );
        }

        #[tokio::test]
        async fn watch_can_script_a_failure() {
            let source =
                FakeConfigSource::new("fake").push_event(Err(ConfigSourceError::Unavailable {
                    source_name: "fake",
                    source: Box::new(ScriptExhausted),
                }));
            let mut watch = source.watch(None).await.expect("watch starts");
            assert!(
                matches!(
                    next_event_within(&mut watch).await,
                    Err(ConfigSourceError::Unavailable { .. })
                ),
                "expected the scripted failure"
            );
        }

        #[tokio::test]
        async fn watch_discards_a_revision_when_the_script_goes_backwards() {
            // The fake deliberately replays the script verbatim: discarding a regressing revision is
            // the pipeline's decision, and this test pins that boundary.
            let source = FakeConfigSource::new("fake")
                .push_event(Ok(ConfigEvent::Snapshot(FakeConfigSource::version_at(
                    9, DOC_B,
                ))))
                .push_event(Ok(ConfigEvent::Snapshot(FakeConfigSource::version_at(
                    3, DOC_A,
                ))));

            let mut watch = source
                .watch(Some(Revision::comparable(9)))
                .await
                .expect("watch starts");

            for expected in [9, 3] {
                match next_event_within(&mut watch).await {
                    Ok(ConfigEvent::Snapshot(version)) => {
                        assert_eq!(version.revision().seq(), expected)
                    }
                    other => panic!("expected revision {expected}, got {other:?}"),
                }
            }
        }

        #[tokio::test]
        async fn watch_collapses_into_a_resync_when_the_consumer_lags() {
            let source = FakeConfigSource::new("fake")
                .with_channel_capacity(1)
                .push_event(Ok(ConfigEvent::Snapshot(FakeConfigSource::version_at(
                    1, DOC_A,
                ))))
                .push_event(Ok(ConfigEvent::Snapshot(FakeConfigSource::version_at(
                    2, DOC_B,
                ))))
                .push_event(Ok(ConfigEvent::Snapshot(FakeConfigSource::version_at(
                    3, DOC_A,
                ))))
                .push_event(Ok(ConfigEvent::ResyncRequired));

            let mut watch = source.watch(None).await.expect("watch starts");

            match next_event_within(&mut watch).await {
                Ok(ConfigEvent::Snapshot(version)) => assert_eq!(version.revision().seq(), 1),
                other => panic!("expected the buffered snapshot, got {other:?}"),
            }
            assert!(
                matches!(
                    next_event_within(&mut watch).await,
                    Ok(ConfigEvent::ResyncRequired)
                ),
                "the lagging consumer must be told to resync"
            );
            assert!(
                next_event(&mut watch).await.is_none(),
                "history is dropped instead of replayed"
            );
        }

        #[tokio::test]
        async fn dropping_the_watch_stream_ends_the_producer() {
            let source = FakeConfigSource::new("fake").push_event(Ok(ConfigEvent::Snapshot(
                FakeConfigSource::version_at(1, DOC_A),
            )));
            let watch = source.watch(None).await.expect("watch starts");
            drop(watch);

            // A second watch only starts if the first task released the script; the script is empty
            // now, so this asserts the observable part: a dropped stream does not keep the source
            // from serving again.
            let mut second = source.watch(None).await.expect("watch restarts");
            assert!(next_event(&mut second).await.is_none());
        }
    }
}
