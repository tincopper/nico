//! Unit tests for the optional `metrics` section.
//!
//! The section is a process knob, so the tests are about what a written file may and may not say:
//! a present section must name an address, that address must be a legal listener, and it must not
//! collide with the data plane's own listener. The optional `tls` block adds one more thing a file
//! states, and one more way for it to be wrong: written-but-unusable must be a startup failure, not
//! a quiet downgrade to plaintext.

use nico_http::InboundTls;

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

/// Returns one file of the workspace's shared TLS fixture set (`fixtures/tls/`).
fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/tls")
        .join(name)
}

/// A `metrics` section on `listen` that presents `cert` and `key` over TLS.
fn tls_section(listen: &str, cert: &std::path::Path, key: &std::path::Path) -> String {
    format!(
        "metrics:\n  listen: {listen}\n  tls:\n    cert_file: {}\n    key_file: {}\n",
        cert.display(),
        key.display()
    )
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
    assert!(
        metrics.tls().is_none(),
        "no `tls` block means plaintext: TLS is never a default a file did not ask for"
    );
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
        "metrics:\n  listen: 127.0.0.1:9090\n  enabled: true\n",
    ))
    .expect_err("the section is closed, like `http`");

    assert!(
        matches!(error, BootstrapError::Parse { .. }),
        "expected Parse, got {error:?}"
    );
}

#[test]
fn a_tls_block_that_is_not_a_mapping_is_rejected() {
    let error = load_document(&document(
        HTTP,
        "metrics:\n  listen: 127.0.0.1:9090\n  tls: true\n",
    ))
    .expect_err("`tls` is a block of two files, not a boolean");

    assert!(
        matches!(error, BootstrapError::Parse { .. }),
        "expected Parse, got {error:?}"
    );
}

#[test]
fn a_tls_block_reaches_the_listener_settings() {
    let cert = fixture("localhost.pem");
    let key = fixture("localhost.key");

    let bootstrap = load_document(&document(HTTP, &tls_section("127.0.0.1:9090", &cert, &key)))
        .expect("the fixture certificate and key are a valid identity");

    let metrics = bootstrap.metrics().expect("the section is present");
    assert_eq!(
        metrics.tls(),
        Some(&InboundTls::from_bootstrap(cert, key)),
        "a written `tls` block is the identity the self-reporting listener presents"
    );
    assert_eq!(
        metrics.listen(),
        "127.0.0.1:9090",
        "TLS is a transport fact; it does not move the address"
    );
}

#[test]
fn an_empty_tls_block_under_metrics_is_not_silently_plaintext() {
    let error = load_document(&document(
        HTTP,
        "metrics:\n  listen: 127.0.0.1:9090\n  tls:\n",
    ))
    .expect_err("an operator who wrote `tls:` meant TLS");

    match error {
        BootstrapError::InvalidField { field, reason } => {
            assert_eq!(field, "metrics.tls.cert_file");
            assert!(
                reason.contains("is required"),
                "the reason says which file the block is missing: {reason}"
            );
        }
        other => panic!("expected InvalidField, got {other:?}"),
    }
}

#[test]
fn a_metrics_tls_block_without_a_key_is_reported_by_field() {
    let section = format!(
        "metrics:\n  listen: 127.0.0.1:9090\n  tls:\n    cert_file: {}\n",
        fixture("localhost.pem").display()
    );

    let error = load_document(&document(HTTP, &section))
        .expect_err("a certificate alone cannot terminate TLS");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "metrics.tls.key_file"
        ),
        "expected metrics.tls.key_file, got {error:?}"
    );
}

#[test]
fn an_unreadable_metrics_certificate_is_reported_by_field() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let section = tls_section(
        "127.0.0.1:9090",
        &directory.path().join("absent.pem"),
        &fixture("localhost.key"),
    );

    let error =
        load_document(&document(HTTP, &section)).expect_err("the certificate does not exist");

    match error {
        BootstrapError::InvalidField { field, reason } => {
            assert_eq!(field, "metrics.tls.cert_file");
            assert!(
                reason.contains("could not be read"),
                "the reason says what failed: {reason}"
            );
        }
        other => panic!("expected InvalidField, got {other:?}"),
    }
}

#[test]
fn a_metrics_identity_that_is_not_a_pair_is_reported_by_field() {
    let section = tls_section(
        "127.0.0.1:9090",
        &fixture("localhost.pem"),
        &fixture("other-root.key"),
    );

    let error =
        load_document(&document(HTTP, &section)).expect_err("two identities cannot be paired");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "metrics.tls.key_file"
        ),
        "expected metrics.tls.key_file, got {error:?}"
    );
}

#[test]
fn an_unknown_field_inside_the_metrics_tls_block_is_rejected() {
    let section = format!(
        "{section}    ca_file: /etc/nico/roots.pem\n",
        section = tls_section(
            "127.0.0.1:9090",
            &fixture("localhost.pem"),
            &fixture("localhost.key")
        )
    );

    let error =
        load_document(&document(HTTP, &section)).expect_err("a `tls` block is a closed shape");

    assert!(
        matches!(error, BootstrapError::Parse { .. }),
        "expected Parse, got {error:?}"
    );
}

#[test]
fn a_colliding_address_is_reported_before_the_identity() {
    // Both are wrong, and the address is the one that decides whether there is a socket at all: an
    // operator sent to the certificate first would fix a field that is not the reason. The
    // certificate is missing as well, so the field that comes back is decided by the order alone —
    // a valid identity here would report `metrics.listen` whichever way the two are resolved.
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let section = tls_section(
        "127.0.0.1:8080",
        &directory.path().join("absent.pem"),
        &fixture("localhost.key"),
    );

    let error = load_document(&document(HTTP, &section))
        .expect_err("the data plane already binds this address");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "metrics.listen"
        ),
        "expected metrics.listen, got {error:?}"
    );
}
