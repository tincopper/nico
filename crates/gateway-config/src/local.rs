//! Local file configuration source.
//!
//! One process reads one file. The adapter reads the whole document, bounds its size, checks the
//! encoding and assigns a process-local revision; change detection is a file-system notification
//! and, on Unix, `SIGHUP`.
//!
//! Design decisions worth keeping:
//!
//! - **Watch the directory, not the file.** Atomic replacement (`rename` over the path) and
//!   Kubernetes ConfigMap updates (a symlink swapped through a `..data` indirection) both replace
//!   the path entry rather than writing the inode we could hold open. Notifications from the parent
//!   directory plus a fresh read on every trigger observe both patterns.
//! - **No polling mode.** File-system events are either available or they are not; a second,
//!   silently different reload model would double the semantics for no gain. When the backend cannot
//!   be armed the source reports [`ConfigSourceError::Unavailable`].
//! - **No debouncing here.** Duplicate and jittery events are expected and cheap: the pipeline
//!   de-duplicates by digest. Implementing the same policy twice would only let the two copies
//!   drift.
//! - **Deletion is not an empty document.** A missing or unreadable file is `Unavailable`, so the
//!   pipeline keeps serving its last-known-good snapshot instead of compiling an empty config.

use std::{
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use bytes::Bytes;
use gateway_core::Revision;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{
    source::{ConfigEvent, ConfigSource, ConfigSourceError, ConfigVersion, ConfigWatch},
    stream::{SourceWatch, WatchItem},
};

/// The name reported by [`ConfigSource::name`] and in source errors.
const SOURCE_NAME: &str = "local";

/// Default maximum document size: 1 MiB.
const DEFAULT_MAX_BYTES: usize = 1024 * 1024;

/// Bounded output channel. A consumer that falls behind is told to resync instead of buffering.
const WATCH_CHANNEL_CAPACITY: usize = 8;

/// Bounded internal trigger channel. Extra triggers carry no information and are dropped.
const TRIGGER_CHANNEL_CAPACITY: usize = 8;

/// How a [`LocalFileSource`] detects changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum WatchMode {
    /// File-system notifications only.
    Notify,
    /// `SIGHUP` only. Rejected on platforms without Unix signals.
    Signal,
    /// Both; the default.
    #[default]
    NotifyAndSignal,
}

impl WatchMode {
    const fn watches_files(self) -> bool {
        matches!(self, Self::Notify | Self::NotifyAndSignal)
    }

    #[cfg(unix)]
    const fn watches_signals(self) -> bool {
        matches!(self, Self::Signal | Self::NotifyAndSignal)
    }
}

/// Configuration of one [`LocalFileSource`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalConfig {
    path: PathBuf,
    max_bytes: usize,
    watch_mode: WatchMode,
}

impl LocalConfig {
    /// Creates a configuration for `path` with a 1 MiB size bound and `notify` + `SIGHUP` watching.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            max_bytes: DEFAULT_MAX_BYTES,
            watch_mode: WatchMode::default(),
        }
    }

    /// Sets the maximum accepted document size.
    #[must_use]
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// Sets how changes are detected.
    #[must_use]
    pub fn with_watch_mode(mut self, watch_mode: WatchMode) -> Self {
        self.watch_mode = watch_mode;
        self
    }

    /// Returns the configured document path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the maximum accepted document size in bytes.
    #[must_use]
    pub const fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Returns how changes are detected.
    #[must_use]
    pub const fn watch_mode(&self) -> WatchMode {
        self.watch_mode
    }
}

/// A [`ConfigSource`] backed by one local file.
///
/// Cloning the source is cheap: clones share the revision counter, so revisions stay monotonic
/// within the process even when two clones load concurrently.
#[derive(Debug, Clone)]
pub struct LocalFileSource {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    config: LocalConfig,
    /// Process-local revision counter. Starts at zero, so the first load reports revision 1.
    seq: AtomicU64,
}

