//! Unit tests for the local source: reading a document, and what a trigger turns into.
//!
//! Everything here is a **decision**, not an observation. Reading is a file read; a trigger is
//! pushed through the same channel the file backend feeds, so the watch task can be driven without
//! a watcher, a signal handler or a running kernel notification stream.
//!
//! The behaviour that *is* an observation — an atomic rename, a deleted-and-recreated document, a
//! Kubernetes-style symlink swap, a SIGHUP — lives in `tests/local_watch.rs`. Those scenarios need a
//! real filesystem and real notifications, cost seconds each, and would make the commit gate slow
//! enough to be worth bypassing.

use std::{path::Path, time::Duration};

use nico_core::RevisionOrdering;
use tempfile::TempDir;

use super::*;

const DOC_A: &[u8] =
    b"apiVersion: gateway/v1\nrequest:\n  max_body_bytes: 1024\n  timeout_ms: 1000\n";
const DOC_B: &[u8] =
    b"apiVersion: gateway/v1\nrequest:\n  max_body_bytes: 2048\n  timeout_ms: 2000\n";

/// Budget for an injected trigger's item. Nothing here waits on the operating system, so this only
/// has to be longer than a scheduling round trip.
const WAIT: Duration = Duration::from_secs(10);

fn config(path: &Path) -> LocalConfig {
    LocalConfig::new(path).with_max_bytes(4096)
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("fixture write succeeds");
}

mod load {
    use super::*;

    #[tokio::test]
    async fn load_returns_the_file_bytes_with_a_monotonic_comparable_revision() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("resources.yaml");
        write(&path, DOC_A);
        let source = LocalFileSource::new(config(&path)).expect("configuration is valid");

        let first = source.load().await.expect("load succeeds");
        assert_eq!(first.bytes().as_ref(), DOC_A);
        assert_eq!(first.revision().ordering(), RevisionOrdering::Comparable);

        let second = source.load().await.expect("load succeeds");
        assert!(
            second.revision().seq() > first.revision().seq(),
            "expected a later revision, got {:?} after {:?}",
            second.revision(),
            first.revision()
        );
    }

    #[tokio::test]
    async fn load_rejects_a_document_larger_than_the_limit() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("resources.yaml");
        write(&path, DOC_A);
        let source =
            LocalFileSource::new(config(&path).with_max_bytes(16)).expect("configuration is valid");

        // The reader stops one byte past the ceiling instead of buffering the file to learn its exact
        // length, so the reported size is a lower bound: the first byte it refused to accept.
        assert_eq!(
            source.load().await.expect_err("the file is too large"),
            ConfigSourceError::TooLarge {
                size: 17,
                limit: 16
            }
        );
    }

    #[tokio::test]
    async fn load_rejects_a_document_that_is_not_utf8() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("resources.yaml");
        write(&path, &[0xff, 0xfe, 0x00]);
        let source = LocalFileSource::new(config(&path)).expect("configuration is valid");

        assert_eq!(
            source.load().await.expect_err("not utf-8"),
            ConfigSourceError::NotUtf8
        );
    }

    #[tokio::test]
    async fn load_reports_unavailable_when_the_file_is_missing() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("resources.yaml");
        let source = LocalFileSource::new(config(&path)).expect("configuration is valid");

        let error = source.load().await.expect_err("the file does not exist");
        assert!(
            matches!(
                error,
                ConfigSourceError::Unavailable {
                    source_name: "local",
                    ..
                }
            ),
            "expected Unavailable from the local source, got {error:?}"
        );
        assert_eq!(error.category(), nico_core::ErrorCategory::Upstream);
    }

    #[tokio::test]
    async fn load_reports_unavailable_when_the_path_is_a_directory() {
        let directory = TempDir::new().expect("temporary directory");
        let source =
            LocalFileSource::new(config(directory.path())).expect("configuration is valid");

        let error = source
            .load()
            .await
            .expect_err("a directory is not a document");
        assert!(
            matches!(error, ConfigSourceError::Unavailable { .. }),
            "expected Unavailable, got {error:?}"
        );
    }
}

mod construction {
    use super::*;

    #[test]
    fn new_rejects_a_zero_size_limit() {
        let source = LocalFileSource::new(LocalConfig::new("resources.yaml").with_max_bytes(0));
        assert!(
            matches!(
                source,
                Err(ConfigSourceError::Unavailable {
                    source_name: "local",
                    ..
                })
            ),
            "expected an Unavailable construction error"
        );
    }

