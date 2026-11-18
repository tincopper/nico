//! End-to-end verification of the etcd source wiring: a bootstrap file's `config_source.type:
//! etcd` becomes the running source, through the same [`build_source`] call `main` makes.
//!
//! The server on the other end is the in-process etcd protocol double (`nico-config-etcd`'s
//! `mock-server` feature), so the whole chain — bootstrap schema, source construction, the etcd
//! client's transports, and the adapter's contract — is exercised on a real gRPC wire without an
//! etcd environment. The pipeline itself is already verified source-agnostically in `local_file`;
//! what only this wiring can prove is that the bootstrap value composes into a working source.

// The panic-free policy protects request paths. A test helper that cannot build its fixture has
// nothing useful to return, and clippy only recognises the lint exemption inside `#[test]`
// functions, not in the helpers they call.
#![expect(
    clippy::expect_used,
    reason = "test helpers fail loudly when their fixture cannot be built"
)]
#![cfg(feature = "etcd")]

use nico_config::ConfigSourceError;
use nico_config_etcd::MockServer;
use nico_server::{BootstrapConfig, bootstrap::SourceSelection, build_source};

/// The bootstrap document with an etcd source pointing at `endpoint`.
fn etcd_bootstrap(endpoints: &str, key: &str) -> String {
    format!(
        "\
config_source:
  type: etcd
  endpoints:
{endpoints}
  key: {key}
  max_bytes: 65536
  debounce_ms: 50
  max_wait_ms: 200
  cache_dir: ./.tmp/cache
  allow_bootstrap_cache: false
http:
  listeners:
    - listen: 127.0.0.1:8080
"
    )
}

/// Loads a bootstrap document from a temporary file, exactly like the process would.
fn load_bootstrap(document: &str) -> BootstrapConfig {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let path = directory.path().join("bootstrap.yaml");
    std::fs::write(&path, document).expect("fixture write succeeds");
    BootstrapConfig::load(&path).expect("the etcd bootstrap is valid")
}

/// Drives one future to completion on this thread.
///
/// The source's calls are plain futures; the etcd client's transports live on their own daemon
/// thread (the construction's runtime), so the driving runtime is irrelevant to them.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("the test runtime builds")
        .block_on(future)
}

#[test]
fn an_etcd_bootstrap_builds_a_source_that_loads_the_document() {
    // One runtime for the whole test: the mock server's task lives on it, and dropping the
    // runtime early would kill the server the client is pointing at.
    block_on(async {
        let server = MockServer::start().await.expect("the mock etcd starts");
        let endpoint = server.endpoint();
        server.state().put(b"routes: []");

        let bootstrap =
            load_bootstrap(&etcd_bootstrap(&format!("    - {endpoint}"), "nico/config"));
        let SourceSelection::Etcd { .. } = bootstrap.source() else {
            panic!("the bootstrap selects the etcd source");
        };

        // `build_source` never blocks the caller: the client connects on its own daemon thread.
        let source = build_source(&bootstrap).expect("the etcd source builds");
        let version = source
            .load()
            .await
            .expect("the document loads over the real wire");

        assert_eq!(version.bytes().as_ref(), b"routes: []");
        assert_eq!(version.revision().seq(), 1);
    });
}

#[test]
fn an_etcd_bootstrap_with_every_endpoint_dead_still_builds_and_reports_unavailable() {
    // The client's connect is lazy: building the source never blocks on reachability, and the
    // failure surfaces through the source contract as "cannot be read" — the pipeline's reload
    // loop answers it, the same as a connection lost mid-run.
    let bootstrap = load_bootstrap(&etcd_bootstrap("    - 127.0.0.1:1", "nico/config"));

    let source = build_source(&bootstrap).expect("the etcd source builds without reachability");
    let error = block_on(source.load()).expect_err("nothing listens on port 1");

    assert!(
        matches!(
            error,
            ConfigSourceError::Unavailable {
                source_name: "etcd",
                ..
            }
        ),
        "expected Unavailable, got {error:?}"
    );
}
