//! The second read: what a bootstrap that validated paths can still fail on when the source is
//! built.
//!
//! Every case here is a file the bootstrap accepted and that changed — or a reference whose
//! value is absent — so each one asserts the field name the operator has to fix, and none of them
//! touches the network: [`connect_options`] runs before any connection is attempted.
//!
//! Two of them assert the other half of the contract: the *value* reaches the client exactly as the
//! source held it. That rule has no field name to fail under, so it is pinned here rather than
//! discovered as an authentication failure on a wire.

use std::path::{Path, PathBuf};

use nico_core::ErrorCategory;

use super::*;

/// The workspace's shared TLS fixtures, above any one crate — see `fixtures/tls/README.md`.
///
/// The trust root the fixtures chain to.
const TEST_ROOT_PEM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/tls/test-root.pem"
));
/// The leaf certificate a client identity is built from.
const LOCALHOST_PEM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/tls/localhost.pem"
));
/// The private key of [`LOCALHOST_PEM`].
const LOCALHOST_KEY: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/tls/localhost.key"
));
/// A private key that belongs to no certificate above.
const OTHER_ROOT_KEY: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/tls/other-root.key"
));

/// A temporary directory holding the PEM files a block names.
struct PemFiles(tempfile::TempDir);

impl PemFiles {
    /// Writes the trust root and the loopback identity.
    fn new() -> Self {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        for (name, bytes) in [
            ("ca.pem", TEST_ROOT_PEM),
            ("client.pem", LOCALHOST_PEM),
            ("client.key", LOCALHOST_KEY),
        ] {
            std::fs::write(directory.path().join(name), bytes).expect("fixture write succeeds");
        }
        Self(directory)
    }

    /// Returns one written file's path.
    fn path(&self, name: &str) -> PathBuf {
        self.0.path().join(name)
    }
}

/// A `tls` block naming `files`' trust root and identity.
fn tls(files: &PemFiles) -> EtcdTls {
    EtcdTls::new(
        files.path("ca.pem"),
        Some((files.path("client.pem"), files.path("client.key"))),
        None,
    )
}

/// Asserts the build failed under `field`, and returns the reason it gave.
fn refused(result: Result<EtcdConnectOptions, SourceBuildError>, field: &str) -> String {
    match result {
        Ok(_) => panic!("expected the build to be refused under `{field}`"),
        Err(SourceBuildError::Setting {
            field: found,
            reason,
        }) => {
            assert_eq!(
                found, field,
                "the refusal must name the field that is wrong"
            );
            assert_eq!(
                SourceBuildError::Setting {
                    field: found,
                    reason: reason.clone(),
                }
                .category(),
                ErrorCategory::Client,
                "a field this process cannot honour is the bootstrap file's mistake"
            );
            reason.to_string()
        }
        Err(other) => panic!("expected Setting under `{field}`, got {other:?}"),
    }
}

#[test]
fn a_ca_file_that_vanished_after_the_check_names_its_field() {
    let files = PemFiles::new();
    let missing = EtcdTls::new(files.path("absent.pem"), None, None);

    refused(connect_options(Some(&missing), None), CA_FILE);
}

#[test]
fn a_ca_file_replaced_after_the_check_names_its_field() {
    let files = PemFiles::new();
    let tls = tls(&files);
    std::fs::write(files.path("ca.pem"), b"no longer a bundle").expect("swap succeeds");

    refused(connect_options(Some(&tls), None), CA_FILE);
}

#[test]
fn a_ca_file_above_the_bound_names_its_field() {
    // The second read applies the same ceiling the bootstrap did, so a file that passed the
    // first read cannot fail this one on size — the bound here is the bootstrap's constant,
    // not a second number that can drift.
    let files = PemFiles::new();
    let tls = tls(&files);
    std::fs::write(files.path("ca.pem"), vec![b'x'; MAX_TLS_MATERIAL_BYTES + 1])
        .expect("oversized write succeeds");

    refused(connect_options(Some(&tls), None), CA_FILE);
}

