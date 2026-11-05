//! Rate limiting through a real server: a burst, the wait it reports, and per-subject isolation.
//!
//! The clock here is the real one. A paused clock cannot reach the server thread, so the bucket's
//! refill is driven by a tiny `per_second` instead — which is why the burst is asserted to run out
//! *within* `burst + 1` exchanges rather than after exactly `burst` of them: a machine slow enough to
//! cross a second between two requests has refilled one token, and that is the bucket working. The
//! exchanges take milliseconds locally, so the assertion's real assumption is that no single request
//! is delayed by a whole second.
//!
//! The route lists its limit **before** its authentication plugin on purpose. If the document's order
//! were the execution order, an unauthenticated request would spend a token of somebody else's quota;
//! the first scenario proves it does not.

mod support;

use std::time::Duration;

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
const JWT_SECRET: &str = "an-hs256-secret-used-by-the-limit-scenario";

/// Requests the bucket admits back to back.
const BURST: usize = 2;

/// How long the log assertions wait for a record Pingora writes after the response.
const LOG_BUDGET: Duration = Duration::from_secs(10);

#[tokio::test(flavor = "current_thread")]
async fn a_bucket_runs_out_per_subject_and_reports_when_to_come_back() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    write_bootstrap(directory.path(), port, false, 1, 1000);
    let secret = token::secret_file(directory.path(), "jwt.secret", JWT_SECRET);

    let upstream = MockUpstream::start(Reply::Body {
        body: b"ok".to_vec(),
        chunk: usize::MAX,
    })
    .await;

    let document = Resources::new()
        .upstream("api-backend", upstream.address())
        .plugin(Plugin::jwt("public-jwt", &secret))
        .plugin(Plugin::rate_limit("api-quota", BURST as u32, 1))
        .route(
            Route::new("limited", "/limited/", "api-backend").plugins(&["api-quota", "public-jwt"]),
        )
        .route(Route::new("open", "/open/", "api-backend"))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source.clone()).await;
    let address = server.address();
    let logs = log::capture();

    // The route that names no plugin is the readiness signal *and* the control: it answers as soon
    // as the document is in service, and it must never be limited.
    let open = Request::get("/open/items", "limit.test");
    let response = wait_for_status(address, &open, 200).await;
    assert_eq!(response.body(), b"ok");

    // An unauthenticated request on the limited route is refused by authentication, not by the
    // limit — and it spends no token, which the burst below would expose.
    let request = Request::get("/limited/items", "limit.test").header("x-request-id", "limit-anon");
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        401,
        "authentication runs before the limit, got {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(response.header("x-nico-error-code"), Some("auth_missing"));

    let subject_a = token::token(JWT_SECRET, 300, "subject-a");
    // A round spends the bucket and is refused within `BURST + 1` exchanges unless the machine takes
    // a whole second between two of them — a second is a token, so that is the bucket working, not a
    // failure. A second round makes a one-off stall recoverable while still failing a limiter that
    // never refuses. Every exchange carries its own trace id: the request log is matched by it, and a
    // repeated id would hand the assertions an earlier, admitted request.
    let mut refused = None;
    'burst: for round in 0..2 {
        for attempt in 0..=BURST {
            let trace = format!("limit-a-{round}-{attempt}");
            let request = Request::get("/limited/items", "limit.test")
                .header("authorization", format!("Bearer {subject_a}"))
                .header("x-request-id", trace.clone());
            let response = exchange(address, &request).await;
            if response.status() == 429 {
                refused = Some((trace, response));
                break 'burst;
            }
            assert_eq!(
                response.status(),
                200,
                "request {attempt} of round {round} must be admitted, got {}: {}",
                response.status(),
                response.text()
            );
        }
    }
    let (refused_trace, refused) =
        refused.expect("the burst is exhausted within burst + 1 exchanges in one round");

    assert_eq!(refused.header("x-nico-error-code"), Some("rate_limited"));
    assert_eq!(refused.json_code().as_deref(), Some("rate_limited"));
    assert_eq!(
        refused.header("retry-after"),
        Some("1"),
        "one token per second means the caller waits one second"
    );
    assert!(
        refused.header("x-nico-trace-id").is_some(),
        "a limit refusal keeps the correlation id"
    );
    let line = log::wait_for_request_log(&logs, &refused_trace, LOG_BUDGET).await;
    assert!(
        line.contains("error.category=Limit"),
        "a rate limit is the caller's quota, not a gateway fault: {line}"
    );
    assert!(
        line.contains("authn=public-jwt"),
        "the record still names who the request was charged to: {line}"
    );

    // A second subject has its own bucket: the first one's exhaustion says nothing about it.
    let subject_b = token::token(JWT_SECRET, 300, "subject-b");
    let request = Request::get("/limited/items", "limit.test")
        .header("authorization", format!("Bearer {subject_b}"))
        .header("x-request-id", "limit-b-0");
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        200,
        "a bucket is per subject, got {}: {}",
        response.status(),
        response.text()
    );

    // The unlimited route is unaffected by everything above.
    for _ in 0..BURST + 2 {
        let response = exchange(address, &open).await;
        assert_eq!(
            response.status(),
            200,
            "a route with no plugins is never limited, got {}: {}",
            response.status(),
            response.text()
        );
    }

    let exit = server.cancel_and_join().await;
    assert_eq!(exit, std::process::ExitCode::SUCCESS);
}