impl Inner {
    async fn load(&self) -> Result<ConfigVersion, ConfigSourceError> {
        let path = &self.config.path;
        let limit = self.config.max_bytes;

        // Size check before reading: an oversized document must never be buffered in full.
        let metadata = tokio::fs::metadata(path).await.map_err(unavailable)?;
        let size = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        if size > limit {
            return Err(ConfigSourceError::TooLarge { size, limit });
        }

        let bytes = tokio::fs::read(path).await.map_err(unavailable)?;
        // Re-check the real length: the file may have grown between `metadata` and `read`.
        if bytes.len() > limit {
            return Err(ConfigSourceError::TooLarge {
                size: bytes.len(),
                limit,
            });
        }
        std::str::from_utf8(&bytes).map_err(|_| ConfigSourceError::NotUtf8)?;

        // Saturating so an exhausted counter cannot panic; reaching it would take 2^64 loads.
        let seq = self.seq.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        Ok(ConfigVersion::new(
            Revision::comparable(seq),
            Bytes::from(bytes),
        ))
    }
}

impl LocalFileSource {
    /// Creates a source from its configuration.
    ///
    /// Nothing is read here: the path is validated, not opened, so a source can be constructed before
    /// the file exists (that case is reported by [`ConfigSource::load`] as `Unavailable`).
    ///
    /// # Errors
    /// Returns [`ConfigSourceError::Unavailable`] when the configuration cannot describe a readable
    /// source: a zero size bound, a path without a file name, or `SIGHUP` watching on a platform
    /// without Unix signals.
    pub fn new(config: LocalConfig) -> Result<Self, ConfigSourceError> {
        if config.max_bytes == 0 {
            return Err(unavailable(ConfigInvalid {
                reason: "max_bytes must be greater than zero",
            }));
        }
        if config.path.file_name().is_none() {
            return Err(unavailable(ConfigInvalid {
                reason: "path must name a file",
            }));
        }
        #[cfg(not(unix))]
        if config.watch_mode != WatchMode::Notify {
            return Err(unavailable(ConfigInvalid {
                reason: "SIGHUP watching is not available on this platform",
            }));
        }
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                seq: AtomicU64::new(0),
            }),
        })
    }

    /// Starts the watch task and returns the stream plus a guard that closes when the task ends.
    ///
    /// The guard is how a test proves that dropping the stream really stopped everything; the public
    /// `watch` drops it.
    fn spawn_watch(&self) -> Result<(ConfigWatch, oneshot::Receiver<()>), ConfigSourceError> {
        let config = &self.inner.config;
        let (trigger_sender, trigger_receiver) = mpsc::channel::<()>(TRIGGER_CHANNEL_CAPACITY);

        let mut watchers = WatchTriggers {
            _watcher: None,
            #[cfg(unix)]
            sighup: None,
        };

        if config.watch_mode.watches_files() {
            // Both paths are resolved once, so notifications are matched against the same paths the
            // backend reports. On macOS a temporary directory is reached through `/var`, while the
            // backend reports `/private/var`, and canonicalizing is what makes them equal.
            let (watched_dir, target) = watch_paths(&config.path)?;
            let watch_root = watched_dir.clone();
            let mut watcher = notify::recommended_watcher(move |result| {
                handle_notification(result, &trigger_sender, &target, &watched_dir);
            })
            .map_err(unavailable)?;
            watcher
                .watch(&watch_root, RecursiveMode::NonRecursive)
                .map_err(unavailable)?;
            watchers._watcher = Some(watcher);
        }

        #[cfg(unix)]
        if config.watch_mode.watches_signals() {
            watchers.sighup = Some(
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                    .map_err(unavailable)?,
            );
        }

        let (sender, receiver) = mpsc::channel::<WatchItem>(WATCH_CHANNEL_CAPACITY);
        let token = CancellationToken::new();
        let (finished_sender, finished_receiver) = oneshot::channel();

        let source = Arc::clone(&self.inner);
        let task_token = token.clone();
        // The handle is detached on purpose: the stream owns the lifecycle, and dropping it cancels
        // the token the task selects on.
        let _handle = tokio::spawn(async move {
            run_watch(
                source,
                EventSink::new(sender),
                task_token,
                trigger_receiver,
                watchers,
                finished_sender,
            )
            .await;
        });

        Ok((
            Box::pin(SourceWatch::new(receiver, token)),
            finished_receiver,
        ))
    }
}

#[async_trait]
impl ConfigSource for LocalFileSource {
    fn name(&self) -> &'static str {
        SOURCE_NAME
    }

    async fn load(&self) -> Result<ConfigVersion, ConfigSourceError> {
        self.inner.load().await
    }

    async fn watch(&self, _from: Option<Revision>) -> Result<ConfigWatch, ConfigSourceError> {
        // `from` is not honoured: every trigger re-reads the current document and stamps it with a
        // fresh local revision. Ordering across reloads is the pipeline's decision, made from the
        // digest and the published generation.
        self.spawn_watch().map(|(watch, _finished)| watch)
    }
}

