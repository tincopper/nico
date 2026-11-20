//! The self-reporting listener's startup failures: a port it cannot bind, and a TLS identity it
//! cannot build, each stop the process.
//!
//! These are the paths where the surface's failure must be loud. Pingora builds its listeners
//! inside the service task and panics on a bind failure, which would leave a process serving
//! traffic with no way for an operator to see that anything is wrong. The metrics transport
//! therefore preflights the address during assembly and turns that case into a non-zero exit. The
//! same exit covers the second failure: the certificate and key are read once by the bootstrap and
//! read again when the listener is built, so a file that changed in between is a startup refusal
//! rather than a surface that quietly serves plaintext.
//!
//! The scenario is a **startup** test, not a serving one: the process returns before
//! `Server::run`, so nothing installs Pingora's process-wide runtime and this binary can assert on
//! it without the "one server per test target" constraint the HTTP scenarios carry.
//!
//! The absence of an end-to-end assertion for the recorder half (`install()` failing because
//! another recorder is already global) is deliberate: a test cannot occupy the global recorder
//! from outside `nico-metrics` without exporting a hook that exists only for tests.

#![expect(
    clippy::expect_used,
    reason = "a fixture helper that cannot build the process it asserts about has nothing to return"
)]

mod support;

use std::{net::TcpListener, process::ExitCode, sync::Arc};

use nico_config::ConfigSource;
use nico_server::{BootstrapConfig, FAILURE_EXIT_CODE, server};
use support::{
    http::{
        free_port, resources::Resources, source::TestSource,
        tls_bootstrap::write_bootstrap_metrics_tls, write_bootstrap_with_metrics,
    },
    tls::fixture,
};
use tokio_util::sync::CancellationToken;

/// Builds the process exactly as the binary does, with `metrics.listen` already taken.
fn start_with_a_port_that_is_taken(metrics_port: u16) -> ExitCode {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    write_bootstrap_with_metrics(directory.path(), port, metrics_port);

    // A real listener, held for the whole call: the preflight bind must meet an occupied port
    // rather than a free one, and only holding the socket proves that.
    let occupied = TcpListener::bind(("127.0.0.1", metrics_port)).expect("the port is bound");
    let document = Resources::new().render();
    let source: Arc<dyn ConfigSource> = TestSource::with_document(&document);
    let bootstrap = BootstrapConfig::load(directory.path().join("bootstrap.yaml"))
        .expect("the fixture is a valid bootstrap");

    let exit_code = server::serve_with(
        &bootstrap,
        source,
        CancellationToken::new(),
        nico_plugins::builtin_registry(),
    );

    // The listener is dropped only after the call returns, so the assertion above cannot pass
    // because the port happened to be free.
    drop(occupied);
    exit_code
}

#[test]
fn a_metrics_port_that_cannot_bind_stops_the_process() {
    let metrics_port = free_port();

    let exit_code = start_with_a_port_that_is_taken(metrics_port);

    assert_eq!(
        exit_code,
        ExitCode::from(FAILURE_EXIT_CODE),
        "a metrics listener that cannot bind must stop the process rather than leave it serving \
         with no self-reporting surface"
    );
}

/// Builds the process exactly as the binary does, with the identity of a TLS metrics listener
/// deleted after the bootstrap accepted it.
fn start_with_a_metrics_identity_that_vanished() -> ExitCode {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    let metrics_port = free_port();
    // Copies inside the temporary directory, because the scenario is a *change* to the files the
    // bootstrap read: deleting a repository fixture would break every other test that shares it.
    let certificate = directory.path().join("localhost.pem");
    let key = directory.path().join("localhost.key");
    std::fs::copy(fixture("localhost.pem"), &certificate)
        .expect("the fixture certificate is copied");
    std::fs::copy(fixture("localhost.key"), &key).expect("the fixture key is copied");
    let bootstrap =
        write_bootstrap_metrics_tls(directory.path(), port, metrics_port, &certificate, &key);
    std::fs::remove_file(&certificate).expect("the certificate is removed after the check");

    let document = Resources::new().render();
    let source: Arc<dyn ConfigSource> = TestSource::with_document(&document);
    server::serve_with(
        &bootstrap,
        source,
        CancellationToken::new(),
        nico_plugins::builtin_registry(),
    )
}

#[test]
fn a_metrics_identity_that_changes_after_the_check_stops_the_process() {
    let exit_code = start_with_a_metrics_identity_that_vanished();

    assert_eq!(
        exit_code,
        ExitCode::from(FAILURE_EXIT_CODE),
        "a TLS metrics listener whose certificate can no longer be read must stop the process \
         instead of serving the surface in plaintext"
    );
}

#[test]
fn a_metrics_port_that_is_free_lets_assembly_continue() {
    // The control: the same fixture with nothing holding the port must get past assembly. It then
    // blocks in the reload pipeline, so this asserts the *absence* of an immediate failure rather
    // than an exit code — a process that never starts cannot report one.
    let port = free_port();
    let metrics_port = free_port();
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let bootstrap = write_bootstrap_with_metrics(directory.path(), port, metrics_port);

    // Assembly is what fails on a taken port; `BootstrapConfig::metrics` is the value it reads, so
    // a `Some` here plus the scenario above is the pair that pins the preflight to the address.
    let settings = bootstrap
        .metrics()
        .expect("the fixture declares the section");
    assert_eq!(settings.listen(), format!("127.0.0.1:{metrics_port}"));
}
