//! An OpenAI client through a converted Gemini route, end to end.
//!
//! The one row whose request target carries the model, exercised on a real server: the client speaks
//! `openai.chat`, the upstream speaks `generateContent`, and the pinned path is visible on the wire.
//! The scenarios are the ones this dialect lives or dies by:
//!
//! 1. the upstream request line names the mapped physical model and `alt=sse`, the caller's
//!    credential does not travel, and the route's `x-goog-api-key` does;
//! 2. a buffered answer is folded from the event stream the upstream always writes;
//! 3. a streamed answer is translated chunk by chunk;
//! 4. a model the route does not serve is `403` before any body byte reaches the provider;
//! 5. a `passthrough` route of this kind refuses publication, and the last good generation keeps
//!    serving.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "the shared helpers sit outside the test functions, where the restriction lints apply"
)]

mod support;

use support::{
    http::{
        RunningServer,
        client::{Client, READ_BUDGET, Request, exchange},
        free_port, log,
        resources::{Resources, Route},
        source::TestSource,
        upstream::{MockUpstream, Reply},
        write_bootstrap,
    },
    token,
};

/// How long a fixture may take to reach a state it was told to reach.
const BUDGET: std::time::Duration = std::time::Duration::from_secs(20);

/// The provider key the document references.
const KEY: &str = "goog-fixture-provider-key";

/// The caller's own credential. A converted route must never send it to the provider.
const CLIENT_KEY: &str = "sk-client-key-must-not-travel";

/// The logical model the caller writes, and the physical one the route pins into the path.
const LOGICAL: &str = "fast";
const PHYSICAL: &str = "gemini-2.0-flash";

/// The upstream path one request of this route must take.
fn upstream_path() -> String {
    format!("/v1beta/models/{PHYSICAL}:streamGenerateContent?alt=sse")
}

/// Builds the Chat Completions request the client sends.
fn chat_request(model: &str, stream: bool) -> Vec<u8> {
    format!(
        r#"{{"model":"{model}","messages":[{{"role":"user","content":"hello there"}}],"stream":{stream}}}"#
    )
    .into_bytes()
}

/// Frames one provider stream document as a server-sent event.
fn sse(payload: &str) -> Vec<u8> {
    format!("data: {payload}\n\n").into_bytes()
}

/// The chunk sequence the Gemini upstream writes: identity, two text parts, counts, the reason.
///
/// One chunk of this dialect is a whole `GenerateContentResponse`, and the endpoint is always the
/// streaming operation — even a buffered answer arrives as this stream and is folded.
fn gemini_events() -> Vec<Vec<u8>> {
    let identity = format!(
        r#"{{"responseId":"resp-gem","modelVersion":"{PHYSICAL}","candidates":[{{"content":{{"role":"model","parts":[{{"text":"Hello"}}]}}}}]}}"#
    );
    let finish = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":" world"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2}}"#;
    vec![sse(&identity), sse(finish)]
}

/// The origin's failure document, whose message must never reach the caller.
fn provider_failure() -> Vec<u8> {
    br#"{"error":{"code":429,"message":"SECRET provider detail","status":"RESOURCE_EXHAUSTED"}}"#
        .to_vec()
}

/// Parses a JSON document from a test-owned byte string.
fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).expect("the fixture is a JSON document")
}

