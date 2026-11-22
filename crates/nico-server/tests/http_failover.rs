//! Same-request failover through a real Pingora server: a member that fails hands the request to the
//! next candidate, within the budget the route declares.
//!
//! Every scenario names the members in document order, and the upstream's default strategy walks them
//! in that order — so the first pick is deterministic and the walk's reach is what each test asserts.
//! The refusals a walk cannot avoid (budget spent, every candidate dead) answer with the pool's
//! existing `502 upstream_unreachable`, and the one answer a walk never replaces — a forwarded
//! upstream `5xx` — is asserted beside them.

// The panic-free policy protects request paths. A test helper that cannot build its fixture has
// nothing useful to return, and clippy only recognises the lint exemption inside `#[test]`
// functions, not in the helpers they call.
#![expect(
    clippy::expect_used,
    reason = "test helpers fail loudly when their fixture cannot be built"
)]

mod support;

use std::net::SocketAddr;

use support::http::{
    RunningServer,
    client::get,
    free_port,
    resources::{Resources, Route},
    source::TestSource,
    upstream::{MockUpstream, Reply},
    write_bootstrap,
};

/// The loopback address nothing listens on: a member whose connection attempt always fails.
///
/// Each call reserves its own port, so two dead members in one scenario refuse independently.
fn dead_member() -> SocketAddr {
    format!("127.0.0.1:{}", free_port())
        .parse()
        .expect("a loopback address")
}

#[tokio::test(flavor = "current_thread")]
async fn a_dead_member_hands_the_request_to_the_next_candidate() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let _bootstrap = write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    let live = MockUpstream::start(Reply::Body {
        body: b"stable".to_vec(),
        chunk: usize::MAX,
    })
    .await;
    let dead = dead_member();

    let document = Resources::new()
        .upstream_with_members(
            "fleet",
            "api.example.test",
            &[("dead", dead), ("live", live.address())],
        )
        .route(Route::new("api", "/api/", "fleet").retry_budget(2))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;

    let response = get(server.address(), "/api/items", "a.test").await;
    assert_eq!(
        response.status(),
        200,
        "the dead member's failure hands the request to the live one: {}",
        response.text()
    );
    assert_eq!(response.text(), "stable");
}

#[tokio::test(flavor = "current_thread")]
async fn the_default_budget_answers_the_failure_without_failover() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let _bootstrap = write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    let live = MockUpstream::start(Reply::Body {
        body: b"stable".to_vec(),
        chunk: usize::MAX,
    })
    .await;
    let dead = dead_member();

    let document = Resources::new()
        .upstream_with_members(
            "fleet",
            "api.example.test",
            &[("dead", dead), ("live", live.address())],
        )
        .route(Route::new("api", "/api/", "fleet"))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;

    let response = get(server.address(), "/api/items", "a.test").await;
    assert_eq!(
        response.status(),
        502,
        "a budget of one is the shape every route had before failover: the failure is the answer"
    );
    assert_eq!(
        response.json_code().as_deref(),
        Some("upstream_unreachable")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_walk_that_exhausts_its_candidates_answers_unreachable() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let _bootstrap = write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    let dead = dead_member();
    let dead_two = dead_member();

    let document = Resources::new()
        .upstream_with_members(
            "fleet",
            "api.example.test",
            &[("dead", dead), ("dead-two", dead_two)],
        )
        .route(Route::new("api", "/api/", "fleet").retry_budget(2))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;

    let response = get(server.address(), "/api/items", "a.test").await;
    assert_eq!(
        response.status(),
        502,
        "every candidate failed within the budget, and the walk has nothing left"
    );
    assert_eq!(
        response.json_code().as_deref(),
        Some("upstream_unreachable")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_forwarded_upstream_error_is_never_replaced_by_failover() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let _bootstrap = write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    // The first member answers `500` itself: an origin's own error is forwarded as it arrived, and
    // the walk — whose budget would allow another member — must not replace it.
    let failing = MockUpstream::start(Reply::Response {
        status: 500,
        headers: Vec::new(),
        body: b"the origin said so".to_vec(),
        chunk: usize::MAX,
    })
    .await;
    let live = MockUpstream::start(Reply::Body {
        body: b"stable".to_vec(),
        chunk: usize::MAX,
    })
    .await;

    let document = Resources::new()
        .upstream_with_members(
            "fleet",
            "api.example.test",
            &[("failing", failing.address()), ("live", live.address())],
        )
        .route(Route::new("api", "/api/", "fleet").retry_budget(2))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;

    let response = get(server.address(), "/api/items", "a.test").await;
    assert_eq!(
        response.status(),
        500,
        "the origin's error is the origin's answer: forwarded, not failed over"
    );
    assert_eq!(response.text(), "the origin said so");
}
