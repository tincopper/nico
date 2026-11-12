//! A traffic upstream with several members, end to end: selection, ejection, recovery, and the TLS
//! seam that keeps one certificate valid for a fleet.
//!
//! Every other HTTP scenario answers with one address per upstream, which is a shape that cannot
//! show what this file exists for. Here one upstream names two real listeners, and the questions are
//! the ones an operator asks when a machine dies:
//!
//! 1. **does the request go where the strategy says** — two requests, two machines, asserted on what
//!    the origins received rather than on what the client was told;
//! 2. **does a dead member leave rotation** — the first request that meets the dead socket is refused
//!    `502`, and every request after it reaches the survivor, asserted by counting the survivor's
//!    requests (no sleep is involved: the client's answer and the origin's count are both facts);
//! 3. **does it stay out until its cooldown** — with a cooldown longer than the test, a restarted
//!    origin receives nothing while the survivor receives everything;
//! 4. **does it come back** — a reload that tightens `fail_timeout_ms` (the ejection state is
//!    inherited with the member's identity, the budget is re-read from the document) admits exactly
//!    one probe, and a successful probe puts the machine back in rotation;
//! 5. **do the operator's facts exist** — the ejection and return lines, the two metric families, and
//!    the request record's `upstream.endpoint`.
//!
//! The last phase is the TLS seam: two `https` members, one certificate for the logical host, and
//! `ip_hash` to prove the caller lands on one machine. What this phase asserts is the `Host` header
//! the origin received: it names `localhost`, the logical host, never the address the socket was
//! opened to. The server name is asserted where it can be read directly — `nico-http`'s
//! `upstream/tests.rs` pins `HttpPeer::sni` to the destination's authority — because this certificate
//! also carries `IP:127.0.0.1`, so a completed handshake alone cannot tell the two names apart.
//!
//! One server per test binary: the trust roots and Pingora's globals are process-level, so both
//! phases run against the same process and the same published document.
#![expect(
    clippy::panic,
    reason = "a helper that cannot find its metric sample quotes the whole scrape instead of returning nothing"
)]

mod support;

use std::{
    net::{Ipv4Addr, SocketAddr},
    time::{Duration, Instant},
};

use support::{
    http::{
        RunningServer,
        client::{Request, Response, exchange},
        free_port, log,
        resources::{Resources, Route},
        source::TestSource,
        upstream::{MockUpstream, Reply},
        write_bootstrap_with_metrics_and_grace,
    },
    tls::TlsOrigin,
};

/// How long a fixture may take to reach a state it was told to reach.
const BUDGET: Duration = Duration::from_secs(20);

/// The cooldown the first publication states: longer than this test, so "ejected" is observable
/// without racing the clock.
const LONG_COOLDOWN_MS: u32 = 60_000;

/// The cooldown the reload states: one millisecond, so the very next request is the probe.
const SHORT_COOLDOWN_MS: u32 = 1;

/// The upstream that has two plaintext members.
const FLEET: &str = "fleet";

/// The upstream that has two `https` members behind one certificate.
const TLS_FLEET: &str = "tls-fleet";

/// The correlation id of the request that met the dead member, so its record can be found.
const EJECTED_TRACE: &str = "lb-eject";

