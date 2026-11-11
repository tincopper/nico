//! What an upstream TLS handshake trusts, stated without naming the framework that performs it.
//!
//! This file is the unit a future `nico-tls` crate would absorb, so it deliberately names no
//! framework type: the values here describe *policy* (which roots, and whether a bundle is one), and
//! `adapter.rs` is what turns them into a framework configuration. Nothing here does I/O either —
//! [`validate_bundle`] takes the bytes a caller already read — which is what makes every failure
//! mode testable without a file system.
//!
//! Two facts about trust anchors are easy to get wrong, so they are stated here:
//!
//! - **A bundle replaces the platform store; it does not extend it.** That is rustls' semantics, not
//!   a choice made here: a configured `ca_file` is used *instead of* the native roots. An operator
//!   that needs both concatenates them into one file.
//! - **An empty store is not "trust everything".** It is "trust nothing", so every handshake would
//!   fail at request time with a certificate error that names no configuration field. A bundle with
//!   no certificate is therefore refused at startup, not accepted and discovered later.

use std::{io, path::PathBuf};

use rustls::RootCertStore;
use rustls_pemfile::Item;

/// Where the trust roots come from.
///
/// The two branches are mutually exclusive on purpose: a process that trusted "the platform plus a
/// bundle" would answer "what is trusted here?" with a set no single file describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RootSource {
    /// The platform certificate store, including whatever `SSL_CERT_FILE` / `SSL_CERT_DIR` select.
    ///
    /// Reading those variables is the TLS provider's job, which is why nothing in this crate reads
    /// them: it is a deployment mechanism, not a field of the gateway's own configuration.
    Platform,
    /// One complete trust-root set, read once at startup, that replaces the platform store.
    Bundle(PathBuf),
}

/// The TLS facts a process is configured with.
///
/// Today the only fact is the root source. It is a struct rather than a bare `Option<PathBuf>`
/// because the same value will carry a client certificate and a protocol-version range when those
/// arrive, and because [`TlsPolicy`] is what a settings value holds — an `Option<PathBuf>` there
/// would make "no bundle" and "a bundle nobody validated" the same shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsPolicy {
    roots: RootSource,
}

impl TlsPolicy {
    /// Builds the policy a bootstrap file describes.
    ///
    /// A `Some` path must be a usable bundle, and asserting that means reading the file — which is
    /// not this layer's job, so the check is [`validate_bundle`] and the read belongs to whoever
    /// calls it. The bootstrap section is that caller: it reads the file, validates the bytes, and
    /// only then builds a policy, so the value that reaches the data plane was checked. This
    /// constructor records the fact; it cannot re-derive it without doing I/O.
    #[must_use]
    pub fn from_bootstrap(ca_file: Option<PathBuf>) -> Self {
        Self {
            roots: ca_file.map_or(RootSource::Platform, RootSource::Bundle),
        }
    }

    /// Returns where this policy takes its trust roots from.
    pub(crate) fn root_source(&self) -> &RootSource {
        &self.roots
    }
}

impl Default for TlsPolicy {
    /// Returns the policy an omitted `tls` section stands for: the platform store.
    fn default() -> Self {
        Self::from_bootstrap(None)
    }
}

/// Reports whether `bytes` is a bundle of certificates a TLS client can use as its roots.
///
/// The parse is **the one the TLS client itself performs**: every PEM item is read with
/// [`rustls_pemfile::read_all`] and each certificate is added to a [`RootCertStore`]. Running the
/// identical path is the point — a stricter or looser check would either reject a file the client
/// accepts or accept one it aborts on, and the client aborts by `unwrap()`ing the result while the
/// service is being built, i.e. a panic with no field name and no clean exit.
///
/// One thing this check asserts beyond the client's own parse: a bundle must hold **at least one**
/// certificate. The client accepts an empty store, and then reports every handshake as a
/// verification failure that points at no configuration field.
///
/// # Errors
/// - [`BundleError::Empty`] when no PEM section is present at all.
/// - [`BundleError::Malformed`] when a section cannot be decoded (a missing end marker, an illegal
///   start line, invalid base64).
/// - [`BundleError::NotACertificate`] when a section is a key, a CRL or a CSR: a file the client
///   rejects as a CA file.
/// - [`BundleError::UnusableCertificate`] when a certificate does not parse as an X.509 certificate.
pub fn validate_bundle(bytes: &[u8]) -> Result<(), BundleError> {
    let mut reader = bytes;
    let mut store = RootCertStore::empty();
    let mut certificates = 0usize;

    for item in rustls_pemfile::read_all(&mut reader) {
        let certificate = match item.map_err(|source| BundleError::Malformed { source })? {
            Item::X509Certificate(certificate) => certificate,
            other => {
                return Err(BundleError::NotACertificate { kind: kind(&other) });
            }
        };
        store
            .add(certificate)
            .map_err(|source| BundleError::UnusableCertificate { source })?;
        certificates += 1;
    }

    if certificates == 0 {
        return Err(BundleError::Empty);
    }
    Ok(())
}

/// Names the kind of PEM section that is not a trust anchor, for the refusal message.
///
/// The wildcard arm is a label, not a decision: a section this crate has never heard of is equally
/// not a certificate, and refusing it is what the TLS client does with it too.
fn kind(item: &Item) -> &'static str {
    match item {
        Item::X509Certificate(_) => "certificate",
        Item::Pkcs1Key(_) | Item::Pkcs8Key(_) | Item::Sec1Key(_) => "private key",
        Item::SubjectPublicKeyInfo(_) => "public key",
        Item::Crl(_) => "certificate revocation list",
        Item::Csr(_) => "certificate signing request",
        _ => "PEM",
    }
}

/// A bundle of bytes that is not a usable set of trust roots.
///
/// Every variant means the same thing to a caller — refuse the process configuration — so they are
/// not distinguished by an action. They are distinguished by what an operator has to fix, which is
/// the only reason a startup failure is worth naming a field in the first place.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BundleError {
    /// No PEM section was found, so the bundle would produce an empty trust store.
    #[error("the file contains no certificate")]
    Empty,
    /// A PEM section could not be decoded.
    #[error("a section of the file cannot be read: {source}")]
    Malformed {
        /// The decoder's diagnostic (a missing end marker, an illegal start line, bad base64).
        #[source]
        source: io::Error,
    },
    /// A PEM section is not a certificate.
    #[error("the file contains a {kind} section, which is not a trust anchor")]
    NotACertificate {
        /// The kind of section found, for example `private key`.
        kind: &'static str,
    },
    /// A certificate in the bundle is not an X.509 certificate a TLS client can parse.
    #[error("the file contains a certificate that cannot be used as a trust anchor: {source}")]
    UnusableCertificate {
        /// The TLS client's own parse diagnostic.
        #[source]
        source: rustls::Error,
    },
}

// Compiled-in proof that a refusal can cross the `.await` of the startup path that reads the file
// and the assignment that stores the policy.
const _: fn() = || {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<BundleError>();
};
