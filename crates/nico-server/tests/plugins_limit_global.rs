//! Rate limiting across the two scopes: who a global bucket is keyed by, and how the scopes layer.
//!
//! Three statements, all about *scope* rather than about the limiter's arithmetic — the bucket math
//! has its own unit tests in `nico-plugins` with a clock they control:
//!
//! - the global authenticator runs before the global limiter in the same scope, so the bucket is
//!   keyed by the subject it established, not by the caller's address (which two subjects share);
//! - a global bucket carries no route id, so one subject's quota spans every route, while a second
//!   subject starts with its own full bucket on the very route that just refused the first;
//! - a route-scope limiter charges the same request again. Its bucket is one request deeper than the
//!   global one, so it refuses first, and the request log names which instance refused each time.
//!
//! The clock is the real one and `per_second` is 1: an exchange that takes a whole second refills a
//! token. Every exhaustion assertion is therefore "within these exchanges", and each scenario runs up
//! to two rounds with a fresh subject — a one-off stall is recoverable, a limiter that never refuses
//! or one that keys by address is not.
//!
//! One server, one document, one scenario: the exchanges below are steps of one story about one
//! table, and a second server in this binary would test Pingora's global runtime instead.

mod support;

use std::time::Duration;

use support::{
    http::{
        RunningServer,
        client::{Request, Response, exchange},
        free_port, log,
        resources::{Plugin, Resources, Route},
        source::TestSource,
        upstream::{MockUpstream, Reply},
        wait_for_status, write_bootstrap,
    },
    token,
};

/// The secret the fixture JWT is signed with.
const JWT_SECRET: &str = "an-hs256-secret-used-by-the-global-limit-scenario";

/// Requests the global bucket admits back to back before the shared one is spent.
const GLOBAL_BURST: u32 = 2;

/// How long one token takes to come back, in seconds: `per_second: 1` in the document below.
const PER_SECOND: u32 = 1;

/// The routes that spend the shared bucket, one token each.
const ADMIT_ROUTES: [&str; GLOBAL_BURST as usize] = ["cross", "other"];

/// How long the log assertions wait for a record Pingora writes after the response.
const LOG_BUDGET: Duration = Duration::from_secs(10);

