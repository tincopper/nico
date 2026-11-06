//! An OpenAI client through a passthrough route: the bytes are the client's, not a translation.
//!
//! `mode: passthrough` is the escape hatch for a route whose client protocol already is the
//! provider's. The data plane still publishes the endpoint, so the path and headers reach the peer
//! the document named — but no canonical value exists on the way: the client's framing, credential and
//! body travel unchanged, and the origin's status, headers and body travel back.
//!
//! The body is deliberately not a chat document (it holds a NUL and an invalid UTF-8 byte): a route
//! that decoded it would have to refuse it, so byte equality is the proof that nothing decoded it.
//!
//! One server per test binary: the route facts are compiled once and every assertion below is a fact
//! about those facts.

mod support;

use std::time::Duration;

use support::{
    http::{
        RunningServer,
        client::{Request, exchange},
        free_port, log,
        resources::{Resources, Route},
        source::TestSource,
        upstream::{MockUpstream, Reply},
        write_bootstrap,
    },
    token,
};

/// The provider key the document references.
const KEY: &str = "sk-fixture-passthrough-key";

/// The caller's credential, which the route's materialized credential must replace.
const CLIENT_KEY: &str = "sk-caller-key";

/// How long a fixture may take to reach a state it was told to reach.
const BUDGET: Duration = Duration::from_secs(20);

#[tokio::test(flavor = "current_thread")]
async fn a_passthrough_route_forwards_both_bodies_byte_for_byte_and_records_no_decode() {
    let captured = log::capture();

    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    write_bootstrap(directory.path(), port, false, 1, 1000);
    let credential = token::secret_file(directory.path(), "openai.key", KEY);

    // Not a chat document: the NUL and the invalid UTF-8 byte would be refused by a codec, so equal
    // bytes on the far side mean the pass-through never built a canonical value. The status and the
    // headers are the origin's, not this gateway's.
    let body = b"\x00\x01passthrough \xf0\x9f\x99\x82 bytes\xff".to_vec();
    let answer = b"origin-answer \xfe\xff".to_vec();
    let upstream = MockUpstream::start(Reply::Response {
        status: 207,
        headers: vec![
            (
                "content-type".to_owned(),
                "application/x-nico-test".to_owned(),
            ),
            ("x-mock-upstream".to_owned(), "yes".to_owned()),
        ],
        body: answer.clone(),
        chunk: 3,
    })
    .await;

    let document = Resources::new()
        .provider(
            "primary",
            &format!("http://{}", upstream.address()),
            &credential,
        )
        .route(Route::provider("llm-passthrough", "/v1/chat/", "primary").mode("passthrough"))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;

    let request = Request::post("/v1/chat/completions", "llm.test")
        .header("content-type", "application/octet-stream")
        .header("authorization", format!("Bearer {CLIENT_KEY}"))
        .header("x-client-tag", "keep-me")
        .header("x-request-id", "passthrough-one")
        .body(body.clone(), 5);
    let response = exchange(server.address(), &request).await;

    assert_eq!(
        response.status(),
        207,
        "the origin's status is forwarded unchanged, not {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(
        response.header("content-type"),
        Some("application/x-nico-test"),
        "the origin's headers describe the bytes the caller receives"
    );
    assert_eq!(response.header("x-mock-upstream"), Some("yes"));
    assert_eq!(
        response.header("content-length"),
        Some(answer.len().to_string().as_str()),
        "a passthrough answer keeps its framing; nothing re-encodes it"
    );
    assert_eq!(
        response.header("x-nico-trace-id"),
        None,
        "no translation ran, so there is no converted head to add a trace id to"
    );
    assert_eq!(
        response.body(),
        answer,
        "the answer changed on the way back"
    );

    let observed = upstream.wait_for_request(BUDGET).await;
    assert!(
        observed
            .head()
            .starts_with(b"POST /v1/chat/completions HTTP/1.1"),
        "the provider sees the endpoint path, got {:?}",
        String::from_utf8_lossy(observed.head())
    );
    assert_eq!(
        observed.body(),
        body,
        "the request body changed on the way to the origin"
    );
    assert_eq!(
        observed.header("content-length"),
        Some(body.len().to_string().as_str()),
        "the caller's framing is the one a passthrough route forwards"
    );
    assert_eq!(
        observed.header("authorization"),
        Some(format!("Bearer {KEY}").as_str()),
        "the provider's materialized credential replaces the caller's"
    );
    assert_eq!(
        observed.header("content-type"),
        Some("application/json"),
        "the provider's headers are written over the caller's"
    );
    assert_eq!(
        observed.header("x-client-tag"),
        Some("keep-me"),
        "a header the route does not own is forwarded"
    );
    assert!(
        !String::from_utf8_lossy(&observed.body()).contains(CLIENT_KEY),
        "the caller's credential must not appear in the provider request"
    );

    let line = log::wait_for_request_log(&captured, "passthrough-one", BUDGET).await;
    assert!(line.contains("llm.protocol=openai.chat"), "got: {line}");
    assert!(line.contains("llm.provider=openai"), "got: {line}");
    assert!(line.contains("http.status=207"), "got: {line}");
    assert!(
        !line.contains("llm.stream=") && !line.contains("llm.model="),
        "a passthrough route decoded nothing, so it reports no model or stream flag, got: {line}"
    );
    assert!(
        !line.contains(KEY) && !line.contains(CLIENT_KEY) && !line.contains("authorization"),
        "the record must not carry a credential, got: {line}"
    );

    assert_eq!(
        server.cancel_and_join().await,
        std::process::ExitCode::SUCCESS
    );
}
