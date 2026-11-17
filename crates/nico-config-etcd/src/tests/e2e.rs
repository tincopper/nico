//! The real client over the real wire: the [`GrpcEtcdClient`] mapping and the full adapter chain,
//! verified against the in-process etcd mock server.
//!
//! The adapter's semantics are table-tested against the scripted client; what only a real
//! environment can prove is the *mapping* — status classification, inclusive revisions, created
//! confirmations, cancellations — and that the whole chain composes on an actual gRPC connection.
//! These tests are those proofs: a real `etcd-client` connection to a loopback server speaking the
//! official etcd v3 protocol.

use std::sync::Arc;

use bytes::Bytes;
use nico_config::{ConfigEvent, ConfigSource, ConfigSourceError};

use crate::{
    GrpcEtcdClient,
    client::{EtcdClient, EtcdError, WatchEvent, WatchOutcome, mock_server::MockServer},
    mock::{MAX_BYTES, MOCK_KEY},
    source::EtcdConfigSource,
};

/// Connects a real client to a fresh mock server.
async fn connected(server: &MockServer) -> GrpcEtcdClient {
    GrpcEtcdClient::connect(&[server.endpoint()])
        .await
        .expect("the real client connects to the mock server")
}

/// Reads the next item from an adapter watch stream.
async fn next(
    watch: &mut nico_config::ConfigWatch,
) -> Option<Result<ConfigEvent, ConfigSourceError>> {
    std::future::poll_fn(|cx| watch.as_mut().poll_next(cx)).await
}

/// Asserts one delivered outcome is the expected events batch.
fn assert_events(outcome: WatchOutcome, expected: &[WatchEvent]) {
    match outcome {
        WatchOutcome::Events { events } => assert_eq!(&events, expected),
        other => panic!("expected events, got {other:?}"),
    }
}

// --- the client seam over the wire ------------------------------------------------------------------

#[tokio::test]
async fn load_reads_the_newest_revision_over_the_real_wire() {
    let server = MockServer::start().await.expect("the mock etcd starts");
    server.state().put(b"doc-1");
    server.state().put(b"doc-2");
    let client = connected(&server).await;

    let entry = client
        .get(MOCK_KEY)
        .await
        .expect("the get succeeds")
        .expect("the key exists");

    assert_eq!(entry.value.as_ref(), b"doc-2");
    assert_eq!(entry.mod_revision, 2);
}

#[tokio::test]
async fn a_missing_key_reads_as_nothing() {
    let server = MockServer::start().await.expect("the mock etcd starts");
    let client = connected(&server).await;

    let entry = client.get(MOCK_KEY).await.expect("the get succeeds");

    assert!(entry.is_none());
}

#[tokio::test]
async fn auth_refusal_maps_to_the_seams_auth_error() {
    let server = MockServer::start().await.expect("the mock etcd starts");
    // Connecting itself is not refused: the client carries no credentials, and the refusal happens
    // when the server answers an operation.
    server.state().set_require_auth(true);
    let client = connected(&server).await;

    let error = client.get(MOCK_KEY).await.expect_err("the read is refused");

    assert!(matches!(error, EtcdError::Auth { .. }));
}

#[tokio::test]
async fn watch_replays_history_from_the_start_revision_inclusive() {
    let server = MockServer::start().await.expect("the mock etcd starts");
    server.state().put(b"doc-1");
    server.state().put(b"doc-2");
    let client = connected(&server).await;

    let mut stream = client
        .watch(MOCK_KEY, 2)
        .await
        .expect("the watch establishes");
    let outcome = stream
        .recv()
        .await
        .expect("an outcome")
        .expect("a delivered outcome");

    assert_events(
        outcome,
        &[WatchEvent::Put {
            value: Bytes::from_static(b"doc-2"),
            mod_revision: 2,
        }],
    );
}

#[tokio::test]
async fn watch_streams_live_changes_over_the_wire() {
    let server = MockServer::start().await.expect("the mock etcd starts");
    let client = connected(&server).await;

    // The client's watch call does not return until the server confirmed the subscription, so the
    // change below cannot race the watcher's creation.
    let mut stream = client
        .watch(MOCK_KEY, 1)
        .await
        .expect("the watch establishes");
    server.state().put(b"live-doc");
    let outcome = stream
        .recv()
        .await
        .expect("an outcome")
        .expect("a delivered outcome");

    assert_events(
        outcome,
        &[WatchEvent::Put {
            value: Bytes::from_static(b"live-doc"),
            mod_revision: 1,
        }],
    );
}

#[tokio::test]
async fn a_delete_surfaces_as_the_seams_delete_event() {
    let server = MockServer::start().await.expect("the mock etcd starts");
    server.state().put(b"doc-1");
    let client = connected(&server).await;

    let mut stream = client
        .watch(MOCK_KEY, 1)
        .await
        .expect("the watch establishes");
    let replay = stream
        .recv()
        .await
        .expect("the replay")
        .expect("a delivered outcome");
    assert_events(
        replay,
        &[WatchEvent::Put {
            value: Bytes::from_static(b"doc-1"),
            mod_revision: 1,
        }],
    );

    server.state().delete();
    let outcome = stream
        .recv()
        .await
        .expect("the deletion")
        .expect("a delivered outcome");
    assert_events(outcome, &[WatchEvent::Delete]);
}

#[tokio::test]
async fn a_compacted_start_is_canceled_naming_the_revision() {
    let server = MockServer::start().await.expect("the mock etcd starts");
    server.state().put(b"doc-1");
    server.state().put(b"doc-2");
    server.state().compact(1);
    let client = connected(&server).await;

    let mut stream = client
        .watch(MOCK_KEY, 1)
        .await
        .expect("the watch establishes");
    let outcome = stream
        .recv()
        .await
        .expect("the cancellation")
        .expect("a delivered outcome");

    assert!(matches!(
        outcome,
        WatchOutcome::Canceled {
            compact_revision: Some(1)
        }
    ));
}

