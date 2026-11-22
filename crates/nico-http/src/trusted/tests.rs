//! The trust boundary: which CIDR values parse, and how the derivation treats a chain.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use super::*;

fn v4(octets: [u8; 4]) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3]))
}

/// A trusted set of one network, so each test states the boundary it probes.
fn trusted_one(value: &str) -> Vec<Cidr> {
    vec![Cidr::parse(value).expect("the fixture CIDR parses")]
}

#[test]
fn a_cidr_parses_with_its_prefix_length() {
    let network = Cidr::parse("10.0.0.0/8").expect("the fixture parses");
    assert!(network.contains(v4([10, 1, 2, 3])), "inside the network");
    assert!(
        !network.contains(v4([11, 1, 2, 3])),
        "outside the network's prefix"
    );
}

#[test]
fn an_ipv6_network_contains_canonicalised_v4_mapped_peers() {
    let network = Cidr::parse("fd00::/8").expect("the fixture parses");
    let peer = IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0xffff, 0xc00a, 1));
    assert!(network.contains(peer));

    // An IPv4-mapped IPv6 peer matches the IPv4 network it really came from.
    let mapped = IpAddr::V6(Ipv6Addr::from([0, 0, 0, 0, 0, 0xffff, 0x0a00, 0x0001]));
    let v4_network = Cidr::parse("10.0.0.0/8").expect("the fixture parses");
    assert!(v4_network.contains(mapped));
}

#[test]
fn a_cidr_rejects_its_illegal_shapes() {
    for (value, why) in [
        ("10.0.0.1", "no prefix length"),
        ("10.0.0.0/33", "prefix past the v4 width"),
        ("fd00::/129", "prefix past the v6 width"),
        ("not-an-ip/8", "the address does not parse"),
        ("10.0.0.0/eight", "the prefix does not parse"),
    ] {
        let error = Cidr::parse(value).expect_err(why);
        assert!(!error.is_empty(), "the refusal names what is wrong");
    }
}

#[test]
fn a_peer_outside_the_trusted_set_is_the_client_whatever_it_claims() {
    let trusted = trusted_one("10.0.0.0/8");
    let peer = v4([203, 0, 113, 7]);

    let derived = real_client_ip(peer, Some("198.51.100.7, 198.51.100.8"), &trusted);
    assert_eq!(
        derived, peer,
        "the forged chain is ignored: trust belongs to the connection's topology"
    );
}

#[test]
fn a_trusted_peer_walks_the_chain_right_to_left() {
    let trusted = trusted_one("10.0.0.0/8");
    let proxy = v4([10, 0, 0, 5]);
    let real = v4([198, 51, 100, 7]);

    // Right to left: the proxy is trusted and skipped, the real caller is not.
    let derived = real_client_ip(proxy, Some("198.51.100.7, 10.0.0.5"), &trusted);
    assert_eq!(derived, real, "the first untrusted address wins");
}

#[test]
fn a_chain_of_nothing_but_trusted_proxies_names_the_leftmost_caller() {
    let trusted = trusted_one("10.0.0.0/8");
    let proxy = v4([10, 0, 0, 5]);
    let leftmost = v4([10, 0, 0, 2]);

    let derived = real_client_ip(proxy, Some("10.0.0.2, 10.0.0.3, 10.0.0.5"), &trusted);
    assert_eq!(
        derived, leftmost,
        "every hop is trusted: the deployment's declared boundary is the answer"
    );
}

#[test]
fn a_peer_without_a_chain_is_the_peer() {
    let trusted = trusted_one("10.0.0.0/8");
    let proxy = v4([10, 0, 0, 5]);

    assert_eq!(
        real_client_ip(proxy, None, &trusted),
        proxy,
        "no chain to walk: the peer is all the gateway knows"
    );
}

#[test]
fn an_unparseable_chain_entry_wins_as_untrusted() {
    let trusted = trusted_one("10.0.0.0/8");
    let proxy = v4([10, 0, 0, 5]);

    let derived = real_client_ip(proxy, Some("198.51.100.7, not-an-address"), &trusted);
    assert_eq!(
        derived,
        v4([198, 51, 100, 7]),
        "the unparseable entry is not known-trusted, so the walk stops one step earlier"
    );
}

#[test]
fn entries_are_trimmed_before_the_walk() {
    let trusted = trusted_one("10.0.0.0/8");
    let proxy = v4([10, 0, 0, 5]);

    let derived = real_client_ip(proxy, Some("  198.51.100.7 , 10.0.0.5  "), &trusted);
    assert_eq!(derived, v4([198, 51, 100, 7]), "the spaces are formatting");
}
