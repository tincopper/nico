//! The `config_source.type: etcd` schema: acceptance and rejection.
//!
//! Included only when this build carries the `etcd` feature, because a build without it refuses
//! the type before any of this runs. See [`super`](super) for the shared fixtures.

use super::*;

/// Builds a bootstrap document whose `config_source` selects etcd with `body` lines.
///
/// The reload knobs are part of the same section, so they follow the type-specific fields inside
/// it; trailing whitespace in `body` is trimmed so a caller cannot accidentally glue two keys
/// together.
fn etcd_document(body: &str) -> String {
    format!(
        "\
config_source:
  type: etcd
{}
  debounce_ms: 100
  max_wait_ms: 500
  cache_dir: ./.tmp/cache
  allow_bootstrap_cache: false
http:
  listeners:
    - listen: 127.0.0.1:8080
",
        body.trim_end()
    )
}

/// A valid etcd body with `endpoints` list lines and `key`.
fn etcd_with(endpoints: &str, key: &str) -> String {
    etcd_document(&format!(
        "  endpoints:\n{endpoints}\n  key: {key}\n  max_bytes: 4096\n"
    ))
}

const VALID_ENDPOINTS: &str = "    - 127.0.0.1:2379\n    - http://127.0.0.1:2380";

#[test]
fn an_etcd_source_is_accepted_with_its_own_fields() {
    let bootstrap = load_document(&etcd_with(VALID_ENDPOINTS, "nico/config"))
        .expect("the etcd source is valid");

    let SourceSelection::Etcd { endpoints, key } = bootstrap.source() else {
        panic!("the document selects the etcd source");
    };
    assert_eq!(
        endpoints,
        &[
            "127.0.0.1:2379".to_owned(),
            "http://127.0.0.1:2380".to_owned()
        ],
        "the endpoint list passes through untouched: failover is the client's business"
    );
    assert_eq!(key, "nico/config");
    assert_eq!(bootstrap.source_max_bytes(), 4096);
}

#[test]
fn an_empty_endpoint_list_is_refused() {
    let error = load_document(&etcd_document(
        "  endpoints: []\n  key: nico/config\n  max_bytes: 4096",
    ))
    .expect_err("no endpoint can serve the document");
    match error {
        BootstrapError::InvalidField { field, .. } => {
            assert_eq!(field, "config_source.endpoints");
        }
        other => panic!("expected InvalidField, got {other:?}"),
    }
}

#[test]
fn a_missing_endpoint_list_is_a_parse_error() {
    let error = load_document(&etcd_document("  key: nico/config\n  max_bytes: 4096"))
        .expect_err("the endpoints are required");
    assert!(
        matches!(error, BootstrapError::Parse { .. }),
        "expected Parse, got {error:?}"
    );
}

#[test]
fn a_missing_key_is_a_parse_error() {
    let error = load_document(&etcd_document(&format!(
        "  endpoints:\n{VALID_ENDPOINTS}\n  max_bytes: 4096"
    )))
    .expect_err("the key is required");
    assert!(
        matches!(error, BootstrapError::Parse { .. }),
        "expected Parse, got {error:?}"
    );
}

#[test]
fn a_whitespace_padded_key_is_refused() {
    // Quoted, because a plain YAML scalar would have its padding trimmed before we ever saw it.
    let error = load_document(&etcd_with(VALID_ENDPOINTS, "\" nico/config \""))
        .expect_err("the key name is read as written");
    match error {
        BootstrapError::InvalidField { field, .. } => {
            assert_eq!(field, "config_source.key");
        }
        other => panic!("expected InvalidField, got {other:?}"),
    }
}

#[test]
fn an_https_endpoint_is_refused_because_this_build_has_no_tls() {
    let error = load_document(&etcd_with("    - https://127.0.0.1:2379", "nico/config"))
        .expect_err("the compiled client cannot serve https");
    match error {
        BootstrapError::InvalidField { field, reason } => {
            assert_eq!(field, "config_source.endpoints[0]");
            assert!(
                reason.contains("TLS"),
                "the reason must say why https cannot be served, got {reason:?}"
            );
        }
        other => panic!("expected InvalidField, got {other:?}"),
    }
}

#[test]
fn a_non_numeric_port_is_refused() {
    for endpoint in ["127.0.0.1:port", "127.0.0.1:0", "127.0.0.1:99999"] {
        let error = load_document(&etcd_with(&format!("    - {endpoint}"), "nico/config"))
            .expect_err("the port must be a usable shape");
        match error {
            BootstrapError::InvalidField { field, .. } => {
                assert_eq!(field, "config_source.endpoints[0]");
            }
            other => panic!("expected InvalidField for {endpoint}, got {other:?}"),
        }
    }
}

#[test]
fn local_only_fields_are_unknown_fields_for_etcd() {
    for body in [
        format!(
            "  endpoints:\n{VALID_ENDPOINTS}\n  key: nico/config\n  max_bytes: 4096\n  path: ./resources.yaml"
        ),
        format!(
            "  endpoints:\n{VALID_ENDPOINTS}\n  key: nico/config\n  max_bytes: 4096\n  watch: notify"
        ),
    ] {
        let error = load_document(&etcd_document(&body))
            .expect_err("a local field cannot describe an etcd source");
        assert!(
            matches!(error, BootstrapError::Parse { .. }),
            "expected Parse, got {error:?}"
        );
    }
}

#[test]
fn a_zero_size_bound_is_refused_for_etcd_too() {
    let error = load_document(
        &etcd_with(VALID_ENDPOINTS, "nico/config").replace("max_bytes: 4096", "max_bytes: 0"),
    )
    .expect_err("a zero bound accepts no document");
    match error {
        BootstrapError::InvalidField { field, .. } => {
            assert_eq!(field, "config_source.max_bytes");
        }
        other => panic!("expected InvalidField, got {other:?}"),
    }
}
