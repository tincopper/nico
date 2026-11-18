//! Tests for the `http` section: listener entries, their addresses, `h2c`, and the trust roots.

use std::path::PathBuf;

use super::{HttpSettings, HttpSettingsError, Listener};
use crate::{InboundTls, TlsPolicy};

/// Builds the platform trust policy: these tests describe the HTTP section, not what is trusted.
fn tls() -> TlsPolicy {
    TlsPolicy::default()
}

/// Builds a plaintext listener entry.
fn plain(listen: &str) -> Listener {
    Listener::Plain {
        listen: listen.to_owned(),
    }
}

/// Builds a TLS listener entry whose identity is a legal (unread-here) path pair.
fn tls_listener(listen: &str) -> Listener {
    Listener::Tls {
        listen: listen.to_owned(),
        tls: InboundTls::from_bootstrap(
            PathBuf::from("/etc/nico/fullchain.pem"),
            PathBuf::from("/etc/nico/key.pem"),
        ),
    }
}

/// Resolves one plaintext listener with `h2c` omitted.
fn one_plaintext(listen: &str) -> HttpSettings {
    HttpSettings::resolve(vec![plain(listen)], None, tls())
        .expect("a single plaintext listener is a valid section")
}

#[test]
fn one_plaintext_listener_is_resolved_in_the_named_order() {
    let settings = one_plaintext("0.0.0.0:8080");

    assert_eq!(settings.listeners(), &[plain("0.0.0.0:8080")]);
    assert_eq!(settings.plain_addresses(), vec!["0.0.0.0:8080"]);
}

#[test]
fn an_empty_listener_list_is_rejected() {
    let error = HttpSettings::resolve(Vec::new(), None, tls())
        .expect_err("an `http` section without listeners serves nothing");

    assert_eq!(error.field(), "listeners");
    assert!(
        error.reason().contains("at least one"),
        "the reason states what is missing, got `{}`",
        error.reason()
    );
}

#[test]
fn an_address_that_is_not_host_and_port_is_rejected() {
    let error = HttpSettings::resolve(vec![plain("not-an-address")], None, tls())
        .expect_err("a bare name is not host:port");

    assert_eq!(error.field(), "listeners");
    assert!(
        error.reason().contains("ip:port"),
        "the reason names the accepted shape, got `{}`",
        error.reason()
    );
}

#[test]
fn an_address_without_a_port_is_rejected() {
    let error = HttpSettings::resolve(vec![plain("0.0.0.0")], None, tls())
        .expect_err("a bare address has no port");

    assert_eq!(error.field(), "listeners");
}

#[test]
fn a_port_of_zero_is_rejected() {
    let error = HttpSettings::resolve(vec![plain("0.0.0.0:0")], None, tls())
        .expect_err("port zero asks the kernel to pick");

    assert_eq!(error.field(), "listeners");
    assert_eq!(error.reason(), "must name a port other than zero");
}

#[test]
fn a_host_name_with_a_port_is_accepted() {
    let settings = one_plaintext("localhost:8080");

    assert_eq!(settings.plain_addresses(), vec!["localhost:8080"]);
}

#[test]
fn an_ipv6_literal_is_accepted() {
    let settings = one_plaintext("[::1]:8080");

    assert_eq!(settings.listeners(), &[plain("[::1]:8080")]);
}

#[test]
fn a_hostname_that_does_not_resolve_is_rejected() {
    // `.invalid` is reserved never to resolve (RFC 2606), so this exercises the resolution failure
    // path deterministically on a machine with or without a resolver.
    let error = HttpSettings::resolve(vec![plain("nico.invalid:8080")], None, tls())
        .expect_err("an address the resolver cannot turn into a socket is not usable");

    assert_eq!(error.field(), "listeners");
    assert_eq!(
        error.reason(),
        "must be an `ip:port` or `host:port` address"
    );
}

#[test]
fn two_listeners_with_different_addresses_are_accepted() {
    let settings = HttpSettings::resolve(
        vec![plain("127.0.0.1:9000"), plain("127.0.0.1:9001")],
        None,
        tls(),
    )
    .expect("distinct sockets can both bind");

    assert_eq!(
        settings.plain_addresses(),
        vec!["127.0.0.1:9000", "127.0.0.1:9001"]
    );
}

#[test]
fn two_listeners_on_one_literal_address_are_rejected() {
    let error = HttpSettings::resolve(
        vec![plain("0.0.0.0:8080"), plain("0.0.0.0:8080")],
        None,
        tls(),
    )
    .expect_err("two listeners cannot bind one socket");

    assert_eq!(error.field(), "listeners");
    assert!(
        error.reason().contains("0.0.0.0:8080"),
        "the reason names the address that repeats, got `{}`",
        error.reason()
    );
}

