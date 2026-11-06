//! Semantic round trips for the canonical fixtures and the invariants a value must keep.
//!
//! A round trip compares `serde_json::Value`s rather than string bytes: key order is not part of the
//! contract, but every value is. A fixture that failed to leave a field in `extensions` would show
//! up here as a missing key.

#![expect(
    clippy::expect_used,
    reason = "a fixture that cannot build its value has nothing useful to return"
)]

use nico_llm::{
    ChatRequest, ChatResponse, JsonSchema, ModelId, ModelRef, RawJson, ResponseFormat, StreamEvent,
    TokenKind, Usage, UsageCounts,
};
use serde_json::{Value, json};

const CHAT_REQUEST: &str = include_str!("fixtures/chat_request.json");
const CHAT_RESPONSE: &str = include_str!("fixtures/chat_response.json");
const STREAM_EVENTS: &str = include_str!("fixtures/stream_events.json");

fn json(text: &str) -> Value {
    serde_json::from_str(text).expect("fixture is valid JSON")
}

#[test]
fn a_chat_request_round_trips_with_every_item_kind() {
    let expected = json(CHAT_REQUEST);
    let request: ChatRequest = serde_json::from_str(CHAT_REQUEST).expect("request fixture decodes");
    request.validate().expect("request fixture is valid");
    let actual = serde_json::to_value(&request).expect("request serializes");
    assert_eq!(actual, expected, "canonical request must round-trip");
    assert!(
        request.extensions().get("x_vendor_trace").is_some(),
        "an unmodelled top-level field must land in extensions"
    );
}

#[test]
fn a_chat_response_round_trips_with_usage_and_extensions() {
    let expected = json(CHAT_RESPONSE);
    let response: ChatResponse =
        serde_json::from_str(CHAT_RESPONSE).expect("response fixture decodes");
    response.validate().expect("response fixture is valid");
    let actual = serde_json::to_value(&response).expect("response serializes");
    assert_eq!(actual, expected, "canonical response must round-trip");
    assert_eq!(
        response
            .usage()
            .reported_counts()
            .and_then(|counts| counts.get(TokenKind::CacheRead)),
        Some(8),
        "provider-reported cache reads must survive"
    );
}

#[test]
fn a_stream_event_sequence_round_trips() {
    let expected = json(STREAM_EVENTS);
    let events: Vec<StreamEvent> =
        serde_json::from_str(STREAM_EVENTS).expect("stream fixture decodes");
    assert!(
        matches!(events.first(), Some(StreamEvent::Start { .. })),
        "a stream starts with a start event"
    );
    assert!(
        matches!(events.last(), Some(StreamEvent::Error(_))),
        "the fixture ends with a structured error"
    );
    let actual = serde_json::to_value(&events).expect("events serialize");
    assert_eq!(actual, expected, "canonical events must round-trip");
}

#[test]
fn an_unmodelled_field_survives_a_round_trip() {
    let raw = r#"{
        "model": "gpt-4o-mini",
        "conversation": [
            {
                "type": "message",
                "data": {
                    "role": "user",
                    "content": [{ "type": "text", "data": "hi" }]
                }
            }
        ],
        "vendor_feature": { "enabled": true }
    }"#;
    let request: ChatRequest = serde_json::from_str(raw).expect("request decodes");
    request.validate().expect("request is valid");
    let stored = request
        .extensions()
        .get("vendor_feature")
        .expect("unknown field must be kept");
    assert_eq!(stored.as_value(), &json!({ "enabled": true }));
    let encoded = serde_json::to_string(&request).expect("request serializes");
    assert!(
        encoded.contains("vendor_feature"),
        "the field must be written back, not dropped"
    );
}

#[test]
fn usage_keeps_measured_and_reported_counts_apart() {
    let reported = UsageCounts::new()
        .with(TokenKind::Input, 42)
        .with(TokenKind::Output, 7);
    let measured = UsageCounts::new().with(TokenKind::Input, 42);
    let usage = Usage::both(reported, measured);
    let encoded = serde_json::to_value(usage).expect("usage serializes");
    assert_eq!(
        encoded,
        json!({
            "reported": { "input": 42, "output": 7 },
            "measured": { "input": 42 }
        })
    );
    let decoded: Usage = serde_json::from_value(encoded).expect("usage decodes");
    assert_eq!(
        decoded
            .reported_counts()
            .and_then(|counts| counts.get(TokenKind::Output)),
        Some(7)
    );
    assert_eq!(
        decoded
            .measured_counts()
            .and_then(|counts| counts.get(TokenKind::Output)),
        None,
        "a measured bucket must not inherit a reported count"
    );
    assert!(Usage::default().is_empty());
}

#[test]
fn a_structured_response_format_round_trips() {
    let schema = JsonSchema::try_new(
        Some("answer".to_owned()),
        RawJson::parse(r#"{"type":"object","properties":{"value":{"type":"integer"}}}"#)
            .expect("schema is JSON"),
    )
    .expect("valid json schema");
    let format = ResponseFormat::JsonSchema(schema);
    let encoded = serde_json::to_value(&format).expect("format serializes");
    assert_eq!(
        encoded,
        json!({
            "type": "json_schema",
            "data": {
                "name": "answer",
                "schema": {
                    "type": "object",
                    "properties": { "value": { "type": "integer" } }
                }
            }
        })
    );
    let decoded: ResponseFormat = serde_json::from_value(encoded).expect("format decodes");
    decoded.validate().expect("format is valid");
}

#[test]
fn a_model_ref_keeps_the_logical_and_physical_names_apart() {
    let logical = ModelId::try_new("gpt-4o-mini").expect("valid logical model");
    let physical = ModelId::try_new("gpt-4o-mini-2024-07-18").expect("valid physical model");
    let model = ModelRef::new(logical, physical);
    let encoded = serde_json::to_value(&model).expect("model ref serializes");
    assert_eq!(
        encoded,
        json!({
            "logical": "gpt-4o-mini",
            "physical": "gpt-4o-mini-2024-07-18"
        })
    );
    let decoded: ModelRef = serde_json::from_value(encoded).expect("model ref decodes");
    assert_eq!(decoded.logical().as_str(), "gpt-4o-mini");
    assert_eq!(decoded.physical().as_str(), "gpt-4o-mini-2024-07-18");
}

#[test]
fn canonical_values_can_cross_await_and_task_boundaries() {
    fn assert_send_sync<T: Send + Sync + 'static>() {}

    assert_send_sync::<ChatRequest>();
    assert_send_sync::<ChatResponse>();
    assert_send_sync::<StreamEvent>();
    assert_send_sync::<Usage>();
}
