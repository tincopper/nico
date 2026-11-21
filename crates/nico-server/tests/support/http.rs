//! Harness for the end-to-end HTTP data-plane tests: one real Pingora server per test binary.
//!
//! The subject of those tests is the wire, so nothing here wraps an HTTP library. A test gets:
//!
//! - [`write_bootstrap`] and [`free_port`], because the bootstrap file rejects port zero and Pingora
//!   never reports the port it actually bound; every bootstrap it writes names [`fixture_ca`] as its
//!   trust root, so no scenario depends on the host's certificate store;
//! - [`resources::Resources`], a builder for a complete resources document (the schema rejects a
//!   missing `providers`, `upstreams`, `routes` or `plugins` section, so every fixture must spell them
//!   out), plus the [`resources::Route`] and [`resources::Plugin`] entries a scenario varies;
//! - [`TestSource`](source::TestSource), a configuration source the test publishes into, so a
//!   document can be in service before the first request or only after one;
//! - [`RunningServer`], which runs the **production** assembly (`nico_server::server::serve_with`, the
//!   function `serve` itself delegates to) on its own thread and stops it through the same shutdown
//!   token the binary uses;
//! - [`MockUpstream`](upstream::MockUpstream) and [`Client`](client::Client), the two wire peers;
//! - [`mock`], a plugin kind only a test registers, so a scenario can make the driver prove what it
//!   called and in which order.
//!
//! One server per test binary. `Server::run` owns the calling thread and installs the runtimes it
//! later tears down, so a test binary starts exactly one and every scenario in the file talks to it.
//! `net::TcpListener::bind("127.0.0.1:0")` is how the tests find a free port; `lib.rs` explains why
//! the port cannot simply be zero.
//!
//! Each integration test is its own crate, so it compiles only the helpers it calls and leaves the
//! rest of this module unreachable — which is what `dead_code` reports. The exemption is per test
//! crate and disappears as soon as an unused helper is deleted.
#![expect(
    dead_code,
    reason = "an integration test compiles this module but uses only the helpers it calls"
)]
#![expect(
    clippy::expect_used,
    reason = "a helper that cannot build its fixture has nothing useful to return"
)]

pub(crate) mod client;
pub(crate) mod log;
pub(crate) mod mock;
pub(crate) mod source;
pub(crate) mod upstream;

use std::{
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use nico_config::ConfigSource;
use nico_server::{BootstrapConfig, server};
use tokio::{net::TcpStream, time::timeout};
use tokio_util::sync::CancellationToken;

use self::client::{Request, Response};

/// How long a fixture waits for a server to bind, or for a document to reach service.
pub(crate) const STARTUP_BUDGET: Duration = Duration::from_secs(20);

/// How long one connect probe may take; well below [`STARTUP_BUDGET`] so probing stays cheap.
const PROBE_BUDGET: Duration = Duration::from_millis(250);

/// Interval between two connect probes. The listen socket has no notification to await, so the only
/// way to observe it is to try to reach it.
const PROBE_INTERVAL: Duration = Duration::from_millis(10);

// --- ports and bootstrap -----------------------------------------------------

/// Returns a port nothing is listening on, by binding one and releasing it.
///
/// A listener entry may not name port zero, and Pingora never reports the port it bound, so the
/// fixture has to pick one. The listener is dropped immediately, which leaves a window in which
/// another process could take the port; a fixed port would instead collide with whatever the
/// developer's machine already runs, so the release-then-rebind race is the smaller risk.
pub(crate) fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is available");
    listener
        .local_addr()
        .expect("the bind has an address")
        .port()
}

/// Returns the trust root every fixture process is configured with.
///
/// A test process must not depend on the host's certificate store. Pingora reads it while building
/// its connector, the assembly refuses to start when it cannot be read, and a sandboxed or headless
/// runner is often exactly such a host. Naming a bundle makes the root set a fact of the fixture
/// instead — and it is the same root the TLS scenarios sign their listeners' certificates with.
///
/// See the workspace's `fixtures/tls/README.md` for what the files are and how they were produced.
pub(crate) fn fixture_ca() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/tls/test-root.pem")
}

