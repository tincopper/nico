//! Inbound TLS: a real client verifies the gateway's listener, and the HTTP data plane runs
//! underneath the encrypted transport unchanged.
//!
//! The certificate is the repository's `localhost` fixture, signed by `test-root`; the client trusts
//! that root and addresses the listener as `localhost`. The upstream is the ordinary plaintext HTTP
//! mock: TLS is only the downstream transport, and the route/plugin/upstream path is the existing one.
#![expect(
    clippy::expect_used,
    reason = "a fixture that cannot build its TLS exchange has nothing useful to return"
)]

mod support;

use std::time::Duration;

use support::{
    http::{
        RunningServer, free_port,
        resources::{Resources, Route},
        source::TestSource,
        write_bootstrap_tls,
    },
    inbound_tls::TlsClient,
    tls::fixture,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

/// How long a fixture may take to reach a state it was told to reach.
const BUDGET: Duration = Duration::from_secs(20);

/// How long a plaintext probe may wait for the TLS listener to close.
const PLAINTEXT_BUDGET: Duration = Duration::from_secs(2);

/// Returns the body between the response head and the end of the bytes.
fn body(response: &[u8]) -> &[u8] {
    let end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("the response has a head terminator");
    &response[end + 4..]
}

#[tokio::test(flavor = "current_thread")]
async fn a_verified_tls_client_reaches_the_http_pipeline_and_plaintext_cannot() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    let certificate = fixture("localhost.pem");
    let key = fixture("localhost.key");
    write_bootstrap_tls(directory.path(), port, &certificate, &key);

    let upstream =
        support::http::upstream::MockUpstream::start(support::http::upstream::Reply::Body {
            body: b"api".to_vec(),
            chunk: usize::MAX,
        })
        .await;
    let document = Resources::new()
        .upstream("api", upstream.address())
        .route(Route::new("api", "/api/", "api"))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;
    let address = server.address();
    let roots = fixture("test-root.pem");

    // --- a verified client reaches the existing route and reports https upstream ---------------
    let mut client = TlsClient::connect(address, "localhost", &roots, &[&b"http/1.1"[..]])
        .await
        .expect("the fixture root verifies the listener certificate");
    assert_eq!(
        client.alpn(),
        Some(&b"http/1.1"[..]),
        "the gateway offers HTTP/2 and HTTP/1.1; a client that only offers h1 gets h1"
    );
    let response = client
        .get("/api/items", "api.test")
        .await
        .expect("the TLS connection carries the request");
    assert!(
        response.starts_with(b"HTTP/1.1 200"),
        "the route answered over TLS: {:?}",
        String::from_utf8_lossy(&response[..response.len().min(200)])
    );
    assert_eq!(
        body(&response),
        b"api",
        "the upstream body arrives unchanged"
    );

    let observed = upstream.wait_for_request(BUDGET).await;
    assert_eq!(
        observed.header("x-forwarded-proto"),
        Some("https"),
        "the data plane states the transport it actually terminated"
    );
    assert!(
        observed.body().is_empty(),
        "a GET has no body; the assertion above is about the forwarded scheme"
    );

    // --- ALPN h2 is advertised by the listener ------------------------------------------------
    let h2 = TlsClient::connect(address, "localhost", &roots, &[&b"h2"[..]])
        .await
        .expect("the listener completes an h2-only handshake");
    assert_eq!(
        h2.alpn(),
        Some(&b"h2"[..]),
        "TLS settings must advertise HTTP/2 over ALPN"
    );
    drop(h2);

    // --- a root that did not sign this certificate is refused at the handshake ---------------
    let refused = TlsClient::connect(
        address,
        "localhost",
        &fixture("other-root.pem"),
        &[&b"http/1.1"[..]],
    )
    .await;
    assert!(
        refused.is_err(),
        "a certificate signed by another root must not verify"
    );

    // --- a plaintext client gets no HTTP response from a TLS listener ------------------------
    let mut plaintext = TcpStream::connect(address)
        .await
        .expect("the TLS listener accepts the TCP connection");
    plaintext
        .write_all(b"GET /api/items HTTP/1.1\r\nHost: api.test\r\nConnection: close\r\n\r\n")
        .await
        .expect("the plaintext probe writes");
    let mut received = Vec::new();
    let _ = timeout(PLAINTEXT_BUDGET, plaintext.read_to_end(&mut received)).await;
    assert!(
        !received.starts_with(b"HTTP/1.1"),
        "a plaintext request must not be parsed as HTTP: {:?}",
        String::from_utf8_lossy(&received)
    );

    // A plaintext request to the TLS listener must not be forwarded, either.
    let after = upstream.observation();
    assert_eq!(
        after.requests.len(),
        1,
        "only the verified TLS request may reach the upstream: {}",
        after.summary()
    );

    // The ordinary plaintext exchange helper is deliberately not used here; one TLS route and one
    // plaintext rejection prove the transport boundary. A plaintext route in another test target
    // keeps the no-TLS path covered.

    let _code = server.cancel_and_join().await;
}