    #[test]
    fn new_rejects_a_path_without_a_file_name() {
        let source = LocalFileSource::new(LocalConfig::new("/"));
        assert!(
            matches!(source, Err(ConfigSourceError::Unavailable { .. })),
            "expected an Unavailable construction error"
        );
    }

    #[test]
    fn config_defaults_to_a_one_mebibyte_bound_and_both_trigger_sources() {
        let config = LocalConfig::new("resources.yaml");
        assert_eq!(config.max_bytes(), 1024 * 1024);
        assert_eq!(config.watch_mode(), WatchMode::NotifyAndSignal);
    }
}

mod watch_task {
    use super::*;

    /// A watch task with **no operating-system backend**: the trigger channel is its only input.
    ///
    /// That is the seam this module exists to use. `WatchTriggers::changed` selects between the
    /// file-notification channel and a SIGHUP; leaving both sources empty makes the channel the only
    /// thing that can wake the task, so "what does a trigger produce" becomes a unit test instead of
    /// a filesystem observation. The real backends are exercised by `tests/local_watch.rs`, which is
    /// an integration target precisely because they cannot run without the kernel.
    fn spawn_injected(path: &Path) -> InjectedWatch {
        let source = LocalFileSource::new(config(path)).expect("configuration is valid");
        let (trigger, trigger_receiver) = mpsc::channel::<()>(1);
        let (sink_sender, events) = mpsc::channel::<WatchItem>(2);
        let token = CancellationToken::new();
        let (finished_sender, finished) = oneshot::channel();

        let watchers = WatchTriggers {
            _watcher: None,
            #[cfg(unix)]
            sighup: None,
        };
        let inner = Arc::clone(&source.inner);
        let task_token = token.clone();
        tokio::spawn(async move {
            run_watch(
                inner,
                EventSink::new(sink_sender),
                task_token,
                trigger_receiver,
                watchers,
                finished_sender,
            )
            .await;
        });

        InjectedWatch {
            trigger,
            events,
            token,
            finished,
        }
    }

    /// The handles one injected watch needs: something to trigger it, a stream to read, a way to
    /// stop it, and the guard that closes when its task ends.
    struct InjectedWatch {
        trigger: mpsc::Sender<()>,
        events: mpsc::Receiver<WatchItem>,
        token: CancellationToken,
        finished: oneshot::Receiver<()>,
    }

    impl InjectedWatch {
        /// Pushes one trigger, which is what a file notification would do.
        async fn trigger(&self) {
            self.trigger.send(()).await.expect("the task is running");
        }

        /// Reads the single item the trigger produced, failing the budget with a diagnosis.
        async fn next(&mut self) -> WatchItem {
            tokio::time::timeout(WAIT, self.events.recv())
                .await
                .expect("a triggered reload produces an item within the budget")
                .expect("the task sends exactly the item it produced")
        }
    }

    #[tokio::test]
    async fn a_trigger_delivers_the_current_document_as_a_snapshot() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("resources.yaml");
        write(&path, DOC_A);
        let mut watch = spawn_injected(&path);

        watch.trigger().await;

