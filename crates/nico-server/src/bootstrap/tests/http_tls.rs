//! The optional `http.tls` section: the listener's certificate and key.
//!
//! These tests use the same real certificate fixtures as the e2e listener. A certificate is public
//! test data; the private key is a fixture key created only for this repository and is not a
//! deployment credential.

use std::path::{Path, PathBuf};

use super::*;

/// Returns one file of the repository's TLS fixture set.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/tls")
        .join(name)
}

/// Appends an `http` section with `body` to an otherwise complete bootstrap document.
fn with_http(body: &str) -> String {
    format!(
        "{}http:\n{body}",
        document("./resources.yaml", "notify", 100, 500)
    )
}

/// Loads a bootstrap document whose `http` section is `http`.
fn launched(http: &str) -> Result<BootstrapConfig, BootstrapError> {
    load_document(&with_http(http))
}

/// Renders the two identity paths as an `http.tls` section.
fn identity(cert: &Path, key: &Path) -> String {
    format!(
        "  tls:\n    cert_file: {}\n    key_file: {}\n",
        cert.display(),
        key.display()
    )
}

#[test]
fn a_valid_identity_reaches_the_listener_settings() {
    let certificate = fixture("localhost.pem");
    let key = fixture("localhost.key");

    let bootstrap = launched(&format!(
        "  listen: 127.0.0.1:8443\n{}",
        identity(&certificate, &key)
    ))
    .expect("a real certificate and key are a valid inbound identity");

    let inbound = bootstrap
        .http()
        .inbound_tls()
        .expect("the listener terminates TLS");
    assert_eq!(inbound.cert_file(), certificate);
    assert_eq!(inbound.key_file(), key);
}

#[test]
fn an_empty_section_is_not_silently_plaintext() {
    let error = launched("  tls:\n").expect_err("an empty section means TLS was intended");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "http.tls.cert_file"
        ),
        "expected http.tls.cert_file, got {error:?}"
    );
}

#[test]
fn a_missing_key_is_reported_by_field() {
    let certificate = fixture("localhost.pem");
    let section = format!("  tls:\n    cert_file: {}\n", certificate.display());
    let error = launched(&section).expect_err("a certificate alone cannot terminate TLS");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "http.tls.key_file"
        ),
        "expected http.tls.key_file, got {error:?}"
    );
}

#[test]
fn an_unreadable_certificate_is_reported_by_field() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let section = identity(
        &directory.path().join("absent.pem"),
        &fixture("localhost.key"),
    );
    let error = launched(&section).expect_err("the certificate does not exist");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "http.tls.cert_file"
        ),
        "expected http.tls.cert_file, got {error:?}"
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

    let error = launched(&identity(&certificate, &fixture("localhost.key")))
        .expect_err("garbage is not a certificate");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "http.tls.cert_file"
        ),
        "expected http.tls.cert_file, got {error:?}"
    );
}

#[test]
fn a_key_that_does_not_match_the_certificate_is_reported_by_field() {
    let error = launched(&identity(
        &fixture("localhost.pem"),
        &fixture("other-root.key"),
    ))
    .expect_err("two identities cannot be paired");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "http.tls.key_file"
        ),
        "expected http.tls.key_file, got {error:?}"
    );
}

#[test]
fn h2c_and_tls_are_mutually_exclusive() {
    let section = format!(
        "  listen: 127.0.0.1:8443\n  h2c: true\n{}",
        identity(&fixture("localhost.pem"), &fixture("localhost.key"))
    );
    let error = launched(&section).expect_err("one listener has one transport");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "http.h2c"
        ),
        "expected http.h2c, got {error:?}"
    );
}

#[test]
fn an_unknown_field_inside_http_tls_is_rejected() {
    let section = format!(
        "  tls:\n    cert_file: {}\n    key_file: {}\n    extra: true\n",
        fixture("localhost.pem").display(),
        fixture("localhost.key").display()
    );
    let error = launched(&section).expect_err("the TLS section is a closed shape");

    assert!(
        matches!(error, BootstrapError::Parse { .. }),
        "expected Parse, got {error:?}"
    );
}
