//! Tests for the runtime settings: defaults, boundaries, and the rules that reject a value.

use super::{ServerSettings, ServerSettingsError};

/// Resolves no field at all, which must produce the documented defaults.
fn defaults() -> ServerSettings {
    ServerSettings::resolve(None, None, None).expect("omitted fields take their defaults")
}

#[test]
fn the_defaults_are_the_documented_values() {
    let settings = ServerSettings::new();

    assert_eq!(settings.threads(), 1);
    assert_eq!(settings.grace_period_ms(), 5_000);
    assert_eq!(settings.graceful_shutdown_ms(), 30_000);
}

#[test]
fn every_omitted_field_takes_its_default() {
    assert_eq!(defaults(), ServerSettings::new());
}

#[test]
fn a_written_field_overrides_its_default() {
    let settings =
        ServerSettings::resolve(Some(8), Some(1_000), Some(60_000)).expect("all fields in range");

    assert_eq!(settings.threads(), 8);
    assert_eq!(settings.grace_period_ms(), 1_000);
    assert_eq!(settings.graceful_shutdown_ms(), 60_000);
}

#[test]
fn zero_threads_are_rejected_because_pingora_panics_on_them() {
    let error =
        ServerSettings::resolve(Some(0), None, None).expect_err("zero is not a runtime size");

    assert_eq!(error.field(), "threads");
    assert_eq!(error.reason(), "must be in 1..=256 (found 0)");
}

#[test]
fn threads_above_the_maximum_are_rejected() {
    let error =
        ServerSettings::resolve(Some(257), None, None).expect_err("the thread count is bounded");

    assert_eq!(error.field(), "threads");
    assert_eq!(error.reason(), "must be in 1..=256 (found 257)");
}

#[test]
fn the_thread_boundaries_are_accepted() {
    for threads in [1, 256] {
        let settings = ServerSettings::resolve(Some(threads), None, None)
            .expect("the boundary is the inclusive bound");
        assert_eq!(settings.threads(), threads);
    }
}

#[test]
fn a_grace_period_of_zero_is_rejected() {
    let error = ServerSettings::resolve(None, Some(0), None)
        .expect_err("a zero window would cut every request off");

    assert_eq!(error.field(), "grace_period_ms");
    assert_eq!(error.reason(), "must be in 1..=300000 (found 0)");
}

#[test]
fn a_grace_period_above_the_maximum_is_rejected() {
    let error =
        ServerSettings::resolve(None, Some(301_000), None).expect_err("the window is bounded");

    assert_eq!(error.field(), "grace_period_ms");
    assert!(
        error.reason().contains("300000"),
        "the reason quotes the bound, got `{}`",
        error.reason()
    );
}

#[test]
fn a_grace_period_at_the_maximum_is_accepted() {
    // The shutdown window is omitted, so the default (30 000) applies: raising the grace period to
    // the maximum drags the shutdown bound along with it.
    let settings = ServerSettings::resolve(None, Some(300_000), Some(300_000))
        .expect("the boundary is the inclusive bound");

    assert_eq!(settings.grace_period_ms(), 300_000);
}

#[test]
fn a_shutdown_window_of_zero_is_rejected() {
    let error = ServerSettings::resolve(None, None, Some(0))
        .expect_err("a zero shutdown bound would truncate the drain");

    assert_eq!(error.field(), "graceful_shutdown_ms");
}

#[test]
fn a_shutdown_window_at_the_maximum_is_accepted() {
    let settings = ServerSettings::resolve(None, None, Some(300_000))
        .expect("the boundary is the inclusive bound");

    assert_eq!(settings.graceful_shutdown_ms(), 300_000);
}

#[test]
fn a_shutdown_window_shorter_than_the_grace_period_is_rejected() {
    let error = ServerSettings::resolve(None, Some(10_000), Some(5_000))
        .expect_err("draining may not be bounded tighter than the grace period");

    assert_eq!(error.field(), "graceful_shutdown_ms");
    assert_eq!(
        error.reason(),
        "must be at least `grace_period_ms` (10000 ms)"
    );
}

#[test]
fn a_shutdown_window_equal_to_the_grace_period_is_accepted() {
    let settings = ServerSettings::resolve(None, Some(10_000), Some(10_000))
        .expect("equal windows are consistent");

    assert_eq!(settings.graceful_shutdown_ms(), settings.grace_period_ms());
}

#[test]
fn a_raised_grace_period_needs_a_raised_shutdown_window() {
    let error = ServerSettings::resolve(None, Some(60_000), None)
        .expect_err("the default shutdown window is now too short");

    assert_eq!(error.field(), "graceful_shutdown_ms");
}

#[test]
fn the_error_names_the_field_and_the_bound_it_quoted() {
    let error = ServerSettings::resolve(Some(0), None, None).expect_err("zero threads");

    assert_eq!(
        error.to_string(),
        "runtime setting `threads` is invalid: must be in 1..=256 (found 0)"
    );
}

#[test]
fn the_error_type_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}

    assert_send_sync::<ServerSettingsError>();
}
