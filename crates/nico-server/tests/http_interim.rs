//! `Expect: 100-continue` through a real proxy: the interim response is forwarded verbatim.
//!
//! Pingora 0.9.1 delivers an informational response through the same response hooks as a final one
//! (phase-0 evidence), so a proxy that adds a header there corrupts the handshake: the client is
//! waiting for permission to send its body, and a `100` carrying `content-length` or `date` is not
//! the response the origin sent. This scenario therefore asserts two separate facts:
//!
//! 1. the `100` the client receives carries exactly the headers the origin sent — nothing added;
//! 2. the final response that follows it is intact, so the interim response did not consume the
//!    request or the connection.

mod support;

use support::http::{
    Resources, Route, RunningServer,
    client::{Client, READ_BUDGET, Request},
    free_port,
    source::TestSource,
    upstream::{MockUpstream, Reply},
    write_bootstrap,
};

/// The body the client sends only after the `100` arrives.
const PAYLOAD: &[u8] = b"hello-body";

#[tokio::test(flavor = "current_thread")]
async fn an_interim_response_is_forwarded_without_added_headers() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    write_bootstrap(directory.path(), port, false, 1, 1000);

    let upstream = MockUpstream::start(Reply::ContinueThen {
        body: b"final-body".to_vec(),
    })
    .await;

    let document = Resources::new()
        .upstream("expect", upstream.address())
        .route(Route::new("expect", "/expect", "expect"))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;

    let mut client = Client::connect(server.address()).await;
    let request = Request::post("/expect", "interim.test")
        .header("expect", "100-continue")
        .body(PAYLOAD.to_vec(), 1);
    client
        .write_head(&request)
        .await
        .expect("the request head is written");

    // The proxy must forward the origin's `100` before the client sends a body byte.
    let interim = client.read_head(READ_BUDGET).await;
    assert_eq!(
        interim.status(),
        100,
        "the origin answered 100 Continue, so the client must see it; headers: {:?}",
        interim.headers().collect::<Vec<_>>()
    );
    let headers: Vec<(String, String)> = interim
        .headers()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    assert_eq!(
        headers,
        vec![("x-mock-interim".to_owned(), "continue".to_owned())],
        "the interim response must carry exactly the origin's headers, with nothing added"
    );

    // Only now does the body follow, which is what `Expect: 100-continue` is for. The head already
    // declared the length, so the bytes are written without any framing of their own.
    client
        .write_bytes(PAYLOAD)
        .await
        .expect("the body is written after the interim response");

    let response_head = client.read_head(READ_BUDGET).await;
    assert_eq!(
        response_head.status(),
        200,
        "the final response must follow the interim one"
    );
    let body = client.read_body(&response_head, READ_BUDGET).await;
    assert_eq!(
        body, b"final-body",
        "the final response body changed while passing through"
    );

    // The origin saw the expectation it answered, and the body it received intact.
    let observed = upstream
        .wait_for_request(std::time::Duration::from_secs(20))
        .await;
    assert_eq!(
        observed.header("expect"),
        Some("100-continue"),
        "the expectation must reach the origin, or it cannot answer it"
    );
    assert_eq!(
        observed.body(),
        PAYLOAD,
        "the body sent after the interim response must reach the origin unchanged"
    );

    assert_eq!(
        server.cancel_and_join().await,
        std::process::ExitCode::SUCCESS
    );
}
