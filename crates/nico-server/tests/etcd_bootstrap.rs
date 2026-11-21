//! End-to-end verification of the etcd source wiring: a bootstrap file's `config_source.type:
//! etcd` becomes the running source, through the same [`build_source`] call `main` makes.
//!
//! The server on the other end is the in-process etcd protocol double (`nico-config-etcd`'s
//! `mock-server` feature), so the whole chain — bootstrap schema, source construction, the etcd
//! client's transports, and the adapter's contract — is exercised on a real gRPC wire without an
//! etcd environment. The pipeline itself is already verified source-agnostically in `local_file`;
//! what only this wiring can prove is that the bootstrap value composes into a working source.
//!
//! One scenario is the transport a production cluster asks for: a `tls` block and a credential
//! reference become a TLS connection that proves the certificate, exchanges the password for a
//! token, and reads the document. It is the only place where the *file* a reference names, the
//! *bytes* the trust root holds, and the client's own assemble-then-connect order are asserted
//! together.

// The panic-free policy protects request paths. A test helper that cannot build its fixture has
// nothing useful to return, and clippy only recognises the lint exemption inside `#[test]`
// functions, not in the helpers they call.
#![expect(
    clippy::expect_used,
    reason = "test helpers fail loudly when their fixture cannot be built"
)]
#![cfg(feature = "etcd")]

use std::path::{Path, PathBuf};

use nico_config::ConfigSourceError;
use nico_config_etcd::{EtcdError, MockServer};
use nico_server::{BootstrapConfig, bootstrap::SourceSelection, build_source};

/// The workspace's shared TLS fixtures, above any one crate — see `fixtures/tls/README.md`.
///
/// The leaf certificate the TLS mock presents on loopback: `DNS:localhost, IP:127.0.0.1`.
const LOCALHOST_PEM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/tls/localhost.pem"
));
/// The private key of [`LOCALHOST_PEM`].
const LOCALHOST_KEY: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/tls/localhost.key"
));
/// The trust root [`LOCALHOST_PEM`] chains to.
const TEST_ROOT_PEM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/tls/test-root.pem"
));

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

/// Drives one future to completion and returns its output.
///
/// The source's calls are plain futures; the etcd client's transports live on their own daemon
/// thread (the construction's runtime), so the client does not need this runtime's worker threads.
/// The mock server does: it is a task on this runtime, and building a source *blocks* the calling
/// thread until the client has connected — with a single-threaded runtime that block would starve
/// the server that is supposed to answer, so the test would deadlock on its own harness.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
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

/// The password the TLS scenarios configure, with the trailing newline a `file:` reference keeps.
const PASSWORD: &[u8] = b"s3cret\n";

/// The bootstrap document for a TLS endpoint, its trust root and its credential reference.
///
/// `auth` is omitted when `password_file` is `None`, which is the shape of a bootstrap that was
/// written before the cluster enabled authentication.
fn tls_bootstrap(
    endpoint: &str,
    ca_file: &Path,
    domain_name: &str,
    password_file: Option<&Path>,
) -> String {
    let auth = password_file.map_or_else(String::new, |path| {
        format!(
            "  auth:\n    user: nico\n    password_ref: file:{}\n",
            path.display()
        )
    });
    format!(
        "\
config_source:
  type: etcd
  endpoints:
    - {endpoint}
  key: nico/config
  max_bytes: 65536
  tls:
    ca_file: {}
    domain_name: {domain_name}
{}  debounce_ms: 50
  max_wait_ms: 200
  cache_dir: ./.tmp/cache
  allow_bootstrap_cache: false
http:
  listeners:
    - listen: 127.0.0.1:8080
",
        ca_file.display(),
        auth
    )
}

/// Writes the trust root and the password file a TLS bootstrap names, and returns their paths.
///
/// The directory is returned so the caller keeps it alive: `build_source` reads the trust root a
/// second time, which is the behaviour the files have to survive.
fn tls_files(directory: &Path) -> (PathBuf, PathBuf) {
    let ca_file = directory.join("ca.pem");
    std::fs::write(&ca_file, TEST_ROOT_PEM).expect("trust root write succeeds");
    let password_file = directory.join("password");
    std::fs::write(&password_file, PASSWORD).expect("password write succeeds");
    (ca_file, password_file)
}

