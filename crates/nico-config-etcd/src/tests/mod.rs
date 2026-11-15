//! The etcd adapter's semantics, driven entirely by the scripted client.
//!
//! Every test states one row of the semantics table: what the "server" sends, and what the source
//! answers. The mock is the real environment these tests run against — the adapter code is the same
//! code a real etcd client would drive, and the only thing a real integration adds is an
//! implementation of the same seam.

use std::sync::Arc;

use bytes::Bytes;
use nico_config::{ConfigEvent, ConfigSource, ConfigSourceError, ConfigVersion, Revision};

use crate::{
    client::{EtcdError, KvEntry, WatchEvent, WatchOutcome},
    mock::{MAX_BYTES, MOCK_KEY, ScriptedWatch, entry},
    source::{EtcdConfigSource, WATCH_CAPACITY},
};

/// Builds a source over a freshly scripted client.
fn scripted(
    gets: Vec<Result<Option<KvEntry>, EtcdError>>,
    watches: Vec<ScriptedWatch>,
) -> Arc<EtcdConfigSource> {
    let client = crate::mock::MockClient::default();
    for get in gets {
        client.script_get(get);
    }
    for watch in watches {
        client.script_watch(watch);
    }
    Arc::new(EtcdConfigSource::new(MOCK_KEY, MAX_BYTES, Arc::new(client)))
}

/// A load that finds one document.
fn found(revision: u64, value: &str) -> Result<Option<KvEntry>, EtcdError> {
    Ok(entry(revision, value))
}

/// Collects every event the watch stream produces, ending when the stream does.
///
/// A bounded guard is unnecessary: every scripted outcome ends the stream, so a test that never
/// terminates would be pointing at a real hang rather than polling forever.
async fn collect(watch: nico_config::ConfigWatch) -> Vec<Result<ConfigEvent, ConfigSourceError>> {
    let mut stream = watch;
    let mut items = Vec::new();
    while let Some(item) = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
        items.push(item);
    }
    items
}

// --- load --------------------------------------------------------------------------------------------

#[tokio::test]
async fn load_maps_the_revision_and_the_bytes() {
    let source = scripted(vec![found(7, "doc-7")], vec![]);

    let version = source.load().await.expect("the key exists");

    assert_eq!(version.revision().seq(), 7);
    assert_eq!(version.bytes(), &Bytes::from_static(b"doc-7"));
}

#[tokio::test]
async fn a_missing_key_is_unavailable_not_an_empty_document() {
    let source = scripted(vec![Ok(None)], vec![]);

    let error = source.load().await.expect_err("the key does not exist");

    assert!(
        matches!(error, ConfigSourceError::Unavailable { source_name, .. } if source_name == "etcd"),
        "a deleted key is a dead source, not emptiness: {error:?}"
    );
}

#[tokio::test]
async fn an_oversized_document_is_too_large() {
    let big = "x".repeat(MAX_BYTES + 1);
    let source = scripted(vec![found(3, &big)], vec![]);

    let error = source
        .load()
        .await
        .expect_err("the document exceeds the bound");

    assert!(
        matches!(
            error,
            ConfigSourceError::TooLarge { size, limit } if size == MAX_BYTES + 1 && limit == MAX_BYTES
        ),
        "expected TooLarge, got {error:?}"
    );
}

#[tokio::test]
async fn a_non_utf8_document_is_refused() {
    // A bare `0xFF` byte is the canonical non-UTF-8: it cannot appear in any encoding of a valid
    // document, so a pass here would be a pass by accident.
    let source = scripted(
        vec![Ok(Some(KvEntry {
            value: Bytes::from_static(&[0x23, 0xFF, 0x61]),
            mod_revision: 5,
        }))],
        vec![],
    );

    let error = source
        .load()
        .await
        .expect_err("documents are text, not bytes");

    assert!(matches!(error, ConfigSourceError::NotUtf8), "got {error:?}");
}

