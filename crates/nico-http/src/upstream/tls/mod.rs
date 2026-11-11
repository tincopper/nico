//! Upstream TLS: what a handshake trusts, the one consumer that applies it, and the startup probe.
//!
//! Two layers, and the split is the point:
//!
//! - `policy.rs` is **pure policy**: where the trust roots come from, and whether a bundle of bytes
//!   is a usable set of them. It names no framework type and touches no file system, so it can be
//!   tested on bytes and moved to a `nico-tls` crate without a rewrite if a second TLS consumer
//!   appears (remote configuration sources, an OTLP exporter, plugin egress).
//! - `adapter.rs` is the single place that speaks Pingora's TLS vocabulary. It is the only consumer
//!   of the policy today, which is why it is a free function over one `ServerConf` and not a trait.
//!
//! [`ensure_usable`] is neither: it is the **assembly-time probe**. It answers one question the two
//! layers above cannot — "will the connector that is about to be built find any certificate at the
//! source this policy names?" — and it answers it by reading that source the way the connector will.
//!
//! The trust roots are a **process-level** fact: Pingora builds its client connector from
//! `ServerConf` once, when the service is built, so a policy is fixed for the life of the process.
//! Reloading a document never re-reads the bundle — changing the CA is a restart, not a reload.
//!
//! # Invariants
//!
//! - **Verification is on.** Neither certificate nor hostname checking is switchable from a
//!   document, so no configuration typo can turn a route into an interception channel.
//! - **One root source.** The platform store or one bundle that replaces it; there is no "both",
//!   because two half-rooted processes make "what is trusted here?" unanswerable.
//! - **A bundle is validated before it is used.** Pingora's connector construction `unwrap()`s its
//!   parse, so a bad file would abort the process inside `HttpProxy::new` instead of failing
//!   startup with a field name. [`validate_bundle`] runs the identical parse first.
//! - **A source with no usable certificate is refused, not accepted.** An empty store does not mean
//!   "trust everything": it means every handshake fails later, at a point that names no configuration
//!   field. The rule is applied twice — to the bytes the bootstrap file named, and again to whatever
//!   source the process will actually use when it is assembled ([`ensure_usable`]) — and it covers
//!   both halves of the connector's own load: at least one entry, and every entry loadable.

use std::{fs, io, path::PathBuf};

use rustls::{RootCertStore, pki_types::CertificateDer};

mod adapter;
mod policy;

pub(crate) use adapter::apply;
pub(crate) use policy::RootSource;
pub use policy::{BundleError, TlsPolicy, validate_bundle};

/// Proves that the trust roots `policy` names can actually be read, right before they are used.
///
/// Pingora reads its roots while building the client connector, and a failure there is
/// `build_connector(...).unwrap()` (`pingora-core/src/connectors/tls/rustls/mod.rs:50`) — a panic
/// inside `HttpProxy::new`, with no field name and no clean exit. This function runs the same read
/// first, so the process can refuse to start with an operator-facing line instead of aborting.
///
/// The platform store is probed even though nothing in this crate configures it: it is the default
/// source, so "the host cannot read its own trust store" is a startup fact on the same footing as a
/// bad bundle. A deployment that only proxies plaintext still fails here on purpose — a gateway
/// that cannot verify any peer would discover it on its first `https` request, far from the cause.
///
/// For a bundle the file is read **again** rather than trusting the parse that happened at
/// bootstrap: the connector opens its own copy, so a file that was deleted, truncated or replaced
/// between the two reads would reach that `unwrap()` unchanged.
///
/// # Errors
/// - [`TrustRootsError::PlatformUnreadable`] when the platform store exists but cannot be read
///   (a headless session, a container without `ca-certificates`).
/// - [`TrustRootsError::PlatformEmpty`] when the platform store reads as nothing at all.
/// - [`TrustRootsError::PlatformUnusable`] when it holds an entry the client cannot load.
/// - [`TrustRootsError::BundleUnreadable`] when the configured bundle cannot be read now.
/// - [`TrustRootsError::Bundle`] when it is readable and is not a usable set of trust roots.
pub fn ensure_usable(policy: &TlsPolicy) -> Result<(), TrustRootsError> {
    match policy.root_source() {
        RootSource::Platform => {
            // The same call Pingora makes, from the same crate: it honours `SSL_CERT_FILE` and
            // `SSL_CERT_DIR`, so a deployment mechanism cannot make the probe and the connector
            // disagree about which store they mean.
            let certificates = rustls_native_certs::load_native_certs()
                .map_err(|source| TrustRootsError::PlatformUnreadable { source })?;
            require_platform_roots(&certificates)
        }
        RootSource::Bundle(path) => {
            let bytes = fs::read(path).map_err(|source| TrustRootsError::BundleUnreadable {
                path: path.clone(),
                source,
            })?;
            validate_bundle(&bytes).map_err(TrustRootsError::Bundle)
        }
    }
}

