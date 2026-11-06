//! Shared fixtures for the Chat Completions codec tests.

#![expect(
    dead_code,
    reason = "each test file uses the subset of these helpers its scenarios need"
)]

use nico_llm::{
    ClientCodec, CodecError, RequestFacts,
    protocol::openai_chat::OpenAiChatCodec,
    schema::{
        AssistantTurn, ChatRequest, ChatResponse, Choice, ContentPart, FinishReason, Item, Message,
        ModelId, ResponseId, StreamEvent,
    },
};
use serde_json::Value;

/// The codec under test.
pub(crate) fn codec() -> OpenAiChatCodec {
    OpenAiChatCodec::new()
}

/// The facts of a chat request; the codec reads none of them, but the exchange always passes them.
pub(crate) fn facts() -> RequestFacts {
    RequestFacts::new(
        "POST",
        "/v1/chat/completions",
        vec![("content-type".to_owned(), "application/json".to_owned())],
    )
}

/// Decodes a request body that must be accepted.
pub(crate) fn decode(body: &str) -> ChatRequest {
    decode_bytes(body.as_bytes())
}

/// Decodes a request body that must be accepted.
pub(crate) fn decode_bytes(body: &[u8]) -> ChatRequest {
    let request = codec()
        .decode_request(&facts(), body)
        .expect("the fixture is a readable request");
    request.validate().expect("a decoded request is valid");
    request
}

/// Decodes a request body that must be refused.
pub(crate) fn decode_err(body: &str) -> CodecError {
    codec()
        .decode_request(&facts(), body.as_bytes())
        .expect_err("the fixture must be refused")
}

/// Parses JSON text.
pub(crate) fn json(text: &str) -> Value {
    serde_json::from_str(text).expect("the fixture is valid JSON")
}

/// Encodes a response and returns its status and document.
pub(crate) fn output(response: &ChatResponse) -> (u16, Value) {
    let reply = codec()
        .encode_response(response)
        .expect("the response encodes");
    let body = std::str::from_utf8(reply.body()).expect("an encoded body is UTF-8");
    (reply.status(), json(body))
}

/// Builds one candidate answer.
pub(crate) fn response(turn: AssistantTurn, finish_reason: FinishReason) -> ChatResponse {
    let choice = Choice::try_new(0, turn, finish_reason).expect("a valid choice");
    ChatResponse::try_new(
        ResponseId::try_new("chatcmpl-1").expect("a valid id"),
        ModelId::try_new("chat-small-2026-01-01").expect("a valid model"),
        vec![choice],
    )
    .expect("a valid response")
}

/// Builds a turn with one text part.
pub(crate) fn text_turn(text: &str) -> AssistantTurn {
    AssistantTurn::try_new(vec![ContentPart::text(text)], Vec::new()).expect("a valid turn")
}

/// Builds a one-message request body.
pub(crate) fn user_body(text: &str) -> String {
    format!(r#"{{"model":"chat-small","messages":[{{"role":"user","content":"{text}"}}]}}"#)
}

/// Encodes a sequence of canonical events through one stream encoder.
pub(crate) fn encode_stream(events: &[StreamEvent]) -> Vec<u8> {
    let mut encoder = codec().stream_encoder();
    let mut bytes = Vec::new();
    for event in events {
        bytes.extend(
            encoder
                .push(event)
                .expect("the event is one this stream can encode"),
        );
    }
    bytes.extend(encoder.finish().expect("the stream terminates"));
    bytes
}

/// Splits an encoded event stream into its payloads, terminator included.
pub(crate) fn frames(stream: &[u8]) -> Vec<String> {
    let text = std::str::from_utf8(stream).expect("an encoded stream is UTF-8");
    if text.is_empty() {
        return Vec::new();
    }
    assert!(
        text.ends_with("\n\n"),
        "every frame ends with a blank line: {text:?}"
    );
    text.trim_end_matches("\n\n")
        .split("\n\n")
        .map(|frame| {
            frame
                .strip_prefix("data: ")
                .expect("every frame is a `data:` field")
                .to_owned()
        })
        .collect()
}

/// Parses the payloads of a stream into JSON documents, dropping the terminator.
pub(crate) fn chunks(stream: &[u8]) -> Vec<Value> {
    frames(stream)
        .into_iter()
        .take_while(|payload| payload != "[DONE]")
        .map(|payload| json(&payload))
        .collect()
}

/// Builds a user message item.
pub(crate) fn user_item(text: &str) -> Item {
    Item::Message(Message::user(text))
}
