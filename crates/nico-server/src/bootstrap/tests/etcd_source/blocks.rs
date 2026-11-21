//! The two optional blocks of an etcd source: `tls` and `auth`.
//!
//! Each block is a closed shape of its own, and each field either reaches the client or fails
//! startup — there is no accepted-but-inert field here, because an operator who writes a trust root
//! or a credential believes the connection uses it. The trust material is read while the bootstrap
//! file is read, so a missing or unusable file names its own field; the password is not read at all,
//! only the reference, which is the whole point of it being a reference.
//!
//! See [`super`](super) for the document fixtures, the PEM files, and the scheme matrix these blocks
//! have to agree with.

use super::*;

#[test]
fn a_tls_block_with_an_identity_and_credentials_is_accepted() {
    let files = PemFiles::new();
    let body = format!(
        "{}    domain_name: localhost\n  auth:\n    user: nico\n    password_ref: env:ETCD_PASSWORD\n",
        files.with_identity()
    );
    let bootstrap = load_document(&etcd_body(
        "    - https://127.0.0.1:2379",
        "nico/config",
        &body,
    ))
    .expect("a complete transport and credential set is valid");

    let SourceSelection::Etcd { tls, auth, .. } = bootstrap.source() else {
        panic!("the document selects the etcd source");
    };
    let tls = tls.as_ref().expect("the `tls` block is carried");
    assert_eq!(tls.ca_file(), Path::new(&files.path("ca.pem")));
    let (cert_file, key_file) = tls.identity().expect("the identity is carried");
    assert_eq!(cert_file, Path::new(&files.path("client.pem")));
    assert_eq!(key_file, Path::new(&files.path("client.key")));
    assert_eq!(tls.domain_name(), Some("localhost"));
    let auth = auth.as_ref().expect("the `auth` block is carried");
    assert_eq!(auth.user(), "nico");
    assert_eq!(
        auth.password_ref(),
        "env:ETCD_PASSWORD",
        "the reference is carried and the password is not read here"
    );
}

#[test]
fn a_tls_block_without_a_ca_file_is_refused() {
    let reason = refused(
        &etcd_body(
            "    - https://127.0.0.1:2379",
            "nico/config",
            "  tls:\n    domain_name: localhost\n",
        ),
        "config_source.tls.ca_file",
    );
    assert!(
        reason.contains("platform store"),
        "the reason must say the host's store is not the answer, got {reason:?}"
    );
}

#[test]
fn a_ca_file_that_cannot_be_read_is_refused() {
    let files = PemFiles::new();
    let missing = files.directory.path().join("absent.pem");
    let body = format!("  tls:\n    ca_file: {}\n", missing.display());

    refused(
        &etcd_body("    - https://127.0.0.1:2379", "nico/config", &body),
        "config_source.tls.ca_file",
    );
}

#[test]
fn a_ca_file_that_is_not_a_certificate_is_refused() {
    let files = PemFiles::new();
    let body = format!("  tls:\n    ca_file: {}\n", files.path("garbage.pem"));
    let reason = refused(
        &etcd_body("    - https://127.0.0.1:2379", "nico/config", &body),
        "config_source.tls.ca_file",
    );

    assert!(
        !reason.contains("BEGIN CERTIFICATE"),
        "the refusal must not quote the file, got {reason:?}"
    );
}

#[test]
fn a_ca_file_above_the_bound_is_refused() {
    // A mis-pointed path must not load an arbitrary file into the process; the ceiling is the
    // bootstrap's constant, which the construction site re-applies on its second read.
    let files = PemFiles::new();
    let oversized = files.directory.path().join("huge.pem");
    std::fs::write(
        &oversized,
        vec![b'x'; crate::bootstrap::MAX_TLS_MATERIAL_BYTES + 1],
    )
    .expect("oversized write succeeds");
    let body = format!("  tls:\n    ca_file: {}\n", oversized.display());

    let reason = refused(
        &etcd_body("    - https://127.0.0.1:2379", "nico/config", &body),
        "config_source.tls.ca_file",
    );
    assert!(
        reason.contains("larger than the accepted bound"),
        "the reason must name the size bound, got {reason:?}"
    );
}

#[test]
fn a_half_written_identity_is_refused() {
    let files = PemFiles::new();
    for body in [
        format!(
            "  tls:\n    ca_file: {}\n    cert_file: {}\n",
            files.path("ca.pem"),
            files.path("client.pem")
        ),
        format!(
            "  tls:\n    ca_file: {}\n    key_file: {}\n",
            files.path("ca.pem"),
            files.path("client.key")
        ),
    ] {
        let reason = refused(
            &etcd_body("    - https://127.0.0.1:2379", "nico/config", &body),
            "config_source.tls.key_file",
        );
        assert!(
            reason.contains("cert_file"),
            "the reason must name the other half of the pair, got {reason:?}"
        );
    }
}

#[test]
fn an_identity_whose_key_does_not_match_is_refused() {
    let files = PemFiles::new();
    let body = format!(
        "  tls:\n    ca_file: {}\n    cert_file: {}\n    key_file: {}\n",
        files.path("ca.pem"),
        files.path("client.pem"),
        files.path("other.key")
    );
    let reason = refused(
        &etcd_body("    - https://127.0.0.1:2379", "nico/config", &body),
        "config_source.tls.key_file",
    );

    assert!(
        !reason.contains("PRIVATE KEY"),
        "the refusal must not quote the key, got {reason:?}"
    );
}

#[test]
fn an_unusable_domain_name_is_refused() {
    let files = PemFiles::new();
    for name in ["\"\"", "etcd.internal:2379", "https://etcd.internal"] {
        let body = format!("{}    domain_name: {name}\n", files.ca_only());
        refused(
            &etcd_body("    - https://127.0.0.1:2379", "nico/config", &body),
            "config_source.tls.domain_name",
        );
    }
}

#[test]
fn a_credentials_block_missing_its_halves_is_refused() {
    for (body, field) in [
        (
            "  auth:\n    password_ref: env:ETCD_PASSWORD\n",
            "config_source.auth.user",
        ),
        (
            "  auth:\n    user: nico\n",
            "config_source.auth.password_ref",
        ),
    ] {
        refused(&etcd_body(VALID_ENDPOINTS, "nico/config", body), field);
    }
}

#[test]
fn a_password_written_as_a_value_is_refused() {
    let reason = refused(
        &etcd_body(
            VALID_ENDPOINTS,
            "nico/config",
            "  auth:\n    user: nico\n    password_ref: s3cret\n",
        ),
        "config_source.auth.password_ref",
    );
    assert!(
        reason.contains("env:") && !reason.contains("s3cret"),
        "the reason must name the reference schemes and never echo the value, got {reason:?}"
    );
}

#[test]
fn an_empty_auth_block_is_refused_rather_than_ignored() {
    // `auth:` with no body is a written block: an operator who wrote it believes a credential is
    // configured, so it must name its missing field instead of quietly staying anonymous.
    refused(
        &etcd_body(VALID_ENDPOINTS, "nico/config", "  auth:\n"),
        "config_source.auth.user",
    );
}
