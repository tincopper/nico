//! Shared machinery for the `gateway-server` integration tests.
//!
//! The scenario files must agree on the same documents, the same fake source and the same paused
//! clock; keeping one copy here means a change to the reload contract updates every test file at
//! once instead of one file at a time. The documents serve all three files — including the
//! real-file test, which only needs the bytes — while the source and [`Harness`] serve the two
//! paused-clock ones.
//!
//! Everything here is test-only: the documents are complete resources documents, [`ChannelSource`]
//! is a watch the test drives by hand, and [`Harness`] runs one pipeline on a paused clock so
//! "nothing else happened" is provable rather than a race.
//!
//! The end-to-end HTTP scenarios in `tests/http_*.rs` use [`http`] instead: a real Pingora server on
//! its own thread, a hand-written client and a hand-written mock upstream. It lives in a submodule
//! because it shares nothing with the paused-clock pipeline fixtures except this module's name.
//!
//! Each integration test is its own crate, so it only compiles the subset of this module it calls:
//! a file that uses the documents but not the harness leaves most of this unreachable, which is what
//! `dead_code` reports. The exemption is per test crate and disappears as soon as a helper is
//! removed from the module.
#![expect(
    dead_code,
    reason = "an integration test compiles this module but uses only the helpers it calls"
)]
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a helper that cannot build its fixture has nothing useful to return"
)]

pub(crate) mod http;

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
    CacheStore, ConfigEvent, ConfigSource, ConfigSourceError, ConfigVersion, ConfigWatch,
};
use gateway_core::{ConfigStatus, ParseLimits, Revision, SnapshotHandle};
use gateway_server::{BootstrapConfig, ReloadError, ReloadLimits, Reloader};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

// --- documents ---------------------------------------------------------------

/// A valid document whose body limit is 1024 bytes.
pub(crate) const DOC_A: &[u8] = b"\
apiVersion: gateway/v1
limits:
  max_request_body_bytes: 1024
  request_timeout_ms: 1000
providers: []
upstreams: []
routes: []
";

/// A valid document whose body limit is 2048 bytes.
pub(crate) const DOC_B: &[u8] = b"\
apiVersion: gateway/v1
limits:
  max_request_body_bytes: 2048
  request_timeout_ms: 2000
providers: []
upstreams: []
routes: []
";

/// A valid document whose body limit is 4096 bytes.
pub(crate) const DOC_C: &[u8] = b"\
apiVersion: gateway/v1
limits:
  max_request_body_bytes: 4096
  request_timeout_ms: 3000
providers: []
upstreams: []
routes: []
";

/// A document that parses as YAML but violates the schema, which is what a bad edit looks like.
pub(crate) const INVALID_DOC: &[u8] = b"apiVersion: gateway/v1\nlimits: [not, a, mapping]\n";

/// Maximum document size a fixture source or pipeline accepts unless a test says otherwise.
pub(crate) const DOCUMENT_MAX_BYTES: usize = 4096;

// --- timing ------------------------------------------------------------------

/// Debounce window used by the paused-clock fixtures.
pub(crate) const DEBOUNCE: Duration = Duration::from_millis(200);

/// Maximum wait used by the paused-clock fixtures.
pub(crate) const MAX_WAIT: Duration = Duration::from_millis(1_000);

/// Clock step used by [`Harness::advance_until`]; smaller than any pipeline timer on purpose.
pub(crate) const STEP: Duration = Duration::from_millis(10);

/// Enough steps to walk far past every timer the pipeline can arm.
const MAX_STEPS: u32 = 5_000;

// --- fake source -------------------------------------------------------------

/// One item of a configuration watch, mirrored from `gateway_config`.
pub(crate) type WatchItem = Result<ConfigEvent, ConfigSourceError>;

/// A source the test drives by hand: scripted loads, and a watch that stays open until the test
/// stops pushing.
///
/// `FakeConfigSource` enqueues its whole script at once, and a debounce is *supposed* to collapse
/// such a burst into one publish, so it cannot express "an event, then a quiet window, then another
/// event". This source can.
#[derive(Debug)]
pub(crate) struct ChannelSource {
    name: &'static str,
    state: Mutex<SourceState>,
}

#[derive(Debug, Default)]
struct SourceState {
    loads: VecDeque<Result<ConfigVersion, ConfigSourceError>>,
    sender: Option<mpsc::Sender<WatchItem>>,
}

