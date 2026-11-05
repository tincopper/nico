//! Harness for the end-to-end HTTP data-plane tests: one real Pingora server per test binary.
//!
//! The subject of those tests is the wire, so nothing here wraps an HTTP library. A test gets:
//!
//! - [`write_bootstrap`] and [`free_port`], because the bootstrap file rejects port zero and Pingora
//!   never reports the port it actually bound;
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
    path::Path,
    process::ExitCode,
    sync::Arc,
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
/// A bootstrap `listen` may not name port zero, and Pingora never reports the port it bound, so the
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

/// Writes a bootstrap file that listens on `port` and returns the parsed configuration.
///
/// The debounce windows are zero: a test publishes a document and then waits for it to be in
/// service, and a debounce would only add a second way for that wait to be slow.
pub(crate) fn write_bootstrap(
    directory: &Path,
    port: u16,
    h2c: bool,
    grace_period_ms: u64,
    graceful_shutdown_ms: u64,
) -> BootstrapConfig {
    let path = directory.join("bootstrap.yaml");
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
http:
  listen: 127.0.0.1:{port}
  h2c: {h2c}
  grace_period_ms: {grace_period_ms}
  graceful_shutdown_ms: {graceful_shutdown_ms}
  threads: 1
",
        document = directory.join("resources.yaml").display(),
        cache = directory.join("cache").display(),
    );
    std::fs::write(&path, text).expect("the bootstrap fixture is written");
    BootstrapConfig::load(&path).expect("the bootstrap fixture is valid")
}

pub(crate) mod resources;

// --- running server ----------------------------------------------------------

/// One real gateway process: Pingora's server, the reload pipeline and the HTTP data plane.
pub(crate) struct RunningServer {
    address: SocketAddr,
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

    /// Starts the production assembly with `plugins` as the set of kinds it can load.
    ///
    /// The scenario files that assert on the plugin driver register [`mock`](mock) here, so the
    /// production registry stays exactly what a deployment gets; everything else uses [`Self::start`].
    pub(crate) async fn start_with_registry(
        directory: &Path,
        source: Arc<dyn ConfigSource>,
        plugins: nico_http::PluginRegistry,
    ) -> Self {
        let bootstrap = BootstrapConfig::load(directory.join("bootstrap.yaml"))
            .expect("the bootstrap fixture loads");
        let address: SocketAddr = bootstrap
            .http()
            .listen()
            .parse()
            .expect("the fixture listens on an ip:port address");
        let shutdown = CancellationToken::new();
        let token = shutdown.clone();
        let thread = std::thread::Builder::new()
            .name("nico-test-server".to_owned())
            .spawn(move || server::serve_with(&bootstrap, source, token, plugins))
            .expect("the server thread starts");

        let server = Self {
            address,
            shutdown,
            thread: Some(thread),
        };
        server.wait_until_accepting().await;
        server
    }

    /// Returns the address the server listens on.
    pub(crate) fn address(&self) -> SocketAddr {
        self.address
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
