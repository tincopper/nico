//! `http.listeners[]` entries that terminate TLS: the listener's certificate and key.
//!
//! These tests use the same real certificate fixtures as the e2e listener. A certificate is public
//! test data; the private key is a fixture key created only for this repository and is not a
//! deployment credential.
//!
//! Two facts are asserted throughout: the transport decision is the *presence* of a `tls` block (so
//! an empty block can never fall back to plaintext), and every error names the entry that carries
//! the problem, because a list is the only name a listener has.

use std::path::{Path, PathBuf};

use nico_http::{InboundTls, Listener};

use super::*;

/// Returns one file of the repository's TLS fixture set.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/tls")
        .join(name)
}

/// A document whose `http` section holds `lines`.
fn with_http(lines: &str) -> String {
    format!(
        "{}http:\n{lines}",
        config_source("./resources.yaml", "notify", 100, 500)
    )
}

/// A document whose `http` section lists `entries` under `listeners`.
fn with_listeners(entries: &str) -> String {
    with_http(&format!("  listeners:\n{entries}"))
}

/// Loads a document whose `http.listeners` holds `entries`.
fn launched(entries: &str) -> Result<BootstrapConfig, BootstrapError> {
    load_document(&with_listeners(entries))
}

/// Renders one listener entry at `listen`, with `tls` as its optional block.
fn listener(listen: &str, tls: &str) -> String {
    format!("    - listen: {listen}\n{tls}")
}

/// Renders a listener's `tls` block for `cert` and `key`.
fn identity(cert: &Path, key: &Path) -> String {
    format!(
        "      tls:\n        cert_file: {}\n        key_file: {}\n",
        cert.display(),
        key.display()
    )
}

/// The fixture identity as a listener's `tls` block.
fn fixture_identity() -> String {
    identity(&fixture("localhost.pem"), &fixture("localhost.key"))
}

#[test]
fn a_tls_entry_reaches_the_listener_settings() {
    let entries = listener("127.0.0.1:8443", &fixture_identity());

    let bootstrap = launched(&entries).expect("a real certificate and key are a valid identity");

    assert_eq!(
        bootstrap.http().listeners(),
        &[Listener::Tls {
            listen: "127.0.0.1:8443".to_owned(),
            tls: InboundTls::from_bootstrap(fixture("localhost.pem"), fixture("localhost.key")),
        }],
        "the entry's identity is the pair the listener presents"
    );
    assert!(
        bootstrap.http().plain_addresses().is_empty(),
        "a TLS entry serves no plaintext address"
    );
}

#[test]
fn a_plaintext_and_a_tls_listener_coexist() {
    let entries = format!(
        "{}{}",
        listener("127.0.0.1:8080", ""),
        listener("127.0.0.1:8443", &fixture_identity())
    );

    let bootstrap = launched(&entries).expect("one listener per transport is a valid section");

    assert_eq!(
        bootstrap.http().listener_addresses(),
        ["127.0.0.1:8080", "127.0.0.1:8443"],
        "both listeners are bound, in the order the section named them"
    );
    assert_eq!(
        bootstrap.http().plain_addresses(),
        ["127.0.0.1:8080"],
        "only the entry without a `tls` block serves plaintext"
    );
    assert!(
        matches!(bootstrap.http().listeners()[1], Listener::Tls { .. }),
        "the second entry terminates TLS"
    );
}

#[test]
fn an_empty_tls_block_under_a_listener_is_not_silently_plaintext() {
    let error = launched(&listener("127.0.0.1:8443", "      tls:\n"))
        .expect_err("an empty block means TLS was intended");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "http.listeners[0].tls.cert_file"
        ),
        "expected http.listeners[0].tls.cert_file, got {error:?}"
    );
}

#[test]
fn a_missing_key_is_reported_by_field() {
    let section = format!(
        "      tls:\n        cert_file: {}\n",
        fixture("localhost.pem").display()
    );
    let error = launched(&listener("127.0.0.1:8443", &section))
        .expect_err("a certificate alone cannot terminate TLS");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "http.listeners[0].tls.key_file"
        ),
        "expected http.listeners[0].tls.key_file, got {error:?}"
    );
}

