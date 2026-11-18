//! The optional `proxy` section of the bootstrap file: what an *upstream* handshake trusts.
//!
//! Trust roots describe the outbound direction, which is why they are named after nginx's
//! `proxy_ssl_*` family rather than sharing the `tls` name with the certificate an inbound listener
//! presents: `http.listeners[].tls` is this process's identity, `proxy.tls` is what it verifies a
//! peer against. Two directions, two sections, no ambiguity.
//!
//! The whole section may be omitted, in which case the platform certificate store is used. A
//! **present** section must name its `tls` block — an operator who wrote `proxy:` meant to configure
//! the outbound trust, so mapping an empty body back to "no section" would silently keep the
//! platform roots. The `tls` block itself follows the same rule: see [`tls::section`].
//!
//! Like every other process fact, the section is read once at startup and never from a
//! configuration source: Pingora bakes the roots into its client connector, so a reload cannot
//! change them.

use nico_http::TlsPolicy;
use serde::{Deserialize, Deserializer};

use super::{BootstrapError, tls};

/// The `proxy` section exactly as written, before validation.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawProxy {
    // Present-but-empty is an error, exactly as it was for the top-level `tls:` this replaced: an
    // operator who wrote `proxy.tls:` meant to name a bundle.
    #[serde(default, deserialize_with = "tls::section")]
    tls: Option<tls::RawTls>,
}

/// Reads the `proxy` key when it is present, including when its body is empty.
///
/// An empty `proxy:` deserializes as `None`, which would be indistinguishable from an omitted
/// section; mapping it to an empty section lets [`RawProxy::resolve`] report the missing `tls`
/// block instead of silently trusting the platform store.
///
/// # Errors
/// Returns the deserializer's error when the section is not a mapping, or has unknown fields.
pub(super) fn section<'de, D>(deserializer: D) -> Result<Option<RawProxy>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<RawProxy>::deserialize(deserializer)?.or(Some(RawProxy::default())))
}

impl RawProxy {
    /// Resolves the section into the trust policy the data plane is built with.
    ///
    /// # Errors
    /// Returns [`BootstrapError::InvalidField`] naming `proxy.tls` when the section is present but
    /// names no `tls` block, and the errors [`tls::RawTls::resolve`] produces under `proxy.tls.*`
    /// when the block is written but unusable.
    pub(super) fn resolve(&self) -> Result<TlsPolicy, BootstrapError> {
        let Some(raw) = &self.tls else {
            return Err(BootstrapError::InvalidField {
                field: "proxy.tls".into(),
                reason: "the `proxy` section is present, so it must name a `tls` section".into(),
            });
        };
        raw.resolve()
    }
}
