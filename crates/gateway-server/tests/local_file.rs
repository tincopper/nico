//! End-to-end verification against a real file: the local source, the pipeline and the published
//! snapshot, with no fake in between.
//!
//! These tests use real time because the subject *is* the file-system notification path; the
//! debounce windows are deliberately small and every wait is bounded by an explicit timeout, so a
//! failure is a failure rather than a hang.

// The panic-free policy protects request paths. A test helper that cannot build its fixture has
// nothing useful to return, and clippy only recognises the lint exemption inside `#[test]`
// functions, not in the helpers they call.
#![expect(
    clippy::expect_used,
    reason = "test helpers fail loudly when their fixture cannot be built"
)]

use std::{path::Path, time::Duration};

use gateway_config::{LocalConfig, LocalFileSource, WatchMode};
use gateway_core::{ConfigStatus, ParseLimits, SnapshotHandle};
use gateway_server::{ReloadError, ReloadLimits, Reloader};
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

/// A document that parses as YAML but violates the schema.
const INVALID_DOC: &[u8] = b"apiVersion: gateway/v1\nlimits: [not, a, mapping]\n";

const DEBOUNCE: Duration = Duration::from_millis(50);
const MAX_WAIT: Duration = Duration::from_millis(200);

/// Poll interval while waiting for the file-system watcher; well below the timeout.
const POLL: Duration = Duration::from_millis(20);

/// Budget for one file-system round trip. Generous because `FSEvents` may coalesce.
const WAIT: Duration = Duration::from_secs(5);

/// Budget for the recovery after a deletion: the pipeline is in a backoff loop by then, so the
/// observation may have to wait for the next retry rather than for a notification.
const RECOVERY_WAIT: Duration = Duration::from_secs(10);

fn write(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("the fixture write succeeds");
}

/// Replaces `path` the way a deployer does: write a sibling, then rename over the target.
fn replace_atomically(path: &Path, bytes: &[u8]) {
    let pending = path.with_extension("pending");
    write(&pending, bytes);
    std::fs::rename(&pending, path).expect("the rename over the target succeeds");
}

/// Waits until the pipeline status satisfies `condition`, failing when the budget is spent.
async fn wait_for(
    handle: &SnapshotHandle,
    condition: impl Fn(&ConfigStatus) -> bool,
    budget: Duration,
) -> ConfigStatus {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let status = handle.status();
        if condition(&status) {
            return status;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the pipeline did not reach the expected state within {budget:?}: {status:?}"
        );
        tokio::time::sleep(POLL).await;
    }
}

fn body_limit(handle: &SnapshotHandle) -> u32 {
    handle
        .current()
        .expect("a snapshot is published")
        .document()
        .limits()
        .max_request_body_bytes()
}

