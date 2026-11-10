//! Tests for the surface's routing and its bodies.
//!
//! No socket and no runtime: the capability layer is a pure function of `(method, path, readiness)`,
//! which is the whole point of the crate boundary. The recorder is installed once per test
//! process; every assertion that needs it goes through [`install`].

use nico_core::{ConfigStatus, ErrorCategory};

use super::*;
use crate::{LIVE_PATH, METRICS_PATH, READY_PATH, Readiness, install};

/// A status with every field set, so an assertion can name the value it expects.
fn status() -> ConfigStatus {
    let mut status = ConfigStatus::default();
    status.generation = 7;
    status.revision = Some(12);
    status.digest = Some("sha256:abcd".to_owned());
    status.ready = true;
    status.last_reload_unix_secs = Some(1_791_234_567);
    status
}

fn metrics() -> Metrics {
    install().expect("a fresh test process installs the recorder once")
}

#[test]
fn install_is_idempotent() {
    let first = metrics();
    let second = metrics();

    assert!(
        first.render(&status()).contains("config_degraded"),
        "the first handle renders"
    );
    assert!(
        second.render(&status()).contains("config_degraded"),
        "the second call returns the same recorder"
    );
}

#[test]
fn live_is_always_ok_and_reads_no_state() {
    // A process that cannot serve is still alive: the verdict is deliberately not this endpoint's.
    let reply = metrics().respond("GET", LIVE_PATH, Readiness::new(false, &status()));

    assert_eq!(reply.status(), 200);
    assert_eq!(reply.content_type(), "text/plain");
    assert_eq!(reply.body(), "ok");
    assert_eq!(reply.allow(), None);
}

#[test]
fn ready_follows_the_servability_verdict() {
    let ready = metrics().respond("GET", READY_PATH, Readiness::new(true, &status()));
    assert_eq!(ready.status(), 200);
    assert_eq!(ready.content_type(), "application/json");
    let body = ready.body();
    assert!(body.contains("\"ready\":true"), "body was {body}");
    assert!(body.contains("\"generation\":7"), "body was {body}");
    assert!(body.contains("\"revision\":12"), "body was {body}");
    assert!(
        body.contains("\"digest\":\"sha256:abcd\""),
        "body was {body}"
    );
    assert!(
        body.contains("\"last_error_category\":null"),
        "body was {body}"
    );

    let not_ready = metrics().respond("GET", READY_PATH, Readiness::new(false, &status()));
    assert_eq!(not_ready.status(), 503);
    assert!(
        not_ready.body().contains("\"ready\":false"),
        "body was {}",
        not_ready.body()
    );
}

#[test]
fn degraded_and_stale_do_not_fail_readiness() {
    let mut degraded = status();
    degraded.degraded = true;
    degraded.stale = true;
    degraded.last_error_category = Some(ErrorCategory::Upstream);

    // Serving a last-known-good snapshot is a designed behaviour: the fields report it, the status
    // does not. A probe that turned red here would pull traffic from a data plane that answers 200.
    let reply = metrics().respond("GET", READY_PATH, Readiness::new(true, &degraded));

    assert_eq!(reply.status(), 200);
    let body = reply.body();
    assert!(body.contains("\"degraded\":true"), "body was {body}");
    assert!(body.contains("\"stale\":true"), "body was {body}");
    assert!(
        body.contains("\"last_error_category\":\"upstream\""),
        "body was {body}"
    );
}

#[test]
fn a_non_get_request_is_refused_with_allow() {
    for path in [LIVE_PATH, READY_PATH, METRICS_PATH] {
        let reply = metrics().respond("POST", path, Readiness::new(true, &status()));
        assert_eq!(reply.status(), 405, "{path} must refuse a non-GET");
        assert_eq!(reply.allow(), Some("GET"), "{path} declares its method set");
    }
}

#[test]
fn an_unknown_path_is_not_found() {
    let reply = metrics().respond("GET", "/nope", Readiness::new(true, &status()));

    assert_eq!(reply.status(), 404);
    assert_eq!(reply.allow(), None);
}

#[test]
fn metrics_render_carries_the_scrape_time_gauges() {
    let reply = metrics().respond("GET", METRICS_PATH, Readiness::new(true, &status()));

    assert_eq!(reply.status(), 200);
    assert_eq!(reply.content_type(), "text/plain; version=0.0.4");
    assert!(
        reply.body().contains("config_degraded"),
        "the degraded gauge is written at scrape time: {}",
        reply.body()
    );
}

#[test]
fn a_histogram_sample_reaches_the_render() {
    // The recorder must be installed before anything is recorded; the facade drops a sample that
    // arrives before it, which is why assembly installs first and the request path never checks.
    let metrics = metrics();
    metrics::histogram!("nico_metrics_upkeep_probe").record(1.5);

    let reply = metrics.respond("GET", METRICS_PATH, Readiness::new(true, &status()));

    let body = reply.body();
    assert!(
        body.contains("nico_metrics_upkeep_probe_count 1"),
        "the histogram sample must survive upkeep and appear in the render: {body}"
    );
}
