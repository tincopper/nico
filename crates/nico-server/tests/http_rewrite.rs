//! A traffic route's path rewrite, end to end: what the origin receives is the rewritten path, with
//! the query and the deep path byte for byte, and a failover's retry round cannot stack it.
//!
//! The origin echoes the request target it received — the only observer that can see what the
//! gateway forwarded — so every assertion here is about the bytes on the wire, not about the
//! document's intention.

mod support;

use support::http::{
    RunningServer,
    client::get,
    free_port,
    resources::{Resources, Route},
    source::TestSource,
    upstream::{MockUpstream, Reply},
    write_bootstrap,
};

#[tokio::test(flavor = "current_thread")]
async fn a_rewrite_prefix_replaces_the_matched_prefix_and_keeps_the_rest() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let _bootstrap = write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    let origin = MockUpstream::start(Reply::Path).await;

    let document = Resources::new()
        .upstream("internal", origin.address())
        .route(Route::new("api", "/api/", "internal").rewrite_prefix("/internal/"))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;

    // The matched prefix is replaced; the remainder and the query pass through byte for byte — the
    // encoded space stays encoded, the order of query parameters is the caller's.
    let response = get(server.address(), "/api/items?a=1&b=%20&c=2", "a.test").await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.text(),
        "/internal/items?a=1&b=%20&c=2",
        "the origin sees the rewritten path with the query it was handed"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_deep_path_keeps_every_byte_after_the_matched_prefix() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let _bootstrap = write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    let origin = MockUpstream::start(Reply::Path).await;

    let document = Resources::new()
        .upstream("internal", origin.address())
        .route(Route::new("api", "/api/", "internal").rewrite_prefix("/internal/v2/"))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;

    let response = get(server.address(), "/api/x/y/z", "a.test").await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.text(),
        "/internal/v2/x/y/z",
        "the replacement is a literal prefix swap: the deep path rides along"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_root_replacement_strips_the_matched_prefix() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let _bootstrap = write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    let origin = MockUpstream::start(Reply::Path).await;

    let document = Resources::new()
        .upstream("internal", origin.address())
        .route(Route::new("api", "/api/", "internal").rewrite_prefix("/"))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;

    let response = get(server.address(), "/api/items", "a.test").await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.text(),
        "/items",
        "a root replacement is what strips the matched prefix: the swap is literal"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_route_without_a_rewrite_forwards_the_client_path() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let _bootstrap = write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    let origin = MockUpstream::start(Reply::Path).await;

    let document = Resources::new()
        .upstream("plain", origin.address())
        .route(Route::new("api", "/api/", "plain"))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;

    let response = get(server.address(), "/api/items?a=1", "a.test").await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.text(),
        "/api/items?a=1",
        "no rewrite: the original path and query are forwarded as they arrived"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_failover_round_rewrites_once_not_twice() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let _bootstrap = write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    // The live origin echoes what it received: if the retry round rewrote from the already-rewritten
    // path, the origin would see `/internal/` nested inside itself instead of the one rewrite.
    let live = MockUpstream::start(Reply::Path).await;
    let dead = format!("127.0.0.1:{}", free_port())
        .parse()
        .expect("a loopback address");

    let document = Resources::new()
        .upstream_with_members(
            "fleet",
            "api.example.test",
            &[("dead", dead), ("live", live.address())],
        )
        .route(
            Route::new("api", "/api/", "fleet")
                .rewrite_prefix("/internal/")
                .retry_budget(2),
        )
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;

    let response = get(server.address(), "/api/items", "a.test").await;
    assert_eq!(
        response.status(),
        200,
        "the dead member's failure hands the request to the live one"
    );
    assert_eq!(
        response.text(),
        "/internal/items",
        "the retry round recomputes from the client's original path: one rewrite, not two"
    );
}