#[tokio::test(flavor = "current_thread")]
async fn the_local_source_drives_a_full_reload_cycle() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let path = directory.path().join("resources.yaml");
    write(&path, DOC_A);

    let source = LocalFileSource::new(
        LocalConfig::new(&path)
            .with_max_bytes(4096)
            .with_watch_mode(WatchMode::Notify),
    )
    .expect("the source configuration is valid");
    let handle = SnapshotHandle::new();
    let shutdown = CancellationToken::new();
    let reloader = Reloader::new(
        std::sync::Arc::new(source),
        std::sync::Arc::clone(&handle),
        None,
        ReloadLimits::new(DEBOUNCE, MAX_WAIT),
        ParseLimits::new(4096),
        false,
        shutdown.clone(),
    );
    let task = tokio::spawn(reloader.run());

    let status = wait_for(&handle, |status| status.ready, WAIT).await;
    assert_eq!(status.generation, 1);
    assert!(!status.degraded);
    assert_eq!(body_limit(&handle), 1024);

    // An atomic replacement must be observed as one new complete document.
    replace_atomically(&path, DOC_B);
    let status = wait_for(&handle, |status| status.generation == 2, WAIT).await;
    assert!(!status.degraded);
    assert_eq!(body_limit(&handle), 2048);

    // A rejected document keeps the last-known-good snapshot and never moves the generation.
    replace_atomically(&path, INVALID_DOC);
    let status = wait_for(&handle, |status| status.degraded, WAIT).await;
    assert_eq!(status.generation, 2);
    assert_eq!(body_limit(&handle), 2048);

    // The next valid document publishes again.
    replace_atomically(&path, DOC_C);
    wait_for(
        &handle,
        |status| status.generation == 3 && !status.degraded,
        WAIT,
    )
    .await;
    assert_eq!(body_limit(&handle), 4096);

    // A deleted document is unavailable, not empty: the snapshot in service must not change.
    std::fs::remove_file(&path).expect("the deletion succeeds");
    let status = wait_for(&handle, |status| status.degraded, WAIT).await;
    assert_eq!(status.generation, 3, "a deletion is not a new snapshot");
    assert_eq!(body_limit(&handle), 4096);

    // Recreating the document restores the pipeline.
    write(&path, DOC_A);
    let status = wait_for(
        &handle,
        |status| status.generation == 4 && !status.degraded,
        RECOVERY_WAIT,
    )
    .await;
    assert_eq!(body_limit(&handle), 1024);
    assert!(!status.stale, "a live source is not a stale snapshot");

    shutdown.cancel();
    match tokio::time::timeout(WAIT, task).await {
        Ok(result) => result
            .expect("the pipeline task did not panic")
            .expect("shutdown is a clean stop"),
        Err(_elapsed) => panic!("the pipeline did not stop within the timeout"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_half_written_document_never_becomes_a_snapshot() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let path = directory.path().join("resources.yaml");
    write(&path, DOC_A);

    let source = LocalFileSource::new(
        LocalConfig::new(&path)
            .with_max_bytes(4096)
            .with_watch_mode(WatchMode::Notify),
    )
    .expect("the source configuration is valid");
    let handle = SnapshotHandle::new();
    let shutdown = CancellationToken::new();
    let reloader = Reloader::new(
        std::sync::Arc::new(source),
        std::sync::Arc::clone(&handle),
        None,
        ReloadLimits::new(DEBOUNCE, MAX_WAIT),
        ParseLimits::new(4096),
        false,
        shutdown.clone(),
    );
    let task = tokio::spawn(reloader.run());
    wait_for(&handle, |status| status.ready, WAIT).await;

    // An in-place writer that has flushed only the first half: valid UTF-8, incomplete YAML. The
    // adapter reports the bytes it found; the pipeline must reject them instead of publishing a
    // partial document, so the snapshot in service cannot regress.
    let half = &DOC_B[..DOC_B.len() / 2];
    replace_atomically(&path, half);
    let status = wait_for(&handle, |status| status.degraded, WAIT).await;
    assert_eq!(
        status.generation, 1,
        "a partial document must not become a snapshot"
    );
    assert_eq!(body_limit(&handle), 1024, "the last-known-good stayed live");

    // The remaining bytes arriving as one complete write publish normally.
    replace_atomically(&path, DOC_B);
    let status = wait_for(&handle, |status| status.generation == 2, WAIT).await;
    assert!(!status.degraded, "a complete document clears degraded");
    assert_eq!(body_limit(&handle), 2048);

    shutdown.cancel();
    match tokio::time::timeout(WAIT, task).await {
        Ok(result) => result
            .expect("the pipeline task did not panic")
            .expect("shutdown is a clean stop"),
        Err(_elapsed) => panic!("the pipeline did not stop within the timeout"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_missing_document_prevents_the_first_publish() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let path = directory.path().join("resources.yaml");

    let source = LocalFileSource::new(
        LocalConfig::new(&path)
            .with_max_bytes(4096)
            .with_watch_mode(WatchMode::Notify),
    )
    .expect("the source configuration is valid");
    let handle = SnapshotHandle::new();
    let reloader = Reloader::new(
        std::sync::Arc::new(source),
        std::sync::Arc::clone(&handle),
        None,
        ReloadLimits::new(DEBOUNCE, MAX_WAIT),
        ParseLimits::new(4096),
        false,
        CancellationToken::new(),
    );

    let error = reloader
        .run()
        .await
        .expect_err("nothing can be published without a document");
    assert!(
        matches!(error, ReloadError::NotReady { .. }),
        "expected NotReady, got {error:?}"
    );
    assert!(
        handle.current().is_none(),
        "a failed startup must not publish anything"
    );
    assert!(!handle.status().ready);
}