/// Refuses a platform store that holds no certificate, or one the client cannot load.
///
/// Split out so the rules can be tested on a slice: the alternative is an environment where the
/// platform store happens to be empty or damaged, which no test can construct portably.
///
/// The two refusals are the connector's own two steps
/// (`pingora-rustls`'s `load_platform_certs_incl_env_into_store`: a `load_native_certs` call, then
/// `RootCertStore::add` for every entry). Checking only emptiness would leave the `unwrap()`
/// reachable through a store that is readable, non-empty, and unusable.
fn require_platform_roots(certificates: &[CertificateDer<'_>]) -> Result<(), TrustRootsError> {
    if certificates.is_empty() {
        return Err(TrustRootsError::PlatformEmpty);
    }
    let mut store = RootCertStore::empty();
    for certificate in certificates {
        store
            .add(certificate.clone())
            .map_err(|source| TrustRootsError::PlatformUnusable { source })?;
    }
    Ok(())
}

/// The trust roots this process would use cannot serve, and the process must not start.
///
/// Every variant means the same thing to the assembly: refuse to start and name the remedy. They are
/// distinguished by what an operator has to fix — a host without a readable trust store, or a bundle
/// that is missing or wrong — because that is the only useful content of a startup failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TrustRootsError {
    /// The platform certificate store could not be read.
    #[error("the platform certificate store could not be read: {source}")]
    PlatformUnreadable {
        /// The platform provider's own diagnostic.
        #[source]
        source: io::Error,
    },
    /// The platform certificate store held no certificate.
    #[error("the platform certificate store holds no certificate")]
    PlatformEmpty,
    /// The platform certificate store holds a certificate the TLS client cannot load.
    ///
    /// The store was readable and non-empty, so the diagnostic names what the client rejected
    /// rather than telling the operator to configure a bundle they may not need.
    #[error(
        "the platform certificate store holds a certificate that cannot be used as a trust \
         anchor: {source}"
    )]
    PlatformUnusable {
        /// The TLS client's own parse diagnostic.
        #[source]
        source: rustls::Error,
    },
    /// The configured bundle could not be read at assembly time.
    ///
    /// The path is named because this is a startup diagnostic, not a caller-facing body: an operator
    /// has to know which file the process tried to open.
    #[error("the trust-root bundle `{path}` could not be read: {source}")]
    BundleUnreadable {
        /// The path the bootstrap file named.
        path: PathBuf,
        /// The file system's diagnostic.
        #[source]
        source: io::Error,
    },
    /// The configured bundle is not a usable set of trust roots.
    #[error("the trust-root bundle is not usable: {0}")]
    Bundle(#[source] BundleError),
}

// Compiled-in proof that the probe's refusal can travel through the startup path's error handling.
const _: fn() = || {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<TrustRootsError>();
};

#[cfg(test)]
mod tests;