#[test]
fn an_etcd_bootstrap_with_tls_and_credentials_reads_the_document() {
    block_on(async {
        // The server presents the loopback leaf and requires the credential pair; the password is
        // the file's bytes exactly, trailing newline included — a reader that trimmed it would fail
        // the exchange here rather than in production.
        let server = MockServer::builder()
            .identity(LOCALHOST_PEM, LOCALHOST_KEY)
            .credentials("nico", "s3cret\n")
            .start()
            .await
            .expect("the mock etcd starts");
        server.state().put(b"routes: []");

        let directory = tempfile::TempDir::new().expect("temporary directory");
        let (ca_file, password_file) = tls_files(directory.path());
        let endpoint = format!("https://localhost:{}", server.port());
        let bootstrap = load_bootstrap(&tls_bootstrap(
            &endpoint,
            &ca_file,
            "localhost",
            Some(&password_file),
        ));

        let SourceSelection::Etcd { tls, auth, .. } = bootstrap.source() else {
            panic!("the bootstrap selects the etcd source");
        };
        assert!(tls.is_some() && auth.is_some(), "both blocks are carried");

        let source = build_source(&bootstrap).expect("the etcd source builds");
        let version = source
            .load()
            .await
            .expect("the document loads over the TLS wire");

        assert_eq!(version.bytes().as_ref(), b"routes: []");
    });
}

#[test]
fn a_tls_source_whose_server_requires_credentials_reports_auth() {
    block_on(async {
        let server = MockServer::builder()
            .identity(LOCALHOST_PEM, LOCALHOST_KEY)
            .credentials("nico", "s3cret\n")
            .start()
            .await
            .expect("the mock etcd starts");
        server.state().put(b"routes: []");

        let directory = tempfile::TempDir::new().expect("temporary directory");
        let (ca_file, _password_file) = tls_files(directory.path());
        let endpoint = format!("https://localhost:{}", server.port());
        // No `auth` block: the client connects anonymously and the token check is what refuses it.
        let bootstrap = load_bootstrap(&tls_bootstrap(&endpoint, &ca_file, "localhost", None));

        let source = build_source(&bootstrap).expect("an anonymous client still builds");
        let error = source
            .load()
            .await
            .expect_err("the server refuses a request that presents no token");

        let ConfigSourceError::Unavailable { source, .. } = &error else {
            panic!("expected Unavailable, got {error:?}");
        };
        assert!(
            source
                .downcast_ref::<EtcdError>()
                .is_some_and(|error| matches!(error, EtcdError::Auth { .. })),
            "the adapter folds a refusal into Unavailable, and the cause stays typed"
        );
    });
}

#[test]
fn an_unreachable_endpoint_with_credentials_refuses_to_start() {
    // Credentials are exchanged for a token while the client connects, so reachability is a startup
    // fact once they are configured: a bootstrap that meant to authenticate must not keep serving
    // because the server never got the chance to refuse it.
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let password_file = directory.path().join("password");
    std::fs::write(&password_file, PASSWORD).expect("password write succeeds");
    let document = format!(
        "\
config_source:
  type: etcd
  endpoints:
    - 127.0.0.1:1
  key: nico/config
  max_bytes: 65536
  auth:
    user: nico
    password_ref: file:{}
  debounce_ms: 50
  max_wait_ms: 200
  cache_dir: ./.tmp/cache
  allow_bootstrap_cache: false
http:
  listeners:
    - listen: 127.0.0.1:8080
",
        password_file.display()
    );
    let bootstrap = load_bootstrap(&document);

    let error = match build_source(&bootstrap) {
        Ok(_) => panic!("nothing listens on port 1, so the source cannot be built"),
        Err(error) => error,
    };

    assert_eq!(
        error.category(),
        nico_core::ErrorCategory::Upstream,
        "an endpoint that did not answer is the dependency's failure, got {error:?}"
    );
}
