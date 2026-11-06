//! An OpenAI client through a converted OpenAI route, end to end: one real server, one mock origin.
//!
//! The scenarios share a publication because they are facts about one document: a route's model map,
//! its response ceiling and its provider's endpoint are compiled once, and every answer below is what
//! the request path makes of those facts. Each step waits for the wire event it asserts — the mock's
//! observation, the client's answer or the request record — so nothing races a sleep.
//!
//! What the file proves, in order:
//!
//! 1. a `chat.completion` document is decoded, model-mapped, re-encoded and translated back with its
//!    usage and its client status;
//! 2. an event stream is translated chunk by chunk, arriving at the rate the origin wrote it;
//! 3. a provider `429` and a provider `401` become this gateway's canonical answers, never the
//!    provider's own message;
//! 4. a declared body over the route's ceiling is `413` before the origin is contacted, and a declared
//!    response over the route's ceiling is `502` before any status line is written;
//! 5. a document this route cannot serve is `422` with the client protocol's error envelope, and the
//!    provider receives no prompt byte;
//! 6. the request record carries `llm.protocol`/`llm.provider`/`llm.model`/`llm.stream` for an LLM
//!    route, none of them for an upstream route, and no prompt, credential or field value.

mod support;

#[path = "llm_convert/fixtures.rs"]
mod fixtures;

use fixtures::{
    BUDGET, CLIENT_KEY, GAP, KEY, LOGICAL, MIN_SPAN, PHYSICAL, PROMPT, base_url, chat_request,
    client_events, completion_body, json, provider_failure, stream_events,
};
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

