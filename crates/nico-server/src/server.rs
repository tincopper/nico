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
//! [`serve_with`] is the single production assembly point, and [`serve`] is it with the built-in
//! plugin kinds. [`crate::serve`] remains the pipeline seam a test drives without a socket, and both
//! create the pipeline with the same [`Reloader`] arguments. Signals, runtime and shutdown live here
//! rather than in `nico-http`, so the data-plane crate stays a library that cannot hijack the process
//! it is linked into.

use std::{
    process::ExitCode,
    sync::{Arc, Mutex, PoisonError},
};

use async_trait::async_trait;
use nico_config::{CacheStore, ConfigSource};
use nico_core::SnapshotHandle;
use nico_http::{
    HttpStateHandle, PluginRegistry, ensure_usable, server_conf, service as http_service,
};
use nico_plugins::{PluginRuntime, builtin_registry};
use pingora::{
    server::{Server, ShutdownWatch},
    services::background::{BackgroundService, background_service},
};
use tokio_util::sync::CancellationToken;

use self::shutdown::run_args;
use crate::{
    BootstrapConfig, FAILURE_EXIT_CODE, Publication, ReloadError, ReloadLimits, ReloadSettings,
    Reloader,
};

mod shutdown;

/// Runs the gateway until shutdown and returns the process exit code.
///
/// This is [`serve_with`] with every kind this build implements, which is what a deployment wants:
/// the built-in set is the whole of "a compiled-in policy is installed" (see [`builtin_registry`]).
/// A test that needs a kind of its own calls `serve_with` instead.
pub fn serve(
    bootstrap: &BootstrapConfig,
    source: Arc<dyn ConfigSource>,
    shutdown: CancellationToken,
) -> ExitCode {
    serve_with(bootstrap, source, shutdown, builtin_registry())
}

/// Runs the gateway with `plugins` as the complete set of document `kind`s it can load.
///
/// The registry is a parameter for two reasons, and neither is a configuration knob:
///
/// - **Which kinds exist is an assembly decision.** A built-in kind arrives through
///   [`builtin_registry`] because it is compiled into this binary; a WASM host will arrive by adding
///   one factory to the same registry. Neither changes the document model, the chains or the phases,
///   so the process — not a library — is the right place to decide the set.
/// - **A test has to observe the stages.** A scenario that asserts "the global chain ran before
///   routing" needs a plugin that records what it was called with, and that plugin must never be
///   reachable from a deployment. Passing the registry in is what lets the integration tests drive
///   the production assembly with a kind only they register.
///
/// The set is taken by value and kept for the process: the registry holds the factories, and a factory
/// may hold state that must survive a reload (a token-bucket table), so it is built once here rather
/// than once per generation. A document that names a kind this registry does not answer to is
/// refused at publication and the last known good generation keeps serving.
///
/// The exit code reports one thing: whether a configuration snapshot ever reached service. A pipeline
/// that gave up before its first publish is logged where it happens and then stops the process —
/// serving `503 not_ready` forever is not a state an operator asked for. Pingora drains first, and
/// only then does this function turn the recorded failure into [`FAILURE_EXIT_CODE`].
pub fn serve_with(
    bootstrap: &BootstrapConfig,
    source: Arc<dyn ConfigSource>,
    shutdown: CancellationToken,
    plugins: PluginRegistry,
) -> ExitCode {
    let settings = bootstrap.http();
    // The trust roots are read once, by Pingora, while it builds the client connector — and its
    // failure there is a panic inside `HttpProxy::new`. Probe the same source first, so a host
    // without a readable trust store, or a bundle that stopped resolving, is a startup refusal with
    // one operator-facing line instead of an abort inside the framework. This runs before anything
    // is assembled: nothing below has a side effect worth undoing.
    if let Err(error) = ensure_usable(settings.tls()) {
        tracing::error!(
            error = %error,
            "the upstream trust roots this process would use are not usable; set `proxy.tls.ca_file` in \
             the bootstrap file to a PEM bundle of certificate authorities, or make the platform \
             certificate store readable"
        );
        return ExitCode::from(FAILURE_EXIT_CODE);
    }
    // The request path reads `state` and nothing else: the pipeline publishes a snapshot and the plan
    // compiled from it there, together. `SnapshotHandle` stays the configuration-state fact source,
    // and the self-reporting surface reads the same handle, so it cannot observe a state the
    // pipeline has not published.
    let state = HttpStateHandle::new();
    let snapshot = SnapshotHandle::new();
    // One plugin runtime for the process: the bucket tables inside it are runtime state, so a reload
    // that republishes the same quota must keep them rather than hand every caller a fresh one.
    let runtime = Arc::new(PluginRuntime::with_registry(plugins));
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
    server.configuration = server_conf(bootstrap.runtime(), settings.tls());

    if let Some(metrics_settings) = bootstrap.metrics() {
        let registry = match crate::metrics::assemble(metrics_settings) {
            Ok(registry) => registry,
            Err(error) => {
                tracing::error!(
                    error = %error,
                    "the self-reporting surface could not be assembled; the process cannot expose \
                     its own state"
                );
                return ExitCode::from(FAILURE_EXIT_CODE);
            }
        };
        server.add_service(crate::metrics::service(
            metrics_settings,
            registry,
            Arc::clone(&snapshot),
            Arc::clone(&state),
        ));
        tracing::info!(
            metrics.listen = metrics_settings.listen(),
            "metrics surface enabled"
        );
    } else {
        tracing::info!("metrics surface disabled");
    }

    let reload_settings = ReloadSettings::new(
        Some(CacheStore::new(bootstrap.cache_dir().to_path_buf())),
        ReloadLimits::new(bootstrap.debounce(), bootstrap.max_wait()),
        bootstrap.parse_limits(),
        bootstrap.allow_bootstrap_cache(),
        shutdown.clone(),
    );
    let reloader = Reloader::new(
        source,
        Publication::new(Arc::clone(&snapshot), Arc::clone(&state)),
        runtime,
        reload_settings,
    );
    server.add_service(background_service(
        "config-reload",
        ReloadService {
            reloader: Mutex::new(Some(reloader)),
            failure: Arc::clone(&failure),
            shutdown: shutdown.clone(),
        },
    ));
    let http_service = match http_service(&server.configuration, state, settings) {
        Ok(service) => service,
        Err(error) => {
            tracing::error!(
                error = %error,
                "the HTTP data plane could not be built; refusing to start instead of serving a \
                 listener whose TLS identity was not usable"
            );
            return ExitCode::from(FAILURE_EXIT_CODE);
        }
    };
    server.add_service(http_service);

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