#[test]
fn a_literal_and_a_hostname_for_one_socket_are_rejected() {
    // Comparison is post-resolution: `localhost:9000` and `127.0.0.1:9000` name one socket, so the
    // strings being different must not smuggle a duplicate past the check.
    let error = HttpSettings::resolve(
        vec![plain("127.0.0.1:9000"), plain("localhost:9000")],
        None,
        tls(),
    )
    .expect_err("both spellings resolve to the same socket");

    assert_eq!(error.field(), "listeners");
}

#[test]
fn a_tls_listener_address_is_checked_and_counted_too() {
    let error = HttpSettings::resolve(
        vec![tls_listener("127.0.0.1:9000"), plain("127.0.0.1:9000")],
        None,
        tls(),
    )
    .expect_err("a TLS entry binds a socket just like a plaintext one");

    assert_eq!(error.field(), "listeners");
}

#[test]
fn h2c_defaults_to_false_and_stays_false_when_written_as_false() {
    assert!(!one_plaintext("0.0.0.0:8080").h2c());
    assert!(
        !HttpSettings::resolve(vec![plain("0.0.0.0:8080")], Some(false), tls())
            .expect("false is a legal value")
            .h2c()
    );
}

#[test]
fn h2c_is_accepted_when_a_plaintext_listener_exists() {
    let settings = HttpSettings::resolve(vec![plain("0.0.0.0:8080")], Some(true), tls())
        .expect("h2c applies to the plaintext listener");

    assert!(settings.h2c());
}

#[test]
fn h2c_without_a_plaintext_listener_is_rejected() {
    let error = HttpSettings::resolve(vec![tls_listener("0.0.0.0:8443")], Some(true), tls())
        .expect_err("h2c would state a knob with no plaintext listener to apply it to");

    assert_eq!(error.field(), "h2c");
    assert!(
        error.reason().contains("plaintext"),
        "the reason states what h2c needs, got `{}`",
        error.reason()
    );
}

#[test]
fn h2c_is_accepted_alongside_a_tls_listener() {
    let settings = HttpSettings::resolve(
        vec![plain("0.0.0.0:8080"), tls_listener("0.0.0.0:8443")],
        Some(true),
        tls(),
    )
    .expect("one listener per transport: h2c serves the plaintext one");

    assert!(settings.h2c());
    assert_eq!(settings.plain_addresses(), vec!["0.0.0.0:8080"]);
}

#[test]
fn the_tls_identity_is_carried_by_its_listener() {
    let identity = InboundTls::from_bootstrap(
        PathBuf::from("/etc/nico/fullchain.pem"),
        PathBuf::from("/etc/nico/key.pem"),
    );
    let settings = HttpSettings::resolve(
        vec![Listener::Tls {
            listen: "0.0.0.0:8443".to_owned(),
            tls: identity,
        }],
        None,
        tls(),
    )
    .expect("the identity paths are a valid listener");

    assert_eq!(settings.listeners(), &[tls_listener("0.0.0.0:8443")]);
}

#[test]
fn a_tls_listener_does_not_appear_in_the_plain_addresses() {
    let settings = HttpSettings::resolve(vec![tls_listener("0.0.0.0:8443")], None, tls())
        .expect("the TLS listener is a valid section");

    assert!(
        settings.plain_addresses().is_empty(),
        "a TLS listener serves no plaintext address"
    );
    assert_eq!(
        settings.listener_addresses(),
        vec!["0.0.0.0:8443"],
        "but it is still an address this process binds"
    );
}

#[test]
fn the_listener_addresses_cover_both_transports_in_the_named_order() {
    let settings = HttpSettings::resolve(
        vec![tls_listener("0.0.0.0:8443"), plain("0.0.0.0:8080")],
        None,
        tls(),
    )
    .expect("one listener per transport is a valid section");

    assert_eq!(
        settings.listener_addresses(),
        vec!["0.0.0.0:8443", "0.0.0.0:8080"],
        "every bound address appears once, in the order the section named it"
    );
    assert_eq!(
        settings.plain_addresses(),
        vec!["0.0.0.0:8080"],
        "the plaintext subset is what `h2c` applies to"
    );
}

#[test]
fn the_trust_roots_are_carried_by_the_settings() {
    let bundle = TlsPolicy::from_bootstrap(Some(PathBuf::from("/etc/nico/roots.pem")));

    let settings = HttpSettings::resolve(vec![plain("0.0.0.0:8080")], None, bundle.clone())
        .expect("defaults are valid");

    assert_eq!(
        settings.tls(),
        &bundle,
        "a configured bundle must survive resolution unchanged: it is a startup fact, not a knob"
    );
    assert_eq!(
        one_plaintext("0.0.0.0:8080").tls(),
        &TlsPolicy::default(),
        "an omitted `proxy` section is the platform store"
    );
}

#[test]
fn the_error_renders_the_field_and_the_reason() {
    let error: HttpSettingsError = HttpSettings::resolve(vec![plain("0.0.0.0:0")], None, tls())
        .expect_err("port zero is rejected");

    assert_eq!(
        error.to_string(),
        "http setting `listeners` is invalid: must name a port other than zero"
    );
}