#[tokio::test]
async fn a_consumer_that_falls_behind_gets_one_resync() {
    // The fake source's rule, applied here: the producer never blocks and never buffers unbounded
    // history. A burst wider than the channel delivers what fits, then collapses the rest into one
    // resync — the pipeline reloads the document instead of replaying a queue it cannot keep.
    let burst = 40usize;
    let events: Vec<WatchOutcome> = (0..burst as u64)
        .map(|revision| WatchOutcome::Events {
            events: vec![WatchEvent::Put {
                value: bytes::Bytes::from(format!("document {revision}")),
                mod_revision: revision + 1,
            }],
        })
        .collect();
    let source = scripted(
        vec![found(1, "document 0")],
        vec![ScriptedWatch::once(events)],
    );

    let watch = source.watch(None).await.expect("the watch is established");
    let items = collect(watch).await;

    let snapshots = items
        .iter()
        .filter_map(|item| item.as_ref().ok())
        .filter(|event| matches!(event, ConfigEvent::Snapshot(_)))
        .count();
    let resyncs = items
        .iter()
        .filter_map(|item| item.as_ref().ok())
        .filter(|event| matches!(event, ConfigEvent::ResyncRequired))
        .count();

    assert_eq!(
        snapshots, WATCH_CAPACITY,
        "the channel delivered exactly what fit, in order"
    );
    assert_eq!(
        resyncs, 1,
        "everything past the capacity collapsed into one resync"
    );
    assert!(
        items.len() <= WATCH_CAPACITY + 1,
        "the collapse ends the stream: nothing is delivered after the resync"
    );
}

// --- watch --------------------------------------------------------------------------------------------

#[tokio::test]
async fn events_become_snapshots_in_revision_order() {
    let source = scripted(
        vec![found(1, "doc-1")],
        vec![ScriptedWatch::once(vec![
            WatchOutcome::Established,
            WatchOutcome::Events {
                events: vec![
                    WatchEvent::Put {
                        value: Bytes::from_static(b"doc-2"),
                        mod_revision: 2,
                    },
                    WatchEvent::Put {
                        value: Bytes::from_static(b"doc-4"),
                        mod_revision: 4,
                    },
                ],
            },
        ])],
    );

    let _ = source.load().await.expect("the key exists");
    let events = collect(
        source
            .watch(Some(Revision::comparable(1)))
            .await
            .expect("the watch establishes"),
    )
    .await;

    let snapshots: Vec<&ConfigVersion> = events
        .iter()
        .filter_map(|item| item.as_ref().ok())
        .filter_map(|event| match event {
            ConfigEvent::Snapshot(version) => Some(version),
            _ => None,
        })
        .collect();
    assert_eq!(snapshots.len(), 2, "each put is one complete snapshot");
    assert_eq!(snapshots[0].revision().seq(), 2);
    assert_eq!(snapshots[0].bytes(), &Bytes::from_static(b"doc-2"));
    assert_eq!(snapshots[1].revision().seq(), 4);
}

#[tokio::test]
async fn events_at_or_before_the_start_are_dropped() {
    // The server may resend older history when a watch is re-established; the source must filter it,
    // or the pipeline reloads a document it already served.
    let source = scripted(
        vec![found(5, "doc-5")],
        vec![ScriptedWatch::once(vec![
            WatchOutcome::Established,
            WatchOutcome::Events {
                events: vec![
                    WatchEvent::Put {
                        value: Bytes::from_static(b"old-4"),
                        mod_revision: 4,
                    },
                    WatchEvent::Put {
                        value: Bytes::from_static(b"dup-5"),
                        mod_revision: 5,
                    },
                    WatchEvent::Put {
                        value: Bytes::from_static(b"new-6"),
                        mod_revision: 6,
                    },
                ],
            },
        ])],
    );

    let _ = source.load().await.expect("the key exists");
    let events = collect(
        source
            .watch(Some(Revision::comparable(5)))
            .await
            .expect("the watch establishes"),
    )
    .await;

    let snapshots: Vec<u64> = events
        .iter()
        .filter_map(|item| item.as_ref().ok())
        .filter_map(|event| match event {
            ConfigEvent::Snapshot(version) => Some(version.revision().seq()),
            _ => None,
        })
        .collect();
    assert_eq!(
        snapshots,
        [6],
        "revisions at or before the start are history, delivered neither twice nor once more"
    );
}

#[tokio::test]
async fn a_compacted_watch_is_a_resync() {
    let source = scripted(
        vec![found(9, "doc-9")],
        vec![ScriptedWatch::once(vec![
            WatchOutcome::Established,
            WatchOutcome::Canceled {
                compact_revision: Some(20),
            },
        ])],
    );

    let _ = source.load().await.expect("the key exists");
    let events = collect(
        source
            .watch(Some(Revision::comparable(9)))
            .await
            .expect("the watch establishes"),
    )
    .await;

    assert!(
        events
            .iter()
            .any(|item| matches!(item, Ok(ConfigEvent::ResyncRequired))),
        "a compacted watch cannot continue incrementally: {:?}",
        events
    );
    assert!(
        events.len() == 1,
        "the resync ends the stream, so the pipeline reloads the whole document: {events:?}"
    );
}

