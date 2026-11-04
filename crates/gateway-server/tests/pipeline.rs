//! Pipeline behaviour: debouncing, de-duplication, ordering, failure handling and shutdown.
//!
//! Every test runs on a paused clock (`tokio::time::pause`) and advances it explicitly, so a
//! 200 ms debounce window costs no wall-clock time and the order of publish decisions is
//! reproducible. The clock is only allowed to move inside [`Harness::advance_until`], which makes
//! "nothing else happened" a provable statement rather than a race.
//!
//! The watch is driven by [`ChannelSource`] rather than `FakeConfigSource`: a scripted fake enqueues
//! its whole script at once, and a debounce is *supposed* to collapse that burst into one publish, so
//! it cannot express "an event, then a quiet window, then another event". `FakeConfigSource` is
//! still used where its own behaviour is the subject, namely a consumer that falls behind.

// The panic-free policy protects request paths. A test helper that cannot build its fixture has
// nothing useful to return, and clippy only recognises the lint exemption inside `#[test]`
// functions, not in the helpers they call.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers fail loudly when their fixture cannot be built"
)]

use std::{
    collections::VecDeque,
    fmt,
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    task::{Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use futures_core::Stream;
use gateway_config::{
    CacheMeta, CacheStore, ConfigEvent, ConfigSource, ConfigSourceError, ConfigVersion,
    ConfigWatch, FakeConfigSource,
};
use gateway_core::{
    ConfigDigest, ConfigStatus, ErrorCategory, ParseLimits, Revision, SnapshotHandle,
};
use gateway_server::{BootstrapConfig, ReloadError, ReloadLimits, Reloader, serve};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

const DOC_A: &[u8] = b"\
apiVersion: gateway/v1
limits:
  max_request_body_bytes: 1024
  request_timeout_ms: 1000
";

const DOC_B: &[u8] = b"\
apiVersion: gateway/v1
limits:
  max_request_body_bytes: 2048
  request_timeout_ms: 2000
";

const DOC_C: &[u8] = b"\
apiVersion: gateway/v1
limits:
  max_request_body_bytes: 4096
  request_timeout_ms: 3000
";

/// A document that parses as YAML but violates the schema, which is what a bad edit looks like.
const INVALID_DOC: &[u8] = b"apiVersion: gateway/v1\nlimits: [not, a, mapping]\n";

const DEBOUNCE: Duration = Duration::from_millis(200);
const MAX_WAIT: Duration = Duration::from_millis(1_000);

/// Clock step used by [`Harness::advance_until`]; smaller than any pipeline timer on purpose.
const STEP: Duration = Duration::from_millis(10);

/// Enough steps to walk far past every timer the pipeline can arm.
const MAX_STEPS: u32 = 5_000;

/// One item of a configuration watch, mirrored from `gateway_config`.
type WatchItem = Result<ConfigEvent, ConfigSourceError>;

/// A source the test drives by hand: scripted loads, and a watch that stays open until the test
/// stops pushing.
///
/// The watch never ends on its own, which is what lets a test assert "the pipeline stayed in this
/// state" instead of racing a disconnect the pipeline would immediately try to recover from.
#[derive(Debug)]
struct ChannelSource {
    name: &'static str,
    state: Mutex<SourceState>,
}

#[derive(Debug, Default)]
struct SourceState {
    loads: VecDeque<Result<ConfigVersion, ConfigSourceError>>,
    sender: Option<mpsc::Sender<WatchItem>>,
}

impl ChannelSource {
    fn new(name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            name,
            state: Mutex::new(SourceState::default()),
        })
    }

    /// Appends one `load` result. Once the queue is empty the source reports `Unavailable`.
    fn push_load(&self, result: Result<ConfigVersion, ConfigSourceError>) {
        self.lock().loads.push_back(result);
    }

    /// Sends one watch event, waiting for the pipeline to be subscribed and to have room.
    async fn push(&self, item: WatchItem) {
        let mut attempts = 0u32;
        let sender = loop {
            let sender = self.lock().sender.clone();
            if let Some(sender) = sender {
                break sender;
            }
            attempts += 1;
            assert!(
                attempts < 10_000,
                "the pipeline never subscribed to the watch"
            );
            tokio::task::yield_now().await;
        };
        sender
            .send(item)
            .await
            .expect("the pipeline still reads the watch");
    }

    /// Sends one snapshot event built from a comparable revision.
    async fn push_comparable(&self, seq: u64, bytes: &[u8]) {
        self.push(Ok(ConfigEvent::Snapshot(ConfigVersion::new(
            Revision::comparable(seq),
            bytes.to_vec().into(),
        ))))
        .await;
    }

    /// Sends one snapshot event built from an equality-only revision.
    async fn push_equality_only(&self, seq: u64, bytes: &[u8]) {
        self.push(Ok(ConfigEvent::Snapshot(ConfigVersion::new(
            Revision::equality_only(seq),
            bytes.to_vec().into(),
        ))))
        .await;
    }

    fn lock(&self) -> MutexGuard<'_, SourceState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Reported when the scripted `load` queue is empty.
#[derive(Debug)]
struct ScriptExhausted;

impl fmt::Display for ScriptExhausted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("no scripted load result remains")
    }
}

