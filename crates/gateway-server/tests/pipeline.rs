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
//! still used where its own behaviour is the subject, namely a consumer that falls behind. Both
//! fixtures live in [`support`].

mod support;

use std::{sync::Arc, time::Duration};

use gateway_config::{
    CacheMeta, CacheStore, ConfigEvent, ConfigSource, ConfigVersion, FakeConfigSource,
};
use gateway_core::{ConfigDigest, ErrorCategory, Revision};
use gateway_server::{ReloadError, serve};
use tokio_util::sync::CancellationToken;

use crate::support::{
    BootstrapFixture, ChannelSource, DEBOUNCE, DOC_A, DOC_B, DOC_C, Harness, INVALID_DOC, MAX_WAIT,
    STEP, bootstrap,
};

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
    let bootstrap = bootstrap(directory.path(), BootstrapFixture::default());
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
        .handle()
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
    let bootstrap = bootstrap(directory.path(), BootstrapFixture::default());
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
        if source.is_watching() {
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
