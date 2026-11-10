//! The optional `metrics` section of the bootstrap file.
//!
//! The self-reporting listener is a process knob, not configuration: it is read once, before any
//! configuration source exists, and a pushed document cannot move it. The whole section is
//! optional, and a present section without an address is a startup error rather than a default —
//! a default port would silently change the security surface of every existing deployment.

use std::net::SocketAddr;

use nico_http::validate_listen;
use serde::Deserialize;

use super::BootstrapError;

/// The `metrics` section exactly as written, before validation.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawMetrics {
    listen: Option<String>,
}

/// Validated settings for the self-reporting listener.
///
/// Fields are private and the only constructor is the bootstrap section's own `resolve`, because
/// an unvalidated address would let a bootstrap typo reach a second listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricsSettings {
    listen: String,
}

impl MetricsSettings {
    /// Returns the address the self-reporting listener binds.
    #[must_use]
    pub fn listen(&self) -> &str {
        &self.listen
    }
}

impl RawMetrics {
    /// Resolves the section, rejecting a present section with no address and an address that
    /// collides with the data plane's.
    ///
    /// # Errors
    /// Returns [`BootstrapError::InvalidField`] under `metrics.listen` when the section is present
    /// but names no address, when the address is not a legal listener, or when it is the same
    /// address the data plane binds.
    pub(super) fn resolve(&self, http_listen: &str) -> Result<MetricsSettings, BootstrapError> {
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
        if same_address(listen, http_listen) {
            return Err(BootstrapError::InvalidField {
                field: "metrics.listen".into(),
                reason: format!("must differ from `http.listen` ({http_listen})").into(),
            });
        }
        Ok(MetricsSettings {
            listen: listen.to_owned(),
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