impl std::error::Error for ScriptExhausted {}

#[async_trait]
impl ConfigSource for ChannelSource {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn load(&self) -> Result<ConfigVersion, ConfigSourceError> {
        let next = self.lock().loads.pop_front();
        next.unwrap_or_else(|| {
            Err(ConfigSourceError::Unavailable {
                source_name: self.name,
                source: Box::new(ScriptExhausted),
            })
        })
    }

    async fn watch(&self, _from: Option<Revision>) -> Result<ConfigWatch, ConfigSourceError> {
        let (sender, receiver) = mpsc::channel(8);
        self.lock().sender = Some(sender);
        Ok(Box::pin(HandDrivenWatch { receiver }))
    }
}

/// The receiving half of a hand-driven watch.
struct HandDrivenWatch {
    receiver: mpsc::Receiver<WatchItem>,
}

impl Stream for HandDrivenWatch {
    type Item = WatchItem;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().receiver.poll_recv(context)
    }
}

/// A running pipeline plus the handle it publishes into.
struct Harness {
    handle: Arc<SnapshotHandle>,
    shutdown: CancellationToken,
    task: JoinHandle<Result<(), ReloadError>>,
}

impl Harness {
    fn start(source: Arc<dyn ConfigSource>) -> Self {
        Self::start_with(source, None, false)
    }

    fn start_with(
        source: Arc<dyn ConfigSource>,
        cache: Option<CacheStore>,
        allow_bootstrap_cache: bool,
    ) -> Self {
        let handle = SnapshotHandle::new();
        let shutdown = CancellationToken::new();
        let reloader = Reloader::new(
            source,
            Arc::clone(&handle),
            cache,
            ReloadLimits::new(DEBOUNCE, MAX_WAIT),
            ParseLimits::new(4096),
            allow_bootstrap_cache,
            shutdown.clone(),
        );
        let task = tokio::spawn(reloader.run());
        Self {
            handle,
            shutdown,
            task,
        }
    }

    fn status(&self) -> ConfigStatus {
        self.handle.status()
    }

    fn generation(&self) -> u64 {
        self.handle.status().generation
    }

    fn ready(&self) -> bool {
        self.handle.status().ready
    }

    fn body_limit(&self) -> Option<u32> {
        self.handle
            .current()
            .map(|snapshot| snapshot.document().limits().max_request_body_bytes())
    }

    /// Advances the paused clock in [`STEP`] increments until `condition` holds.
    ///
    /// The test task is always ready, so the runtime never auto-advances the clock behind the test's
    /// back: the pipeline can only make timer-driven progress here.
    async fn advance_until(&self, condition: impl Fn(&Self) -> bool) -> ConfigStatus {
        for _ in 0..MAX_STEPS {
            assert!(
                !self.task.is_finished(),
                "the pipeline stopped before the condition was met: {:?}",
                self.status()
            );
            if condition(self) {
                return self.status();
            }
            tokio::time::advance(STEP).await;
        }
        panic!(
            "the condition was not met after {} ms of paused time: {:?}",
            MAX_STEPS * 10,
            self.status()
        );
    }

