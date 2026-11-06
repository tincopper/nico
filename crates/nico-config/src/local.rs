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
use nico_core::Revision;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{
    ReadError,
    file::read_bounded,
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
        let limit = self.config.max_bytes;
        // The ceiling is applied by the read itself, on a blocking thread: a length checked before a
        // read says nothing about what the read will pull in, and the path may not even name a regular
        // file. `tokio::fs` would make this same hop internally, so it is taken explicitly and the
        // rule lives in one place both this source and the credential resolver use.
        let path = self.config.path.clone();
        let bytes = tokio::task::spawn_blocking(move || read_bounded(&path, limit))
            .await
            .map_err(unavailable)?
            .map_err(|error| match error {
                ReadError::TooLarge { at_least, limit } => ConfigSourceError::TooLarge {
                    size: at_least,
                    limit,
                },
                other => unavailable(other),
            })?;
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
    sink: EventSink,
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
        if !sink.send(item).await {
            // The stream was dropped while the reload was in flight.
            break;
        }
    }
}

/// The bounded output side of a watch.
///
/// It holds no state of its own: what it delivers is the state of the world at the moment it ran —
/// a snapshot, or the failure that prevented one — and the pipeline coalesces from there. A full
/// channel therefore means "wait", not "remember this for later": an item held back until the next
/// trigger is the last change of a burst, and a burst has no next trigger to deliver it on.
struct EventSink {
    sender: mpsc::Sender<WatchItem>,
}

impl EventSink {
    fn new(sender: mpsc::Sender<WatchItem>) -> Self {
        Self { sender }
    }

    /// Delivers one item, waiting for capacity. Returns `false` when the receiver is gone.
    async fn send(&self, item: WatchItem) -> bool {
        self.sender.send(item).await.is_ok()
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
mod tests;
