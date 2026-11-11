//! The local source against the operating system's own file notifications.
//!
//! These scenarios are **not** unit tests and deliberately live outside `src/`: each one asks the
//! kernel to deliver a real notification, and every backend answers differently. macOS `FSEvents`
//! coalesces, an atomic rename reports as two events on one path and a creation on another, and a
//! Kubernetes-style configuration swap replaces a symlink rather than the file. None of that can be
//! observed without a real filesystem and a real watch, which is why they cost seconds each.
//!
//! What stays in `src/local/tests.rs` instead is the part that is a decision rather than an
//! observation: whether a notification describes the configured document, and what the watch task
//! does with a trigger. Those are driven by an injected trigger channel, so the fast commit gate
//! keeps them and this file keeps the operating-system behaviour.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a fixture that cannot build the filesystem it asserts about, or whose notification \
              never arrives, has nothing to return — the panic message is the diagnosis"
)]

use std::{path::Path, pin::Pin, task::Context, time::Duration};

use futures_core::Stream;
use nico_config::{
    ConfigEvent, ConfigSource, ConfigSourceError, ConfigVersion, ConfigWatch, LocalConfig,
    LocalFileSource, WatchMode,
};
use tempfile::TempDir;

/// A complete, minimal document; the schema rejects a missing section, so the fixture spells them.
const DOC_A: &[u8] = b"upstreams: []\nroutes: []\nproviders: []\nplugins: []\n";

/// A second document that differs from [`DOC_A`] in a way the digest can see.
const DOC_B: &[u8] = b"upstreams: []\nroutes: []\nproviders: []\nplugins:\n  - name: a\n";

/// How long one scenario waits for the operating system to deliver a notification.
///
/// Generous on purpose: the assertion is that the notification arrives, not how fast. A backend
/// that never delivers it fails the budget, which is the diagnosis this test exists to produce.
const WAIT: Duration = Duration::from_secs(10);

fn config(path: &Path) -> LocalConfig {
    LocalConfig::new(path)
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("the fixture is written");
}

/// Replaces the document the way an editor or a config push does: temp file, then `rename`.
fn replace_atomically(path: &Path, bytes: &[u8]) {
    let temporary = path.with_extension("tmp");
    write(&temporary, bytes);
    std::fs::rename(&temporary, path).expect("the rename succeeds");
}

/// Polls a watch the way the pipeline does, so a scenario does not depend on `StreamExt`.
async fn next_event(watch: &mut ConfigWatch) -> Option<Result<ConfigEvent, ConfigSourceError>> {
    std::future::poll_fn(|context: &mut Context<'_>| Pin::new(&mut *watch).poll_next(context)).await
}

/// Reads events until one carries `expected`, failing the test when the budget runs out.
///
/// A `ResyncRequired` or an error event is skipped rather than failed: a backend may deliver either
/// before the snapshot that matters, and the scenario is about the snapshot.
async fn snapshot_with_bytes(
    watch: &mut ConfigWatch,
    expected: &[u8],
    budget: Duration,
) -> ConfigVersion {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = tokio::time::timeout(remaining, next_event(watch))
            .await
            .unwrap_or_else(|_elapsed| panic!("no snapshot within {budget:?}"));
        match event {
            Some(Ok(ConfigEvent::Snapshot(version))) => {
                if version.bytes().as_ref() == expected {
                    return version;
                }
            }
            Some(Ok(ConfigEvent::ResyncRequired)) => {}
            // `ConfigEvent` is `#[non_exhaustive]`, so a variant added later must not silently fail
            // this loop; skipping it is the same treatment `ResyncRequired` gets above.
            Some(Ok(_other)) => {}
            Some(Err(_error)) => {}
            None => panic!("the watch ended before the expected document arrived"),
        }
    }
}

/// Reads events until one is an error, failing the test when the budget runs out.
async fn error_event(watch: &mut ConfigWatch, budget: Duration) -> ConfigSourceError {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = tokio::time::timeout(remaining, next_event(watch))
            .await
            .unwrap_or_else(|_elapsed| panic!("no error within {budget:?}"));
        match event {
            Some(Err(error)) => return error,
            Some(Ok(_event)) => {}
            None => panic!("the watch ended before reporting an error"),
        }
    }
}

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
        matches!(error, ConfigSourceError::Unavailable { .. }),
        "a deleted document is Unavailable, got {error:?}"
    );

    write(&path, DOC_B);
    snapshot_with_bytes(&mut watch, DOC_B, WAIT).await;
}

#[tokio::test]
async fn watch_emits_a_snapshot_when_the_file_is_created_after_the_watch_started() {
    // A watch on a path that does not exist yet: the parent directory is what is actually watched,
    // which is the case a bootstrap file written after startup depends on.
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

#[cfg(unix)]
fn raise_sighup() {
    let status = std::process::Command::new("kill")
        .arg("-HUP")
        .arg(std::process::id().to_string())
        .status()
        .expect("kill runs");
    assert!(status.success(), "sending SIGHUP must succeed");
}
