//! The public-network smoke: the platform trust store, a real CA and a real certificate.
//!
//! Every other TLS scenario signs its own root, which proves the connector reads the bundle it is
//! given and nothing about whether the *default* source works. This one leaves `tls` out of the
//! bootstrap — so the process uses the platform store — and asks a public site for its front page.
//!
//! It is `#[ignore]`d because it needs two things a gate cannot promise: outbound network access and
//! a host whose platform trust store is readable. When the store is not readable the process refuses
//! to start, which is the designed behaviour (see `nico_http::ensure_usable`), so the premise is
//! asserted here rather than left to appear as a server that never binds.
//!
//! Run it by hand:
//!
//! ```sh
//! cargo test -p nico-server --test tls_public -- --ignored --nocapture
//! ```

mod support;

use nico_http::{TlsPolicy, ensure_usable};
use support::http::{
    RunningServer,
    client::{Request, exchange},
    free_port,
    resources::{Resources, Route},
    source::TestSource,
    write_bootstrap_on_platform_roots,
};

/// The public origin, chosen because it needs no credential and answers a GET with `200`.
const PUBLIC_HOST: &str = "example.com";

/// The scheme's default port, which the certificate is signed for under the same name.
const PUBLIC_PORT: u16 = 443;

#[tokio::test(flavor = "current_thread")]
#[ignore = "reaches a public https site: needs network access and a readable platform trust store"]
async fn a_public_https_upstream_completes_a_handshake() {
    // The premise, asserted where the failure can name it. Without this the assembly would exit with
    // `FAILURE_EXIT_CODE` and the scenario would report a bind timeout instead.
    if let Err(error) = ensure_usable(&TlsPolicy::default()) {
        panic!("this scenario needs a readable platform trust store: {error}");
    }

    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    write_bootstrap_on_platform_roots(directory.path(), port);

    let document = Resources::new()
        .tls_upstream("public", PUBLIC_HOST, PUBLIC_PORT)
        .route(Route::new("public", "/", "public"))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;

    let response = exchange(server.address(), &Request::get("/", "public.test")).await;
    assert_eq!(
        response.status(),
        200,
        "a public https origin answered {}: {}",
        response.status(),
        response.text()
    );
    assert!(
        !response.body().is_empty(),
        "a completed handshake produced a body"
    );
}
