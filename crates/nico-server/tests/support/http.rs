//! Harness for the end-to-end HTTP data-plane tests: one real Pingora server per test binary.
//!
//! The subject of those tests is the wire, so nothing here wraps an HTTP library. A test gets:
//!
//! - [`write_bootstrap`] and [`free_port`], because the bootstrap file rejects port zero and Pingora
//!   never reports the port it actually bound;
//! - [`Resources`], a builder for a complete resources document (the schema rejects a missing
//!   `providers`, `upstreams` or `routes` section, so every fixture must spell them out);
//! - [`TestSource`](source::TestSource), a configuration source the test publishes into, so a
//!   document can be in service before the first request or only after one;
//! - [`RunningServer`], which runs the **production** assembly (`nico_server::server::serve`) on
//!   its own thread and stops it through the same shutdown token the binary uses;
//! - [`MockUpstream`](upstream::MockUpstream) and [`Client`](client::Client), the two wire peers.
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

// --- resources document ------------------------------------------------------

/// What a route forwards to: exactly one of the two, like the document itself.
#[derive(Debug, Clone)]
enum Target {
    Upstream(String),
    Provider(String),
}

/// One `routes` entry of a fixture document.
#[derive(Debug, Clone)]
pub(crate) struct Route {
    id: String,
    host: Option<String>,
    path_prefix: String,
    target: Target,
    timeout_ms: Option<u32>,
    max_body_bytes: Option<u32>,
}

impl Route {
    /// A route for every host that forwards `path_prefix` to an upstream.
    pub(crate) fn new(id: &str, path_prefix: &str, upstream: &str) -> Self {
        Self {
            id: id.to_owned(),
            host: None,
            path_prefix: path_prefix.to_owned(),
            target: Target::Upstream(upstream.to_owned()),
            timeout_ms: None,
            max_body_bytes: None,
        }
    }

    /// Restricts the route to one exact `Host`.
    pub(crate) fn host(mut self, host: &str) -> Self {
        self.host = Some(host.to_owned());
        self
    }

    /// Adds the route-level timeout override, which may only tighten the document limit.
    pub(crate) fn timeout_ms(mut self, timeout_ms: u32) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }

    /// Adds the route-level body bound, which may only tighten the document limit.
    pub(crate) fn max_body_bytes(mut self, max_body_bytes: u32) -> Self {
        self.max_body_bytes = Some(max_body_bytes);
        self
    }
}

/// A complete resources document, rendered from parts.
///
/// The defaults describe a usable document: a 1 MiB body bound, a 30 s whole-request budget, and no
/// providers, upstreams or routes until the test adds one.
#[derive(Debug, Clone)]
pub(crate) struct Resources {
    max_request_body_bytes: u32,
    request_timeout_ms: u32,
    providers: Vec<String>,
    upstreams: Vec<(String, SocketAddr)>,
    routes: Vec<Route>,
}

impl Resources {
    /// A document with the fixture defaults and nothing else.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Sets `limits.max_request_body_bytes`.
    pub(crate) fn body_limit(mut self, bytes: u32) -> Self {
        self.max_request_body_bytes = bytes;
        self
    }

    /// Sets `limits.request_timeout_ms`.
    pub(crate) fn timeout_ms(mut self, timeout_ms: u32) -> Self {
        self.request_timeout_ms = timeout_ms;
        self
    }

    /// Adds the provider a provider route can name.
    pub(crate) fn provider(mut self, name: &str) -> Self {
        self.providers.push(name.to_owned());
        self
    }

    /// Adds an upstream reachable at `address`.
    pub(crate) fn upstream(mut self, name: &str, address: SocketAddr) -> Self {
        self.upstreams.push((name.to_owned(), address));
        self
    }

    /// Adds a route. A provider route is added with [`Resources::provider_route`].
    pub(crate) fn route(mut self, route: Route) -> Self {
        self.routes.push(route);
        self
    }

    /// Adds a route that forwards to a provider instead of an upstream.
    pub(crate) fn provider_route(mut self, id: &str, path_prefix: &str, provider: &str) -> Self {
        let route = Route {
            id: id.to_owned(),
            host: None,
            path_prefix: path_prefix.to_owned(),
            target: Target::Provider(provider.to_owned()),
            timeout_ms: None,
            max_body_bytes: None,
        };
        self.routes.push(route);
        self
    }

    /// Renders the document as YAML.
    pub(crate) fn render(&self) -> String {
        let mut yaml = format!(
            "\
apiVersion: gateway/v1
limits:
  max_request_body_bytes: {body}
  request_timeout_ms: {timeout}
",
            body = self.max_request_body_bytes,
            timeout = self.request_timeout_ms,
        );

        if self.providers.is_empty() {
            yaml.push_str("providers: []\n");
        } else {
            yaml.push_str("providers:\n");
            for name in &self.providers {
                yaml.push_str(&format!(
                    "  - name: {name}\n    kind: openai\n    base_url: https://api.example.test\n    credential_ref: env:FIXTURE_KEY\n"
                ));
            }
        }

        if self.upstreams.is_empty() {
            yaml.push_str("upstreams: []\n");
        } else {
            yaml.push_str("upstreams:\n");
            for (name, address) in &self.upstreams {
                yaml.push_str(&format!(
                    "  - name: {name}\n    scheme: http\n    host: {host}\n    port: {port}\n",
                    host = address.ip(),
                    port = address.port(),
                ));
            }
        }

        if self.routes.is_empty() {
            yaml.push_str("routes: []\n");
        } else {
            yaml.push_str("routes:\n");
            for route in &self.routes {
                yaml.push_str(&format!(
                    "  - id: {id}\n    path_prefix: {prefix}\n",
                    id = route.id,
                    prefix = route.path_prefix,
                ));
                if let Some(host) = &route.host {
                    yaml.push_str(&format!("    host: {host}\n"));
                }
                match &route.target {
                    Target::Upstream(name) => yaml.push_str(&format!("    upstream: {name}\n")),
                    Target::Provider(name) => yaml.push_str(&format!("    provider: {name}\n")),
                }
                if let Some(timeout_ms) = route.timeout_ms {
                    yaml.push_str(&format!("    timeout_ms: {timeout_ms}\n"));
                }
                if let Some(max_body_bytes) = route.max_body_bytes {
                    yaml.push_str(&format!("    max_body_bytes: {max_body_bytes}\n"));
                }
            }
        }

        yaml
    }
}

impl Default for Resources {
    fn default() -> Self {
        Self {
            max_request_body_bytes: 1024 * 1024,
            request_timeout_ms: 30_000,
            providers: Vec::new(),
            upstreams: Vec::new(),
            routes: Vec::new(),
        }
    }
}

// --- running server ----------------------------------------------------------

/// One real gateway process: Pingora's server, the reload pipeline and the HTTP data plane.
pub(crate) struct RunningServer {
    address: SocketAddr,
    shutdown: CancellationToken,
    thread: Option<JoinHandle<ExitCode>>,
}

impl RunningServer {
    /// Starts the production assembly on its own thread and waits until it accepts connections.
    ///
    /// `directory` must already contain a bootstrap file created by [`write_bootstrap`].
    pub(crate) async fn start(directory: &Path, source: Arc<dyn ConfigSource>) -> Self {
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
            .spawn(move || server::serve(&bootstrap, source, token))
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
