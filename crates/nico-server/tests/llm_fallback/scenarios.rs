//! The scenarios of `llm_fallback.rs`: the wire facts each one pins, and the fixtures it asks with.
//!
//! Each scenario publishes its own document on the one live server and asserts on three things: the
//! answer the caller saw, the connections each origin observed, and the attempt lines the log
//! captured. The helpers at the top are the shared vocabulary — the two request shapes, the four
//! replies, and the two waits (one on the wire, one on the log).

#![expect(
    clippy::expect_used,
    reason = "a scenario that cannot read its own observation has nothing to assert on"
)]

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::support::http::{
    client::{Request, exchange},
    log,
    resources::{Resources, Route},
    source::TestSource,
    upstream::{MockUpstream, Reply},
};

/// The provider key every fixture document references.
pub(super) const KEY: &str = "sk-fallback-key";

/// The captured log lines, for the attempt and circuit records the scenarios pin.
type Captured = Arc<Mutex<Vec<u8>>>;

/// The caller's request body: a minimal Chat Completions document.
fn chat_request() -> Vec<u8> {
    br#"{"model":"fast","messages":[{"role":"user","content":"fallback-scenario"}]}"#.to_vec()
}

/// A Chat Completions answer naming `model`, which is how a scenario tells the two origins apart.
fn completion(model: &str) -> Vec<u8> {
    format!(
        r#"{{"id":"chatcmpl-{model}","object":"chat.completion","created":0,"model":"{model}","choices":[{{"index":0,"message":{{"role":"assistant","content":"from {model}"}},"finish_reason":"stop"}}]}}"#
    )
    .into_bytes()
}

/// The provider's own failure document; its message must never reach the caller.
fn provider_failure() -> Vec<u8> {
    br#"{"error":{"message":"provider-internal-detail","type":"server_error"}}"#.to_vec()
}

/// Returns the loopback base URL one origin names.
pub(super) fn base_url(upstream: &MockUpstream) -> String {
    format!("http://{}", upstream.address())
}

/// A JSON reply of `status` carrying `body` in one write.
fn reply(status: u16, body: Vec<u8>) -> Reply {
    Reply::Response {
        status,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body,
        chunk: usize::MAX,
    }
}

/// A failure reply of `status` carrying the provider's own error document.
pub(super) fn failure(status: u16) -> Reply {
    reply(status, provider_failure())
}

/// A success reply carrying `model`'s completion.
pub(super) fn success(model: &str) -> Reply {
    reply(200, completion(model))
}

/// A streamed reply: two SSE deltas naming `model`, the shape a streaming provider speaks.
fn streamed_success(model: &str) -> Reply {
    let chunk = |delta: &str, finish: &str| {
        format!(
            r#"{{"id":"chatcmpl-{model}","object":"chat.completion.chunk","created":0,"model":"{model}","choices":[{{"index":0,"delta":{{"content":"{delta}"}},"finish_reason":{finish}}}]}}"#
        )
    };
    Reply::StreamWithHeaders {
        headers: vec![("content-type".to_owned(), "text/event-stream".to_owned())],
        events: vec![
            format!("data: {}\n\n", chunk("from ", "null")).into_bytes(),
            format!("data: {}\n\ndata: [DONE]\n\n", chunk(model, "\"stop\"")).into_bytes(),
        ],
        gap: Duration::from_millis(5),
    }
}

/// The fixture request: a complete-answer POST to `/v1/chat/completions`.
fn chat() -> Request {
    Request::post("/v1/chat/completions", "llm.test")
        .header("content-type", "application/json")
        .body(chat_request(), usize::MAX)
}

/// The streamed fixture request: the same conversation, asked as an event stream.
fn chat_stream() -> Request {
    const BODY: &[u8] =
        br#"{"model":"fast","messages":[{"role":"user","content":"stream me"}],"stream":true}"#;
    Request::post("/v1/chat/completions", "llm.test")
        .header("content-type", "application/json")
        .body(BODY.to_vec(), usize::MAX)
}

