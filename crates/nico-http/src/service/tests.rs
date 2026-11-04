use std::sync::Arc;

use nico_core::SnapshotHandle;
use pingora::{server::configuration::ServerConf, services::Service as ServiceApi};

use super::{server_conf, service};
use crate::{filter::FilterChain, settings::HttpSettings};

/// Settings whose two drain windows are `grace_ms` and `shutdown_ms`.
fn settings(grace_ms: u64, shutdown_ms: u64) -> HttpSettings {
    HttpSettings::resolve(None, Some(true), Some(grace_ms), Some(shutdown_ms), None)
        .expect("the fixture windows are in range")
}

fn conf(settings: &HttpSettings) -> Arc<ServerConf> {
    server_conf(settings)
}

#[test]
fn the_grace_period_rounds_up_to_whole_seconds() {
    let conf = conf(&settings(1_500, 30_000));

    assert_eq!(
        conf.grace_period_seconds,
        Some(2),
        "1.5 s rounds up so a stream is never cut mid-response"
    );
}

#[test]
fn the_shutdown_window_rounds_up_to_whole_seconds() {
    let conf = conf(&settings(1_000, 2_500));

    assert_eq!(conf.graceful_shutdown_timeout_seconds, Some(3));
}

#[test]
fn a_one_millisecond_window_is_still_one_second_and_never_zero() {
    let conf = conf(&settings(1, 1));

    assert_eq!(
        conf.grace_period_seconds,
        Some(1),
        "Pingora reads zero as `do not wait at all`"
    );
    assert_eq!(conf.graceful_shutdown_timeout_seconds, Some(1));
    assert_ne!(conf.grace_period_seconds, Some(0));
}

#[test]
fn both_windows_come_from_the_settings_rather_than_from_a_pingora_default() {
    let conf = conf(&settings(7_000, 60_000));

    assert_eq!(conf.grace_period_seconds, Some(7));
    assert_eq!(conf.graceful_shutdown_timeout_seconds, Some(60));
}

#[test]
fn exactly_one_upstream_attempt_is_configured() {
    // Pingora's loop is `while retries < max_retries`: `1` is one attempt, `0` would proxy nothing.
    assert_eq!(conf(&settings(5_000, 30_000)).max_retries, 1);
}

#[test]
fn the_thread_count_is_passed_through() {
    let settings = HttpSettings::resolve(None, None, None, None, Some(8)).expect("8 is in range");

    assert_eq!(conf(&settings).threads, 8);
}

#[test]
fn the_default_thread_count_is_pingoras_own_default() {
    let settings = HttpSettings::resolve(None, None, None, None, None).expect("defaults are valid");

    assert_eq!(
        conf(&settings).threads,
        1,
        "zero would panic Tokio's builder, so the default is a usable runtime size"
    );
}

#[test]
fn h2c_reaches_the_proxy_server_options() {
    let settings = settings(5_000, 30_000);
    let service = service(
        &conf(&settings),
        SnapshotHandle::new(),
        FilterChain::empty(),
        &settings,
    );

    let proxy = service
        .app_logic()
        .expect("the service holds its application until it starts");
    let options = proxy
        .server_options
        .as_ref()
        .expect("the service was built with explicit server options");

    assert!(options.h2c, "h2c is the only way to accept cleartext H2");
}

#[test]
fn h2c_stays_disabled_when_the_settings_do_not_ask_for_it() {
    let settings = HttpSettings::resolve(None, None, None, None, None).expect("defaults are valid");
    let service = service(
        &conf(&settings),
        SnapshotHandle::new(),
        FilterChain::empty(),
        &settings,
    );

    let proxy = service.app_logic().expect("the application is held");
    let options = proxy
        .server_options
        .as_ref()
        .expect("the service was built with explicit server options");

    assert!(!options.h2c);
}

#[test]
fn the_configured_listener_is_registered_once() {
    let settings = HttpSettings::resolve(Some("127.0.0.1:18080"), None, None, None, None)
        .expect("the fixture address is valid");
    let service = service(
        &conf(&settings),
        SnapshotHandle::new(),
        FilterChain::empty(),
        &settings,
    );

    assert_eq!(
        ServiceApi::listen_addresses(&service),
        Some(vec!["127.0.0.1:18080".to_owned()])
    );
}