#[test]
fn an_identity_replaced_after_the_check_names_its_field() {
    let files = PemFiles::new();
    let tls = tls(&files);
    std::fs::write(files.path("client.key"), OTHER_ROOT_KEY).expect("swap succeeds");

    let reason = refused(connect_options(Some(&tls), None), KEY_FILE);
    assert!(
        !reason.contains("PRIVATE KEY"),
        "the refusal must not quote the key, got {reason:?}"
    );
}

#[test]
fn an_unset_password_variable_names_the_reference() {
    let auth = EtcdAuth::new(
        "nico".to_owned(),
        "env:NICO_TEST_ETCD_PASSWORD_UNSET".to_owned(),
    );

    refused(connect_options(None, Some(&auth)), PASSWORD_REF);
}

#[test]
fn a_password_file_that_cannot_be_read_names_the_reference() {
    let auth = EtcdAuth::new(
        "nico".to_owned(),
        format!("file:{}", Path::new("absent-password").display()),
    );

    refused(connect_options(None, Some(&auth)), PASSWORD_REF);
}

#[test]
fn a_password_file_is_read_byte_for_byte() {
    // `echo s3cret > password` leaves the newline behind, and every other etcd client sends it.
    // A reader that trimmed or normalised it would be the one client presenting a different
    // secret than the file holds — and only against the real cluster would that show up.
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let path = directory.path().join("password");
    std::fs::write(&path, b"s3cret\n").expect("password write succeeds");

    let resolved =
        resolve(&format!("file:{}", path.display())).expect("a readable password file resolves");

    assert_eq!(
        resolved.as_bytes(),
        b"s3cret\n",
        "the newline is part of the password, not formatting"
    );
}

#[test]
fn an_empty_password_file_is_refused_rather_than_sent() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let path = directory.path().join("password");
    std::fs::write(&path, b"").expect("password write succeeds");
    let auth = EtcdAuth::new("nico".to_owned(), format!("file:{}", path.display()));

    let reason = refused(connect_options(None, Some(&auth)), PASSWORD_REF);
    assert!(
        reason.contains("empty"),
        "the reason must say the value is empty, got {reason:?}"
    );
}

#[test]
fn a_password_above_the_bound_is_refused_wherever_it_came_from() {
    // An `env:` reference passes no file read, so the rule both sources end at is what bounds
    // it — the same ceiling a `file:` value meets at its read, applied at exactly the constant.
    let error = accepted("x".repeat(MAX_PASSWORD_BYTES + 1))
        .expect_err("a value larger than the bound is not a password");

    match error {
        SourceBuildError::Setting { field, reason } => {
            assert_eq!(field, PASSWORD_REF, "the refusal names the reference");
            assert!(
                reason.contains("larger than the accepted bound"),
                "the reason must name the size bound, got {reason:?}"
            );
        }
        other => panic!("expected Setting, got {other:?}"),
    }
    assert!(
        accepted("x".repeat(MAX_PASSWORD_BYTES)).is_ok(),
        "a value at the bound is still a password"
    );
}

#[test]
fn a_password_written_as_a_value_is_refused_rather_than_guessed() {
    // The bootstrap validates the scheme, so this can only arrive if the section changed under
    // this crate; the answer is a named field, never "send the text as a password".
    let auth = EtcdAuth::new("nico".to_owned(), "s3cret".to_owned());

    let reason = refused(connect_options(None, Some(&auth)), PASSWORD_REF);
    assert!(
        !reason.contains("s3cret"),
        "the refusal must never echo the value, got {reason:?}"
    );
}

#[test]
fn a_complete_block_becomes_connection_options() {
    let files = PemFiles::new();
    let tls = EtcdTls::new(
        files.path("ca.pem"),
        Some((files.path("client.pem"), files.path("client.key"))),
        Some("localhost".to_owned()),
    );
    let auth = EtcdAuth::new(
        "nico".to_owned(),
        "env:NICO_TEST_ETCD_PASSWORD_UNSET".to_owned(),
    );

    assert!(
        connect_options(Some(&tls), None).is_ok(),
        "a complete transport block is enough on its own: credentials are independent"
    );
    // The credential half is the negative case above: a reference whose value is absent fails
    // the build rather than connecting anonymously.
    refused(connect_options(Some(&tls), Some(&auth)), PASSWORD_REF);
}
