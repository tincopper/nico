//! A peer whose chain does not lead to the configured roots is refused, and the refusal is explicit.
//!
//! This file is its own test binary because the trust roots are a process-level fact: Pingora builds
//! its connector once, when the service is built, so a process configured with one bundle cannot
//! also observe a second one. Everything else about the handshake — the destination, the route, the
//! listener — is identical to `tls_upstream.rs`, which is the point: the only difference between a
//! verified handshake and this refusal is *which root set the process was given*.
//!
//! The counterpart of the deleted "no TLS backend" branch lives here as well. The intent it carried
//! was "fail loudly rather than downgrade to plaintext", and the assertion below is that intent
//! against a real peer: a certificate this process does not trust produces `502 upstream_unreachable`
//! and never a served request.

mod support;

use std::time::Duration;

use support::{
    http::{
        RunningServer,
        client::{Request, exchange},
        free_port, log,
        resources::{Resources, Route},
        source::TestSource,
        write_bootstrap_trusting,
    },
    tls::{TlsOrigin, fixture},
};

/// What the origin answers with, which none of this file's requests may reach.
const ANSWER: &[u8] = br#"{"origin":"untrusted"}"#;

/// The correlation id of the refused request, so its log line can be found among the others.
const TRACE_ID: &str = "tls-untrusted";

/// How long a fixture may take to reach a state it was told to reach.
const BUDGET: Duration = Duration::from_secs(20);

#[tokio::test(flavor = "current_thread")]
async fn a_chain_that_does_not_lead_to_the_configured_roots_is_refused() {
    // Installed before the server starts: the refusal assertion is about the request record.
    let captured = log::capture();

    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    // Deliberately the *other* fixture root: the peer presents a chain signed by `test-root`, so
    // nothing it offers can be verified here. Omitting the section instead would fall back to the
    // host's platform store, which would make the scenario a statement about the machine.
    write_bootstrap_trusting(directory.path(), port, &fixture("other-root.pem"));

    let untrusted = TlsOrigin::start("localhost.pem", "localhost.key", ANSWER).await;

    let document = Resources::new()
        .tls_upstream("untrusted", "localhost", untrusted.port())
        .route(Route::new("tls-untrusted", "/tls-untrusted", "untrusted"))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;

    let request = Request::get("/tls-untrusted", "tls.test").header("x-request-id", TRACE_ID);
    let response = exchange(server.address(), &request).await;

    assert_eq!(
        response.status(),
        502,
        "an untrusted chain must not be served, got {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(
        response.json_code().as_deref(),
        Some("upstream_unreachable"),
        "the caller learns the peer is unusable and nothing about why"
    );
    assert_eq!(
        response.body().windows(6).filter(|w| *w == ANSWER).count(),
        0,
        "no byte of the origin's answer may reach the caller"
    );
    // The cause is a log fact: the same refusal must not name the certificate error to the caller.
    let body = response.text();
    assert!(
        !body.contains("certificate") && !body.contains("UnknownIssuer"),
        "the refusal body must not carry the TLS diagnostic: {body}"
    );

    // The origin completed a TCP connection and the client aborted the handshake: the refusal
    // happened during verification, not because the peer was unreachable.
    let observed = untrusted.wait_for_handshake_failure(BUDGET).await;
    assert_eq!(
        observed.handshakes, 0,
        "no handshake may complete against a root set that does not cover it: {observed:?}"
    );
    assert_eq!(
        observed.requests.len(),
        0,
        "and no request may reach the origin"
    );

    // The cause stays in the log, where an operator can tell this refusal from a name mismatch.
    let line = log::wait_for_request_log(&captured, TRACE_ID, BUDGET).await;
    assert!(
        line.contains("upstream request failed"),
        "the record names the failure as an upstream one: {line}"
    );
    assert!(
        line.contains("UnknownIssuer"),
        "the record carries the certificate error that explains the refusal: {line}"
    );
}
