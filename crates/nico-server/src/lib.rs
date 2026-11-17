//! Gateway binary support library.
//!
//! The crate owns everything a gateway process does around its data plane:
//!
//! - [`bootstrap`] — reads the local-only file that selects one configuration source and states
//!   how far the process may trust its cache.
//! - [`reload`] — the pipeline that loads, validates, de-bounces and publishes configuration
//!   snapshots, keeping the last known good one in service when anything fails.
//! - [`llm`] — the LLM plan one document compiles into: the client codecs, provider adapters and
//!   materialized credentials a converted route is served with, published with the snapshot it was
//!   compiled from.
//! - [`serve`] — wires one source to one publication handle and runs the pipeline until shutdown.
//! - [`server`] — assembles the process: Pingora's server, the pipeline on its runtime, and the
//!   HTTP data plane that reads the published snapshots.
//! - `metrics` — the self-reporting transport: a separate Pingora listener that delegates every
//!   request to `nico-metrics` and writes back its response.
//! - [`run`] — the command line: parse arguments, build the local source, map problems to exit
//!   codes.
//!
//! `main.rs` is a thin shim over [`run`], so the whole wiring is reachable from an integration
//! test. Two entry points exist on purpose: [`server::serve`] is production (Pingora owns the
//! runtime and the sockets) and [`serve`] is the pipeline seam a test drives without a socket. Both
//! build the pipeline through the same [`Reloader`], so there is no second reload implementation.
#![deny(missing_docs)]

use std::{
    ffi::OsString,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
};

use nico_config::{CacheStore, ConfigSource};
use nico_core::SnapshotHandle;
use nico_http::HttpStateHandle;
use nico_plugins::PluginRuntime;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

pub mod bootstrap;
/// The AI role's compilation surface: client codecs, provider adapters and credentials.
///
/// Compiled only with the `llm` feature. The traffic role has no such module, and the pipeline
/// reaches it through [`reload::LlmCompiler`] so nothing above this line names `nico-llm`.
#[cfg(feature = "llm")]
pub mod llm;
mod metrics;
pub mod reload;
pub mod server;
mod source_build;

pub use bootstrap::{BootstrapConfig, BootstrapError};
#[cfg(feature = "llm")]
pub use llm::{
    CredentialError, CredentialResolver, EnvFileCredentials, LlmBuildError, LlmPlan, LlmRuntime,
};
pub use reload::{LlmCompiler, Publication, ReloadError, ReloadLimits, ReloadSettings, Reloader};
pub use source_build::{SourceBuildError, build_source};

/// Exit code used when the command line is missing or not understood.
pub const USAGE_EXIT_CODE: u8 = 2;

/// Exit code used when the process could not reach a serving state, or stopped with an error.
pub const FAILURE_EXIT_CODE: u8 = 1;

/// Command-line usage summary.
pub const USAGE: &str = "usage: nico --bootstrap <path>";

/// Runs the configuration pipeline for one source until `shutdown` is cancelled.
///
/// This is the pipeline seam a test drives in-process; production composes the same [`Reloader`]
/// with a real HTTP data plane in [`server::serve`]. Nothing here builds a runtime: the caller
/// owns it, and in production that owner is Pingora.
///
/// The cache is always written after a successful publish; it is only read when startup fails and
/// `bootstrap.allow_bootstrap_cache()` is set, so passing `true` never changes the running
/// behaviour of a healthy process.
///
/// The plugin runtime is built from `PluginRuntime::new()`, which is `with_registry(builtin_registry())`:
/// one constructor, so this seam loads the same kinds the assembled process does and a document that
/// compiles here cannot be refused there for a missing kind. It takes no registry parameter because it
/// publishes into a handle nothing serves from — a kind a test registers would have no request to
/// observe. [`server::serve_with`] is the seam that carries one.
///
/// The LLM plan is compiled with the built-in runtime for the same reason: `Reloader::new` assembles
/// `LlmRuntime::builtin()`, so a provider route that compiles here is one the assembled process could
/// serve, and a document whose AI route cannot be served is refused in both places alike.
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
    // This seam serves no traffic, so the request-path half of the publication is discarded. It is
    // still wired through the same constructor production uses, so the two paths cannot diverge.
    let publication = Publication::new(SnapshotHandle::new(), HttpStateHandle::new());
    let settings = ReloadSettings::new(
        Some(CacheStore::new(bootstrap.cache_dir().to_path_buf())),
        ReloadLimits::new(bootstrap.debounce(), bootstrap.max_wait()),
        bootstrap.parse_limits(),
        bootstrap.allow_bootstrap_cache(),
        shutdown,
    );
    let reloader = Reloader::new(
        source,
        publication,
        Arc::new(PluginRuntime::new()),
        settings,
    );
    reloader.run().await
}

/// Handles the command line and returns the process exit code.
///
/// `--help` prints [`USAGE`] to stdout and succeeds. A missing, duplicated or unknown argument
/// prints usage to stderr and returns [`USAGE_EXIT_CODE`]. A bootstrap file that cannot be read or
/// accepted, a configuration source that cannot be built, or a pipeline that never reaches a
/// serving state returns [`FAILURE_EXIT_CODE`].
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

    let source = match build_source(&bootstrap) {
        Ok(source) => source,
        Err(error) => {
            tracing::error!(
                error = %error,
                error.category = ?error.category(),
                "the configuration source could not be built"
            );
            return ExitCode::from(FAILURE_EXIT_CODE);
        }
    };

    // Pingora owns the runtime from here: no Tokio runtime is built in this function, and the
    // reload pipeline runs on Pingora's.
    server::serve(&bootstrap, source, CancellationToken::new())
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
