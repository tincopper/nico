//! Resource limits at the edge of the data plane: body bounds and the whole-request deadline.
//!
//! The two body checks are different code paths and both matter:
//!
//! - a declared `Content-Length` above the route's bound is rejected **before** an upstream is
//!   contacted, because the answer is known from the head alone;
//! - a chunked body has no declared length, so the bound is enforced while the bytes stream through
//!   and the request is aborted mid-body. The upstream sees a partial body, which is the evidence
//!   that the abort happened during the transfer rather than before it.
//!
//! The deadline is the third resource: an upstream that never answers must produce `504`, not `502`
//! and not a hung client. Once a response head has been committed there is nowhere left to write a
//! status, so the same timeout can only end the stream — and must be logged as a cancellation
//! instead of being reported as a server failure or as a second response.

mod support;

use std::time::{Duration, Instant};

use support::http::{
    Resources, Route, RunningServer,
    client::{Client, READ_BUDGET, Request, exchange, get},
    free_port, log,
    source::TestSource,
    upstream::{MockUpstream, Reply},
    write_bootstrap,
};

/// The route-level bound the two body scenarios must enforce.
const ROUTE_LIMIT: usize = 1024;

/// How long a fixture may take to reach a state it was told to reach.
const BUDGET: Duration = Duration::from_secs(20);