    /// Cancels the pipeline and waits for it to stop.
    async fn stop(self) -> Result<(), ReloadError> {
        self.shutdown.cancel();
        match tokio::time::timeout(Duration::from_secs(5), self.task).await {
            Ok(result) => result.expect("the pipeline task did not panic"),
            Err(_elapsed) => panic!("the pipeline did not stop within the timeout"),
        }
    }
}

/// Writes a bootstrap file into `directory` and loads it.
fn bootstrap(directory: &Path, allow_bootstrap_cache: bool) -> BootstrapConfig {
    let path = directory.join("bootstrap.yaml");
    let text = format!(
        "\
config_source:
  type: local
  path: {document}
  max_bytes: 4096
  watch: notify
debounce_ms: {debounce}
max_wait_ms: {max_wait}
cache_dir: {cache}
allow_bootstrap_cache: {allow}
",
        document = directory.join("resources.yaml").display(),
        debounce = DEBOUNCE.as_millis(),
        max_wait = MAX_WAIT.as_millis(),
        cache = directory.join("cache").display(),
        allow = allow_bootstrap_cache,
    );
    std::fs::write(&path, text).expect("the bootstrap fixture is written");
    BootstrapConfig::load(&path).expect("the bootstrap fixture is valid")
}

#[tokio::test(flavor = "current_thread")]
async fn publishes_a_new_generation_for_a_new_document() {
    tokio::time::pause();
    let source = ChannelSource::new("channel");
    source.push_load(Ok(ConfigVersion::new(
        Revision::comparable(1),
        DOC_A.into(),
    )));
    let harness = Harness::start(Arc::clone(&source) as Arc<dyn ConfigSource>);

    let status = harness.advance_until(Harness::ready).await;
    assert_eq!(status.generation, 1);
    assert_eq!(harness.body_limit(), Some(1024));
    assert_eq!(status.revision, Some(1));

    source.push_comparable(2, DOC_B).await;
    let status = harness
        .advance_until(|harness| harness.generation() == 2)
        .await;
    assert_eq!(harness.body_limit(), Some(2048));
    assert_eq!(status.revision, Some(2));

    harness.stop().await.expect("shutdown ends the pipeline");
}

#[tokio::test(flavor = "current_thread")]
async fn skips_a_document_that_is_already_in_service() {
    tokio::time::pause();
    let source = ChannelSource::new("channel");
    source.push_load(Ok(ConfigVersion::new(
        Revision::comparable(1),
        DOC_A.into(),
    )));
    let harness = Harness::start(Arc::clone(&source) as Arc<dyn ConfigSource>);
    harness.advance_until(Harness::ready).await;

    source.push_comparable(2, DOC_A).await;
    // Long enough for several debounce windows: nothing may be published.
    tokio::time::advance(DEBOUNCE * 5).await;

    assert_eq!(harness.generation(), 1);
    assert_eq!(harness.status().revision, Some(1));

    harness.stop().await.expect("shutdown ends the pipeline");
}

#[tokio::test(flavor = "current_thread")]
async fn discards_a_revision_the_source_proves_is_older() {
    tokio::time::pause();
    let source = ChannelSource::new("channel");
    source.push_load(Ok(ConfigVersion::new(
        Revision::comparable(5),
        DOC_A.into(),
    )));
    let harness = Harness::start(Arc::clone(&source) as Arc<dyn ConfigSource>);
    harness.advance_until(Harness::ready).await;

    source.push_comparable(3, DOC_B).await;
    tokio::time::advance(DEBOUNCE * 5).await;

    assert_eq!(harness.generation(), 1);
    assert_eq!(harness.body_limit(), Some(1024));

    harness.stop().await.expect("shutdown ends the pipeline");
}

