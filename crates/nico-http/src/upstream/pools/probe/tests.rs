//! The probe's verdict reading, its Host construction, and the slot interaction.

use std::time::{Duration, Instant};

use super::{ProbeResult, is_default_port, judge};
use crate::upstream::pools::active::{
    ActiveHealth, ActiveTransition, CheckFingerprint, ProbeOutcome,
};

fn check(expect: nico_core::ExpectedStatus) -> nico_core::HealthCheck {
    check_with(expect, 5_000, 1, 1)
}

/// Builds a document check through the real parse surface, so a schema drift fails here too.
fn check_with(
    expect: nico_core::ExpectedStatus,
    interval: u32,
    fall: u8,
    rise: u8,
) -> nico_core::HealthCheck {
    let expect_spelling = match expect {
        nico_core::ExpectedStatus::Non5xx => "non_5xx",
        nico_core::ExpectedStatus::S2xx => "2xx",
    };
    let upstream = format!(
        "  - name: api-backend\n    scheme: http\n    host: 127.0.0.1\n    port: 8080\n    health_check:\n      path: /health\n      expect: {expect_spelling}\n      interval_ms: {interval}\n      timeout_ms: 1000\n      unhealthy_threshold: {fall}\n      healthy_threshold: {rise}\n"
    );
    let document = nico_core::Document::parse(
        format!(
            "apiVersion: gateway/v1\nrequest:\n  timeout_ms: 30000\n  max_body_bytes: 1024\nupstreams:\n{upstream}routes: []\nproviders: []\nplugins: []\n"
        )
        .as_bytes(),
        nico_core::ParseLimits::new(4096),
    )
    .expect("the fixture parses");
    document
        .upstreams()
        .first()
        .expect("one upstream")
        .health_check()
        .expect("the check is present")
        .clone()
}

#[test]
fn non_5xx_reads_any_status_below_500_as_an_answer() {
    for status in [200, 204, 301, 404, 499] {
        assert_eq!(
            judge(status, &check(nico_core::ExpectedStatus::Non5xx)),
            ProbeResult::Ok,
            "{status} proves the link under non_5xx"
        );
    }
}

#[test]
fn non_5xx_reads_a_5xx_as_failed() {
    for status in [500, 502, 599] {
        assert_eq!(
            judge(status, &check(nico_core::ExpectedStatus::Non5xx)),
            ProbeResult::Status5xx
        );
    }
}

#[test]
fn s2xx_reads_only_the_2xx_range_as_an_answer() {
    for status in [200, 201, 204, 299] {
        assert_eq!(
            judge(status, &check(nico_core::ExpectedStatus::S2xx)),
            ProbeResult::Ok
        );
    }
}

#[test]
fn s2xx_reads_a_4xx_as_a_mismatch_not_a_5xx() {
    assert_eq!(
        judge(404, &check(nico_core::ExpectedStatus::S2xx)),
        ProbeResult::StatusMismatch
    );
    assert_eq!(
        judge(500, &check(nico_core::ExpectedStatus::S2xx)),
        ProbeResult::Status5xx
    );
}

#[test]
fn the_host_header_omits_a_default_port() {
    assert!(is_default_port(nico_core::Scheme::Http, 80));
    assert!(is_default_port(nico_core::Scheme::Https, 443));
    assert!(!is_default_port(nico_core::Scheme::Http, 8080));
    assert!(!is_default_port(nico_core::Scheme::Https, 8443));
}

#[test]
fn every_probe_result_carries_a_metric_name() {
    for result in [
        ProbeResult::Ok,
        ProbeResult::Connect,
        ProbeResult::Timeout,
        ProbeResult::Status5xx,
        ProbeResult::StatusMismatch,
    ] {
        assert!(!result.as_str().is_empty());
    }
}

#[test]
fn connect_timeout_and_mismatch_are_failures_in_the_counters() {
    for result in [
        ProbeResult::Connect,
        ProbeResult::Timeout,
        ProbeResult::Status5xx,
        ProbeResult::StatusMismatch,
    ] {
        assert_eq!(
            result.outcome(),
            ProbeOutcome::Failed,
            "{} is a failed probe",
            result.as_str()
        );
    }
    assert_eq!(ProbeResult::Ok.outcome(), ProbeOutcome::Passed);
}

#[test]
fn a_claim_sets_the_due_floor_until_the_interval_passes() {
    let active = ActiveHealth::new();
    let interval = Duration::from_secs(1);

    // The claim itself is the floor: begin_probe records when the probe went out.
    assert!(active.begin_probe(false));
    assert!(
        !active.due(interval, Instant::now()),
        "a member just claimed is not due in the same tick"
    );
    active.finish_probe(ProbeOutcome::Passed, 1, 1);
    assert!(
        !active.due(interval, Instant::now()),
        "the slot released, but the interval has not passed"
    );
}

#[test]
fn the_fall_threshold_flips_the_member_through_the_probe_loop_path() {
    // The state machine's full path as the executor drives it: claim, report, metric, transition.
    let active = ActiveHealth::new();
    active.counted_under(fingerprint_of(1, 1));
    for round in 0..2 {
        assert!(active.begin_probe(false), "round {round} claims the slot");
        // The executor's result flows through `outcome()`; the state machine only sees the verdict.
        let outcome = ProbeResult::Timeout.outcome();
        let transition = active.finish_probe(outcome, 2, 1);
        assert_eq!(
            transition,
            if round == 1 {
                ActiveTransition::Fell
            } else {
                ActiveTransition::None
            },
            "the second timeout is the fall"
        );
    }
    assert!(!active.admits(), "the member left rotation");
}

fn fingerprint_of(fall: u8, rise: u8) -> CheckFingerprint {
    CheckFingerprint::of_parts(fall, rise, 5_000, 5_000, 1_000)
}
