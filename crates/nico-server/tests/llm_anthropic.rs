//! An Anthropic Messages client through a converted OpenAI route, end to end.
//!
//! The cross-dialect row the matrix pairs in `nico-llm`, exercised on a real server: the client
//! speaks `anthropic.messages`, the upstream speaks Chat Completions, and neither sees the other's
//! spelling. The scenarios are the ones a translation lives or dies by:
//!
//! 1. a complete answer is translated into one Messages document with its usage and its stop reason;
//! 2. an event stream is translated into Messages events, terminated by `message_stop`;
//! 3. a provider refusal reaches the client in the Anthropic error envelope, never the provider's
//!    own words;
//! 4. the caller's `x-api-key` does not travel; the route's materialized credential does.

#![expect(
    clippy::expect_used,
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
const KEY: &str = "sk-fixture-openai-key";

/// The caller's own credential, sent the way an Anthropic client sends one.
const CLIENT_KEY: &str = "sk-ant-client-key-must-not-travel";

/// The logical model the caller writes, and the physical one the map resolves it to.
const LOGICAL: &str = "fast";
const PHYSICAL: &str = "gpt-physical";

/// Builds the Messages request the client sends.
fn messages_request(stream: bool) -> Vec<u8> {
    format!(
        r#"{{"model":"{LOGICAL}","max_tokens":64,"messages":[{{"role":"user","content":"hello there"}}],"stream":{stream}}}"#
    )
    .into_bytes()
}

/// Builds the complete answer the OpenAI upstream returns.
fn completion_body() -> Vec<u8> {
    format!(
        r#"{{"id":"chatcmpl-ant","object":"chat.completion","created":0,"model":"{PHYSICAL}","choices":[{{"index":0,"message":{{"role":"assistant","content":"the answer"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}}}"#
    )
    .into_bytes()
}

/// Frames one provider stream document as a server-sent event.
fn sse(payload: &str) -> Vec<u8> {
    format!("data: {payload}\n\n").into_bytes()
}

/// The chunk sequence the OpenAI upstream writes for one streamed answer.
fn stream_events() -> Vec<Vec<u8>> {
    let start = format!(
        r#"{{"id":"chatcmpl-ant-sse","object":"chat.completion.chunk","created":0,"model":"{PHYSICAL}","choices":[{{"index":0,"delta":{{"role":"assistant","content":""}},"finish_reason":null}}]}}"#
    );
    let hello = format!(
        r#"{{"id":"chatcmpl-ant-sse","object":"chat.completion.chunk","created":0,"model":"{PHYSICAL}","choices":[{{"index":0,"delta":{{"content":"Hello"}},"finish_reason":null}}]}}"#
    );
    let finish = format!(
        r#"{{"id":"chatcmpl-ant-sse","object":"chat.completion.chunk","created":0,"model":"{PHYSICAL}","choices":[{{"index":0,"delta":{{}},"finish_reason":"stop"}}]}}"#
    );
    let usage = format!(
        r#"{{"id":"chatcmpl-ant-sse","object":"chat.completion.chunk","created":0,"model":"{PHYSICAL}","choices":[],"usage":{{"prompt_tokens":3,"completion_tokens":2}}}}"#
    );
    vec![
        sse(&start),
        sse(&hello),
        sse(&finish),
        sse(&usage),
        sse("[DONE]"),
    ]
}

/// The origin's failure document, whose message must never reach the caller.
fn provider_failure() -> Vec<u8> {
    br#"{"error":{"message":"SECRET provider detail","type":"rate_limit_error","code":"rate_limit_exceeded"}}"#
        .to_vec()
}

/// Parses a JSON document from a test-owned byte string.
fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).expect("the fixture is a JSON document")
}

