//! The identity an inbound TLS listener presents, and the checks that keep a bad one from reaching
//! Pingora.
//!
//! Pingora's `TlsSettings::build` loads the certificate and key itself and **panics** when they are
//! unreadable or do not match. A startup path must not rely on that: the bootstrap file is validated
//! before the listener is bound, and a file that changes between that check and the build is refused
//! by the same check in [`service`](crate::service::service). The values here are therefore the one place that
//! turns PEM bytes into the `CertifiedKey` Pingora hands to rustls.
//!
//! No type in this module prints certificate or key material. The paths are configuration (and are
//! printed); the bytes and the key are not.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use rustls::{
    ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer},
    server::ResolvesServerCert,
    sign::CertifiedKey,
};

/// Where a TLS listener's certificate chain and private key live.
///
/// The values are paths, not bytes: the bootstrap file names files, and the data plane reads them
/// once when it builds the listener. Keeping paths in the settings type keeps `HttpSettings` a plain
/// value even though the bytes are a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundTls {
    cert_file: PathBuf,
    key_file: PathBuf,
}

impl InboundTls {
    /// Wraps the two paths a bootstrap file wrote.
    #[must_use]
    pub fn from_bootstrap(cert_file: PathBuf, key_file: PathBuf) -> Self {
        Self {
            cert_file,
            key_file,
        }
    }

    /// Returns the certificate-chain path.
    #[must_use]
    pub fn cert_file(&self) -> &Path {
        &self.cert_file
    }

    /// Returns the private-key path.
    #[must_use]
    pub fn key_file(&self) -> &Path {
        &self.key_file
    }
}

/// Why a certificate chain and private key cannot serve a TLS listener.
///
/// Every variant is a startup failure. The message names the rule or the field, never a PEM section
/// or a key byte: a certificate is public, but a private key is not, and one error type covers both.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServerIdentityError {
    /// The certificate PEM could not be decoded.
    #[error("the certificate chain cannot be read: {source}")]
    CertificatePem {
        /// The PEM decoder's diagnostic.
        #[source]
        source: io::Error,
    },
    /// The certificate PEM held no certificate.
    #[error("the certificate chain contains no certificate")]
    EmptyCertificates,
    /// The private-key PEM could not be decoded.
    #[error("the private key cannot be read: {source}")]
    KeyPem {
        /// The PEM decoder's diagnostic.
        #[source]
        source: io::Error,
    },
    /// The private-key PEM held no key.
    #[error("the private key is missing")]
    MissingKey,
    /// The key type is not one rustls can sign with.
    #[error("the private key type is not supported: {source}")]
    UnsupportedKey {
        /// rustls' own diagnostic.
        #[source]
        source: rustls::Error,
    },
    /// The certificate chain and the key do not form one identity.
    #[error("the certificate chain and private key do not match: {source}")]
    Mismatch {
        /// rustls' own pairing diagnostic.
        #[source]
        source: rustls::Error,
    },
}

/// Checks that `cert_pem` and `key_pem` are a usable server identity.
///
/// # Errors
/// Returns [`ServerIdentityError`] when either PEM cannot be decoded, the chain is empty, the key is
/// missing or unsupported, or the pair does not match.
pub fn validate_server_identity(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> Result<(), ServerIdentityError> {
    // `ServerConfig::builder` reads the process-wide crypto provider. Pingora installs the same
    // provider when it builds the acceptor; installing it here makes the bootstrap check
    // independent of whether the service has been built yet.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (certificates, key) = parse(cert_pem, key_pem)?;
    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .map(|_config| ())
        .map_err(|source| ServerIdentityError::Mismatch { source })
}

/// Builds the signing identity Pingora's rustls acceptor uses.
///
/// The bytes must already have passed [`validate_server_identity`]; this function repeats the parse
/// because `with_single_cert` consumes the key, but it does not repeat the pairing check. The caller
/// in [`service`](crate::service::service) performs both, so a mismatch cannot reach a handshake.
///
/// # Errors
/// Returns the same parse/key errors as [`validate_server_identity`], plus
/// [`ServerIdentityError::UnsupportedKey`].
pub(crate) fn certified_key(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> Result<Arc<CertifiedKey>, ServerIdentityError> {
    let (certificates, key) = parse(cert_pem, key_pem)?;
    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|source| ServerIdentityError::UnsupportedKey { source })?;
    Ok(Arc::new(CertifiedKey::new(certificates, signing_key)))
}

/// Returns the certificate chain and private key one PEM pair holds.
fn parse(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), ServerIdentityError> {
    let mut cert_reader = cert_pem;
    let certificates = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| ServerIdentityError::CertificatePem { source })?;
    if certificates.is_empty() {
        return Err(ServerIdentityError::EmptyCertificates);
    }

    let mut key_reader = key_pem;
    let key = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|source| ServerIdentityError::KeyPem { source })?
        .ok_or(ServerIdentityError::MissingKey)?;
    Ok((certificates, key))
}

