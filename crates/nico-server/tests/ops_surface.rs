//! The self-reporting surface, end to end: three endpoints, default-off, and the data plane's
//! inability to shadow them.
//!
//! The scenarios answer four questions the unit tests in `nico-metrics` cannot:
//!
//! - is the surface **off** when the bootstrap declares no `metrics` section (and does the data
//!   plane then answer `404` on `/metrics`, proving no second listener was started)?
//! - is it **on** at its own address when declared, while the data plane keeps answering `404` on
//!   the same paths (proving the surface is not a route a pushed document could shadow)?
//! - does readiness track the published snapshot, and does a degraded last-known-good generation
//!   still read as ready?
//! - do the config and request metric families appear once the process has done the work?
//!
//! One server per test binary: `Server::run` owns the calling thread. The first scenario therefore
//! starts the only process this file gets, and the rest assert against it in sequence. That is why
//! the surface is enabled in the single fixture: a default-off process and an enabled one cannot
//! both exist in one test target, and the default-off half is asserted by
//! `a_process_without_the_section_serves_no_surface` in the unit tests of `nico-server`'s bootstrap
//! plus the data plane's own `404` asserted below.

mod support;

use std::time::Duration;

use support::http::{
    RunningServer,
    client::{Request, exchange},
    free_port,
    resources::{Resources, Route},
    source::TestSource,
    upstream::{MockUpstream, Reply},
    write_bootstrap_with_metrics,
};

/// How long a scrape may take to report a fact the process produced.
const SCRAPE_BUDGET: Duration = Duration::from_secs(10);

/// A document that serves one plain route, so the request metrics have something to count.
fn document(upstream: std::net::SocketAddr) -> String {
    Resources::new()
        .upstream("backend", upstream)
        .route(Route::new("api", "/api/", "backend"))
        .render()
}

/// Reads one endpoint from the self-reporting address.
async fn fetch(address: std::net::SocketAddr, path: &str) -> (u16, String) {
    let response = exchange(address, &Request::get(path, "metrics.test")).await;
    let status = response.status();
    let body = String::from_utf8_lossy(response.body()).into_owned();
    (status, body)
}

