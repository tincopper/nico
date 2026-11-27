//! Active health checks, end to end: a gateway serving a document whose upstream declares a
//! `health_check`, against real mock origins, with **no client traffic at all**.
//!
//! The state machines and the probe executor are unit-verified; what only a running process can
//! prove is the chain: the document's check reaches the scheduler, the scheduler's probes reach the
//! real origin (the mock observes them), the verdicts move the member out of and back into
//! rotation, and the surface reports it all as probe metrics — without ever touching the passive
//! ejection counter, whose events come from real requests only.

mod support;

use std::{sync::Arc, time::Duration};

use support::http::{
    RunningServer,
    client::{Request, exchange, get},
    free_port,
    resources::{Resources, Route},
    source::TestSource,
    upstream::MockUpstream,
};

/// How long a fixture may take to reach a state it was told to reach.
const BUDGET: Duration = Duration::from_secs(20);

/// The poll cadence while waiting for a metric to move; fast enough that a fast check converges
/// within its budget, slow enough not to busy-poll the surface.
const POLL: Duration = Duration::from_millis(100);

/// The upstream whose members the scenarios probe.
const FLEET: &str = "fleet";

/// Waits until `probe_line` reads a value of at least `minimum`, returning the scrape's last read.
async fn wait_for_probe(
    metrics_address: std::net::SocketAddr,
    labels: &[&str],
    minimum: u64,
    context: &str,
) -> String {
    let deadline = tokio::time::Instant::now() + BUDGET;
    loop {
        let scrape = fetch(metrics_address, "/metrics").await;
        if let Ok(value) = probe_value(&scrape, labels)
            && value >= minimum
        {
            return scrape;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{context}: the probe counter {labels:?} never reached {minimum} within {BUDGET:?}:\n{scrape}"
        );
        tokio::time::sleep(POLL).await;
    }
}

/// Reads one endpoint from the self-reporting address.
async fn fetch(address: std::net::SocketAddr, path: &str) -> String {
    let response = exchange(address, &Request::get(path, "metrics.test")).await;
    String::from_utf8_lossy(response.body()).into_owned()
}

/// Reads one probe counter's value from a scrape, or errors when the family is absent.
fn probe_value(scrape: &str, labels: &[&str]) -> Result<u64, String> {
    let line = scrape
        .lines()
        .filter(|line| !line.starts_with('#'))
        .find(|line| {
            line.starts_with("gateway_upstream_probe_total")
                && labels.iter().all(|label| line.contains(label))
        })
        .ok_or_else(|| format!("no probe line carrying {labels:?} in:\n{scrape}"))?;
    line.rsplit(' ')
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| format!("the probe line has no numeric tail: {line}"))
}

/// Builds the pooled-route document both scenarios share, differing only in the check's `expect`.
fn document(a: std::net::SocketAddr, b: std::net::SocketAddr, expect: &str) -> String {
    Resources::new()
        .upstream_with_members(FLEET, "checked.example.test", &[("a", a), ("b", b)])
        .health_check("/health", expect, 200, 500, 2, 2)
        .route(Route::new("api", "/api/", FLEET))
        .render()
}