/// Reads one identity pair from the paths a bootstrap file named.
///
/// # Errors
/// Returns the read failure with the field name the caller can report; the bytes are never part of
/// the error.
pub(crate) fn read_identity(
    identity: &InboundTls,
) -> Result<(Vec<u8>, Vec<u8>), IdentityReadError> {
    let certificate = fs::read(identity.cert_file())
        .map_err(|source| IdentityReadError::Certificate { source })?;
    let key = fs::read(identity.key_file()).map_err(|source| IdentityReadError::Key { source })?;
    Ok((certificate, key))
}

/// Why one of the two identity files could not be read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum IdentityReadError {
    /// The certificate file could not be read.
    #[error("the certificate file cannot be read: {source}")]
    Certificate {
        /// The file-system error.
        #[source]
        source: io::Error,
    },
    /// The private-key file could not be read.
    #[error("the private-key file cannot be read: {source}")]
    Key {
        /// The file-system error.
        #[source]
        source: io::Error,
    },
}

/// One certificate resolver for the lifetime of the listener.
///
/// The resolver returns the one identity the process was configured with, ignoring SNI. A future
/// multi-certificate feature replaces this value, not the listener plumbing.
#[derive(Debug)]
pub(crate) struct StaticCertResolver {
    key: Arc<CertifiedKey>,
}

impl StaticCertResolver {
    /// Wraps the one identity this listener presents.
    pub(crate) fn new(key: Arc<CertifiedKey>) -> Self {
        Self { key }
    }
}

impl ResolvesServerCert for StaticCertResolver {
    fn resolve(&self, _client_hello: rustls::server::ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(&self.key))
    }
}

impl ServerIdentityError {
    /// Returns the bootstrap field this failure belongs to.
    ///
    /// A certificate parse failure is the certificate file; a key parse, unsupported key or pair
    /// mismatch is the private-key file. The split lets the bootstrap name the file an operator has
    /// to fix without guessing from a string.
    #[must_use]
    pub const fn field(&self) -> &'static str {
        match self {
            Self::CertificatePem { .. } | Self::EmptyCertificates => "cert_file",
            Self::KeyPem { .. }
            | Self::MissingKey
            | Self::UnsupportedKey { .. }
            | Self::Mismatch { .. } => "key_file",
        }
    }
}

impl fmt::Display for InboundTls {
    /// Prints both paths; they are configuration, not key material.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "certificate {} and key {}",
            self.cert_file.display(),
            self.key_file.display()
        )
    }
}

// Compiled-in proof that a refusal crosses the startup assignments that store it.
const _: fn() = || {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<ServerIdentityError>();
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_certificate_is_refused() {
        let error = validate_server_identity(b"", b"").expect_err("no identity");
        assert!(matches!(error, ServerIdentityError::EmptyCertificates));
    }

    #[test]
    fn malformed_pem_names_the_certificate_side() {
        let error = validate_server_identity(b"not a pem", b"not a key").expect_err("bad identity");
        assert!(matches!(
            error,
            ServerIdentityError::CertificatePem { .. } | ServerIdentityError::EmptyCertificates
        ));
    }

    #[test]
    fn a_certificate_without_a_key_is_refused() {
        let certificate = include_bytes!("../../nico-server/tests/fixtures/tls/localhost.pem");
        let error = validate_server_identity(certificate, b"").expect_err("no key");
        assert!(matches!(
            error,
            ServerIdentityError::KeyPem { .. } | ServerIdentityError::MissingKey
        ));
    }

    #[test]
    fn the_paths_are_visible_but_not_the_bytes() {
        let identity = InboundTls::from_bootstrap(
            PathBuf::from("/etc/nico/fullchain.pem"),
            PathBuf::from("/etc/nico/key.pem"),
        );
        assert_eq!(identity.cert_file(), Path::new("/etc/nico/fullchain.pem"));
        assert_eq!(identity.key_file(), Path::new("/etc/nico/key.pem"));
        let rendered = format!("{identity}");
        assert!(!rendered.contains("BEGIN"));
    }
}