impl ChannelSource {
    /// Creates a source that reports `name` to the pipeline.
    pub(crate) fn new(name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            name,
            state: Mutex::new(SourceState::default()),
        })
    }

    /// Appends one `load` result. Once the queue is empty the source reports `Unavailable`.
    pub(crate) fn push_load(&self, result: Result<ConfigVersion, ConfigSourceError>) {
        self.lock().loads.push_back(result);
    }

    /// Returns whether the pipeline has subscribed to the watch.
    pub(crate) fn is_watching(&self) -> bool {
        self.lock().sender.is_some()
    }

    /// Sends one watch event, waiting for the pipeline to be subscribed and to have room.
    pub(crate) async fn push(&self, item: WatchItem) {
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
    pub(crate) async fn push_comparable(&self, seq: u64, document: &[u8]) {
        self.push(Ok(ConfigEvent::Snapshot(ConfigVersion::new(
            Revision::comparable(seq),
            document.to_vec().into(),
        ))))
        .await;
    }

    /// Sends one snapshot event built from an equality-only revision.
    pub(crate) async fn push_equality_only(&self, seq: u64, document: &[u8]) {
        self.push(Ok(ConfigEvent::Snapshot(ConfigVersion::new(
            Revision::equality_only(seq),
            document.to_vec().into(),
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
        // Bounded on purpose: a test that stops draining the watch must block, not grow memory.
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

// --- bootstrap fixture -------------------------------------------------------

/// What a test wants written into the local bootstrap file.
///
/// `max_bytes` bounds the resources document, `schema_bounds` is the body of the optional
/// `schema_bounds` section — `None` omits the section, `Some` lets a test write it partially.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BootstrapFixture<'a> {
    pub(crate) max_bytes: usize,
    pub(crate) allow_bootstrap_cache: bool,
    pub(crate) schema_bounds: Option<&'a str>,
}

impl Default for BootstrapFixture<'_> {
    fn default() -> Self {
        Self {
            max_bytes: DOCUMENT_MAX_BYTES,
            allow_bootstrap_cache: false,
            schema_bounds: None,
        }
    }
}

/// Writes a bootstrap file into `directory` and loads it.
pub(crate) fn bootstrap(directory: &Path, fixture: BootstrapFixture<'_>) -> BootstrapConfig {
    let section = fixture
        .schema_bounds
        .map(|bounds| format!("schema_bounds:\n{bounds}"))
        .unwrap_or_default();
    let path = directory.join("bootstrap.yaml");
    let text = format!(
        "\
config_source:
  type: local
  path: {document}
  max_bytes: {max_bytes}
  watch: notify
debounce_ms: {debounce}
max_wait_ms: {max_wait}
cache_dir: {cache}
allow_bootstrap_cache: {allow}
{section}",
        document = directory.join("resources.yaml").display(),
        max_bytes = fixture.max_bytes,
        debounce = DEBOUNCE.as_millis(),
        max_wait = MAX_WAIT.as_millis(),
        cache = directory.join("cache").display(),
        allow = fixture.allow_bootstrap_cache,
    );
    std::fs::write(&path, text).expect("the bootstrap fixture is written");
    BootstrapConfig::load(&path).expect("the bootstrap fixture is valid")
}

// --- harness -----------------------------------------------------------------

/// A running pipeline plus the handle it publishes into.
pub(crate) struct Harness {
    handle: Arc<SnapshotHandle>,
    shutdown: CancellationToken,
    task: JoinHandle<Result<(), ReloadError>>,
}

impl Harness {
    /// Starts a pipeline on the paused-clock fixtures.
    pub(crate) fn start(source: Arc<dyn ConfigSource>) -> Self {
        Self::start_with(source, None, false)
    }

    /// Starts a pipeline on the paused-clock fixtures, with an optional cache fallback.
    pub(crate) fn start_with(
        source: Arc<dyn ConfigSource>,
        cache: Option<CacheStore>,
        allow_bootstrap_cache: bool,
    ) -> Self {
        Self::spawn(
            source,
            ReloadLimits::new(DEBOUNCE, MAX_WAIT),
            ParseLimits::new(DOCUMENT_MAX_BYTES),
            cache,
            allow_bootstrap_cache,
        )
    }

    /// Starts a pipeline configured exactly as `serve` configures it from `bootstrap`.
    pub(crate) fn from_bootstrap(
        bootstrap: &BootstrapConfig,
        source: Arc<dyn ConfigSource>,
    ) -> Self {
        Self::spawn(
            source,
            ReloadLimits::new(bootstrap.debounce(), bootstrap.max_wait()),
            bootstrap.parse_limits(),
            None,
            bootstrap.allow_bootstrap_cache(),
        )
    }

    fn spawn(
        source: Arc<dyn ConfigSource>,
        reload: ReloadLimits,
        limits: ParseLimits,
        cache: Option<CacheStore>,
        allow_bootstrap_cache: bool,
    ) -> Self {
        let handle = SnapshotHandle::new();
        let shutdown = CancellationToken::new();
        let reloader = Reloader::new(
            source,
            Arc::clone(&handle),
            cache,
            reload,
            limits,
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

    /// Returns the handle the pipeline publishes into.
    pub(crate) fn handle(&self) -> &SnapshotHandle {
        &self.handle
    }

    pub(crate) fn status(&self) -> ConfigStatus {
        self.handle.status()
    }

    pub(crate) fn generation(&self) -> u64 {
        self.handle.status().generation
    }

    pub(crate) fn ready(&self) -> bool {
        self.handle.status().ready
    }

    /// Returns the published body limit, which identifies the document in service.
    pub(crate) fn body_limit(&self) -> Option<u32> {
        self.handle
            .current()
            .map(|snapshot| snapshot.document().limits().max_request_body_bytes())
    }

    /// Returns how many routes the published document carries.
    pub(crate) fn route_count(&self) -> Option<usize> {
        self.handle
            .current()
            .map(|snapshot| snapshot.document().routes().len())
    }

    /// Advances the paused clock in [`STEP`] increments until `condition` holds.
    ///
    /// The test task is always ready, so the runtime never auto-advances the clock behind the test's
    /// back: the pipeline can only make timer-driven progress here.
    pub(crate) async fn advance_until(&self, condition: impl Fn(&Self) -> bool) -> ConfigStatus {
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
    pub(crate) async fn stop(self) -> Result<(), ReloadError> {
        self.shutdown.cancel();
        match tokio::time::timeout(Duration::from_secs(5), self.task).await {
            Ok(result) => result.expect("the pipeline task did not panic"),
            Err(_elapsed) => panic!("the pipeline did not stop within the timeout"),
        }
    }
}