#[tokio::test(flavor = "current_thread")]
async fn publishes_equality_only_revisions_out_of_order_but_skips_duplicates() {
    tokio::time::pause();
    let source = ChannelSource::new("channel");
    source.push_load(Ok(ConfigVersion::new(
        Revision::equality_only(1),
        DOC_A.into(),
    )));
    let harness = Harness::start(Arc::clone(&source) as Arc<dyn ConfigSource>);
    harness.advance_until(Harness::ready).await;

    // An equality-only source may report a lower sequence number with new content: order is not
    // provable, so the document must still be published.
    source.push_equality_only(9, DOC_B).await;
    harness
        .advance_until(|harness| harness.generation() == 2)
        .await;
    assert_eq!(harness.body_limit(), Some(2048));

    // The same content again is a duplicate, whatever its sequence number.
    source.push_equality_only(2, DOC_B).await;
    tokio::time::advance(DEBOUNCE * 5).await;
    assert_eq!(harness.generation(), 2);

    source.push_equality_only(3, DOC_C).await;
    harness
        .advance_until(|harness| harness.generation() == 3)
        .await;
    assert_eq!(harness.body_limit(), Some(4096));

    harness.stop().await.expect("shutdown ends the pipeline");
}

#[tokio::test(flavor = "current_thread")]
async fn keeps_the_last_known_good_snapshot_when_a_document_is_rejected() {
    tokio::time::pause();
    let source = ChannelSource::new("channel");
    source.push_load(Ok(ConfigVersion::new(
        Revision::comparable(1),
        DOC_A.into(),
    )));
    let harness = Harness::start(Arc::clone(&source) as Arc<dyn ConfigSource>);
    harness.advance_until(Harness::ready).await;

    source.push_comparable(2, INVALID_DOC).await;
    let status = harness
        .advance_until(|harness| harness.status().degraded)
        .await;
    assert_eq!(status.generation, 1, "a rejected document must not publish");
    assert_eq!(status.last_error_category, Some(ErrorCategory::Client));
    assert_eq!(harness.body_limit(), Some(1024));

    source.push_comparable(3, DOC_B).await;
    let status = harness
        .advance_until(|harness| harness.generation() == 2 && !harness.status().degraded)
        .await;
    assert_eq!(status.last_error_category, None);
    assert_eq!(harness.body_limit(), Some(2048));

    harness.stop().await.expect("shutdown ends the pipeline");
}

#[tokio::test(flavor = "current_thread")]
async fn coalesces_a_burst_into_one_publish() {
    tokio::time::pause();
    let source = ChannelSource::new("channel");
    source.push_load(Ok(ConfigVersion::new(
        Revision::comparable(1),
        DOC_A.into(),
    )));
    let harness = Harness::start(Arc::clone(&source) as Arc<dyn ConfigSource>);
    harness.advance_until(Harness::ready).await;

    // Three events inside one quiet window: only the last document may be published.
    source.push_comparable(2, DOC_A).await;
    source.push_comparable(3, DOC_B).await;
    source.push_comparable(4, DOC_C).await;

    harness
        .advance_until(|harness| harness.generation() == 2)
        .await;
    tokio::time::advance(DEBOUNCE * 10).await;

    assert_eq!(harness.generation(), 2, "a burst must publish once");
    assert_eq!(harness.body_limit(), Some(4096));

    harness.stop().await.expect("shutdown ends the pipeline");
}

