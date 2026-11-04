//! Tests for the process assembly.
//!
//! Running the whole process (`Server::run` owns the calling thread and installs a global runtime)
//! lives in its own test target, `tests/failed_pipeline.rs`: one real server per test binary, and a
//! unit-test binary must stay free of framework globals. What is left here is the assembly's logic,
//! driven hook by hook.

use std::{sync::Arc, time::Duration};

use nico_config::{ConfigSource, ConfigSourceError, FakeConfigSource};
use nico_core::{ParseLimits, SnapshotHandle};
use tokio::sync::watch;

use super::*;

/// Returns a source whose every load fails, so the pipeline can never publish.
fn unusable_source() -> Arc<dyn ConfigSource> {
    Arc::new(
        FakeConfigSource::new("fake").push_load(Err(ConfigSourceError::Unavailable {
            source_name: "fake",
            source: Box::new(std::io::Error::other("the fixture source is unreachable")),
        })),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn a_failing_pipeline_records_the_failure_and_cancels_the_token() {
    let shutdown = CancellationToken::new();
    let failure = Arc::new(Mutex::new(None));
    let service = ReloadService {
        reloader: Mutex::new(Some(Reloader::new(
            unusable_source(),
            SnapshotHandle::new(),
            None,
            ReloadLimits::new(Duration::from_millis(10), Duration::from_millis(50)),
            ParseLimits::new(4096),
            false,
            shutdown.clone(),
        ))),
        failure: Arc::clone(&failure),
        shutdown: shutdown.clone(),
    };
    let (_notifier, watch) = watch::channel(false);

    service.start(watch).await;

    assert!(
        matches!(lock(&failure).as_ref(), Some(ReloadError::NotReady { .. })),
        "the pipeline's failure must be recorded for the exit code"
    );
    assert!(
        shutdown.is_cancelled(),
        "a pipeline that can never publish must stop the process, not leave it serving 503s"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_pipeline_that_already_ran_is_not_started_a_second_time() {
    let shutdown = CancellationToken::new();
    let failure = Arc::new(Mutex::new(None));
    let service = ReloadService {
        reloader: Mutex::new(None),
        failure: Arc::clone(&failure),
        shutdown: shutdown.clone(),
    };
    let (_notifier, watch) = watch::channel(false);

    service.start(watch).await;

    assert!(
        lock(&failure).is_none(),
        "there was no pipeline to fail, so nothing may be recorded"
    );
    assert!(!shutdown.is_cancelled());
}
