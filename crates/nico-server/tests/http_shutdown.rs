//! Graceful shutdown with a stream in flight: the configured drain window is what decides.
//!
//! Pingora's own default grace period is 300 seconds, so a process that fails to configure it looks
//! healthy and then takes five minutes to stop. The fixture therefore writes explicit millisecond
//! values and the assertions are about *those* values:
//!
//! - the event stream keeps flowing after shutdown starts (draining, not an immediate cut);
//! - the connection is closed only after `grace_period` **and** the runtime drain window elapsed,
//!   measured in seconds, not minutes;
//! - `serve` returns, and the bytes received before the cut are an exact prefix of what the
//!   upstream sent.
//!
//! Whole seconds are the unit Pingora reads and both windows round up, so the fixture's
//! `grace_period_ms: 1200` means two seconds of grace. `graceful_shutdown_ms` is the bound on
//! *stopping the runtime after the grace period*, not a second window for the request: the service
//! tasks are cancelled when the grace period ends, which is why the measured drain lands on the
//! grace boundary and not past it.

mod support;

use std::time::{Duration, Instant};

use support::http::{
    RunningServer,
    client::{Client, READ_BUDGET, Request},
    free_port,
    resources::{Resources, Route},
    source::TestSource,
    upstream::{MockUpstream, Reply},
    write_bootstrap,
};

/// The configured grace period: how long in-flight requests keep running after shutdown begins.
const GRACE_MS: u64 = 1_200;

/// The configured upper bound on draining once the grace period has elapsed.
const GRACEFUL_MS: u64 = 2_000;

/// Interval between two events, so "still draining" is observable as more than one event.
const GAP: Duration = Duration::from_millis(100);

/// How many events the upstream is prepared to write; far more than the drain window needs, so the
/// stream is definitely still live when shutdown starts.
const EVENTS: usize = 200;

#[tokio::test(flavor = "current_thread")]
async fn a_stream_in_flight_survives_the_configured_grace_period_and_then_stops() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    let bootstrap = write_bootstrap(directory.path(), port, false, GRACE_MS, GRACEFUL_MS);

    // The bootstrap value is a fact the test can state: the process was told 1200 ms, not 300 s.
    assert_eq!(bootstrap.runtime().grace_period_ms(), GRACE_MS);
    assert_eq!(bootstrap.runtime().graceful_shutdown_ms(), GRACEFUL_MS);

    let events: Vec<Vec<u8>> = (0..EVENTS)
        .map(|index| format!("data: {{\"event\":{index}}}\n\n").into_bytes())
        .collect();
    let expected: Vec<u8> = events.iter().flatten().copied().collect();
    let upstream = MockUpstream::start(Reply::Stream { events, gap: GAP }).await;

    let document = Resources::new()
        .upstream("events", upstream.address())
        .route(Route::new("events", "/events", "events"))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;

    let mut client = Client::connect(server.address()).await;
    let request = Request::get("/events", "shutdown.test").header("accept", "text/event-stream");
    client
        .write_request(&request)
        .await
        .expect("the request is written");
    let head = client.read_head(READ_BUDGET).await;
    assert_eq!(head.status(), 200, "the stream must start before shutdown");

    // Read two events, so the response is provably in flight when shutdown begins.
    let mut received = Vec::new();
    for _event in 0..2 {
        let chunk = client
            .next_chunk(Duration::from_secs(10))
            .await
            .expect("the stream delivers an event before shutdown");
        received.extend_from_slice(&chunk);
    }

    let cancelled_at = Instant::now();
    server.begin_shutdown();

    // Keep reading until the server releases the connection.
    let mut after_cancel = 0;
    while let Some(chunk) = client.next_chunk(Duration::from_secs(10)).await {
        received.extend_from_slice(&chunk);
        after_cancel += 1;
    }
    let drained = cancelled_at.elapsed();

    assert!(
        after_cancel > 0,
        "the stream was cut the instant shutdown began, so no grace period was applied"
    );
    // The configured grace period is 1200 ms, which Pingora rounds up to two whole seconds. A
    // smaller window (or none) would release the connection well before that, and Pingora's own
    // 300-second default would hold it far longer.
    assert!(
        drained >= Duration::from_millis(GRACE_MS + 500),
        "the connection was released after {drained:?}, before the configured {GRACE_MS} ms grace \
         period could have elapsed"
    );
    assert!(
        drained <= Duration::from_secs(6),
        "the connection was held for {drained:?}, which is past the configured {GRACE_MS} ms grace \
         period plus the {GRACEFUL_MS} ms bound on stopping the runtime"
    );
    assert!(
        expected.starts_with(&received),
        "the bytes received before the cut are not a prefix of the stream ({} received, {} sent)",
        received.len(),
        expected.len()
    );
    assert!(
        received.len() < expected.len(),
        "the whole stream was delivered, so the shutdown never cut it short"
    );

    let exit_code = server.cancel_and_join().await;
    assert_eq!(exit_code, std::process::ExitCode::SUCCESS);
    assert!(
        cancelled_at.elapsed() <= Duration::from_secs(10),
        "`serve` did not return within the configured drain window: {:?}",
        cancelled_at.elapsed()
    );
}
