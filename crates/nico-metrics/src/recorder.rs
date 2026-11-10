//! The one recorder installation and the Prometheus text render.
//!
//! The global recorder is process-wide state, so it lives in a [`OnceLock`]: the first
//! [`install`] wins, and every later call returns the same handle. That is also why a failed
//! installation is a startup error — a second recorder cannot be installed, and a surface that
//! silently renders nothing is the worst failure an operator can be left with.

use std::{
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use nico_core::ConfigStatus;

/// The process-wide recorder handle, installed at most once.
///
/// The `Result` is cached on purpose: the only failure is a global recorder already being
/// installed, which cannot become installable later, so a failed first call is a permanent
/// startup error rather than a transient one to retry.
static RECORDER: OnceLock<Result<PrometheusHandle, MetricsError>> = OnceLock::new();

/// Installs the Prometheus recorder and returns the handle that renders it.
///
/// Idempotent: the first call installs the global recorder, and every later call returns the same
/// handle. Only the first call can fail, because a process can set a global recorder exactly once.
///
/// # Errors
/// Returns [`MetricsError::Recorder`] when another recorder is already installed globally, which
/// is a startup failure: the process cannot expose metrics, and pretending otherwise would leave
/// an operator with an empty surface.
pub fn install() -> Result<Metrics, MetricsError> {
    let installed = RECORDER.get_or_init(|| {
        PrometheusBuilder::new()
            .install_recorder()
            .map_err(|error| MetricsError::Recorder {
                reason: error.to_string(),
            })
    });
    match installed {
        Ok(handle) => Ok(Metrics {
            handle: handle.clone(),
        }),
        Err(error) => Err(error.clone()),
    }
}

/// The installed recorder, able to render the current metrics as Prometheus text.
#[derive(Clone, Debug)]
pub struct Metrics {
    handle: PrometheusHandle,
}

impl Metrics {
    /// Renders the metrics as Prometheus text.
    ///
    /// The two scrape-time gauges are written first, and the recorder's upkeep runs before the
    /// render. `install_recorder()` starts no background task, and a histogram's samples reach the
    /// distribution the renderer reads only through `run_upkeep()`; without it a histogram renders
    /// empty. The order is the contract, not an optimisation.
    ///
    /// `config_snapshot_age_seconds` is skipped when the status has never recorded a successful
    /// publish: a missing value is honest, while a zero-second age would claim the snapshot was
    /// just published.
    #[must_use]
    pub fn render(&self, status: &ConfigStatus) -> String {
        metrics::gauge!("config_degraded").set(f64::from(status.degraded));
        if let Some(last_reload) = status.last_reload_unix_secs {
            let age = unix_now().saturating_sub(last_reload);
            metrics::gauge!("config_snapshot_age_seconds").set(age as f64);
        }
        self.handle.run_upkeep();
        self.handle.render()
    }
}

/// Why the recorder could not be installed.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum MetricsError {
    /// A global recorder was already installed, so this process cannot expose metrics.
    #[error("the global metrics recorder could not be installed: {reason}")]
    Recorder {
        /// The exporter's own explanation.
        reason: String,
    },
}

/// Returns the current Unix time in seconds, or `0` before the epoch.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