/// Writes a bootstrap file that listens on `port` and returns the parsed configuration.
///
/// The debounce windows are zero: a test publishes a document and then waits for it to be in
/// service, and a debounce would only add a second way for that wait to be slow. The trust root is
/// [`fixture_ca`], which is what every scenario but the untrusted-chain one configures.
pub(crate) fn write_bootstrap(
    directory: &Path,
    port: u16,
    h2c: bool,
    grace_period_ms: u64,
    graceful_shutdown_ms: u64,
) -> BootstrapConfig {
    write(
        directory,
        port,
        h2c,
        grace_period_ms,
        graceful_shutdown_ms,
        Some(&fixture_ca()),
        None,
    )
}

/// Writes a bootstrap that trusts `ca` instead of [`fixture_ca`].
///
/// Only the untrusted-chain scenario needs this: it asserts what a process does when the peer's
/// chain leads to a root set this process was **not** given, which is a fact about the configured
/// `proxy.tls.ca_file` and not about the peer. Every other scenario uses the common fixture.
pub(crate) fn write_bootstrap_trusting(directory: &Path, port: u16, ca: &Path) -> BootstrapConfig {
    write(directory, port, false, 1, 1000, Some(ca), None)
}

/// Writes the same bootstrap as [`write_bootstrap`] **plus** a `metrics` section on `metrics_port`.
///
/// A separate function rather than an extra parameter: the three-argument form is called from every
/// scenario that reads the surface, and the self-reporting surface is a property only some of them
/// need. A missing `metrics` section is what makes the surface default-off, so the absence has its own
/// helper ([`write_bootstrap`]) and stays the common case.
pub(crate) fn write_bootstrap_with_metrics(
    directory: &Path,
    port: u16,
    metrics_port: u16,
) -> BootstrapConfig {
    write_bootstrap_with_metrics_and_grace(directory, port, metrics_port, 5000, 30000)
}

/// Writes the metrics bootstrap with an explicit drain window.
///
/// The drain is Pingora's `grace_period_seconds`, so a scenario that has nothing to assert about it
/// pays the whole window in wall-clock time on every run. The suite runs this on every push, so the
/// one scenario that reads metrics *and* starts many requests says how long it wants to wait.
pub(crate) fn write_bootstrap_with_metrics_and_grace(
    directory: &Path,
    port: u16,
    metrics_port: u16,
    grace_period_ms: u64,
    graceful_shutdown_ms: u64,
) -> BootstrapConfig {
    write(
        directory,
        port,
        false,
        grace_period_ms,
        graceful_shutdown_ms,
        Some(&fixture_ca()),
        Some(metrics_port),
    )
}

/// Writes a bootstrap with **no** `proxy` section, so the process uses the platform trust store.
///
/// Only the public-network smoke test wants this: every other scenario names a bundle, because a
/// test runner's platform store is not a fact the suite may depend on.
pub(crate) fn write_bootstrap_on_platform_roots(directory: &Path, port: u16) -> BootstrapConfig {
    write(directory, port, false, 1, 1000, None, None)
}

/// Renders the bootstrap file every helper above shares, and returns it parsed.
///
/// One renderer for the three shapes: the fields a scenario varies are arguments, and the rest — a
/// zero debounce, one worker thread, a local source — is stated here once. A fixture that the
/// bootstrap schema rejects fails while the test is being written, not while it runs.
fn write(
    directory: &Path,
    port: u16,
    h2c: bool,
    grace_period_ms: u64,
    graceful_shutdown_ms: u64,
    ca: Option<&Path>,
    metrics_port: Option<u16>,
) -> BootstrapConfig {
    let metrics = metrics_port.map_or_else(String::new, |port| {
        format!("metrics:\n  listen: 127.0.0.1:{port}\n")
    });
    // An absent `proxy` section is the platform store; a present one is a bundle that replaces it.
    let proxy = ca.map_or_else(String::new, |ca| {
        format!("proxy:\n  tls:\n    ca_file: \"{}\"\n", ca.display())
    });
    let text = format!(
        "\
config_source:
  type: local
  path: {document}
  max_bytes: 1048576
  watch: notify
  debounce_ms: 0
  max_wait_ms: 0
  cache_dir: {cache}
  allow_bootstrap_cache: false
runtime:
  threads: 1
  grace_period_ms: {grace_period_ms}
  graceful_shutdown_ms: {graceful_shutdown_ms}
{proxy}http:
  h2c: {h2c}
  listeners:
    - listen: 127.0.0.1:{port}
{metrics}",
        document = directory.join("resources.yaml").display(),
        cache = directory.join("cache").display(),
    );
    write_and_load(directory, &text)
}

