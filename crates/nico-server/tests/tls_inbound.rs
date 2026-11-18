//! Inbound TLS: a real client verifies the gateway's listener, and the HTTP data plane runs
//! underneath the encrypted transport unchanged.
//!
//! The certificate is the repository's `localhost` fixture, signed by `test-root`; the client trusts
//! that root and addresses the listener as `localhost`. The upstream is the ordinary plaintext HTTP
//! mock: TLS is only the downstream transport, and the route/plugin/upstream path is the existing one.
//!
//! Two shapes are asserted, each with its own server: one TLS-only listener (the identity, the ALPN
//! offer, and the refusal of a plaintext client), and two listeners in one section — plaintext next
//! to TLS — where the transport fact is decided per connection rather than per process.
#![expect(
    clippy::expect_used,
    reason = "a fixture that cannot build its TLS exchange has nothing useful to return"
)]

mod support;

use std::{
    net::SocketAddr,
    path::Path,
    time::{Duration, Instant},
};

use support::{
    http::{
        RunningServer,
        client::{Request, exchange},
        free_port,
        resources::{Resources, Route},
        source::TestSource,
        tls_bootstrap::{write_bootstrap_dual, write_bootstrap_tls},
        upstream::{MockUpstream, Reply},
        wait_for_status,
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

/// Interval between two readiness probes.
const PROBE: Duration = Duration::from_millis(10);

/// A path no fixture route matches, so the answer states only whether a generation is in service.
const UNROUTED: &str = "/probe";

/// Returns the body between the response head and the end of the bytes.
fn body(response: &[u8]) -> &[u8] {
    let end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("the response has a head terminator");
    &response[end + 4..]
}

/// Waits until a generation is in service, observed through the TLS listener itself.
///
/// The harness's own readiness wait reads a process-wide log, and this binary starts one server per
/// scenario: a publication line cannot be attributed to either of them. The fact that belongs to the
/// server this scenario started is its own answer — an unrouted path is `404` once a snapshot exists
/// and `503` before one, and it never touches the upstream.
async fn wait_until_published_over_tls(address: SocketAddr, roots: &Path) {
    let deadline = Instant::now() + BUDGET;
    loop {
        if let Ok(mut client) =
            TlsClient::connect(address, "localhost", roots, &[&b"http/1.1"[..]]).await
            && let Ok(response) = client.get(UNROUTED, "api.test").await
            && response.starts_with(b"HTTP/1.1 404")
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the TLS listener never published a generation within {BUDGET:?}"
        );
        tokio::time::sleep(PROBE).await;
    }
}

/// Reads everything a plaintext connection returns, or what it returned before the budget expired.
async fn plaintext_probe(address: SocketAddr) -> Vec<u8> {
    let mut plaintext = TcpStream::connect(address)
        .await
        .expect("the listener accepts the TCP connection");
    plaintext
        .write_all(b"GET /api/items HTTP/1.1\r\nHost: api.test\r\nConnection: close\r\n\r\n")
        .await
        .expect("the plaintext probe writes");
    let mut received = Vec::new();
    let _ = timeout(PLAINTEXT_BUDGET, plaintext.read_to_end(&mut received)).await;
    received
}

#[tokio::test(flavor = "current_thread")]
async fn a_verified_tls_client_reaches_the_http_pipeline_and_plaintext_cannot() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    let certificate = fixture("localhost.pem");
    let key = fixture("localhost.key");
    write_bootstrap_tls(directory.path(), port, &certificate, &key);

    let upstream = MockUpstream::start(Reply::Body {
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
    wait_until_published_over_tls(address, &roots).await;

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
    let received = plaintext_probe(address).await;
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
    // plaintext rejection prove the transport boundary. The scenario below covers the plaintext
    // listener's own path.

    let _code = server.cancel_and_join().await;
}

/// One section, two listeners: the plaintext one serves HTTP/1.1, the TLS one terminates the
/// handshake, and the scheme each request reports upstream is the transport of *its own* connection.
///
/// The `h2c: true` in this fixture is part of the subject rather than an extra knob. The previous
/// schema refused `http.h2c` together with an inbound TLS identity because one listener had one
/// transport; a listener list has a transport per entry, so the flag applies to the plaintext
/// entries and coexists with a TLS entry — and this process starting *is* that assertion.
#[tokio::test(flavor = "current_thread")]
async fn plaintext_and_tls_listeners_serve_side_by_side() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let plain_port = free_port();
    // Two ephemeral binds can hand out the same port; the section rejects a repeat, so re-draw.
    let mut tls_port = free_port();
    while tls_port == plain_port {
        tls_port = free_port();
    }
    let certificate = fixture("localhost.pem");
    let key = fixture("localhost.key");
    write_bootstrap_dual(
        directory.path(),
        plain_port,
        tls_port,
        &certificate,
        &key,
        true,
    );

    let upstream = MockUpstream::start(Reply::Body {
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
    let plain_address = server.address();
    assert_eq!(
        plain_address.port(),
        plain_port,
        "the plaintext entry is the first listener, so it is the address the harness talks to"
    );
    let tls_address: SocketAddr = format!("127.0.0.1:{tls_port}")
        .parse()
        .expect("the TLS listener is an ip:port address");
    let roots = fixture("test-root.pem");

    // A generation reached service: an unrouted path is 404 once one exists, 503 before.
    wait_for_status(plain_address, &Request::get(UNROUTED, "api.test"), 404).await;

    // --- the plaintext listener serves the route and reports `http` --------------------------
    let response = exchange(plain_address, &Request::get("/api/items", "api.test")).await;
    assert_eq!(
        response.status(),
        200,
        "the plaintext listener serves the route: {}",
        response.text()
    );
    assert_eq!(
        response.text(),
        "api",
        "the upstream body arrives unchanged"
    );

    // --- the TLS listener serves the same route and reports `https` --------------------------
    let mut client = TlsClient::connect(tls_address, "localhost", &roots, &[&b"http/1.1"[..]])
        .await
        .expect("the fixture root verifies the second listener");
    assert_eq!(
        client.alpn(),
        Some(&b"http/1.1"[..]),
        "the TLS entry advertises HTTP/2 and HTTP/1.1 exactly like a lone TLS listener"
    );
    let response = client
        .get("/api/items", "api.test")
        .await
        .expect("the TLS connection carries the request");
    assert!(
        response.starts_with(b"HTTP/1.1 200"),
        "the same route answers on the second listener: {:?}",
        String::from_utf8_lossy(&response[..response.len().min(200)])
    );
    assert_eq!(
        body(&response),
        b"api",
        "the upstream body arrives unchanged"
    );

    // Both requests reached the origin, and each states the transport of the connection it came in
    // on: the decision is per connection, not a process-wide flag — which is what makes two
    // listeners with two transports possible at all.
    upstream.wait_for_request(BUDGET).await;
    let observed = upstream.observation();
    assert_eq!(
        observed.requests.len(),
        2,
        "one request per listener: {}",
        observed.summary()
    );
    assert_eq!(
        observed.requests[0].header("x-forwarded-proto"),
        Some("http"),
        "the plaintext listener's request reports plaintext"
    );
    assert_eq!(
        observed.requests[1].header("x-forwarded-proto"),
        Some("https"),
        "the TLS listener's request reports TLS"
    );

    // --- neither listener accepts the other listener's transport ----------------------------
    let received = plaintext_probe(tls_address).await;
    assert!(
        !received.starts_with(b"HTTP/1.1"),
        "plaintext on the TLS listener must not be parsed as HTTP: {:?}",
        String::from_utf8_lossy(&received)
    );
    let refused = TlsClient::connect(plain_address, "localhost", &roots, &[&b"http/1.1"[..]]).await;
    assert!(
        refused.is_err(),
        "a TLS handshake must not complete on the plaintext listener"
    );
    assert_eq!(
        upstream.observation().requests.len(),
        2,
        "a cross-transport probe must not reach the origin: {}",
        upstream.observation().summary()
    );

    // --- a client that already stated the scheme keeps it -----------------------------------
    // The proxy owns the scheme of a hop it terminated; a value the caller supplied is not
    // appended to (that would produce a list nothing parses) and not replaced (that would rewrite a
    // caller's own header). Previously this rule could not be asserted end to end: with one
    // listener's transport fixed for the process, a client's `https` on a plaintext hop would have
    // been indistinguishable from an invented value.
    let request = Request::get("/api/items", "api.test").header("x-forwarded-proto", "https");
    let response = exchange(plain_address, &request).await;
    assert_eq!(
        response.status(),
        200,
        "the route still serves a request that states its own scheme: {}",
        response.text()
    );
    let observed = upstream.wait_for_request(BUDGET).await;
    assert_eq!(
        observed.header("x-forwarded-proto"),
        Some("https"),
        "the caller's value survives a plaintext hop"
    );
    let head = String::from_utf8_lossy(observed.head()).to_ascii_lowercase();
    assert_eq!(
        head.matches("x-forwarded-proto").count(),
        1,
        "the data plane must not add a second `x-forwarded-proto`: {head}"
    );

    let _code = server.cancel_and_join().await;
}