#[tokio::test(flavor = "current_thread")]
async fn bodies_over_the_route_bound_are_rejected_and_a_silent_upstream_times_out() {
    // Installed before the server starts: the committed-response assertions are about the log line.
    let captured = log::capture();

    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    write_bootstrap(directory.path(), port, false, 1, 1000);

    // Answers successfully; the `capped` scenario must never reach it.
    let capped = MockUpstream::start(Reply::Body {
        body: b"capped".to_vec(),
        chunk: usize::MAX,
    })
    .await;
    // Echoes what it receives, which is how the partial body of an aborted upload becomes visible.
    let chunked = MockUpstream::start(Reply::Echo).await;
    // Accepts the request and answers nothing at all.
    let silent = MockUpstream::start(Reply::StallBeforeHead).await;
    // Answers the head and one chunk, then goes quiet: the response is already committed.
    let midstream = MockUpstream::start(Reply::StallMidStream {
        first_chunk: b"first-event".to_vec(),
    })
    .await;
    // Answers the head and never a byte of body, which is the same situation one step earlier.
    let stalled = MockUpstream::start(Reply::StallAfterHead).await;
    // Accepts a partial upload and waits for the rest, which the client never sends.
    let slow = MockUpstream::start(Reply::Echo).await;

    let document = Resources::new()
        .body_limit(4096)
        .upstream("capped", capped.address())
        .upstream("chunked", chunked.address())
        .upstream("silent", silent.address())
        .upstream("midstream", midstream.address())
        .upstream("stalled", stalled.address())
        .upstream("slow", slow.address())
        .route(Route::new("capped", "/capped", "capped").max_body_bytes(ROUTE_LIMIT as u32))
        .route(Route::new("chunked", "/chunked", "chunked").max_body_bytes(ROUTE_LIMIT as u32))
        .route(Route::new("silent", "/silent", "silent").timeout_ms(200))
        .route(Route::new("midstream", "/midstream", "midstream").timeout_ms(200))
        .route(Route::new("stalled", "/stalled", "stalled").timeout_ms(200))
        .route(Route::new("slow-upload", "/slow-upload", "slow").timeout_ms(200))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;
    let address = server.address();

    // --- a declared length over the bound ----------------------------------------------------
    let request = Request::post("/capped", "limits.test").body(vec![b'x'; 2048], 1024);
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        413,
        "a declared body over the route bound must be 413, not {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(response.json_code().as_deref(), Some("body_too_large"));
    assert_eq!(
        capped.observation().connections,
        0,
        "the length was declared in the head, so no upstream may be contacted"
    );

    // --- a chunked body that crosses the bound while streaming ------------------------------
    let oversized = vec![b'y'; 2048];
    let mut client = Client::connect(address).await;
    let request = Request::post("/chunked", "limits.test").chunked(oversized.clone(), 256);
    client
        .write_head(&request)
        .await
        .expect("the request head is written");
    // The proxy aborts the body as soon as the bound is crossed, so the client may see its write
    // fail; that is the abort arriving, not a fixture problem.
    for piece in oversized.chunks(256) {
        if client.write_chunk(piece).await.is_err() {
            break;
        }
    }
    let head = client.read_head(READ_BUDGET).await;
    assert_eq!(
        head.status(),
        413,
        "a chunked body over the bound must be 413, not {}; headers: {:?}",
        head.status(),
        head.headers().collect::<Vec<_>>()
    );
    let body = client.read_body(&head, READ_BUDGET).await;
    let response = String::from_utf8_lossy(&body);
    assert!(
        response.contains("body_too_large"),
        "the abort must carry the stable code, got {response:?}"
    );
    let observed = chunked.wait_for_request(BUDGET).await;
    assert!(
        observed.body_len() < oversized.len(),
        "the upstream received {} of {} bytes, so the abort waited for the whole body",
        observed.body_len(),
        oversized.len()
    );

    // --- an upstream that never answers ------------------------------------------------------
    let started = Instant::now();
    let response = get(address, "/silent", "limits.test").await;
    let elapsed = started.elapsed();
    assert_eq!(
        response.status(),
        504,
        "an upstream that never answers must time out as 504, not {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(response.json_code().as_deref(), Some("upstream_timeout"));
    assert!(
        elapsed >= Duration::from_millis(150),
        "the answer arrived after {elapsed:?}, before the route's 200 ms budget could expire"
    );

    // --- a committed response is never replaced ----------------------------------------------
    //
    // Pingora writes the upstream response head into the downstream session as soon as it arrives,
    // so from that moment on there is no place for a 504. The client sees the head it already had
    // and a stream that ends early, and the log records the cause as a cancellation rather than
    // paging an operator for a response that was delivered.
    let response = exchange(
        address,
        &Request::get("/midstream", "limits.test").header("x-request-id", "committed-midstream"),
    )
    .await;
    assert_eq!(
        response.status(),
        200,
        "the upstream answered before the deadline, so the client must see its head"
    );
    assert_eq!(
        response.body(),
        b"first-event",
        "the bytes received before the stall must be delivered unchanged"
    );
    let line = log::wait_for_request_log(&captured, "committed-midstream", BUDGET).await;
    assert!(
        line.contains("error.category=Cancelled"),
        "a stream cut after the response was committed is a cancellation, got: {line}"
    );
    assert!(
        !line.contains("http.status=5"),
        "a committed response must never be reported as a server failure, got: {line}"
    );

    // The same rule one step earlier: the head arrived, no body byte did.
    let response = exchange(
        address,
        &Request::get("/stalled", "limits.test").header("x-request-id", "committed-head"),
    )
    .await;
    assert_eq!(
        response.status(),
        200,
        "the head was committed before the deadline expired"
    );
    assert!(
        response.body().is_empty(),
        "the upstream sent no body, so the client must receive none"
    );
    let line = log::wait_for_request_log(&captured, "committed-head", BUDGET).await;
    assert!(
        line.contains("error.category=Cancelled"),
        "a committed response that timed out is a cancellation, got: {line}"
    );
    assert!(
        !line.contains("http.status=5"),
        "a committed response must never be reported as a server failure, got: {line}"
    );

    // --- a client that trickles its body -------------------------------------------------------
    //
    // Pingora bounds neither the time nor the rate at which a client sends a body, so only the
    // deadline check in the body meter ends this request — and because it runs while the body is
    // still incomplete, it knows the caller is the slow party: `408`, not a `504` that would send
    // an operator to inspect a healthy upstream.
    let mut slow_upload = Client::connect(address).await;
    let request = Request::post("/slow-upload", "limits.test")
        .header("x-request-id", "stalled-upload")
        .chunked(vec![b'z'; 2048], 2048);
    slow_upload
        .write_head(&request)
        .await
        .expect("the request head is written");
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(2) {
        // One byte every 25 ms: an upload that never completes but never stalls long enough for
        // the peer's read timeout to be the thing that fires.
        if slow_upload.write_chunk(b"z").await.is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let head = slow_upload.read_head(READ_BUDGET).await;
    assert_eq!(
        head.status(),
        408,
        "a caller that never finishes sending gets 408, not {}; headers: {:?}",
        head.status(),
        head.headers().collect::<Vec<_>>()
    );
    let body = slow_upload.read_body(&head, READ_BUDGET).await;
    let answer = String::from_utf8_lossy(&body);
    assert!(
        answer.contains("request_timeout"),
        "the answer must carry the stable code, got {answer:?}"
    );
    let line = log::wait_for_request_log(&captured, "stalled-upload", BUDGET).await;
    assert!(
        line.contains("error.category=Client") && line.contains("http.status=408"),
        "a slow caller is a client fault, not an upstream one, got: {line}"
    );
    assert!(
        !line.contains("http.status=5"),
        "a slow caller must never be reported as a server failure, got: {line}"
    );

    assert_eq!(
        server.cancel_and_join().await,
        std::process::ExitCode::SUCCESS
    );
}