#[test]
fn an_unreadable_certificate_is_reported_by_field() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let section = identity(
        &directory.path().join("absent.pem"),
        &fixture("localhost.key"),
    );
    let error = launched(&listener("127.0.0.1:8443", &section))
        .expect_err("the certificate does not exist");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "http.listeners[0].tls.cert_file"
        ),
        "expected http.listeners[0].tls.cert_file, got {error:?}"
    );
    assert!(
        error.to_string().contains("could not be read"),
        "the reason says what failed: {error}"
    );
}

#[test]
fn a_bad_certificate_is_reported_by_field() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let certificate = directory.path().join("bad.pem");
    std::fs::write(&certificate, b"not a certificate").expect("fixture write succeeds");

    let error = launched(&listener(
        "127.0.0.1:8443",
        &identity(&certificate, &fixture("localhost.key")),
    ))
    .expect_err("garbage is not a certificate");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "http.listeners[0].tls.cert_file"
        ),
        "expected http.listeners[0].tls.cert_file, got {error:?}"
    );
}

#[test]
fn a_key_that_does_not_match_the_certificate_is_reported_by_field() {
    let error = launched(&listener(
        "127.0.0.1:8443",
        &identity(&fixture("localhost.pem"), &fixture("other-root.key")),
    ))
    .expect_err("two identities cannot be paired");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "http.listeners[0].tls.key_file"
        ),
        "expected http.listeners[0].tls.key_file, got {error:?}"
    );
}

#[test]
fn the_second_entry_reports_its_own_index() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let entries = format!(
        "{}{}",
        listener("127.0.0.1:8080", ""),
        listener(
            "127.0.0.1:8443",
            &identity(
                &directory.path().join("absent.pem"),
                &fixture("localhost.key")
            )
        )
    );

    let error = launched(&entries).expect_err("the second entry's certificate does not exist");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "http.listeners[1].tls.cert_file"
        ),
        "the error must name the entry that carries the bad file, got {error:?}"
    );
}

#[test]
fn h2c_with_only_tls_entries_is_rejected() {
    let text = with_http(&format!(
        "  h2c: true\n  listeners:\n{}",
        listener("127.0.0.1:8443", &fixture_identity())
    ));

    let error = load_document(&text).expect_err("cleartext HTTP/2 needs a plaintext listener");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "http.h2c"
        ),
        "expected http.h2c, got {error:?}"
    );
}

#[test]
fn h2c_with_one_plaintext_entry_beside_a_tls_entry_is_accepted() {
    let entries = format!(
        "{}{}",
        listener("127.0.0.1:8080", ""),
        listener("127.0.0.1:8443", &fixture_identity())
    );
    let text = with_http(&format!("  h2c: true\n  listeners:\n{entries}"));

    let bootstrap = load_document(&text).expect("h2c applies to the plaintext listener");

    assert!(bootstrap.http().h2c());
}

#[test]
fn the_old_inbound_tls_section_is_an_unknown_field() {
    // `http.tls` used to take over the whole section. It is not aliased: a file that still writes
    // it must fail loudly instead of silently serving plaintext.
    let text = with_http(concat!(
        "  listen: 127.0.0.1:8443\n",
        "  tls:\n",
        "    cert_file: /etc/nico/fullchain.pem\n",
        "    key_file: /etc/nico/key.pem\n",
    ));

    let error = load_document(&text).expect_err("the section shape changed");

    assert!(
        matches!(error, BootstrapError::Parse { .. }),
        "expected Parse, got {error:?}"
    );
}

#[test]
fn an_unknown_field_inside_a_listeners_tls_block_is_rejected() {
    let section = format!("{}        extra: true\n", fixture_identity());
    let error = launched(&listener("127.0.0.1:8443", &section))
        .expect_err("a listener's tls block is a closed shape");

    assert!(
        matches!(error, BootstrapError::Parse { .. }),
        "expected Parse, got {error:?}"
    );
}
