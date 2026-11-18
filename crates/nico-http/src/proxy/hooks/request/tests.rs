//! Unit tests for the per-connection transport decision behind `x-forwarded-proto`.
//!
//! The hook itself needs a live `Session`, so the digest branches are asserted here on values the
//! real connection layer produces the same shape of, and the end-to-end tests cover the wire
//! behaviour: a real TLS client's upstream sees `https`, a plaintext one sees `http`.

use std::sync::Arc;

use pingora::protocols::{Digest, tls::SslDigest};

use super::is_tls_connection;

/// A digest shaped like the one Pingora's TLS layer writes on a completed handshake.
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
    assert!(
        is_tls_connection(Some(&tls_digest())),
        "an `ssl_digest` means the handshake succeeded on this connection"
    );
}

#[test]
fn a_digest_without_tls_information_is_plaintext() {
    assert!(
        !is_tls_connection(Some(&Digest::default())),
        "a plaintext connection carries a digest with no TLS layer in it"
    );
}

#[test]
fn a_missing_digest_is_defended_as_plaintext() {
    assert!(
        !is_tls_connection(None),
        "a path without a digest must not be upgraded to https by accident"
    );
}
