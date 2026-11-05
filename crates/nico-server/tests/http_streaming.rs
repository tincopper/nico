//! Server-sent events through a real proxy: each event as it is written, byte for byte.
//!
//! Two properties matter and neither can be checked from a single response body:
//!
//! - **Arrival rhythm.** The upstream writes one event every 200 ms. A proxy that collected the
//!   response before forwarding it would deliver all five at once, at the end; the timestamps show
//!   which happened.
//! - **Exact bytes, unchanged framing.** The body the client assembled must equal the bytes the
//!   upstream wrote, and the response must carry no `Content-Encoding` and no injected `Server`
//!   header: this data plane does not compress and does not claim the response.

mod support;

use std::time::{Duration, Instant};

use support::http::{
    RunningServer,
    client::{READ_BUDGET, Request},
    free_port,
    resources::{Resources, Route},
    source::TestSource,
    upstream::{MockUpstream, Reply},
    write_bootstrap,
};

/// How many events the upstream writes.
const EVENTS: usize = 5;

/// The upstream's writing interval.
const GAP: Duration = Duration::from_millis(200);

/// The smallest consecutive gap the client accepts, allowing for scheduler jitter on a loaded
/// machine while still failing a proxy that delivered the whole stream at once.
const MIN_GAP: Duration = Duration::from_millis(120);

#[tokio::test(flavor = "current_thread")]
async fn events_reach_the_client_as_they_are_written_and_unchanged() {
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
        .route(Route::new("events", "/events", "events"))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;

    let mut client = support::http::client::Client::connect(server.address()).await;
    let request = Request::get("/events", "stream.test").header("accept", "text/event-stream");
    client
        .write_request(&request)
        .await
        .expect("the request is written");
    let head = client.read_head(READ_BUDGET).await;
    assert_eq!(head.status(), 200, "the event stream must be served");

    let expected: Vec<u8> = events.iter().flatten().copied().collect();
    assert_eq!(
        head.header("transfer-encoding"),
        Some("chunked"),
        "an event stream is chunked, and the proxy must forward the framing unchanged"
    );
    assert_eq!(
        head.header("content-encoding"),
        None,
        "this data plane must not compress a response"
    );
    assert_eq!(
        head.header("server"),
        None,
        "this data plane must not inject a `Server` header"
    );

    let chunks = client.read_chunks(&head, READ_BUDGET).await;
    let raw: Vec<String> = chunks
        .iter()
        .map(|(at, bytes)| format!("{}({})", at.elapsed().as_millis(), bytes.len()))
        .collect();
    let mut received = Vec::new();
    let mut boundaries = Vec::new();
    let mut total = 0;
    for event in &events {
        total += event.len();
        boundaries.push(total);
    }
    let mut arrivals: Vec<Instant> = Vec::new();
    let mut next = 0;
    for (at, bytes) in chunks {
        received.extend_from_slice(&bytes);
        while next < boundaries.len() && received.len() >= boundaries[next] {
            arrivals.push(at);
            next += 1;
        }
    }

    assert_eq!(
        received.len(),
        expected.len(),
        "the stream ended after {} of {} bytes",
        received.len(),
        expected.len()
    );
    assert_eq!(
        received, expected,
        "the events changed while passing through"
    );
    assert_eq!(arrivals.len(), EVENTS, "every event must have arrived");
    for index in 1..arrivals.len() {
        let gap = arrivals[index].saturating_duration_since(arrivals[index - 1]);
        assert!(
            gap >= MIN_GAP,
            "events {} and {} arrived {gap:?} apart, so the stream was buffered instead of forwarded; reads: {raw:?}",
            index - 1,
            index
        );
    }
    let span = arrivals[EVENTS - 1].saturating_duration_since(arrivals[0]);
    assert!(
        span >= MIN_GAP * (EVENTS as u32 - 1),
        "all events arrived within {span:?}, which is not the upstream's writing rhythm"
    );

    assert_eq!(
        server.cancel_and_join().await,
        std::process::ExitCode::SUCCESS
    );
}
