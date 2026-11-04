//! Gateway binary support library.
//!
//! The crate owns everything a gateway process does around its data plane:
//!
//! - [`bootstrap`] — reads the local-only file that selects one configuration source and states
//!   how far the process may trust its cache.
//! - [`reload`] — the pipeline that loads, validates, de-bounces and publishes configuration
//!   snapshots, keeping the last known good one in service when anything fails.
//! - [`serve`] — wires one source to one publication handle and runs until shutdown.
//! - [`run`] — the command line: parse arguments, build the local source, map problems to exit
//!   codes.
//!
//! `main.rs` is a thin shim over [`run`], so the whole wiring is reachable from an integration
//! test. No data plane exists yet: [`serve`] creates the snapshot handle that a HTTP data plane
//! will read in the next phase.
#![deny(missing_docs)]

use std::{
    ffi::OsString,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
};

use gateway_config::{CacheStore, ConfigSource, LocalFileSource};
use gateway_core::SnapshotHandle;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

pub mod bootstrap;
pub mod reload;

pub use bootstrap::{BootstrapConfig, BootstrapError};
pub use reload::{ReloadError, ReloadLimits, Reloader};

/// Exit code used when the command line is missing or not understood.
pub const USAGE_EXIT_CODE: u8 = 2;

/// Exit code used when the process could not reach a serving state, or stopped with an error.
pub const FAILURE_EXIT_CODE: u8 = 1;

/// Command-line usage summary.
pub const USAGE: &str = "usage: gateway-server --bootstrap <path>";

/// Runs the configuration pipeline for one source until `shutdown` is cancelled.
///
/// The cache is always written after a successful publish; it is only read when startup fails and
/// `bootstrap.allow_bootstrap_cache()` is set, so passing `true` never changes the running
/// behaviour of a healthy process.
///
/// # Errors
/// Returns [`ReloadError::NotReady`] when no snapshot could be published and no cache fallback was
/// allowed or usable, and [`ReloadError::GenerationOverflow`] when the generation counter is
/// exhausted. A source that fails *after* the first publish is not an error: the pipeline keeps
/// serving the last-known-good snapshot and retries.
pub async fn serve(
    bootstrap: BootstrapConfig,
    source: Arc<dyn ConfigSource>,
    shutdown: CancellationToken,
) -> Result<(), ReloadError> {
    let handle = SnapshotHandle::new();
    let reloader = Reloader::new(
        source,
        handle,
        Some(CacheStore::new(bootstrap.cache_dir().to_path_buf())),
        ReloadLimits::new(bootstrap.debounce(), bootstrap.max_wait()),
        bootstrap.parse_limits(),
        bootstrap.allow_bootstrap_cache(),
        shutdown,
    );
    reloader.run().await
}

/// Handles the command line and returns the process exit code.
///
/// `--help` prints [`USAGE`] to stdout and succeeds. A missing, duplicated or unknown argument
/// prints usage to stderr and returns [`USAGE_EXIT_CODE`]. A bootstrap file that cannot be read or
/// accepted, or a pipeline that never reaches a serving state, returns [`FAILURE_EXIT_CODE`].
#[must_use]
pub fn run(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    init_subscriber();

    let bootstrap_path = match parse_args(args) {
        Ok(Invocation::Help) => {
            write_usage(io::stdout());
            return ExitCode::SUCCESS;
        }
        Ok(Invocation::Bootstrap(path)) => path,
        Err(UsageProblem) => {
            write_usage(io::stderr());
            return ExitCode::from(USAGE_EXIT_CODE);
        }
    };

    let bootstrap = match BootstrapConfig::load(&bootstrap_path) {
        Ok(bootstrap) => bootstrap,
        Err(error) => {
            tracing::error!(
                error = %error,
                bootstrap.path = %bootstrap_path.display(),
                "bootstrap configuration could not be loaded"
            );
            return ExitCode::from(FAILURE_EXIT_CODE);
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(error = %error, "the async runtime could not be started");
            return ExitCode::from(FAILURE_EXIT_CODE);
        }
    };

    match runtime.block_on(start_gateway(bootstrap)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(
                error = %error,
                error.category = ?error.category(),
                "the gateway stopped before it could serve traffic"
            );
            ExitCode::from(FAILURE_EXIT_CODE)
        }
    }
}

/// Builds the concrete source and runs the pipeline until a signal arrives.
async fn start_gateway(bootstrap: BootstrapConfig) -> Result<(), ReloadError> {
    let shutdown = CancellationToken::new();
    // Detached on purpose: the handler holds nothing but the token, and it ends with the runtime.
    let _signal = tokio::spawn(watch_for_shutdown_signal(shutdown.clone()));

    let source: Arc<dyn ConfigSource> = Arc::new(LocalFileSource::new(bootstrap.local_config())?);
    serve(bootstrap, source, shutdown).await
}

/// Cancels the token on `SIGINT` or, where the platform has it, `SIGTERM`.
async fn watch_for_shutdown_signal(shutdown: CancellationToken) {
    wait_for_signal().await;
    tracing::info!("shutdown signal received; draining");
    shutdown.cancel();
}