#[tokio::test(flavor = "current_thread")]
async fn a_global_bucket_is_keyed_by_the_subject_and_the_route_scope_charges_again() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    write_bootstrap(directory.path(), port, false, 1, 1000);
    let secret = token::secret_file(directory.path(), "jwt.secret", JWT_SECRET);

    let upstream = MockUpstream::start(Reply::Body {
        body: b"ok".to_vec(),
        chunk: usize::MAX,
    })
    .await;

    // `edge-quota` is listed **before** the authenticator on purpose: the class order still puts
    // `edge` in front, which is the premise of everything below — a limiter that ran first would see
    // no subject and fall back to the peer address, and the second subject at the end of step 3 would
    // be refused by the first one's spent bucket.
    //
    // `route-quota` is a *second* instance with a burst of one: a distinct instance keeps its own
    // bucket table, so the layered step can tell which scope refused without reading the key format.
    let document = Resources::new()
        .upstream("api-backend", upstream.address())
        .plugin(Plugin::jwt("edge", &secret))
        .plugin(Plugin::rate_limit("edge-quota", GLOBAL_BURST, PER_SECOND))
        .plugin(Plugin::rate_limit("route-quota", 1, PER_SECOND))
        .global_plugins(&["edge-quota", "edge"])
        .route(Route::new("cross", "/cross/", "api-backend"))
        .route(Route::new("other", "/other/", "api-backend"))
        .route(Route::new("third", "/third/", "api-backend"))
        .route(Route::new("layered", "/layered/", "api-backend").plugins(&["route-quota"]))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source.clone()).await;
    let address = server.address();
    let logs = log::capture();

    // 1. No credential: the global authenticator refuses before routing has anything to match. This
    //    401 is also the readiness signal — the document is in service as soon as it can be refused
    //    on its content.
    let anonymous = Request::get("/cross/items", "limit-global.test");
    let refused = wait_for_status(address, &anonymous, 401).await;
    assert_eq!(
        refused.header("x-nico-error-code"),
        Some("auth_missing"),
        "the global authenticator runs before routing, got {}: {}",
        refused.status(),
        refused.text()
    );

    // 2. One subject, three routes, one bucket. Each route matches a different route id, so a bucket
    //    that were per route would admit all three; a bucket keyed by address would also admit all
    //    three, because the three exchanges share one peer. The third refusal is what rules out both.
    //    `ADMIT_ROUTES` is `GLOBAL_BURST` long by construction, so the admission count cannot drift
    //    away from the document's burst without a compile error.
    let mut shared = None;
    'shared: for round in 0..2 {
        let subject = token::token(JWT_SECRET, 300, &format!("shared-{round}"));
        for (attempt, route) in ADMIT_ROUTES.into_iter().enumerate() {
            let trace = format!("shared-{round}-admit-{attempt}");
            let request = Request::get(&format!("/{route}/items"), "limit-global.test")
                .header("authorization", format!("Bearer {subject}"))
                .header("x-request-id", trace);
            let response = exchange(address, &request).await;
            assert_eq!(
                response.status(),
                200,
                "request {attempt} of round {round} on /{route}/ must be admitted, got {}: {}",
                response.status(),
                response.text()
            );
        }
        let trace = format!("shared-{round}-exhausted");
        let request = Request::get("/third/items", "limit-global.test")
            .header("authorization", format!("Bearer {subject}"))
            .header("x-request-id", trace.clone());
        let response = exchange(address, &request).await;
        match response.status() {
            429 => {
                shared = Some((trace, response));
                break 'shared;
            }
            200 => continue 'shared,
            other => panic!(
                "a third route answers 200 or 429, got {other}: {}",
                response.text()
            ),
        }
    }
    let (refused_trace, refused) =
        shared.expect("the global bucket is spent within burst + 1 exchanges in one round");

    assert_eq!(refused.header("x-nico-error-code"), Some("rate_limited"));
    assert_eq!(refused.json_code().as_deref(), Some("rate_limited"));
    assert_eq!(
        refused.header("retry-after"),
        Some("1"),
        "one token per second means the caller waits one second"
    );
    let line = log::wait_for_request_log(&logs, &refused_trace, LOG_BUDGET).await;
    assert!(
        line.contains("error.category=Limit"),
        "a rate limit is the caller's quota, not a gateway fault: {line}"
    );
    assert!(
        line.contains("plugin=edge-quota"),
        "the refusal names the global instance that refused: {line}"
    );
    assert!(
        line.contains("authn=edge"),
        "the bucket was charged to the subject `edge` established: {line}"
    );

    // 3. A second subject, on the route that just refused the first. Its bucket is untouched: if the
    //    limiter had keyed by the peer address (both exchanges come from the same test process) this
    //    would be a 429, which is what makes the class ordering above load-bearing.
    let second = token::token(JWT_SECRET, 300, "a-second-subject");
    let request = Request::get("/third/items", "limit-global.test")
        .header("authorization", format!("Bearer {second}"))
        .header("x-request-id", "limit-second-subject");
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        200,
        "a bucket is per subject, not per peer, got {}: {}",
        response.status(),
        response.text()
    );

    // 4. Both scopes charge the same request. The route's bucket holds one token, the global bucket
    //    `GLOBAL_BURST`, so the route refuses one request **before** the global limiter does — and the
    //    global chain runs first, so on its turn it refuses before the route chain is reached. The
    //    order of the two refusals is therefore the evidence that both scopes ran, one after the
    //    other, on a single request.
    let mut refusals: Vec<(String, Response)> = Vec::new();
    'layered: for round in 0..2 {
        refusals.clear();
        let subject = token::token(JWT_SECRET, 300, &format!("layered-{round}"));
        for attempt in 0..=GLOBAL_BURST + 1 {
            let trace = format!("layered-{round}-{attempt}");
            let request = Request::get("/layered/items", "limit-global.test")
                .header("authorization", format!("Bearer {subject}"))
                .header("x-request-id", trace.clone());
            let response = exchange(address, &request).await;
            if response.status() == 200 {
                continue;
            }
            assert_eq!(
                response.status(),
                429,
                "exchange {attempt} of round {round} answers 200 or 429, got {}: {}",
                response.status(),
                response.text()
            );
            refusals.push((trace, response));
            if refusals.len() == 2 {
                break 'layered;
            }
        }
    }
    assert_eq!(
        refusals.len(),
        2,
        "both scopes spend within one round: the global burst is {GLOBAL_BURST}, the route's is 1"
    );
    assert_eq!(
        refusals[0].1.header("x-nico-error-code"),
        Some("rate_limited"),
        "the first refusal is the route scope's"
    );
    let route_line = log::wait_for_request_log(&logs, &refusals[0].0, LOG_BUDGET).await;
    assert!(
        route_line.contains("plugin=route-quota"),
        "the route's bucket is one request deeper, so it refuses first: {route_line}"
    );
    assert_eq!(
        refusals[1].1.header("x-nico-error-code"),
        Some("rate_limited"),
        "the second refusal is the global scope's"
    );
    let global_line = log::wait_for_request_log(&logs, &refusals[1].0, LOG_BUDGET).await;
    assert!(
        global_line.contains("plugin=edge-quota"),
        "the global bucket is spent one request later: {global_line}"
    );

    let exit = server.cancel_and_join().await;
    assert_eq!(exit, std::process::ExitCode::SUCCESS);
}
