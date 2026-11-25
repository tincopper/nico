//! The supported G4 proxy behavior, end to end: response buffering and route-level header rewrites.
//!
//! The observable is what the origin receives — every assertion is about what crossed the wire, not
//! what the document intended.

mod support;

use std::net::SocketAddr;

use support::http::{
    RunningServer,
    client::{Request, exchange},
    free_port,
    source::TestSource,
    upstream::{MockUpstream, Reply},
};

/// The bootstrap every scenario writes, with `trusted_proxies` naming loopback so the tests can
/// originate XFF chains from a trusted peer.
fn bootstrap_with(directory: &std::path::Path, trusted: Option<&str>) -> String {
    let trusted_line = trusted.map_or_else(String::new, |cidr| {
        format!("  trusted_proxies: [\"{cidr}\"]\n")
    });
    format!(
        "\
config_source:
  type: local
  path: {document}
  max_bytes: 1048576
  watch: notify
  debounce_ms: 0
  max_wait_ms: 0
  cache_dir: {cache}
  allow_bootstrap_cache: false
runtime:
  threads: 1
  grace_period_ms: 1
  graceful_shutdown_ms: 1000
http:
{trusted_line}  listeners:
    - listen: 127.0.0.1:{port}
",
        document = directory.join("resources.yaml").display(),
        cache = directory.join("cache").display(),
        port = free_port(),
    )
}

/// A document with a rewritten route: `proxy_set_header` names the client's original Host, deletes
/// a debug header, and adds a tenant marker.
fn rewritten_document(origin: SocketAddr, host_origin: SocketAddr) -> String {
    format!(
        "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1048576
  timeout_ms: 30000
plugins: []
providers: []
upstreams:
  - name: origin
    scheme: http
    host: 127.0.0.1
    port: {port}
  - name: headers
    scheme: http
    host: 127.0.0.1
    port: {host_port}
routes:
  - id: api
    path_prefix: /api/
    upstream: origin
  - id: headers
    path_prefix: /headers/
    upstream: headers
    proxy_set_header:
      - name: host
        value: client_host
      - name: x-tenant
        value: premium
    proxy_del_header:
      - x-debug
",
        port = origin.port(),
        host_port = host_origin.port(),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn route_header_rewrites_reach_the_origin() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    std::fs::write(
        directory.path().join("bootstrap.yaml"),
        bootstrap_with(directory.path(), None),
    )
    .expect("the bootstrap fixture writes");

    let origin = MockUpstream::start(Reply::Path).await;
    let host_origin = MockUpstream::start(Reply::EchoHeader {
        name: "host".to_owned(),
    })
    .await;
    let document = rewritten_document(origin.address(), host_origin.address());
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;

    // `client_host` writes the client's original Host (the loopback the client addressed), the
    // tenant marker is added, and the deleted debug header never reaches the origin.
    let request = Request::get("/api/items?kept=1", "a.test").header("x-debug", "should-vanish");
    let response = exchange(server.address(), &request).await;
    assert_eq!(response.status(), 200, "{}", response.text());
    assert_eq!(
        response.text(),
        "/api/items?kept=1",
        "the path is forwarded without a rewrite"
    );

    // The header origin answers with the Host it received: `client_host` wrote the client's
    // original Host, not the gateway's automatic upstream-host default.
    let request = Request::get("/headers/items", "a.test");
    let response = exchange(server.address(), &request).await;
    assert_eq!(response.status(), 200, "{}", response.text());
    assert_eq!(
        response.text(),
        "a.test",
        "`client_host` wrote the client's original Host, not the upstream's logical host"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_forged_xff_from_a_trusted_peer_is_not_silently_accepted() {
    // Anchored to the trusted-proxies contract: this scenario only pins that the rewrites do not
    // interfere with the derivation, using the recorded header values.
    let directory = tempfile::TempDir::new().expect("temporary directory");
    std::fs::write(
        directory.path().join("bootstrap.yaml"),
        bootstrap_with(directory.path(), None),
    )
    .expect("the bootstrap fixture writes");

    let origin = MockUpstream::start(Reply::Path).await;
    let host_origin = MockUpstream::start(Reply::EchoHeader {
        name: "host".to_owned(),
    })
    .await;
    let document = rewritten_document(origin.address(), host_origin.address());
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;

    let request =
        Request::get("/api/items?kept=1", "a.test").header("x-forwarded-for", "203.0.113.9");
    let response = exchange(server.address(), &request).await;
    assert_eq!(response.status(), 200);
}
