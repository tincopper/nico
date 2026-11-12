//! Semantic validation of one `upstreams` entry — its logical identity, its members and the failure
//! budget they are judged by.
//!
//! The scheme and the strategy need no check anywhere: both are closed enums whose serde
//! implementation rejects an unknown spelling while the document is decoded. What is left is what a
//! document can get wrong — a name, an address, a port, a weight, a count, a budget — and every rule
//! names the field it came from.
//!
//! A member's `address` is a connection target, so it follows the same host rule `upstreams[].host`
//! does: one rule, two field names. Keeping them together here is what stops "a host is a name" from
//! growing a second, weaker copy for members.

use std::collections::HashSet;

use super::{MAX_COOLDOWN_MS, invalid, validate_host, validate_name};
use crate::config::{DocumentError, Upstream};

/// Checks one upstream: name, host, port, the failure budget and every member.
pub(super) fn validate(upstream: &Upstream) -> Result<(), DocumentError> {
    validate_name("upstreams[].name", upstream.name())?;
    validate_host("upstreams[].host", upstream.host())?;
    if upstream.port() == 0 {
        return Err(invalid("upstreams[].port", "must be in 1..=65535"));
    }
    validate_ejection(upstream)?;
    validate_endpoints(upstream)
}

/// Checks one upstream's ejection parameters.
///
/// `max_fails` of `0` is legal and means "never eject": the same spelling a provider's breaker uses
/// for "never stop calling this endpoint". `fail_timeout_ms` cannot be zero — a cooldown of zero
/// admits every request as a probe, which is indistinguishable from not ejecting at all while still
/// pretending to.
fn validate_ejection(upstream: &Upstream) -> Result<(), DocumentError> {
    let ejection = upstream.ejection();
    if ejection.max_fails() > MAX_MAX_FAILS {
        return Err(DocumentError::LimitOutOfRange {
            field: "upstreams[].max_fails",
            value: u64::from(ejection.max_fails()),
        });
    }
    if !(1..=MAX_COOLDOWN_MS).contains(&ejection.fail_timeout_ms()) {
        return Err(DocumentError::LimitOutOfRange {
            field: "upstreams[].fail_timeout_ms",
            value: u64::from(ejection.fail_timeout_ms()),
        });
    }
    Ok(())
}

/// Checks the members one upstream may send a request to.
///
/// A member's `address` is a connection target, so the host rule is the same one `upstreams[].host`
/// follows: one rule, two field names. Names are checked for uniqueness because they identify the
/// member in logs and metrics — a duplicate would make two machines indistinguishable in a dashboard
/// while still receiving traffic. Targets are checked too, because two members that resolve to one
/// `address:port` are one machine counted twice: its failures would be split across two counters and
/// the selector would keep sending traffic to it after either one had reached its budget.
fn validate_endpoints(upstream: &Upstream) -> Result<(), DocumentError> {
    if upstream.endpoints().is_empty() {
        return Err(invalid(
            "upstreams[].endpoints",
            "must list at least one member",
        ));
    }
    let mut names: HashSet<&str> = HashSet::with_capacity(upstream.endpoints().len());
    let mut targets: HashSet<(&str, u16)> = HashSet::with_capacity(upstream.endpoints().len());
    for endpoint in upstream.endpoints() {
        validate_name("upstreams[].endpoints[].name", endpoint.name())?;
        if !names.insert(endpoint.name()) {
            return Err(DocumentError::DuplicateId {
                kind: "upstream-endpoint",
                id: format!("{}.{}", upstream.name(), endpoint.name()),
            });
        }
        validate_host("upstreams[].endpoints[].address", endpoint.address())?;
        if !targets.insert((endpoint.address(), endpoint.port())) {
            return Err(invalid(
                "upstreams[].endpoints[].address",
                "must not repeat another member's address and port",
            ));
        }
        if endpoint.port() == 0 {
            return Err(invalid(
                "upstreams[].endpoints[].port",
                "must be in 1..=65535",
            ));
        }
        if !(1..=MAX_ENDPOINT_WEIGHT).contains(&endpoint.weight()) {
            return Err(DocumentError::LimitOutOfRange {
                field: "upstreams[].endpoints[].weight",
                value: u64::from(endpoint.weight()),
            });
        }
    }
    Ok(())
}

/// Inclusive upper bound for `upstreams[].endpoints[].weight`.
///
/// Zero is not in the range: a member with no share never receives traffic, which in a traffic-layer
/// member list would be a silent way to disable a machine instead of a weight. The range is wide
/// enough that a document can express any ratio it needs without arithmetic on the selector's side.
const MAX_ENDPOINT_WEIGHT: u16 = 1000;

/// Inclusive upper bound for `upstreams[].max_fails`.
///
/// `0` stays legal and means ejection is disabled, exactly like a provider's
/// `circuit_breaker.failure_threshold`; the ceiling is the same "this is not a number, it is a
/// posture" bound a breaker threshold has.
const MAX_MAX_FAILS: u32 = 1000;
