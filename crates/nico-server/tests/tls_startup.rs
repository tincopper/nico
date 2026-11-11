//! A trust bundle that cannot be read when the service is assembled stops the process.
//!
//! The bootstrap file is validated when it is parsed, but Pingora opens the bundle *again* while it
//! builds its client connector, and a failure there is `build_connector(...).unwrap()` inside
//! `HttpProxy::new` — a panic with no field name and no clean exit. The assembly therefore re-reads
//! the source once more, immediately before the service is built, and refuses to start if it cannot.
//!
//! This file is its own test target because it asserts a **process** outcome: the exit code and the
//! line an operator reads. The refusal happens before `Server::new`, so nothing installs Pingora's
//! global runtime and this binary can hold more than one scenario.
//!
//! The platform-store half of the same rule is unit-tested in `nico-http` (`tls::tests::probe`):
//! whether a host's platform store can be read is not something a test can construct portably, so
//! the rule is tested on the slice it decides and the wiring is exercised here.

mod support;

use std::{process::ExitCode, sync::Arc};

use nico_config::ConfigSource;
use nico_server::{BootstrapConfig, FAILURE_EXIT_CODE, server};
use support::http::{
    fixture_ca, free_port, log, resources::Resources, source::TestSource, write_bootstrap_trusting,
};
use tokio_util::sync::CancellationToken;

/// The line the assembly writes when the upstream trust roots cannot be used.
const REFUSAL: &str = "trust roots";

/// Builds the process exactly as the binary does, from `bootstrap`.
fn launch(bootstrap: &BootstrapConfig) -> ExitCode {
    let document = Resources::new().render();
    let source: Arc<dyn ConfigSource> = TestSource::with_document(&document);
    server::serve_with(
        bootstrap,
        source,
        CancellationToken::new(),
        nico_plugins::builtin_registry(),
    )
}

#[test]
fn a_bundle_that_disappeared_between_bootstrap_and_assembly_stops_the_process() {
    let captured = log::capture();
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();

    // A copy this test owns: the fixture is shared with every other scenario in the suite, so
    // removing it here must not be observable anywhere else.
    let bundle = directory.path().join("roots.pem");
    std::fs::copy(fixture_ca(), &bundle).expect("the fixture bundle is copied");
    let bootstrap = write_bootstrap_trusting(directory.path(), port, &bundle);
    std::fs::remove_file(&bundle).expect("the copied bundle is removed");

    let code = launch(&bootstrap);

    assert_eq!(
        code,
        ExitCode::from(FAILURE_EXIT_CODE),
        "a process that cannot read its trust roots must not report success"
    );
    // The capture is one buffer per process and this file's scenarios run in parallel, so the line
    // is identified by its own wording rather than by being the first refusal in the buffer.
    let text = log::text(&captured);
    let line = text
        .lines()
        .find(|line| line.contains(REFUSAL) && line.contains("could not be read"))
        .unwrap_or_else(|| panic!("the refusal was not logged; captured:\n{text}"));
    assert!(
        line.contains("tls.ca_file"),
        "the line names the remedy an operator can act on: {line}"
    );
}

#[test]
fn a_bundle_that_is_not_a_trust_root_set_stops_the_process() {
    // The bootstrap parse accepts nothing that is not a certificate, so this can only be reached by
    // replacing the file after it was validated — the same window the re-read exists for. A working
    // bundle is written first, then swapped for a key.
    let captured = log::capture();
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();

    let bundle = directory.path().join("roots.pem");
    std::fs::copy(fixture_ca(), &bundle).expect("the fixture bundle is copied");
    let bootstrap = write_bootstrap_trusting(directory.path(), port, &bundle);
    std::fs::write(
        &bundle,
        "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49\n-----END PRIVATE KEY-----\n",
    )
    .expect("the bundle is replaced");

    let code = launch(&bootstrap);

    assert_eq!(
        code,
        ExitCode::from(FAILURE_EXIT_CODE),
        "a bundle the connector would `unwrap()` on must not be allowed through"
    );
    let text = log::text(&captured);
    assert!(
        text.lines()
            .any(|line| line.contains(REFUSAL) && line.contains("not usable")),
        "the refusal names the reason, not just the file; captured:\n{text}"
    );
}
