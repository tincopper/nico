//! Unit tests for the optional `metrics` section.
//!
//! The section is a process knob, so the tests are about what a written file may and may not say:
//! a present section must name an address, that address must be a legal listener, and it must not
//! collide with the data plane's own listener.

use super::*;
use crate::BootstrapConfig;

/// Writes `document` to a temporary file and loads it as a bootstrap.
fn load_document(document: &str) -> Result<BootstrapConfig, BootstrapError> {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let path = directory.path().join("bootstrap.yaml");
    std::fs::write(&path, document).expect("fixture write succeeds");
    BootstrapConfig::load(&path)
}

/// A complete bootstrap document whose `http` and `metrics` sections are appended by the caller.
fn document(http: &str, metrics: &str) -> String {
    format!(
        "\
config_source:
  type: local
  path: ./resources.yaml
  max_bytes: 4096
  watch: notify
  debounce_ms: 100
  max_wait_ms: 500
  cache_dir: ./.tmp/cache
  allow_bootstrap_cache: false
{http}{metrics}"
    )
}

/// Returns one file of the repository's TLS fixture set.
fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/tls")
        .join(name)
}

const HTTP: &str = "http:\n  listeners:\n    - listen: 127.0.0.1:8080\n";

/// The same section with a plaintext and a TLS listener on two other addresses.
fn dual_http(tls_port: u16) -> String {
    format!(
        "\
http:
  listeners:
    - listen: 127.0.0.1:8080
    - listen: 127.0.0.1:{tls_port}
      tls:
        cert_file: {}
        key_file: {}
",
        fixture("localhost.pem").display(),
        fixture("localhost.key").display()
    )
}

#[test]
fn an_omitted_section_disables_the_surface() {
    let bootstrap = load_document(&document(HTTP, "")).expect("the bootstrap is valid");

    assert!(
        bootstrap.metrics().is_none(),
        "no `metrics` section means no listener, no recorder and no behaviour change"
    );
}

#[test]
fn a_written_address_is_validated_and_kept() {
    let bootstrap = load_document(&document(HTTP, "metrics:\n  listen: 127.0.0.1:9090\n"))
        .expect("the bootstrap is valid");

    let metrics = bootstrap.metrics().expect("the section is present");
    assert_eq!(metrics.listen(), "127.0.0.1:9090");
}

#[test]
fn a_present_section_without_an_address_is_rejected() {
    let error = load_document(&document(HTTP, "metrics: {}\n"))
        .expect_err("a present section must name an address");

    match error {
        BootstrapError::InvalidField { field, reason } => {
            assert_eq!(field, "metrics.listen");
            assert!(
                reason.contains("must name a `listen` address"),
                "the reason explains the rule: {reason}"
            );
        }
        other => panic!("expected InvalidField, got {other:?}"),
    }
}

#[test]
fn an_illegal_address_is_rejected() {
    let error = load_document(&document(HTTP, "metrics:\n  listen: 127.0.0.1:0\n"))
        .expect_err("port zero is not a bindable address");

    match error {
        BootstrapError::InvalidField { field, .. } => assert_eq!(field, "metrics.listen"),
        other => panic!("expected InvalidField, got {other:?}"),
    }
}

#[test]
fn the_data_planes_own_address_is_rejected() {
    let error = load_document(&document(HTTP, "metrics:\n  listen: 127.0.0.1:8080\n"))
        .expect_err("two listeners cannot bind the same address");

    match error {
        BootstrapError::InvalidField { field, reason } => {
            assert_eq!(field, "metrics.listen");
            assert!(
                reason.contains("http.listen") && reason.contains("127.0.0.1:8080"),
                "the reason names the other listener: {reason}"
            );
        }
        other => panic!("expected InvalidField, got {other:?}"),
    }
}

#[test]
fn a_tls_listeners_address_is_rejected_too() {
    let error = load_document(&document(
        &dual_http(8443),
        "metrics:\n  listen: 127.0.0.1:8443\n",
    ))
    .expect_err("a TLS listener owns its socket exactly as a plaintext one does");

    match error {
        BootstrapError::InvalidField { field, reason } => {
            assert_eq!(field, "metrics.listen");
            assert!(
                reason.contains("127.0.0.1:8443"),
                "the reason names the listener it collides with: {reason}"
            );
        }
        other => panic!("expected InvalidField, got {other:?}"),
    }
}

#[test]
fn an_unknown_field_is_rejected() {
    let error = load_document(&document(
        HTTP,
        "metrics:\n  listen: 127.0.0.1:9090\n  tls: true\n",
    ))
    .expect_err("the section is closed, like `http`");

    assert!(
        matches!(error, BootstrapError::Parse { .. }),
        "expected Parse, got {error:?}"
    );
}
