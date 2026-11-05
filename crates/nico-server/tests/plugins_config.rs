//! A refused plugin document: the generation in service keeps serving, and the process says so.
//!
//! This is the accepted cost of materializing credentials at publication time. A credential reference
//! that cannot be read rejects the *whole* document — a partially materialized key set would accept
//! fewer tokens than the operator wrote — so the previously published generation stays in service and
//! the pipeline records the failure. A rotation therefore costs one reload, and a broken rotation
//! costs nothing at all.

mod support;

use std::{net::SocketAddr, time::Duration};

use support::{
    http::{
        RunningServer,
        client::{Request, exchange},
        free_port, log,
        resources::{Plugin, Resources, Route},
        source::TestSource,
        upstream::{MockUpstream, Reply},
        wait_for_status, write_bootstrap,
    },
    token,
};

/// The secret the fixture JWT is signed with.
const JWT_SECRET: &str = "an-hs256-secret-used-by-the-config-scenario";

/// How long the log assertions wait for a record Pingora writes after the response.
const LOG_BUDGET: Duration = Duration::from_secs(10);

/// Builds the fixture document: one JWT plugin, one route that names it.
fn document(secret_ref: &str, upstream: SocketAddr, path_prefix: &str, body_limit: u32) -> String {
    Resources::new()
        .body_limit(body_limit)
        .upstream("api-backend", upstream)
        .plugin(Plugin::jwt("public-jwt", secret_ref))
        .route(Route::new("api", path_prefix, "api-backend").plugins(&["public-jwt"]))
        .render()
}

#[tokio::test(flavor = "current_thread")]
async fn a_document_whose_credential_cannot_be_read_leaves_the_published_one_serving() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    write_bootstrap(directory.path(), port, false, 1, 1000);
    let secret = token::secret_file(directory.path(), "jwt.secret", JWT_SECRET);
    let secret_path = directory.path().join("jwt.secret");

    let upstream = MockUpstream::start(Reply::Body {
        body: b"ok".to_vec(),
        chunk: usize::MAX,
    })
    .await;

    let source = TestSource::with_document(&document(&secret, upstream.address(), "/api/", 1024));
    let server = RunningServer::start(directory.path(), source.clone()).await;
    let address = server.address();
    let logs = log::capture();

    let token = token::token(JWT_SECRET, 300, "user-42");
    let authenticated = |trace: &str| {
        Request::get("/api/items", "config.test")
            .header("authorization", format!("Bearer {token}"))
            .header("x-request-id", trace)
    };

    let response = wait_for_status(address, &authenticated("config-v1"), 200).await;
    assert_eq!(response.body(), b"ok");
    let line = log::wait_for_request_log(&logs, "config-v1", LOG_BUDGET).await;
    assert!(
        line.contains("config.generation=1"),
        "the first document is generation 1: {line}"
    );

    // The credential disappears and a new document arrives. Compiling it fails, so the document is
    // refused as a whole: nothing is half-published, and the generation in service is untouched.
    std::fs::remove_file(&secret_path).expect("the credential fixture is removed");
    source.publish(&document(&secret, upstream.address(), "/api/", 2048));
    let refused = log::wait_for_line_matching(&logs, LOG_BUDGET, |line| {
        line.contains("plugin compilation failed")
    })
    .await;
    assert!(
        refused.contains("public-jwt"),
        "the refusal must name the plugin at fault: {refused}"
    );
    assert!(
        refused.contains(&secret),
        "and the reference, not the value: {refused}"
    );
    assert!(
        !refused.contains(JWT_SECRET),
        "the credential value must never be logged: {refused}"
    );

    let response = exchange(address, &authenticated("config-still-v1")).await;
    assert_eq!(
        response.status(),
        200,
        "the generation in service still serves, got {}: {}",
        response.status(),
        response.text()
    );
    let line = log::wait_for_request_log(&logs, "config-still-v1", LOG_BUDGET).await;
    assert!(
        line.contains("config.generation=1"),
        "and it is still generation 1: {line}"
    );

    // Restoring the credential lets the next document through. It moves the route, so the wire says
    // which generation is answering rather than the log alone.
    std::fs::write(&secret_path, JWT_SECRET).expect("the credential fixture is restored");
    source.publish(&document(&secret, upstream.address(), "/api2/", 4096));
    let response = wait_for_status(
        address,
        &Request::get("/api2/items", "config.test")
            .header("authorization", format!("Bearer {token}")),
        200,
    )
    .await;
    assert_eq!(response.body(), b"ok", "the new generation is in service");
    let response = exchange(address, &authenticated("config-old-path")).await;
    assert_eq!(
        response.status(),
        404,
        "the old route is gone with the old generation, got {}",
        response.status()
    );

    let exit = server.cancel_and_join().await;
    assert_eq!(exit, std::process::ExitCode::SUCCESS);
}
