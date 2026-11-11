use std::{net::SocketAddr, time::Duration};

use nico_core::{Scheme, Upstream};
use pingora::protocols::tls::ALPN;

use super::{Destination, build, host_header, peer};
use crate::test_support::document_with;

const CLEARTEXT: &str = concat!(
    "  - name: api-backend\n",
    "    scheme: http\n",
    "    host: 127.0.0.1\n",
    "    port: 8080\n",
);

const DEFAULT_HTTP_PORT: &str = concat!(
    "  - name: api-backend\n",
    "    scheme: http\n",
    "    host: api.example.com\n",
    "    port: 80\n",
);

const SECURE: &str = concat!(
    "  - name: api-secure\n",
    "    scheme: https\n",
    "    host: 127.0.0.1\n",
    "    port: 443\n",
);

const SECURE_NAMED_HOST: &str = concat!(
    "  - name: api-secure\n",
    "    scheme: https\n",
    "    host: localhost\n",
    "    port: 8443\n",
);

const BUDGET: Duration = Duration::from_millis(500);

fn upstream(upstreams: &str) -> Upstream {
    let document = document_with(upstreams, "  []");
    document.upstreams()[0].clone()
}

/// Returns the destination a document upstream describes.
fn destination(upstreams: &str) -> Destination {
    Destination::from(&upstream(upstreams))
}

/// Returns an address a peer can be built for without a resolver.
fn address(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

#[test]
fn a_document_upstream_reads_as_the_destination_it_describes() {
    assert_eq!(
        destination(CLEARTEXT),
        Destination::new(Scheme::Http, "127.0.0.1", 8080),
        "the seam takes scheme, host and port, and drops the configuration name"
    );
    assert_eq!(destination(CLEARTEXT).host(), "127.0.0.1");
    assert_eq!(destination(CLEARTEXT).port(), 8080);
    assert_eq!(destination(SECURE).scheme(), Scheme::Https);
}

#[test]
fn a_cleartext_destination_builds_a_peer_without_tls() {
    let peer = build(&destination(CLEARTEXT), address(8080), BUDGET);

    assert!(!peer.is_tls(), "scheme http must not enable TLS");
    assert_eq!(peer.sni, "127.0.0.1");
    assert!(
        peer.to_string().contains("127.0.0.1:8080"),
        "the peer must carry the upstream address: {peer}"
    );
}

#[test]
fn an_https_destination_builds_a_peer_that_speaks_tls() {
    // The backend is compiled in, so the scheme decides only *whether* to hand the connection to
    // the TLS connector. Which roots it is verified against is a service-built fact (`tls`).
    let peer = build(&destination(SECURE), address(443), BUDGET);

    assert!(peer.is_tls(), "scheme https must enable TLS");
    assert_eq!(
        peer.sni, "127.0.0.1",
        "SNI is the destination host, which is also what the certificate is verified against"
    );
}

#[test]
fn the_request_budget_bounds_both_connecting_and_reading() {
    let budget = Duration::from_millis(1500);

    let peer = build(&destination(CLEARTEXT), address(8080), budget);

    assert_eq!(peer.options.connection_timeout, Some(budget));
    assert_eq!(peer.options.read_timeout, Some(budget));
}

#[test]
fn the_default_alpn_stays_http1() {
    let peer = build(&destination(CLEARTEXT), address(8080), BUDGET);

    assert_eq!(
        peer.options.alpn,
        ALPN::H1,
        "a gRPC route must opt into H2 explicitly"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_peer_resolves_an_address_literal_without_a_lookup() {
    let peer = peer(&destination(CLEARTEXT), BUDGET)
        .await
        .expect("127.0.0.1 is an address, not a name");

    assert!(
        peer.to_string().contains("127.0.0.1:8080"),
        "the resolved address must be the destination's: {peer}"
    );
}

#[test]
fn the_host_header_keeps_a_non_default_port() {
    assert_eq!(host_header(&destination(CLEARTEXT)), "127.0.0.1:8080");
}

#[test]
fn the_host_header_omits_the_default_port_of_its_scheme() {
    assert_eq!(
        host_header(&destination(DEFAULT_HTTP_PORT)),
        "api.example.com"
    );
    assert_eq!(host_header(&destination(SECURE)), "127.0.0.1");
}

#[test]
fn the_host_header_keeps_a_non_default_https_port() {
    assert_eq!(
        host_header(&destination(SECURE_NAMED_HOST)),
        "localhost:8443"
    );
}
