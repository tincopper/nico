//! The optional `tls` section: the trust roots a process is built with.
//!
//! Kept in its own file for the reason its parent states: the fixtures plus the startup
//! outcomes are longer than the module they exercise, and the 500-line budget applies to the
//! parent file.

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

/// Appends a `tls` section to an otherwise complete bootstrap document.
fn with_tls(section: &str) -> String {
    format!(
        "{}tls:\n{section}",
        document("./resources.yaml", "notify", 100, 500)
    )
}

/// Loads a document whose `tls` section holds `section`.
fn launched(section: &str) -> Result<BootstrapConfig, BootstrapError> {
    load_document(&with_tls(section))
}

/// Writes `contents` into a fresh file and returns the path plus the document pointing at it.
fn pointing_at(name: &str, contents: &str) -> (tempfile::TempDir, PathBuf, String) {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let path = directory.path().join(name);
    std::fs::write(&path, contents).expect("fixture write succeeds");
    let section = format!("  ca_file: {}\n", path.display());
    (directory, path, section)
}

#[test]
fn an_omitted_section_uses_the_platform_roots() {
    let bootstrap = load_document(&document("./resources.yaml", "notify", 100, 500))
        .expect("the document is valid");

    assert_eq!(
        bootstrap.http(),
        &nico_http::HttpSettings::default(),
        "no `tls` section must be the defaults, exactly as before the section existed"
    );
    assert_eq!(
        bootstrap.http().tls(),
        &nico_http::TlsPolicy::default(),
        "and the default trust roots are the platform store"
    );
}

#[test]
fn an_empty_section_is_rejected() {
    // `tls:` with no body would otherwise read as "no section" and silently keep the platform
    // roots, which is the opposite of what an operator who wrote the key intended.
    let error = launched("").expect_err("an empty `tls` section says nothing");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "tls.ca_file"
        ),
        "expected an invalid tls.ca_file, got {error:?}"
    );
}

#[test]
fn a_section_without_a_file_is_rejected() {
    for section in ["  ca_file:\n", "  ca_file: \"\"\n"] {
        let error = launched(section).expect_err("the section names no file");

        assert!(
            matches!(
                &error,
                BootstrapError::InvalidField { field, .. } if field == "tls.ca_file"
            ),
            "`{section}` must be reported as an invalid tls.ca_file, got {error:?}"
        );
    }
}

#[test]
fn an_unreadable_path_is_rejected() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let section = format!(
        "  ca_file: {}\n",
        directory.path().join("absent.pem").display()
    );

    let error = launched(&section).expect_err("the file does not exist");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "tls.ca_file"
        ),
        "expected an invalid tls.ca_file, got {error:?}"
    );
    assert!(
        error.to_string().contains("could not be read"),
        "the reason must say what failed: {error}"
    );
}

#[test]
fn a_file_that_is_not_a_trust_root_set_is_rejected() {
    let (_directory, _path, section) = pointing_at(
        "key.pem",
        "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49\n-----END PRIVATE KEY-----\n",
    );

    let error = launched(&section).expect_err("a private key is not a trust anchor");

    assert!(
        matches!(
            &error,
            BootstrapError::InvalidField { field, .. } if field == "tls.ca_file"
        ),
        "expected an invalid tls.ca_file, got {error:?}"
    );
}

#[test]
fn a_valid_bundle_becomes_the_trust_policy() {
    let (_directory, path, section) = pointing_at("roots.pem", TEST_ROOT);

    let bootstrap = launched(&section).expect("a certificate is a usable trust root set");

    assert_eq!(
        bootstrap.http().tls(),
        &nico_http::TlsPolicy::from_bootstrap(Some(path)),
        "the checked bundle is what the data plane is built with"
    );
}
