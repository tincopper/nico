//! Unit tests for the chain's identity values: what makes two targets the same, and what a log
//! line may name.
//!
//! The chain's construction (member pairing, capability intersection) is covered with real exchanges
//! in the integration tests under `tests/route/chain.rs`, where the registry-backed fixtures live.

use super::*;
use crate::provider::ProviderKind;

#[test]
fn identities_with_equal_facts_are_equal() {
    let kind = ProviderKind::new("chain-test-kind");
    let params = BreakerParams::new(5, 30_000);
    let left = TargetIdentity::new("primary", "https://api.primary.test", kind, params);
    let right = TargetIdentity::new("primary", "https://api.primary.test", kind, params);
    assert_eq!(left, right);
}

#[test]
fn identities_differ_when_the_breaker_parameter_changes() {
    let kind = ProviderKind::new("chain-test-kind");
    let left = TargetIdentity::new(
        "primary",
        "https://api.primary.test",
        kind,
        BreakerParams::new(5, 30_000),
    );
    let right = TargetIdentity::new(
        "primary",
        "https://api.primary.test",
        kind,
        BreakerParams::new(10, 30_000),
    );
    assert_ne!(left, right);
}

#[test]
fn identities_differ_when_the_endpoint_moves() {
    let kind = ProviderKind::new("chain-test-kind");
    let params = BreakerParams::new(5, 30_000);
    let left = TargetIdentity::new("primary", "https://api.primary.test", kind, params);
    let right = TargetIdentity::new("primary", "https://api.secondary.test", kind, params);
    assert_ne!(left, right);
}

#[test]
fn a_zero_threshold_reports_a_disabled_breaker() {
    let params = BreakerParams::new(0, 30_000);
    assert!(params.is_disabled());
    assert_eq!(params.threshold(), 0);
}

#[test]
fn the_identity_debug_never_names_the_base_url() {
    let identity = TargetIdentity::new(
        "primary",
        "https://api.primary.test",
        ProviderKind::new("chain-test-kind"),
        BreakerParams::new(5, 30_000),
    );
    let printed = format!("{identity:?}");
    assert!(
        !printed.contains("api.primary.test"),
        "the base URL must not reach a log line: {printed}"
    );
    assert!(
        printed.contains("primary"),
        "the provider name is the log's way to say which target: {printed}"
    );
}