/// Returns the `data:` payloads of a translated event stream, keeping the event names.
///
/// An Anthropic stream names its events on an `event:` line, so the pair (name, payload) is what the
/// assertions read.
fn client_events(body: &[u8]) -> Vec<(String, serde_json::Value)> {
    let text = std::str::from_utf8(body).expect("the translated stream is UTF-8");
    text.split("\n\n")
        .filter(|frame| !frame.is_empty())
        .map(|frame| {
            let mut name = String::new();
            let mut data = "";
            for line in frame.lines() {
                if let Some(event) = line.strip_prefix("event: ") {
                    name = event.to_owned();
                }
                if let Some(payload) = line.strip_prefix("data: ") {
                    data = payload;
                }
            }
            (name, json(data.as_bytes()))
        })
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn an_anthropic_client_is_translated_to_an_openai_upstream_and_back() {
    // Installed before the server starts: the record of the first request must land in this buffer.
    let captured = log::capture();

    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    write_bootstrap(directory.path(), port, false, 1, 1000);
    let credential = token::secret_file(directory.path(), "openai.key", KEY);

    let upstream = MockUpstream::start(Reply::Response {
        status: 200,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: completion_body(),
        chunk: usize::MAX,
    })
    .await;
    let streaming = MockUpstream::start(Reply::StreamWithHeaders {
        headers: vec![("content-type".to_owned(), "text/event-stream".to_owned())],
        events: stream_events(),
        gap: std::time::Duration::ZERO,
    })
    .await;
    let rate_limited = MockUpstream::start(Reply::Response {
        status: 429,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: provider_failure(),
        chunk: usize::MAX,
    })
    .await;

    let document = Resources::new()
        .provider_with_model_map(
            "primary",
            &format!("http://{}", upstream.address()),
            &credential,
            &[(LOGICAL, PHYSICAL)],
        )
        .provider_with_model_map(
            "streaming",
            &format!("http://{}", streaming.address()),
            &credential,
            &[(LOGICAL, PHYSICAL)],
        )
        .provider_with_model_map(
            "limited",
            &format!("http://{}", rate_limited.address()),
            &credential,
            &[(LOGICAL, PHYSICAL)],
        )
        .route(
            Route::provider("llm-anthropic", "/v1/messages", "primary")
                .protocol("anthropic.messages"),
        )
        .route(
            Route::provider("llm-anthropic-sse", "/v1/ant-sse/", "streaming")
                .protocol("anthropic.messages"),
        )
        .route(
            Route::provider("llm-anthropic-limited", "/v1/ant-limited/", "limited")
                .protocol("anthropic.messages"),
        )
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;
    let address = server.address();

    // --- 1. a complete answer ------------------------------------------------------------------
    let request = Request::post("/v1/messages", "llm.test")
        .header("content-type", "application/json")
        .header("x-api-key", CLIENT_KEY)
        .header("anthropic-version", "2023-06-01")
        .header("x-request-id", "ant-one")
        .body(messages_request(false), usize::MAX);
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        200,
        "a converted success must be 200, not {}: {}",
        response.status(),
        response.text()
    );
    let answer = json(response.body());
    assert_eq!(
        answer["type"], "message",
        "the client protocol's own document: {answer}"
    );
    assert_eq!(answer["id"], "chatcmpl-ant");
    assert_eq!(answer["model"], PHYSICAL);
    assert_eq!(answer["content"][0]["type"], "text");
    assert_eq!(answer["content"][0]["text"], "the answer");
    assert_eq!(answer["stop_reason"], "end_turn");
    assert_eq!(answer["usage"]["input_tokens"], 5);
    assert_eq!(answer["usage"]["output_tokens"], 2);

    let observed = upstream.wait_for_request(BUDGET).await;
    assert!(
        observed
            .head()
            .starts_with(b"POST /v1/chat/completions HTTP/1.1"),
        "the provider sees the OpenAI endpoint, got {:?}",
        String::from_utf8_lossy(observed.head())
    );
    assert_eq!(
        observed.header("authorization"),
        Some(format!("Bearer {KEY}").as_str()),
        "the route's materialized credential replaces the caller's"
    );
    assert_eq!(
        observed.header("x-api-key"),
        None,
        "the Anthropic credential is not forwarded"
    );
    assert!(
        !String::from_utf8_lossy(&observed.body()).contains(CLIENT_KEY),
        "the caller's credential must not appear in the provider request"
    );
    let forwarded = json(&observed.body());
    assert_eq!(
        forwarded["model"], PHYSICAL,
        "the provider is asked for the mapped physical model"
    );
    assert_eq!(
        forwarded["max_tokens"], 64,
        "the Messages ceiling becomes the OpenAI one"
    );
    assert_eq!(forwarded["messages"][0]["content"], "hello there");

    // --- 2. an event stream --------------------------------------------------------------------
    let mut client = Client::connect(address).await;
    let request = Request::post("/v1/ant-sse/v1/messages", "llm.test")
        .header("content-type", "application/json")
        .header("x-request-id", "ant-sse")
        .body(messages_request(true), usize::MAX);
    client
        .write_request(&request)
        .await
        .expect("the streaming request is written");
    let head = client.read_head(READ_BUDGET).await;
    assert_eq!(head.status(), 200, "the event stream must be served");
    assert_eq!(head.header("content-type"), Some("text/event-stream"));
    let chunks = client.read_chunks(&head, READ_BUDGET).await;
    let mut body = Vec::new();
    for (_at, bytes) in &chunks {
        body.extend_from_slice(bytes);
    }
    let events = client_events(&body);
    assert_eq!(
        events[0].0, "message_start",
        "the stream opens the way Messages does"
    );
    assert_eq!(events[0].1["message"]["model"], PHYSICAL);
    assert!(
        events
            .iter()
            .any(|(name, event)| name == "content_block_delta"
                && event["delta"]["type"] == "text_delta"
                && event["delta"]["text"] == "Hello"),
        "the provider's delta reached the client as a text delta"
    );
    assert!(
        events
            .iter()
            .any(|(name, event)| name == "message_delta"
                && event["delta"]["stop_reason"] == "end_turn"),
        "the finish reason reached the client in Messages' vocabulary"
    );
    assert!(
        events.iter().any(|(name, _)| name == "message_stop"),
        "the terminator is written when the body ends"
    );
    assert!(
        events.iter().all(|(name, _)| name != "error"),
        "a translated success stream carries no error event"
    );

    // --- 3. a provider refusal -----------------------------------------------------------------
    let request = Request::post("/v1/ant-limited/v1/messages", "llm.test")
        .header("content-type", "application/json")
        .header("x-api-key", CLIENT_KEY)
        .header("x-request-id", "ant-limited")
        .body(messages_request(false), usize::MAX);
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        429,
        "a provider rate limit keeps its status for a client's own retry policy, not {}: {}",
        response.status(),
        response.text()
    );
    let failure = json(response.body());
    assert_eq!(
        failure["type"], "error",
        "the refusal keeps the client protocol's envelope"
    );
    assert_eq!(failure["error"]["type"], "rate_limit_error");
    assert!(
        !response.text().contains("SECRET provider detail"),
        "the provider's own message must not reach the caller"
    );

    // --- 4. the request record -----------------------------------------------------------------
    let line = log::wait_for_request_log(&captured, "ant-one", BUDGET).await;
    assert!(
        line.contains("llm.protocol=anthropic.messages"),
        "the route serves the protocol the document named, got: {line}"
    );
    assert!(line.contains("llm.provider=openai"), "got: {line}");
    assert!(
        line.contains("llm.model=\"fast -> gpt-physical\""),
        "the record names both sides of the model map, got: {line}"
    );
    assert!(
        !line.contains(PHYSICAL) || line.contains("llm.model"),
        "the physical model appears only as the model map's other half, got: {line}"
    );
    assert!(
        !line.contains(CLIENT_KEY) && !line.contains(KEY),
        "the record must not carry a credential, got: {line}"
    );
    assert!(!line.contains("x-api-key"), "got: {line}");

    assert_eq!(
        server.cancel_and_join().await,
        std::process::ExitCode::SUCCESS
    );
}
