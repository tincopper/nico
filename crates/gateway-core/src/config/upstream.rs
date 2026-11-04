//! Plain HTTP upstreams a route can forward to.
//!
//! An [`Upstream`] is a resolved destination: scheme, host and port. It deliberately carries no
//! path — the client's path is forwarded as-is by the data plane — and no credential.

use serde::Deserialize;

/// The transport scheme used to reach an [`Upstream`].
///
/// The set is closed for this milestone but may grow, so matches from other crates must be
/// exhaustive over the variants they know plus a wildcard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Scheme {
    /// Cleartext HTTP.
    Http,
    /// HTTP over TLS.
    Https,
}

/// An HTTP destination referenced by a [`Route`](super::Route).
///
/// Fields are private and there is no public constructor: an `Upstream` only exists inside a
/// [`Document`](super::Document) that has passed reference and semantic validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    pub(super) name: String,
    pub(super) scheme: Scheme,
    pub(super) host: String,
    pub(super) port: u16,
}

impl Upstream {
    /// Returns the unique upstream name referenced by routes.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the transport scheme.
    #[must_use]
    pub const fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// Returns the destination host, for example `127.0.0.1` or `api.internal`.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Returns the destination port.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }
}
