//! A route that declared `buffers_body`: the plugin sees the whole body, once, before it is forwarded.
//!
//! The mechanism has an observable consequence and this file asserts exactly it: the request head is
//! forwarded as soon as it arrives, the body is held back until the client has finished sending, and
//! only then does one plugin call rewrite the body the upstream receives. That is why the upstream's
//! first body byte cannot arrive before the client wrote its last one.
//!
//! Framing is the data plane's decision, not the plugin's: the client declared a `content-length`
//! that describes none of the bytes the upstream will get, so the header is removed and the request
//! is re-framed as chunked. The assertions below read those two facts off the upstream's head.

mod support;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use support::http::{
    RunningServer,
    client::{Client, READ_BUDGET, Request},
    free_port,
    mock::{Event, MockFactory, MockRecorder},
    resources::{Plugin, Resources, Route},
    source::TestSource,
    upstream::{MockUpstream, Reply},
    wait_for_status, write_bootstrap,
};

/// How long a wait driven by the mock upstream or the recorder may take.
const BUDGET: Duration = Duration::from_secs(10);

/// The bytes the route buffers, split into two uploads by the scenario.
///
/// A real multi-byte body rather than a short constant: the buffered mode is about holding bytes the
/// client has not finished writing, and an empty or one-piece body would not exercise it.
const PAYLOAD: &[u8] = b"the-whole-upload-arrives-in-two-pieces-and-the-plugin-sees-it-once";

#[tokio::test(flavor = "current_thread")]
async fn a_buffered_route_forwards_the_rewritten_body_after_the_upload_finishes() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    write_bootstrap(directory.path(), port, false, 1, 1000);

    let upstream = MockUpstream::start(Reply::Body {
        body: b"ok".to_vec(),
        chunk: usize::MAX,
    })
    .await;

    let document = Resources::new()
        .upstream("upload-backend", upstream.address())
        .plugin(Plugin::mock_rewrite_body("upload-guard", "seen[", "]").buffers_body())
        .route(Route::new("upload", "/upload/", "upload-backend").plugins(&["upload-guard"]))
        .render();

    let recorder = MockRecorder::new();
    let mut registry = nico_plugins::builtin_registry();
    registry
        .register(MockFactory::new(Arc::clone(&recorder)))
        .expect("the mock kind is registered once");

    let source = TestSource::with_document(&document);
    let server = RunningServer::start_with_registry(directory.path(), source, registry).await;
    let address = server.address();
    // Readiness: a 404 means a document is in service. The probe deliberately never reaches the
    // upstream, so the upload below is the first request it observes and its records are the
    // scenario's own.
    let _ready = wait_for_status(address, &Request::get("/missing", "buffered.test"), 404).await;

    let (first, second) = PAYLOAD.split_at(PAYLOAD.len() / 2);
    let request =
        Request::new("POST", "/upload/file", "buffered.test").body(PAYLOAD.to_vec(), usize::MAX);
    let mut client = Client::connect(address).await;
    client
        .write_head(&request)
        .await
        .expect("the request head is written");
    client
        .write_bytes(first)
        .await
        .expect("the first half of the body is written");

    // The head reached the upstream, and no body byte had arrived by then. That alone is a snapshot
    // (the mock records the head first), so the proof that the body really was withheld is the
    // arrival instant checked below, against the moment the client finished uploading: this check
    // only states that the head and the body are not one write.
    let observed = upstream.wait_for_request(BUDGET).await;
    assert_eq!(
        observed.first_body_at(),
        None,
        "the head reached the upstream with no body byte behind it"
    );
    assert_eq!(
        observed.header("transfer-encoding"),
        Some("chunked"),
        "the buffered route states the framing itself, in the request-header hook; head: {}",
        String::from_utf8_lossy(observed.head())
    );
    assert_eq!(
        observed.header("content-length"),
        None,
        "the length the client declared describes neither the size nor the existence of the bytes sent"
    );

    // The rest of the upload. Its start instant is what makes the hold-back a measured fact rather
    // than a lucky snapshot: no body byte may reach the upstream before this point.
    let second_half_started = Instant::now();
    client
        .write_bytes(second)
        .await
        .expect("the second half of the body is written");

    let head = client.read_head(READ_BUDGET).await;
    assert_eq!(head.status(), 200, "the buffered upload is forwarded");
    assert_eq!(
        client.read_body(&head, READ_BUDGET).await,
        b"ok",
        "the upstream's answer is passed back"
    );

    let observed = upstream.wait_for_request(BUDGET).await;
    let expected = format!("seen[{}]", String::from_utf8_lossy(PAYLOAD));
    assert_eq!(
        String::from_utf8_lossy(&observed.body()),
        expected,
        "the upstream receives exactly the bytes the plugin produced"
    );
    assert!(
        observed
            .first_body_at()
            .is_some_and(|first_at| first_at >= second_half_started),
        "the first body byte arrived before the client finished the upload, so the body was not held back"
    );

    // One call, with the whole body, and the driver said the body was complete. A streamed route
    // would have produced one call per chunk with `complete` false on all but the last.
    let recorded = recorder.events();
    assert_eq!(
        recorded.len(),
        1,
        "a buffered route calls the body stage once, got {recorded:?}"
    );
    match &recorded[0] {
        Event::RequestBody {
            plugin,
            len,
            complete,
        } => {
            assert_eq!(plugin, "upload-guard");
            assert_eq!(*len, PAYLOAD.len(), "the plugin was handed the whole body");
            assert!(
                *complete,
                "the buffered mode always ends the body in one call"
            );
        }
        other => panic!("expected one request-body call, got {other:?}"),
    }

    let exit = server.cancel_and_join().await;
    assert_eq!(exit, std::process::ExitCode::SUCCESS);
}
