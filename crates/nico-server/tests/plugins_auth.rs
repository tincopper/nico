//! Authentication through a real server: a JWT route and an API-key route on the same process.
//!
//! Every assertion is on the wire, because the wire is the contract: which status, which stable code,
//! which challenge, which correlation id — and, for the log, that the request record names the plugin
//! that claimed the request while never containing the credential it was claimed with.

mod support;

use std::time::Duration;

use support::{
    http::{
        RunningServer,
        client::{Request, exchange},
        free_port, log,
        resources::{Plugin, Resources, Route},
        source::TestSource,
        upstream::{MockUpstream, Reply},
        write_bootstrap,
    },
    token,
};

/// The secret the fixture JWT is signed with.
const JWT_SECRET: &str = "an-hs256-secret-used-by-the-auth-scenario";

/// The API key the fixture accepts.
const API_KEY: &str = "partner-api-key-value";

/// How long the log assertions wait for a record Pingora writes after the response.
const LOG_BUDGET: Duration = Duration::from_secs(10);

#[tokio::test(flavor = "current_thread")]
async fn a_route_refuses_unauthenticated_requests_and_admits_a_signed_one() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    write_bootstrap(directory.path(), port, false, 1, 1000);
    let jwt_ref = token::secret_file(directory.path(), "jwt.secret", JWT_SECRET);
    let key_ref = token::secret_file(directory.path(), "api-key.secret", API_KEY);

    let upstream = MockUpstream::start(Reply::Body {
        body: b"ok".to_vec(),
        chunk: usize::MAX,
    })
    .await;

    // Two routes, one plugin each: the two authentication kinds answer the same statuses, so one
    // document proves both halves of the contract without a second server.
    let document = Resources::new()
        .upstream("api-backend", upstream.address())
        .plugin(Plugin::jwt("public-jwt", &jwt_ref))
        .plugin(Plugin::api_key("partner-keys", &key_ref))
        .route(Route::new("api", "/api/", "api-backend").plugins(&["public-jwt"]))
        .route(Route::new("partner", "/partner/", "api-backend").plugins(&["partner-keys"]))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source.clone()).await;
    let address = server.address();
    let logs = log::capture();

    // --- JWT ---------------------------------------------------------------

    // A request with no credential is told how to authenticate: 401 with a challenge, the stable
    // `auth_missing` code, and the correlation id the caller supplied.
    let request = Request::get("/api/items", "auth.test").header("x-request-id", "auth-missing");
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        401,
        "expected 401 without a credential, got {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(response.header("www-authenticate"), Some("Bearer"));
    assert_eq!(response.header("x-nico-error-code"), Some("auth_missing"));
    assert_eq!(response.header("x-nico-trace-id"), Some("auth-missing"));
    assert_eq!(response.json_code().as_deref(), Some("auth_missing"));

    // A signed, unexpired token is admitted, and the request log names the plugin instance that
    // authenticated it.
    let admitted = token::token(JWT_SECRET, 300, "user-42");
    let request = Request::get("/api/items", "auth.test")
        .header("authorization", format!("Bearer {admitted}"))
        .header("x-request-id", "auth-valid");
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        200,
        "a valid token must be admitted, got {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(response.body(), b"ok");
    assert_eq!(
        upstream.observation().requests.len(),
        1,
        "the upstream saw it"
    );
    let line = log::wait_for_request_log(&logs, "auth-valid", LOG_BUDGET).await;
    assert!(
        line.contains("authn=public-jwt"),
        "the request log must name the authenticating plugin: {line}"
    );
    assert!(
        !line.contains(&admitted),
        "the token must not reach the log: {line}"
    );
    assert!(
        line.contains("config.generation=1"),
        "the request was served by the first published generation: {line}"
    );

    // An expired token is refused with the same code a bad signature would produce: the code says
    // "not acceptable", never *why*, so a caller cannot probe which keys or lifetimes exist.
    let expired = token::token(JWT_SECRET, -60, "user-42");
    let request = Request::get("/api/items", "auth.test")
        .header("authorization", format!("Bearer {expired}"))
        .header("x-request-id", "auth-expired");
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        401,
        "an expired token must be refused, got {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(response.header("x-nico-error-code"), Some("auth_invalid"));
    assert_eq!(response.header("www-authenticate"), Some("Bearer"));
    assert_eq!(
        upstream.observation().requests.len(),
        1,
        "nothing was forwarded"
    );

    // --- API key -----------------------------------------------------------

    let request = Request::get("/partner/orders", "auth.test")
        .header("authorization", format!("Bearer {API_KEY}"))
        .header("x-request-id", "key-valid");
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        200,
        "a configured key must be admitted, got {}: {}",
        response.status(),
        response.text()
    );
    let line = log::wait_for_request_log(&logs, "key-valid", LOG_BUDGET).await;
    assert!(
        line.contains("authn=partner-keys"),
        "the request log must name the key set: {line}"
    );
    assert!(
        !line.contains(API_KEY),
        "the key must not reach the log: {line}"
    );

    let request = Request::get("/partner/orders", "auth.test")
        .header("authorization", "Bearer not-a-configured-key")
        .header("x-request-id", "key-wrong");
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        401,
        "an unconfigured key must be refused, got {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(response.header("x-nico-error-code"), Some("auth_invalid"));
    assert!(
        !response.text().contains("not-a-configured-key"),
        "the refusal must not echo the presented key: {}",
        response.text()
    );
    assert_eq!(
        upstream.observation().requests.len(),
        2,
        "nothing else was forwarded"
    );

    // The route that names no plugin keeps behaving exactly as it did before plugins existed.
    let response = support::http::client::get(address, "/open/items", "auth.test").await;
    assert_eq!(
        response.status(),
        404,
        "a path no route matches must still be a 404, got {}",
        response.status()
    );

    let exit = server.cancel_and_join().await;
    assert_eq!(exit, std::process::ExitCode::SUCCESS);
}
