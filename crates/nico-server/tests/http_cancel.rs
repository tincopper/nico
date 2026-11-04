//! A client that leaves mid-request: the upstream is released, and the request is logged as
//! cancelled rather than as a server failure.
//!
//! Cancellation is the outcome this data plane is most likely to get wrong, because every layer
//! reports it differently: Pingora calls it a downstream read/write error, `ErrorCategory` calls it
//! `Cancelled`, and the wire has no status code for it at all. The test therefore checks three
//! separate facts:
//!
//! 1. the upstream connection is closed, so the stalled request does not hold a peer forever;
//! 2. the request's log line carries `error.category=Cancelled` and no 5xx status;
//! 3. the next request is served normally, so the cancellation damaged nothing else.

mod support;

use std::time::Duration;

use support::http::{
    Resources, Route, RunningServer,
    client::{Client, Request, get},
    free_port, log,
    source::TestSource,
    upstream::{MockUpstream, Reply},
    write_bootstrap,
};

/// How long the cancellation must be visible on the upstream side.
const CANCEL_BUDGET: Duration = Duration::from_secs(20);

/// The correlation id of the cancelled request, so its log line can be found among the others.
const TRACE_ID: &str = "cancel-proof";

#[tokio::test(flavor = "current_thread")]
async fn a_client_that_leaves_mid_request_releases_the_upstream_and_is_logged_as_cancelled() {
    // The subscriber has to be installed before the server starts: the request log is an event on
    // Pingora's worker thread, and there is no second chance to capture it.
    let captured = log::capture();

    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    write_bootstrap(directory.path(), port, false, 1, 1000);

    // Accepts the request and never answers: the only thing that can end this connection is the
    // client leaving or a shutdown.
    let holding = MockUpstream::start(Reply::StallBeforeHead).await;
    let ping = MockUpstream::start(Reply::Body {
        body: b"pong".to_vec(),
        chunk: usize::MAX,
    })
    .await;

    let document = Resources::new()
        .upstream("holding", holding.address())
        .upstream("ping", ping.address())
        .route(Route::new("hold", "/hold", "holding"))
        .route(Route::new("ping", "/ping", "ping"))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;
    let address = server.address();

    // Wait for the head to reach the upstream before leaving, so the request is genuinely in flight
    // and the cancellation is about the client, not about a request that never started.
    let mut client = Client::connect(address).await;
    let request = Request::get("/hold", "cancel.test").header("x-request-id", TRACE_ID);
    client
        .write_head(&request)
        .await
        .expect("the request head is written");
    holding.wait_for_request(CANCEL_BUDGET).await;
    drop(client);

    let observed = holding.wait_until_closed(CANCEL_BUDGET, 1).await;
    assert_eq!(
        observed.closed,
        1,
        "the upstream connection must be closed when the client leaves, not left stalled: {}",
        observed.summary()
    );

    let line = log::wait_for_request_log(&captured, TRACE_ID, CANCEL_BUDGET).await;
    assert!(
        line.contains("error.category=Cancelled"),
        "a client disconnect must be logged as cancelled, got: {line}"
    );
    assert!(
        !line.contains("http.status=5"),
        "a client disconnect must not be reported as a server failure, got: {line}"
    );
    assert!(
        line.contains("http.method=") && line.contains("body_bytes="),
        "the record must carry the method and how many body bytes were metered, got: {line}"
    );
    assert!(
        line.contains("route_id=hold") && line.contains("upstream=holding"),
        "the cancelled request must still name its route and upstream, got: {line}"
    );
    // The reserved fields the log contract requires, on the record itself.
    assert!(
        line.contains("config.generation=1"),
        "the record must carry the snapshot generation the request was served from, got: {line}"
    );
    // `499` is the log-only status the error spec reserves for cancellation (nginx's convention):
    // nothing was written to the caller, and the record still says why.
    assert!(
        line.contains("http.status=499"),
        "a cancelled request must be logged with the log-only 499, got: {line}"
    );
    assert!(
        line.contains("elapsed_ms="),
        "the record must carry how long the request took, got: {line}"
    );

    // The server is still healthy: one cancelled request does not poison the next one.
    let response = get(address, "/ping", "cancel.test").await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.text(), "pong");

    assert_eq!(
        server.cancel_and_join().await,
        std::process::ExitCode::SUCCESS
    );
}