#[tokio::test(flavor = "current_thread")]
async fn publishes_when_the_maximum_wait_elapses_during_continuous_changes() {
    tokio::time::pause();
    let source = ChannelSource::new("channel");
    source.push_load(Ok(ConfigVersion::new(
        Revision::comparable(1),
        DOC_A.into(),
    )));
    let harness = Harness::start(Arc::clone(&source) as Arc<dyn ConfigSource>);
    harness.advance_until(Harness::ready).await;

    // Change faster than the quiet window, forever: only `max_wait` can end the debounce.
    let mut await_publish = Duration::ZERO;
    let mut published_after = None;
    for index in 0..40u32 {
        let document = if index % 2 == 0 { DOC_B } else { DOC_C };
        source.push_comparable(u64::from(index) + 2, document).await;

        let until = Duration::from_millis(100) * (index + 1);
        while await_publish < until {
            tokio::time::advance(STEP).await;
            await_publish += STEP;
            if harness.generation() == 2 && published_after.is_none() {
                published_after = Some(await_publish);
            }
        }
    }

    let published_after = published_after.expect("max_wait must force a publish");
    assert!(
        published_after >= MAX_WAIT,
        "a publish before max_wait means the quiet window was ignored: {published_after:?}"
    );
    assert!(
        published_after < MAX_WAIT + DEBOUNCE,
        "max_wait postponed the publish: {published_after:?}"
    );

    harness.stop().await.expect("shutdown ends the pipeline");
}

#[tokio::test(flavor = "current_thread")]
async fn reloads_the_whole_document_when_the_consumer_lags() {
    tokio::time::pause();
    // A capacity of one forces the fake to collapse its remaining script into a resync.
    let source = FakeConfigSource::new("fake")
        .with_channel_capacity(1)
        .push_load(Ok(FakeConfigSource::version_at(1, DOC_A)))
        .push_load(Ok(FakeConfigSource::version_at(9, DOC_B)))
        .push_event(Ok(ConfigEvent::Snapshot(FakeConfigSource::version_at(
            2, DOC_C,
        ))))
        .push_event(Ok(ConfigEvent::Snapshot(FakeConfigSource::version_at(
            3, DOC_A,
        ))))
        .push_event(Ok(ConfigEvent::Snapshot(FakeConfigSource::version_at(
            4, DOC_C,
        ))));
    let harness = Harness::start(Arc::new(source));

    // The resync asks for a full load, and that load is the only source of `DOC_B`.
    harness.advance_until(Harness::ready).await;
    harness
        .advance_until(|harness| harness.body_limit() == Some(2048))
        .await;
    assert!(harness.generation() >= 2);

    harness.stop().await.expect("shutdown ends the pipeline");
}

