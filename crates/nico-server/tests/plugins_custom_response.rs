//! A refusal whose presentation the document configures, and one it does not.
//!
//! What a plugin may change is a closed set: the status (4xx only), the content type and the body. It
//! may not change the stable error code, and it may not add a header — `x-nico-error-code`,
//! `x-nico-trace-id`, `www-authenticate` and `content-length` are written by the data plane from the
//! compiled rejection, so a custom body cannot desynchronise the response or cost an operator the
//! ability to join a refusal to a log line.

mod support;

use support::{
    http::{
        RunningServer,
        client::{Request, exchange, get},
        free_port,
        resources::{Plugin, Resources, Route},
        source::TestSource,
        upstream::{MockUpstream, Reply},
        wait_for_status, write_bootstrap,
    },
    token,
};

/// The secret the fixture JWT is signed with.
const JWT_SECRET: &str = "an-hs256-secret-used-by-the-response-scenario";

/// The API key the fixture accepts.
const API_KEY: &str = "partner-api-key-value";

/// The body a JSON content type renders: an object, with the whitelisted variables inside it.
const JSON_BODY: &str = r#"{detail: "${message}", plugin_code: "${code}", trace: "${trace_id}"}"#;

/// The body a non-JSON content type renders: a literal template.
const HTML_BODY: &str = r#""<h1>denied</h1><p>${code}</p>""#;

#[tokio::test(flavor = "current_thread")]
async fn a_configured_refusal_replaces_the_body_but_not_the_observability() {
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

    // Two instances, two presentations: a JSON object behind 403, and a literal HTML body behind the
    // default 401. The route with no plugins stays the baseline.
    let document = Resources::new()
        .upstream("api-backend", upstream.address())
        .plugin(Plugin::jwt("public-jwt", &jwt_ref).error_response(
            403,
            "application/json",
            JSON_BODY,
        ))
        .plugin(Plugin::api_key("partner-keys", &key_ref).error_response(
            401,
            "text/html",
            HTML_BODY,
        ))
        .route(Route::new("api", "/api/", "api-backend").plugins(&["public-jwt"]))
        .route(Route::new("partner", "/partner/", "api-backend").plugins(&["partner-keys"]))
        .route(Route::new("open", "/open/", "api-backend"))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source.clone()).await;
    let address = server.address();

    let open = Request::get("/open/items", "custom.test");
    let baseline = wait_for_status(address, &open, 200).await;
    assert_eq!(
        baseline.body(),
        b"ok",
        "a route with no plugins behaves exactly as before plugins existed"
    );

    // A JSON content type carries a structured body: the document writes an object, the renderer
    // escapes every value it substitutes, and the stable code still travels in its own header.
    let request = Request::get("/api/items", "custom.test").header("x-request-id", "custom-json");
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        403,
        "the configured status replaces the default 401, got {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(response.header("content-type"), Some("application/json"));
    assert_eq!(response.header("x-nico-error-code"), Some("auth_missing"));
    assert_eq!(response.header("x-nico-trace-id"), Some("custom-json"));
    assert_eq!(
        response.header("www-authenticate"),
        Some("Bearer"),
        "a custom body does not remove the challenge"
    );
    assert_eq!(
        response.text(),
        r#"{"detail":"no credential was presented","plugin_code":"auth_missing","trace":"custom-json"}"#,
        "the document's own object, with the whitelisted variables substituted"
    );
    let rendered_length = response.body().len().to_string();
    assert_eq!(
        response.header("content-length"),
        Some(rendered_length.as_str()),
        "the length is the rendered body's, not the template's"
    );

    // A presented-but-refused credential keeps the same presentation and the same *code family*: the
    // body cannot become a way to tell "expired" from "unknown key".
    let expired = token::token(JWT_SECRET, -60, "user-42");
    let request = Request::get("/api/items", "custom.test")
        .header("authorization", format!("Bearer {expired}"))
        .header("x-request-id", "custom-json-invalid");
    let response = exchange(address, &request).await;
    assert_eq!(response.status(), 403);
    assert_eq!(response.header("x-nico-error-code"), Some("auth_invalid"));
    assert!(
        response.text().contains(r#""plugin_code":"auth_invalid""#),
        "the template variable carries the code the header does: {}",
        response.text()
    );

    // A non-JSON content type carries a literal body. The status stays the kind's default here, so
    // the same document proves both "configured shape" and "configured status only".
    let request =
        Request::get("/partner/orders", "custom.test").header("x-request-id", "custom-html");
    let response = exchange(address, &request).await;
    assert_eq!(response.status(), 401);
    assert_eq!(response.header("content-type"), Some("text/html"));
    assert_eq!(response.header("x-nico-error-code"), Some("auth_missing"));
    assert_eq!(response.header("x-nico-trace-id"), Some("custom-html"));
    assert_eq!(response.text(), "<h1>denied</h1><p>auth_missing</p>");

    // The upstream was never contacted for any refusal, and the baseline route is still healthy.
    assert_eq!(
        upstream.observation().requests.len(),
        1,
        "refusals answer before the upstream is chosen"
    );
    let response = get(address, "/open/again", "custom.test").await;
    assert_eq!(response.status(), 200);

    let exit = server.cancel_and_join().await;
    assert_eq!(exit, std::process::ExitCode::SUCCESS);
}