#[tokio::test]
async fn a_fragmented_watch_is_a_resync() {
    // Reassembly is not the adapter's job: a fragmented watch is answered with a full reload.
    let source = scripted(
        vec![found(3, "doc-3")],
        vec![ScriptedWatch::once(vec![
            WatchOutcome::Established,
            WatchOutcome::Fragmented,
        ])],
    );

    let _ = source.load().await.expect("the key exists");
    let events = collect(
        source
            .watch(Some(Revision::comparable(3)))
            .await
            .expect("the watch establishes"),
    )
    .await;

    assert!(
        events
            .iter()
            .any(|item| matches!(item, Ok(ConfigEvent::ResyncRequired))),
        "a fragmented watch is a resync, not a guess: {events:?}"
    );
}

#[tokio::test]
async fn a_deleted_key_is_unavailable_not_empty() {
    // An empty document would parse and publish "clear everything", legally overwriting the last
    // known good snapshot. The deletion is a dead source instead.
    let source = scripted(
        vec![found(4, "doc-4")],
        vec![ScriptedWatch::once(vec![
            WatchOutcome::Established,
            WatchOutcome::Events {
                events: vec![WatchEvent::Delete],
            },
        ])],
    );

    let _ = source.load().await.expect("the key exists");
    let events = collect(
        source
            .watch(Some(Revision::comparable(4)))
            .await
            .expect("the watch establishes"),
    )
    .await;

    assert!(
        events
            .iter()
            .any(|item| matches!(item, Err(ConfigSourceError::Unavailable { .. }))),
        "a deletion is a dead source: {events:?}"
    );
    assert!(
        events
            .iter()
            .all(|item| !matches!(item, Ok(ConfigEvent::Snapshot(_)))),
        "no snapshot is produced from a deletion: {events:?}"
    );
}

#[tokio::test]
async fn an_auth_refusal_is_unavailable() {
    let source = scripted(
        vec![Err(EtcdError::Auth {
            message: "permission denied".into(),
        })],
        vec![],
    );

    let error = source.load().await.expect_err("the server refused");

    assert!(
        matches!(error, ConfigSourceError::Unavailable { .. }),
        "auth failures are unreachable-source failures: {error:?}"
    );
}

#[tokio::test]
async fn an_oversized_event_is_too_large() {
    let big = "x".repeat(MAX_BYTES + 1);
    let source = scripted(
        vec![found(1, "doc-1")],
        vec![ScriptedWatch::once(vec![
            WatchOutcome::Established,
            WatchOutcome::Events {
                events: vec![WatchEvent::Put {
                    value: Bytes::from(big),
                    mod_revision: 2,
                }],
            },
        ])],
    );

    let _ = source.load().await.expect("the key exists");
    let events = collect(
        source
            .watch(Some(Revision::comparable(1)))
            .await
            .expect("the watch establishes"),
    )
    .await;

    assert!(
        events
            .iter()
            .any(|item| matches!(item, Err(ConfigSourceError::TooLarge { .. }))),
        "an oversized event answers the same bound a load would: {events:?}"
    );
}

#[tokio::test]
async fn an_ended_stream_is_unavailable() {
    // A stream that ends without a cancellation is a connection that died: the contract's answer is
    // "cannot be read", which is what tells the pipeline to re-establish later.
    let source = scripted(vec![found(1, "doc-1")], vec![ScriptedWatch::once(vec![])]);

    let _ = source.load().await.expect("the key exists");
    let events = collect(
        source
            .watch(Some(Revision::comparable(1)))
            .await
            .expect("the watch establishes"),
    )
    .await;

    assert!(
        events
            .iter()
            .any(|item| matches!(item, Err(ConfigSourceError::Unavailable { .. }))),
        "an ended stream is a dead source: {events:?}"
    );
}

#[tokio::test]
async fn the_client_is_asked_for_the_right_key_and_start() {
    // The inclusive-start rule, stated where the request is made: load reads 7, watch asks for 8.
    let client = Arc::new(crate::mock::MockClient::default());
    client.script_get(found(7, "doc-7"));
    client.script_watch(ScriptedWatch::once(vec![]));
    let source = Arc::new(EtcdConfigSource::new(
        crate::mock::MOCK_KEY,
        MAX_BYTES,
        Arc::clone(&client) as Arc<dyn crate::client::EtcdClient>,
    ));

    let _ = source.load().await.expect("the key exists");
    let _ = source
        .watch(Some(Revision::comparable(7)))
        .await
        .expect("the watch establishes");

    assert_eq!(
        client.watched(),
        [(crate::mock::MOCK_KEY.to_owned(), 8)],
        "the subscription resumes one past the revision the load read, inclusive of 8"
    );
}

// The real client over the real wire, against the in-process etcd mock server.
#[cfg(feature = "client")]
mod e2e;