        match watch.next().await {
            Ok(ConfigEvent::Snapshot(version)) => {
                assert_eq!(
                    version.bytes().as_ref(),
                    DOC_A,
                    "the snapshot carries the document as it is now"
                );
                assert!(
                    version.revision().seq() >= 1,
                    "a triggered reload carries a revision"
                );
            }
            other => panic!("expected a snapshot, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_trigger_re_reads_the_document_rather_than_replaying_the_first_read() {
        // The trigger means "look again", not "deliver what you saw last time": a change written
        // between the first reload and the second must be what the second one reports.
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("resources.yaml");
        write(&path, DOC_A);
        let mut watch = spawn_injected(&path);

        watch.trigger().await;
        assert!(matches!(watch.next().await, Ok(ConfigEvent::Snapshot(_))));

        write(&path, DOC_B);
        watch.trigger().await;
        match watch.next().await {
            Ok(ConfigEvent::Snapshot(version)) => assert_eq!(version.bytes().as_ref(), DOC_B),
            other => panic!("expected the rewritten document, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_trigger_for_a_missing_document_delivers_the_error() {
        // Nothing is ever written: the trigger finds nothing to read, and the item delivered is the
        // failure rather than silence. That is what lets the pipeline mark itself degraded.
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("resources.yaml");
        let mut watch = spawn_injected(&path);

        watch.trigger().await;

        let item = watch.next().await;
        assert!(
            matches!(item, Err(ConfigSourceError::Unavailable { .. })),
            "a missing document is Unavailable, got {item:?}"
        );
    }

    #[tokio::test]
    async fn cancelling_the_token_ends_the_task() {
        // The task must not outlive the stream that owns it: a leaked watch task keeps a watcher and
        // a channel alive for the rest of the process.
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("resources.yaml");
        write(&path, DOC_A);
        let watch = spawn_injected(&path);

        watch.token.cancel();

        match tokio::time::timeout(WAIT, watch.finished).await {
            Ok(Ok(())) => panic!("the guard must never receive a value"),
            // A closed channel without a value means the task dropped the guard, i.e. it ended.
            Ok(Err(_closed)) => {}
            Err(_elapsed) => panic!("the watch task did not stop within the budget"),
        }
    }
}

mod sink {
    use super::*;

    #[tokio::test]
    async fn a_full_channel_delivers_the_item_instead_of_dropping_it() {
        // A full channel is backpressure, not a reason to keep the item for "the next trigger": a burst
        // of file events has no next trigger, so an item held back is the last change of the burst and
        // the configuration never reaches the pipeline.
        let (sender, mut receiver) = mpsc::channel::<WatchItem>(WATCH_CHANNEL_CAPACITY);
        let sink = EventSink::new(sender);
        for _ in 0..WATCH_CHANNEL_CAPACITY {
            assert!(
                sink.send(Err(ConfigSourceError::NotUtf8)).await,
                "a drained channel accepts every item"
            );
        }

        // The next send must wait for capacity, and then land: nothing is dropped and nothing is
        // deferred.
        let waiting = tokio::spawn(async move { sink.send(Err(ConfigSourceError::NotUtf8)).await });
        tokio::task::yield_now().await;
        assert!(
            !waiting.is_finished(),
            "a full channel makes the send wait rather than return early"
        );

        assert!(receiver.recv().await.is_some(), "the consumer drains one");
        assert!(
            waiting.await.expect("the send task does not panic"),
            "the waiting item is delivered"
        );
        for _ in 0..WATCH_CHANNEL_CAPACITY {
            assert!(
                receiver.try_recv().is_ok(),
                "every accepted item is on the channel"
            );
        }
        assert!(
            receiver.try_recv().is_err(),
            "nothing was queued twice or invented"
        );
    }

    #[tokio::test]
    async fn a_closed_channel_reports_that_the_subscriber_is_gone() {
        let (sender, receiver) = mpsc::channel::<WatchItem>(WATCH_CHANNEL_CAPACITY);
        let sink = EventSink::new(sender);
        drop(receiver);

        assert!(
            !sink.send(Err(ConfigSourceError::NotUtf8)).await,
            "a dropped watch ends the task instead of retrying forever"
        );
    }
}

mod relevance {
    use super::*;

    #[test]
    fn the_document_path_itself_is_relevant() {
        let watched = Path::new("/etc/gateway");
        let target = watched.join("resources.yaml");
        assert!(is_relevant(std::slice::from_ref(&target), &target, watched));
    }

    #[test]
    fn a_sibling_rename_is_relevant() {
        let watched = Path::new("/etc/gateway");
        let target = watched.join("resources.yaml");
        let pending = watched.join("..data_tmp");
        assert!(is_relevant(&[pending], &target, watched));
    }

    #[test]
    fn the_watched_directory_itself_is_relevant() {
        let watched = Path::new("/etc/gateway");
        let target = watched.join("resources.yaml");
        assert!(is_relevant(&[watched.to_path_buf()], &target, watched));
    }

    #[test]
    fn a_deeper_path_is_not_relevant() {
        let watched = Path::new("/etc/gateway");
        let target = watched.join("resources.yaml");
        let nested = watched.join("..data/resources.yaml");
        assert!(!is_relevant(&[nested], &target, watched));
    }

    #[test]
    fn an_unrelated_sibling_is_not_relevant_for_a_plain_file() {
        let watched = Path::new("/etc/gateway");
        let target = watched.join("resources.yaml");
        assert!(!is_relevant(
            &[PathBuf::from("/etc/other.yaml")],
            &target,
            watched
        ));
    }
}