/// Returns the `data:` payloads of a translated event stream, skipping the terminator.
fn client_events(body: &[u8]) -> Vec<serde_json::Value> {
    let text = std::str::from_utf8(body).expect("the translated stream is UTF-8");
    text.split("\n\n")
        .filter(|frame| !frame.is_empty())
        .filter_map(|frame| {
            let data = frame
                .strip_prefix("data: ")
                .unwrap_or_else(|| panic!("a client event is not a data frame: {frame:?}"));
            (data != "[DONE]").then(|| json(data.as_bytes()))
        })
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn an_openai_client_is_translated_to_a_gemini_upstream_and_back() {
    // Installed before the server starts: the record of the first request must land in this buffer.
    let captured = log::capture();

    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    write_bootstrap(directory.path(), port, false, 1, 1000);
    let credential = token::secret_file(directory.path(), "gemini.key", KEY);

    let upstream = MockUpstream::start(Reply::StreamWithHeaders {
        headers: vec![("content-type".to_owned(), "text/event-stream".to_owned())],
        events: gemini_events(),
        gap: std::time::Duration::ZERO,
    })
    .await;
    let refused = MockUpstream::start(Reply::Response {
        status: 429,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: provider_failure(),
        chunk: usize::MAX,
    })
    .await;

    let base_url = |upstream: &MockUpstream| format!("http://{}", upstream.address());
    let document = |passthrough: bool| {
        let route = Route::provider("llm-gemini", "/v1/chat/", "primary").protocol("openai.chat");
        let route = if passthrough {
            route.mode("passthrough")
        } else {
            route
        };
        Resources::new()
            .provider_of_kind(
                "primary",
                "gemini",
                &base_url(&upstream),
                &credential,
                &[(LOGICAL, PHYSICAL)],
            )
            .provider_of_kind(
                "limited",
                "gemini",
                &base_url(&refused),
                &credential,
                &[(LOGICAL, PHYSICAL)],
            )
            .route(route)
            .route(
                Route::provider("llm-gemini-limited", "/v1/gem-limited/", "limited")
                    .protocol("openai.chat"),
            )
            .render()
    };
    let source = TestSource::with_document(&document(false));
    let server = RunningServer::start(directory.path(), source.clone()).await;
    let address = server.address();

    // --- 1. the upstream request line, and a folded answer --------------------------------------
    let request = Request::post("/v1/chat/completions", "llm.test")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {CLIENT_KEY}"))
        .header("x-request-id", "gem-one")
        .body(chat_request(LOGICAL, false), usize::MAX);
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        200,
        "a folded success must be 200, not {}: {}",
        response.status(),
        response.text()
    );
    let answer = json(response.body());
    assert_eq!(
        answer["id"], "resp-gem",
        "the answer is the one the upstream named"
    );
    assert_eq!(answer["model"], PHYSICAL);
    assert_eq!(
        answer["choices"][0]["message"]["content"], "Hello world",
        "the buffered answer is folded from the event stream"
    );
    assert_eq!(answer["choices"][0]["finish_reason"], "stop");
    assert_eq!(answer["usage"]["prompt_tokens"], 3);
    assert_eq!(answer["usage"]["completion_tokens"], 2);

    let observed = upstream.wait_for_request(BUDGET).await;
    assert!(
        observed
            .head()
            .starts_with(format!("POST {} HTTP/1.1", upstream_path()).as_bytes()),
        "the request line pins the physical model and the streaming operation, got {:?}",
        String::from_utf8_lossy(observed.head())
    );
    assert_eq!(
        observed.header("x-goog-api-key"),
        Some(KEY),
        "the route's materialized credential is the one the dialect sends"
    );
    assert_eq!(
        observed.header("authorization"),
        None,
        "the caller's OpenAI credential is not the Gemini dialect's spelling"
    );
    assert!(
        !String::from_utf8_lossy(&observed.body()).contains(CLIENT_KEY),
        "the caller's credential must not appear in the provider request"
    );
    let forwarded = json(&observed.body());
    assert!(
        forwarded.get("model").is_none(),
        "the model travels in the request line, not in the body: {forwarded}"
    );
    assert_eq!(
        forwarded["contents"][0]["role"], "user",
        "the conversation reaches the provider as the dialect's own document"
    );
    assert_eq!(forwarded["contents"][0]["parts"][0]["text"], "hello there");

    // --- 2. a streamed answer -------------------------------------------------------------------
    let mut client = Client::connect(address).await;
    let request = Request::post("/v1/chat/completions", "llm.test")
        .header("content-type", "application/json")
        .header("x-request-id", "gem-sse")
        .body(chat_request(LOGICAL, true), usize::MAX);
    client
        .write_request(&request)
        .await
        .expect("the streaming request is written");
    let head = client.read_head(READ_BUDGET).await;
    assert_eq!(head.status(), 200);
    assert_eq!(head.header("content-type"), Some("text/event-stream"));
    let chunks = client.read_chunks(&head, READ_BUDGET).await;
    let mut body = Vec::new();
    for (_at, bytes) in &chunks {
        body.extend_from_slice(bytes);
    }
    assert!(
        body.ends_with(b"data: [DONE]\n\n"),
        "the client protocol's terminator is written, got {:?}",
        String::from_utf8_lossy(&body)
    );
    let events = client_events(&body);
    assert_eq!(
        events[0]["id"], "resp-gem",
        "the chunks name the response the upstream announced"
    );
    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|event| event["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert!(
        deltas.contains(&"Hello") && deltas.contains(&" world"),
        "both folded text parts reached the client as deltas, got {deltas:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event["choices"][0]["finish_reason"] == "stop"),
        "the finish reason reached the client"
    );
    assert!(
        events
            .iter()
            .any(|event| event["usage"]["completion_tokens"] == 2),
        "the provider's usage reached the client"
    );

    // --- 3. a model the route does not serve ----------------------------------------------------
    let request = Request::post("/v1/chat/completions", "llm.test")
        .header("content-type", "application/json")
        .header("x-request-id", "gem-mismatch")
        .body(chat_request("other-model", false), usize::MAX);
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        403,
        "the route's pinned target cannot answer another model, not {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(
        response.header("x-nico-error-code"),
        Some("model_not_allowed")
    );
    assert!(
        !response.text().contains("other-model"),
        "the refusal names the fact, not the caller's value"
    );
    let observed = upstream.wait_for_request(BUDGET).await;
    assert_eq!(
        observed.body_len(),
        0,
        "a request refused before encoding must not send any body byte to the provider"
    );

    // --- 4. a provider refusal ------------------------------------------------------------------
    let request = Request::post("/v1/gem-limited/chat", "llm.test")
        .header("content-type", "application/json")
        .header("x-request-id", "gem-limited")
        .body(chat_request(LOGICAL, false), usize::MAX);
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        429,
        "a provider rate limit keeps its status, not {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(
        response.header("x-nico-error-code"),
        Some("upstream_rate_limited")
    );
    let failure = json(response.body());
    assert_eq!(
        failure["error"]["type"], "rate_limit_error",
        "the refusal keeps the client protocol's envelope, not the provider's token"
    );
    assert!(
        !response.text().contains("SECRET provider detail"),
        "the provider's own message must not reach the caller"
    );

    // --- 5. the request record ------------------------------------------------------------------
    let line = log::wait_for_request_log(&captured, "gem-one", BUDGET).await;
    assert!(line.contains("llm.protocol=openai.chat"), "got: {line}");
    assert!(line.contains("llm.provider=gemini"), "got: {line}");
    assert!(
        line.contains("llm.model=\"fast -> gemini-2.0-flash\""),
        "the record names both sides of the model map, got: {line}"
    );
    assert!(
        !line.contains(KEY) && !line.contains(CLIENT_KEY),
        "got: {line}"
    );

    // --- 6. a passthrough route of this kind refuses publication, and the good one keeps serving --
    source.publish(&document(true));
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let request = Request::post("/v1/chat/completions", "llm.test")
        .header("content-type", "application/json")
        .header("x-request-id", "gem-after-refusal")
        .body(chat_request(LOGICAL, false), usize::MAX);
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        200,
        "the last good generation must keep serving, not {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(
        json(response.body())["choices"][0]["message"]["content"],
        "Hello world",
        "the route in service is still the converting one"
    );
    let line = log::wait_for_request_log(&captured, "gem-after-refusal", BUDGET).await;
    assert!(
        line.contains("llm.provider=gemini"),
        "the published plan is still the previous generation's, got: {line}"
    );

    assert_eq!(
        server.cancel_and_join().await,
        std::process::ExitCode::SUCCESS
    );
}