#[tokio::test(flavor = "current_thread")]
async fn is_not_ready_when_the_source_fails_and_no_cache_is_allowed() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let bootstrap = bootstrap(directory.path(), false);
    let source: Arc<dyn ConfigSource> = ChannelSource::new("channel");

    let error = serve(bootstrap, source, CancellationToken::new())
        .await
        .expect_err("nothing can be published");

    match error {
        ReloadError::NotReady { category } => assert_eq!(category, ErrorCategory::Upstream),
        other => panic!("expected NotReady, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn serves_a_stale_snapshot_from_the_cache_when_the_source_fails() {
    tokio::time::pause();
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let cache = CacheStore::new(directory.path().join("cache"));
    cache
        .write(
            DOC_C,
            &CacheMeta::new(
                7,
                ConfigDigest::of(DOC_C).hex(),
                4,
                1_700_000_000,
                "channel",
            ),
        )
        .await
        .expect("the cache fixture is written");

    // No scripted load: the source is unavailable from the first call.
    let harness = Harness::start_with(ChannelSource::new("channel"), Some(cache), true);
    let status = harness.advance_until(Harness::ready).await;

    assert!(status.stale, "a cached document must be marked stale");
    assert!(status.degraded, "serving a memory is still degraded");
    assert_eq!(status.last_error_category, Some(ErrorCategory::Upstream));
    assert_eq!(
        status.generation, 4,
        "the generation continues from the cache"
    );
    assert_eq!(harness.body_limit(), Some(4096));

    harness.stop().await.expect("shutdown ends the pipeline");
}

#[tokio::test(flavor = "current_thread")]
async fn a_source_confirmation_clears_the_stale_flag() {
    tokio::time::pause();
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let cache = CacheStore::new(directory.path().join("cache"));
    cache
        .write(
            DOC_C,
            &CacheMeta::new(
                7,
                ConfigDigest::of(DOC_C).hex(),
                4,
                1_700_000_000,
                "channel",
            ),
        )
        .await
        .expect("the cache fixture is written");

    let source = ChannelSource::new("channel");
    let harness = Harness::start_with(
        Arc::clone(&source) as Arc<dyn ConfigSource>,
        Some(cache),
        true,
    );
    harness.advance_until(Harness::ready).await;
    assert!(harness.status().stale);

    // The authoritative source now serves exactly the cached document.
    source.push_comparable(1, DOC_C).await;
    let status = harness
        .advance_until(|harness| !harness.status().stale)
        .await;

    assert!(!status.degraded, "a confirming source is a healthy source");
    assert_eq!(
        status.generation, 4,
        "a confirmation is not a new generation"
    );
    assert_eq!(harness.body_limit(), Some(4096));

    harness.stop().await.expect("shutdown ends the pipeline");
}

#[tokio::test(flavor = "current_thread")]
async fn publishing_a_new_document_clears_the_stale_flag() {
    tokio::time::pause();
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let cache = CacheStore::new(directory.path().join("cache"));
    cache
        .write(
            DOC_C,
            &CacheMeta::new(
                7,
                ConfigDigest::of(DOC_C).hex(),
                4,
                1_700_000_000,
                "channel",
            ),
        )
        .await
        .expect("the cache fixture is written");

    let source = ChannelSource::new("channel");
    let harness = Harness::start_with(
        Arc::clone(&source) as Arc<dyn ConfigSource>,
        Some(cache),
        true,
    );
    harness.advance_until(Harness::ready).await;
    assert!(harness.status().stale);

    source.push_comparable(1, DOC_A).await;
    let status = harness
        .advance_until(|harness| harness.generation() == 5)
        .await;

    assert!(!status.stale, "a compiled document is not a cached one");
    assert!(!status.degraded);
    assert_eq!(harness.body_limit(), Some(1024));

    harness.stop().await.expect("shutdown ends the pipeline");
}

#[tokio::test(flavor = "current_thread")]
async fn an_in_flight_reader_keeps_the_snapshot_it_loaded() {
    tokio::time::pause();
    let source = ChannelSource::new("channel");
    source.push_load(Ok(ConfigVersion::new(
        Revision::comparable(1),
        DOC_A.into(),
    )));
    let harness = Harness::start(Arc::clone(&source) as Arc<dyn ConfigSource>);
    harness.advance_until(Harness::ready).await;

    let in_flight = harness
        .handle
        .current()
        .expect("a snapshot is published before the reload");

    source.push_comparable(2, DOC_B).await;
    harness
        .advance_until(|harness| harness.generation() == 2)
        .await;

    assert_eq!(in_flight.generation().get(), 1);
    assert_eq!(in_flight.document().limits().max_request_body_bytes(), 1024);
    assert_eq!(harness.body_limit(), Some(2048));

    harness.stop().await.expect("shutdown ends the pipeline");
}

#[tokio::test(flavor = "current_thread")]
async fn stops_within_the_timeout_when_shutdown_is_cancelled() {
    // No paused clock: this test is about real cancellation latency.
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let bootstrap = bootstrap(directory.path(), false);
    let source = ChannelSource::new("channel");
    source.push_load(Ok(ConfigVersion::new(
        Revision::comparable(1),
        DOC_A.into(),
    )));
    let shutdown = CancellationToken::new();

    let task = tokio::spawn(serve(
        bootstrap,
        Arc::clone(&source) as Arc<dyn ConfigSource>,
        shutdown.clone(),
    ));
    // Wait for the watch to be live, so shutdown has something to interrupt.
    for _ in 0..1_000 {
        if source.lock().sender.is_some() {
            break;
        }
        tokio::task::yield_now().await;
    }

    shutdown.cancel();

    match tokio::time::timeout(Duration::from_secs(5), task).await {
        Ok(result) => result
            .expect("the pipeline task did not panic")
            .expect("shutdown is a clean stop"),
        Err(_elapsed) => panic!("serve did not return after cancellation"),
    }
}
