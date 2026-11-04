use super::{HttpSettings, HttpSettingsError};

/// Resolves no field at all, which must produce the documented defaults.
fn defaults() -> HttpSettings {
    HttpSettings::resolve(None, None, None, None, None).expect("omitted fields take their defaults")
}

#[test]
fn every_omitted_field_takes_its_default() {
    let settings = defaults();

    assert_eq!(settings.listen(), "0.0.0.0:8080");
    assert!(!settings.h2c(), "plaintext HTTP/2 is never implied");
    assert_eq!(settings.grace_period(), std::time::Duration::from_secs(5));
    assert_eq!(
        settings.graceful_shutdown(),
        std::time::Duration::from_secs(30)
    );
    assert_eq!(
        settings.threads(),
        1,
        "one is Pingora's own default runtime size"
    );
}

#[test]
fn the_default_value_is_also_accepted_when_written_out() {
    assert_eq!(
        HttpSettings::resolve(
            Some("0.0.0.0:8080"),
            Some(true),
            Some(5_000),
            Some(30_000),
            Some(4)
        )
        .expect("written fields are accepted"),
        // Only `h2c` and `threads` differ from the defaults; the windows are equal.
        HttpSettings {
            h2c: true,
            threads: 4,
            ..defaults()
        }
    );
}

#[test]
fn a_written_field_overrides_its_default() {
    let settings = HttpSettings::resolve(
        Some("127.0.0.1:9000"),
        Some(true),
        Some(1_000),
        Some(60_000),
        Some(8),
    )
    .expect("every field is in range");

    assert_eq!(settings.listen(), "127.0.0.1:9000");
    assert!(settings.h2c());
    assert_eq!(settings.grace_period(), std::time::Duration::from_secs(1));
    assert_eq!(
        settings.graceful_shutdown(),
        std::time::Duration::from_secs(60)
    );
    assert_eq!(settings.threads(), 8);
}

#[test]
fn h2c_defaults_to_false_and_stays_false_when_written_as_false() {
    assert!(!defaults().h2c());
    assert!(
        !HttpSettings::resolve(None, Some(false), None, None, None)
            .expect("false is in range")
            .h2c()
    );
}

#[test]
fn an_unparsable_listen_address_is_rejected() {
    let error = HttpSettings::resolve(Some("not-an-address"), None, None, None, None)
        .expect_err("a bare name is not host:port");

    assert_eq!(error.field(), "listen");
    assert!(
        error.reason().contains("ip:port"),
        "the reason names the accepted shape, got `{}`",
        error.reason()
    );
}

#[test]
fn a_listen_address_without_a_port_is_rejected() {
    assert_eq!(
        HttpSettings::resolve(Some("0.0.0.0"), None, None, None, None)
            .expect_err("a bare address has no port")
            .field(),
        "listen"
    );
}

#[test]
fn a_listen_port_of_zero_is_rejected() {
    let error = HttpSettings::resolve(Some("0.0.0.0:0"), None, None, None, None)
        .expect_err("port zero asks the kernel to pick");

    assert_eq!(error.field(), "listen");
    assert_eq!(error.reason(), "must name a port other than zero");
}

#[test]
fn a_host_name_with_a_port_is_accepted() {
    let settings = HttpSettings::resolve(Some("localhost:8080"), None, None, None, None)
        .expect("a name is checked for shape only");

    assert_eq!(settings.listen(), "localhost:8080");
}

#[test]
fn an_ipv6_literal_is_accepted() {
    let settings = HttpSettings::resolve(Some("[::1]:8080"), None, None, None, None)
        .expect("a bracketed literal is a socket address");

    assert_eq!(settings.listen(), "[::1]:8080");
}

#[test]
fn a_grace_period_of_zero_is_rejected() {
    let error = HttpSettings::resolve(None, None, Some(0), None, None)
        .expect_err("a zero window would cut every request off");

    assert_eq!(error.field(), "grace_period_ms");
    assert_eq!(error.reason(), "must be in 1..=300000 (found 0)");
}

#[test]
fn a_grace_period_above_the_maximum_is_rejected() {
    let error = HttpSettings::resolve(None, None, Some(301_000), None, None)
        .expect_err("the window is bounded");

    assert_eq!(error.field(), "grace_period_ms");
    assert!(
        error.reason().contains("300000"),
        "the reason quotes the bound, got `{}`",
        error.reason()
    );
}

#[test]
fn a_shutdown_window_of_zero_is_rejected() {
    let error = HttpSettings::resolve(None, None, None, Some(0), None)
        .expect_err("a zero shutdown bound would truncate the drain");

    assert_eq!(error.field(), "graceful_shutdown_ms");
}

#[test]
fn a_shutdown_window_shorter_than_the_grace_period_is_rejected() {
    let error = HttpSettings::resolve(None, None, Some(10_000), Some(5_000), None)
        .expect_err("draining may not be bounded tighter than the grace period");

    assert_eq!(error.field(), "graceful_shutdown_ms");
    assert_eq!(
        error.reason(),
        "must be at least `grace_period_ms` (10000 ms)"
    );
}

#[test]
fn a_raised_grace_period_needs_a_raised_shutdown_window() {
    let error = HttpSettings::resolve(None, None, Some(60_000), None, None)
        .expect_err("the default shutdown window is now too short");

    assert_eq!(error.field(), "graceful_shutdown_ms");
}

#[test]
fn a_shutdown_window_equal_to_the_grace_period_is_accepted() {
    let settings = HttpSettings::resolve(None, None, Some(10_000), Some(10_000), None)
        .expect("equal windows are consistent");

    assert_eq!(settings.grace_period(), settings.graceful_shutdown());
}

#[test]
fn too_many_threads_are_rejected() {
    let error = HttpSettings::resolve(None, None, None, None, Some(257))
        .expect_err("the thread count is bounded");

    assert_eq!(error.field(), "threads");
    assert_eq!(error.reason(), "must be in 1..=256 (found 257)");
}

#[test]
fn zero_threads_are_rejected_because_pingora_panics_on_them() {
    let error = HttpSettings::resolve(None, None, None, None, Some(0))
        .expect_err("zero is not a runtime size");

    assert_eq!(error.field(), "threads");
    assert_eq!(error.reason(), "must be in 1..=256 (found 0)");
}

#[test]
fn the_largest_accepted_thread_count_is_256() {
    assert_eq!(
        HttpSettings::resolve(None, None, None, None, Some(256))
            .expect("256 is the inclusive upper bound")
            .threads(),
        256
    );
}

#[test]
fn the_error_renders_the_field_and_the_reason() {
    let error: HttpSettingsError = HttpSettings::resolve(Some("0.0.0.0:0"), None, None, None, None)
        .expect_err("port zero is rejected");

    assert_eq!(
        error.to_string(),
        "http setting `listen` is invalid: must name a port other than zero"
    );
}
