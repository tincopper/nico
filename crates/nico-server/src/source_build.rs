//! The one place a validated bootstrap becomes a running configuration source.
//!
//! Every source type this build can serve is constructed here and nowhere else: `main` stays free
//! of branching, and a test can build the same source a deployment would from the same bootstrap
//! value. The pipeline beyond this point is generic over `Arc<dyn ConfigSource>` and never learns
//! which type it is driving.
//!
//! The etcd branch exists only when compiled with the `etcd` feature, because a validated
//! bootstrap can never name a source this build cannot serve. Connecting has one hard fact to
//! honour, taken from the seam's own evidence: the etcd client's transports live on the runtime
//! that connected them, and that runtime must outlive the client — when it dies, every later
//! operation fails. The connect therefore runs on a dedicated daemon thread that drives its
//! current-thread runtime for the rest of the process, while the client itself is `Send + Sync`
//! and its operations are driven by whatever runtime calls them (in production, Pingora's).
//! Startup failures have no degraded form: the process exits, because the pipeline's reconnect
//! answers "the source went away mid-run", not "the source never existed".

use std::sync::Arc;
#[cfg(feature = "etcd")]
use std::sync::mpsc;

use nico_config::{ConfigSource, ConfigSourceError, LocalConfig, LocalFileSource};
use nico_core::ErrorCategory;

use crate::bootstrap::{BootstrapConfig, SourceSelection};

/// Why a source could not be built from a validated bootstrap.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SourceBuildError {
    /// The local source could not be built.
    #[error("the local configuration source could not be built: {0}")]
    Local(#[from] ConfigSourceError),
    /// The etcd client could not connect, or the runtime it depends on could not start.
    #[cfg(feature = "etcd")]
    #[error("the etcd configuration source could not be built: {0}")]
    Etcd(#[from] nico_config_etcd::EtcdError),
}

impl SourceBuildError {
    /// Returns the pipeline's category for this failure, so startup logging groups a build failure
    /// with the runtime failures it would have become.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::Local(inner) => inner.category(),
            // Both client failures — unreachable and refused — are "the source did not answer",
            // the same bucket the adapter folds them into once the pipeline is running.
            #[cfg(feature = "etcd")]
            Self::Etcd(_) => ErrorCategory::Upstream,
        }
    }
}

// Compiled-in proof that a source-build error can cross `.await` and `tokio::spawn` boundaries.
const _: fn() = || {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<SourceBuildError>();
};

/// Builds the authoritative source the bootstrap selects.
///
/// # Errors
/// Returns [`SourceBuildError`] when the selected source cannot be constructed: a local source
/// with an unusable cache path, or an etcd source whose endpoints cannot be connected.
pub fn build_source(
    bootstrap: &BootstrapConfig,
) -> Result<Arc<dyn ConfigSource>, SourceBuildError> {
    match bootstrap.source() {
        SourceSelection::Local { path, watch_mode } => {
            let config = LocalConfig::new(path.clone())
                .with_max_bytes(bootstrap.source_max_bytes())
                .with_watch_mode(*watch_mode);
            Ok(Arc::new(LocalFileSource::new(config)?))
        }
        #[cfg(feature = "etcd")]
        SourceSelection::Etcd { endpoints, key } => {
            let client = connect_on_its_own_runtime(endpoints.clone())?;
            Ok(Arc::new(nico_config_etcd::EtcdConfigSource::new(
                key.clone(),
                bootstrap.source_max_bytes(),
                Arc::new(client),
            )))
        }
    }
}

/// Connects the etcd client on the runtime that will own its transports forever.
///
/// The thread is detached on purpose: the runtime must not be dropped while the client lives, and
/// the process's shutdown protocol stays the single cancellation token — this thread does not
/// outlive the process it serves.
#[cfg(feature = "etcd")]
fn connect_on_its_own_runtime(
    endpoints: Vec<String>,
) -> Result<nico_config_etcd::GrpcEtcdClient, nico_config_etcd::EtcdError> {
    use nico_config_etcd::{EtcdError, GrpcEtcdClient};

    let (sender, receiver) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("etcd-client".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = sender.send(Err(EtcdError::Connect {
                        message: format!("the client runtime could not start: {error}"),
                    }));
                    return;
                }
            };
            let connected = runtime.block_on(GrpcEtcdClient::connect(&endpoints));
            // The client is only delivered once the runtime that owns its transports is running
            // for good; everything after this send exists to keep that runtime alive.
            if sender.send(connected).is_err() {
                return;
            }
            runtime.block_on(std::future::pending::<()>());
        });
    spawned.map_err(|error| EtcdError::Connect {
        message: format!("the client runtime thread could not start: {error}"),
    })?;
    receiver.recv().map_err(|_| EtcdError::Connect {
        message: "the client runtime thread ended before the client was delivered".into(),
    })?
}