/// Writes `text` as the bootstrap file and returns it parsed.
///
/// A file the schema rejects fails while the test is being written, not while it runs, so every
/// fixture shape ends here rather than repeating the write and the load.
fn write_and_load(directory: &Path, text: &str) -> BootstrapConfig {
    let path = directory.join("bootstrap.yaml");
    std::fs::write(&path, text).expect("the bootstrap fixture is written");
    BootstrapConfig::load(&path).expect("the bootstrap fixture is valid")
}

pub(crate) mod resources;
pub(crate) mod tls_bootstrap;

// --- running server ----------------------------------------------------------

/// What [`RunningServer::start`] waits for before it returns.
///
/// The distinction is a property of the scenario, not a knob: a scenario that asserts on a served
/// request needs a published generation, and a scenario that asserts the not-ready window needs the
/// absence of one. Making it explicit keeps both readable, where a boolean argument would not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wait {
    /// Wait until a generation reached service.
    Published,
    /// Return as soon as the listener accepts a connection.
    ListenerOnly,
}

/// One real gateway process: Pingora's server, the reload pipeline and the HTTP data plane.
pub(crate) struct RunningServer {
    address: SocketAddr,
    /// Where the self-reporting surface listens, when the bootstrap enabled it.
    metrics_address: Option<SocketAddr>,
    shutdown: CancellationToken,
    thread: Option<JoinHandle<ExitCode>>,
}

impl RunningServer {
    /// Starts the production assembly with every plugin kind this build implements.
    ///
    /// `directory` must already contain a bootstrap file created by [`write_bootstrap`].
    pub(crate) async fn start(directory: &Path, source: Arc<dyn ConfigSource>) -> Self {
        Self::start_with_registry(directory, source, nico_plugins::builtin_registry()).await
    }

    /// Starts the production assembly and returns as soon as the listener accepts a connection.
    ///
    /// **Only** for the scenario that asserts the not-ready window itself: a source that holds its
    /// answer publishes nothing, so waiting for a generation would never return. Every other test
    /// wants [`Self::start`], which waits for the generation and therefore cannot race the pipeline.
    pub(crate) async fn start_before_first_publish(
        directory: &Path,
        source: Arc<dyn ConfigSource>,
    ) -> Self {
        Self::start_inner(
            directory,
            source,
            nico_plugins::builtin_registry(),
            Wait::ListenerOnly,
        )
        .await
    }

    /// Starts the production assembly with `plugins` as the set of kinds it can load.
    ///
    /// The scenario files that assert on the plugin driver register [`mock`](mock) here, so the
    /// production registry stays exactly what a deployment gets; everything else uses [`Self::start`].
    pub(crate) async fn start_with_registry(
        directory: &Path,
        source: Arc<dyn ConfigSource>,
        plugins: nico_http::PluginRegistry,
    ) -> Self {
        Self::start_inner(directory, source, plugins, Wait::Published).await
    }

    /// Starts the production assembly and waits for `wait`.
    async fn start_inner(
        directory: &Path,
        source: Arc<dyn ConfigSource>,
        plugins: nico_http::PluginRegistry,
        wait: Wait,
    ) -> Self {
        let bootstrap = BootstrapConfig::load(directory.join("bootstrap.yaml"))
            .expect("the bootstrap fixture loads");
        // The first listener is the one a scenario talks to: a multi-listener fixture writes its
        // plaintext entry first, and a TLS-only one writes its single entry. Reading the address
        // from the parsed bootstrap rather than from the caller keeps the fixture and the file that
        // describes it from disagreeing; any listener after the first is reached through the port
        // the scenario chose before writing the file.
        let address: SocketAddr = bootstrap.http().listener_addresses()[0]
            .parse()
            .expect("the fixture listens on an ip:port address");
        // Read from the parsed bootstrap rather than from the caller, so a scenario cannot believe a
        // surface is enabled that the file did not actually ask for.
        let metrics_address: Option<SocketAddr> = bootstrap.metrics().map(|settings| {
            settings
                .listen()
                .parse()
                .expect("a metrics address is ip:port")
        });
        // Before the thread exists: the subscriber has to be installed before the process can write
        // its first line, or the publication this fixture waits for could be logged into nothing.
        // `capture` is idempotent, so a scenario that already called it gets the same buffer.
        let captured = log::capture();
        let shutdown = CancellationToken::new();
        let token = shutdown.clone();
        let thread = std::thread::Builder::new()
            .name("nico-test-server".to_owned())
            .spawn(move || server::serve_with(&bootstrap, source, token, plugins))
            .expect("the server thread starts");

        let server = Self {
            address,
            metrics_address,
            shutdown,
            thread: Some(thread),
        };
        server.wait_until_accepting().await;
        if wait == Wait::Published {
            server.wait_until_published(&captured).await;
        }
        server
    }

