//! Unit tests for the trust policy and its Pingora adapter.
//!
//! Everything here is bytes and values: no socket, no handshake, no resolver. The probe tests do
//! open the files they own, because "the bundle is read again" and "the bundle stopped resolving"
//! are facts about a path; that is the only file system this module touches.

use pingora::server::configuration::ServerConf;

use super::{
    BundleError, RootSource, TlsPolicy, TrustRootsError, apply, ensure_usable,
    require_platform_roots, validate_bundle,
};
use crate::test_support::{TEST_ROOT_PEM as ROOT_ONE, tls_bundle};

/// A second, unrelated self-signed certificate: a bundle is a *set*, not one file per root.
const ROOT_TWO: &str = "\
-----BEGIN CERTIFICATE-----
MIIBoDCCAUegAwIBAgIUZh6zrDH25c4eHv+pBbfHVZPwHEkwCgYIKoZIzj0EAwIw
HTEbMBkGA1UEAwwSbmljbyB0ZXN0IHJvb3QgdHdvMCAXDTI2MTExMTA3Mjg1OFoY
DzIxMjYxMDE4MDcyODU4WjAdMRswGQYDVQQDDBJuaWNvIHRlc3Qgcm9vdCB0d28w
WTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAAQFce4FVMEwaR2ZY2PXLubO7/gvOS4c
fz8HCTom537JcQhhwDRPMdfhW1w7YzadMb2wC9xoCMVRxvA2HnysKSTlo2MwYTAd
BgNVHQ4EFgQUkydKPj9I3x1DlHCpflRtlAne+hAwHwYDVR0jBBgwFoAUkydKPj9I
3x1DlHCpflRtlAne+hAwDwYDVR0TAQH/BAUwAwEB/zAOBgNVHQ8BAf8EBAMCAgQw
CgYIKoZIzj0EAwIDRwAwRAIgPBbXqy8FzgUDwTVUCEhg6o91nH24ctxnH3qDgmjV
uaICICn134p4IUzDLdeUhiqokC5uo5CZ1cIX1+wr+dEP0gGh
-----END CERTIFICATE-----
";

/// A PKCS#8 private key: a well-formed PEM file that is not a trust anchor.
const PRIVATE_KEY: &str = "\
-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgIwCECYkzvYUi6iiD
wbUIAodT01KTy5T49r6pT6THrrahRANCAARUW8ZyKse5cT4emOomnnXp/E28o1yN
Zmlodpe9IBhQQCLUR0Gl3dMZJixVwfQ0vpI1aYu8dMCiR2t6ItRXxmzt
-----END PRIVATE KEY-----
";

/// A section that starts like a certificate and is not one.
const NOT_A_CERTIFICATE: &str = "\
-----BEGIN CERTIFICATE-----
aGVsbG8gd29ybGQ=
-----END CERTIFICATE-----
";

mod bundle {
    use super::*;

    /// Validates `bytes` and returns why they were refused.
    fn refusal(bytes: &[u8]) -> BundleError {
        validate_bundle(bytes).expect_err("the fixture must not be a usable trust root set")
    }

    #[test]
    fn an_empty_file_is_not_a_trust_root_set() {
        // Accepting it would configure the client with an empty store: every handshake then fails
        // as a certificate error that names no configuration field.
        assert!(
            matches!(refusal(b""), BundleError::Empty),
            "expected Empty, got {:?}",
            refusal(b"")
        );
        assert!(
            matches!(refusal(b"\n\n"), BundleError::Empty),
            "whitespace holds no PEM section either"
        );
    }

    #[test]
    fn a_file_that_holds_only_a_private_key_is_refused() {
        let error = refusal(PRIVATE_KEY.as_bytes());

        assert!(
            matches!(error, BundleError::NotACertificate { kind } if kind == "private key"),
            "expected NotACertificate(private key), got {error:?}"
        );
        assert!(
            error.to_string().contains("private key"),
            "the operator has to learn which section is wrong: {error}"
        );
    }

    #[test]
    fn one_certificate_is_a_usable_trust_root_set() {
        assert!(
            validate_bundle(ROOT_ONE.as_bytes()).is_ok(),
            "a single root is a complete set: {:?}",
            validate_bundle(ROOT_ONE.as_bytes()).unwrap_err()
        );
    }

    #[test]
    fn several_certificates_are_a_usable_trust_root_set() {
        let bundle = format!("{ROOT_ONE}{ROOT_TWO}");

        assert!(
            validate_bundle(bundle.as_bytes()).is_ok(),
            "a bundle replaces the platform store, so two roots are one file with both: {:?}",
            validate_bundle(bundle.as_bytes()).unwrap_err()
        );
    }

    #[test]
    fn a_malformed_section_after_a_valid_certificate_is_refused() {
        // The first certificate parses; the second block cannot be decoded. A check that stopped at
        // the first success would hand the client a file it aborts on.
        let bundle = format!(
            "{ROOT_ONE}-----BEGIN CERTIFICATE-----\nnot base64!\n-----END CERTIFICATE-----\n"
        );
        let error = refusal(bundle.as_bytes());

        assert!(
            matches!(error, BundleError::Malformed { .. }),
            "expected Malformed, got {error:?}"
        );
    }