#[cfg(unix)]
async fn wait_for_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    match signal(SignalKind::terminate()) {
        Ok(mut terminate) => {
            tokio::select! {
                result = tokio::signal::ctrl_c() => report_signal(result, "SIGINT"),
                _ = terminate.recv() => tracing::info!("SIGTERM received"),
            }
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                "SIGTERM handling is unavailable; only SIGINT will stop the gateway"
            );
            report_signal(tokio::signal::ctrl_c().await, "SIGINT");
        }
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() {
    report_signal(tokio::signal::ctrl_c().await, "SIGINT");
}

/// Logs how a signal wait ended. A failed handler must not be mistaken for a signal.
fn report_signal(result: io::Result<()>, name: &'static str) {
    match result {
        Ok(()) => tracing::info!(signal = name, "interrupt received"),
        Err(error) => tracing::warn!(error = %error, signal = name, "signal handling failed"),
    }
}

/// What the command line asked for.
enum Invocation {
    /// Print usage and succeed.
    Help,
    /// Run the pipeline described by this bootstrap file.
    Bootstrap(PathBuf),
}

/// The command line does not match the accepted shape.
struct UsageProblem;

/// Parses `--help` or `--bootstrap <path>`.
fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Invocation, UsageProblem> {
    let mut args = args.into_iter();
    match args.next() {
        Some(flag) if flag == "--help" => Ok(Invocation::Help),
        Some(flag) if flag == "--bootstrap" => {
            let path = args.next().ok_or(UsageProblem)?;
            if args.next().is_some() {
                return Err(UsageProblem);
            }
            Ok(Invocation::Bootstrap(PathBuf::from(path)))
        }
        _ => Err(UsageProblem),
    }
}

/// Writes [`USAGE`] to the given sink.
fn write_usage(mut sink: impl Write) {
    // A failed write to the terminal must not replace the exit code computed from the arguments.
    let _ = writeln!(sink, "{USAGE}");
}

/// Installs the tracing subscriber once per process.
///
/// Called from [`run`] rather than from [`serve`], because a test may serve several pipelines in
/// one process and only the first subscriber can win. Logs go to stderr so stdout stays reserved for
/// the command-line contract.
fn init_subscriber() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_error| {
        // An unset or unparsable `RUST_LOG` must not silence a fresh process's lifecycle events.
        EnvFilter::new("info")
    });
    // A second call is not an error: the first subscriber stays installed.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn no_arguments_is_reported_as_usage() {
        assert_eq!(run(args(&[])), ExitCode::from(USAGE_EXIT_CODE));
    }

    #[test]
    fn help_exits_successfully() {
        assert_eq!(run(args(&["--help"])), ExitCode::SUCCESS);
    }

    #[test]
    fn an_unknown_argument_is_rejected() {
        assert_eq!(run(args(&["--nope"])), ExitCode::from(USAGE_EXIT_CODE));
    }

    #[test]
    fn a_bootstrap_flag_without_a_path_is_rejected() {
        assert_eq!(run(args(&["--bootstrap"])), ExitCode::from(USAGE_EXIT_CODE));
    }

    #[test]
    fn extra_arguments_after_the_bootstrap_path_are_rejected() {
        assert_eq!(
            run(args(&["--bootstrap", "a.yaml", "extra"])),
            ExitCode::from(USAGE_EXIT_CODE)
        );
    }

    #[test]
    fn a_missing_bootstrap_file_fails_without_panicking() {
        assert_eq!(
            run(args(&["--bootstrap", "does-not-exist.yaml"])),
            ExitCode::from(FAILURE_EXIT_CODE)
        );
    }

    #[test]
    fn an_out_of_range_schema_bound_fails_at_startup() {
        let directory = tempfile::TempDir::new().expect("temporary directory");
        let path = directory.path().join("bootstrap.yaml");
        std::fs::write(
            &path,
            "\
config_source:
  type: local
  path: ./resources.yaml
  max_bytes: 4096
  watch: notify
debounce_ms: 100
max_wait_ms: 500
cache_dir: ./.tmp/cache
allow_bootstrap_cache: false
schema_bounds:
  max_depth: 0
",
        )
        .expect("the bootstrap fixture is written");
        let path = path.to_str().expect("the fixture path is UTF-8");

        assert_eq!(
            run(args(&["--bootstrap", path])),
            ExitCode::from(FAILURE_EXIT_CODE),
            "a zero bound must stop the process, not fall back to the default"
        );
    }

    #[test]
    fn parse_args_accepts_the_documented_invocations() {
        assert!(matches!(
            parse_args(args(&["--help"])),
            Ok(Invocation::Help)
        ));
        match parse_args(args(&["--bootstrap", "/etc/gateway/bootstrap.yaml"])) {
            Ok(Invocation::Bootstrap(path)) => {
                assert_eq!(path, PathBuf::from("/etc/gateway/bootstrap.yaml"));
            }
            Ok(Invocation::Help) => panic!("expected a bootstrap path"),
            Err(UsageProblem) => panic!("expected a bootstrap path"),
        }
    }

    #[test]
    fn parse_args_rejects_an_empty_invocation() {
        assert!(parse_args(args(&[])).is_err());
    }
}