/// Builds the document both publications share, differing only in the fleet's cooldown.
///
/// Everything else is held constant on purpose: the member identities are the same addresses and
/// names across the reload, which is exactly the condition under which the ejection state is
/// inherited rather than reset.
fn document(
    fleet: &[(&'static str, SocketAddr)],
    tls_fleet: &[(&'static str, SocketAddr)],
    fail_timeout_ms: u32,
) -> String {
    Resources::new()
        .upstream_with_members(FLEET, "127.0.0.1", fleet)
        .ejection(1, fail_timeout_ms)
        .tls_upstream_with_members(TLS_FLEET, "localhost", tls_fleet)
        .strategy("ip_hash")
        .route(Route::new("lb", "/lb/", FLEET))
        .route(Route::new("tls-lb", "/tls-lb/", TLS_FLEET))
        .render()
}

/// Sends one request through the fleet route, returning the answer.
async fn fleet_request(address: SocketAddr, trace_id: &str) -> Response {
    exchange(
        address,
        &Request::get("/lb/items", "lb.test").header("x-request-id", trace_id),
    )
    .await
}

/// Returns the one sample of counter `name` whose labels contain every entry of `labels`.
///
/// A sample rather than a whole scrape: the question is always "is this documented label pair
/// present, and with what value", and the panic quotes the scrape so a missing family is diagnosable
/// from the failure alone.
fn counter_line<'a>(scrape: &'a str, name: &str, labels: &[&str]) -> &'a str {
    scrape
        .lines()
        .filter(|line| !line.starts_with('#'))
        .find(|line| line.starts_with(name) && labels.iter().all(|label| line.contains(label)))
        .unwrap_or_else(|| panic!("the scrape has no {name} carrying {labels:?}:\n{scrape}"))
}

/// Reads one endpoint from the self-reporting address.
async fn fetch(address: SocketAddr, path: &str) -> String {
    let response = exchange(address, &Request::get(path, "metrics.test")).await;
    String::from_utf8_lossy(response.body()).into_owned()
}

#[tokio::test(flavor = "current_thread")]
async fn a_dead_member_leaves_rotation_and_returns_after_its_cooldown() {
    // Installed before the server starts: the ejection lines are written on Pingora's worker thread.
    let captured = log::capture();

    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    let metrics_port = free_port();
    // A millisecond of grace: this file asserts the fleet, not the drain.
    write_bootstrap_with_metrics_and_grace(directory.path(), port, metrics_port, 1, 1000);

    // Both `localhost.pem` fixtures carry `subjectAltName = DNS:localhost, IP:127.0.0.1`, so either
    // origin answers a verified handshake for the name the document writes — and, because the address
    // is in the certificate too, the handshake alone does not prove which name was presented.
    let first = TlsOrigin::start("localhost.pem", "localhost.key", b"one").await;
    let second = TlsOrigin::start("localhost.pem", "localhost.key", b"two").await;
    let tls_members = [
        ("one", SocketAddr::from((Ipv4Addr::LOCALHOST, first.port()))),
        (
            "two",
            SocketAddr::from((Ipv4Addr::LOCALHOST, second.port())),
        ),
    ];

    let mut survivor = MockUpstream::start(Reply::Body {
        body: b"survivor".to_vec(),
        chunk: usize::MAX,
    })
    .await;
    let survivor_address = survivor.address();
    // The member that will die and come back: the document names it by the address it must keep.
    let mut dying = MockUpstream::start(Reply::Body {
        body: b"dying".to_vec(),
        chunk: usize::MAX,
    })
    .await;
    let dying_address = dying.address();

    let fleet = [("dying", dying_address), ("survivor", survivor_address)];
    let source = TestSource::with_document(&document(&fleet, &tls_members, LONG_COOLDOWN_MS));
    let server = RunningServer::start(directory.path(), source.clone()).await;
    let address = server.address();
    let metrics_address = server
        .metrics_address()
        .expect("the fixture declared a metrics section");

    // --- 1. the strategy spreads the requests ------------------------------------------------
    let response = fleet_request(address, "lb-first").await;
    assert_eq!(
        response.text(),
        "dying",
        "the first request takes member one"
    );
    let response = fleet_request(address, "lb-second").await;
    assert_eq!(
        response.text(),
        "survivor",
        "and the second takes member two"
    );
    assert_eq!(dying.observation().requests.len(), 1);
    assert_eq!(survivor.observation().requests.len(), 1);

    // --- 2. a dead member is refused once and then skipped -----------------------------------
    dying.stop().await;
    let response = fleet_request(address, EJECTED_TRACE).await;
    assert_eq!(
        response.status(),
        502,
        "the request that meets the dead socket is the one that reports it, got {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(
        response.json_code().as_deref(),
        Some("upstream_unreachable"),
        "the refusal is the existing unreachable pair, not a new code"
    );

    // Every request from here on reaches the survivor, and the dead member's budget is spent.
    for trace_id in ["lb-after-1", "lb-after-2", "lb-after-3"] {
        let response = fleet_request(address, trace_id).await;
        assert_eq!(
            response.status(),
            200,
            "{trace_id} must be served by the survivor, got {}: {}",
            response.status(),
            response.text()
        );
        assert_eq!(response.text(), "survivor");
    }
    assert_eq!(
        survivor.observation().requests.len(),
        4,
        "the survivor answered the first request and the three after the ejection"
    );
    assert_eq!(
        dying.observation().requests.len(),
        1,
        "the dead member received nothing after it was ejected"
    );

    // The two operator-visible facts: the ejection line, and the request record's member field.
    let line = log::wait_for_line_matching(&captured, BUDGET, |line| {
        line.contains("upstream member ejected")
    })
    .await;
    assert!(
        line.contains("upstream=fleet") && line.contains("upstream.endpoint=dying"),
        "the ejection names the upstream and the member: {line}"
    );
    assert!(
        line.contains("reason=\"connect\""),
        "and the failure that decided it: {line}"
    );
    let record = log::wait_for_request_log(&captured, EJECTED_TRACE, BUDGET).await;
    assert!(
        record.contains("upstream.endpoint=dying"),
        "a multi-member upstream names the member in the request record: {record}"
    );

    // --- 3. the member stays out until its cooldown ------------------------------------------
    // The origin comes back immediately; the cooldown is fifty-nine seconds away, so what keeps it
    // out is the ejection state and not the socket.
    let revived = MockUpstream::start_at(
        dying_address,
        Reply::Body {
            body: b"dying".to_vec(),
            chunk: usize::MAX,
        },
    )
    .await;
    for trace_id in ["lb-cooling-1", "lb-cooling-2", "lb-cooling-3"] {
        let response = fleet_request(address, trace_id).await;
        assert_eq!(response.text(), "survivor", "{trace_id} is still cooling");
    }
    assert_eq!(
        revived.observation().requests.len(),
        0,
        "a member inside its cooldown is not a candidate, however healthy it looks"
    );

    // --- 4. the metrics the surface publishes ------------------------------------------------
    let scrape = fetch(metrics_address, "/metrics").await;
    let ejected = counter_line(
        &scrape,
        "gateway_upstream_ejected_total",
        &["upstream=\"fleet\"", "endpoint=\"dying\""],
    );
    assert!(
        ejected.ends_with(" 1"),
        "one member was ejected once: {ejected}"
    );
    let selected = counter_line(
        &scrape,
        "gateway_upstream_selected_total",
        &["upstream=\"fleet\"", "endpoint=\"survivor\""],
    );
    // One selection for the first request, three after the ejection and three while the other member
    // cooled: seven. The count is asserted because a counter that never moves would satisfy the label
    // check while telling an operator nothing.
    assert!(
        selected.ends_with(" 7"),
        "the survivor was selected by every request that reached it: {selected}"
    );

    // --- 5. a tighter budget admits one probe, and the member returns -------------------------
    // The reload changes the budget and nothing else: the member identities are the same, so the
    // ejection state is inherited and only the cooldown that judges it moves.
    source.publish(&document(&fleet, &tls_members, SHORT_COOLDOWN_MS));
    let deadline = Instant::now() + BUDGET;
    loop {
        let response = fleet_request(address, "lb-revive").await;
        assert_eq!(
            response.status(),
            200,
            "every request is served while the reload lands, got {}: {}",
            response.status(),
            response.text()
        );
        if !revived.observation().requests.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the revived member never became the probe within {BUDGET:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let answered = revived
        .observation()
        .requests
        .last()
        .cloned()
        .expect("the member answered a probe");
    assert_eq!(
        answered.header("host"),
        Some(format!("127.0.0.1:{}", dying_address.port()).as_str()),
        "the member is addressed by the upstream's host, got {:?}",
        answered.head()
    );

    let line = log::wait_for_line_matching(&captured, BUDGET, |line| {
        line.contains("upstream member returned")
    })
    .await;
    assert!(
        line.contains("upstream.endpoint=dying"),
        "the return names the member that rejoined: {line}"
    );

    // --- 6. the TLS seam: one certificate, two machines --------------------------------------
    let mut served = 0;
    for _ in 0..4 {
        let response = exchange(address, &Request::get("/tls-lb/items", "lb.test")).await;
        assert_eq!(
            response.status(),
            200,
            "an https member completes a verified handshake, got {}: {}",
            response.status(),
            response.text()
        );
        served += 1;
    }
    let one = first.observation().requests.len();
    let two = second.observation().requests.len();
    assert_eq!(
        one + two,
        served,
        "every request reached a member: {one} + {two} != {served}"
    );
    assert!(
        one == 0 || two == 0,
        "`ip_hash` keeps one caller on one machine, got {one} and {two}"
    );
    let (reachable_origin, observed) = if one > 0 {
        (&first, first.observation().requests.last().cloned())
    } else {
        (&second, second.observation().requests.last().cloned())
    };
    let observed = observed.expect("the reached origin answered");
    assert_eq!(
        observed.header("host"),
        Some(format!("localhost:{}", reachable_origin.port())),
        "`Host` names the logical host, not the address the socket was opened to: {:?}",
        observed.request_line()
    );

    let exit_code = server.cancel_and_join().await;
    assert_eq!(
        exit_code,
        std::process::ExitCode::SUCCESS,
        "and the reload that re-tuned the budget did not break the process"
    );
    survivor.stop().await;
}
