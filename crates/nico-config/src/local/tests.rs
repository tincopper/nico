use std::{
    future::poll_fn,
    path::Path,
    time::{Duration, Instant},
};

use nico_core::RevisionOrdering;
use tempfile::TempDir;

use super::*;

const DOC_A: &[u8] =
    b"apiVersion: gateway/v1\nrequest:\n  max_body_bytes: 1024\n  timeout_ms: 1000\n";
const DOC_B: &[u8] =
    b"apiVersion: gateway/v1\nrequest:\n  max_body_bytes: 2048\n  timeout_ms: 2000\n";

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
        let source =
            LocalFileSource::new(config(&path).with_max_bytes(16)).expect("configuration is valid");

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
