//! Process assembly: Pingora owns the runtime, and one token stops everything on it.
//!
//! A gateway process is three things wired together before any traffic is served:
//!
//! 1. the configuration pipeline, which is a plain future and rides Pingora's runtime as a
//!    background service — there is no second Tokio runtime;
//! 2. the HTTP data plane, which owns the listening socket and reads the snapshot the pipeline
//!    publishes;
//! 3. one [`CancellationToken`], which is the only shutdown source: the signal watcher cancels it,
//!    Pingora drains on it, and the pipeline stops watching its source because of it.
//!
//! [`serve`] is the single production assembly point. [`crate::serve`] remains the pipeline seam a
//! test drives without a socket, and both create the pipeline with the same [`Reloader`] arguments.
//! Signals, runtime and shutdown live here rather than in `nico-http`, so the data-plane crate
//! stays a library that cannot hijack the process it is linked into.

use std::{
    process::ExitCode,
    sync::{Arc, Mutex, PoisonError},
};

use async_trait::async_trait;
use nico_config::{CacheStore, ConfigSource};
use nico_core::SnapshotHandle;
use nico_http::{FilterChain, server_conf, service as http_service};
use pingora::{
    server::{Server, ShutdownWatch},
    services::background::{BackgroundService, background_service},
};
use tokio_util::sync::CancellationToken;

use self::shutdown::run_args;
use crate::{BootstrapConfig, FAILURE_EXIT_CODE, ReloadError, ReloadLimits, Reloader};

mod shutdown;

/// Runs the gateway until shutdown and returns the process exit code.
///
/// The exit code reports one thing: whether a configuration snapshot ever reached service. A
/// pipeline that gave up before its first publish is logged where it happens and then stops the
/// process — serving `503 not_ready` forever is not a state an operator asked for. Pingora drains
/// first, and only then does this function turn the recorded failure into [`FAILURE_EXIT_CODE`].
pub fn serve(
    bootstrap: &BootstrapConfig,
    source: Arc<dyn ConfigSource>,
    shutdown: CancellationToken,
) -> ExitCode {
    let settings = bootstrap.http();
    let handle = SnapshotHandle::new();
    let failure = Arc::new(Mutex::new(None));

    let mut server = match Server::new(None) {
        Ok(server) => server,
        Err(error) => {
            tracing::error!(error = %error, "the HTTP server could not be created");
            return ExitCode::from(FAILURE_EXIT_CODE);
        }
    };
    // Pingora's own default conf is discarded here rather than passed to the constructor: the only
    // fields it reads at construction time are the daemon and pid settings, and this build sets
    // neither. Every field a running server reads comes from this replacement.
    server.configuration = server_conf(settings);

    let reloader = Reloader::new(
        source,
        Arc::clone(&handle),
        Some(CacheStore::new(bootstrap.cache_dir().to_path_buf())),
        ReloadLimits::new(bootstrap.debounce(), bootstrap.max_wait()),
        bootstrap.parse_limits(),
        bootstrap.allow_bootstrap_cache(),
        shutdown.clone(),
    );
    server.add_service(background_service(
        "config-reload",
        ReloadService {
            reloader: Mutex::new(Some(reloader)),
            failure: Arc::clone(&failure),
            shutdown: shutdown.clone(),
        },
    ));
    server.add_service(http_service(
        &server.configuration,
        handle,
        FilterChain::empty(),
        settings,
    ));

    server.bootstrap();
    server.run(run_args(shutdown));

    // The failure itself was logged when it happened; this is only the exit decision, so the
    // reason appears exactly once even though both places could report it.
    match lock(&failure).take() {
        Some(_error) => ExitCode::from(FAILURE_EXIT_CODE),
        None => ExitCode::SUCCESS,
    }
}

/// Hosts the reload pipeline on Pingora's runtime as a background service.
///
/// Pingora hands a background service a shutdown watch of its own, which this build deliberately
/// ignores: the process's [`CancellationToken`] is the single shutdown source, and following two
/// would let the pipeline outlive the drain or stop before it.
struct ReloadService {
    /// The pipeline consumes itself when it runs, and it must run exactly once: the slot is taken,
    /// never shared. It is read and emptied before the first `.await`, so no lock is ever held
    /// across one.
    reloader: Mutex<Option<Reloader>>,
    /// Where a final failure is recorded for the exit code, once the server has stopped.
    failure: Arc<Mutex<Option<ReloadError>>>,
    /// Cancelled when the pipeline gives up, so the process stops instead of serving 503s.
    shutdown: CancellationToken,
}

#[async_trait]
impl BackgroundService for ReloadService {
    async fn start(&self, _shutdown: ShutdownWatch) {
        let reloader = { lock(&self.reloader).take() };
        let Some(reloader) = reloader else {
            // A second start would run a second pipeline against the same handle; refuse.
            tracing::warn!("the configuration pipeline was already started");
            return;
        };

        match reloader.run().await {
            Ok(()) => tracing::info!("configuration pipeline stopped"),
            Err(error) => {
                tracing::error!(
                    error = %error,
                    error.category = ?error.category(),
                    "the configuration pipeline stopped before a snapshot reached service"
                );
                *lock(&self.failure) = Some(error);
                // An error here means no snapshot can ever be published, so there is nothing left
                // to serve: stop the drain the same way a signal would.
                self.shutdown.cancel();
            }
        }
    }
}

/// Locks a mutex, recovering the value when a previous holder panicked.
///
/// Neither lock guards anything that can be left half-updated — one holds a task to run, the other a
/// failure to report — so recovering is safer than turning a panic elsewhere into a second panic
/// here.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
