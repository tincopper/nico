//! The optional `proxy.tls` block: the trust roots an upstream handshake is verified against.
//!
//! Kept in its own file for the reason its parent states: the fixtures plus the startup outcomes
//! are longer than the module they exercise, and the 500-line budget applies to the parent file.
//!
//! The block is the outbound half of the trust story. The inbound half is a listener's own `tls`
//! block in `http.listeners[]`, and the two are deliberately named apart: one is what this process
//! presents, the other is what it verifies a peer against.

use nico_http::TlsPolicy;

use super::*;

/// A self-signed test root, valid far beyond any clock this test will see.
///
/// It carries no key material: it is a trust anchor and nothing else, so committing it is safe.
const TEST_ROOT: &str = "\
-----BEGIN CERTIFICATE-----
MIIBoTCCAUegAwIBAgIUEt8o0UNscp3Lwon0omZoZsZOOkwwCgYIKoZIzj0EAwIw
HTEbMBkGA1UEAwwSbmljbyB0ZXN0IHJvb3Qgb25lMCAXDTI2MTExMTA3Mjg1OFoY
DzIxMjYxMDE4MDcyODU4WjAdMRswGQYDVQQDDBJuaWNvIHRlc3Qgcm9vdCBvbmUw
WTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAARUW8ZyKse5cT4emOomnnXp/E28o1yN
Zmlodpe9IBhQQCLUR0Gl3dMZJixVwfQ0vpI1aYu8dMCiR2t6ItRXxmzto2MwYTAd
BgNVHQ4EFgQUneYc2pRsCc5JwdjcdQNNR5jI/qwwHwYDVR0jBBgwFoAUneYc2pRs
Cc5JwdjcdQNNR5jI/qwwDwYDVR0TAQH/BAUwAwEB/zAOBgNVHQ8BAf8EBAMCAgQw
CgYIKoZIzj0EAwIDSAAwRQIhAOQbPbb+aw5l9N8JERpU8ZajiiY+wF+EzV20fBTA
uTHPAiAb6xBdOVSdgqWqSnggn4c27L3Zd0u/ZDTLCUc3ZDJeHw==
-----END CERTIFICATE-----
";

/// Appends a `proxy` section to an otherwise complete bootstrap document.
fn with_proxy(section: &str) -> String {
    format!(
        "{}proxy:\n{section}",
        document("./resources.yaml", "notify", 100, 500)
    )
}

/// Loads a document whose `proxy` section holds `section`.
fn launched(section: &str) -> Result<BootstrapConfig, BootstrapError> {
    load_document(&with_proxy(section))
}

/// Renders the `tls` block that points at `path`.
fn tls_block(path: &Path) -> String {
    format!("  tls:\n    ca_file: {}\n", path.display())
}

/// Writes `contents` into a fresh file and returns the directory that owns it plus its path.
fn pointing_at(name: &str, contents: &str) -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let path = directory.path().join(name);
    std::fs::write(&path, contents).expect("fixture write succeeds");
    (directory, path)
}

#[test]
fn an_omitted_section_uses_the_platform_roots() {
    let bootstrap = load_document(&document("./resources.yaml", "notify", 100, 500))
        .expect("the document is valid");

    assert_eq!(
        bootstrap.http().tls(),
        &TlsPolicy::default(),
        "no `proxy` section must keep the platform store, exactly as the top-level `tls` did"
    );
}

#[test]
fn an_empty_section_is_rejected() {
    // `proxy:` with no body would otherwise read as "no section" and silently keep the platform
    // roots, which is the opposite of what an operator who wrote the key intended.
    let error = launched("").expect_err("an empty `proxy` section says nothing");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "proxy.tls"
        ),
        "expected an invalid proxy.tls, got {error:?}"
    );
}

#[test]
fn an_empty_tls_block_is_rejected() {
    let error = launched("  tls:\n").expect_err("an empty `proxy.tls` block says nothing");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "proxy.tls.ca_file"
        ),
        "expected an invalid proxy.tls.ca_file, got {error:?}"
    );
}

#[test]
fn a_tls_block_without_a_file_is_rejected() {
    for section in ["  tls:\n    ca_file:\n", "  tls:\n    ca_file: \"\"\n"] {
        let error = launched(section).expect_err("the block names no file");

        assert!(
            matches!(
                &error,
                BootstrapError::InvalidField { field, .. } if field == "proxy.tls.ca_file"
            ),
            "`{section}` must be reported as an invalid proxy.tls.ca_file, got {error:?}"
        );
    }
}

#[test]
fn an_unreadable_path_is_rejected() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let section = tls_block(&directory.path().join("absent.pem"));

    let error = launched(&section).expect_err("the file does not exist");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "proxy.tls.ca_file"
        ),
        "expected an invalid proxy.tls.ca_file, got {error:?}"
    );
    assert!(
        error.to_string().contains("could not be read"),
        "the reason must say what failed: {error}"
    );
}

#[test]
fn a_file_that_is_not_a_trust_root_set_is_rejected() {
    let (_directory, path) = pointing_at(
        "key.pem",
        "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49\n-----END PRIVATE KEY-----\n",
    );

    let error = launched(&tls_block(&path)).expect_err("a private key is not a trust anchor");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "proxy.tls.ca_file"
        ),
        "expected an invalid proxy.tls.ca_file, got {error:?}"
    );
}

#[test]
fn a_valid_bundle_becomes_the_trust_policy() {
    let (_directory, path) = pointing_at("roots.pem", TEST_ROOT);

    let bootstrap = launched(&tls_block(&path)).expect("a certificate is a usable trust root set");

    assert_eq!(
        bootstrap.http().tls(),
        &TlsPolicy::from_bootstrap(Some(path)),
        "the checked bundle is what the data plane is built with"
    );
}

#[test]
fn the_old_top_level_tls_section_is_an_unknown_field() {
    // The rename is not an alias: a file that still says `tls:` must fail loudly rather than have
    // its bundle silently ignored while the platform store stays in force.
    let (_directory, path) = pointing_at("roots.pem", TEST_ROOT);
    let text = format!(
        "{}tls:\n  ca_file: {}\n",
        document("./resources.yaml", "notify", 100, 500),
        path.display()
    );

    let error = load_document(&text).expect_err("the top level holds no `tls` section anymore");

    assert!(
        matches!(error, BootstrapError::Parse { .. }),
        "expected Parse, got {error:?}"
    );
}