#[tokio::test]
async fn a_start_past_the_compaction_serves_the_live_tail() {
    let server = MockServer::start().await.expect("the mock etcd starts");
    server.state().put(b"doc-1");
    server.state().put(b"doc-2");
    server.state().compact(1);
    let client = connected(&server).await;

    // The reload answered the cancellation above; the next subscription resumes one past the
    // revision that load carried.
    let mut stream = client
        .watch(MOCK_KEY, 3)
        .await
        .expect("the watch establishes");
    server.state().put(b"doc-3");
    let outcome = stream
        .recv()
        .await
        .expect("the live event")
        .expect("a delivered outcome");

    assert_events(
        outcome,
        &[WatchEvent::Put {
            value: Bytes::from_static(b"doc-3"),
            mod_revision: 3,
        }],
    );
}

// --- the whole chain: adapter over the real client over the wire ------------------------------------

#[tokio::test]
async fn the_adapter_delivers_snapshots_over_the_real_wire() {
    let server = MockServer::start().await.expect("the mock etcd starts");
    server.state().put(b"doc-1");
    let client: Arc<dyn EtcdClient> = Arc::new(connected(&server).await);
    let source = EtcdConfigSource::new(MOCK_KEY, MAX_BYTES, client);

    let loaded = source.load().await.expect("the load succeeds");
    assert_eq!(loaded.revision().seq(), 1);
    assert_eq!(loaded.bytes().as_ref(), b"doc-1");

    server.state().put(b"doc-2");
    let mut watch = source
        .watch(Some(loaded.revision()))
        .await
        .expect("the watch establishes");
    let event = next(&mut watch)
        .await
        .expect("an event")
        .expect("a delivered event");
    let ConfigEvent::Snapshot(version) = event else {
        panic!("expected a snapshot, got {event:?}");
    };
    assert_eq!(version.revision().seq(), 2);
    assert_eq!(version.bytes().as_ref(), b"doc-2");
}

#[tokio::test]
async fn a_compacted_start_answers_one_resync() {
    let server = MockServer::start().await.expect("the mock etcd starts");
    server.state().put(b"doc-1");
    server.state().put(b"doc-2");
    server.state().compact(1);
    let client: Arc<dyn EtcdClient> = Arc::new(connected(&server).await);
    let source = EtcdConfigSource::new(MOCK_KEY, MAX_BYTES, client);

    let mut watch = source.watch(None).await.expect("the watch establishes");
    let event = next(&mut watch)
        .await
        .expect("an event")
        .expect("a delivered event");
    assert_eq!(event, ConfigEvent::ResyncRequired);

    // The task ends after the resync: the stream is over, and the pipeline reloads.
    assert!(next(&mut watch).await.is_none());
}

#[tokio::test]
async fn a_missing_key_is_unavailable_at_the_contract() {
    let server = MockServer::start().await.expect("the mock etcd starts");
    let client: Arc<dyn EtcdClient> = Arc::new(connected(&server).await);
    let source = EtcdConfigSource::new(MOCK_KEY, MAX_BYTES, client);

    let error = source.load().await.expect_err("the key does not exist");

    assert!(matches!(
        error,
        ConfigSourceError::Unavailable {
            source_name: "etcd",
            ..
        }
    ));
}

#[tokio::test]
async fn an_auth_refusal_is_unavailable_at_the_contract() {
    let server = MockServer::start().await.expect("the mock etcd starts");
    server.state().set_require_auth(true);
    let client: Arc<dyn EtcdClient> = Arc::new(connected(&server).await);
    let source = EtcdConfigSource::new(MOCK_KEY, MAX_BYTES, client);

    let error = source.load().await.expect_err("the read is refused");

    assert!(matches!(
        error,
        ConfigSourceError::Unavailable {
            source_name: "etcd",
            ..
        }
    ));
}

// --- the endpoint list: failover beneath the seam ---------------------------------------------------

#[tokio::test]
async fn a_dead_endpoint_in_the_list_does_not_break_the_client() {
    let server = MockServer::start().await.expect("the mock etcd starts");
    server.state().put(b"doc-1");
    let live = server.endpoint();
    let dead = format!("{live}/unreachable");

    // The client's transports live on the runtime that connected them, so that runtime must
    // outlive the client — the fact the bootstrap wiring honours with its daemon thread. The
    // dedicated thread here is the same shape.
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the client runtime builds");
        let client = runtime
            .block_on(async { GrpcEtcdClient::connect(&[dead, live]).await })
            .expect("connect succeeds with a dead endpoint listed");
        let _ = sender.send(client);
        // Drive the runtime forever: the client's transports depend on it.
        runtime.block_on(std::future::pending::<()>());
    });
    let client = receiver
        .recv()
        .expect("the connect thread delivers the client");

    // Both operations run on the test's runtime, not the client's: `GrpcEtcdClient` is `Send +
    // Sync` and only the transports are pinned to their thread.
    let entry = client
        .get(MOCK_KEY)
        .await
        .expect("the get succeeds from a foreign runtime")
        .expect("the key exists");
    assert_eq!(entry.value.as_ref(), b"doc-1");

    let mut stream = client
        .watch(MOCK_KEY, 2)
        .await
        .expect("the watch establishes");
    server.state().put(b"doc-2");
    let outcome = stream
        .recv()
        .await
        .expect("an outcome")
        .expect("a delivered outcome");
    assert_events(
        outcome,
        &[WatchEvent::Put {
            value: Bytes::from_static(b"doc-2"),
            mod_revision: 2,
        }],
    );
}
