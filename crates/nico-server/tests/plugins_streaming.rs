//! A response-body plugin on a stream: one call per chunk, never one call for the whole body.
//!
//! The response direction is the opposite of the request direction on purpose. A request can be
//! buffered because it has an end the client declares; a response may be an event stream that never
//! ends, so no document can ask for it whole and the plugin is handed the bytes chunk by chunk.
//!
//! Two consequences are asserted here:
//!
//! - every chunk the plugin rewrote is what the client receives, which is only true if the data plane
//!   gave up length framing — a response whose `content-length` still described the upstream's bytes
//!   would be a lie after the first appended suffix;
//! - the number of plugin calls equals the number of chunks, which is what "streamed, not buffered"
//!   means in calls rather than in prose.

mod support;

use std::{sync::Arc, time::Duration};

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

/// How many events the upstream writes.
const EVENTS: usize = 4;

/// The gap the upstream leaves between two events, so they cannot coalesce in one read.
const GAP: Duration = Duration::from_millis(10);

/// What the plugin appends to every chunk it is handed.
const SUFFIX: &[u8] = b"|x";

#[tokio::test(flavor = "current_thread")]
async fn a_response_body_plugin_rewrites_each_chunk_of_a_stream() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    write_bootstrap(directory.path(), port, false, 1, 1000);

    let events: Vec<Vec<u8>> = (0..EVENTS)
        .map(|index| format!("data: {{\"event\":{index}}}\n\n").into_bytes())
        .collect();
    let upstream = MockUpstream::start(Reply::Stream {
        events: events.clone(),
        gap: GAP,
    })
    .await;

    let document = Resources::new()
        .upstream("events", upstream.address())
        .plugin(Plugin::mock_append_response("suffixer", "|x"))
        .route(
            Route::new("events", "/events/", "events")
                .plugins(&["suffixer"])
                .proxy_buffering_off(),
        )
        .render();

    let recorder = MockRecorder::new();
    let mut registry = nico_plugins::builtin_registry();
    registry
        .register(MockFactory::new(Arc::clone(&recorder)))
        .expect("the mock kind is registered once");

    let source = TestSource::with_document(&document);
    let server = RunningServer::start_with_registry(directory.path(), source, registry).await;
    let address = server.address();

    // Readiness: a 404 means a document is in service, and it is a request the streaming route never
    // sees, so the stream below is the first and only one.
    let _ready = wait_for_status(address, &Request::get("/missing", "streaming.test"), 404).await;

    let mut client = Client::connect(address).await;
    let request =
        Request::get("/events/feed", "streaming.test").header("accept", "text/event-stream");
    client
        .write_request(&request)
        .await
        .expect("the request is written");
    let head = client.read_head(READ_BUDGET).await;
    assert_eq!(head.status(), 200, "the event stream is served");
    assert!(
        head.header("content-length").is_none(),
        "a route whose chain rewrites response bytes cannot keep a length, got {:?}",
        head.header("content-length")
    );
    assert_eq!(
        head.header("transfer-encoding"),
        Some("chunked"),
        "the rewritten stream is framed by chunked encoding"
    );

    let chunks = client.read_chunks(&head, READ_BUDGET).await;
    let mut received = Vec::new();
    for (index, (_at, bytes)) in chunks.iter().enumerate() {
        assert!(
            bytes.ends_with(SUFFIX),
            "chunk {index} did not carry the plugin's suffix: {:?}",
            String::from_utf8_lossy(bytes)
        );
        received.extend_from_slice(bytes);
    }
    let mut expected = Vec::new();
    for event in &events {
        expected.extend_from_slice(event);
        expected.extend_from_slice(SUFFIX);
    }
    assert_eq!(
        received, expected,
        "every event must reach the client with exactly the bytes the plugin produced"
    );

    // The stream is complete, so every stage call has already happened: the count is a fact, not a
    // race. A buffered driver would have made one call with the whole body.
    let recorded = recorder.events();
    assert_eq!(
        recorded.len(),
        EVENTS,
        "the plugin is called once per chunk, never once for the whole body: {recorded:?}"
    );
    for event in &recorded {
        match event {
            Event::ResponseBody { plugin, len } => {
                assert_eq!(plugin, "suffixer");
                assert!(
                    *len < expected.len(),
                    "no call carried the whole body: {len} of {} bytes",
                    expected.len()
                );
            }
            other => panic!("an append-only plugin has no other stage: {other:?}"),
        }
    }

    let exit = server.cancel_and_join().await;
    assert_eq!(exit, std::process::ExitCode::SUCCESS);
}
