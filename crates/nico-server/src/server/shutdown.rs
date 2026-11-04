//! Bridging the process's shutdown token into Pingora's `RunArgs`.
//!
//! The process has exactly one shutdown source, a [`CancellationToken`] created in
//! [`server::serve`](super::serve). Pingora needs a `ShutdownSignalWatch` instead, and its own
//! watcher is Unix-signal only and knows nothing about the token — so this module translates, and
//! the translation is the instrument a test cancels in-process rather than forking or raising a
//! signal.
//!
//! Signals belong here, not in `nico-http`: a data-plane library that installs a signal handler
//! as a side effect of being linked cannot be embedded in a test or a second front-end.

use async_trait::async_trait;
use pingora::server::{RunArgs, ShutdownSignal, ShutdownSignalWatch};
#[cfg(unix)]
use tokio::signal::unix::Signal;
use tokio_util::sync::CancellationToken;

/// Builds the `RunArgs` that route Pingora's shutdown through `shutdown`.
///
/// On a platform without Unix signals Pingora's `RunArgs` carries no watcher field, so the token is
/// still the shutdown source for the pipeline but the runtime's own default watcher stops the
/// server.
pub(super) fn run_args(shutdown: CancellationToken) -> RunArgs {
    #[cfg(unix)]
    {
        RunArgs {
            shutdown_signal: Box::new(CancelWatch::new(shutdown)),
        }
    }
    #[cfg(not(unix))]
    {
        // The token is still what stops the reload pipeline; there is no field to carry it further.
        let _ = shutdown;
        RunArgs::default()
    }
}

/// Reports a graceful termination when the token is cancelled or the process is asked to stop.
///
/// `GracefulTerminate` makes Pingora wait `grace_period_seconds` for in-flight requests before it
/// stops the runtime; that window is configured by the bootstrap file, never Pingora's 300-second
/// default.
struct CancelWatch {
    shutdown: CancellationToken,
}

impl CancelWatch {
    /// Builds a watcher over the process's shutdown token.
    fn new(shutdown: CancellationToken) -> Self {
        Self { shutdown }
    }
}

#[async_trait]
impl ShutdownSignalWatch for CancelWatch {
    async fn recv(&self) -> ShutdownSignal {
        wait_for_shutdown(&self.shutdown).await;
        tracing::info!("shutdown requested; draining");
        // Cancelling here is what stops the reload pipeline too: one token, one shutdown.
        self.shutdown.cancel();
        ShutdownSignal::GracefulTerminate
    }
}

/// Waits until the token is cancelled or the process is asked to stop.
async fn wait_for_shutdown(shutdown: &CancellationToken) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(stream) => Some(stream),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "SIGTERM handling is unavailable; only SIGINT will stop the gateway"
                );
                None
            }
        };
        tokio::select! {
            () = shutdown.cancelled() => {}
            result = tokio::signal::ctrl_c() => report_signal(result, "SIGINT"),
            () = next_terminate(terminate.as_mut()) => {}
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            () = shutdown.cancelled() => {}
            result = tokio::signal::ctrl_c() => report_signal(result, "SIGINT"),
        }
    }
}

/// Waits for the next `SIGTERM`, or forever when the handler could not be installed.
#[cfg(unix)]
async fn next_terminate(stream: Option<&mut Signal>) {
    match stream {
        Some(stream) => {
            stream.recv().await;
        }
        None => std::future::pending().await,
    }
}

/// Logs how a signal wait ended; a failed handler must not be mistaken for a signal.
fn report_signal(result: std::io::Result<()>, name: &'static str) {
    match result {
        Ok(()) => tracing::info!(signal = name, "signal received"),
        Err(error) => tracing::warn!(error = %error, signal = name, "signal handling failed"),
    }
}

#[cfg(test)]
mod tests;