/// Sends the fixture request to `/v1/chat/completions` on `address`.
async fn ask(address: SocketAddr) -> crate::support::http::client::Response {
    exchange(address, &chat()).await
}

/// Asks until the answer is `status` and `observed` has seen `connections` connections.
///
/// Publishing is asynchronous and the previous generation keeps serving until the new one lands;
/// an ask the old generation served cannot touch a socket the old document never named, so the
/// mock's observation is what proves the new generation answered.
async fn settled(
    address: SocketAddr,
    request: &Request,
    observed: &MockUpstream,
    connections: usize,
    status: u16,
) -> crate::support::http::client::Response {
    let deadline = std::time::Instant::now() + crate::support::http::STARTUP_BUDGET;
    loop {
        let response = exchange(address, request).await;
        let seen = observed.observation().connections;
        if response.status() == status && seen >= connections {
            return response;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the new generation never answered {status} with {connections} connections: {} {:?}",
            response.status(),
            observed.observation().summary()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Waits for one captured log line carrying `marker`, and returns it.
async fn wait_line(captured: &Captured, marker: &str) -> String {
    log::wait_for_line_matching(captured, crate::support::http::STARTUP_BUDGET, |line| {
        line.contains(marker)
    })
    .await
}

/// The budget is spent on the primary before the chain moves, and the caller sees one answer.
pub(super) async fn a_retryable_failure_on_the_primary_is_answered_by_the_fallback(
    captured: &Captured,
    address: &SocketAddr,
    primary: &MockUpstream,
    secondary: &MockUpstream,
) {
    let response = settled(*address, &chat(), primary, 2, 200).await;
    assert_eq!(
        response.status(),
        200,
        "the fallback answered: {}",
        response.text()
    );
    assert!(
        response.text().contains("from fallback-model"),
        "the answer came from the fallback origin: {}",
        response.text()
    );
    assert_eq!(
        primary.observation().connections,
        2,
        "a budget of two is two attempts"
    );
    assert_eq!(
        secondary.observation().connections,
        1,
        "the fallback was asked once"
    );
    let observed = secondary.observation();
    let last = observed.last().expect("the fallback observed a request");
    assert!(
        last.head()
            .starts_with(b"POST /v1/chat/completions HTTP/1.1"),
        "the fallback sees the endpoint's own path, got {:?}",
        String::from_utf8_lossy(last.head())
    );
    assert!(
        !last.body().is_empty(),
        "the fallback received the body the first target never served"
    );
    assert!(
        !String::from_utf8_lossy(response.body()).contains("provider-internal-detail"),
        "the provider's own failure message never reaches the caller"
    );

    // Each replaced attempt is one line with its target and its result, and the request record
    // carries neither the prompt nor the credential.
    let line = wait_line(captured, r#"llm.outcome="failed""#).await;
    assert!(
        line.contains("llm.target=primary"),
        "the failed attempt names its target"
    );
    let record = log::text(captured);
    let ours: Vec<&str> = record
        .lines()
        .filter(|l| l.contains("http.status="))
        .collect();
    assert!(
        ours.iter()
            .all(|l| !l.contains(KEY) && !l.contains("fallback-scenario")),
        "the request record carries neither the credential nor the prompt"
    );
}

/// Threshold 1: the first request's failure opens the breaker; the second request skips the target.
pub(super) async fn an_open_breaker_is_not_contacted_at_all(
    captured: &Captured,
    source: &Arc<TestSource>,
    address: &SocketAddr,
    credential: &str,
) {
    let primary = MockUpstream::start(failure(502)).await;
    let secondary = MockUpstream::start(success("fallback-model")).await;
    source.publish(
        &Resources::new()
            .provider_with_breaker("primary", &base_url(&primary), credential, 1, 30_000)
            .provider_with_breaker("secondary", &base_url(&secondary), credential, 0, 30_000)
            .route(
                Route::provider("llm-chat", "/v1/chat/", "primary")
                    .retry_budget(1)
                    .fallback(&["secondary"]),
            )
            .render(),
    );

    let first = settled(*address, &chat(), &primary, 1, 200).await;
    assert_eq!(first.status(), 200, "the fallback served the first request");
    let second = ask(*address).await;
    assert_eq!(
        second.status(),
        200,
        "the fallback serves the request the breaker skipped"
    );
    // The primary is never contacted again: the count it observed is the same one it had before.
    assert_eq!(
        primary.observation().connections,
        1,
        "an open breaker makes no connection"
    );
    assert_eq!(
        secondary.observation().connections,
        2,
        "the fallback answered both"
    );

    // The opened breaker is one warn line, and the request that skipped the target says so.
    let opened = wait_line(captured, r#"llm.circuit="open""#).await;
    assert!(
        opened.contains("llm.target=primary"),
        "the circuit names its target"
    );
    let skipped = wait_line(captured, r#"llm.outcome="skipped""#).await;
    assert!(
        skipped.contains("llm.target=primary"),
        "the skip names its target"
    );
}

/// 401 is not retryable: the caller is refused with the canonical answer, no budget, no chain.
pub(super) async fn a_final_failure_is_answered_without_asking_the_fallback(
    source: &Arc<TestSource>,
    address: &SocketAddr,
    credential: &str,
) {
    let primary = MockUpstream::start(failure(401)).await;
    let secondary = MockUpstream::start(success("fallback-model")).await;
    source.publish(
        &Resources::new()
            .provider_with_breaker("primary", &base_url(&primary), credential, 0, 30_000)
            .provider_with_breaker("secondary", &base_url(&secondary), credential, 0, 30_000)
            .route(
                Route::provider("llm-chat", "/v1/chat/", "primary")
                    .retry_budget(3)
                    .fallback(&["secondary"]),
            )
            .render(),
    );

    let response = settled(*address, &chat(), &primary, 1, 502).await;
    assert_eq!(
        response.status(),
        502,
        "a credential failure answers 502: {}",
        response.text()
    );
    assert!(
        !response.text().contains("provider-internal-detail"),
        "the provider's message never reaches the caller: {}",
        response.text()
    );
    assert_eq!(
        primary.observation().connections,
        1,
        "a final failure burns no budget"
    );
    assert_eq!(
        secondary.observation().connections,
        0,
        "the fallback is never asked"
    );
}

/// A committed response is never replaced: the stream ends, and the fallback is never asked.
pub(super) async fn a_failure_after_the_head_was_written_does_not_switch_targets(
    source: &Arc<TestSource>,
    address: &SocketAddr,
    credential: &str,
) {
    let primary = MockUpstream::start(Reply::StreamThenDie {
        headers: vec![("content-type".to_owned(), "text/event-stream".to_owned())],
        first_chunk: br#"{"choices":[{"index":0,"delta":{"content":"partial"}}]}"#.to_vec(),
    })
    .await;
    let secondary = MockUpstream::start(success("fallback-model")).await;
    source.publish(
        &Resources::new()
            .provider_with_breaker("primary", &base_url(&primary), credential, 0, 30_000)
            .provider_with_breaker("secondary", &base_url(&secondary), credential, 0, 30_000)
            .route(
                Route::provider("llm-chat", "/v1/chat/", "primary")
                    .retry_budget(2)
                    .fallback(&["secondary"]),
            )
            .render(),
    );

    // The head was committed before the origin died, so the caller sees a 200 whose stream ended.
    let response = settled(*address, &chat_stream(), &primary, 1, 200).await;
    assert_eq!(
        response.status(),
        200,
        "the head is written before the body fails"
    );
    assert_eq!(
        secondary.observation().connections,
        0,
        "a committed response stands alone"
    );
}

/// 65 KiB is above Pingora's 64 KiB replay buffer: the failure is answered, not retried.
pub(super) async fn a_body_above_the_replay_cap_is_not_retried_after_dispatch(
    source: &Arc<TestSource>,
    address: &SocketAddr,
    credential: &str,
) {
    let primary = MockUpstream::start(failure(502)).await;
    let secondary = MockUpstream::start(success("fallback-model")).await;
    source.publish(
        &Resources::new()
            .body_limit(1024 * 1024)
            .provider_with_breaker("primary", &base_url(&primary), credential, 0, 30_000)
            .provider_with_breaker("secondary", &base_url(&secondary), credential, 0, 30_000)
            .route(
                Route::provider("llm-chat", "/v1/chat/", "primary")
                    .retry_budget(2)
                    .fallback(&["secondary"]),
            )
            .render(),
    );

    // A prompt just over the cap; the document stays valid JSON and within the route's body bound.
    let filler = "x".repeat(65 * 1024);
    let body = format!(r#"{{"model":"fast","messages":[{{"role":"user","content":"{filler}"}}]}}"#)
        .into_bytes();
    // Chunked, so the client keeps writing while the gateway may already be answering.
    let request = Request::post("/v1/chat/completions", "llm.test")
        .header("content-type", "application/json")
        .chunked(body, 8 * 1024);
    crate::support::http::wait_for_status(*address, &request, 502).await;
    assert_eq!(
        secondary.observation().connections,
        0,
        "no body-less retry is sent"
    );
}

/// A name that does not resolve is the target's own transport failure: the chain walks past it.
///
/// `256.256.256.256` is a literal no resolver can answer: name resolution fails at once, with no
/// network query, so the scenario pins the walk-past behavior deterministically.
pub(super) async fn a_target_that_does_not_resolve_is_walked_past(
    source: &Arc<TestSource>,
    address: &SocketAddr,
    credential: &str,
) {
    let secondary = MockUpstream::start(success("fallback-model")).await;
    source.publish(
        &Resources::new()
            .provider_with_breaker(
                "primary",
                "http://256.256.256.256:8080",
                credential,
                0,
                30_000,
            )
            .provider_with_breaker("secondary", &base_url(&secondary), credential, 0, 30_000)
            .route(
                Route::provider("llm-chat", "/v1/chat/", "primary")
                    .retry_budget(1)
                    .fallback(&["secondary"]),
            )
            .render(),
    );

    let response = settled(*address, &chat(), &secondary, 1, 200).await;
    assert_eq!(
        response.status(),
        200,
        "the unresolvable primary was skipped: {}",
        response.text()
    );
    assert!(
        response.text().contains("from fallback-model"),
        "the answer came from the fallback origin: {}",
        response.text()
    );
}

/// A fallback answers a streamed success too: the chain moves before the head, and the stream the
/// caller reads is the fallback's.
pub(super) async fn a_streamed_fallback_answers_the_stream(
    source: &Arc<TestSource>,
    address: &SocketAddr,
    credential: &str,
) {
    let primary = MockUpstream::start(failure(502)).await;
    let secondary = MockUpstream::start(streamed_success("fallback-model")).await;
    source.publish(
        &Resources::new()
            .provider_with_breaker("primary", &base_url(&primary), credential, 0, 30_000)
            .provider_with_breaker("secondary", &base_url(&secondary), credential, 0, 30_000)
            .route(
                Route::provider("llm-chat", "/v1/chat/", "primary")
                    .retry_budget(2)
                    .fallback(&["secondary"]),
            )
            .render(),
    );

    let response = settled(*address, &chat_stream(), &primary, 2, 200).await;
    assert_eq!(
        response.status(),
        200,
        "the fallback's stream is the answer: {}",
        response.text()
    );
    let body = response.text();
    assert!(
        body.contains(r#""content":"from ""#) && body.contains(r#""content":"fallback-model""#),
        "the streamed deltas arrived: {body}"
    );
    assert_eq!(
        primary.observation().connections,
        2,
        "the primary spent its budget"
    );
    assert_eq!(
        secondary.observation().connections,
        1,
        "the streamed fallback ran once"
    );
}