    /// Returns the address the server listens on.
    pub(crate) fn address(&self) -> SocketAddr {
        self.address
    }

    /// Returns the self-reporting address, or `None` when the bootstrap declared no `metrics` section.
    ///
    /// A test that asserts the surface is default-off asserts `None` here, and the data plane's own
    /// 404 on `/metrics` is the second half of the same claim.
    pub(crate) fn metrics_address(&self) -> Option<SocketAddr> {
        self.metrics_address
    }

    /// Starts a graceful shutdown without waiting for it, for tests that observe the drain.
    pub(crate) fn begin_shutdown(&self) {
        self.shutdown.cancel();
    }

    /// Cancels the shutdown token and waits for the server thread to return its exit code.
    pub(crate) async fn cancel_and_join(mut self) -> ExitCode {
        self.shutdown.cancel();
        let thread = self
            .thread
            .take()
            .expect("the server thread is joined once");
        // Blocking on the join keeps the current-thread runtime free, so mock upstreams and clients
        // can keep making progress while the server drains.
        tokio::task::spawn_blocking(move || thread.join())
            .await
            .expect("the join task is not cancelled")
            .expect("the server thread did not panic")
    }

    /// Waits until a generation reached service, or [`STARTUP_BUDGET`] is spent.
    ///
    /// Accepting a connection is not the same fact. The listener is bound before the configuration
    /// pipeline publishes its first snapshot, and a request that arrives in that window is answered
    /// `503 not_ready` — correct behaviour, and a scenario that asserts on its *first* request would
    /// read it as a defect. Waiting on the socket alone left that window open, and it widened under
    /// the parallel load of a full test run.
    ///
    /// The signal is the pipeline's own publication line, which is the fact "a generation reached
    /// service". The caller installs the capturing subscriber *before* starting the server thread and
    /// hands the buffer in, so a line written the instant the pipeline boots is not missed.
    async fn wait_until_published(&self, captured: &Arc<Mutex<Vec<u8>>>) {
        // The helper panics with the captured log when the budget is spent, which is the diagnosis
        // a scenario that never published needs.
        log::wait_for_line_matching(captured, STARTUP_BUDGET, |line| {
            line.contains("runtime snapshot published")
        })
        .await;
    }

    /// Polls the listen socket until a connection is accepted or [`STARTUP_BUDGET`] is spent.
    async fn wait_until_accepting(&self) {
        let deadline = Instant::now() + STARTUP_BUDGET;
        loop {
            let probe = timeout(PROBE_BUDGET, TcpStream::connect(self.address)).await;
            if matches!(probe, Ok(Ok(_stream))) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the server never accepted a connection on {} within {STARTUP_BUDGET:?}",
                self.address
            );
            tokio::time::sleep(PROBE_INTERVAL).await;
        }
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        // A test that failed mid-scenario must not leave the server draining for its whole grace
        // period while the runtime is being torn down.
        self.shutdown.cancel();
    }
}

// --- waiting for a document --------------------------------------------------

/// Repeats `request` on fresh connections until it answers `status`, or fails after the budget.
///
/// Publishing a document is asynchronous: the pipeline validates it, publishes a new generation and
/// only then can a request match it. There is no global handle to await from a test, so the only
/// observable event is the answer itself, and this waits for that answer instead of guessing a
/// sleep.
pub(crate) async fn wait_for_status(
    address: SocketAddr,
    request: &Request,
    status: u16,
) -> Response {
    let deadline = Instant::now() + STARTUP_BUDGET;
    loop {
        let response = client::exchange(address, request).await;
        if response.status() == status {
            return response;
        }
        assert!(
            Instant::now() < deadline,
            "{} {} never answered {status} within {STARTUP_BUDGET:?}; the last answer was {} {:?}",
            request.method(),
            request.path(),
            response.status(),
            response.text()
        );
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}
