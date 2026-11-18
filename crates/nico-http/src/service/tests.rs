use std::path::{Path, PathBuf};

use pingora::services::Service as ServiceApi;

use super::{ServiceBuildError, server_conf, service};
use crate::{
    InboundTls, Listener, ServerSettings, TlsPolicy, settings::HttpSettings,
    state::HttpStateHandle, test_support::tls_bundle,
};

/// Trust roots that are a bundle this crate owns.
///
/// A service cannot be built against the platform store here: Pingora reads it while building its
/// TLS connector, and a test process may not be allowed to read the host's trust settings. The
/// bundle makes the root set a fact of the test instead of a fact of the machine.
fn tls() -> TlsPolicy {
    TlsPolicy::from_bootstrap(Some(tls_bundle().to_owned()))
}

/// Runtime settings whose two drain windows are `grace_ms` and `shutdown_ms`.
fn runtime(grace_ms: u64, shutdown_ms: u64) -> ServerSettings {
    ServerSettings::resolve(None, Some(grace_ms), Some(shutdown_ms))
        .expect("the fixture windows are in range")
}

/// Http settings naming the given plaintext listeners.
fn settings(listens: &[&str], h2c: bool) -> HttpSettings {
    let listeners = listens
        .iter()
        .map(|listen| Listener::Plain {
            listen: (*listen).to_owned(),
        })
        .collect();
    HttpSettings::resolve(listeners, Some(h2c), tls()).expect("the fixture listeners are valid")
}

/// The `localhost` identity the workspace's shared TLS fixtures hold, as real files on disk.
fn localhost_identity() -> InboundTls {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../nico-server/tests/fixtures/tls");
    InboundTls::from_bootstrap(
        fixtures.join("localhost.pem"),
        fixtures.join("localhost.key"),
    )
}

#[test]
fn the_grace_period_rounds_up_to_whole_seconds() {
    let conf = server_conf(&runtime(1_500, 30_000), &tls());

    assert_eq!(
        conf.grace_period_seconds,
        Some(2),
        "1.5 s rounds up so a stream is never cut mid-response"
    );
}

#[test]
fn the_shutdown_window_rounds_up_to_whole_seconds() {
    let conf = server_conf(&runtime(1_000, 2_500), &tls());

    assert_eq!(conf.graceful_shutdown_timeout_seconds, Some(3));
}

#[test]
fn a_one_millisecond_window_is_still_one_second_and_never_zero() {
    let conf = server_conf(&runtime(1, 1), &tls());

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
    let conf = server_conf(&runtime(7_000, 60_000), &tls());

    assert_eq!(conf.grace_period_seconds, Some(7));
    assert_eq!(conf.graceful_shutdown_timeout_seconds, Some(60));
}

#[test]
fn the_attempt_ceiling_covers_every_chain_a_document_can_declare() {
    // Pingora's loop is `while retries < max_retries`, and it is the loop's *fuse*, not the
    // sequence: a route that declared nothing makes one attempt because its walk answers "no"
    // immediately. The fuse must be large enough never to be what ends a legitimate chain — and the
    // longest one a document can express is `retry_budget` (≤ 4) times the pool's members (≤ 8) times
    // the chain's targets.
    let configured = server_conf(&runtime(5_000, 30_000), &tls()).max_retries;
    assert!(
        configured > 1,
        "a route that declares a fallback needs more than one round: {configured}"
    );
    assert!(
        configured >= 8,
        "the fuse must exceed the longest declarable chain: {configured}"
    );
    assert_eq!(
        configured, 128,
        "the ceiling is a constant, and this test pins it so lowering it is a deliberate change"
    );
}

#[test]
fn the_thread_count_is_passed_through() {
    let threads = ServerSettings::resolve(Some(8), None, None).expect("8 is in range");

    assert_eq!(server_conf(&threads, &tls()).threads, 8);
}

#[test]
fn the_default_thread_count_is_pingoras_own_default() {
    assert_eq!(
        server_conf(&ServerSettings::new(), &tls()).threads,
        1,
        "zero would panic Tokio's builder, so the default is a usable runtime size"
    );
}

#[test]
fn the_trust_roots_reach_the_server_configuration_from_the_runtime() {
    // `server_conf` takes the trust roots separately from the runtime: the two are different
    // sections of the bootstrap file (`runtime` and `proxy`), and the CA is read by the connector
    // Pingora builds once, not by the runtime the requests run on.
    let policy = TlsPolicy::from_bootstrap(Some(PathBuf::from("/etc/nico/roots.pem")));
    let conf = server_conf(&ServerSettings::new(), &policy);

    assert_eq!(
        conf.ca_file.as_deref(),
        Some("/etc/nico/roots.pem"),
        "Pingora reads this field once, when it builds the connector the service uses"
    );
}

