//! The fixtures the converted-route scenarios send and assert on.
//!
//! Keeping them out of the scenario body makes the document and the assertions readable: the origin's
//! answer, the stream's chunks and the model pair are one decision each, and a change to one of them
//! should not require reading the request flow to find it.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a fixture that cannot read the bytes it just built has nothing useful to return"
)]

use std::time::Duration;

use crate::support::http::upstream::MockUpstream;

/// The provider key the document references; a fixture file is read byte for byte.
pub(super) const KEY: &str = "sk-fixture-openai-key";

/// The caller's own credential. A converted route must never send it to the provider.
pub(super) const CLIENT_KEY: &str = "sk-client-key-must-not-travel";

/// The prompt every scenario sends; the request record must never contain it.
pub(super) const PROMPT: &str = "convert-scenario-prompt";

/// The logical model the caller writes, and the physical one the map resolves it to.
pub(super) const LOGICAL: &str = "fast";
pub(super) const PHYSICAL: &str = "gpt-4o-mini";

/// The origin's writing interval, and the smallest span that proves the stream was not collected.
pub(super) const GAP: Duration = Duration::from_millis(200);
pub(super) const MIN_SPAN: Duration = Duration::from_millis(600);

/// How long a fixture may take to reach a state it was told to reach.
pub(super) const BUDGET: Duration = Duration::from_secs(20);

/// Returns the loopback base URL a provider entry names.
pub(super) fn base_url(upstream: &MockUpstream) -> String {
    format!("http://{}", upstream.address())
}

/// Builds the client's Chat Completions request.
pub(super) fn chat_request(stream: bool) -> Vec<u8> {
    format!(
        r#"{{"model":"{LOGICAL}","messages":[{{"role":"user","content":"{PROMPT}"}}],"stream":{stream}}}"#
    )
    .into_bytes()
}

/// Builds the complete answer the origin returns for a non-streamed request.
pub(super) fn completion_body() -> Vec<u8> {
    format!(
        r#"{{"id":"chatcmpl-convert","object":"chat.completion","created":0,"model":"{PHYSICAL}","choices":[{{"index":0,"message":{{"role":"assistant","content":"hello there"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}}}"#
    )
    .into_bytes()
}

/// Frames one provider stream document as a server-sent event.
pub(super) fn sse(payload: &str) -> Vec<u8> {
    format!("data: {payload}\n\n").into_bytes()
}

/// The chunk sequence the origin writes: identity, two text deltas, usage, the finish, the sentinel.
pub(super) fn stream_events() -> Vec<Vec<u8>> {
    let start = format!(
        r#"{{"id":"chatcmpl-sse","object":"chat.completion.chunk","created":0,"model":"{PHYSICAL}","choices":[{{"index":0,"delta":{{"role":"assistant","content":""}},"finish_reason":null}}]}}"#
    );
    let hello = format!(
        r#"{{"id":"chatcmpl-sse","object":"chat.completion.chunk","created":0,"model":"{PHYSICAL}","choices":[{{"index":0,"delta":{{"content":"Hello"}},"finish_reason":null}}]}}"#
    );
    let world = format!(
        r#"{{"id":"chatcmpl-sse","object":"chat.completion.chunk","created":0,"model":"{PHYSICAL}","choices":[{{"index":0,"delta":{{"content":" world"}},"finish_reason":null}}]}}"#
    );
    let usage = format!(
        r#"{{"id":"chatcmpl-sse","object":"chat.completion.chunk","created":0,"model":"{PHYSICAL}","choices":[],"usage":{{"prompt_tokens":3,"completion_tokens":2}}}}"#
    );
    let finish = format!(
        r#"{{"id":"chatcmpl-sse","object":"chat.completion.chunk","created":0,"model":"{PHYSICAL}","choices":[{{"index":0,"delta":{{}},"finish_reason":"stop"}}]}}"#
    );
    vec![
        sse(&start),
        sse(&hello),
        sse(&world),
        sse(&usage),
        sse(&finish),
        sse("[DONE]"),
    ]
}

/// The origin's failure document, whose message must never reach the caller.
pub(super) fn provider_failure() -> Vec<u8> {
    br#"{"error":{"message":"SECRET provider detail","type":"rate_limit_error","code":"rate_limit_exceeded"}}"#
        .to_vec()
}

/// Parses a JSON document from a test-owned byte string.
pub(super) fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).expect("the fixture is a JSON document")
}

/// Returns the data frames of a translated event stream, skipping the terminator.
pub(super) fn client_events(body: &[u8]) -> Vec<serde_json::Value> {
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