/// Watches the parent directory with a non-recursive subscription.
struct WatchTriggers {
    /// Held to keep the backend alive; dropping it stops the subscription.
    _watcher: Option<RecommendedWatcher>,
    #[cfg(unix)]
    sighup: Option<tokio::signal::unix::Signal>,
}

impl WatchTriggers {
    /// Resolves on the next change, reporting what woke the watch for the debug log.
    async fn changed(&mut self, file_events: &mut mpsc::Receiver<()>) -> &'static str {
        #[cfg(unix)]
        {
            tokio::select! {
                biased;
                // The sender lives in the watcher, which outlives this call, so the branch stays
                // armed for as long as the task runs.
                Some(()) = file_events.recv() => "file",
                () = wait_for_signal(self.sighup.as_mut()) => "signal",
            }
        }
        #[cfg(not(unix))]
        {
            match file_events.recv().await {
                Some(()) => "file",
                None => std::future::pending().await,
            }
        }
    }
}

/// Waits for a signal, or never when the mode has no signal source.
#[cfg(unix)]
async fn wait_for_signal(signal: Option<&mut tokio::signal::unix::Signal>) {
    match signal {
        Some(signal) => {
            signal.recv().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// The body of a watch task: translate triggers into reloads until cancelled or dropped.
async fn run_watch(
    source: Arc<Inner>,
    mut sink: EventSink,
    token: CancellationToken,
    mut triggers: mpsc::Receiver<()>,
    mut watchers: WatchTriggers,
    finished: oneshot::Sender<()>,
) {
    // Dropping the guard at the end of the task closes `finished`, which is what tells an awaiting
    // test that nothing is left running.
    let _finished = finished;

    loop {
        tokio::select! {
            biased;
            () = token.cancelled() => break,
            trigger = watchers.changed(&mut triggers) => {
                tracing::debug!(trigger = trigger, "configuration change detected");
            }
        }

        let item = match source.load().await {
            Ok(version) => Ok(ConfigEvent::Snapshot(version)),
            Err(error) => Err(error),
        };
        if !sink.send(item) {
            // The stream was dropped while the reload was in flight.
            break;
        }
    }
}

/// The bounded output side of a watch, including the resync-collapse rule.
struct EventSink {
    sender: mpsc::Sender<WatchItem>,
    pending_resync: bool,
}

impl EventSink {
    fn new(sender: mpsc::Sender<WatchItem>) -> Self {
        Self {
            sender,
            pending_resync: false,
        }
    }

    /// Returns `false` when the receiver is gone, meaning the subscriber dropped the stream.
    fn send(&mut self, item: WatchItem) -> bool {
        let item = if self.pending_resync {
            // One resync supersedes every event that could not be delivered: the pipeline reloads
            // the whole document instead of replaying history.
            self.pending_resync = false;
            Ok(ConfigEvent::ResyncRequired)
        } else {
            item
        };

        match self.sender.try_send(item) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_dropped)) => {
                self.pending_resync = true;
                true
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

/// Returns the directory to watch and the path whose changes matter.
fn watch_paths(path: &Path) -> Result<(PathBuf, PathBuf), ConfigSourceError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let watched_dir = fs::canonicalize(parent).map_err(unavailable)?;
    let file_name = path.file_name().ok_or_else(|| {
        unavailable(ConfigInvalid {
            reason: "path must name a file",
        })
    })?;
    Ok((watched_dir.clone(), watched_dir.join(file_name)))
}

/// Decides whether a notification describes a change to the configured document.
///
/// Three shapes are accepted because different backends and different replacement strategies each
/// produce one of them: the document path itself, any direct child of the watched directory (a
/// `rename` of a temporary file or a swapped `..data` symlink), and the watched directory itself
/// (a coalesced macOS `FSEvents` notification). Notifications from deeper paths are ignored.
fn is_relevant(paths: &[PathBuf], target: &Path, watched_dir: &Path) -> bool {
    paths.iter().any(|path| {
        let path = path.as_path();
        path == target || path == watched_dir || path.parent() == Some(watched_dir)
    })
}

/// Turns one backend notification into a reload trigger.
fn handle_notification(
    result: notify::Result<notify::Event>,
    trigger: &mpsc::Sender<()>,
    target: &Path,
    watched_dir: &Path,
) {
    match result {
        Ok(event) => {
            if is_relevant(&event.paths, target, watched_dir) {
                // A full trigger channel means a reload is already pending, so the extra trigger
                // carries no information.
                let _ = trigger.try_send(());
            }
        }
        Err(error) => {
            // A backend error does not mean the document changed, but reloading is the fail-loud
            // reaction and the pipeline de-duplicates by digest anyway.
            tracing::warn!(error = %error, "file watch reported an error");
            let _ = trigger.try_send(());
        }
    }
}

/// Maps any foreign error into the contract's "the source cannot answer" variant.
fn unavailable(source: impl Error + Send + Sync + 'static) -> ConfigSourceError {
    ConfigSourceError::Unavailable {
        source_name: SOURCE_NAME,
        source: Box::new(source),
    }
}

/// A configuration that cannot describe a working source.
#[derive(Debug)]
struct ConfigInvalid {
    reason: &'static str,
}

impl fmt::Display for ConfigInvalid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "local configuration is invalid: {}", self.reason)
    }
}