#[tokio::test(flavor = "current_thread")]
async fn a_converted_route_translates_requests_streams_failures_and_records_only_its_facts() {
    // Installed before the server starts: the record of the first request must land in this buffer.
    let captured = log::capture();

    let directory = tempfile::TempDir::new().expect("temporary directory");
    let port = free_port();
    write_bootstrap(directory.path(), port, false, 1, 1000);
    let credential = token::secret_file(directory.path(), "openai.key", KEY);

    let chat = MockUpstream::start(Reply::Response {
        status: 200,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: completion_body(),
        chunk: usize::MAX,
    })
    .await;
    let streaming = MockUpstream::start(Reply::StreamWithHeaders {
        headers: vec![("content-type".to_owned(), "text/event-stream".to_owned())],
        events: stream_events(),
        gap: GAP,
    })
    .await;
    let rate_limited = MockUpstream::start(Reply::Response {
        status: 429,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: provider_failure(),
        chunk: usize::MAX,
    })
    .await;
    let unauthorized = MockUpstream::start(Reply::Response {
        status: 401,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: provider_failure(),
        chunk: usize::MAX,
    })
    .await;
    let declared = MockUpstream::start(Reply::Declared {
        status: 200,
        content_length: 8192,
        body: b"the ceiling is reached before this body matters".to_vec(),
        chunk: usize::MAX,
    })
    .await;
    // Started but expected to receive no request body: the refusals below happen before the provider
    // encoder runs, so its head may be written but not a prompt byte.
    let unused = MockUpstream::start(Reply::Echo).await;
    let health = MockUpstream::start(Reply::Response {
        status: 200,
        headers: Vec::new(),
        body: b"ok".to_vec(),
        chunk: usize::MAX,
    })
    .await;

    let document = Resources::new()
        // The large-request scenario sends 8 MiB; every other route keeps its own tighter bound.
        .body_limit(16 * 1024 * 1024)
        .provider_with_model_map(
            "primary",
            &base_url(&chat),
            &credential,
            &[(LOGICAL, PHYSICAL)],
        )
        .provider("streaming", &base_url(&streaming), &credential)
        .provider("rate-limited", &base_url(&rate_limited), &credential)
        .provider("unauthorized", &base_url(&unauthorized), &credential)
        .provider("declared", &base_url(&declared), &credential)
        .provider("unused", &base_url(&unused), &credential)
        .upstream("health", health.address())
        .route(Route::provider("llm-chat", "/v1/chat/", "primary"))
        .route(Route::provider("llm-sse", "/v1/sse/", "streaming"))
        .route(Route::provider(
            "llm-rate-limited",
            "/v1/rate-limited/",
            "rate-limited",
        ))
        .route(Route::provider(
            "llm-unauthorized",
            "/v1/unauthorized/",
            "unauthorized",
        ))
        .route(
            Route::provider("llm-declared", "/v1/declared/", "declared").max_response_bytes(1024),
        )
        .route(Route::provider("llm-large-body", "/v1/large-body/", "unused").max_body_bytes(128))
        .route(Route::provider(
            "llm-unsupported",
            "/v1/unsupported/",
            "unused",
        ))
        .route(Route::new("health", "/health/", "health"))
        .render();
    let source = TestSource::with_document(&document);
    let server = RunningServer::start(directory.path(), source).await;
    let address = server.address();

    // --- 1. a complete answer ------------------------------------------------------------------
    let request = Request::post("/v1/chat/completions", "llm.test")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {CLIENT_KEY}"))
        .header("x-request-id", "convert-one")
        .body(chat_request(false), usize::MAX);
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        200,
        "a converted success must be 200, not {}: {}",
        response.status(),
        response.text()
    );
    let answer = json(response.body());
    assert_eq!(answer["id"], "chatcmpl-convert");
    assert_eq!(
        answer["model"], PHYSICAL,
        "the answer names the physical model the provider served"
    );
    assert_eq!(answer["choices"][0]["message"]["content"], "hello there");
    assert_eq!(answer["usage"]["total_tokens"], 7);
    assert_eq!(response.header("x-nico-trace-id"), Some("convert-one"));
    assert_eq!(
        response.header("transfer-encoding"),
        Some("chunked"),
        "a translated answer is re-framed, and the provider's length is dropped"
    );
    assert_eq!(response.header("content-length"), None);

    let observed = chat.wait_for_request(BUDGET).await;
    assert!(
        observed
            .head()
            .starts_with(b"POST /v1/chat/completions HTTP/1.1"),
        "the provider sees the endpoint path, got {:?}",
        String::from_utf8_lossy(observed.head())
    );
    let expected_authorization = format!("Bearer {KEY}");
    assert_eq!(
        observed.header("authorization"),
        Some(expected_authorization.as_str()),
        "the route's materialized credential replaces the caller's, which must not travel"
    );
    assert_eq!(observed.header("content-type"), Some("application/json"));
    assert!(
        !String::from_utf8_lossy(&observed.body()).contains(CLIENT_KEY),
        "the caller's credential must not appear in the provider request"
    );
    let forwarded = json(&observed.body());
    assert_eq!(
        forwarded["model"], PHYSICAL,
        "the provider is asked for the mapped physical model"
    );
    assert_eq!(forwarded["messages"][0]["content"], PROMPT);
    assert_eq!(
        observed.header("transfer-encoding"),
        Some("chunked"),
        "a re-encoded body is framed by the data plane"
    );

    // --- 1b. an 8 MiB buffered body ------------------------------------------------------------
    let large_prompt = "x".repeat(8 * 1024 * 1024);
    let large = format!(
        r#"{{"model":"{LOGICAL}","messages":[{{"role":"user","content":"{large_prompt}"}}]}}"#
    )
    .into_bytes();
    let request = Request::post("/v1/chat/completions", "llm.test")
        .header("content-type", "application/json")
        .header("x-request-id", "convert-large-request")
        .body(large.clone(), 64 * 1024);
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        200,
        "an 8 MiB body must survive conversion, not {}: {}",
        response.status(),
        response.text()
    );
    let observed = chat.wait_for_request(BUDGET).await;
    assert_eq!(
        observed.body_len(),
        large.len() - LOGICAL.len() + PHYSICAL.len(),
        "the buffered body reached the provider in full; only the mapped model name differs in length"
    );
    let forwarded = json(&observed.body());
    assert_eq!(
        forwarded["messages"][0]["content"].as_str().map(str::len),
        Some(large_prompt.len()),
        "the prompt survived the decode and encode unchanged"
    );

    // --- 2. an event stream --------------------------------------------------------------------
    let mut client = Client::connect(address).await;
    let request = Request::post("/v1/sse/chat", "llm.test")
        .header("content-type", "application/json")
        .header("x-request-id", "convert-sse")
        .body(chat_request(true), 17);
    client
        .write_request(&request)
        .await
        .expect("the streaming request is written");
    let head = client.read_head(READ_BUDGET).await;
    assert_eq!(head.status(), 200, "the event stream must be served");
    assert_eq!(head.header("content-type"), Some("text/event-stream"));
    assert_eq!(head.header("transfer-encoding"), Some("chunked"));
    assert_eq!(head.header("content-length"), None);
    assert_eq!(head.header("x-nico-trace-id"), Some("convert-sse"));
    let chunks = client.read_chunks(&head, READ_BUDGET).await;
    let mut body = Vec::new();
    for (_at, bytes) in &chunks {
        body.extend_from_slice(bytes);
    }
    assert!(
        body.ends_with(b"data: [DONE]\n\n"),
        "the terminator is written when the body ends, got {:?}",
        String::from_utf8_lossy(&body)
    );
    let events = client_events(&body);
    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|event| event["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert!(
        deltas.contains(&"Hello"),
        "the first delta reached the client"
    );
    assert!(
        deltas.contains(&" world"),
        "the second delta reached the client"
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
            .any(|event| event["choices"][0]["delta"]["role"] == "assistant"
                && event["id"] == "chatcmpl-sse"),
        "the start chunk names the response the origin announced"
    );
    assert!(
        events
            .iter()
            .any(|event| event["usage"]["completion_tokens"] == 2),
        "the provider's usage reached the client"
    );
    assert!(
        events.iter().all(|event| event["model"] == PHYSICAL),
        "every chunk repeats the response model"
    );
    let first_at = chunks
        .first()
        .map(|(at, _bytes)| *at)
        .expect("the stream had at least one chunk");
    let last_at = chunks
        .last()
        .map(|(at, _bytes)| *at)
        .expect("the stream had at least one chunk");
    let span = last_at.saturating_duration_since(first_at);
    assert!(
        span >= MIN_SPAN,
        "the stream arrived within {span:?}, so it was buffered instead of translated as it was written"
    );

    // --- 3. a provider refusal -----------------------------------------------------------------
    let request = Request::post("/v1/rate-limited/chat", "llm.test")
        .header("content-type", "application/json")
        .header("x-request-id", "convert-rate-limited")
        .body(chat_request(false), usize::MAX);
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        429,
        "a provider rate limit keeps its status for a client's own retry policy, not {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(
        response.header("x-nico-error-code"),
        Some("upstream_rate_limited")
    );
    assert_eq!(
        json(response.body())["error"]["code"],
        "upstream_rate_limited",
        "the client receives this gateway's envelope, not the provider's"
    );
    assert!(
        !response.text().contains("SECRET provider detail"),
        "the provider's own message must not reach the caller"
    );
    assert_eq!(
        rate_limited.observation().connections,
        1,
        "one attempt: this build never retries"
    );

    let request = Request::post("/v1/unauthorized/chat", "llm.test")
        .header("content-type", "application/json")
        .header("x-request-id", "convert-unauthorized")
        .body(chat_request(false), usize::MAX);
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        502,
        "a provider credential failure is this gateway's problem, not the caller's, not {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(
        response.header("x-nico-error-code"),
        Some("upstream_auth_failed")
    );
    assert!(
        !response.text().contains("SECRET provider detail"),
        "the provider's own message must not reach the caller"
    );
    assert_eq!(unauthorized.observation().connections, 1);

    // --- 4. the two ceilings -------------------------------------------------------------------
    let request = Request::post("/v1/large-body/chat", "llm.test")
        .header("content-type", "application/json")
        .header("x-request-id", "convert-large-body")
        .body(vec![b'x'; 2048], 1024);
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        413,
        "a declared body over the route bound must be 413, not {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(response.header("x-nico-error-code"), Some("body_too_large"));
    assert!(
        unused.observation().requests.is_empty(),
        "the length was declared in the head, so no provider request may be written"
    );

    let request = Request::post("/v1/declared/chat", "llm.test")
        .header("content-type", "application/json")
        .header("x-request-id", "convert-declared")
        .body(chat_request(false), usize::MAX);
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        502,
        "a declared response over the route bound is answerable before the head is written, not {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(
        response.header("x-nico-error-code"),
        Some("upstream_response_too_large")
    );
    assert_eq!(
        response.header("x-mock-upstream"),
        None,
        "the provider's head was never written: the answer is the gateway's"
    );
    assert_eq!(declared.observation().connections, 1);

    // --- 5. a request the route cannot serve ---------------------------------------------------
    let unsupported = format!(
        r#"{{"model":"{LOGICAL}","messages":[{{"role":"user","content":"{PROMPT}"}}],"n":2}}"#
    );
    let request = Request::post("/v1/unsupported/chat", "llm.test")
        .header("content-type", "application/json")
        .header("x-request-id", "convert-unsupported")
        .body(unsupported.into_bytes(), usize::MAX);
    let response = exchange(address, &request).await;
    assert_eq!(
        response.status(),
        422,
        "a well-formed request this route cannot carry is a typed refusal, not {}: {}",
        response.status(),
        response.text()
    );
    assert_eq!(
        response.header("x-nico-error-code"),
        Some("unsupported_capability")
    );
    assert_eq!(
        json(response.body())["error"]["code"],
        "unsupported_capability",
        "the refusal keeps the client protocol's envelope"
    );
    assert!(
        !response.text().contains(PROMPT),
        "the refusal must not quote the prompt it refused"
    );
    // The upstream head is written before the client body is decoded (an AI route's endpoint is a
    // publication fact), so a refusal at end of body may already have opened the connection and
    // written the head. What must not travel is a byte of the prompt: the refusal happens before
    // `Provider::encode`, and the body stays empty.
    let observed = unused.wait_for_request(BUDGET).await;
    assert_eq!(
        observed.body_len(),
        0,
        "a request refused before encoding must not send any body byte to the provider"
    );

    // --- 6. the request record -----------------------------------------------------------------
    let line = log::wait_for_request_log(&captured, "convert-one", BUDGET).await;
    assert!(line.contains("llm.protocol=openai.chat"), "got: {line}");
    assert!(line.contains("llm.provider=openai"), "got: {line}");
    assert!(
        line.contains("llm.model=\"fast -> gpt-4o-mini\""),
        "the record names both sides of the model map, got: {line}"
    );
    assert!(line.contains("llm.stream=false"), "got: {line}");
    assert!(line.contains("http.status=200"), "got: {line}");
    assert!(
        !line.contains(PROMPT) && !line.contains(KEY) && !line.contains(CLIENT_KEY),
        "the record must not carry the prompt or a credential, got: {line}"
    );
    assert!(!line.contains("authorization"), "got: {line}");

    let line = log::wait_for_request_log(&captured, "convert-sse", BUDGET).await;
    assert!(line.contains("llm.stream=true"), "got: {line}");

    // A request refused before its body was decoded knows the two publication facts and no more.
    let line = log::wait_for_request_log(&captured, "convert-large-body", BUDGET).await;
    assert!(line.contains("llm.protocol=openai.chat"), "got: {line}");
    assert!(line.contains("llm.provider=openai"), "got: {line}");
    assert!(
        !line.contains("llm.model=") && !line.contains("llm.stream="),
        "a body that was never decoded has no model or stream flag, got: {line}"
    );

    // An upstream route is not an LLM route, so none of the fields exists.
    let response = exchange(
        address,
        &Request::get("/health/", "llm.test").header("x-request-id", "upstream-record"),
    )
    .await;
    assert_eq!(response.status(), 200);
    let line = log::wait_for_request_log(&captured, "upstream-record", BUDGET).await;
    assert!(
        !line.contains("llm."),
        "a request that never reached an LLM front-end must not pretend one of its fields, got: {line}"
    );

    assert_eq!(
        server.cancel_and_join().await,
        std::process::ExitCode::SUCCESS
    );
}
