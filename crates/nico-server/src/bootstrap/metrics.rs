//! The optional `metrics` section of the bootstrap file.
//!
//! The self-reporting listener is a process knob, not configuration: it is read once, before any
//! configuration source exists, and a pushed document cannot move it. The whole section is
//! optional, and a present section without an address is a startup error rather than a default —
//! a default port would silently change the security surface of every existing deployment.
//!
//! The section also carries the optional `tls` block, so `/health/live`, `/health/ready` and
//! `/metrics` need no fronting proxy to be served only over an encrypted connection. It is the same
//! block an `http.listeners[]` entry carries, parsed by the same code: one certificate, read and
//! paired at startup, one protocol selection (HTTP/1.1 only — scrapers and probes speak nothing
//! else). A block with no files is a configuration error rather than a plaintext fallback.

use std::net::SocketAddr;

use nico_http::{InboundTls, validate_listen};
use serde::Deserialize;

use super::{BootstrapError, inbound_tls};

/// The `metrics` section exactly as written, before validation.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawMetrics {
    listen: Option<String>,
    // Present-but-empty is an error, exactly as on a listener entry: an operator who wrote `tls:`
    // meant TLS, and the self-reporting surface is the last place to start guessing.
    #[serde(default, deserialize_with = "inbound_tls::section")]
    tls: Option<inbound_tls::RawTlsIdentity>,
}

/// Validated settings for the self-reporting listener.
///
/// Fields are private and the only constructor is the bootstrap section's own `resolve`, because
/// an unvalidated address would let a bootstrap typo reach a second listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricsSettings {
    listen: String,
    tls: Option<InboundTls>,
}

impl MetricsSettings {
    /// Returns the address the self-reporting listener binds.
    #[must_use]
    pub fn listen(&self) -> &str {
        &self.listen
    }

    /// Returns the identity the self-reporting listener presents, or `None` when it serves plaintext.
    ///
    /// The presence of this value **is** the transport decision, exactly as a listener entry's
    /// `tls` block is: a present identity means the socket terminates TLS and serves nothing else.
    #[must_use]
    pub fn tls(&self) -> Option<&InboundTls> {
        self.tls.as_ref()
    }
}

impl RawMetrics {
    /// Resolves the section, rejecting a present section with no address, an address that
    /// collides with one the data plane binds, and an unusable `tls` block.
    ///
    /// `listeners` is every address the `http` section resolves to — plaintext **and** TLS. A TLS
    /// listener owns its socket exactly as a plaintext one does, so a self-reporting listener that
    /// named it would fight it for the bind.
    ///
    /// # Errors
    /// Returns [`BootstrapError::InvalidField`] under `metrics.listen` when the section is present
    /// but names no address, when the address is not a legal listener, or when it is the same
    /// address one of the data plane's listeners binds. A written `tls` block is resolved after the
    /// address rules and reports its own failures under `metrics.tls.cert_file` / `metrics.tls.key_file`.
    pub(super) fn resolve(&self, listeners: &[&str]) -> Result<MetricsSettings, BootstrapError> {
        let Some(listen) = self.listen.as_deref() else {
            return Err(BootstrapError::InvalidField {
                field: "metrics.listen".into(),
                reason: "the `metrics` section is present, so it must name a `listen` address"
                    .into(),
            });
        };
        validate_listen(listen).map_err(|error| BootstrapError::InvalidField {
            field: "metrics.listen".into(),
            reason: error.reason().to_owned().into(),
        })?;
        if let Some(collision) = listeners
            .iter()
            .find(|listener| same_address(listen, listener))
        {
            return Err(BootstrapError::InvalidField {
                field: "metrics.listen".into(),
                reason: format!("must differ from `http.listeners` entry `{collision}`").into(),
            });
        }
        Ok(MetricsSettings {
            listen: listen.to_owned(),
            // The address is checked first on purpose: a section that names a socket the data plane
            // already owns has nothing to bind whether or not it terminates TLS, and reporting the
            // certificate first would send the operator to fix the wrong field.
            tls: match &self.tls {
                Some(identity) => Some(identity.resolve("metrics.tls")?),
                None => None,
            },
        })
    }
}

/// Reports whether two listener addresses name the same socket.
///
/// Both are parsed when they are IP literals, so two spellings of one socket collide. A hostname
/// falls back to a string comparison, because resolving it here would make startup depend on DNS.
///
/// The fallback is deliberately incomplete, and the gap is worth naming: `127.0.0.1:9090` and
/// `localhost:9090` are the same socket but compare as different, so this check does not catch them.
/// The failure it misses is loud rather than silent — Pingora's second listener panics on the bind —
/// and closing the gap would mean either a DNS lookup at startup or a resolver dependency, both
/// worse than the panic this check already turns into a readable startup error for every literal.
fn same_address(left: &str, right: &str) -> bool {
    match (left.parse::<SocketAddr>(), right.parse::<SocketAddr>()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

#[cfg(test)]
mod tests;
