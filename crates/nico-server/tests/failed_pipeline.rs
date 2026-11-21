//! A pipeline that can never publish stops the process instead of serving `503` for ever.
//!
//! This file is its own test target because the subject *is* the process: `Server::run` owns the
//! calling thread and installs the runtime it later tears down, so one real server per binary — and
//! never next to the fast unit tests. What only this target covers is the exit path after a
//! **failed** reload inside a real server; every other end-to-end target publishes successfully.
#![expect(
    clippy::expect_used,
    reason = "a fixture that cannot build its process has nothing useful to return"
)]

use std::{
    net::TcpListener,
    path::Path,
    process::ExitCode,
    sync::Arc,
    time::{Duration, Instant},
};

use nico_config::{ConfigSource, ConfigSourceError, FakeConfigSource};
use nico_server::{BootstrapConfig, FAILURE_EXIT_CODE, server};
use tokio_util::sync::CancellationToken;

/// Returns a port nothing is listening on, by binding one and releasing it.
///
/// A bootstrap `listen` may not name port zero, and Pingora never reports the port it bound, so the
/// fixture has to pick one. The listener is dropped immediately, leaving a window in which another
/// process could take the port; a fixed port would instead collide with whatever the developer's
/// machine already runs.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port is available");
    listener
        .local_addr()
        .expect("the bind has an address")
        .port()
}

/// Returns the trust root this fixture process is configured with.
///
/// Every bootstrap in this suite names one: the assembly refuses to start when the upstream trust
/// roots cannot be read, and a test runner is often a host whose platform store cannot be. The files
/// and their provenance are described in the workspace's `fixtures/tls/README.md`.
fn fixture_ca() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/tls/test-root.pem")
}

/// Returns a bootstrap file that listens on `port` and drains as fast as the settings allow.
fn bootstrap_file(directory: &Path, port: u16) -> BootstrapConfig {
    let path = directory.join("bootstrap.yaml");
    let cache_dir = directory.join("cache");
    let document = format!(
        "\
config_source:
  type: local
  path: ./resources.yaml
  max_bytes: 4096
  watch: notify
  debounce_ms: 100
  max_wait_ms: 500
  cache_dir: {}
  allow_bootstrap_cache: false
runtime:
  threads: 1
  grace_period_ms: 1
  graceful_shutdown_ms: 1000
proxy:
  tls:
    ca_file: \"{}\"
http:
  listeners:
    - listen: 127.0.0.1:{port}
",
        cache_dir.display(),
        fixture_ca().display(),
    );
    std::fs::write(&path, document).expect("the bootstrap fixture is written");
    BootstrapConfig::load(&path).expect("the bootstrap fixture is valid")
}

/// Returns a source whose every load fails, so the pipeline can never publish.
fn unusable_source() -> Arc<dyn ConfigSource> {
    Arc::new(
        FakeConfigSource::new("fake").push_load(Err(ConfigSourceError::Unavailable {
            source_name: "fake",
            source: Box::new(std::io::Error::other("the fixture source is unreachable")),
        })),
    )
}

#[test]
fn a_process_that_can_never_publish_stops_with_a_failure() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let bootstrap = bootstrap_file(directory.path(), free_port());

    let started = Instant::now();
    let code = server::serve(&bootstrap, unusable_source(), CancellationToken::new());
    let elapsed = started.elapsed();

    assert_eq!(
        code,
        ExitCode::from(FAILURE_EXIT_CODE),
        "a pipeline that cannot publish must not report success"
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "the drain window came from the bootstrap file, not Pingora's 300 s default (took {elapsed:?})"
    );
}