#[tokio::test(flavor = "current_thread")]
async fn the_surface_reports_health_and_metrics_and_the_data_plane_cannot_shadow_it() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    let metrics_port = free_port();
    write_bootstrap_with_metrics(directory.path(), port, metrics_port);

    let upstream = MockUpstream::start(Reply::Response {
        status: 200,
        headers: Vec::new(),
        body: b"served".to_vec(),
        chunk: 7,
    })
    .await;
    let source = TestSource::with_document(&document(upstream.address()));
    // Kept so the last scenario can publish a document the pipeline refuses: the surface's job is
    // to report a degraded process as still servable, and that needs a refusal to observe. The
    // concrete handle stays by, because `publish` is the fixture's own method, not the source trait.
    let publisher = std::sync::Arc::clone(&source);
    let server = RunningServer::start(directory.path(), source).await;

    // The surface has its own address, read back from the parsed bootstrap rather than assumed.
    let metrics_address = server
        .metrics_address()
        .expect("the fixture declared a metrics section");
    assert_eq!(metrics_address.port(), metrics_port);

    // 1. Liveness answers without reading shared state, and it is a plain-text `ok`.
    let (status, body) = fetch(metrics_address, "/health/live").await;
    assert_eq!(status, 200, "liveness must answer once the process is up");
    assert_eq!(body, "ok", "liveness is the plain-text acknowledgement");

    // 2. Readiness answers from the published snapshot: this process published before the first
    //    request, so it is ready and reports the generation it serves.
    let (status, body) = fetch(metrics_address, "/health/ready").await;
    assert_eq!(status, 200, "a published snapshot is servable");
    assert!(
        body.contains("\"ready\":true"),
        "the readiness body must say it can serve, got {body}"
    );
    assert!(
        body.contains("\"generation\":1"),
        "the readiness body must report the generation in service, got {body}"
    );

    // 3. A method other than GET is refused with the one method the surface serves.
    let response = exchange(
        metrics_address,
        &Request::new("POST", "/metrics", "metrics.test"),
    )
    .await;
    assert_eq!(response.status(), 405, "the surface serves GET only");
    assert_eq!(
        response.header("allow"),
        Some("GET"),
        "a 405 must declare the legal method"
    );

    // 4. An unknown path is a 404 and reads nothing.
    let (status, _body) = fetch(metrics_address, "/nope").await;
    assert_eq!(status, 404);

    // 5. The data plane cannot be made to serve the surface: the same paths there are ordinary
    //    route misses, because the surface is a separate listener rather than a reserved prefix.
    for path in ["/health/live", "/health/ready", "/metrics"] {
        let response = exchange(server.address(), &Request::get(path, "gateway.test")).await;
        assert_eq!(
            response.status(),
            404,
            "{path} must be an ordinary route miss on the data plane"
        );
    }

    // 6. A real request through the data plane, so the request metrics have a sample.
    let served = exchange(server.address(), &Request::get("/api/x", "gateway.test")).await;
    assert_eq!(served.status(), 200, "the upstream answered");

    // 7. The scrape carries the config baseline and the request counter the gateway just produced.
    //
    //    The first publication records its counters only after its last-known-good cache flush
    //    returned, so a scrape can legitimately arrive before that attempt is counted. Waiting for
    //    the fact — the counter appearing — is what makes this deterministic; a sleep would encode
    //    this machine's disk latency as the contract.
    let (status, body) = wait_for_scrape(metrics_address, "config_reload_total").await;
    assert_eq!(status, 200);
    // The data-plane answer is a plain proxied response: it must not have grown the metrics
    // content type, which would mean the surface had leaked into the proxy path.
    assert_eq!(
        served.header("content-type"),
        None,
        "the data-plane answer carries no metrics content type"
    );
    for family in [
        "config_load_total",
        "config_reload_total",
        "config_compile_seconds",
        "config_snapshot_age_seconds",
        "config_degraded",
        "gateway_http_requests_total",
        "gateway_http_request_seconds",
    ] {
        assert!(
            body.contains(family),
            "the scrape is missing the {family} family; got:\n{body}"
        );
    }
    assert!(
        body.contains("route_id=\"api\""),
        "the request counter must be labelled with the route that served, got:\n{body}"
    );
    assert!(
        body.contains("status_class=\"2xx\""),
        "the request counter must record the status class, got:\n{body}"
    );
    // The histogram is only visible because the scrape drains it: `install_recorder` starts no
    // upkeep task, so an empty histogram here would mean the render skipped `run_upkeep`.
    assert!(
        body.contains("gateway_http_request_seconds_count"),
        "the histogram must have a sample after one request, got:\n{body}"
    );

    // 8. The snapshot age advances between two scrapes, which is what makes it a gauge rather than
    //    a value frozen at publication time.
    let (_, first) = fetch(metrics_address, "/metrics").await;
    let first_age = gauge_value(&first, "config_snapshot_age_seconds")
        .expect("the scrape reports the snapshot age");
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let (_, second) = fetch(metrics_address, "/metrics").await;
    let second_age = gauge_value(&second, "config_snapshot_age_seconds")
        .expect("the second scrape reports the snapshot age");
    assert!(
        second_age > first_age,
        "the snapshot age must advance between scrapes ({first_age} then {second_age})"
    );

    // 9. A refused publication is reported, and it does not make the process unservable. This is the
    //    scenario the whole surface exists for: the source is broken, the last-known-good generation
    //    keeps answering, and an operator reads both facts from here.
    publisher.publish("routes: this is not a resources document\n");
    let (_, refused) =
        wait_for_scrape(metrics_address, "config_reload_total{result=\"error\"}").await;
    assert!(
        gauge_value(&refused, "config_degraded").is_some_and(|value| value == 1.0),
        "a refused document must raise the degraded gauge, got:\n{refused}"
    );

    // Readiness still answers 200: pulling every replica out because the source is broken would turn
    // a partial failure into a total one, and the data plane is still serving the previous document.
    let (status, body) = fetch(metrics_address, "/health/ready").await;
    assert_eq!(
        status, 200,
        "a degraded process serving a last-known-good generation is still ready, got {body}"
    );
    assert!(
        body.contains("\"degraded\":true"),
        "the readiness body must report the degradation, got {body}"
    );
    assert!(
        body.contains("\"stale\":false"),
        "a last-known-good generation that was published in this process is not stale, got {body}"
    );

    let exit_code = server.cancel_and_join().await;
    assert_eq!(exit_code, std::process::ExitCode::SUCCESS);
}

/// Scrapes until `family` appears, or fails after [`SCRAPE_BUDGET`].
///
/// The wait is on the observable fact, not on a duration: the pipeline records its publication
/// counters once per attempt, after the attempt's cache flush returned, so "the family is absent"
/// is a real state that a scrape can observe and not a failure to retry blindly.
async fn wait_for_scrape(address: std::net::SocketAddr, family: &str) -> (u16, String) {
    let deadline = std::time::Instant::now() + SCRAPE_BUDGET;
    loop {
        let (status, body) = fetch(address, "/metrics").await;
        if body.contains(family) {
            return (status, body);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the scrape never reported {family} within {SCRAPE_BUDGET:?}; last body:\n{body}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Returns the value of one unlabelled gauge from a Prometheus text scrape.
///
/// `None` means the sample is absent, which the caller reports: the failure message has to quote the
/// whole scrape to be useful, and that belongs where the assertion lives.
fn gauge_value(scrape: &str, name: &str) -> Option<f64> {
    scrape.lines().find_map(|line| {
        let rest = line.strip_prefix(name)?.strip_prefix(' ')?;
        rest.parse::<f64>().ok()
    })
}
