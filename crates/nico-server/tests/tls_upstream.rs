//! A real TLS upstream: a handshake that verifies, and a name that does not.
//!
//! Every other HTTP scenario in this suite talks to a plaintext mock, so none of them would notice if
//! the TLS backend were not compiled in, if the configured trust roots were ignored, or if hostname
//! verification were switched off. This file exists to make those three facts observable on a real
//! handshake against a real listener:
//!
//! 1. **the configured bundle is used** — the origin's certificate is signed by a fixture CA that no
//!    platform trust store knows, so a successful `200` can only come from `proxy.tls.ca_file`;
//! 2. **both target kinds reach it** — an `upstreams[].scheme: https` route and a provider route
//!    whose `base_url` is `https://…` produce the same destination, so both complete a handshake;
//! 3. **the hostname is verified** — a chain that verifies against the same root while its
//!    `subjectAltName` names another host is refused, and the origin observes a client that aborted
//!    the handshake rather than a connection that never arrived.
//!
//! One server per test binary: the trust roots are process-level and fixed when the service is built,
//! so every scenario here shares the same configured CA and differs only in what the origin presents.

mod support;

use std::time::Duration;

use support::{
    http::{
        RunningServer,
        client::{Request, exchange},
        free_port, log,
        resources::{Resources, Route},
        source::TestSource,
        write_bootstrap,
    },
    tls::TlsOrigin,
    token,
};

/// What every TLS origin in this file answers with.
const ANSWER: &[u8] = br#"{"origin":"tls"}"#;

/// The provider key the document references; the route materializes it for the peer.
const KEY: &str = "sk-fixture-tls-key";

/// The caller's body through the provider route, which a passthrough route must forward unchanged.
const PROMPT: &[u8] = br#"{"model":"gpt-fixture","messages":[]}"#;

/// The correlation id of the refused request, so its log line can be found among the others.
const MISMATCH_TRACE: &str = "tls-mismatch";

/// How long a fixture may take to reach a state it was told to reach.
const BUDGET: Duration = Duration::from_secs(20);

#[tokio::test(flavor = "current_thread")]
async fn a_verified_handshake_reaches_the_origin_and_a_mismatched_name_is_refused() {
    // Installed before the server starts: the refusal assertion is about the request record, which is
    // written on Pingora's worker thread.
    let captured = log::capture();

    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    write_bootstrap(directory.path(), port, false, 1, 1000);
    let credential = token::secret_file(directory.path(), "openai.key", KEY);

    // Signed by the fixture CA, `subjectAltName = DNS:localhost, IP:127.0.0.1`.
    let verified = TlsOrigin::start("localhost.pem", "localhost.key", ANSWER).await;
    // Signed by the same CA, `subjectAltName = DNS:example.invalid` only: the chain verifies and the
    // name does not, which is the case a `ca_file` alone cannot catch.
    let mismatched = TlsOrigin::start("wrong-name.pem", "wrong-name.key", ANSWER).await;

    let document = Resources::new()
        .tls_upstream("verified", "localhost", verified.port())
        .tls_upstream("mismatched", "127.0.0.1", mismatched.port())
        .provider(
            "primary",
            &format!("https://localhost:{}", verified.port()),
            &credential,
        )
        .route(Route::new("tls-ok", "/tls-ok", "verified"))
        .route(Route::new("tls-mismatch", "/tls-mismatch", "mismatched"))
        .route(Route::provider("llm-passthrough", "/v1/chat/", "primary").mode("passthrough"))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;
    let address = server.address();

    // --- an https upstream route completes a verified handshake ------------------------------
    let response = exchange(address, &Request::get("/tls-ok", "tls.test")).await;
    assert_eq!(
        response.status(),
        200,
        "a verified handshake must reach the origin, got {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(
        response.body(),
        ANSWER,
        "the origin's body arrived over TLS unchanged"
    );

    let observed = verified.wait_for_request(BUDGET).await;
    assert_eq!(
        observed.request_line(),
        "GET /tls-ok HTTP/1.1",
        "the origin sees the route's path, got {:?}",
        observed.head_text()
    );
    assert_eq!(
        observed.header("host"),
        Some(format!("localhost:{}", verified.port())),
        "the peer is addressed by the name the certificate is signed for"
    );

    // --- an https provider route reaches the same kind of peer -------------------------------
    // The `base_url` scheme becomes the destination's scheme, so a provider route completes the same
    // handshake. The route is `passthrough`, which is the one mode that does not decode a body, so
    // this asserts the transport alone.
    let request = Request::post("/v1/chat/completions", "llm.test")
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-caller-key")
        .body(PROMPT.to_vec(), PROMPT.len());
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        200,
        "a provider route over https must reach its endpoint, got {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(response.body(), ANSWER);

    let observed = verified.wait_for_request(BUDGET).await;
    assert_eq!(
        observed.request_line(),
        "POST /v1/chat/completions HTTP/1.1",
        "the front-end's operation path reaches the origin, got {:?}",
        observed.head_text()
    );
    assert_eq!(
        observed.body, PROMPT,
        "the passthrough body reached a TLS peer unchanged"
    );
    assert_eq!(
        observed.header("authorization").as_deref(),
        Some("Bearer sk-fixture-tls-key"),
        "the route's own credential is what the TLS peer sees"
    );

    // --- a chain that verifies while the name does not --------------------------------------
    let request = Request::get("/tls-mismatch", "tls.test").header("x-request-id", MISMATCH_TRACE);
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        502,
        "a certificate for another name must not be accepted, got {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(
        response.json_code().as_deref(),
        Some("upstream_unreachable"),
        "a refused handshake is an unreachable peer, not a caller error"
    );

    // The origin observed a client that completed the TCP connection and then aborted the handshake,
    // which is what distinguishes a certificate refusal from a peer that was never reachable.
    let observed = mismatched.wait_for_handshake_failure(BUDGET).await;
    assert!(
        observed.handshakes == 0,
        "no handshake may complete for a name the certificate does not carry: {observed:?}"
    );
    assert_eq!(
        observed.requests.len(),
        0,
        "and no request may reach the origin"
    );

    // The cause is a typed diagnostic in the request record — never in the response body, which the
    // closed refusal header set above already asserted.
    let line = log::wait_for_request_log(&captured, MISMATCH_TRACE, BUDGET).await;
    assert!(
        line.contains("upstream request failed"),
        "the record names the failure as an upstream one: {line}"
    );
    assert!(
        line.contains("not valid for name"),
        "the record distinguishes a name mismatch from an untrusted chain: {line}"
    );
}
