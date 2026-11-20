//! The self-reporting surface behind TLS: the same endpoints, an encrypted transport.
//!
//! The certificate is the repository's `localhost` fixture, signed by `test-root`, and the client
//! trusts that root and addresses the listener as `localhost`. One process serves the data plane in
//! plaintext and the self-reporting surface over TLS, which is the shape a deployment uses when an
//! external probe or exporter must not speak cleartext to the process.
//!
//! Three facts belong to the transport and are asserted here rather than in the unit tests:
//!
//! - a verified client reaches `/metrics` and `/health/ready`, with the answers the plaintext
//!   surface gives;
//! - the listener declares `http/1.1` only, so a client offering `h2` as well is answered with
//!   `http/1.1` — the self-reporting surface has no second protocol path;
//! - a plaintext request to the same port produces no HTTP response: the refusal happens in the
//!   handshake, not in a status code.
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
        RunningServer, free_port, resources::Resources, source::TestSource,
        tls_bootstrap::write_bootstrap_metrics_tls,
    },
    inbound_tls::TlsClient,
    tls::fixture,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

/// How long the surface may take to accept its first handshake.
const BUDGET: Duration = Duration::from_secs(20);

/// How long a plaintext probe may wait for the TLS listener to close.
const PLAINTEXT_BUDGET: Duration = Duration::from_secs(2);

/// Interval between two handshake attempts.
const PROBE: Duration = Duration::from_millis(10);

/// Returns the lowercased head of one raw response.
///
/// Header names and the `content-type` value are compared case-insensitively, so the assertion does
/// not depend on how the capability layer spells them.
fn head(response: &[u8]) -> String {
    let end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("the response has a head terminator");
    String::from_utf8_lossy(&response[..end]).to_ascii_lowercase()
}

/// Returns the body between the response head and the end of the bytes.
fn body(response: &[u8]) -> &[u8] {
    let end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("the response has a head terminator");
    &response[end + 4..]
}

/// Connects a verified TLS client, retrying until the self-reporting listener accepts one.
///
/// The harness returns once a generation reached service, which is later than the bind; the surface
/// has no readiness signal of its own, so its first completed handshake is what says it is up.
async fn connect(address: SocketAddr, roots: &Path, alpn: &[&[u8]]) -> TlsClient {
    let deadline = Instant::now() + BUDGET;
    loop {
        match TlsClient::connect(address, "localhost", roots, alpn).await {
            Ok(client) => return client,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "the TLS metrics listener never completed a handshake on {address} within \
                     {BUDGET:?}: {error}"
                );
                tokio::time::sleep(PROBE).await;
            }
        }
    }
}

/// Reads everything a plaintext connection returns, or what it returned before the budget expired.
async fn plaintext_probe(address: SocketAddr) -> Vec<u8> {
    let mut plaintext = TcpStream::connect(address)
        .await
        .expect("the listener accepts the TCP connection");
    plaintext
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: metrics.test\r\nConnection: close\r\n\r\n")
        .await
        .expect("the plaintext probe writes");
    let mut received = Vec::new();
    let _ = timeout(PLAINTEXT_BUDGET, plaintext.read_to_end(&mut received)).await;
    received
}

#[tokio::test(flavor = "current_thread")]
async fn tls_terminates_the_self_reporting_surface_and_plaintext_cannot_reach_it() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    // Two ephemeral binds can hand out the same port; the section rejects a repeat, so re-draw.
    let mut metrics_port = free_port();
    while metrics_port == port {
        metrics_port = free_port();
    }
    let certificate = fixture("localhost.pem");
    let key = fixture("localhost.key");
    write_bootstrap_metrics_tls(directory.path(), port, metrics_port, &certificate, &key);

    let source = TestSource::with_document(&Resources::new().render());
    let server = RunningServer::start(directory.path(), source).await;
    let address = server
        .metrics_address()
        .expect("the fixture declared a metrics section");
    assert_eq!(
        address.port(),
        metrics_port,
        "the surface listens on the address the bootstrap named"
    );
    let roots = fixture("test-root.pem");

    // --- a verified client scrapes the surface over TLS --------------------------------------
    let mut client = connect(address, &roots, &[&b"http/1.1"[..]]).await;
    let response = client
        .get("/metrics", "metrics.test")
        .await
        .expect("the TLS connection carries the scrape");
    assert!(
        response.starts_with(b"HTTP/1.1 200"),
        "the scrape is answered over TLS: {:?}",
        String::from_utf8_lossy(&response[..response.len().min(200)])
    );
    let head = head(&response);
    assert!(
        head.contains("content-type: text/plain"),
        "the encrypted answer is the same Prometheus document the plaintext surface renders: {head}"
    );
    assert!(
        !body(&response).is_empty(),
        "a served process renders at least its own metric families"
    );
    drop(client);

    // --- readiness is answered over TLS, from the same projection as over plaintext -----------
    let mut client = connect(address, &roots, &[&b"http/1.1"[..]]).await;
    let response = client
        .get("/health/ready", "metrics.test")
        .await
        .expect("the TLS connection carries the readiness probe");
    assert!(
        response.starts_with(b"HTTP/1.1 200"),
        "a process with a published generation is ready: {:?}",
        String::from_utf8_lossy(&response[..response.len().min(200)])
    );
    let readiness = String::from_utf8_lossy(body(&response)).into_owned();
    assert!(
        readiness.contains("\"ready\":true"),
        "the readiness body is the same JSON document the plaintext surface answers: {readiness}"
    );
    drop(client);

    // --- ALPN declares HTTP/1.1 and nothing else ---------------------------------------------
    // The client offers both on purpose. A listener that inherited the data plane's offer would
    // answer `h2` here, and one that declared no ALPN at all would negotiate nothing.
    let h2_capable = connect(address, &roots, &[&b"h2"[..], &b"http/1.1"[..]]).await;
    assert_eq!(
        h2_capable.alpn(),
        Some(&b"http/1.1"[..]),
        "the self-reporting listener offers HTTP/1.1 only, so an h2-capable client is answered \
         with http/1.1"
    );
    drop(h2_capable);

    // --- a plaintext client gets no HTTP response at all -------------------------------------
    let received = plaintext_probe(address).await;
    assert!(
        !received.windows(5).any(|window| window == b"HTTP/"),
        "plaintext on the TLS surface must fail in the handshake, not be parsed as HTTP: {:?}",
        String::from_utf8_lossy(&received)
    );

    let exit = server.cancel_and_join().await;
    assert_eq!(exit, std::process::ExitCode::SUCCESS);
}
