//! The one place a validated bootstrap becomes a running configuration source.
//!
//! Every source type this build can serve is constructed here and nowhere else: `main` stays free
//! of branching, and a test can build the same source a deployment would from the same bootstrap
//! value. The pipeline beyond this point is generic over `Arc<dyn ConfigSource>` and never learns
//! which type it is driving.
//!
//! A validated bootstrap holds no secret and no key material — only paths and references — so
//! everything a client needs to *present* is read here, once, at startup. That second read is the
//! point: a file the bootstrap checked and that changed in between must fail the process with a
//! field name, not fail the first request with a transport error.
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

use nico_config::{ConfigSource, ConfigSourceError, LocalConfig, LocalFileSource};
use nico_core::ErrorCategory;

use crate::bootstrap::{BootstrapConfig, SourceSelection};

#[cfg(feature = "etcd")]
mod etcd;

/// Why a source could not be built from a validated bootstrap.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SourceBuildError {
    /// The local source could not be built.
    #[error("the local configuration source could not be built: {0}")]
    Local(#[from] ConfigSourceError),
    /// A field the bootstrap validated cannot be used now that the source is being built.
    ///
    /// The bootstrap checked that the *paths and references* an operator wrote are usable; this
    /// failure means the thing they name is not, at the moment the client is assembled — a trust
    /// root removed after the check, a reference whose value is unset. It is the operator's file,
    /// not a dependency's failure, which is why [`category`](Self::category) says `Client`. Neither
    /// a password value nor a key byte is part of the message.
    #[cfg(feature = "etcd")]
    #[error("bootstrap field `{field}` is invalid: {reason}")]
    Setting {
        /// Dotted field path, for example `config_source.auth.password_ref`.
        field: std::borrow::Cow<'static, str>,
        /// Why the value was rejected.
        reason: std::borrow::Cow<'static, str>,
    },
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
            // A field that cannot be honoured is the bootstrap file's mistake, the same bucket
            // `BootstrapError::InvalidField` lands in.
            #[cfg(feature = "etcd")]
            Self::Setting { .. } => ErrorCategory::Client,
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
/// Returns [`SourceBuildError`] when the selected source cannot be constructed: a local source with
/// an unusable cache path, an etcd source whose trust root or credentials cannot be read, or an etcd
/// source whose endpoints cannot be connected.
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
        SourceSelection::Etcd {
            endpoints,
            key,
            tls,
            auth,
        } => {
            let options = etcd::connect_options(tls.as_ref(), auth.as_ref())?;
            let client = etcd::connect_on_its_own_runtime(endpoints.clone(), options)?;
            Ok(Arc::new(nico_config_etcd::EtcdConfigSource::new(
                key.clone(),
                bootstrap.source_max_bytes(),
                Arc::new(client),
            )))
        }
    }
}
