//! The self-reporting transport: port preflight, the Pingora service and the assembly step.
//!
//! This module owns every transport decision the capability layer deliberately does not: whether
//! the port can be bound, which Pingora service carries the surface, and where in the process
//! assembly it is attached. It contains the only Pingora code that knows a metrics endpoint exists,
//! and even here the adapter does not know what `/health/live` means — it forwards a method and a
//! path and writes back the [`Response`] the capability layer produced.
//!
//! The socket has two possible transports and one of them is not decided here: `listener.rs` asks
//! `nico-http` for the TLS settings of the configured identity, so the self-reporting surface and
//! the data plane terminate TLS through the same builder.

mod app;
mod listener;

pub(crate) use listener::service;
use nico_metrics::Metrics;
use thiserror::Error;

use crate::bootstrap::MetricsSettings;

/// Assembles the self-reporting surface: preflights the port and installs the recorder.
///
/// Both steps are startup failures rather than runtime degradations. A port that cannot be bound
/// means the process has no self-reporting surface, and a recorder that cannot be installed means
/// the surface would render nothing — an operator reading an empty `/metrics` cannot tell that
/// apart from a gateway with no traffic.
///
/// # Errors
/// Returns [`AssembleError::Bind`] when the address cannot be bound, and
/// [`AssembleError::Recorder`] when a global recorder is already installed.
pub(crate) fn assemble(settings: &MetricsSettings) -> Result<Metrics, AssembleError> {
    // Preflight bind: Pingora builds its listeners inside the service task and panics on failure,
    // which would leave the process serving traffic with no self-reporting surface and a dead
    // task. Binding here turns that into a startup error the operator can read. The window between
    // this bind and Pingora's own is accepted and recorded in the design: the port is released
    // only after the check, and a collision in that window is the same failure Pingora reports.
    match std::net::TcpListener::bind(settings.listen()) {
        Ok(listener) => drop(listener),
        Err(source) => {
            return Err(AssembleError::Bind {
                listen: settings.listen().to_owned(),
                source,
            });
        }
    }
    nico_metrics::install().map_err(|source| AssembleError::Recorder { source })
}

/// Why the self-reporting surface could not be assembled.
///
/// No `#[non_exhaustive]`: this type is `pub(crate)` and the only matcher is `server.rs`, which
/// turns either variant into the same non-zero exit. The attribute would protect a downstream crate
/// that cannot exist.
#[derive(Debug, Error)]
pub(crate) enum AssembleError {
    /// The preflight bind failed.
    #[error("the metrics listener on {listen} could not bind: {source}")]
    Bind {
        /// The address that was being bound.
        listen: String,
        /// The underlying socket error.
        #[source]
        source: std::io::Error,
    },
    /// The global recorder could not be installed.
    #[error("the metrics recorder could not be installed: {source}")]
    Recorder {
        /// The capability layer's own failure.
        #[source]
        source: nico_metrics::MetricsError,
    },
}
