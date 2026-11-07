//! One `parts[]` entry in each direction: what a canonical part writes and what an answer's part means.

use nico_llm::{ProviderError, StreamEvent};
use serde_json::{Value, json};

use crate::support::{MODEL_VERSION, call, encoded, events_of, frame, one, push_err, request};

/// Returns the events one answer's part list produces.
fn parts_events(parts: Value) -> Vec<StreamEvent> {
    events_of(&[
        json!({
            "candidates": [{"content": {"role": "model", "parts": parts}}],
            "modelVersion": MODEL_VERSION
        }),
        json!({"candidates": [{"finishReason": "STOP"}]}),
    ])
}

/// Returns the refusal one answer's part list produces.
fn parts_err(parts: Value) -> ProviderError {
    push_err(&[frame(&json!({
        "candidates": [{"content": {"role": "model", "parts": parts}}],
        "modelVersion": MODEL_VERSION
    }))])
}

/// Reports whether the events carry the given text delta.
fn has_text(events: &[StreamEvent], expected: &str) -> bool {
    events
        .iter()
        .any(|event| matches!(event, StreamEvent::TextDelta(text) if text == expected))
}

#[test]
fn a_text_part_is_one_text_delta() {
    let events = parts_events(json!([{"text": "hello"}]));
    assert!(has_text(&events, "hello"), "{events:?}");
}

#[test]
fn an_empty_text_part_adds_nothing() {
    // A zero-length increment is not content being dropped: the answer says nothing here.
    let events = parts_events(json!([{"text": ""}]));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::TextDelta(_))),
        "{events:?}"
    );
}

#[test]
fn a_thought_part_is_reasoning_and_its_signature_is_dropped() {
    // The canonical stream has one reasoning increment, carrying text and no signature; the signature
    // is read (so a malformed one is still a refusal) and then dropped, as the module documentation
    // records.
    let events = parts_events(json!([
        {"text": "thinking", "thought": true, "thoughtSignature": "sig-1"},
        {"text": "visible"}
    ]));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::ReasoningDelta(text) if text == "thinking")),
        "a thought is hidden reasoning, not visible text: {events:?}"
    );
    assert!(has_text(&events, "visible"), "{events:?}");
    assert!(
        !events.iter().any(
            |event| matches!(event, StreamEvent::TextDelta(text) if text.contains("thinking"))
        )
    );
}

#[test]
fn a_call_without_an_id_gets_one_from_the_adapters_position() {
    let events =
        parts_events(json!([{"functionCall": {"name": "weather", "args": {"city": "Paris"}}}]));
    let Some(StreamEvent::ToolCallStart { id, name, index }) = events
        .iter()
        .find(|event| matches!(event, StreamEvent::ToolCallStart { .. }))
    else {
        panic!("the answer must start one tool call: {events:?}");
    };
    assert_eq!(id.as_str(), "call_0");
    assert_eq!(name.as_str(), "weather");
    assert_eq!(*index, 0);
    assert!(
        events.iter().any(|event| matches!(
            event,
            StreamEvent::ToolCallDelta { arguments, index: 0, .. }
                if arguments == r#"{"city":"Paris"}"#
        )),
        "the dialect sends no fragments, so the whole document is one delta: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::ToolCallEnd { index: 0, .. }))
    );
}

#[test]
fn a_call_id_the_provider_sent_is_kept_and_calls_are_numbered_in_order() {
    let events = parts_events(json!([
        {"functionCall": {"id": "call-a", "name": "one"}},
        {"functionCall": {"name": "two"}}
    ]));
    let starts: Vec<(&str, u32)> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolCallStart { id, index, .. } => Some((id.as_str(), *index)),
            _ => None,
        })
        .collect();
    assert_eq!(
        starts,
        vec![("call-a", 0), ("call_1", 1)],
        "the assigned id is stable for one answer and the next request writes it back"
    );
}

#[test]
fn a_call_without_arguments_carries_an_empty_object() {
    let events = parts_events(json!([{"functionCall": {"name": "ping"}}]));
    assert!(
        events.iter().any(|event| matches!(
            event,
            StreamEvent::ToolCallDelta { arguments, .. } if arguments == "{}"
        )),
        "{events:?}"
    );
}

#[test]
fn a_call_whose_arguments_are_not_an_object_is_refused() {
    let error = parts_err(json!([{"functionCall": {"name": "ping", "args": "oops"}}]));
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "the canonical ToolCall requires an object: {error:?}"
    );
}

#[test]
fn inline_data_in_an_answer_is_refused() {
    // The canonical stream has no image increment, so an image part cannot be answered without
    // hiding it.
    let error = parts_err(json!([
        {"inlineData": {"mimeType": "image/png", "data": "cG5n"}}
    ]));
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
    assert!(!format!("{error}").contains("cG5n"));
}

#[test]
fn a_tool_result_in_an_answer_is_refused() {
    // `functionResponse` is a request shape: an answer that carries one is not this dialect's answer.
    let error = parts_err(json!([
        {"functionResponse": {"id": "call-1", "name": "weather", "response": {"content": "x"}}}
    ]));
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn a_stored_file_reference_in_an_answer_is_refused() {
    let error = parts_err(json!([
        {"fileData": {"mimeType": "application/pdf", "fileUri": "https://example.test/a.pdf"}}
    ]));
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn an_unknown_member_inside_a_part_is_refused_and_not_quoted() {
    let error = parts_err(json!([{"text": "hello", "executableCode": {"language": "PYTHON"}}]));
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
    assert!(
        !format!("{error}").contains("PYTHON"),
        "a diagnostic names the position, never the value: {error}"
    );
}

#[test]
fn a_part_carrying_two_kinds_is_refused() {
    let error = parts_err(json!([
        {"text": "hello", "functionCall": {"name": "weather"}}
    ]));
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn a_part_carrying_nothing_is_refused() {
    let error = parts_err(json!([{}]));
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn a_canonical_tool_call_and_its_answer_round_trip() {
    // One call and one result, written by the encoder and read back from an answer built the way the
    // provider would have written it.
    let request = request(vec![
        nico_llm::Item::Message(nico_llm::Message::user("weather?")),
        call("call-1", "weather", json!({"city": "Paris"})),
        crate::support::result("call-1", "sunny"),
    ]);
    let document = encoded(&request);
    assert_eq!(
        document["contents"][1]["parts"][0]["functionCall"]["name"],
        "weather"
    );
    assert_eq!(
        document["contents"][2]["parts"][0]["functionResponse"]["response"],
        json!({"content": "sunny"})
    );

    let events = parts_events(json!([
        {"functionCall": {"id": "call-1", "name": "weather", "args": {"city": "Paris"}}}
    ]));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::ToolCallStart { id, name, .. }
            if id.as_str() == "call-1" && name.as_str() == "weather"
    )));
}

#[test]
fn a_request_with_only_an_inline_image_round_trips() {
    let request = one(nico_llm::Item::Message(
        nico_llm::Message::try_new(
            nico_llm::Role::User,
            vec![crate::support::inline_image("image/png", b"png")],
        )
        .expect("a valid message"),
    ));
    let document = encoded(&request);
    assert_eq!(
        document["contents"][0]["parts"][0]["inlineData"]["data"],
        "cG5n"
    );
}