impl Error for ConfigInvalid {}

#[cfg(test)]
mod tests {
    use std::{
        future::poll_fn,
        path::Path,
        time::{Duration, Instant},
    };

    use gateway_core::RevisionOrdering;
    use tempfile::TempDir;

    use super::*;

    const DOC_A: &[u8] = b"apiVersion: gateway/v1\nlimits:\n  max_request_body_bytes: 1024\n  request_timeout_ms: 1000\n";
    const DOC_B: &[u8] = b"apiVersion: gateway/v1\nlimits:\n  max_request_body_bytes: 2048\n  request_timeout_ms: 2000\n";

    /// Generous enough for macOS `FSEvents` latency, short enough to fail a hung test.
    const WAIT: Duration = Duration::from_secs(10);

    fn config(path: &Path) -> LocalConfig {
        LocalConfig::new(path).with_max_bytes(4096)
    }

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).expect("fixture write succeeds");
    }

    /// Replaces `path` the way a deployer does: write a sibling, then rename over the target.
    fn replace_atomically(path: &Path, bytes: &[u8]) {
        let temporary = path.with_extension("tmp");
        write(&temporary, bytes);
        std::fs::rename(&temporary, path).expect("rename over the target succeeds");
    }

    async fn next_event(watch: &mut ConfigWatch) -> Option<WatchItem> {
        poll_fn(|context| watch.as_mut().poll_next(context)).await
    }

    /// Reads events until one matches, failing the test when the budget runs out.
    async fn snapshot_with_bytes(
        watch: &mut ConfigWatch,
        expected: &[u8],
        budget: Duration,
    ) -> ConfigVersion {
        let deadline = Instant::now() + budget;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let event = tokio::time::timeout(remaining, next_event(watch))
                .await
                .expect("an event arrives within the budget");
            match event {
                Some(Ok(ConfigEvent::Snapshot(version))) => {
                    if version.bytes().as_ref() == expected {
                        return version;
                    }
                }
                Some(Ok(ConfigEvent::ResyncRequired)) => {}
                Some(Err(_error)) => {}
                None => panic!("the watch ended before the expected document arrived"),
            }
        }
    }

    async fn error_event(watch: &mut ConfigWatch, budget: Duration) -> ConfigSourceError {
        let deadline = Instant::now() + budget;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let event = tokio::time::timeout(remaining, next_event(watch))
                .await
                .expect("an event arrives within the budget");
            match event {
                Some(Err(error)) => return error,
                Some(Ok(_event)) => {}
                None => panic!("the watch ended before the expected error arrived"),
            }
        }
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
            let source = LocalFileSource::new(config(&path).with_max_bytes(16))
                .expect("configuration is valid");

            assert_eq!(
                source.load().await.expect_err("the file is too large"),
                ConfigSourceError::TooLarge {
                    size: DOC_A.len(),
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
            assert_eq!(error.category(), gateway_core::ErrorCategory::Upstream);
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

    mod watch {
        use super::*;

        #[tokio::test]
        async fn watch_emits_a_snapshot_after_an_atomic_rename() {
            let directory = TempDir::new().expect("temporary directory");
            let path = directory.path().join("resources.yaml");
            write(&path, DOC_A);
            let source = LocalFileSource::new(config(&path)).expect("configuration is valid");
            let mut watch = source.watch(None).await.expect("watch starts");

            replace_atomically(&path, DOC_B);

            let version = snapshot_with_bytes(&mut watch, DOC_B, WAIT).await;
            assert!(
                version.revision().seq() >= 1,
                "a watched change must carry a revision"
            );
        }

        #[tokio::test]
        async fn watch_reports_unavailable_after_deletion_and_recovers_on_recreation() {
            let directory = TempDir::new().expect("temporary directory");
            let path = directory.path().join("resources.yaml");
            write(&path, DOC_A);
            let source = LocalFileSource::new(config(&path)).expect("configuration is valid");
            let mut watch = source.watch(None).await.expect("watch starts");

            std::fs::remove_file(&path).expect("delete succeeds");
            let error = error_event(&mut watch, WAIT).await;
            assert!(
                matches!(
                    error,
                    ConfigSourceError::Unavailable {
                        source_name: "local",
                        ..
                    }
                ),
                "a deleted document is unavailable, not empty, got {error:?}"
            );

            write(&path, DOC_B);
            snapshot_with_bytes(&mut watch, DOC_B, WAIT).await;
        }

        #[tokio::test]
        async fn watch_emits_a_snapshot_when_the_file_is_created_after_the_watch_started() {
            let directory = TempDir::new().expect("temporary directory");
            let path = directory.path().join("resources.yaml");
            let source = LocalFileSource::new(config(&path)).expect("configuration is valid");
            let mut watch = source.watch(None).await.expect("watch starts");

            write(&path, DOC_A);

            snapshot_with_bytes(&mut watch, DOC_A, WAIT).await;
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn watch_emits_a_snapshot_after_a_kubernetes_style_symlink_swap() {
            use std::os::unix::fs::symlink;

            let directory = TempDir::new().expect("temporary directory");
            let version_one = directory.path().join("..data_v1");
            let version_two = directory.path().join("..data_v2");
            std::fs::create_dir_all(&version_one).expect("create the first version");
            std::fs::create_dir_all(&version_two).expect("create the second version");
            write(&version_one.join("resources.yaml"), DOC_A);
            write(&version_two.join("resources.yaml"), DOC_B);

            symlink("..data_v1", directory.path().join("..data")).expect("link ..data");
            let path = directory.path().join("resources.yaml");
            symlink("..data/resources.yaml", &path).expect("link the document name");

            let source = LocalFileSource::new(config(&path)).expect("configuration is valid");
            let mut watch = source.watch(None).await.expect("watch starts");

            // The ConfigMap update pattern: build a new symlink, then rename it over `..data`.
            let pending = directory.path().join("..data_tmp");
            symlink("..data_v2", &pending).expect("link the pending ..data");
            std::fs::rename(&pending, directory.path().join("..data")).expect("swap ..data");

            snapshot_with_bytes(&mut watch, DOC_B, WAIT).await;
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn watch_emits_a_snapshot_on_sighup() {
            let directory = TempDir::new().expect("temporary directory");
            let path = directory.path().join("resources.yaml");
            write(&path, DOC_A);
            let source = LocalFileSource::new(config(&path).with_watch_mode(WatchMode::Signal))
                .expect("configuration is valid");
            let mut watch = source.watch(None).await.expect("watch starts");

            raise_sighup();

            snapshot_with_bytes(&mut watch, DOC_A, WAIT).await;
        }

        #[tokio::test]
        async fn dropping_the_stream_stops_the_watch_task() {
            let directory = TempDir::new().expect("temporary directory");
            let path = directory.path().join("resources.yaml");
            write(&path, DOC_A);
            let source = LocalFileSource::new(config(&path)).expect("configuration is valid");

            let (watch, finished) = source.spawn_watch().expect("watch starts");
            // The first event proves the task is actually running before it is dropped.
            let mut watch = watch;
            replace_atomically(&path, DOC_B);
            snapshot_with_bytes(&mut watch, DOC_B, WAIT).await;
            drop(watch);

            match tokio::time::timeout(WAIT, finished).await {
                Ok(Ok(())) => panic!("the guard must never receive a value"),
                // A closed channel without a value means the task dropped the guard, i.e. it ended.
                Ok(Err(_closed)) => {}
                Err(_elapsed) => panic!("the watch task did not stop within the budget"),
            }
        }

        #[cfg(unix)]
        fn raise_sighup() {
            let status = std::process::Command::new("kill")
                .arg("-HUP")
                .arg(std::process::id().to_string())
                .status()
                .expect("kill runs");
            assert!(status.success(), "sending SIGHUP must succeed");
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
}
