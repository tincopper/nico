use std::{net::SocketAddr, time::Duration};

use nico_core::Scheme;
use pingora::protocols::tls::ALPN;

use super::{Destination, build, host_header, peer};

const BUDGET: Duration = Duration::from_millis(500);

fn cleartext() -> Destination {
    Destination::new(Scheme::Http, "127.0.0.1", 8080)
}

fn default_http_port() -> Destination {
    Destination::new(Scheme::Http, "api.example.com", 80)
}

fn secure() -> Destination {
    Destination::new(Scheme::Https, "127.0.0.1", 443)
}

fn secure_named_host() -> Destination {
    Destination::new(Scheme::Https, "localhost", 8443)
}

/// A destination whose connection target and authority differ — the shape a member of a multi-address
/// upstream produces.
fn member() -> Destination {
    Destination::new(Scheme::Https, "10.0.0.7", 8443).with_authority("api.example.test")
}

/// Returns an address a peer can be built for without a resolver.
fn address(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

#[test]
fn a_destination_names_its_connection_target_until_it_is_given_an_authority() {
    assert_eq!(
        cleartext().authority(),
        "127.0.0.1",
        "a target owner that names one address has one name for it"
    );
    assert_eq!(
        member().host(),
        "10.0.0.7",
        "the connection target is what is dialled"
    );
    assert_eq!(
        member().authority(),
        "api.example.test",
        "the authority is what the peer is addressed by"
    );
    assert_eq!(
        member().port(),
        8443,
        "the port belongs to the connection, not to the name"
    );
}

#[test]
fn a_cleartext_destination_builds_a_peer_without_tls() {
    let peer = build(&cleartext(), address(8080), BUDGET, Some(BUDGET));

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
    let peer = build(&secure(), address(443), BUDGET, Some(BUDGET));

    assert!(peer.is_tls(), "scheme https must enable TLS");
    assert_eq!(
        peer.sni, "127.0.0.1",
        "SNI is the destination's authority, which is also what the certificate is verified against"
    );
}

#[test]
fn a_member_presents_its_authority_as_the_server_name() {
    let peer = build(&member(), address(8443), BUDGET, Some(BUDGET));

    assert_eq!(
        peer.sni, "api.example.test",
        "a certificate names the service, not the machine that serves it"
    );
    assert!(
        peer.to_string().contains("127.0.0.1:8443"),
        "the resolved address is the member's: {peer}"
    );
}

#[test]
fn the_request_budget_bounds_both_connecting_and_reading() {
    let budget = Duration::from_millis(1500);

    let peer = build(&cleartext(), address(8080), budget, Some(budget));

    assert_eq!(peer.options.connection_timeout, Some(budget));
    assert_eq!(peer.options.read_timeout, Some(budget));
}

#[test]
fn a_first_byte_budget_bounds_the_read_without_bounding_the_connect() {
    // The two budgets answer different questions: connecting is how long the socket may take to open,
    // reading is how long one read may wait. A route that states a first-byte budget narrows only the
    // second — a peer that cannot be reached in the request budget is still a connect failure.
    let budget = Duration::from_secs(30);
    let ttft = Duration::from_secs(2);

    let peer = build(&cleartext(), address(8080), budget, Some(ttft));

    assert_eq!(
        peer.options.connection_timeout,
        Some(budget),
        "the first-byte budget does not shorten the connect"
    );
    assert_eq!(
        peer.options.read_timeout,
        Some(ttft),
        "and it is what one read waits for"
    );
}

#[test]
fn a_first_byte_budget_cannot_exceed_the_request_budget() {
    // The request's own deadline is the ceiling: a route cannot ask a read to outlive the request it
    // belongs to.
    let budget = Duration::from_secs(3);
    let ttft = Duration::from_secs(30);

    let peer = build(&cleartext(), address(8080), budget, Some(ttft));

    assert_eq!(
        peer.options.read_timeout,
        Some(budget),
        "the smaller of the two is what a read may wait"
    );
}

#[test]
fn the_default_alpn_stays_http1() {
    let peer = build(&cleartext(), address(8080), BUDGET, Some(BUDGET));

    assert_eq!(
        peer.options.alpn,
        ALPN::H1,
        "a gRPC route must opt into H2 explicitly"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_peer_resolves_an_address_literal_without_a_lookup() {
    let peer = peer(&cleartext(), BUDGET, Some(BUDGET))
        .await
        .expect("127.0.0.1 is an address, not a name");

    assert!(
        peer.to_string().contains("127.0.0.1:8080"),
        "the resolved address must be the destination's: {peer}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_member_resolves_its_address_and_presents_its_authority() {
    // The one combination that proves the two fields are read for two different jobs: an address that
    // needs no lookup, and a name no resolver should ever see.
    let member =
        Destination::new(Scheme::Http, "127.0.0.1", 8080).with_authority("api.example.test");

    let peer = peer(&member, BUDGET, Some(BUDGET))
        .await
        .expect("127.0.0.1 is an address, not a name");

    assert!(
        peer.to_string().contains("127.0.0.1:8080"),
        "resolution reads the connection target: {peer}"
    );
    assert_eq!(
        peer.sni, "api.example.test",
        "the server name is the authority, never the member address"
    );
}

#[test]
fn the_host_header_keeps_a_non_default_port() {
    assert_eq!(host_header(&cleartext()), "127.0.0.1:8080");
}

#[test]
fn the_host_header_omits_the_default_port_of_its_scheme() {
    assert_eq!(host_header(&default_http_port()), "api.example.com");
    assert_eq!(host_header(&secure()), "127.0.0.1");
}

#[test]
fn the_host_header_keeps_a_non_default_https_port() {
    assert_eq!(host_header(&secure_named_host()), "localhost:8443");
}

#[test]
fn a_member_writes_its_authority_into_the_host_header() {
    assert_eq!(
        host_header(&member()),
        "api.example.test:8443",
        "the upstream's logical host is what a virtual host matches on"
    );
}
