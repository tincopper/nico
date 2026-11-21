//! The options a real etcd client is built with: its trust material and its credentials.
//!
//! These types carry **bytes**, never paths. Where a process keeps a certificate, how it reads one,
//! and whether it re-checks it before connecting are all the caller's business; this crate's only
//! job is to turn the material into the configuration the transport needs. That is what keeps a
//! protocol crate from learning a repository's test directory or a deployment's file layout.
//!
//! Two rules shape the values:
//!
//! - **An absent option is not an empty one.** No trust material means no TLS configuration at all —
//!   never an empty root store, and never a fall back to the platform's, because a client that
//!   silently trusted the host's store would make "which etcd is this" a property of the machine.
//!   Likewise, no credentials means the client presents none: etcd's anonymous access and an empty
//!   user name are not the same request.
//! - **The material is not printable.** Every `Debug` here reports what is present and how large it
//!   is; a password, a certificate and a private key have no spelling in a log line.

use std::fmt;

/// The trust material one etcd connection uses.
///
/// The certificate authority is a complete replacement for any platform trust store, so it must be
/// the bundle the endpoints' certificates chain to. A client identity is optional: it is what a
/// cluster asks for when it authenticates its clients by certificate rather than by password.
#[derive(Clone, PartialEq, Eq)]
pub struct EtcdTlsOptions {
    ca_pem: Vec<u8>,
    identity: Option<(Vec<u8>, Vec<u8>)>,
    domain_name: Option<String>,
}

impl EtcdTlsOptions {
    /// Builds the options from the certificate authority bundle's PEM bytes.
    #[must_use]
    pub fn new(ca_pem: impl Into<Vec<u8>>) -> Self {
        Self {
            ca_pem: ca_pem.into(),
            identity: None,
            domain_name: None,
        }
    }

    /// Presents a client certificate and its private key to the server.
    #[must_use]
    pub fn with_identity(
        mut self,
        certificate_pem: impl Into<Vec<u8>>,
        key_pem: impl Into<Vec<u8>>,
    ) -> Self {
        self.identity = Some((certificate_pem.into(), key_pem.into()));
        self
    }

    /// Verifies every endpoint's certificate against this name instead of the endpoint's host.
    ///
    /// One client verifies one name: a list whose endpoints carry certificates for different names
    /// needs more than one connection, and the caller decides that rather than this type.
    #[must_use]
    pub fn with_domain_name(mut self, domain_name: impl Into<String>) -> Self {
        self.domain_name = Some(domain_name.into());
        self
    }

    /// Returns the certificate authority bundle.
    pub(crate) fn ca_pem(&self) -> &[u8] {
        &self.ca_pem
    }

    /// Returns the `(certificate chain, private key)` pair, when one is configured.
    pub(crate) fn identity(&self) -> Option<(&[u8], &[u8])> {
        self.identity
            .as_ref()
            .map(|(certificate, key)| (certificate.as_slice(), key.as_slice()))
    }

    /// Returns the name the certificate is verified against, or `None` for the endpoint's host.
    pub(crate) fn domain_name(&self) -> Option<&str> {
        self.domain_name.as_deref()
    }
}

impl fmt::Debug for EtcdTlsOptions {
    /// Reports the shape of the material, never its bytes.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EtcdTlsOptions")
            .field("ca_pem_bytes", &self.ca_pem.len())
            .field("has_identity", &self.identity.is_some())
            .field("domain_name", &self.domain_name)
            .finish()
    }
}

/// The user and password one etcd connection authenticates with.
///
/// The password is a value here, not a reference: the caller resolved it, and this crate never
/// learns where from.
#[derive(Clone, PartialEq, Eq)]
pub struct EtcdCredentials {
    user: String,
    password: String,
}

impl EtcdCredentials {
    /// Builds the credentials from the user name and the resolved password.
    #[must_use]
    pub fn new(user: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            user: user.into(),
            password: password.into(),
        }
    }

    /// Returns the user name.
    pub(crate) fn user(&self) -> &str {
        &self.user
    }

    /// Returns the password.
    pub(crate) fn password(&self) -> &str {
        &self.password
    }
}

impl fmt::Debug for EtcdCredentials {
    /// Names the user and never the password: a credential in a log line is a credential leaked.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EtcdCredentials")
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Everything one etcd connection is configured with beyond its endpoints.
///
/// [`Default`] is the plaintext, anonymous client — the shape this crate had before either option
/// existed — so a caller that wants today's behaviour writes nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EtcdConnectOptions {
    tls: Option<EtcdTlsOptions>,
    credentials: Option<EtcdCredentials>,
}

impl EtcdConnectOptions {
    /// Builds the plaintext, anonymous options.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Connects over TLS with this trust material.
    #[must_use]
    pub fn with_tls(mut self, tls: EtcdTlsOptions) -> Self {
        self.tls = Some(tls);
        self
    }

    /// Authenticates with these credentials.
    #[must_use]
    pub fn with_credentials(mut self, credentials: EtcdCredentials) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// Returns the TLS options, when the connection is encrypted.
    pub(crate) fn tls(&self) -> Option<&EtcdTlsOptions> {
        self.tls.as_ref()
    }

    /// Returns the credentials, when the connection authenticates.
    pub(crate) fn credentials(&self) -> Option<&EtcdCredentials> {
        self.credentials.as_ref()
    }
}