#[test]
fn h2c_reaches_the_proxy_server_options() {
    let settings = settings(&["0.0.0.0:8080"], true);
    let service = service(
        &server_conf(&ServerSettings::new(), &tls()),
        HttpStateHandle::new(),
        &settings,
    )
    .expect("the plaintext fixture service builds");

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
    let settings = settings(&["0.0.0.0:8080"], false);
    let service = service(
        &server_conf(&ServerSettings::new(), &tls()),
        HttpStateHandle::new(),
        &settings,
    )
    .expect("the plaintext fixture service builds");

    let proxy = service.app_logic().expect("the application is held");
    let options = proxy
        .server_options
        .as_ref()
        .expect("the service was built with explicit server options");

    assert!(!options.h2c);
}

#[test]
fn the_configured_listener_is_registered_once() {
    let settings = settings(&["127.0.0.1:18080"], false);
    let service = service(
        &server_conf(&ServerSettings::new(), &tls()),
        HttpStateHandle::new(),
        &settings,
    )
    .expect("the plaintext fixture service builds");

    assert_eq!(
        ServiceApi::listen_addresses(&service),
        Some(vec!["127.0.0.1:18080".to_owned()])
    );
}

#[test]
fn every_named_listener_is_registered_in_the_named_order() {
    let settings = settings(&["127.0.0.1:18080", "127.0.0.1:18081"], false);
    let service = service(
        &server_conf(&ServerSettings::new(), &tls()),
        HttpStateHandle::new(),
        &settings,
    )
    .expect("distinct plaintext listeners build");

    assert_eq!(
        ServiceApi::listen_addresses(&service),
        Some(vec![
            "127.0.0.1:18080".to_owned(),
            "127.0.0.1:18081".to_owned()
        ])
    );
}

#[test]
fn a_tls_listener_is_registered_as_a_tls_endpoint() {
    let identity = localhost_identity();
    let listeners = vec![Listener::Tls {
        listen: "127.0.0.1:18443".to_owned(),
        tls: identity,
    }];
    let settings = HttpSettings::resolve(listeners, None, tls())
        .expect("the fixture listener is a valid section");
    let service = service(
        &server_conf(&ServerSettings::new(), &tls()),
        HttpStateHandle::new(),
        &settings,
    )
    .expect("the workspace fixture identity is readable and valid");

    assert_eq!(
        ServiceApi::listen_addresses(&service),
        Some(vec!["127.0.0.1:18443".to_owned()]),
        "the TLS entry registers its address, through Pingora's TLS path"
    );
}

#[test]
fn a_plaintext_and_a_tls_listener_coexist() {
    let listeners = vec![
        Listener::Plain {
            listen: "127.0.0.1:18080".to_owned(),
        },
        Listener::Tls {
            listen: "127.0.0.1:18443".to_owned(),
            tls: localhost_identity(),
        },
    ];
    let settings = HttpSettings::resolve(listeners, None, tls())
        .expect("one listener per transport is the whole point");
    let service = service(
        &server_conf(&ServerSettings::new(), &tls()),
        HttpStateHandle::new(),
        &settings,
    )
    .expect("both transports build");

    assert_eq!(
        ServiceApi::listen_addresses(&service),
        Some(vec![
            "127.0.0.1:18080".to_owned(),
            "127.0.0.1:18443".to_owned()
        ])
    );
}

#[test]
fn an_unreadable_tls_identity_is_a_service_error_not_a_panic() {
    let identity = InboundTls::from_bootstrap(
        PathBuf::from("/nonexistent/nico-cert.pem"),
        PathBuf::from("/nonexistent/nico-key.pem"),
    );
    let listeners = vec![Listener::Tls {
        listen: "127.0.0.1:18443".to_owned(),
        tls: identity,
    }];
    let settings = HttpSettings::resolve(listeners, None, tls())
        .expect("the paths are a legal setting; reading them is the service's job");

    let error = match service(
        &server_conf(&ServerSettings::new(), &tls()),
        HttpStateHandle::new(),
        &settings,
    ) {
        Ok(_service) => panic!("a missing identity must not produce a listener"),
        Err(error) => error,
    };

    assert!(
        matches!(error, ServiceBuildError::CertificateRead { .. }),
        "the first unreadable file is reported, got {error:?}"
    );
}