    #[test]
    fn a_certificate_shaped_section_that_is_not_a_certificate_is_refused() {
        let error = refusal(NOT_A_CERTIFICATE.as_bytes());

        assert!(
            matches!(error, BundleError::UnusableCertificate { .. }),
            "expected UnusableCertificate, got {error:?}"
        );
        assert!(
            std::error::Error::source(&error).is_some(),
            "the client's own parse diagnostic is kept for the log"
        );
    }
}

mod policy {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn an_omitted_ca_file_uses_the_platform_roots() {
        assert_eq!(
            TlsPolicy::from_bootstrap(None).root_source(),
            &RootSource::Platform
        );
        assert_eq!(
            TlsPolicy::default().root_source(),
            &RootSource::Platform,
            "an omitted `tls` section is the default, and the default is the platform store"
        );
    }

    #[test]
    fn a_written_ca_file_replaces_the_platform_roots() {
        let path = PathBuf::from("/etc/nico/roots.pem");
        let policy = TlsPolicy::from_bootstrap(Some(path.clone()));

        assert_eq!(
            policy.root_source(),
            &RootSource::Bundle(path),
            "one root source, and a bundle is the whole of it — there is nothing to append to"
        );
    }
}

mod probe {
    use rustls::pki_types::CertificateDer;

    use super::*;

    /// Returns the DER the PEM in `pem` holds, which is the shape the platform store yields.
    fn entry(pem: &str) -> CertificateDer<'static> {
        let mut reader = pem.as_bytes();
        rustls_pemfile::certs(&mut reader)
            .next()
            .expect("the fixture holds one certificate")
            .expect("the fixture certificate is valid DER")
    }

    #[test]
    fn an_empty_platform_store_is_refused() {
        // An empty store is not "trust everything": every handshake would fail later, naming no
        // configuration field, so the process must not be allowed to start with one.
        assert!(
            matches!(
                require_platform_roots(&[]),
                Err(TrustRootsError::PlatformEmpty)
            ),
            "expected PlatformEmpty, got {:?}",
            require_platform_roots(&[])
        );
    }

    #[test]
    fn one_platform_certificate_is_enough() {
        assert!(
            require_platform_roots(&[entry(ROOT_ONE)]).is_ok(),
            "a store with a loadable certificate in it is usable: {:?}",
            require_platform_roots(&[entry(ROOT_ONE)]).unwrap_err()
        );
    }

    #[test]
    fn a_platform_entry_the_client_cannot_load_is_refused() {
        // Readable and non-empty is not the connector's rule: it `add`s every entry to its root
        // store and `unwrap()`s the result, so an entry that does not parse aborts just as hard as
        // an empty store would have failed later.
        let error = require_platform_roots(&[CertificateDer::from(vec![1_u8, 2, 3])])
            .expect_err("three bytes are not an X.509 certificate");

        assert!(
            matches!(error, TrustRootsError::PlatformUnusable { .. }),
            "expected PlatformUnusable, got {error:?}"
        );
        assert!(
            std::error::Error::source(&error).is_some(),
            "the client's own parse diagnostic is kept for the log"
        );
    }

    #[test]
    fn a_bundle_is_read_again_when_the_process_is_assembled() {
        let policy = TlsPolicy::from_bootstrap(Some(tls_bundle().to_owned()));

        assert!(
            ensure_usable(&policy).is_ok(),
            "the fixture bundle is a usable root set: {:?}",
            ensure_usable(&policy).unwrap_err()
        );
    }

    #[test]
    fn a_bundle_that_disappeared_after_bootstrap_is_refused() {
        // The connector opens its own copy of the file while the service is built, so a path that
        // no longer resolves has to fail here rather than inside Pingora's `unwrap()`.
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let path = directory.path().join("roots.pem");
        let policy = TlsPolicy::from_bootstrap(Some(path.clone()));

        let error = ensure_usable(&policy).expect_err("the file does not exist");

        assert!(
            matches!(&error, TrustRootsError::BundleUnreadable { path: named, .. } if named == &path),
            "expected BundleUnreadable for {path:?}, got {error:?}"
        );
        assert!(
            error.to_string().contains("could not be read"),
            "the message must say what failed: {error}"
        );
    }

    #[test]
    fn a_bundle_that_is_not_a_trust_root_set_is_refused_at_assembly_time() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let path = directory.path().join("roots.pem");
        std::fs::write(&path, PRIVATE_KEY).expect("the fixture is written");
        let policy = TlsPolicy::from_bootstrap(Some(path));

        let error = ensure_usable(&policy).expect_err("a private key is not a trust anchor");

        assert!(
            matches!(
                error,
                TrustRootsError::Bundle(BundleError::NotACertificate { .. })
            ),
            "expected Bundle(NotACertificate), got {error:?}"
        );
    }
}

mod adapter {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn a_bundle_reaches_the_server_configuration_as_a_path() {
        let path = PathBuf::from("/etc/nico/roots.pem");
        let mut conf = ServerConf::default();

        apply(&TlsPolicy::from_bootstrap(Some(path)), &mut conf);

        assert_eq!(
            conf.ca_file.as_deref(),
            Some("/etc/nico/roots.pem"),
            "the field Pingora's connector reads must carry the configured bundle"
        );
    }

    #[test]
    fn the_platform_store_leaves_the_configuration_untouched() {
        let mut conf = ServerConf::default();

        apply(&TlsPolicy::default(), &mut conf);

        assert_eq!(
            conf.ca_file, None,
            "no `ca_file` is how Pingora is told to read the platform store"
        );
    }
}