#[tokio::test(flavor = "current_thread")]
async fn an_unhealthy_member_is_probed_out_and_back_without_any_client_traffic() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    let metrics_port = free_port();

    // Member `a` fails every probe; member `b` passes. No client request is ever sent until the
    // member has come back — zero-traffic discovery is the whole point being proven.
    let mut failing = MockUpstream::start(support::http::upstream::Reply::Response {
        status: 500,
        headers: Vec::new(),
        body: b"down".to_vec(),
        chunk: usize::MAX,
    })
    .await;
    let healthy = MockUpstream::start(support::http::upstream::Reply::Body {
        body: b"healthy".to_vec(),
        chunk: usize::MAX,
    })
    .await;

    let document = document(failing.address(), healthy.address(), "non_5xx");
    let source = TestSource::with_document(&document);
    let _bootstrap =
        support::http::write_bootstrap_with_metrics(directory.path(), port, metrics_port);
    let server = RunningServer::start(
        directory.path(),
        Arc::clone(&source) as Arc<dyn nico_config::ConfigSource>,
    )
    .await;
    let metrics_address = server.metrics_address().expect("the metrics surface is on");

    // --- 1. the probes really happen, and the fall takes the member out -----------------------
    // fall = 2: the first failed probe is a count, the second is the verdict. The third proves the
    // scheduler keeps probing (and the member keeps answering 500) after the verdict.
    let scrape = wait_for_probe(
        metrics_address,
        &["upstream=\"fleet\"", "endpoint=\"a\"", "result=\"5xx\""],
        3,
        "the failing member should have been probed at least three times",
    )
    .await;
    assert!(
        failing.observation().requests.iter().any(|request| {
            let head = String::from_utf8_lossy(request.head());
            head.starts_with("GET /health")
                && request
                    .header("host")
                    .is_some_and(|host| host.starts_with("checked.example.test"))
        }),
        "the probes hit the mock's /health with the upstream's logical Host: {:?}",
        failing.observation().summary()
    );

    // The passive ejection counter stays silent: its events come from real requests, and this
    // scenario sends none.
    assert!(
        !scrape
            .lines()
            .any(|line| line.starts_with("gateway_upstream_ejected_total")),
        "a probe-driven removal is not a passive ejection:\n{scrape}"
    );

    // --- 2. the member returns after two passed probes ----------------------------------------
    // The mock stops answering 500 and starts answering 200; rise = 2, and the third ok proves the
    // member is back in rotation (and still being probed).
    failing.stop().await;
    let revived = MockUpstream::start_at(
        failing.address(),
        support::http::upstream::Reply::Body {
            body: b"revived".to_vec(),
            chunk: usize::MAX,
        },
    )
    .await;
    let scrape = wait_for_probe(
        metrics_address,
        &["upstream=\"fleet\"", "endpoint=\"a\"", "result=\"ok\""],
        3,
        "the recovered member should have been probed to its rise threshold and beyond",
    )
    .await;
    let _ = scrape;

    // --- 3. the member really is back: client traffic reaches both origins again ---------------
    // With `a` still out, every request would answer `healthy`; the revival is proven by the
    // original member serving its share within a bounded sample.
    let address = server.address();
    let deadline = tokio::time::Instant::now() + BUDGET;
    let mut saw_revived = false;
    let mut saw_healthy = false;
    while tokio::time::Instant::now() < deadline {
        let response = get(address, "/api/items", "a.test").await;
        match response.text().as_str() {
            "revived" => saw_revived = true,
            "healthy" => saw_healthy = true,
            _ => {}
        }
        if saw_revived && saw_healthy {
            break;
        }
        tokio::time::sleep(POLL).await;
    }
    assert!(
        saw_revived && saw_healthy,
        "after the rise, client traffic must reach both members (revived: {saw_revived}, healthy: {saw_healthy})"
    );
    assert!(!revived.observation().requests.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn the_expect_reading_decides_whether_a_4xx_answer_passes() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    let metrics_port = free_port();

    // The strict member answers 404 to every probe: a failure under `expect: 2xx`, a pass under
    // `non_5xx`. Two upstreams with the same members differ only in the check's reading.
    let member = MockUpstream::start(support::http::upstream::Reply::Response {
        status: 404,
        headers: Vec::new(),
        body: b"not-found".to_vec(),
        chunk: usize::MAX,
    })
    .await;
    let filler = MockUpstream::start(support::http::upstream::Reply::Body {
        body: b"filler".to_vec(),
        chunk: usize::MAX,
    })
    .await;

    let strict = document(member.address(), filler.address(), "2xx");
    let lenient = document(member.address(), filler.address(), "non_5xx");
    let source = TestSource::with_document(&strict);
    let _bootstrap =
        support::http::write_bootstrap_with_metrics(directory.path(), port, metrics_port);
    let server = RunningServer::start(
        directory.path(),
        Arc::clone(&source) as Arc<dyn nico_config::ConfigSource>,
    )
    .await;
    let metrics_address = server.metrics_address().expect("the metrics surface is on");

    // Under `expect: 2xx`, the 404 is a mismatch: three probes all fail.
    wait_for_probe(
        metrics_address,
        &[
            "upstream=\"fleet\"",
            "endpoint=\"a\"",
            "result=\"status_mismatch\"",
        ],
        3,
        "a 404 under expect: 2xx must be a mismatch",
    )
    .await;

    // The reload changes only the reading: under `non_5xx`, the same 404 proves the member alive,
    // and the ok counter starts moving.
    source.publish(&lenient);
    wait_for_probe(
        metrics_address,
        &["upstream=\"fleet\"", "endpoint=\"a\"", "result=\"ok\""],
        2,
        "the same 404 under expect: non_5xx must pass",
    )
    .await;
}
