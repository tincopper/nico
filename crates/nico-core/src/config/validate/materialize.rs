//! The materialization half of validation: written entries become model values, with the
//! section's defaults applied here and nowhere else.
//!
//! "What a document that wrote nothing gets" has one definition per section — the same rule the
//! provider breaker started — and keeping every default in one module is what stops a second
//! copy of any default from appearing beside the parser.

use std::sync::Arc;

use crate::config::{
    CircuitBreaker, Ejection, Endpoint, ExpectedStatus, HealthCheck, ProbeMethod, Upstream,
    provider::{
        DEFAULT_COOLDOWN_MS as DEFAULT_BREAKER_COOLDOWN_MS,
        DEFAULT_FAILURE_THRESHOLD as DEFAULT_BREAKER_THRESHOLD,
    },
    raw::{RawHealthCheck, RawUpstream},
    upstream::{
        DEFAULT_ENDPOINT_NAME, DEFAULT_ENDPOINT_WEIGHT, DEFAULT_FAIL_TIMEOUT_MS,
        DEFAULT_HEALTH_THRESHOLD, DEFAULT_MAX_FAILS,
    },
};

/// Turns one written upstream into its logical identity plus its members.
///
/// The single-address form is expanded here, not read later: a document that writes no `endpoints`
/// becomes exactly one member named [`DEFAULT_ENDPOINT_NAME`], with the upstream's host as its
/// address. Every consumer consequently sees one shape, and "there was no member list" stops being a
/// case the data plane could get wrong.
pub(super) fn materialize_upstream(raw: RawUpstream) -> Upstream {
    let RawUpstream {
        name,
        scheme,
        host,
        port,
        endpoints,
        strategy,
        max_fails,
        fail_timeout_ms,
        health_check,
    } = raw;
    let endpoints: Arc<[Endpoint]> = match endpoints {
        Some(members) => members
            .into_iter()
            .map(|member| {
                Endpoint::new(
                    member.name,
                    member.address.unwrap_or_else(|| host.clone()),
                    member.port.unwrap_or(port),
                    member.weight.unwrap_or(DEFAULT_ENDPOINT_WEIGHT),
                )
            })
            .collect(),
        None => Arc::from([Endpoint::new(
            DEFAULT_ENDPOINT_NAME,
            host.clone(),
            port,
            DEFAULT_ENDPOINT_WEIGHT,
        )]),
    };
    Upstream {
        name: name.into(),
        scheme,
        host: host.into(),
        port,
        strategy: strategy.unwrap_or_default(),
        endpoints,
        // The ejection budget is materialized here rather than left to the data plane, so "what a
        // document that wrote nothing gets" has exactly one definition — the same rule the
        // provider breaker follows.
        ejection: Ejection::new(
            max_fails.unwrap_or(DEFAULT_MAX_FAILS),
            fail_timeout_ms.unwrap_or(DEFAULT_FAIL_TIMEOUT_MS),
        ),
        health_check: health_check.map(materialize_health_check),
    }
}

/// Turns one written health check into its validated model, applying the section's defaults.
///
/// The defaults are the parser's job, not the data plane's: "what a check that wrote no threshold
/// gets" has one definition, here. `no_traffic_interval_ms` defaults to `interval_ms` — an
/// upstream that wrote no separate idle cadence is probed at the same rate either way.
fn materialize_health_check(raw: RawHealthCheck) -> HealthCheck {
    let interval_ms = raw.interval_ms;
    let RawHealthCheck {
        method,
        path,
        expect,
        no_traffic_interval_ms,
        timeout_ms,
        unhealthy_threshold,
        healthy_threshold,
        ..
    } = raw;
    let method = match method.as_deref() {
        Some("HEAD") => ProbeMethod::HEAD,
        _ => ProbeMethod::GET,
    };
    let expect = match expect.as_deref() {
        Some("2xx") => ExpectedStatus::S2xx,
        _ => ExpectedStatus::Non5xx,
    };
    HealthCheck::new(
        method,
        path.into(),
        expect,
        interval_ms,
        no_traffic_interval_ms.unwrap_or(interval_ms),
        timeout_ms,
        unhealthy_threshold.unwrap_or(DEFAULT_HEALTH_THRESHOLD),
        healthy_threshold.unwrap_or(DEFAULT_HEALTH_THRESHOLD),
    )
}

/// Turns the written `circuit_breaker` section into the parameters a provider carries, filling the
/// fields the document left out from the built-in defaults.
pub(super) fn materialize_breaker(
    raw: Option<crate::config::raw::RawCircuitBreaker>,
) -> CircuitBreaker {
    match raw {
        Some(section) => CircuitBreaker::new(
            section
                .failure_threshold
                .unwrap_or(DEFAULT_BREAKER_THRESHOLD),
            section.cooldown_ms.unwrap_or(DEFAULT_BREAKER_COOLDOWN_MS),
        ),
        None => CircuitBreaker::new(DEFAULT_BREAKER_THRESHOLD, DEFAULT_BREAKER_COOLDOWN_MS),
    }
}
