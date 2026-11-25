//! Unit tests for the per-connection transport decision behind `x-forwarded-proto`.

use std::sync::Arc;

use pingora::protocols::{Digest, tls::SslDigest};

use super::is_tls_connection;

fn tls_digest() -> Digest {
    Digest {
        ssl_digest: Some(Arc::new(SslDigest::new(
            "TLS_AES_128_GCM_SHA256",
            "TLSv1.3",
            None,
            None,
            Vec::new(),
        ))),
        ..Digest::default()
    }
}

#[test]
fn a_digest_with_tls_information_is_a_tls_connection() {
    assert!(is_tls_connection(Some(&tls_digest())));
}

#[test]
fn a_digest_without_tls_information_is_plaintext() {
    assert!(!is_tls_connection(Some(&Digest::default())));
}

#[test]
fn a_missing_digest_is_defended_as_plaintext() {
    assert!(!is_tls_connection(None));
}
