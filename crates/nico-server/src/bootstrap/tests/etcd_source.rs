//! The `config_source.type: etcd` schema: acceptance and rejection.
//!
//! Included only when this build carries the `etcd` feature, because a build without it refuses
//! the type before any of this runs. See [`super`](super) for the shared fixtures.
//!
//! What is asserted here is the body itself — the endpoint list, its shapes, the transport matrix and
//! the key — plus the fixtures the two optional blocks share. The blocks have their own module:
//! [`blocks`]. Every refusal names the field the operator wrote, because that name is the only thing
//! that tells them where to look.

use super::*;

mod blocks;

/// The workspace's shared TLS fixtures, above any one crate — see `fixtures/tls/README.md`.
///
/// The leaf certificate a client identity is built from: `DNS:localhost, IP:127.0.0.1`.
const LOCALHOST_PEM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/tls/localhost.pem"
));
/// The private key of [`LOCALHOST_PEM`].
const LOCALHOST_KEY: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/tls/localhost.key"
));
/// The trust root [`LOCALHOST_PEM`] chains to.
const TEST_ROOT_PEM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/tls/test-root.pem"
));
/// A private key that belongs to no certificate above: the pair rule's negative case.
const OTHER_ROOT_KEY: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/tls/other-root.key"
));

/// The PEM files a `tls` block names, written once per test and deleted with it.
///
/// The bootstrap reads real files, because that is the check under test: a path that does not exist
/// and bytes that are not a certificate are two different refusals with the same field name.
struct PemFiles {
    directory: tempfile::TempDir,
}

impl PemFiles {
    /// Writes the two roots, the loopback leaf and its key, plus an unrelated key.
    fn new() -> Self {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        for (name, bytes) in [
            ("ca.pem", TEST_ROOT_PEM),
            ("client.pem", LOCALHOST_PEM),
            ("client.key", LOCALHOST_KEY),
            ("other.key", OTHER_ROOT_KEY),
            ("garbage.pem", b"not a certificate".as_slice()),
        ] {
            std::fs::write(directory.path().join(name), bytes).expect("fixture write succeeds");
        }
        Self { directory }
    }

    /// Returns the path of one written file, as a `tls` block would write it.
    fn path(&self, name: &str) -> String {
        self.directory.path().join(name).display().to_string()
    }

    /// Returns a `tls` block naming the usable trust root.
    fn ca_only(&self) -> String {
        format!("  tls:\n    ca_file: {}\n", self.path("ca.pem"))
    }

    /// Returns a `tls` block naming the trust root and a usable client identity.
    fn with_identity(&self) -> String {
        format!(
            "  tls:\n    ca_file: {}\n    cert_file: {}\n    key_file: {}\n",
            self.path("ca.pem"),
            self.path("client.pem"),
            self.path("client.key")
        )
    }
}

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

/// A valid etcd body with `endpoints` list lines, `key`, and any extra block lines.
fn etcd_body(endpoints: &str, key: &str, extra: &str) -> String {
    etcd_document(&format!(
        "  endpoints:\n{endpoints}\n  key: {key}\n  max_bytes: 4096\n{extra}"
    ))
}

/// A valid etcd body with `endpoints` list lines and `key`.
fn etcd_with(endpoints: &str, key: &str) -> String {
    etcd_body(endpoints, key, "")
}

/// A valid etcd body with a plaintext endpoint list.
const VALID_ENDPOINTS: &str = "    - 127.0.0.1:2379\n    - http://127.0.0.1:2380";

/// Asserts a document was refused under `field`, and returns the reason it gave.
fn refused(document: &str, field: &str) -> String {
    match load_document(document) {
        Ok(_) => panic!("expected the document to be refused under `{field}`"),
        Err(BootstrapError::InvalidField {
            field: found,
            reason,
        }) => {
            assert_eq!(
                found, field,
                "the refusal must name the field that is wrong"
            );
            reason.to_string()
        }
        Err(other) => panic!("expected InvalidField under `{field}`, got {other:?}"),
    }
}

#[test]
fn an_etcd_source_is_accepted_with_its_own_fields() {
    let bootstrap = load_document(&etcd_with(VALID_ENDPOINTS, "nico/config"))
        .expect("the etcd source is valid");

    let SourceSelection::Etcd {
        endpoints,
        key,
        tls,
        auth,
    } = bootstrap.source()
    else {
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
    assert!(tls.is_none(), "no `tls` block is no TLS");
    assert!(auth.is_none(), "no `auth` block is anonymous access");
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
fn an_https_endpoint_without_a_tls_block_is_refused() {
    let reason = refused(
        &etcd_with("    - https://127.0.0.1:2379", "nico/config"),
        "config_source.endpoints[0]",
    );
    assert!(
        reason.contains("tls"),
        "the reason must name the block that is missing, got {reason:?}"
    );
}

#[test]
fn a_tls_block_over_a_plaintext_endpoint_is_refused() {
    let files = PemFiles::new();
    let ca_only = files.ca_only();
    for endpoint in ["http://127.0.0.1:2379", "127.0.0.1:2379"] {
        let reason = refused(
            &etcd_body(&format!("    - {endpoint}"), "nico/config", &ca_only),
            "config_source.endpoints[0]",
        );
        assert!(
            reason.contains("https"),
            "the reason must say which scheme TLS serves, got {reason:?}"
        );
    }
}

#[test]
fn a_list_mixing_the_two_schemes_is_refused() {
    let files = PemFiles::new();
    let ca_only = files.ca_only();
    for extra in ["", &ca_only] {
        let error = load_document(&etcd_body(
            "    - https://127.0.0.1:2379\n    - http://127.0.0.1:2380",
            "nico/config",
            extra,
        ))
        .expect_err("one client serves one transport");
        match error {
            BootstrapError::InvalidField { field, reason } => {
                assert_eq!(
                    field, "config_source.endpoints",
                    "a mixed list is refused as a list, not as one entry"
                );
                assert!(
                    reason.contains("scheme"),
                    "the reason must name what disagrees, got {reason:?}"
                );
            }
            other => panic!("expected InvalidField, got {other:?}"),
        }
    }
}

#[test]
fn the_etcd_blocks_are_unknown_fields_for_local() {
    // `local` has no transport and no credentials: writing them there is a typo, not a preference.
    for block in [
        "  tls:\n    ca_file: /etc/nico/ca.pem\n",
        "  auth:\n    user: nico\n",
    ] {
        let document = format!(
            "{}http:\n  listeners:\n    - listen: 127.0.0.1:8080\n",
            config_source("./resources.yaml", "notify", 100, 500) + block
        );
        let error = load_document(&document)
            .expect_err("a block of another source type cannot describe a local source");
        assert!(
            matches!(error, BootstrapError::Parse { .. }),
            "expected Parse for {block:?}, got {error:?}"
        );
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
